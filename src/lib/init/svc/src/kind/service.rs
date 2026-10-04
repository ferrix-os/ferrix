//! `.service`: processes init starts, each service in a cgroup of its own
//! (§4.4, §5).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use super::sandbox::{self, Sandbox};
use super::{Config, UnitError};
use crate::Warnings;
use crate::exec::{self, Command};
use crate::ini::{Assignment, Section};
use crate::keys::{self, Setter};
use crate::limits::{self, Limits};
use crate::name::{UnitName, UnitType};
use crate::value::{self, Signal, Span, ValueError};

/// systemd's `DefaultTimeoutStartSec=` and `DefaultTimeoutStopSec=`.
pub(crate) const DEFAULT_TIMEOUT: Span = Span::Finite(Duration::from_secs(90));

/// systemd's default `RestartSec=`.
pub(crate) const DEFAULT_RESTART_SEC: Duration = Duration::from_millis(100);

/// `Type=`: when a service counts as started (§5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServiceType {
    /// Started as soon as it is spawned.
    #[default]
    Simple,
    /// Started once `execve` succeeded.
    Exec,
    /// Started when the process it spawned exits 0, leaving a daemon.
    Forking,
    /// Started when its commands have run; nothing is left running.
    Oneshot,
    /// Started when it writes `READY=1` to its notify descriptor.
    Notify,
    /// A native program in the cgroup's job, started on its READY message.
    Native,
}

impl ServiceType {
    fn parse(text: &str) -> Result<ServiceType, ValueError> {
        Ok(match text {
            "simple" | "idle" => ServiceType::Simple,
            "exec" => ServiceType::Exec,
            "forking" => ServiceType::Forking,
            "oneshot" => ServiceType::Oneshot,
            "notify" | "notify-reload" => ServiceType::Notify,
            "native" => ServiceType::Native,
            _ => return Err(ValueError::Invalid),
        })
    }
}

pub use ferrix_restart::Restart;

/// `Restart=`'s value. A free function, since [`Restart`] is
/// `ferrix-restart`'s, which parses nothing.
fn parse_restart(text: &str) -> Result<Restart, ValueError> {
    Ok(match text {
        "no" => Restart::No,
        "on-success" => Restart::OnSuccess,
        "on-failure" => Restart::OnFailure,
        "on-abnormal" => Restart::OnAbnormal,
        "on-watchdog" => Restart::OnWatchdog,
        "on-abort" => Restart::OnAbort,
        "always" => Restart::Always,
        _ => return Err(ValueError::Invalid),
    })
}

/// `KillMode=`: who is signalled to stop (§5.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KillMode {
    /// Every process in the cgroup.
    #[default]
    ControlGroup,
    /// The main process, then `cgroup.kill` for the rest.
    Mixed,
    /// The main process only.
    Process,
    /// Nobody; only `ExecStop=` runs.
    None,
}

/// How a unit's processes are asked, then made, to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kill {
    /// `KillMode=`.
    pub mode: KillMode,
    /// `KillSignal=`: the polite signal, `SIGTERM` by default.
    pub signal: Signal,
    /// `FinalKillSignal=`: what is left after the timeout gets this,
    /// `SIGKILL` by default; with `cgroup.kill` it is always `SIGKILL`.
    pub final_signal: Signal,
    /// `SendSIGHUP=`: a `SIGHUP` after the polite signal, for shells.
    pub send_sighup: bool,
}

impl Default for Kill {
    fn default() -> Self {
        Self {
            mode: KillMode::ControlGroup,
            signal: Signal::TERM,
            final_signal: Signal::KILL,
            send_sighup: false,
        }
    }
}

/// The kill keys, shared by services and scopes.
pub(crate) const KILL_KEYS: [(&str, Setter<Kill>); 4] = [
    ("KillMode", |kill, a, w| {
        let mode = keys::parsed(a, w, |text| {
            Ok::<_, ValueError>(match text {
                "control-group" => KillMode::ControlGroup,
                "mixed" => KillMode::Mixed,
                "process" => KillMode::Process,
                "none" => KillMode::None,
                _ => return Err(ValueError::Invalid),
            })
        });
        kill.mode = mode.unwrap_or(kill.mode);
    }),
    ("KillSignal", |kill, a, w| {
        kill.signal = keys::parsed(a, w, value::signal).unwrap_or(kill.signal);
    }),
    ("FinalKillSignal", |kill, a, w| {
        kill.final_signal = keys::parsed(a, w, value::signal).unwrap_or(kill.final_signal);
    }),
    ("SendSIGHUP", |kill, a, w| {
        kill.send_sighup = keys::boolean(a, w).unwrap_or(kill.send_sighup);
    }),
];

/// `OOMPolicy=`: what the kernel's OOM kill of one process means (§5.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OomPolicy {
    /// Carry on with what is left.
    Continue,
    /// Stop the whole service, recording `oom-kill`.
    #[default]
    Stop,
    /// Kill the whole service at once.
    Kill,
}

/// `StandardInput=`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Input {
    /// `/dev/null`.
    #[default]
    Null,
    /// `TTYPath=`, as the controlling terminal of a new session.
    Tty,
    /// As `Tty`, taking the terminal from whoever has it.
    TtyForce,
    /// As `Tty`, failing if someone else has it.
    TtyFail,
    /// `file:path`.
    File(String),
    /// `socket`: the connection a `.socket` unit with `Accept=yes` took.
    Socket,
}

/// `StandardOutput=` and `StandardError=`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Output {
    /// Standard output: as standard input if that is a terminal, else the
    /// log. Standard error: as standard output.
    #[default]
    Inherit,
    /// `/dev/null`.
    Null,
    /// `TTYPath=`.
    Tty,
    /// `/dev/console`.
    Console,
    /// The manager's log (§10): `log`, and systemd's `journal` and `kmsg`,
    /// with or without `+console`, since the log reaches the console.
    Log,
    /// `file:path`: written from the start.
    File(String),
    /// `append:path`.
    Append(String),
    /// `truncate:path`.
    Truncate(String),
    /// `socket`: the connection a `.socket` unit with `Accept=yes` took.
    Socket,
}

/// `WorkingDirectory=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingDirectory {
    /// The path, or `~` for the user's home.
    pub path: String,
    /// `-` before it: carry on if it does not exist.
    pub missing_ok: bool,
}

/// A service's settings: the keys of §4.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// `Type=`.
    pub service_type: ServiceType,
    /// `ExecStartPre=`.
    pub exec_start_pre: Vec<Command>,
    /// `ExecStart=`: one command, or any number for `oneshot`.
    pub exec_start: Vec<Command>,
    /// `ExecStartPost=`.
    pub exec_start_post: Vec<Command>,
    /// `ExecStop=`.
    pub exec_stop: Vec<Command>,
    /// `ExecStopPost=`.
    pub exec_stop_post: Vec<Command>,
    /// `ExecReload=`.
    pub exec_reload: Vec<Command>,
    /// `RemainAfterExit=`: active after its processes have gone.
    pub remain_after_exit: bool,
    /// `PIDFile=`, for `forking`.
    pub pid_file: Option<String>,
    /// `NotifyFd=`: the descriptor a `notify` service writes `READY=1` to.
    pub notify_fd: Option<u32>,
    /// `Restart=`.
    pub restart: Restart,
    /// `RestartSec=`: the first delay, doubled on each restart up to 32
    /// times it (§5.4).
    pub restart_sec: Duration,
    /// `TimeoutStartSec=`.
    pub timeout_start: Span,
    /// `TimeoutStopSec=`: how long before `cgroup.kill`.
    pub timeout_stop: Span,
    /// `KillMode=`, `KillSignal=` and their siblings.
    pub kill: Kill,
    /// `Slice=`; `None` is `system.slice`.
    pub slice: Option<UnitName>,
    /// The resource keys.
    pub limits: Limits,
    /// `OOMPolicy=`.
    pub oom_policy: OomPolicy,
    /// `Delegate=`.
    pub delegate: bool,
    /// `User=`.
    pub user: Option<String>,
    /// `Group=`.
    pub group: Option<String>,
    /// `SupplementaryGroups=`.
    pub supplementary_groups: Vec<String>,
    /// `WorkingDirectory=`.
    pub working_directory: Option<WorkingDirectory>,
    /// `Environment=`, in order; a later assignment to a name wins.
    pub environment: Vec<(String, String)>,
    /// `EnvironmentFile=`: paths, and whether a missing one is fine (`-`).
    pub environment_files: Vec<(String, bool)>,
    /// `StandardInput=`.
    pub standard_input: Input,
    /// `StandardOutput=`.
    pub standard_output: Output,
    /// `StandardError=`.
    pub standard_error: Output,
    /// `TTYPath=`; `/dev/console` when a terminal is wanted and none named.
    pub tty_path: Option<String>,
    /// `TTYReset=`.
    pub tty_reset: bool,
    /// `TTYVHangup=`.
    pub tty_vhangup: bool,
    /// `Offers=`: names in the directory (§6).
    pub offers: Vec<String>,
    /// `Uses=`: names in the directory it may open.
    pub uses: Vec<String>,
    /// The sandboxing keys (§4.5).
    pub sandbox: Sandbox,
}

impl Default for Service {
    fn default() -> Self {
        Self {
            service_type: ServiceType::Simple,
            exec_start_pre: Vec::new(),
            exec_start: Vec::new(),
            exec_start_post: Vec::new(),
            exec_stop: Vec::new(),
            exec_stop_post: Vec::new(),
            exec_reload: Vec::new(),
            remain_after_exit: false,
            pid_file: None,
            notify_fd: None,
            restart: Restart::No,
            restart_sec: DEFAULT_RESTART_SEC,
            timeout_start: DEFAULT_TIMEOUT,
            timeout_stop: DEFAULT_TIMEOUT,
            kill: Kill::default(),
            slice: None,
            limits: Limits::default(),
            oom_policy: OomPolicy::Stop,
            delegate: false,
            user: None,
            group: None,
            supplementary_groups: Vec::new(),
            working_directory: None,
            environment: Vec::new(),
            environment_files: Vec::new(),
            standard_input: Input::Null,
            standard_output: Output::Inherit,
            standard_error: Output::Inherit,
            tty_path: None,
            tty_reset: false,
            tty_vhangup: false,
            offers: Vec::new(),
            uses: Vec::new(),
            sandbox: Sandbox::default(),
        }
    }
}

impl Service {
    /// The terminal the service's standard streams name, if any.
    pub fn tty(&self) -> Option<&str> {
        let wants = matches!(
            self.standard_input,
            Input::Tty | Input::TtyForce | Input::TtyFail
        ) || self.standard_output == Output::Tty
            || self.standard_error == Output::Tty;
        wants.then(|| self.tty_path.as_deref().unwrap_or("/dev/console"))
    }
}

/// `TimeoutStartSec=` and `TimeoutStopSec=` read zero as no timeout.
pub(crate) fn zero_is_infinity(span: Span) -> Span {
    match span {
        Span::Finite(Duration::ZERO) => Span::Infinity,
        other => other,
    }
}

/// `Slice=`: a slice unit's name, or empty for the default.
pub(crate) fn slice(assignment: &Assignment, warnings: &mut Warnings) -> Option<Option<UnitName>> {
    if assignment.value.is_empty() {
        return Some(None);
    }
    match UnitName::parse(&assignment.value) {
        Ok(name) if name.unit_type() == UnitType::Slice => Some(Some(name)),
        Ok(_) => {
            keys::invalid(assignment, warnings, "not a slice");
            None
        }
        Err(error) => {
            keys::invalid(assignment, warnings, error);
            None
        }
    }
}

/// `Delegate=`: a boolean, or a list of controllers, which delegates.
pub(crate) fn delegate(assignment: &Assignment, warnings: &mut Warnings) -> Option<bool> {
    if let Ok(yes) = value::boolean(&assignment.value) {
        return Some(yes);
    }
    let controllers = [
        "cpu", "io", "memory", "pids", "cpuset", "hugetlb", "rdma", "misc",
    ];
    if assignment
        .value
        .split([' ', '\t'])
        .filter(|word| !word.is_empty())
        .all(|word| controllers.contains(&word))
    {
        Some(true)
    } else {
        keys::invalid(
            assignment,
            warnings,
            "not a boolean or a list of controllers",
        );
        None
    }
}

/// Apply an `Exec…=` assignment to one of the command lists.
fn exec(list: &mut Vec<Command>, assignment: &Assignment, warnings: &mut Warnings) {
    if assignment.value.is_empty() {
        list.clear();
        return;
    }
    match exec::commands(&assignment.value) {
        Ok(commands) => list.extend(commands),
        Err(why) => warnings.at(assignment, format!("{why}, ignoring.")),
    }
}

/// The `Exec…=` keys.
const EXEC_KEYS: [(&str, Setter<Service>); 6] = [
    ("ExecStartPre", |s, a, w| exec(&mut s.exec_start_pre, a, w)),
    ("ExecStart", |s, a, w| exec(&mut s.exec_start, a, w)),
    ("ExecStartPost", |s, a, w| {
        exec(&mut s.exec_start_post, a, w);
    }),
    ("ExecStop", |s, a, w| exec(&mut s.exec_stop, a, w)),
    ("ExecStopPost", |s, a, w| exec(&mut s.exec_stop_post, a, w)),
    ("ExecReload", |s, a, w| exec(&mut s.exec_reload, a, w)),
];

/// The keys for how a service runs and ends.
const RUN_KEYS: [(&str, Setter<Service>); 13] = [
    ("Type", |s, a, w| {
        s.service_type = keys::parsed(a, w, ServiceType::parse).unwrap_or(s.service_type);
    }),
    ("RemainAfterExit", |s, a, w| {
        s.remain_after_exit = keys::boolean(a, w).unwrap_or(s.remain_after_exit);
    }),
    ("PIDFile", |s, a, w| {
        if let Some(path) = keys::path(a, w) {
            s.pid_file = path;
        }
    }),
    ("NotifyFd", |s, a, w| {
        if a.value.is_empty() {
            s.notify_fd = None;
        } else if let Some(fd) = keys::parsed(a, w, |text| {
            text.parse::<u32>()
                .ok()
                .filter(|&fd| fd > 2)
                .ok_or(ValueError::Invalid)
        }) {
            s.notify_fd = Some(fd);
        }
    }),
    ("Restart", |s, a, w| {
        s.restart = keys::parsed(a, w, parse_restart).unwrap_or(s.restart);
    }),
    ("RestartSec", |s, a, w| match keys::span(a, w) {
        Some(Span::Finite(delay)) => s.restart_sec = delay,
        Some(Span::Infinity) => keys::invalid(a, w, "a restart delay must be finite"),
        None => {}
    }),
    ("TimeoutStartSec", |s, a, w| {
        if let Some(span) = keys::span(a, w) {
            s.timeout_start = zero_is_infinity(span);
        }
    }),
    ("TimeoutStopSec", |s, a, w| {
        if let Some(span) = keys::span(a, w) {
            s.timeout_stop = zero_is_infinity(span);
        }
    }),
    ("TimeoutSec", |s, a, w| {
        if let Some(span) = keys::span(a, w) {
            s.timeout_start = zero_is_infinity(span);
            s.timeout_stop = zero_is_infinity(span);
        }
    }),
    ("Slice", |s, a, w| {
        if let Some(slice) = slice(a, w) {
            s.slice = slice;
        }
    }),
    ("OOMPolicy", |s, a, w| {
        let policy = keys::parsed(a, w, |text| {
            Ok::<_, ValueError>(match text {
                "continue" => OomPolicy::Continue,
                "stop" => OomPolicy::Stop,
                "kill" => OomPolicy::Kill,
                _ => return Err(ValueError::Invalid),
            })
        });
        s.oom_policy = policy.unwrap_or(s.oom_policy);
    }),
    ("Delegate", |s, a, w| {
        s.delegate = delegate(a, w).unwrap_or(s.delegate);
    }),
    // `[Unit]`'s, accepted here as systemd accepts them; the loader applies
    // them to the unit's start limit.
    ("StartLimitBurst", |_, _, _| {}),
];

/// The keys for what a service's processes run as and with.
const CONTEXT_KEYS: [(&str, Setter<Service>); 16] = [
    ("User", |s, a, _| s.user = keys::string(a)),
    ("Group", |s, a, _| s.group = keys::string(a)),
    ("SupplementaryGroups", |s, a, _| {
        keys::words(&mut s.supplementary_groups, a);
    }),
    ("WorkingDirectory", |s, a, w| {
        let (missing_ok, path) = match a.value.strip_prefix('-') {
            Some(path) => (true, path),
            None => (false, a.value.as_str()),
        };
        if path.is_empty() {
            s.working_directory = None;
        } else if path == "~" || value::is_absolute_path(path) {
            s.working_directory = Some(WorkingDirectory {
                path: String::from(path),
                missing_ok,
            });
        } else {
            keys::invalid(a, w, "not an absolute path or ~");
        }
    }),
    ("Environment", |s, a, w| {
        if a.value.is_empty() {
            s.environment.clear();
            return;
        }
        match exec::environment(&a.value) {
            Ok((good, bad)) => {
                s.environment.extend(good);
                for word in bad {
                    w.at(
                        a,
                        format!("Invalid environment assignment, ignoring: {word}"),
                    );
                }
            }
            Err(why) => w.at(a, format!("{why}, ignoring.")),
        }
    }),
    ("EnvironmentFile", |s, a, w| {
        if a.value.is_empty() {
            s.environment_files.clear();
            return;
        }
        let (missing_ok, path) = match a.value.strip_prefix('-') {
            Some(path) => (true, path),
            None => (false, a.value.as_str()),
        };
        if value::is_absolute_path(path) {
            s.environment_files.push((String::from(path), missing_ok));
        } else {
            keys::invalid(a, w, "not an absolute path");
        }
    }),
    ("StandardInput", |s, a, w| {
        s.standard_input = keys::parsed(a, w, input).unwrap_or_else(|| s.standard_input.clone());
    }),
    ("StandardOutput", |s, a, w| {
        s.standard_output = keys::parsed(a, w, output).unwrap_or_else(|| s.standard_output.clone());
    }),
    ("StandardError", |s, a, w| {
        s.standard_error = keys::parsed(a, w, output).unwrap_or_else(|| s.standard_error.clone());
    }),
    ("TTYPath", |s, a, w| {
        if let Some(path) = keys::path(a, w) {
            s.tty_path = path;
        }
    }),
    ("TTYReset", |s, a, w| {
        s.tty_reset = keys::boolean(a, w).unwrap_or(s.tty_reset);
    }),
    ("TTYVHangup", |s, a, w| {
        s.tty_vhangup = keys::boolean(a, w).unwrap_or(s.tty_vhangup);
    }),
    ("Offers", |s, a, w| directory_names(&mut s.offers, a, w)),
    ("Uses", |s, a, w| directory_names(&mut s.uses, a, w)),
    ("StartLimitIntervalSec", |_, _, _| {}),
    ("StartLimitInterval", |_, _, _| {}),
];

/// `StandardInput=`.
fn input(text: &str) -> Result<Input, ValueError> {
    if let Some(path) = text.strip_prefix("file:") {
        return value::is_absolute_path(path)
            .then(|| Input::File(String::from(path)))
            .ok_or(ValueError::Invalid);
    }
    Ok(match text {
        "null" => Input::Null,
        "tty" => Input::Tty,
        "tty-force" => Input::TtyForce,
        "tty-fail" => Input::TtyFail,
        "socket" => Input::Socket,
        _ => return Err(ValueError::Invalid),
    })
}

/// `StandardOutput=` and `StandardError=`.
fn output(text: &str) -> Result<Output, ValueError> {
    let file = |path: &str, make: fn(String) -> Output| {
        value::is_absolute_path(path)
            .then(|| make(String::from(path)))
            .ok_or(ValueError::Invalid)
    };
    if let Some(path) = text.strip_prefix("file:") {
        return file(path, Output::File);
    }
    if let Some(path) = text.strip_prefix("append:") {
        return file(path, Output::Append);
    }
    if let Some(path) = text.strip_prefix("truncate:") {
        return file(path, Output::Truncate);
    }
    Ok(match text {
        "inherit" => Output::Inherit,
        "null" => Output::Null,
        "tty" => Output::Tty,
        "console" => Output::Console,
        "log" | "journal" | "kmsg" | "journal+console" | "kmsg+console" => Output::Log,
        "socket" => Output::Socket,
        _ => return Err(ValueError::Invalid),
    })
}

/// `Offers=` and `Uses=`: dotted names, `ferrix.clipboard`.
fn directory_names(list: &mut Vec<String>, assignment: &Assignment, warnings: &mut Warnings) {
    if assignment.value.is_empty() {
        list.clear();
        return;
    }
    for word in assignment
        .value
        .split([' ', '\t'])
        .filter(|w| !w.is_empty())
    {
        let valid = word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !valid {
            keys::invalid(
                assignment,
                warnings,
                format!("'{word}' is not a directory name"),
            );
        } else if !list.iter().any(|known| known == word) {
            list.push(String::from(word));
        }
    }
}

/// The service kind.
pub(super) struct ServiceKind;

/// The `[Service]` section, parsed and checked.
pub(super) fn parse(
    _: &UnitName,
    section: &Section,
    warnings: &mut Warnings,
) -> Result<Config, UnitError> {
    let mut service = Service::default();
    for assignment in &section.assignments {
        let key = assignment.key.as_str();
        if let Some(setter) = keys::find(&EXEC_KEYS, key)
            .or_else(|| keys::find(&RUN_KEYS, key))
            .or_else(|| keys::find(&CONTEXT_KEYS, key))
        {
            setter(&mut service, assignment, warnings);
        } else if let Some(setter) = keys::find(&limits::KEYS, key) {
            setter(&mut service.limits, assignment, warnings);
        } else if let Some(setter) = keys::find(&KILL_KEYS, key) {
            setter(&mut service.kill, assignment, warnings);
        } else if let Some(setter) = keys::find(&sandbox::KEYS, key) {
            setter(&mut service.sandbox, assignment, warnings);
        } else if sandbox::NOT_BUILT.contains(&key) {
            sandbox::not_built(assignment, warnings);
        } else {
            keys::unknown(&section.name, assignment, warnings);
        }
    }
    check(&service)?;
    Ok(Config::Service(Box::new(service)))
}

/// What systemd's `service_verify` refuses.
fn check(service: &Service) -> Result<(), UnitError> {
    let oneshot = service.service_type == ServiceType::Oneshot;
    if service.exec_start.is_empty() && service.exec_stop.is_empty() {
        return Err(UnitError::new(
            "Service has no ExecStart= and no ExecStop= setting. Refusing.",
        ));
    }
    if !oneshot && service.exec_start.is_empty() {
        return Err(UnitError::new(
            "Service has no ExecStart= setting, which is only allowed for Type=oneshot \
             services. Refusing.",
        ));
    }
    if !oneshot && service.exec_start.len() > 1 {
        return Err(UnitError::new(
            "Service has more than one ExecStart= setting, which is only allowed for \
             Type=oneshot services. Refusing.",
        ));
    }
    if oneshot && matches!(service.restart, Restart::Always | Restart::OnSuccess) {
        return Err(UnitError::new(
            "Service has Restart= set to either always or on-success, which isn't allowed \
             for Type=oneshot services. Refusing.",
        ));
    }
    Ok(())
}
