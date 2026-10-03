//! What goes into [`Manager::step`](crate::Manager::step) and what comes
//! out (§3).
//!
//! Every name here is one the backend maps: a [`UnitId`] to a cgroup and
//! its processes, a [`GroupPath`] to a cgroupfs directory, a [`Token`] to a
//! channel end, a [`ClientId`] to a control connection. The core holds no
//! handle, no descriptor and no directory, which is what lets the same
//! manager sit in a Linux-ABI init now and in a native root task later.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::Mode;
use crate::exec::Command;
use crate::kind::{Input, Listen, Output, Sandbox, ServiceType, WorkingDirectory};
use crate::limits::Limits;
use crate::restart::Ended;
use crate::value::Signal;

/// A unit, as the manager numbers it. A number is never reused while the
/// manager runs, and a unit keeps its number across a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitId(pub u32);

/// A process id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(pub u32);

/// A connection of the control client, `svc`, numbered by the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u64);

/// A handle in transit, numbered by the backend: the channel end an OPEN
/// carried (§6), which the core routes without holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Token(pub u64);

/// An error number, as a backend's call failed with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(pub i32);

/// A name in the directory (§6): `ferrix.clipboard`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(pub String);

/// A cgroup, as a path below the cgroup2 mount: `system.slice/sshd.service`,
/// and the empty path for the root. Opaque to the core; the backend joins
/// it to wherever it mounted cgroup2.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupPath(pub String);

impl GroupPath {
    /// The root of the tree.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// The path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// How deep it is: 0 for the root, 1 for `system.slice`.
    pub fn depth(&self) -> usize {
        if self.0.is_empty() {
            0
        } else {
            self.0.split('/').count()
        }
    }

    /// The path of a child called `name`.
    pub fn child(&self, name: &str) -> Self {
        if self.0.is_empty() {
            Self(String::from(name))
        } else {
            let mut path = self.0.clone();
            path.push('/');
            path.push_str(name);
            Self(path)
        }
    }
}

impl fmt::Display for GroupPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.0)
    }
}

pub use ferrix_restart::Exit;

/// Everything that happened, as the backends saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The backend has mounted cgroup2, moved itself into `init.scope` and
    /// run the generators: start the system.
    Boot,
    /// A process a [`Action::Spawn`] asked for exists, in the unit's cgroup.
    Spawned {
        /// The unit.
        unit: UnitId,
        /// Its pid.
        pid: Pid,
    },
    /// That process's `execve` succeeded: the pipe closed on exec (§5.2).
    Execed {
        /// The unit.
        unit: UnitId,
        /// The process.
        pid: Pid,
    },
    /// A spawn failed before the program ran.
    SpawnFailed {
        /// The unit.
        unit: UnitId,
        /// Why.
        error: Errno,
    },
    /// A process was reaped. A pid the manager does not know is an orphan
    /// that still belongs to some cgroup, and is ignored.
    Exited {
        /// The process.
        pid: Pid,
        /// How it ended.
        how: Exit,
    },
    /// A cgroup's `cgroup.events` says `populated 0`.
    Emptied {
        /// The unit whose cgroup it is.
        unit: UnitId,
    },
    /// A cgroup's `memory.events` says its `oom_kill` count grew.
    OomKilled {
        /// The unit.
        unit: UnitId,
    },
    /// A `notify` service wrote `READY=1`, or a `native` one sent READY.
    Ready {
        /// The unit.
        unit: UnitId,
        /// Its `STATUS=` line, if it gave one.
        status: Option<String>,
    },
    /// A `notify` service wrote `STATUS=` without `READY=1`: what `svc
    /// status` shows, and not readiness.
    Status {
        /// The unit.
        unit: UnitId,
        /// The text.
        status: String,
    },
    /// A `forking` service's main process, found in its `PIDFile=` or as the
    /// one process left in its cgroup.
    MainPid {
        /// The unit.
        unit: UnitId,
        /// The process.
        pid: Pid,
    },
    /// [`Manager::deadline`](crate::Manager::deadline) has come.
    Timer,
    /// A control client asked for something.
    Request {
        /// Who asked, to reply to.
        client: ClientId,
        /// What.
        request: Request,
    },
    /// A service sent OPEN on its bootstrap channel (§6).
    Open {
        /// The service.
        from: UnitId,
        /// The name it asked for.
        name: Name,
        /// The client's end of the new channel.
        end: Token,
    },
    /// A [`Action::Mount`] finished.
    Mounted {
        /// The unit.
        unit: UnitId,
        /// How.
        result: Result<(), Errno>,
    },
    /// A [`Action::Unmount`] finished.
    Unmounted {
        /// The unit.
        unit: UnitId,
        /// How.
        result: Result<(), Errno>,
    },
    /// A [`Action::Listen`] finished.
    Listening {
        /// The socket unit.
        unit: UnitId,
        /// How.
        result: Result<(), Errno>,
    },
    /// A connection waits on a socket with `Accept=no`, which the socket's
    /// service is to accept: start it. Said once per [`Action::Watch`].
    Incoming {
        /// The socket unit.
        unit: UnitId,
    },
    /// The backend accepted a connection on a socket with `Accept=yes`:
    /// start an instance of its service with it. Said once per
    /// [`Action::Watch`].
    Accepted {
        /// The socket unit.
        unit: UnitId,
        /// The connection, which the backend holds.
        connection: Token,
    },
    /// Whether a device unit's node exists: said once as soon as a
    /// [`Action::WatchDevice`] is armed, then whenever that changes, until
    /// [`Action::UnwatchDevice`].
    Device {
        /// The device unit.
        unit: UnitId,
        /// Whether the node is there.
        present: bool,
    },
}

/// What a control client can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `svc start`, meeting a queued operation as `Mode` says:
    /// `--job-mode=`, `replace` by default.
    Start(String, Mode),
    /// `svc stop`.
    Stop(String, Mode),
    /// `svc restart`.
    Restart(String, Mode),
    /// `svc reload`: run a running service's `ExecReload=`, answered when
    /// it has run.
    Reload(String),
    /// `svc isolate`: start a target and stop everything it does not need.
    Isolate(String),
    /// `svc reset-failed`, for one unit or every one.
    ResetFailed(Option<String>),
    /// `svc status`, for one unit or every loaded one.
    Status(Option<String>),
    /// `svc poweroff`, and what the backend asks for on `SIGTERM` or
    /// `SIGINT` to pid 1.
    Poweroff,
    /// `svc reboot`.
    Reboot,
    /// `svc scope`: group processes init did not start (§5.6).
    Scope {
        /// The scope's name, `session-1.scope`.
        unit: String,
        /// The slice to put it under; `system.slice` if none.
        slice: Option<String>,
        /// The processes to move into it.
        pids: Vec<Pid>,
    },
}

impl Request {
    /// `svc start unit`, replacing what it conflicts with.
    pub fn start(unit: &str) -> Self {
        Request::Start(String::from(unit), Mode::Replace)
    }

    /// `svc stop unit`.
    pub fn stop(unit: &str) -> Self {
        Request::Stop(String::from(unit), Mode::Replace)
    }

    /// `svc restart unit`.
    pub fn restart(unit: &str) -> Self {
        Request::Restart(String::from(unit), Mode::Replace)
    }
}

/// How an operation ended: systemd's job results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpResult {
    /// It did what it was for.
    Done,
    /// The unit failed.
    Failed,
    /// A condition skipped the unit, which is not a failure.
    Skipped,
    /// An assertion failed.
    Assert,
    /// A unit it required failed.
    Dependency,
    /// A later operation replaced it.
    Canceled,
    /// It took longer than its timeout.
    Timeout,
}

impl OpResult {
    /// systemd's name for it.
    pub fn name(self) -> &'static str {
        match self {
            OpResult::Done => "done",
            OpResult::Failed => "failed",
            OpResult::Skipped => "skipped",
            OpResult::Assert => "assert",
            OpResult::Dependency => "dependency",
            OpResult::Canceled => "canceled",
            OpResult::Timeout => "timeout",
        }
    }
}

/// Whether a unit is running, as `svc status` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActiveState {
    /// Not running.
    Inactive,
    /// Starting.
    Activating,
    /// Running, or done and remaining, or always there.
    Active,
    /// Stopping.
    Deactivating,
    /// Stopped by a failure, until reset or started again.
    Failed,
}

impl ActiveState {
    /// systemd's name for it.
    pub fn name(self) -> &'static str {
        match self {
            ActiveState::Inactive => "inactive",
            ActiveState::Activating => "activating",
            ActiveState::Active => "active",
            ActiveState::Deactivating => "deactivating",
            ActiveState::Failed => "failed",
        }
    }
}

/// One unit, as `svc status` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The unit.
    pub unit: UnitId,
    /// Its name.
    pub name: String,
    /// `Description=`.
    pub description: Option<String>,
    /// systemd's load state: `loaded`, `not-found`, `masked`, `error` or
    /// `bad-setting`.
    pub load: &'static str,
    /// Its active state.
    pub active: ActiveState,
    /// Its kind's own state: `running`, `exited`, `stop-sigterm`, ….
    pub sub: &'static str,
    /// Its main process.
    pub main: Option<Pid>,
    /// How it last ended, if it did.
    pub result: Option<Ended>,
    /// A `notify` service's last `STATUS=`.
    pub status: Option<String>,
}

/// An answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The operation the request started has ended so.
    Done(OpResult),
    /// The request was refused, and why.
    Refused(String),
    /// The units asked about.
    Status(Vec<Status>),
}

/// Who a signal is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Whom {
    /// One process.
    Process(Pid),
    /// Every process in the unit's cgroup.
    Group,
}

/// What a process is to its unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The main process: `ExecStart=`.
    Main,
    /// A control process: `ExecStartPre=`, `ExecStartPost=`, `ExecStop=` or
    /// `ExecStopPost=`.
    Control,
}

/// Everything the Spawn backend needs to start one command (§5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The command.
    pub command: Command,
    /// Main or control.
    pub role: Role,
    /// The cgroup it starts in, with `CLONE_INTO_CGROUP`.
    pub group: GroupPath,
    /// `Type=`: `native` is started with `process_create` in the cgroup's
    /// job, everything else with `clone3` and `execve`.
    pub service_type: ServiceType,
    /// `User=`.
    pub user: Option<String>,
    /// `Group=`.
    pub group_name: Option<String>,
    /// `SupplementaryGroups=`.
    pub supplementary_groups: Vec<String>,
    /// `WorkingDirectory=`.
    pub working_directory: Option<WorkingDirectory>,
    /// The environment: `Environment=` in order, then what the manager
    /// adds (`MAINPID`), a later name winning.
    pub environment: Vec<(String, String)>,
    /// `EnvironmentFile=`.
    pub environment_files: Vec<(String, bool)>,
    /// `StandardInput=`.
    pub stdin: Input,
    /// `StandardOutput=`.
    pub stdout: Output,
    /// `StandardError=`.
    pub stderr: Output,
    /// The terminal, when a stream is `tty`: made the controlling terminal
    /// of a new session.
    pub tty: Option<String>,
    /// `NotifyFd=`.
    pub notify_fd: Option<u32>,
    /// The listening sockets of the `.socket` units that activate this
    /// service, passed on as descriptors 3 and up with `LISTEN_FDS`.
    pub sockets: Vec<UnitId>,
    /// The connection an `Accept=yes` socket took, for `socket` streams.
    pub connection: Option<Token>,
    /// Whether the unit uses or offers directory names, and so gets a
    /// bootstrap channel (§6).
    pub bootstrap: bool,
    /// The sandboxing keys (§4.5), empty for a command with the `+` prefix,
    /// which runs without them as under systemd.
    pub sandbox: Sandbox,
}

/// Everything the backend needs to listen for one `.socket` unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenSpec {
    /// What to listen on, in order: `LISTEN_FDS` passes them in this order.
    pub listen: Vec<Listen>,
    /// `Accept=`.
    pub accept: bool,
    /// `SocketMode=`, for a socket in the file system.
    pub mode: u32,
    /// `Backlog=`.
    pub backlog: u32,
}

/// Everything the Filesystem backend needs to mount one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    /// `What=`.
    pub what: String,
    /// `Where=`.
    pub r#where: String,
    /// `Type=`.
    pub fs_type: Option<String>,
    /// `Options=`.
    pub options: String,
}

/// How the machine ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PowerAction {
    /// Power off.
    Poweroff,
    /// Reboot.
    Reboot,
}

/// Everything to do about what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Make the unit's cgroup at `path` and write `limits` to it. For a
    /// slice, also enable in its `cgroup.subtree_control` the controllers
    /// the kernel has. A controller the kernel lacks is a warning (§5.5).
    MakeGroup {
        /// The unit.
        unit: UnitId,
        /// Where.
        path: GroupPath,
        /// Its limits.
        limits: Limits,
    },
    /// Write new limits to a unit's cgroup.
    SetLimits {
        /// The unit.
        unit: UnitId,
        /// The limits.
        limits: Limits,
    },
    /// Remove a unit's cgroup, which is empty.
    RemoveGroup {
        /// The unit.
        unit: UnitId,
    },
    /// Start a process in the unit's cgroup; answered by
    /// [`Event::Spawned`] or [`Event::SpawnFailed`].
    Spawn {
        /// The unit.
        unit: UnitId,
        /// What to start.
        spec: Box<SpawnSpec>,
    },
    /// Move processes init did not start into a scope's cgroup.
    Move {
        /// The scope.
        unit: UnitId,
        /// The processes.
        pids: Vec<Pid>,
    },
    /// Send a signal.
    Signal {
        /// The unit.
        unit: UnitId,
        /// The signal.
        signal: Signal,
        /// To whom.
        whom: Whom,
    },
    /// Write `cgroup.kill`: `SIGKILL` to every process in the unit's
    /// cgroup and below it. [`Event::Emptied`] follows.
    KillGroup {
        /// The unit.
        unit: UnitId,
    },
    /// Mount; answered by [`Event::Mounted`]. A mount the kernel has made
    /// already is a success.
    Mount {
        /// The unit.
        unit: UnitId,
        /// What.
        spec: MountSpec,
    },
    /// Unmount; answered by [`Event::Unmounted`].
    Unmount {
        /// The unit.
        unit: UnitId,
    },
    /// Make a `.socket` unit's sockets; answered by [`Event::Listening`].
    Listen {
        /// The socket unit.
        unit: UnitId,
        /// What to listen on.
        spec: ListenSpec,
    },
    /// Close a `.socket` unit's sockets, and remove one in the file system.
    Unlisten {
        /// The socket unit.
        unit: UnitId,
    },
    /// Watch a socket unit's sockets for a connection, and say
    /// [`Event::Incoming`] or [`Event::Accepted`] once for it; stop watching
    /// until asked again.
    Watch {
        /// The socket unit.
        unit: UnitId,
    },
    /// Watch for a device unit's node to appear or go, and answer with
    /// [`Event::Device`]: at once with whether it is there now, then at each
    /// change. A directory on the way to it may not exist yet either.
    WatchDevice {
        /// The device unit.
        unit: UnitId,
        /// The node, absolute and below `/dev`.
        path: String,
    },
    /// Stop watching a device unit's node.
    UnwatchDevice {
        /// The device unit.
        unit: UnitId,
    },
    /// Close a connection nothing took.
    Close {
        /// The connection.
        connection: Token,
    },
    /// Forward the client end of a channel to the unit that offers a name
    /// (§6): CONNECT.
    Route {
        /// The provider.
        to: UnitId,
        /// The name.
        name: Name,
        /// The end.
        end: Token,
    },
    /// Refuse an OPEN: REFUSED, and the end is closed.
    Refuse {
        /// The unit that asked.
        to: UnitId,
        /// The name.
        name: Name,
        /// The end.
        end: Token,
    },
    /// Answer a control client.
    Reply {
        /// The client.
        client: ClientId,
        /// The answer.
        reply: Reply,
    },
    /// A line for the console and the boot log, in the `  init     …`
    /// format `xtask` reads.
    Log {
        /// The unit it is about, if one.
        unit: Option<UnitId>,
        /// The line.
        line: String,
    },
    /// The end (§8.2, steps 2 and 3): `sync`, remount `/` and `/data`
    /// read-only, unmount the rest in reverse order, and `reboot(2)`.
    Power(PowerAction),
}

/// What one step asks for, in order.
pub type Actions = Vec<Action>;
