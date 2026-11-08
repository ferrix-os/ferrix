//! The dependency graph: loading units as they are named, resolving their
//! dependencies to units, the implied and default dependencies each kind
//! adds (§4.3), the order `After=` and `Before=` make, and the slice tree
//! (§5.1).

use alloc::string::String;
use alloc::vec::Vec;

use super::{Manager, Slot, Sub};
use crate::UnitName;
use crate::event::{ActiveState, GroupPath, UnitId};
use crate::kind::Config;
use crate::name::UnitType;
use crate::restart::Policy;
use crate::source::{LoadError, Unit};
use crate::unit::Dependency;

pub(super) use crate::kind::{DRIVERS_SLICE, INIT_SCOPE, ROOT_SLICE, SHUTDOWN, SYSTEM_SLICE};

/// The dependencies a unit's own settings and its kind give it, by name:
/// its `[Unit]` keys and links, then what its kind implies and its default
/// dependencies ([`Kind::implied`](crate::kind::Kind::implied)). `exists`
/// says whether the unit directories have a unit by a name.
pub(super) fn declared(
    unit: &Unit,
    exists: impl Fn(&UnitName) -> bool,
) -> Vec<(Dependency, UnitName)> {
    let mut out: Vec<(Dependency, UnitName)> = Dependency::ALL
        .into_iter()
        .flat_map(|dependency| {
            unit.unit
                .named(dependency)
                .iter()
                .map(move |name| (dependency, name.clone()))
        })
        .collect();
    out.extend(crate::kind::of(unit.name.unit_type()).implied(unit, &exists));
    out
}

impl Manager {
    /// The unit `name` names, loading it and everything it depends on if
    /// the manager has not heard of it; `None` for a string that is not a
    /// unit name.
    pub(super) fn ensure(&mut self, name: &str) -> Option<UnitId> {
        let unit = self.slot_for(name)?;
        self.resolve();
        Some(unit)
    }

    /// The unit `name` names, made and loaded but with its edges left for
    /// [`Manager::resolve`].
    fn slot_for(&mut self, name: &str) -> Option<UnitId> {
        if let Some(&unit) = self.names.get(name) {
            return Some(unit);
        }
        let asked = UnitName::parse(name).ok()?;
        if asked.is_template() {
            return None;
        }
        let loaded = self.source.load(name);
        let canonical = match &loaded {
            Ok(unit) => unit.name.clone(),
            Err(_) => asked,
        };
        if let Some(&unit) = self.names.get(canonical.as_str()) {
            let _ = self.names.insert(String::from(name), unit);
            return Some(unit);
        }
        let unit = UnitId(u32::try_from(self.units.len()).ok()?);
        // What systemd logs as it loads a unit: a key it does not know, a
        // value that does not parse, a specifier it refuses. The unit loads
        // without them, so without a line nobody learns they were dropped.
        // A unit refused whole says why, once, as systemd's `bad-setting`
        // does: `svc status` shows only the state.
        let warned: Vec<String> = match &loaded {
            Ok(loaded) => loaded
                .warnings
                .list()
                .iter()
                .map(|warning| alloc::format!("{warning}"))
                .collect(),
            Err(LoadError::Refused(why)) => {
                alloc::vec![alloc::format!("{}: {}", canonical.as_str(), why.message)]
            }
            Err(_) => Vec::new(),
        };
        for line in warned {
            self.log(Some(unit), line);
        }
        let slot = Slot::new(canonical.clone(), loaded);
        self.units.push(slot);
        let _ = self.names.insert(String::from(canonical.as_str()), unit);
        let _ = self.names.insert(String::from(name), unit);
        if let Some(aliases) = self
            .slot(unit)
            .and_then(|slot| slot.loaded.as_ref().ok())
            .map(|u| u.aliases.clone())
        {
            for alias in aliases {
                let _ = self.names.insert(String::from(alias.as_str()), unit);
            }
        }
        self.setup(unit);
        self.unresolved.push(unit);
        Some(unit)
    }

    /// Resolve the edges of every unit made since, making the units they
    /// name, until none is left.
    pub(super) fn resolve(&mut self) {
        while let Some(unit) = self.unresolved.pop() {
            self.resolve_one(unit);
        }
    }

    /// Resolve one unit's edges.
    fn resolve_one(&mut self, unit: UnitId) {
        let names = {
            let Some(Ok(loaded)) = self.slot(unit).map(|slot| &slot.loaded) else {
                return;
            };
            let source = &self.source;
            declared(loaded, |name| source.load(name.as_str()).is_ok())
        };
        let mut edges: alloc::collections::BTreeMap<Dependency, Vec<UnitId>> =
            alloc::collections::BTreeMap::new();
        for (dependency, name) in names {
            let Some(other) = self.slot_for(name.as_str()) else {
                continue;
            };
            let list = edges.entry(dependency).or_default();
            if other != unit && !list.contains(&other) {
                list.push(other);
            }
        }
        self.order_target(unit, &mut edges);
        if let Some(slot) = self.slot_mut(unit) {
            slot.edges = edges;
        }
    }

    /// A target is after what it wants and requires, when both have
    /// default dependencies and it is not already before it: systemd's
    /// `unit_add_default_target_dependency`.
    fn order_target(
        &self,
        unit: UnitId,
        edges: &mut alloc::collections::BTreeMap<Dependency, Vec<UnitId>>,
    ) {
        let Some(slot) = self.slot(unit) else {
            return;
        };
        let Ok(loaded) = &slot.loaded else {
            return;
        };
        if slot.name.unit_type() != UnitType::Target || !loaded.unit.default_dependencies {
            return;
        }
        let pulled: Vec<UnitId> = [Dependency::Wants, Dependency::Requires]
            .iter()
            .filter_map(|dependency| edges.get(dependency))
            .flatten()
            .copied()
            .collect();
        let before = edges.get(&Dependency::Before).cloned().unwrap_or_default();
        for other in pulled {
            let defaults = self
                .slot(other)
                .and_then(|slot| slot.loaded.as_ref().ok())
                .is_some_and(|u| u.unit.default_dependencies);
            let after = edges.entry(Dependency::After).or_default();
            if defaults && !before.contains(&other) && !after.contains(&other) {
                after.push(other);
            }
        }
    }

    /// A new slot's cgroup path, restart policy, and whether it is always
    /// active.
    fn setup(&mut self, unit: UnitId) {
        let group = self.group_path(unit);
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        slot.group = group;
        let name = slot.name.as_str();
        slot.perpetual = name == ROOT_SLICE || name == INIT_SCOPE || name == DRIVERS_SLICE;
        slot.adopted = name == DRIVERS_SLICE;
        if let Ok(loaded) = &slot.loaded {
            let limit = loaded.unit.start_limit;
            slot.policy = match &loaded.config {
                Config::Service(service) => Some(Policy::new(
                    service.restart,
                    service.restart_sec,
                    limit.burst,
                    limit.interval,
                )),
                _ => None,
            };
            if matches!(loaded.config, Config::Builtin) {
                slot.perpetual = true;
            }
        }
        if slot.perpetual {
            slot.active = ActiveState::Active;
            slot.sub = Sub::Active;
            slot.made = slot.group.is_some();
        }
    }

    /// Where a unit's cgroup is: a slice under its parent slice, a service
    /// or a scope under its slice, and nothing for the other kinds.
    pub(super) fn group_path(&self, unit: UnitId) -> Option<GroupPath> {
        let slot = self.slot(unit)?;
        let name = &slot.name;
        match name.unit_type() {
            UnitType::Slice => Some(slice_path(name)),
            UnitType::Service | UnitType::Scope => {
                let slice = match slot.loaded.as_ref().ok().map(|u| &u.config) {
                    Some(Config::Service(service)) => service.slice.clone(),
                    Some(Config::Scope(scope)) => scope.slice.clone(),
                    _ => None,
                };
                let fallback = if name.as_str() == INIT_SCOPE {
                    ROOT_SLICE
                } else {
                    SYSTEM_SLICE
                };
                let slice = slice.or_else(|| UnitName::parse(fallback).ok())?;
                Some(slice_path(&slice).child(name.as_str()))
            }
            _ => None,
        }
    }

    /// Whether `a` is ordered after `b`: `a` has `After=b`, or `b` has
    /// `Before=a`.
    pub(super) fn after(&self, a: UnitId, b: UnitId) -> bool {
        let has = |x: UnitId, dependency: Dependency, y: UnitId| {
            self.slot(x)
                .and_then(|slot| slot.edges.get(&dependency))
                .is_some_and(|list| list.contains(&y))
        };
        has(a, Dependency::After, b) || has(b, Dependency::Before, a)
    }

    /// The units that name `unit` in one of `dependencies`.
    pub(super) fn dependents(&self, unit: UnitId, dependencies: &[Dependency]) -> Vec<UnitId> {
        (0..self.units.len())
            .filter_map(|index| u32::try_from(index).ok().map(UnitId))
            .filter(|&other| {
                self.slot(other).is_some_and(|slot| {
                    dependencies.iter().any(|dependency| {
                        slot.edges
                            .get(dependency)
                            .is_some_and(|list| list.contains(&unit))
                    })
                })
            })
            .collect()
    }

    /// The units `unit` names in `dependency`.
    pub(super) fn targets(&self, unit: UnitId, dependency: Dependency) -> Vec<UnitId> {
        self.slot(unit)
            .and_then(|slot| slot.edges.get(&dependency))
            .cloned()
            .unwrap_or_default()
    }
}

/// A slice's cgroup: its parents' names joined, `-.slice` the root.
fn slice_path(slice: &UnitName) -> GroupPath {
    let mut chain = Vec::new();
    let mut current = Some(slice.clone());
    while let Some(name) = current {
        if name.as_str() == ROOT_SLICE {
            break;
        }
        current = name.slice_parent();
        chain.push(name);
    }
    chain
        .iter()
        .rev()
        .fold(GroupPath::root(), |path, name| path.child(name.as_str()))
}

impl Slot {
    /// A slot for a unit just loaded, or not.
    fn new(name: UnitName, loaded: Result<Unit, LoadError>) -> Self {
        Self {
            name,
            loaded,
            edges: alloc::collections::BTreeMap::new(),
            active: ActiveState::Inactive,
            sub: Sub::Dead,
            group: None,
            made: false,
            populated: false,
            op: None,
            policy: None,
            result: None,
            status: None,
            perpetual: false,
            adopted: false,
            deadline: None,
            watchdog: None,
            watchdog_interval: None,
            watchdog_late: None,
            watchdog_pinged: None,
            watchdog_longest: core::time::Duration::ZERO,
            main: None,
            control: None,
            spawning: None,
            step: 0,
            started: false,
            stopping: false,
            scope_pids: Vec::new(),
            reloading: Vec::new(),
            connection: None,
        }
    }
}
