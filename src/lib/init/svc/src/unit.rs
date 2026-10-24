//! The `[Unit]` and `[Install]` sections, which every kind shares.
//!
//! `[Unit]` says what a unit is called for people, what it depends on and
//! how (§4.3), which conditions skip it, and how often it may be started.
//! `[Install]` says where `svc enable` links it; it does nothing at load,
//! as in systemd.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use crate::ini::{Assignment, Section};
use crate::keys::{self, Setter};
use crate::value::Span;
use crate::{UnitName, Warnings};

/// A dependency, with systemd's meaning (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dependency {
    /// Start it too; fail if it fails.
    Requires,
    /// Fail unless it is already active; do not start it.
    Requisite,
    /// Start it too; carry on if it fails.
    Wants,
    /// As `Requires`, and stop when it stops, for whatever reason.
    BindsTo,
    /// Stop and restart when it does; start nothing.
    PartOf,
    /// Never active at once: starting one stops the other.
    Conflicts,
    /// Order: this starts before it, and stops after it.
    Before,
    /// Order: this starts after it, and stops before it.
    After,
    /// Start it when this one fails.
    OnFailure,
}

impl Dependency {
    /// Every dependency, in key order.
    pub const ALL: [Dependency; 9] = [
        Dependency::Requires,
        Dependency::Requisite,
        Dependency::Wants,
        Dependency::BindsTo,
        Dependency::PartOf,
        Dependency::Conflicts,
        Dependency::Before,
        Dependency::After,
        Dependency::OnFailure,
    ];

    /// The key that declares it.
    pub fn key(self) -> &'static str {
        match self {
            Dependency::Requires => "Requires",
            Dependency::Requisite => "Requisite",
            Dependency::Wants => "Wants",
            Dependency::BindsTo => "BindsTo",
            Dependency::PartOf => "PartOf",
            Dependency::Conflicts => "Conflicts",
            Dependency::Before => "Before",
            Dependency::After => "After",
            Dependency::OnFailure => "OnFailure",
        }
    }
}

/// What a condition, or an assertion, tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Test {
    /// `PathExists=`.
    PathExists,
    /// `PathExistsGlob=`.
    PathExistsGlob,
    /// `PathIsDirectory=`.
    PathIsDirectory,
    /// `PathIsSymbolicLink=`.
    PathIsSymbolicLink,
    /// `PathIsMountPoint=`.
    PathIsMountPoint,
    /// `PathIsReadWrite=`.
    PathIsReadWrite,
    /// `DirectoryNotEmpty=`.
    DirectoryNotEmpty,
    /// `FileNotEmpty=`.
    FileNotEmpty,
    /// `FileIsExecutable=`.
    FileIsExecutable,
    /// `KernelCommandLine=`: a word, or `word=value`, of the command line.
    KernelCommandLine,
    /// `Virtualization=`.
    Virtualization,
    /// `Architecture=`.
    Architecture,
    /// `Host=`.
    Host,
    /// `Environment=`: a variable in the manager's environment.
    Environment,
    /// `User=`.
    User,
    /// `Group=`.
    Group,
    /// `FirstBoot=`.
    FirstBoot,
}

/// The tests, by the name after `Condition` or `Assert`.
const TESTS: [(&str, Test); 17] = [
    ("PathExists", Test::PathExists),
    ("PathExistsGlob", Test::PathExistsGlob),
    ("PathIsDirectory", Test::PathIsDirectory),
    ("PathIsSymbolicLink", Test::PathIsSymbolicLink),
    ("PathIsMountPoint", Test::PathIsMountPoint),
    ("PathIsReadWrite", Test::PathIsReadWrite),
    ("DirectoryNotEmpty", Test::DirectoryNotEmpty),
    ("FileNotEmpty", Test::FileNotEmpty),
    ("FileIsExecutable", Test::FileIsExecutable),
    ("KernelCommandLine", Test::KernelCommandLine),
    ("Virtualization", Test::Virtualization),
    ("Architecture", Test::Architecture),
    ("Host", Test::Host),
    ("Environment", Test::Environment),
    ("User", Test::User),
    ("Group", Test::Group),
    ("FirstBoot", Test::FirstBoot),
];

impl Test {
    /// Whether its argument is a path, which must be absolute.
    pub fn takes_path(self) -> bool {
        matches!(
            self,
            Test::PathExists
                | Test::PathExistsGlob
                | Test::PathIsDirectory
                | Test::PathIsSymbolicLink
                | Test::PathIsMountPoint
                | Test::PathIsReadWrite
                | Test::DirectoryNotEmpty
                | Test::FileNotEmpty
                | Test::FileIsExecutable
        )
    }

    /// Its name, after `Condition` or `Assert`.
    pub fn name(self) -> &'static str {
        TESTS
            .iter()
            .find(|&&(_, test)| test == self)
            .map_or("", |&(name, _)| name)
    }
}

/// One `Condition…=` or `Assert…=`.
///
/// A condition that fails skips the unit, which is not a failure: what is
/// ordered after it still starts. An assertion that fails fails the start.
/// Tests are the backend's to run, since they look at the machine; the
/// manager asks through a probe it is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// What it tests.
    pub test: Test,
    /// The argument, without its `|` and `!`.
    pub argument: String,
    /// `!`: holds when the test fails.
    pub negate: bool,
    /// `|`: one of a group of which at least one must hold.
    pub trigger: bool,
}

impl Condition {
    /// Parse `|!argument`.
    fn parse(test: Test, text: &str) -> Condition {
        let (trigger, text) = match text.strip_prefix('|') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, text),
        };
        let (negate, argument) = match text.strip_prefix('!') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, text),
        };
        Condition {
            test,
            argument: String::from(argument),
            negate,
            trigger,
        }
    }
}

/// Whether a list of conditions holds, given `test`, which runs one test
/// and says whether it passed before negation: every condition without `|`
/// must hold, and if any has `|`, at least one of those must.
pub fn conditions_hold(conditions: &[Condition], mut test: impl FnMut(&Condition) -> bool) -> bool {
    let mut any_trigger = false;
    let mut trigger_held = false;
    for condition in conditions {
        let held = test(condition) != condition.negate;
        if condition.trigger {
            any_trigger = true;
            trigger_held |= held;
        } else if !held {
            return false;
        }
    }
    !any_trigger || trigger_held
}

/// How often a unit may start: `StartLimitBurst=` starts within
/// `StartLimitIntervalSec=` (§5.4). systemd's defaults are five in ten
/// seconds; an interval of zero turns the limit off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartLimit {
    /// The window.
    pub interval: Duration,
    /// Starts allowed within it.
    pub burst: u32,
}

impl Default for StartLimit {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            burst: 5,
        }
    }
}

/// The `[Unit]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitSection {
    /// `Description=`.
    pub description: Option<String>,
    /// `Documentation=`: URIs.
    pub documentation: Vec<String>,
    /// The dependency keys, each with the units it names, in order.
    pub dependencies: BTreeMap<Dependency, Vec<UnitName>>,
    /// `DefaultDependencies=`: whether the kind adds its default
    /// dependencies (§4.3). Yes by default.
    pub default_dependencies: bool,
    /// `Condition…=`, in order.
    pub conditions: Vec<Condition>,
    /// `Assert…=`, in order.
    pub asserts: Vec<Condition>,
    /// `StartLimitIntervalSec=` and `StartLimitBurst=`.
    pub start_limit: StartLimit,
    /// `AllowIsolate=`.
    pub allow_isolate: bool,
    /// `IgnoreOnIsolate=`.
    pub ignore_on_isolate: bool,
    /// `RefuseManualStart=`.
    pub refuse_manual_start: bool,
    /// `RefuseManualStop=`.
    pub refuse_manual_stop: bool,
    /// `StopWhenUnneeded=`.
    pub stop_when_unneeded: bool,
    /// `StartableBy=` (Ferrix's): users, by name or number, who may `svc
    /// start` the unit without being root -- lode's alerts
    /// (`docs/LOGGING.md` §7.4).
    pub startable_by: Vec<String>,
}

impl Default for UnitSection {
    fn default() -> Self {
        Self {
            description: None,
            documentation: Vec::new(),
            dependencies: BTreeMap::new(),
            default_dependencies: true,
            conditions: Vec::new(),
            asserts: Vec::new(),
            start_limit: StartLimit::default(),
            allow_isolate: false,
            ignore_on_isolate: false,
            refuse_manual_start: false,
            refuse_manual_stop: false,
            stop_when_unneeded: false,
            startable_by: Vec::new(),
        }
    }
}

impl UnitSection {
    /// The units `dependency` names.
    pub fn named(&self, dependency: Dependency) -> &[UnitName] {
        self.dependencies
            .get(&dependency)
            .map_or(&[], Vec::as_slice)
    }

    /// Add a dependency, as a kind's implied ones and `.wants/` links do.
    pub fn add(&mut self, dependency: Dependency, name: UnitName) {
        let list = self.dependencies.entry(dependency).or_default();
        if !list.contains(&name) {
            list.push(name);
        }
    }

    /// Parse the section.
    pub fn parse(section: &Section, warnings: &mut Warnings) -> UnitSection {
        let mut unit = UnitSection::default();
        for assignment in &section.assignments {
            if let Some(dependency) = Dependency::ALL
                .into_iter()
                .find(|dependency| dependency.key() == assignment.key)
            {
                let list = unit.dependencies.entry(dependency).or_default();
                keys::names(list, assignment, warnings, false);
            } else if !unit.condition(assignment, warnings) {
                match keys::find(&UNIT_KEYS, &assignment.key) {
                    Some(setter) => setter(&mut unit, assignment, warnings),
                    None => keys::unknown(&section.name, assignment, warnings),
                }
            }
        }
        unit.dependencies.retain(|_, list| !list.is_empty());
        unit
    }

    /// Apply the start-limit keys a `[Service]` section has, which systemd
    /// still takes there from before they moved to `[Unit]`.
    pub(crate) fn start_limit_from(&mut self, section: &Section, warnings: &mut Warnings) {
        for assignment in &section.assignments {
            if assignment.key.starts_with("StartLimit")
                && let Some(setter) = keys::find(&UNIT_KEYS, &assignment.key)
            {
                setter(self, assignment, warnings);
            }
        }
    }

    /// Apply a `Condition…=` or `Assert…=`, if that is what `assignment` is.
    fn condition(&mut self, assignment: &Assignment, warnings: &mut Warnings) -> bool {
        let (list, name) = if let Some(name) = assignment.key.strip_prefix("Condition") {
            (&mut self.conditions, name)
        } else if let Some(name) = assignment.key.strip_prefix("Assert") {
            (&mut self.asserts, name)
        } else {
            return false;
        };
        let Some(&(_, test)) = TESTS.iter().find(|(known, _)| *known == name) else {
            return false;
        };
        if assignment.value.is_empty() {
            list.clear();
            return true;
        }
        let condition = Condition::parse(test, &assignment.value);
        if test.takes_path() && !crate::value::is_absolute_path(&condition.argument) {
            keys::invalid(assignment, warnings, "not an absolute path");
        } else {
            list.push(condition);
        }
        true
    }
}

/// The `[Unit]` keys that are neither dependencies nor conditions.
const UNIT_KEYS: [(&str, Setter<UnitSection>); 12] = [
    ("Description", |unit, a, _| {
        unit.description = keys::string(a);
    }),
    ("Documentation", |unit, a, _| {
        keys::words(&mut unit.documentation, a);
    }),
    ("DefaultDependencies", |unit, a, w| {
        if let Some(yes) = keys::boolean(a, w) {
            unit.default_dependencies = yes;
        }
    }),
    ("StartLimitIntervalSec", start_limit_interval),
    ("StartLimitInterval", start_limit_interval),
    ("StartLimitBurst", |unit, a, w| {
        if let Some(burst) = keys::parsed(a, w, str::parse::<u32>) {
            unit.start_limit.burst = burst;
        }
    }),
    ("AllowIsolate", |unit, a, w| {
        unit.allow_isolate = keys::boolean(a, w).unwrap_or(unit.allow_isolate);
    }),
    ("IgnoreOnIsolate", |unit, a, w| {
        unit.ignore_on_isolate = keys::boolean(a, w).unwrap_or(unit.ignore_on_isolate);
    }),
    ("RefuseManualStart", |unit, a, w| {
        unit.refuse_manual_start = keys::boolean(a, w).unwrap_or(unit.refuse_manual_start);
    }),
    ("RefuseManualStop", |unit, a, w| {
        unit.refuse_manual_stop = keys::boolean(a, w).unwrap_or(unit.refuse_manual_stop);
    }),
    ("StopWhenUnneeded", |unit, a, w| {
        unit.stop_when_unneeded = keys::boolean(a, w).unwrap_or(unit.stop_when_unneeded);
    }),
    ("StartableBy", |unit, a, _| {
        keys::words(&mut unit.startable_by, a);
    }),
];

/// `StartLimitIntervalSec=`, whose `infinity` and zero both turn the limit
/// off.
fn start_limit_interval(unit: &mut UnitSection, assignment: &Assignment, warnings: &mut Warnings) {
    match keys::span(assignment, warnings) {
        Some(Span::Finite(interval)) => unit.start_limit.interval = interval,
        Some(Span::Infinity) => unit.start_limit.interval = Duration::ZERO,
        None => {}
    }
}

/// The `[Install]` section: what `svc enable` links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Install {
    /// `WantedBy=`: a link in each one's `.wants/`.
    pub wanted_by: Vec<UnitName>,
    /// `RequiredBy=`: a link in each one's `.requires/`.
    pub required_by: Vec<UnitName>,
    /// `Alias=`: a link under each name.
    pub alias: Vec<UnitName>,
    /// `Also=`: units enabled and disabled with this one.
    pub also: Vec<UnitName>,
    /// `DefaultInstance=`: the instance a template is enabled as.
    pub default_instance: Option<String>,
}

impl Install {
    /// Parse the section.
    pub fn parse(section: &Section, warnings: &mut Warnings) -> Install {
        let mut install = Install::default();
        keys::apply(section, &INSTALL_KEYS, &mut install, warnings);
        install
    }
}

/// The `[Install]` keys.
const INSTALL_KEYS: [(&str, Setter<Install>); 5] = [
    ("WantedBy", |i, a, w| {
        keys::names(&mut i.wanted_by, a, w, true);
    }),
    ("RequiredBy", |i, a, w| {
        keys::names(&mut i.required_by, a, w, true);
    }),
    ("Alias", |i, a, w| keys::names(&mut i.alias, a, w, true)),
    ("Also", |i, a, w| keys::names(&mut i.also, a, w, true)),
    ("DefaultInstance", |i, a, w| {
        if a.value.is_empty()
            || a.value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ":-_.\\@".contains(c))
        {
            i.default_instance = keys::string(a);
        } else {
            keys::invalid(a, w, format!("'{}' is not a valid instance", a.value));
        }
    }),
];
