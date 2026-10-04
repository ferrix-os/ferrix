//! The service state machine (§5.2 to §5.5).
//!
//! A start makes the cgroup, kills what a previous run left in it, runs
//! `ExecStartPre=`, starts the main process, waits for readiness by
//! `Type=`, and runs `ExecStartPost=`. A stop runs `ExecStop=`, sends
//! `KillSignal=` by `KillMode=`, waits `TimeoutStopSec=` and then writes
//! `cgroup.kill`, and runs `ExecStopPost=`. A service is stopped when its
//! cgroup is empty, not when its main process exits: until then it is
//! `deactivating`. With `KillMode=process` or `none` only the main process
//! is waited for, and what else was in the cgroup is left there, to be
//! killed before the next start (§5.4).
//!
//! When a service ends without being asked to, `Restart=` decides, through
//! [`Policy`](crate::restart::Policy): not at all, now, at a time, or not
//! any more because the start limit is spent.

use alloc::format;
use alloc::string::String;

use super::{Manager, OpId, OpKind, Sub};
use crate::event::{
    Action, ActiveState, ClientId, Errno, Exit, OpResult, Pid, Reply, Role, SpawnSpec, UnitId, Whom,
};
use crate::exec::Command;
use crate::kind::{Config, KillMode, OomPolicy, Service, ServiceType};
use crate::restart::{Decision, Ended};
use crate::value::{Signal, Span};

/// Which command list a step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    StartPre,
    Start,
    StartPost,
    Reload,
    Stop,
    StopPost,
}

impl Manager {
    /// A service's settings.
    fn service(&self, unit: UnitId) -> Option<Service> {
        match self.slot(unit)?.loaded.as_ref().ok()?.config {
            Config::Service(ref service) => Some((**service).clone()),
            _ => None,
        }
    }

    /// Set a unit's timer `span` from now; none for `infinity`.
    fn arm(&mut self, unit: UnitId, span: Span) {
        let deadline = match span {
            Span::Finite(duration) => Some(self.now + duration),
            Span::Infinity => None,
        };
        if let Some(slot) = self.slot_mut(unit) {
            slot.deadline = deadline;
        }
    }

    /// Record how a service ended, keeping the first failure.
    fn record(&mut self, unit: UnitId, ended: Ended) {
        if let Some(slot) = self.slot_mut(unit)
            && slot.result.is_none_or(|r| r == Ended::Success)
        {
            slot.result = Some(ended);
        }
    }

    /// Make a unit's cgroup if it does not exist.
    pub(super) fn make_group(&mut self, unit: UnitId) {
        let limits = match self
            .slot(unit)
            .and_then(|s| s.loaded.as_ref().ok())
            .map(|u| &u.config)
        {
            Some(Config::Service(service)) => service.limits,
            Some(Config::Slice(slice)) => slice.limits,
            Some(Config::Scope(scope)) => scope.limits,
            _ => crate::limits::Limits::default(),
        };
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        if slot.made || slot.adopted {
            return;
        }
        let Some(path) = slot.group.clone() else {
            return;
        };
        slot.made = true;
        self.emit(Action::MakeGroup { unit, path, limits });
    }

    /// Remove a unit's cgroup if it exists and is empty.
    pub(super) fn remove_group(&mut self, unit: UnitId) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        if !slot.made || slot.populated || slot.perpetual || slot.adopted {
            return;
        }
        slot.made = false;
        self.emit(Action::RemoveGroup { unit });
    }

    /// Begin a service's start.
    pub(super) fn service_start(&mut self, id: OpId, unit: UnitId) {
        let counted = self.ops.get(&id).is_some_and(|op| op.counted);
        let now = self.now;
        let Some(service) = self.service(unit) else {
            self.finish(id, OpResult::Failed);
            return;
        };
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        let allowed = counted
            || slot
                .policy
                .as_mut()
                .is_none_or(|policy| policy.start(Some(now)));
        slot.stopping = false;
        slot.started = false;
        slot.status = None;
        slot.main = None;
        slot.control = None;
        if !allowed {
            slot.result = Some(Ended::StartLimitHit);
            let line = format!("{}: start limit hit, not starting", slot.name);
            self.log(Some(unit), line);
            self.set_state(unit, ActiveState::Failed, Sub::Failed);
            return;
        }
        slot.result = None;
        self.make_group(unit);
        self.arm(unit, service.timeout_start);
        let populated = self.slot(unit).is_some_and(|slot| slot.populated);
        if populated {
            self.set_state(unit, ActiveState::Activating, Sub::Cleaning);
            self.emit(Action::KillGroup { unit });
            return;
        }
        self.run_list(unit, &service, List::StartPre, 0);
    }

    /// Run command `index` of a list, or go on to what follows the list.
    fn run_list(&mut self, unit: UnitId, service: &Service, list: List, index: usize) {
        let commands = match list {
            List::StartPre => &service.exec_start_pre,
            List::Start => &service.exec_start,
            List::StartPost => &service.exec_start_post,
            List::Reload => &service.exec_reload,
            List::Stop => &service.exec_stop,
            List::StopPost => &service.exec_stop_post,
        };
        let oneshot = service.service_type == ServiceType::Oneshot;
        let command = match list {
            List::Start if !oneshot && index > 0 => None,
            _ => commands.get(index).cloned(),
        };
        let (active, sub) = match list {
            List::StartPre => (ActiveState::Activating, Sub::StartPre),
            List::Start => (ActiveState::Activating, Sub::Start),
            List::StartPost => (ActiveState::Activating, Sub::StartPost),
            List::Reload => (ActiveState::Active, Sub::Reload),
            List::Stop => (ActiveState::Deactivating, Sub::Stop),
            List::StopPost => (ActiveState::Deactivating, Sub::StopPost),
        };
        let Some(command) = command else {
            self.after_list(unit, service, list);
            return;
        };
        if let Some(slot) = self.slot_mut(unit) {
            slot.step = index;
        }
        self.set_state(unit, active, sub);
        let role = if list == List::Start {
            Role::Main
        } else {
            Role::Control
        };
        self.spawn(unit, service, command, role);
    }

    /// What follows a list's last command.
    fn after_list(&mut self, unit: UnitId, service: &Service, list: List) {
        match list {
            List::StartPre => self.run_list(unit, service, List::Start, 0),
            List::Start => {
                // Only a oneshot, or a service with no ExecStart=, gets here.
                self.run_list(unit, service, List::StartPost, 0);
            }
            List::StartPost => self.running(unit, service),
            List::Reload => self.reloaded(unit, OpResult::Done),
            List::Stop => self.sigterm(unit, service),
            List::StopPost => self.stopped(unit),
        }
    }

    /// Ask the backend for a process.
    fn spawn(&mut self, unit: UnitId, service: &Service, command: Command, role: Role) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        slot.spawning = Some(role);
        let group = slot
            .group
            .clone()
            .unwrap_or_else(crate::event::GroupPath::root);
        let mut environment = service.environment.clone();
        if let Some(main) = slot.main {
            environment.push((String::from("MAINPID"), format!("{}", main.0)));
        }
        // Only the main process takes the sockets and the connection, as
        // under systemd.
        let connection = match role {
            Role::Main => slot.connection.take(),
            Role::Control => None,
        };
        let sockets = match role {
            Role::Main => self.sockets_for(unit),
            Role::Control => alloc::vec::Vec::new(),
        };
        let sandbox = if command.privilege == crate::exec::Privilege::Full {
            crate::kind::Sandbox::default()
        } else {
            service.sandbox.clone()
        };
        let spec = SpawnSpec {
            command,
            role,
            group,
            service_type: service.service_type,
            user: service.user.clone(),
            group_name: service.group.clone(),
            supplementary_groups: service.supplementary_groups.clone(),
            working_directory: service.working_directory.clone(),
            environment,
            environment_files: service.environment_files.clone(),
            stdin: service.standard_input.clone(),
            stdout: service.standard_output.clone(),
            stderr: service.standard_error.clone(),
            tty: service.tty().map(String::from),
            notify_fd: service.notify_fd,
            sockets,
            connection,
            bootstrap: !service.uses.is_empty() || !service.offers.is_empty(),
            sandbox,
        };
        self.emit(Action::Spawn {
            unit,
            spec: alloc::boxed::Box::new(spec),
        });
    }

    /// A spawned process exists.
    pub(super) fn spawned(&mut self, unit: UnitId, pid: Pid) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        let Some(role) = slot.spawning.take() else {
            // No run waits for it: the one that asked ended first, and its
            // cgroup may be gone. Left alone it would run unsupervised, so
            // it is ended, as systemd ends what a stopped unit leaves.
            self.emit(Action::Signal {
                unit,
                signal: Signal::KILL,
                whom: Whom::Process(pid),
            });
            return;
        };
        slot.populated = true;
        match role {
            Role::Main => slot.main = Some(pid),
            Role::Control => slot.control = Some(pid),
        }
        let _ = self.pids.insert(pid, (unit, role));
        let Some(service) = self.service(unit) else {
            return;
        };
        if role == Role::Main && service.service_type == ServiceType::Simple {
            self.run_list(unit, &service, List::StartPost, 0);
        }
    }

    /// A spawned process's `execve` succeeded.
    pub(super) fn execed(&mut self, unit: UnitId, pid: Pid) {
        let Some(service) = self.service(unit) else {
            return;
        };
        let starting = self
            .slot(unit)
            .is_some_and(|s| s.sub == Sub::Start && s.main == Some(pid));
        if starting && service.service_type == ServiceType::Exec {
            self.run_list(unit, &service, List::StartPost, 0);
        }
    }

    /// A `notify` or `native` service said it is ready.
    pub(super) fn ready(&mut self, unit: UnitId, status: Option<String>) {
        let Some(service) = self.service(unit) else {
            return;
        };
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        if status.is_some() {
            slot.status = status;
        }
        let waiting = slot.sub == Sub::Start
            && matches!(
                service.service_type,
                ServiceType::Notify | ServiceType::Native
            );
        if waiting {
            self.run_list(unit, &service, List::StartPost, 0);
        }
    }

    /// A `forking` service's main process was found.
    pub(super) fn main_pid(&mut self, unit: UnitId, pid: Pid) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        if let Some(old) = slot.main.replace(pid) {
            let _ = self.pids.remove(&old);
        }
        let _ = self.pids.insert(pid, (unit, Role::Main));
    }

    /// A spawn failed.
    pub(super) fn spawn_failed(&mut self, unit: UnitId, error: Errno) {
        let Some(service) = self.service(unit) else {
            return;
        };
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        let Some(role) = slot.spawning.take() else {
            return;
        };
        let (sub, step) = (slot.sub, slot.step);
        let line = format!(
            "{}: could not start a process (errno {})",
            self.display(unit),
            error.0
        );
        self.log(Some(unit), line);
        match (role, sub) {
            (Role::Control, Sub::Stop) => self.run_list(unit, &service, List::Stop, step + 1),
            (Role::Control, Sub::StopPost) => {
                self.run_list(unit, &service, List::StopPost, step + 1);
            }
            _ => self.fail(unit, &service, Ended::Resources),
        }
    }

    /// A process the manager started was reaped.
    pub(super) fn exited(&mut self, pid: Pid, how: Exit) {
        let Some((unit, role)) = self.pids.remove(&pid) else {
            return;
        };
        let Some(service) = self.service(unit) else {
            return;
        };
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        let (sub, step) = (slot.sub, slot.step);
        match role {
            Role::Main if slot.main == Some(pid) => slot.main = None,
            Role::Control if slot.control == Some(pid) => slot.control = None,
            _ => return,
        }
        let list = match sub {
            Sub::StartPre => &service.exec_start_pre,
            Sub::Start => &service.exec_start,
            Sub::StartPost => &service.exec_start_post,
            Sub::Reload => &service.exec_reload,
            Sub::Stop => &service.exec_stop,
            Sub::StopPost => &service.exec_stop_post,
            _ => &service.exec_start,
        };
        let ignore = list.get(step).is_some_and(|command| command.ignore_failure);
        let ended = match Ended::of(how) {
            Ended::Success => Ended::Success,
            _ if ignore => Ended::Success,
            other => other,
        };
        match role {
            Role::Control => self.control_exited(unit, &service, sub, step, ended),
            Role::Main => self.main_exited(unit, &service, sub, step, ended),
        }
    }

    /// A control process ended.
    fn control_exited(
        &mut self,
        unit: UnitId,
        service: &Service,
        sub: Sub,
        step: usize,
        ended: Ended,
    ) {
        match sub {
            Sub::StartPre | Sub::StartPost if ended != Ended::Success => {
                self.fail(unit, service, ended);
            }
            Sub::Reload if ended != Ended::Success => {
                let line = format!("{}: reload failed ({})", self.display(unit), ended.name());
                self.log(Some(unit), line);
                self.reloaded(unit, OpResult::Failed);
            }
            Sub::Reload => self.run_list(unit, service, List::Reload, step + 1),
            Sub::StartPre => self.run_list(unit, service, List::StartPre, step + 1),
            Sub::StartPost => self.run_list(unit, service, List::StartPost, step + 1),
            Sub::Stop => self.run_list(unit, service, List::Stop, step + 1),
            Sub::StopPost => self.run_list(unit, service, List::StopPost, step + 1),
            Sub::StopSigterm | Sub::StopSigkill => self.check_stopped(unit, service),
            _ => {}
        }
    }

    /// The main process ended.
    fn main_exited(
        &mut self,
        unit: UnitId,
        service: &Service,
        sub: Sub,
        step: usize,
        ended: Ended,
    ) {
        match sub {
            Sub::Start => match service.service_type {
                ServiceType::Oneshot if ended == Ended::Success => {
                    self.run_list(unit, service, List::Start, step + 1);
                }
                ServiceType::Forking if ended == Ended::Success => {
                    // The parent is gone; the daemon it left is the service.
                    self.run_list(unit, service, List::StartPost, 0);
                }
                _ if ended == Ended::Success => self.fail(unit, service, Ended::ExitCode),
                _ => self.fail(unit, service, ended),
            },
            Sub::StartPost => self.record(unit, ended),
            Sub::Running | Sub::Reload => {
                self.reloaded(unit, OpResult::Failed);
                self.record(unit, ended);
                if service.remain_after_exit && ended == Ended::Success {
                    self.set_state(unit, ActiveState::Active, Sub::Exited);
                } else {
                    self.stop_sequence(unit, service);
                }
            }
            Sub::StopSigterm | Sub::StopSigkill => self.check_stopped(unit, service),
            _ => {}
        }
    }

    /// The service's cgroup emptied.
    pub(super) fn service_emptied(&mut self, unit: UnitId) {
        let Some(service) = self.service(unit) else {
            return;
        };
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        slot.populated = false;
        let (sub, main) = (slot.sub, slot.main);
        match sub {
            Sub::Cleaning => self.run_list(unit, &service, List::StartPre, 0),
            Sub::Running if main.is_none() => self.stop_sequence(unit, &service),
            Sub::StopSigterm | Sub::StopSigkill => self.check_stopped(unit, &service),
            Sub::Dead | Sub::Failed => self.remove_group(unit),
            _ => {}
        }
    }

    /// `svc reload`: run `ExecReload=` of a service that is up.
    pub(super) fn reload_service(&mut self, client: ClientId, name: &str) {
        let Some(unit) = self.ensure(name) else {
            self.reply(client, Reply::Refused(format!("{name}: not a unit name")));
            return;
        };
        let Some(service) = self.service(unit) else {
            self.reply(
                client,
                Reply::Refused(format!("{name}: only a service reloads")),
            );
            return;
        };
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        match slot.sub {
            Sub::Reload => slot.reloading.push(client),
            Sub::Running | Sub::Exited if service.exec_reload.is_empty() => {
                self.reply(client, Reply::Refused(format!("{name} has no ExecReload=")));
            }
            Sub::Running | Sub::Exited => {
                slot.reloading.push(client);
                self.run_list(unit, &service, List::Reload, 0);
            }
            _ => self.reply(client, Reply::Refused(format!("{name} is not running"))),
        }
    }

    /// A reload ended: back to running, and answer who asked.
    fn reloaded(&mut self, unit: UnitId, result: OpResult) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        let clients = core::mem::take(&mut slot.reloading);
        if slot.sub == Sub::Reload {
            let sub = if slot.main.is_some() || slot.populated {
                Sub::Running
            } else {
                Sub::Exited
            };
            self.set_state(unit, ActiveState::Active, sub);
        }
        for client in clients {
            self.reply(client, Reply::Done(result));
        }
    }

    /// Up: readiness reached and `ExecStartPost=` done.
    fn running(&mut self, unit: UnitId, service: &Service) {
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        slot.started = true;
        slot.deadline = None;
        let oneshot = service.service_type == ServiceType::Oneshot;
        let nothing_left = slot.main.is_none() && (oneshot || !slot.populated);
        let failed = slot.result.is_some_and(|r| r != Ended::Success);
        if failed || (nothing_left && !service.remain_after_exit) {
            self.stop_sequence(unit, service);
        } else if nothing_left {
            self.set_state(unit, ActiveState::Active, Sub::Exited);
        } else {
            self.set_state(unit, ActiveState::Active, Sub::Running);
        }
    }

    /// A start failed: record why and stop what is there.
    fn fail(&mut self, unit: UnitId, service: &Service, ended: Ended) {
        self.record(unit, ended);
        self.stop_sequence(unit, service);
    }

    /// Begin stopping: `ExecStop=` if the service got as far as running,
    /// then the signals.
    fn stop_sequence(&mut self, unit: UnitId, service: &Service) {
        self.arm(unit, service.timeout_stop);
        let started = self.slot(unit).is_some_and(|s| s.started);
        if started {
            self.run_list(unit, service, List::Stop, 0);
        } else {
            self.sigterm(unit, service);
        }
    }

    /// A stop was asked for.
    pub(super) fn service_stop(&mut self, unit: UnitId) {
        let Some(service) = self.service(unit) else {
            return;
        };
        self.reloaded(unit, OpResult::Canceled);
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        slot.stopping = true;
        match slot.sub {
            Sub::AutoRestart => {
                slot.deadline = None;
                self.set_state(unit, ActiveState::Inactive, Sub::Dead);
            }
            Sub::Stop | Sub::StopSigterm | Sub::StopSigkill | Sub::StopPost => {}
            _ => self.stop_sequence(unit, &service),
        }
    }

    /// Send the polite signal, by `KillMode=`.
    fn sigterm(&mut self, unit: UnitId, service: &Service) {
        self.set_state(unit, ActiveState::Deactivating, Sub::StopSigterm);
        self.arm(unit, service.timeout_stop);
        let Some(slot) = self.slot(unit) else {
            return;
        };
        let (main, populated) = (slot.main, slot.populated);
        let kill = service.kill;
        let whom = match (kill.mode, main) {
            (KillMode::None, _) | (KillMode::Process, None) => None,
            (KillMode::Process | KillMode::Mixed, Some(main)) => Some(Whom::Process(main)),
            (KillMode::ControlGroup | KillMode::Mixed, _) if populated => Some(Whom::Group),
            _ => None,
        };
        if let Some(whom) = whom {
            self.emit(Action::Signal {
                unit,
                signal: kill.signal,
                whom,
            });
            if kill.send_sighup {
                self.emit(Action::Signal {
                    unit,
                    signal: Signal::HUP,
                    whom,
                });
            }
        }
        self.check_stopped(unit, service);
    }

    /// `TimeoutStopSec=` ran out after the polite signal.
    fn sigkill(&mut self, unit: UnitId, service: &Service) {
        self.record(unit, Ended::Timeout);
        self.set_state(unit, ActiveState::Deactivating, Sub::StopSigkill);
        self.arm(unit, service.timeout_stop);
        let Some(slot) = self.slot(unit) else {
            return;
        };
        let (main, populated) = (slot.main, slot.populated);
        match (service.kill.mode, main) {
            (KillMode::Process, Some(main)) => self.emit(Action::Signal {
                unit,
                signal: service.kill.final_signal,
                whom: Whom::Process(main),
            }),
            (KillMode::ControlGroup | KillMode::Mixed, _) if populated || main.is_some() => {
                self.emit(Action::KillGroup { unit });
            }
            _ => {}
        }
        self.check_stopped(unit, service);
    }

    /// Whether what the kill mode waits for has gone; if so, go on.
    fn check_stopped(&mut self, unit: UnitId, service: &Service) {
        let Some(slot) = self.slot(unit) else {
            return;
        };
        if !matches!(slot.sub, Sub::StopSigterm | Sub::StopSigkill) {
            return;
        }
        let gone = match service.kill.mode {
            KillMode::None => true,
            KillMode::Process => slot.main.is_none(),
            KillMode::ControlGroup | KillMode::Mixed => {
                !slot.populated && slot.main.is_none() && slot.control.is_none()
            }
        };
        if gone {
            self.run_list(unit, service, List::StopPost, 0);
        }
    }

    /// Everything ran: the service is down. Decide about restarting.
    fn stopped(&mut self, unit: UnitId) {
        let now = self.now;
        let shutting_down = self.shutdown.is_some();
        let asked = self
            .slot(unit)
            .and_then(|s| s.op)
            .and_then(|id| self.ops.get(&id))
            .is_some_and(|op| matches!(op.kind, OpKind::Stop | OpKind::Restart));
        let Some(slot) = self.slot_mut(unit) else {
            return;
        };
        slot.deadline = None;
        slot.started = false;
        // A spawn asked for and not yet answered belongs to this run, which
        // has ended: its answer, if it comes, must not start the unit again
        // with no cgroup to run in.
        slot.spawning = None;
        let result = slot.result.unwrap_or(Ended::Success);
        let restarting = !slot.stopping && !asked && !shutting_down;
        let decision = match slot.policy.as_mut() {
            Some(policy) if restarting => policy.decide(result, Some(now)),
            _ => Decision::Stay,
        };
        self.remove_group(unit);
        match decision {
            Decision::Stay => {
                let failed = result != Ended::Success;
                let (active, sub) = if failed {
                    (ActiveState::Failed, Sub::Failed)
                } else {
                    (ActiveState::Inactive, Sub::Dead)
                };
                self.set_state(unit, active, sub);
            }
            Decision::GiveUp => {
                if let Some(slot) = self.slot_mut(unit) {
                    slot.result = Some(Ended::StartLimitHit);
                }
                self.set_state(unit, ActiveState::Failed, Sub::Failed);
            }
            Decision::Now => self.restart_now(unit),
            Decision::At(at) => {
                if let Some(slot) = self.slot_mut(unit) {
                    slot.deadline = Some(at);
                }
                let line = format!(
                    "{}: {}; restarting at {at}",
                    self.display(unit),
                    result.name()
                );
                self.log(Some(unit), line);
                self.set_state(unit, ActiveState::Activating, Sub::AutoRestart);
            }
        }
    }

    /// Start a service again, its restart already counted.
    fn restart_now(&mut self, unit: UnitId) {
        self.set_state(unit, ActiveState::Activating, Sub::AutoRestart);
        if let Some(slot) = self.slot_mut(unit) {
            slot.deadline = None;
        }
        match self.transaction(unit, OpKind::Start, super::Mode::Replace) {
            Ok(Some(id)) => {
                if let Some(op) = self.ops.get_mut(&id) {
                    op.counted = true;
                }
            }
            Ok(None) => {}
            Err(why) => {
                self.log(Some(unit), why);
                self.set_state(unit, ActiveState::Failed, Sub::Failed);
            }
        }
    }

    /// A service's timer ran out.
    pub(super) fn service_timer(&mut self, unit: UnitId) {
        let Some(service) = self.service(unit) else {
            return;
        };
        let Some(sub) = self.slot(unit).map(|s| s.sub) else {
            return;
        };
        match sub {
            Sub::AutoRestart => self.restart_now(unit),
            Sub::Cleaning | Sub::StartPre | Sub::Start | Sub::StartPost => {
                let line = format!("{}: start timed out", self.display(unit));
                self.log(Some(unit), line);
                self.record(unit, Ended::Timeout);
                self.sigterm(unit, &service);
            }
            Sub::Stop => {
                self.record(unit, Ended::Timeout);
                self.sigterm(unit, &service);
            }
            Sub::StopSigterm => self.sigkill(unit, &service),
            Sub::StopSigkill => {
                let line = format!(
                    "{}: processes still around after SIGKILL",
                    self.display(unit)
                );
                self.log(Some(unit), line);
                if let Some(slot) = self.slot_mut(unit) {
                    slot.main = None;
                    slot.control = None;
                }
                self.run_list(unit, &service, List::StopPost, 0);
            }
            Sub::StopPost => self.stopped(unit),
            _ => {}
        }
    }

    /// The kernel's OOM kill reached a service (§5.5).
    pub(super) fn service_oom(&mut self, unit: UnitId) {
        let Some(service) = self.service(unit) else {
            return;
        };
        let line = format!(
            "{}: a process was killed by the OOM killer",
            self.display(unit)
        );
        self.log(Some(unit), line);
        let Some(sub) = self.slot(unit).map(|s| s.sub) else {
            return;
        };
        let up = matches!(
            sub,
            Sub::StartPre | Sub::Start | Sub::StartPost | Sub::Running | Sub::Exited
        );
        match service.oom_policy {
            OomPolicy::Continue => {}
            OomPolicy::Stop if up => {
                self.record(unit, Ended::OomKill);
                self.stop_sequence(unit, &service);
            }
            OomPolicy::Kill if up => {
                self.record(unit, Ended::OomKill);
                self.sigkill(unit, &service);
            }
            _ => {}
        }
    }
}
