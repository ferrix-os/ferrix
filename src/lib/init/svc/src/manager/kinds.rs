//! The kinds besides services: slices, scopes, mounts and devices. Targets
//! and builtins have no state machine of their own; [`ops`](super::ops)
//! moves them between up and down directly.
//!
//! A device is up while its node exists. Its start asks the backend to
//! watch for the node ([`Action::WatchDevice`]) and waits, with no timeout,
//! until [`Event::Device`](crate::event::Event::Device) says it is there:
//! the backend answers every watch at once, so a node that is there already
//! starts it in the same step. A node that goes while the device is up
//! takes it down as nobody asked, which stops what is `BindsTo=` it
//! ([`ops`](super::ops)); `Requires=` alone, as under systemd, does not.

use alloc::format;
use alloc::vec::Vec;

use super::{Manager, OpId, Sub};
use crate::event::{Action, ActiveState, Errno, MountSpec, OpResult, UnitId, Whom};
use crate::kind::{Config, Device, KillMode};
use crate::name::UnitType;
use crate::value::Span;

impl Manager {
    /// A slice's start: its cgroup, then up.
    pub(super) fn slice_start(&mut self, unit: UnitId) {
        self.make_group(unit);
        self.set_state(unit, ActiveState::Active, Sub::Active);
    }

    /// A slice's stop: everything in it has stopped first, being ordered
    /// after it, so its cgroup goes.
    pub(super) fn slice_stop(&mut self, unit: UnitId) {
        if let Some(slot) = self.slot_mut(unit) {
            slot.populated = false;
        }
        self.remove_group(unit);
        self.set_state(unit, ActiveState::Inactive, Sub::Dead);
    }

    /// A scope's start (§5.6): its cgroup, and the processes moved into it.
    pub(super) fn scope_start(&mut self, id: OpId, unit: UnitId) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        let pids: Vec<_> = core::mem::take(&mut slot.scope_pids);
        if pids.is_empty() {
            self.finish(id, OpResult::Failed);
            return;
        }
        slot.populated = true;
        slot.result = None;
        self.make_group(unit);
        self.emit(Action::Move { unit, pids });
        self.set_state(unit, ActiveState::Active, Sub::Running);
    }

    /// A scope's settings: how to stop it.
    fn scope_kill(&self, unit: UnitId) -> Option<(crate::kind::Kill, Span)> {
        match &self.slot(unit)?.loaded.as_ref().ok()?.config {
            Config::Scope(scope) => Some((scope.kill, scope.timeout_stop)),
            _ => None,
        }
    }

    /// A scope's stop: the signal to all of it, then `cgroup.kill`.
    pub(super) fn scope_stop(&mut self, unit: UnitId) {
        let Some((kill, timeout)) = self.scope_kill(unit) else {
            return;
        };
        let populated = self.slot(unit).is_some_and(|s| s.populated);
        if !populated || kill.mode == KillMode::None {
            self.scope_gone(unit);
            return;
        }
        self.set_state(unit, ActiveState::Deactivating, Sub::StopSigterm);
        self.emit(Action::Signal {
            unit,
            signal: kill.signal,
            whom: Whom::Group,
        });
        let deadline = match timeout {
            Span::Finite(duration) => Some(self.now + duration),
            Span::Infinity => None,
        };
        if let Some(slot) = self.slot_mut(unit) {
            slot.deadline = deadline;
        }
    }

    /// A scope's timer: `cgroup.kill` after the signal, and giving up after
    /// that.
    pub(super) fn scope_timer(&mut self, unit: UnitId) {
        let Some((_, timeout)) = self.scope_kill(unit) else {
            return;
        };
        match self.slot(unit).map(|s| s.sub) {
            Some(Sub::StopSigterm) => {
                self.set_state(unit, ActiveState::Deactivating, Sub::StopSigkill);
                self.emit(Action::KillGroup { unit });
                let deadline = match timeout {
                    Span::Finite(duration) => Some(self.now + duration),
                    Span::Infinity => None,
                };
                if let Some(slot) = self.slot_mut(unit) {
                    slot.deadline = deadline;
                }
            }
            Some(Sub::StopSigkill) => {
                let line = format!(
                    "{}: processes still around after SIGKILL",
                    self.display(unit)
                );
                self.log(Some(unit), line);
                if let Some(slot) = self.slot_mut(unit) {
                    slot.populated = false;
                }
                self.scope_gone(unit);
            }
            _ => {}
        }
    }

    /// A scope is empty: it is down, and its cgroup goes.
    fn scope_gone(&mut self, unit: UnitId) {
        if let Some(slot) = self.slot_mut(unit) {
            slot.deadline = None;
        }
        self.remove_group(unit);
        self.set_state(unit, ActiveState::Inactive, Sub::Dead);
    }

    /// A cgroup emptied.
    pub(super) fn emptied(&mut self, unit: UnitId) {
        self.shutdown_emptied(unit);
        let Some(kind) = self.slot(unit).map(|s| s.name.unit_type()) else {
            return;
        };
        match kind {
            UnitType::Service => self.service_emptied(unit),
            UnitType::Scope => {
                let up = self.slot(unit).is_some_and(|s| !s.perpetual && s.populated);
                if let Some(slot) = self.slot_mut(unit) {
                    slot.populated = false;
                }
                if up {
                    self.scope_gone(unit);
                }
            }
            _ => {
                if let Some(slot) = self.slot_mut(unit) {
                    slot.populated = false;
                }
            }
        }
    }

    /// The kernel's OOM kill reached a cgroup.
    pub(super) fn oom_killed(&mut self, unit: UnitId) {
        match self.slot(unit).map(|s| s.name.unit_type()) {
            Some(UnitType::Service) => self.service_oom(unit),
            Some(_) => {
                let line = format!(
                    "{}: a process was killed by the OOM killer",
                    self.display(unit)
                );
                self.log(Some(unit), line);
            }
            None => {}
        }
    }

    /// A timer ran out: every unit whose deadline has come, then shutdown.
    pub(super) fn timer(&mut self) {
        let now = self.now;
        let due: Vec<UnitId> = (0..self.units.len())
            .filter_map(|index| u32::try_from(index).ok().map(UnitId))
            .filter(|&unit| {
                self.slot(unit)
                    .and_then(|s| s.deadline)
                    .is_some_and(|at| at <= now)
            })
            .collect();
        for unit in due {
            let Some(slot) = self.slot_mut(unit) else {
                continue;
            };
            slot.deadline = None;
            match slot.name.unit_type() {
                UnitType::Service => self.service_timer(unit),
                UnitType::Scope => self.scope_timer(unit),
                _ => {}
            }
        }
        self.shutdown_timer();
    }

    /// A mount's start.
    pub(super) fn mount_start(&mut self, unit: UnitId) {
        let spec = match self
            .slot(unit)
            .and_then(|s| s.loaded.as_ref().ok())
            .map(|u| &u.config)
        {
            Some(Config::Mount(mount)) => MountSpec {
                what: mount.what.clone(),
                r#where: mount.r#where.clone(),
                fs_type: mount.fs_type.clone(),
                options: mount.options.clone(),
            },
            _ => return,
        };
        self.set_state(unit, ActiveState::Activating, Sub::Mounting);
        self.emit(Action::Mount { unit, spec });
    }

    /// A mount's stop.
    pub(super) fn mount_stop(&mut self, unit: UnitId) {
        self.set_state(unit, ActiveState::Deactivating, Sub::Unmounting);
        self.emit(Action::Unmount { unit });
    }

    /// A mount finished.
    pub(super) fn mounted(&mut self, unit: UnitId, result: Result<(), Errno>) {
        if self.slot(unit).map(|s| s.sub) != Some(Sub::Mounting) {
            return;
        }
        match result {
            Ok(()) => self.set_state(unit, ActiveState::Active, Sub::Active),
            Err(error) => {
                let line = format!("{}: mount failed (errno {})", self.display(unit), error.0);
                self.log(Some(unit), line);
                self.set_state(unit, ActiveState::Failed, Sub::Failed);
            }
        }
    }

    /// An unmount finished.
    pub(super) fn unmounted(&mut self, unit: UnitId, result: Result<(), Errno>) {
        if self.slot(unit).map(|s| s.sub) != Some(Sub::Unmounting) {
            return;
        }
        match result {
            Ok(()) => self.set_state(unit, ActiveState::Inactive, Sub::Dead),
            Err(error) => {
                let line = format!("{}: unmount failed (errno {})", self.display(unit), error.0);
                self.log(Some(unit), line);
                self.set_state(unit, ActiveState::Failed, Sub::Failed);
            }
        }
    }

    /// A device's start: watch for its node, and wait for it.
    pub(super) fn device_start(&mut self, unit: UnitId) {
        let path = match self
            .slot(unit)
            .and_then(|s| s.loaded.as_ref().ok())
            .map(|u| &u.config)
        {
            Some(Config::Device(Device { path })) => path.clone(),
            _ => return,
        };
        self.set_state(unit, ActiveState::Activating, Sub::Waiting);
        self.emit(Action::WatchDevice { unit, path });
    }

    /// A device's stop: stop watching its node. The node is the kernel's
    /// and stays.
    pub(super) fn device_stop(&mut self, unit: UnitId) {
        if matches!(
            self.slot(unit).map(|s| s.sub),
            Some(Sub::Waiting | Sub::Plugged)
        ) {
            self.emit(Action::UnwatchDevice { unit });
        }
        self.set_state(unit, ActiveState::Inactive, Sub::Dead);
    }

    /// The backend says whether a watched device's node exists.
    pub(super) fn device(&mut self, unit: UnitId, present: bool) {
        match (self.slot(unit).map(|s| s.sub), present) {
            (Some(Sub::Waiting), true) => {
                self.set_state(unit, ActiveState::Active, Sub::Plugged);
            }
            (Some(Sub::Plugged), false) => {
                let line = format!("{}: the node went away", self.display(unit));
                self.log(Some(unit), line);
                self.emit(Action::UnwatchDevice { unit });
                self.set_state(unit, ActiveState::Inactive, Sub::Dead);
            }
            _ => {}
        }
    }
}
