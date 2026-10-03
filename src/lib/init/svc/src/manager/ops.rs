//! Running the queue: an operation begins once nothing it is ordered after
//! is still queued, and ends when its unit gets where it asked, or cannot.
//!
//! Every unit's change of state goes through [`Manager::set_state`], which
//! ends the unit's operation if the change answers it, and otherwise does
//! what an unasked-for change means: units `BindsTo=` it stop, its
//! `OnFailure=` units start, and a provider that came up gets its pending
//! OPENs.

use alloc::format;
use alloc::string::String;

use super::graph::ROOT_SLICE;
use super::{Manager, OpId, OpKind, Sub};
use crate::event::{ActiveState, OpResult, Reply, UnitId};
use crate::kind::Config;
use crate::name::UnitType;
use crate::unit::{Dependency, conditions_hold};

impl Manager {
    /// Begin every operation that can, until none can.
    pub(super) fn dispatch(&mut self) {
        loop {
            let next = self
                .ops
                .iter()
                .find(|&(id, op)| !op.running && self.runnable(*id))
                .map(|(id, _)| *id);
            let Some(id) = next else {
                return;
            };
            self.run(id);
        }
    }

    /// Whether an operation waits for no other queued one.
    fn runnable(&self, id: OpId) -> bool {
        let Some(op) = self.ops.get(&id) else {
            return false;
        };
        !self.ops.iter().any(|(&other_id, other)| {
            other_id != id
                && other.unit != op.unit
                && self.waits(op.unit, op.kind, other.unit, other.kind)
        })
    }

    /// Begin an operation.
    fn run(&mut self, id: OpId) {
        let Some(op) = self.ops.get_mut(&id) else {
            return;
        };
        op.running = true;
        let (unit, kind) = (op.unit, op.kind);
        match kind {
            OpKind::Verify => {
                let active = self
                    .slot(unit)
                    .is_some_and(|s| s.active == ActiveState::Active);
                let result = if active {
                    OpResult::Done
                } else {
                    OpResult::Dependency
                };
                self.finish(id, result);
            }
            OpKind::Start => self.begin_start(id, unit),
            OpKind::Stop | OpKind::Restart => self.begin_stop(unit),
        }
    }

    /// Begin a start: conditions, assertions, then the kind's own start.
    fn begin_start(&mut self, id: OpId, unit: UnitId) {
        let Some(slot) = self.slot(unit) else {
            return;
        };
        if slot.active == ActiveState::Active && slot.sub != Sub::AutoRestart {
            self.finish(id, OpResult::Done);
            return;
        }
        let Ok(loaded) = &slot.loaded else {
            self.finish(id, OpResult::Failed);
            return;
        };
        let conditions = loaded.unit.conditions.clone();
        let asserts = loaded.unit.asserts.clone();
        let config_kind = loaded.name.unit_type();
        if !conditions_hold(&conditions, |c| self.probe.test(c)) {
            let line = format!("{}: skipped, a condition failed", self.display(unit));
            self.log(Some(unit), line);
            self.finish(id, OpResult::Skipped);
            return;
        }
        if !conditions_hold(&asserts, |c| self.probe.test(c)) {
            let line = format!("{}: an assertion failed", self.display(unit));
            self.log(Some(unit), line);
            self.finish(id, OpResult::Assert);
            return;
        }
        match config_kind {
            UnitType::Service => self.service_start(id, unit),
            UnitType::Socket => self.socket_start(unit),
            UnitType::Mount => self.mount_start(unit),
            UnitType::Scope => self.scope_start(id, unit),
            UnitType::Slice => self.slice_start(unit),
            UnitType::Device => self.device_start(unit),
            UnitType::Target | UnitType::Builtin => {
                self.set_state(unit, ActiveState::Active, Sub::Active);
            }
        }
    }

    /// Begin a stop, for a stop or the first half of a restart.
    fn begin_stop(&mut self, unit: UnitId) {
        let Some(slot) = self.slot(unit) else {
            return;
        };
        if slot.perpetual {
            let op = slot.op;
            if let Some(op) = op {
                self.finish(op, OpResult::Done);
            }
            return;
        }
        if matches!(slot.active, ActiveState::Inactive | ActiveState::Failed) {
            let (active, sub) = (slot.active, slot.sub);
            self.set_state(unit, active, sub);
            return;
        }
        match slot.name.unit_type() {
            UnitType::Service => self.service_stop(unit),
            UnitType::Mount => self.mount_stop(unit),
            UnitType::Scope => self.scope_stop(unit),
            UnitType::Slice => self.slice_stop(unit),
            UnitType::Socket => self.socket_stop(unit),
            UnitType::Device => self.device_stop(unit),
            UnitType::Target | UnitType::Builtin => {
                self.set_state(unit, ActiveState::Inactive, Sub::Dead);
            }
        }
    }

    /// End an operation: answer its clients, fail what depended on a start
    /// that failed, and see to boot and shutdown.
    pub(super) fn finish(&mut self, id: OpId, result: OpResult) {
        let Some(op) = self.ops.remove(&id) else {
            return;
        };
        if let Some(slot) = self.slot_mut(op.unit)
            && slot.op == Some(id)
        {
            slot.op = None;
        }
        for client in &op.clients {
            self.reply(*client, Reply::Done(result));
        }
        let failed = matches!(
            result,
            OpResult::Failed | OpResult::Dependency | OpResult::Timeout | OpResult::Assert
        );
        if failed && op.kind.starts() {
            let dependents = self.dependents(
                op.unit,
                &[
                    Dependency::Requires,
                    Dependency::BindsTo,
                    Dependency::Requisite,
                ],
            );
            for other in dependents {
                let queued = self
                    .slot(other)
                    .and_then(|slot| slot.op)
                    .filter(|other_id| {
                        self.ops
                            .get(other_id)
                            .is_some_and(|o| !o.running && o.kind != OpKind::Stop)
                    });
                if let Some(other_id) = queued {
                    self.finish(other_id, OpResult::Dependency);
                }
            }
            self.refuse_opens(op.unit);
        }
        if self.boot == Some(id) {
            self.boot = None;
            if !matches!(result, OpResult::Done | OpResult::Skipped) {
                let line = format!(
                    "{}: {}; starting rescue.target",
                    self.display(op.unit),
                    result.name()
                );
                self.log(Some(op.unit), line);
                self.rescue();
            }
        }
        if self.shutdown.as_ref().is_some_and(|s| s.anchor == Some(id)) {
            self.kill_the_rest();
        }
    }

    /// A unit's state changed: record it, end its operation if the change
    /// answers it, and otherwise act on a change nobody asked for.
    pub(super) fn set_state(&mut self, unit: UnitId, active: ActiveState, sub: Sub) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        let was = slot.active;
        slot.active = active;
        slot.sub = sub;
        let op = slot.op;
        let service = slot.name.unit_type() == UnitType::Service;
        if was != active {
            self.announce(unit, was, active);
            if service && matches!(active, ActiveState::Inactive | ActiveState::Failed) {
                self.socket_service_down(unit);
            }
        }
        let answered = op.and_then(|id| self.ops.get(&id).map(|o| (id, o.kind, o.running)));
        match answered {
            Some((id, OpKind::Start, true)) => match active {
                ActiveState::Active => self.finish(id, OpResult::Done),
                ActiveState::Failed => self.finish(id, OpResult::Failed),
                ActiveState::Inactive if sub != Sub::AutoRestart => {
                    let success = self.slot(unit).is_some_and(|s| {
                        s.result.is_none_or(|r| r == crate::restart::Ended::Success)
                    });
                    self.finish(
                        id,
                        if success {
                            OpResult::Done
                        } else {
                            OpResult::Failed
                        },
                    );
                }
                _ => {}
            },
            Some((id, OpKind::Stop, true))
                if matches!(active, ActiveState::Inactive | ActiveState::Failed) =>
            {
                self.finish(id, OpResult::Done);
            }
            Some((id, OpKind::Restart, true))
                if matches!(active, ActiveState::Inactive | ActiveState::Failed) =>
            {
                if let Some(op) = self.ops.get_mut(&id) {
                    op.kind = OpKind::Start;
                    op.running = false;
                }
            }
            _ => {}
        }
        if was != active {
            self.unasked(unit, was, active, answered.is_some());
        }
    }

    /// A line in the boot log for a change of state.
    fn announce(&mut self, unit: UnitId, was: ActiveState, active: ActiveState) {
        let name = self.display(unit);
        let result = self
            .slot(unit)
            .and_then(|slot| slot.result)
            .map_or("", crate::restart::Ended::name);
        let line = match active {
            ActiveState::Active => format!("{name}: active"),
            ActiveState::Failed => format!("{name}: failed ({result})"),
            ActiveState::Inactive if was != ActiveState::Activating => format!("{name}: stopped"),
            _ => return,
        };
        self.log(Some(unit), line);
    }

    /// What a change of state means besides its operation.
    fn unasked(&mut self, unit: UnitId, was: ActiveState, active: ActiveState, had_op: bool) {
        let went_down = was == ActiveState::Active
            && matches!(
                active,
                ActiveState::Inactive | ActiveState::Failed | ActiveState::Deactivating
            );
        if went_down && !had_op && self.shutdown.is_none() {
            for other in self.dependents(unit, &[Dependency::BindsTo]) {
                if self
                    .slot(other)
                    .is_some_and(|s| s.active == ActiveState::Active)
                {
                    let _ = self.transaction(other, OpKind::Stop, super::Mode::Replace);
                }
            }
        }
        if active == ActiveState::Failed && self.shutdown.is_none() {
            for other in self.targets(unit, Dependency::OnFailure) {
                if let Err(why) = self.transaction(other, OpKind::Start, super::Mode::Replace) {
                    self.log(Some(other), why);
                }
            }
        }
        if active == ActiveState::Active {
            self.deliver_opens(unit);
        }
    }

    /// Queue `kind` on the unit named `name` for a client, answering at once
    /// if there is nothing to do or it cannot be done.
    pub(super) fn request_op(
        &mut self,
        client: crate::event::ClientId,
        name: &str,
        kind: OpKind,
        mode: super::Mode,
    ) {
        if self.shutdown.is_some() && kind != OpKind::Stop {
            self.reply(
                client,
                Reply::Refused(String::from("the machine is going down")),
            );
            return;
        }
        let Some(unit) = self.ensure(name) else {
            self.reply(client, Reply::Refused(format!("{name}: not a unit name")));
            return;
        };
        if let Some(refusal) = self.refusal(unit, kind, mode) {
            self.reply(client, Reply::Refused(refusal));
            return;
        }
        match self.transaction(unit, kind, mode) {
            Ok(Some(id)) => {
                if let Some(op) = self.ops.get_mut(&id) {
                    op.clients.push(client);
                }
            }
            Ok(None) => self.reply(client, Reply::Done(OpResult::Done)),
            Err(why) => self.reply(client, Reply::Refused(why)),
        }
    }

    /// Why a unit refuses what a client asks, if it does.
    fn refusal(&self, unit: UnitId, kind: OpKind, mode: super::Mode) -> Option<String> {
        let slot = self.slot(unit)?;
        let name = slot.name.as_str();
        let loaded = slot.loaded.as_ref().ok();
        let manual = loaded.map(|u| &u.unit);
        match kind {
            OpKind::Stop | OpKind::Restart if slot.perpetual || name == ROOT_SLICE => {
                Some(format!("{name} cannot be stopped"))
            }
            OpKind::Stop if manual.is_some_and(|u| u.refuse_manual_stop) => {
                Some(format!("{name} may not be stopped by request"))
            }
            OpKind::Start | OpKind::Restart if manual.is_some_and(|u| u.refuse_manual_start) => {
                Some(format!("{name} may not be started by request"))
            }
            OpKind::Start
                if mode == super::Mode::Isolate && !manual.is_some_and(|u| u.allow_isolate) =>
            {
                Some(format!("{name} may not be isolated"))
            }
            OpKind::Start if loaded.is_some_and(|u| matches!(u.config, Config::Scope(_))) => {
                Some(format!("{name}: a scope is made with svc scope"))
            }
            _ => None,
        }
    }
}
