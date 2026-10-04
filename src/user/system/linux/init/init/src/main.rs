//! `/sbin/init`: pid 1, around `src/lib/init/svc`'s manager (`docs/INIT.md`).
//!
//! The manager is a pure state machine: [`Manager::step`] takes what
//! happened and returns what to do. This program is everything around it
//! (§9): it boots the machine far enough for the manager to run (§8.1), then
//! waits in one `epoll_wait` for a signal, a child's exec report or a
//! cgroup's `cgroup.events`, turns what it finds into events, and carries
//! out the actions each step returns, until one of them is
//! [`Action::Power`].
//!
//! # Boot (§8.1)
//!
//! 1. Ignore every signal but the three it acts on, which it blocks and
//!    reads through a signalfd: `SIGCHLD` to reap, `SIGTERM` and `SIGINT` to
//!    power off (§8.2). Become a subreaper (§5.7).
//! 2. Mount a tmpfs on `/run`, then cgroup2 on `/sys/fs/cgroup`, move into
//!    `init.scope`, and enable the controllers the kernel has (§5.1). The
//!    kernel mounts `/proc`, `/dev`, `/sys` and `/tmp` itself.
//! 3. Run each generator in `/lib/ferrix/generators` with
//!    `/run/ferrix/units` as its argument, each for up to five seconds.
//! 4. Read the three unit directories and hand the manager
//!    [`Event::Boot`].
//!
//! If cgroup2 cannot be mounted there is nothing to run a service in, and
//! init says why and becomes a shell on the console, so the machine can be
//! looked at.

mod admin;
mod audit;
mod cgroup;
mod control;
mod directory;
mod logs;
mod probe;
mod readiness;
mod sandbox;
mod sockets;
mod spawn;
mod sys;
mod units;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::fs;
use std::io::{self, Write as _};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use ferrix_svc::event::Role;
use ferrix_svc::event::{
    Action, ClientId, Event, Exit, MountSpec, Pid, PowerAction, Reply, Request, Status, UnitId,
    Whom,
};
use ferrix_svc::kind::{Config, ServiceType};
use ferrix_svc::source::Source;
use ferrix_svc::{Instant, Manager, Options};
use ferrix_svc_proto::control::{Answer, Call, UnitStatus};

use crate::cgroup::Groups;
use crate::control::{Control, Heard};
use crate::directory::Directory;
use crate::logs::Logs;
use crate::probe::Machine;
use crate::readiness::Readiness;
use crate::sockets::Sockets;
use crate::spawn::Report;

/// The unit directory generators write to (§8.1).
const RUNTIME_UNITS: &str = "/run/ferrix/units";

/// Where generators are.
const GENERATORS: &str = "/lib/ferrix/generators";

/// How long one generator may take, in polls of 10 ms.
const GENERATOR_PATIENCE: u32 = 500;

/// The client a signal to pid 1 asks for as: a power-off with nobody to
/// answer.
const SIGNALLED: ClientId = ClientId(0);

/// The signals init reads, blocked everywhere else.
const HANDLED: [libc::c_int; 3] = [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT];

/// The signals init ignores. `SIGCHLD` stays at its default, since ignoring
/// it would reap children before init could see how they ended.
const IGNORED: [libc::c_int; 9] = [
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGPIPE,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGALRM,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
];

/// What an epoll token stands for: its kind in the top 32 bits, a number
/// below them.
mod token {
    /// The signalfd.
    pub(crate) const SIGNALS: u64 = 1 << 32;
    /// A cgroup's `cgroup.events`; the unit's number below.
    pub(crate) const GROUP: u64 = 2 << 32;
    /// A child's exec report pipe; its pid below.
    pub(crate) const REPORT: u64 = 3 << 32;
    /// A log pipe; its number below.
    pub(crate) const LOG: u64 = 4 << 32;
    /// The control socket's listener.
    pub(crate) const LISTEN: u64 = 5 << 32;
    /// A control connection; its number below.
    pub(crate) const CLIENT: u64 = 6 << 32;
    /// A `Type=notify` service's readiness pipe; its number below.
    pub(crate) const NOTIFY: u64 = 7 << 32;
    /// A `.socket` unit's sockets; the unit's number below.
    pub(crate) const SOCKET: u64 = 8 << 32;
    /// The port, as `port_fd`'s descriptor (§6, §9).
    pub(crate) const PORT: u64 = 9 << 32;
    /// The kind of `token`.
    pub(crate) fn kind(token: u64) -> u64 {
        token & !0xffff_ffff
    }
    /// The number in `token`.
    pub(crate) fn number(token: u64) -> u32 {
        u32::try_from(token & 0xffff_ffff).unwrap_or(0)
    }
}

/// One line on the console, in the `  init     …` form the kernel's own
/// lines about init take and `xtask` reads.
fn say(line: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "  init     {line}");
    let _ = out.flush();
}

/// The whole of init's state.
#[derive(Debug)]
struct Init {
    manager: Manager,
    epoll: OwnedFd,
    signals: OwnedFd,
    groups: Groups,
    /// The report pipe of each child that has not yet exec'd or failed.
    reports: BTreeMap<u32, (UnitId, OwnedFd)>,
    /// Where each mounted unit is, for its unmount.
    mounts: BTreeMap<UnitId, String>,
    /// Events waiting to be stepped, in order.
    queue: VecDeque<Event>,
    /// The `TERM` a service on a terminal gets.
    terminal: String,
    /// The unit directories as last read, for `enable` and `set-property`.
    source: Source,
    /// The control socket, unless it could not be made.
    control: Option<Control>,
    /// What each control connection asked, to shape the manager's reply.
    pending: BTreeMap<u64, Pending>,
    /// The log pipes and each unit's last lines.
    logs: Logs,
    /// The readiness pipes of `Type=notify` services.
    readiness: Readiness,
    /// The first process of each `Type=forking` service still starting.
    forking: BTreeMap<u32, UnitId>,
    /// The `.socket` units' sockets.
    sockets: Sockets,
    /// Bootstrap channels, native services and the directory (§6), unless
    /// the kernel has no native calls to make them with.
    directory: Option<Directory>,
    /// Under `ferrix.devmgr=init` (§7.3, L12): whether the boot waits for
    /// the kernel to say where `/` is, having started only `devmgr.service`.
    awaiting_root: bool,
    /// The audit record's reader, once `/` is settled
    /// (`docs/certification/AUDIT.md` §4).
    audit: Option<audit::Reader>,
}

/// The unit that runs `devmgr` when pid 1 starts it (§7.3), and the program
/// its `ExecStart=` names, which the kernel loads itself.
const DEVMGR_UNIT: &str = "devmgr.service";
const DEVMGR_PROGRAM: &str = "/sbin/devmgr";

/// What a control connection is waiting for from the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// Units: every one, or with `Some`, a list, the failed ones alone when
    /// it holds `true`.
    Units(Option<bool>),
    /// An operation's end.
    Done,
}

fn main() {
    let mut init = match Init::boot() {
        Ok(init) => init,
        Err(why) => emergency(&why),
    };
    init.run();
}

/// Say why init cannot go on, and become a shell on the console.
fn emergency(why: &str) -> ! {
    say(&format!("{why}; starting a shell on the console"));
    let _ = sys::unblock_all();
    let error = sys::execve(
        c"/bin/sh",
        &[c"sh".as_ptr(), c"-i".as_ptr(), std::ptr::null()],
        &[c"PATH=/bin:/sbin".as_ptr(), std::ptr::null()],
    );
    say(&format!("/bin/sh could not be started: {error}"));
    sys::exit_now(1)
}

/// Now, on the manager's clock.
fn now() -> Instant {
    Instant::from_nanos(sys::monotonic())
}

impl Init {
    /// §8.1's steps 1 to 4, up to the first event.
    fn boot() -> Result<Init, String> {
        for signal in IGNORED {
            sys::disposition(signal, libc::SIG_IGN);
        }
        let set = sys::block(&HANDLED).map_err(|e| format!("blocking signals failed: {e}"))?;
        let signals = sys::signalfd(&set).map_err(|e| format!("signalfd failed: {e}"))?;
        if let Err(error) = sys::subreaper() {
            say(&format!("PR_SET_CHILD_SUBREAPER failed: {error}"));
        }

        mount_run();
        let mut log = |line: String| say(&line);
        let groups =
            Groups::mount(&mut log).map_err(|e| format!("cgroup2 at {}: {e}", cgroup::MOUNT))?;
        run_generators();

        let command_line = fs::read_to_string("/proc/cmdline").unwrap_or_default();
        let options = Options {
            target: command_line
                .split_whitespace()
                .find_map(|word| word.strip_prefix("ferrix.target="))
                .map(str::to_owned),
        };
        let source = units::read(&mut log);
        let machine = Machine { command_line };
        let manager = Manager::new(source.clone(), Box::new(machine), options);

        let epoll = sys::epoll().map_err(|e| format!("epoll_create1 failed: {e}"))?;
        sys::watch(
            epoll.as_fd(),
            signals.as_raw_fd(),
            libc::EPOLLIN as u32,
            token::SIGNALS,
        )
        .map_err(|e| format!("watching the signalfd failed: {e}"))?;
        let terminal = std::env::var("TERM").unwrap_or_else(|_| "vt220".to_owned());
        let control = match Control::listen() {
            Ok(control) => {
                if let Err(error) = sys::watch(
                    epoll.as_fd(),
                    control.fd(),
                    libc::EPOLLIN as u32,
                    token::LISTEN,
                ) {
                    say(&format!("watching the control socket failed: {error}"));
                }
                Some(control)
            }
            Err(error) => {
                say(&format!(
                    "the control socket {} could not be made: {error}",
                    ferrix_svc_proto::control::SOCKET
                ));
                None
            }
        };
        let directory = match Directory::open() {
            Ok((directory, fd, said)) => {
                say(&said);
                if let Err(error) = sys::watch(epoll.as_fd(), fd, libc::EPOLLIN as u32, token::PORT)
                {
                    say(&format!("watching the port failed: {error}"));
                }
                Some(directory)
            }
            Err(error) => {
                say(&format!(
                    "no native calls ({error:?}): no bootstrap channels, native services or directory"
                ));
                None
            }
        };
        // Under `ferrix.devmgr=init`: devmgr.service alone, until the kernel
        // has switched `/` and said so; then the units again, from the
        // volume, and the boot (§7.3, L12).
        let awaiting_root = directory.as_ref().is_some_and(Directory::has_starter);
        let first = if awaiting_root {
            say("starting devmgr.service, and the rest once / is known");
            Event::Request {
                client: SIGNALLED,
                request: Request::start(DEVMGR_UNIT),
            }
        } else {
            Event::Boot
        };
        let mut init = Init {
            awaiting_root,
            audit: None,
            directory,
            manager,
            epoll,
            signals,
            groups,
            reports: BTreeMap::new(),
            mounts: BTreeMap::new(),
            queue: VecDeque::from([first]),
            terminal,
            source,
            control,
            pending: BTreeMap::new(),
            logs: Logs::default(),
            readiness: Readiness::default(),
            forking: BTreeMap::new(),
            sockets: Sockets::default(),
        };
        // `/` is settled already when the kernel started devmgr itself: the
        // audit record's reader starts now, and otherwise once it is.
        if !awaiting_root {
            make_homes();
            init.start_audit();
        }
        Ok(init)
    }

    /// Step, act and wait, for as long as the machine runs.
    fn run(&mut self) -> ! {
        loop {
            while let Some(event) = self.queue.pop_front() {
                let actions = self.manager.step(event, now());
                for action in actions {
                    self.perform(action);
                }
            }
            let manager = self
                .manager
                .deadline()
                .map(|deadline| deadline.saturating_since(now()));
            let groups = self
                .groups
                .next_try()
                .map(|at| at.saturating_duration_since(std::time::Instant::now()));
            let audit = self.audit.as_ref().map(|reader| {
                reader
                    .next_read()
                    .saturating_duration_since(std::time::Instant::now())
            });
            let timeout = match manager.into_iter().chain(groups).chain(audit).min() {
                None => -1,
                Some(wait) => {
                    // Rounded up, so a timer is never looked at early.
                    let millis = wait.as_nanos().div_ceil(1_000_000);
                    i32::try_from(millis).unwrap_or(i32::MAX)
                }
            };
            let ready = match sys::wait(self.epoll.as_fd(), timeout) {
                Ok(ready) => ready,
                Err(error) => {
                    say(&format!("epoll_wait failed: {error}"));
                    std::thread::sleep(Duration::from_millis(100));
                    Vec::new()
                }
            };
            self.gather(&ready);
        }
    }

    /// Turn what woke init into events, in the order the manager wants
    /// them: exec reports before the exits of the same children, exits
    /// before the cgroups they leave empty, and the timer last.
    fn gather(&mut self, ready: &[(u32, u64)]) {
        for &(events, token) in ready {
            match token::kind(token) {
                token::REPORT => self.report(token::number(token)),
                // Before the reaping below, so what a process said comes
                // out before its end is reported.
                token::LOG => self.read_log(token::number(token)),
                token::LISTEN => self.accept(),
                token::CLIENT => self.client(u64::from(token::number(token)), events),
                token::NOTIFY => self.read_notify(token::number(token)),
                token::PORT => self.drain_port(),
                token::SOCKET => self.connected(UnitId(token::number(token))),
                _ => {}
            }
        }
        loop {
            match sys::next_signal(self.signals.as_fd()) {
                Ok(Some(signal))
                    if signal == libc::SIGTERM as u32 || signal == libc::SIGINT as u32 =>
                {
                    self.queue.push_back(Event::Request {
                        client: SIGNALLED,
                        request: Request::Poweroff,
                    });
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    say(&format!("reading the signalfd failed: {error}"));
                    break;
                }
            }
        }
        // An OOM kill before the exit it caused, so the service's result is
        // `oom-kill` rather than the signal (the manager keeps the first).
        for unit in self.groups.oom_killed() {
            self.queue.push_back(Event::OomKilled { unit });
        }
        self.reap();
        for unit in self.groups.emptied() {
            self.queue.push_back(Event::Emptied { unit });
        }
        if self.manager.deadline().is_some_and(|at| at <= now()) {
            self.queue.push_back(Event::Timer);
        }
        for (unit, error) in self.groups.retry() {
            let name = self.display(unit);
            say(&format!("{name}: removing its cgroup: {error}"));
        }
        if self
            .audit
            .as_ref()
            .is_some_and(|reader| reader.next_read() <= std::time::Instant::now())
        {
            self.read_audit();
        }
    }

    /// Start reading the audit record into a file on `/`, now that it is
    /// settled, if the kernel gave pid 1 the handle.
    fn start_audit(&mut self) {
        let Some(handle) = self.directory.as_mut().and_then(Directory::take_audit) else {
            return;
        };
        match audit::Reader::open(handle) {
            Ok(reader) => {
                say(&reader.said());
                self.audit = Some(reader);
            }
            Err(error) => say(&format!("audit: the record cannot be kept: {error}")),
        }
    }

    /// Read the audit record's rings into the file. A reader that fails
    /// says so and stops, rather than saying it every second.
    fn read_audit(&mut self) {
        let Some(reader) = self.audit.as_mut() else {
            return;
        };
        if let Err(error) = reader.read() {
            say(&format!(
                "audit: reading the record failed, and stopped: {error}"
            ));
            self.audit = None;
        }
    }

    /// Reap every child that has ended; a pid the manager does not know is
    /// an orphan, which it ignores (§5.7).
    fn reap(&mut self) {
        loop {
            match sys::reap() {
                Ok(Some((pid, how))) => {
                    if self.reports.contains_key(&pid) {
                        self.report(pid);
                    }
                    self.queue.push_back(Event::Exited { pid: Pid(pid), how });
                    if let Some(unit) = self.forking.remove(&pid)
                        && how == Exit::Code(0)
                    {
                        self.find_forked_main(unit);
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    say(&format!("waitpid failed: {error}"));
                    return;
                }
            }
        }
    }

    /// Read a child's exec report, which is ready: its pipe has closed.
    fn report(&mut self, pid: u32) {
        let Some((unit, pipe)) = self.reports.remove(&pid) else {
            return;
        };
        sys::unwatch(self.epoll.as_fd(), pipe.as_raw_fd());
        match spawn::read_report(&pipe) {
            Report::Execed => self.queue.push_back(Event::Execed {
                unit,
                pid: Pid(pid),
            }),
            Report::Failed(step, error) => {
                let name = self.display(unit);
                say(&format!("{name}: {} failed: {error}", step.name()));
            }
        }
    }

    /// A unit's name, for the log.
    fn display(&self, unit: UnitId) -> String {
        self.manager.name(unit).map_or_else(
            || format!("unit {}", unit.0),
            |name| name.as_str().to_owned(),
        )
    }

    /// Carry out one action.
    fn perform(&mut self, action: Action) {
        match action {
            Action::Log { line, .. } => say(&line),
            Action::MakeGroup { unit, path, limits } => {
                let mut log = |line: String| say(&line);
                match self.groups.make(unit, &path, &limits, &mut log) {
                    Ok((events, memory)) => {
                        self.watch(events, token::GROUP | u64::from(unit.0));
                        // A kernel whose memory.events does not poll
                        // refuses the watch (EPERM); it is read after every
                        // wake all the same.
                        if let Some(memory) = memory {
                            let token = token::GROUP | u64::from(unit.0);
                            let _ = sys::watch(
                                self.epoll.as_fd(),
                                memory,
                                libc::EPOLLPRI as u32,
                                token,
                            );
                        }
                        self.delegate(unit);
                    }
                    Err(error) => say(&format!("making the cgroup {path}: {error}")),
                }
            }
            Action::SetLimits { unit, limits } => {
                let mut log = |line: String| say(&line);
                self.groups.set_limits(unit, &limits, &mut log);
            }
            Action::RemoveGroup { unit } => {
                let offered = self.offers(unit);
                if let Some(directory) = self.directory.as_mut() {
                    directory.forget(unit, &offered);
                }
                if let Some(fd) = self.groups.events_fd(unit) {
                    sys::unwatch(self.epoll.as_fd(), fd);
                }
                if let Some((_, Err(error))) = self.groups.remove(unit) {
                    let name = self.display(unit);
                    say(&format!("{name}: removing its cgroup: {error}"));
                }
            }
            Action::Spawn { unit, spec } => self.spawn(unit, &spec),
            Action::Move { unit, pids } => {
                let pids: Vec<u32> = pids.iter().map(|pid| pid.0).collect();
                for (pid, error) in self.groups.move_in(unit, &pids) {
                    let name = self.display(unit);
                    say(&format!("{name}: moving {pid} in: {error}"));
                }
            }
            Action::Signal { unit, signal, whom } => {
                let signal = libc::c_int::from(signal.0);
                let pids = match whom {
                    Whom::Process(pid) => vec![pid.0],
                    Whom::Group => self.groups.procs_beneath(unit),
                };
                for pid in pids {
                    let _ = sys::kill(pid, signal);
                }
            }
            Action::KillGroup { unit } => {
                if let Err(error) = self.groups.kill(unit) {
                    let name = self.display(unit);
                    say(&format!("{name}: writing cgroup.kill: {error}"));
                }
            }
            Action::Mount { unit, spec } => {
                let result = mount(&spec);
                if result.is_ok() {
                    let _ = self.mounts.insert(unit, spec.r#where.clone());
                }
                self.queue.push_back(Event::Mounted { unit, result });
            }
            Action::Unmount { unit } => {
                let result = match self.mounts.remove(&unit) {
                    Some(target) => unmount(&target),
                    None => Ok(()),
                };
                self.queue.push_back(Event::Unmounted { unit, result });
            }
            Action::Listen { unit, spec } => {
                let result = self.sockets.listen(unit, &spec).map_err(|error| {
                    let name = self.display(unit);
                    say(&format!("{name}: {error}"));
                    ferrix_svc::event::Errno(error.raw_os_error().unwrap_or(libc::EINVAL))
                });
                self.queue.push_back(Event::Listening { unit, result });
            }
            Action::Unlisten { unit } => {
                self.unwatch_sockets(unit);
                self.sockets.unlisten(unit);
            }
            Action::Watch { unit } => {
                for fd in self.sockets.fds(unit) {
                    let token = token::SOCKET | u64::from(unit.0);
                    if let Err(error) =
                        sys::watch(self.epoll.as_fd(), fd, libc::EPOLLIN as u32, token)
                    {
                        say(&format!("watching a socket failed: {error}"));
                    }
                }
            }
            Action::Close { connection } => self.sockets.close(connection),
            Action::Route { to, name, end } => {
                let manager = &self.manager;
                let client = |unit: UnitId| {
                    manager
                        .name(unit)
                        .map_or_else(|| format!("unit {}", unit.0), |n| n.as_str().to_owned())
                };
                let routed = match self.directory.as_mut() {
                    Some(directory) => directory.route(to, &name.0, end, client),
                    None => Err("no directory".to_owned()),
                };
                if let Err(why) = routed {
                    say(&format!("routing {}: {why}", name.0));
                }
            }
            Action::Refuse { to, name, end } => {
                if let Some(directory) = self.directory.as_mut() {
                    directory.refuse(
                        to,
                        &name.0,
                        end,
                        "the unit does not name it in Uses=, or no unit offers it",
                    );
                }
            }
            Action::Reply { client, reply } => self.reply(client, reply),
            Action::Power(action) => power(action, self.audit.as_mut()),
        }
    }

    /// Watch `fd` for `EPOLLPRI`, as `cgroup.events` reports a change.
    fn watch(&self, fd: RawFd, token: u64) {
        if let Err(error) = sys::watch(self.epoll.as_fd(), fd, libc::EPOLLPRI as u32, token) {
            say(&format!("watching a cgroup failed: {error}"));
        }
    }

    /// Start a process for `unit`.
    fn spawn(&mut self, unit: UnitId, spec: &ferrix_svc::event::SpawnSpec) {
        let failed = |init: &mut Init, errno: i32, why: &str| {
            let name = init.display(unit);
            say(&format!("{name}: {why}"));
            init.queue.push_back(Event::SpawnFailed {
                unit,
                error: ferrix_svc::event::Errno(errno),
            });
        };
        let passed = spawn::Passed {
            sockets: spec
                .sockets
                .iter()
                .flat_map(|&socket| {
                    let name = self.display(socket);
                    self.sockets
                        .fds(socket)
                        .into_iter()
                        .map(move |fd| (fd, name.clone()))
                })
                .collect(),
            connection: spec
                .connection
                .and_then(|token| self.sockets.connection(token)),
        };
        if spec.command.path == DEVMGR_PROGRAM
            && spec.role == Role::Main
            && self.directory.as_ref().is_some_and(Directory::has_starter)
        {
            return self.spawn_devmgr(unit, spec);
        }
        if spec.service_type == ServiceType::Native && spec.role == Role::Main {
            return self.spawn_native(unit, spec);
        }
        let given = if spec.bootstrap && spec.role == Role::Main {
            match self
                .directory
                .as_mut()
                .map(|directory| directory.channel_for(unit))
            {
                Some(Ok(given)) => Some(given),
                Some(Err(error)) => {
                    say(&format!(
                        "{}: no bootstrap channel: {error:?}",
                        self.display(unit)
                    ));
                    None
                }
                None => None,
            }
        } else {
            None
        };
        let prepared = match spawn::prepare(spec, &self.terminal, &passed) {
            Ok(prepared) => prepared,
            Err(unprepared) => return failed(self, unprepared.errno, &unprepared.why),
        };
        let Some(cgroup) = self.groups.dir(&spec.group) else {
            let why = format!("its cgroup {} was never made", spec.group);
            return failed(self, libc::ENOENT, &why);
        };
        let started = spawn::start(&prepared, &passed, cgroup);
        // The child has the connection now, or nobody will.
        if let Some(token) = spec.connection {
            self.sockets.close(token);
        }
        match started {
            Ok(spawn::Started {
                pid,
                report,
                log,
                notify,
                go,
            }) => {
                self.groups.filled(&spec.group);
                if let Some(given) = given
                    && let Err(error) = Directory::give(pid, given.end)
                {
                    say(&format!("{}: process_give: {error:?}", self.display(unit)));
                }
                // Given or not, the child may run now.
                if let Some(go) = go {
                    sys::write_once(go.as_raw_fd(), b"g");
                }
                if let Some(notify) = notify {
                    let (id, fd) = self.readiness.add(unit, notify);
                    if let Err(error) = sys::watch(
                        self.epoll.as_fd(),
                        fd,
                        libc::EPOLLIN as u32,
                        token::NOTIFY | u64::from(id),
                    ) {
                        say(&format!("watching a readiness pipe failed: {error}"));
                    }
                }
                if spec.service_type == ServiceType::Forking && spec.role == Role::Main {
                    let _ = self.forking.insert(pid, unit);
                }
                if let Some(log) = log {
                    let (id, fd) = self.logs.add(unit, pid, log);
                    if let Err(error) = sys::watch(
                        self.epoll.as_fd(),
                        fd,
                        libc::EPOLLIN as u32,
                        token::LOG | u64::from(id),
                    ) {
                        say(&format!("watching a log pipe failed: {error}"));
                    }
                }
                let fd = report.as_raw_fd();
                let _ = self.reports.insert(pid, (unit, report));
                if let Err(error) = sys::watch(
                    self.epoll.as_fd(),
                    fd,
                    libc::EPOLLIN as u32,
                    token::REPORT | u64::from(pid),
                ) {
                    say(&format!("watching an exec report failed: {error}"));
                }
                self.queue.push_back(Event::Spawned {
                    unit,
                    pid: Pid(pid),
                });
            }
            Err(error) => {
                let why = format!("clone3 failed: {error}");
                failed(self, error.raw_os_error().unwrap_or(libc::EIO), &why);
            }
        }
    }
}

impl Init {
    /// Start a `Type=native` service's main process (§5.2): in the job
    /// behind its cgroup, from the ELF file `ExecStart=` names, with a
    /// bootstrap channel of its own. Its pid is the one its cgroup has.
    fn spawn_native(&mut self, unit: UnitId, spec: &ferrix_svc::event::SpawnSpec) {
        let name = self.display(unit);
        let fail = |init: &mut Init, why: String| {
            say(&format!("{name}: {why}"));
            init.queue.push_back(Event::SpawnFailed {
                unit,
                error: ferrix_svc::event::Errno(libc::EINVAL),
            });
        };
        if !spec.sandbox.is_empty() {
            return fail(
                self,
                "the sandboxing keys apply to Linux programs, and a Type=native service has \
                 none of them yet; refusing to start it without them"
                    .to_owned(),
            );
        }
        let Some(directory) = self.directory.as_mut() else {
            return fail(self, "Type=native needs the native calls".to_owned());
        };
        let given = match directory.channel_for(unit) {
            Ok(given) => given,
            Err(error) => return fail(self, format!("no bootstrap channel: {error:?}")),
        };
        // As its User=, Group= and SupplementaryGroups= (P0b): a helper
        // that has become them makes the process, and needs the cgroup's
        // cgroup.procs for the length of the start, unless the unit is
        // delegated and has it already.
        let ids = match spawn::identity(spec) {
            Ok(ids) => ids,
            Err(unprepared) => return fail(self, unprepared.why),
        };
        let lent = ids.is_some() && !self.delegated(unit);
        if let (true, Some((uid, gid, _))) = (lent, &ids)
            && let Err(error) = self.groups.lend_procs(unit, *uid, *gid)
        {
            return fail(self, format!("lending its cgroup.procs: {error}"));
        }
        let Some(cgroup) = self.groups.dir(&spec.group) else {
            return fail(self, format!("its cgroup {} was never made", spec.group));
        };
        let Some(directory) = self.directory.as_mut() else {
            return;
        };
        let started =
            directory.start_native(unit, cgroup, &spec.command.path, given.end, ids.as_ref());
        if lent && let Err(error) = self.groups.lend_procs(unit, 0, 0) {
            say(&format!("{name}: taking back its cgroup.procs: {error}"));
        }
        let key = match started {
            Ok(key) => key,
            Err(why) => return fail(self, why),
        };
        self.groups.filled(&spec.group);
        let Some(pid) = self.groups.procs(unit).into_iter().next() else {
            return fail(self, "started, and not in its cgroup".to_owned());
        };
        if let Some(directory) = self.directory.as_mut() {
            directory.found_pid(key, pid);
        }
        self.queue.push_back(Event::Spawned {
            unit,
            pid: Pid(pid),
        });
    }

    /// What init set up on the tmpfs before the switch, set up again where
    /// `/` is now: `/run`, cgroup2 at its place, and the control socket.
    /// The cgroups themselves are the kernel's one tree, and stay.
    fn settle_on_new_root(&mut self) {
        mount_run();
        if let Err(error) = self.groups.remount() {
            say(&format!("cgroup2 at {}: {error}", cgroup::MOUNT));
        }
        if let Some(old) = self.control.take() {
            sys::unwatch(self.epoll.as_fd(), old.fd());
        }
        match Control::listen() {
            Ok(control) => {
                if let Err(error) = sys::watch(
                    self.epoll.as_fd(),
                    control.fd(),
                    libc::EPOLLIN as u32,
                    token::LISTEN,
                ) {
                    say(&format!("watching the control socket failed: {error}"));
                }
                self.control = Some(control);
            }
            Err(error) => say(&format!(
                "the control socket {} could not be made again: {error}",
                ferrix_svc_proto::control::SOCKET
            )),
        }
    }

    /// Start `devmgr.service`'s process by asking the kernel, with the
    /// starter, in the job behind its cgroup (§7.3, L12): the kernel loads
    /// `devmgr` and hands it its devices; init gets only a handle to the
    /// process, and watches it as a native service's.
    fn spawn_devmgr(&mut self, unit: UnitId, spec: &ferrix_svc::event::SpawnSpec) {
        let name = self.display(unit);
        let fail = |init: &mut Init, why: String| {
            say(&format!("{name}: {why}"));
            init.queue.push_back(Event::SpawnFailed {
                unit,
                error: ferrix_svc::event::Errno(libc::EINVAL),
            });
        };
        let Some(cgroup) = self.groups.dir(&spec.group) else {
            return fail(self, format!("its cgroup {} was never made", spec.group));
        };
        let Some(directory) = self.directory.as_mut() else {
            return;
        };
        let key = match directory.start_devmgr(unit, cgroup) {
            Ok(key) => key,
            Err(why) => return fail(self, why),
        };
        self.groups.filled(&spec.group);
        let Some(pid) = self.groups.procs(unit).into_iter().next() else {
            return fail(self, "started, and not in its cgroup".to_owned());
        };
        if let Some(directory) = self.directory.as_mut() {
            directory.found_pid(key, pid);
        }
        self.queue.push_back(Event::Spawned {
            unit,
            pid: Pid(pid),
        });
    }

    /// The port has packets: native services' ends and their channels'
    /// messages.
    fn drain_port(&mut self) {
        let Some(directory) = self.directory.as_mut() else {
            return;
        };
        let (events, lines) = directory.drain();
        let root = directory.take_root();
        for line in lines {
            say(&line);
        }
        self.queue.extend(events);
        if let Some(switched) = root {
            self.rooted(switched);
        }
    }

    /// The kernel said where `/` is (§7.3, L12). On the volume, the units are
    /// read again from it, since pid 1 now is; then the boot goes on.
    fn rooted(&mut self, switched: bool) {
        if switched {
            say("/ is the root volume now, and init with it; reading the units again");
            self.settle_on_new_root();
            self.reload_units();
        } else {
            say("/ stays in memory");
        }
        make_homes();
        self.start_audit();
        if std::mem::take(&mut self.awaiting_root) {
            self.queue.push_back(Event::Boot);
        }
    }

    /// The names a unit's file says it `Offers=`.
    fn offers(&self, unit: UnitId) -> Vec<String> {
        let Some(name) = self.manager.name(unit) else {
            return Vec::new();
        };
        match self.source.load(name.as_str()).map(|loaded| loaded.config) {
            Ok(Config::Service(service)) => service.offers,
            _ => Vec::new(),
        }
    }

    /// Whether `unit` is a service or scope with `Delegate=yes`.
    fn delegated(&self, unit: UnitId) -> bool {
        let Some(name) = self.manager.name(unit) else {
            return false;
        };
        match self.source.load(name.as_str()).map(|loaded| loaded.config) {
            Ok(Config::Service(service)) => service.delegate,
            Ok(Config::Scope(scope)) => scope.delegate,
            _ => false,
        }
    }

    /// `Delegate=yes`: the cgroup just made is its `User=`'s to manage.
    fn delegate(&mut self, unit: UnitId) {
        let Some(name) = self.manager.name(unit).map(|name| name.as_str().to_owned()) else {
            return;
        };
        let (delegate, user) = match self.source.load(&name).map(|loaded| loaded.config) {
            Ok(Config::Service(service)) => (service.delegate, service.user),
            Ok(Config::Scope(scope)) => (scope.delegate, None),
            _ => return,
        };
        if !delegate {
            return;
        }
        let Some(user) = user else {
            // Root's own subtree: nothing to hand over.
            return;
        };
        match spawn::account(&user) {
            Some((uid, gid)) => {
                if let Err(error) = self.groups.delegate(unit, uid, gid) {
                    say(&format!("{name}: delegating its cgroup to {user}: {error}"));
                }
            }
            None => say(&format!(
                "{name}: Delegate=yes, but User={user} is not known"
            )),
        }
    }

    /// A socket unit's sockets are readable: a connection, or data, waits.
    /// Stop watching them until the manager asks again, and tell it.
    fn connected(&mut self, unit: UnitId) {
        self.unwatch_sockets(unit);
        if !self.sockets.accepts(unit) {
            self.queue.push_back(Event::Incoming { unit });
            return;
        }
        match self.sockets.accept(unit) {
            Some(connection) => self.queue.push_back(Event::Accepted { unit, connection }),
            // Gone before it was taken: listen on.
            None => self.perform(Action::Watch { unit }),
        }
    }

    /// Stop watching a socket unit's sockets.
    fn unwatch_sockets(&self, unit: UnitId) {
        for fd in self.sockets.fds(unit) {
            sys::unwatch(self.epoll.as_fd(), fd);
        }
    }

    /// Read readiness pipe `id` into events.
    fn read_notify(&mut self, id: u32) {
        let (events, closed) = self.readiness.read(id);
        self.queue.extend(events);
        if let Some(fd) = closed {
            sys::unwatch(self.epoll.as_fd(), fd);
        }
    }

    /// A forking service's first process exited 0: tell the manager which
    /// process is now its main one, if one can be told (§5.3).
    fn find_forked_main(&mut self, unit: UnitId) {
        let Some(name) = self.manager.name(unit).map(|name| name.as_str().to_owned()) else {
            return;
        };
        let pid_file = match self.source.load(&name).map(|loaded| loaded.config) {
            Ok(Config::Service(service)) => service.pid_file,
            _ => None,
        };
        let procs = self.groups.procs(unit);
        match readiness::forked_main(pid_file.as_deref(), &procs) {
            Some(pid) => self.queue.push_back(Event::MainPid {
                unit,
                pid: Pid(pid),
            }),
            None => say(&format!(
                "{name}: no main process found (PIDFile={}, {} processes left)",
                pid_file.as_deref().unwrap_or("none"),
                procs.len()
            )),
        }
    }

    /// Read log pipe `id`, and say each line as its unit's.
    fn read_log(&mut self, id: u32) {
        let (said, closed) = self.logs.read(id);
        for line in said {
            let name = self.display(line.unit);
            say(&format!("{name}[{}]: {}", line.pid, line.line));
        }
        if let Some(fd) = closed {
            sys::unwatch(self.epoll.as_fd(), fd);
        }
    }

    /// Accept control connections.
    fn accept(&mut self) {
        let Some(control) = self.control.as_mut() else {
            return;
        };
        for (id, fd) in control.accept() {
            let token = token::CLIENT | (id & 0xffff_ffff);
            if let Err(error) = sys::watch(self.epoll.as_fd(), fd, libc::EPOLLIN as u32, token) {
                say(&format!("watching a control connection failed: {error}"));
            }
        }
    }

    /// A control connection is readable, or has room to write.
    fn client(&mut self, id: u64, events: u32) {
        let Some(control) = self.control.as_mut() else {
            return;
        };
        if events & libc::EPOLLOUT as u32 != 0 {
            let waiting = control.flush(id);
            self.rewatch_client(id, waiting);
        }
        if events & (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32 == 0 {
            return;
        }
        let Some(control) = self.control.as_mut() else {
            return;
        };
        match control.read(id) {
            Heard::Call { client, uid, call } => self.call(client, uid, call),
            Heard::Gone => {
                let _ = self.pending.remove(&id);
            }
            Heard::Nothing => {}
        }
    }

    /// Watch a connection for room to write while it has bytes waiting.
    fn rewatch_client(&mut self, id: u64, waiting: Option<bool>) {
        let Some(waiting) = waiting else {
            let _ = self.pending.remove(&id);
            return;
        };
        let Some(fd) = self
            .control
            .as_ref()
            .and_then(|control| control.client_fd(id))
        else {
            return;
        };
        let mut events = libc::EPOLLIN as u32;
        if waiting {
            events |= libc::EPOLLOUT as u32;
        }
        let _ = sys::rewatch(
            self.epoll.as_fd(),
            fd,
            events,
            token::CLIENT | (id & 0xffff_ffff),
        );
    }

    /// Answer control connection `id`.
    fn answer(&mut self, id: u64, answer: &Answer) {
        let Some(control) = self.control.as_mut() else {
            return;
        };
        let waiting = control.answer(id, answer);
        if answer.is_final() {
            let _ = self.pending.remove(&id);
        }
        self.rewatch_client(id, waiting);
    }

    /// A call from `svc`, made by `uid`.
    fn call(&mut self, id: u64, uid: u32, call: Call) {
        if let Some(why) = refusal(uid, &call) {
            self.answer(id, &Answer::Refused(why));
            return;
        }
        let client = ClientId(id);
        let ask = |init: &mut Init, pending: Pending, request: Request| {
            let _ = init.pending.insert(id, pending);
            init.queue.push_back(Event::Request { client, request });
        };
        match call {
            Call::Status(unit) => ask(self, Pending::Units(None), Request::Status(unit)),
            Call::List { failed } => {
                ask(self, Pending::Units(Some(failed)), Request::Status(None));
            }
            Call::Start(unit) => ask(self, Pending::Done, Request::start(&unit)),
            Call::Stop(unit) => ask(self, Pending::Done, Request::stop(&unit)),
            Call::Restart(unit) => ask(self, Pending::Done, Request::restart(&unit)),
            Call::Reload(unit) => ask(self, Pending::Done, Request::Reload(unit)),
            Call::Isolate(unit) => ask(self, Pending::Done, Request::Isolate(unit)),
            Call::ResetFailed(unit) => ask(self, Pending::Done, Request::ResetFailed(unit)),
            Call::Poweroff => ask(self, Pending::Done, Request::Poweroff),
            Call::Reboot => ask(self, Pending::Done, Request::Reboot),
            Call::Scope { unit, slice, pids } => {
                let pids = pids.into_iter().map(Pid).collect();
                ask(self, Pending::Done, Request::Scope { unit, slice, pids });
            }
            Call::Log { unit, lines } => {
                let answer = match self.manager.unit(&unit) {
                    Some(found) => {
                        let count = usize::try_from(lines).unwrap_or(usize::MAX);
                        Answer::Lines(self.logs.tail(found, count))
                    }
                    None => Answer::Refused(format!("{unit}: not loaded")),
                };
                self.answer(id, &answer);
            }
            Call::DaemonReload => {
                self.reload_units();
                self.answer(id, &Answer::Done("done".to_owned()));
            }
            Call::Enable(unit) => {
                let done = admin::enable(&self.source, &unit);
                self.changed(id, done);
            }
            Call::Disable(unit) => self.changed(id, admin::disable(&unit)),
            Call::Mask(unit) => self.changed(id, admin::mask(&unit)),
            Call::Unmask(unit) => self.changed(id, admin::unmask(&unit)),
            Call::SetProperty {
                unit,
                assignments,
                persistent,
            } => {
                // Written to the running cgroup before the answer, so what
                // the caller reads next is the new limit.
                match admin::set_property(&unit, &assignments, persistent) {
                    Ok(notes) => {
                        for note in notes {
                            self.answer(id, &Answer::Note(note));
                        }
                        self.reload_units();
                        self.apply_limits(&unit);
                        self.answer(id, &Answer::Done("done".to_owned()));
                    }
                    Err(error) => self.answer(id, &Answer::Refused(error.to_string())),
                }
            }
        }
    }

    /// The unit directories changed: say what changed, read them again,
    /// and answer.
    fn changed(&mut self, id: u64, done: io::Result<Vec<String>>) {
        match done {
            Ok(notes) => {
                for note in notes {
                    self.answer(id, &Answer::Note(note));
                }
                self.reload_units();
                self.answer(id, &Answer::Done("done".to_owned()));
            }
            Err(error) => self.answer(id, &Answer::Refused(error.to_string())),
        }
    }

    /// `svc daemon-reload`: run the generators and read the directories
    /// again; what runs is left running.
    fn reload_units(&mut self) {
        run_generators();
        let mut log = |line: String| say(&line);
        let source = units::read(&mut log);
        self.source = source.clone();
        self.manager.reload(source);
    }

    /// Write a running unit's limits again, as its files now say them.
    fn apply_limits(&mut self, name: &str) {
        let Some(unit) = self.manager.unit(name) else {
            return;
        };
        let limits = match self.source.load(name).map(|loaded| loaded.config) {
            Ok(Config::Service(service)) => service.limits,
            Ok(Config::Slice(slice)) => slice.limits,
            Ok(Config::Scope(scope)) => scope.limits,
            _ => return,
        };
        let mut log = |line: String| say(&line);
        self.groups.set_limits(unit, &limits, &mut log);
    }

    /// The manager answered a client.
    fn reply(&mut self, client: ClientId, reply: Reply) {
        if client == SIGNALLED {
            return;
        }
        let id = client.0;
        let pending = self.pending.get(&id).copied();
        let answer = match reply {
            Reply::Done(result) => Answer::Done(result.name().to_owned()),
            Reply::Refused(why) => Answer::Refused(why),
            Reply::Status(list) => {
                let failed_only = matches!(pending, Some(Pending::Units(Some(true))));
                Answer::Units(
                    list.iter()
                        .filter(|status| !failed_only || status.active.name() == "failed")
                        .map(|status| self.unit_status(status))
                        .collect(),
                )
            }
        };
        self.answer(id, &answer);
    }

    /// One unit's status on the wire.
    fn unit_status(&self, status: &Status) -> UnitStatus {
        UnitStatus {
            name: status.name.clone(),
            description: status.description.clone(),
            load: status.load.to_owned(),
            active: status.active.name().to_owned(),
            sub: status.sub.to_owned(),
            main: status.main.map(|pid| pid.0),
            result: status.result.map(|result| result.name().to_owned()),
            status: status.status.clone(),
            cgroup: self
                .manager
                .group(status.unit)
                .map(|group| group.as_str().to_owned()),
        }
    }
}

/// Why `uid` may not make `call`, if it may not (§10): anyone may read,
/// only root may change state, and a user may group their own processes in
/// a scope under their own slice.
fn refusal(uid: u32, call: &Call) -> Option<String> {
    if uid == 0 || !call.changes_state() {
        return None;
    }
    if let Call::Scope { slice, pids, .. } = call {
        let own = format!("user-{uid}.slice");
        let owned = pids.iter().all(|&pid| owner(pid) == Some(uid));
        if slice.as_deref() == Some(own.as_str()) && owned {
            return None;
        }
        return Some(format!(
            "Permission denied: uid {uid} may make a scope only of its own processes, under {own}"
        ));
    }
    Some(format!(
        "Permission denied: only root may change the system, and this is uid {uid}"
    ))
}

/// The real uid of process `pid`, from `/proc/<pid>/status`.
fn owner(pid: u32) -> Option<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = text.lines().find(|line| line.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Each account's home under `/home` that is not there, made empty, its
/// own and private: `0700`, with the account's uid and gid.
///
/// `/home` is the home disk when the machine has one, which starts empty
/// and is kept when the root is made again (`cargo xtask run
/// --reset-root`); the system's archive carries the homes, but onto the
/// root, where the disk's mount hides them. A home that is there is left as
/// it is, so this does nothing on a boot without the disk. Done once `/` is
/// settled, before any unit runs as an account.
fn make_homes() {
    use std::os::unix::fs::{DirBuilderExt, chown};
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, _, home, ..] = fields.as_slice() else {
            continue;
        };
        let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) else {
            continue;
        };
        let Some(rest) = home.strip_prefix("/home/") else {
            continue;
        };
        if rest.is_empty() || rest.contains('/') || fs::symlink_metadata(home).is_ok() {
            continue;
        }
        let made = fs::DirBuilder::new()
            .mode(0o700)
            .create(home)
            .and_then(|()| chown(home, Some(uid), Some(gid)));
        match made {
            Ok(()) => say(&format!("made {name}'s home {home}")),
            Err(error) => say(&format!("{name}'s home {home} could not be made: {error}")),
        }
    }
}

/// A tmpfs on `/run`, and the runtime unit directory in it.
fn mount_run() {
    let _ = fs::create_dir("/run");
    match sys::mount(
        c"tmpfs",
        c"/run",
        c"tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(c"mode=755"),
    ) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {}
        Err(error) => say(&format!("mounting a tmpfs on /run failed: {error}")),
    }
    if let Err(error) = fs::create_dir_all(RUNTIME_UNITS) {
        say(&format!("making {RUNTIME_UNITS} failed: {error}"));
    }
}

/// Run every generator, in name order, each to its end or its deadline.
fn run_generators() {
    let Ok(entries) = fs::read_dir(GENERATORS) else {
        return;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .collect();
    paths.sort();
    for path in paths {
        let name = path.display().to_string();
        match run_generator(&path) {
            Ok(Exit::Code(0)) => {}
            Ok(how) => say(&format!("generator {name} ended: {how:?}")),
            Err(why) => say(&format!("generator {name}: {why}")),
        }
    }
}

/// Run one generator with the runtime directory as its argument.
fn run_generator(path: &Path) -> Result<Exit, String> {
    let program = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let argument = CString::new(RUNTIME_UNITS).map_err(|e| e.to_string())?;
    let argv = [program.as_ptr(), argument.as_ptr(), std::ptr::null()];
    let envp = [c"PATH=/bin:/sbin".as_ptr(), std::ptr::null()];
    match sys::fork().map_err(|e| format!("fork failed: {e}"))? {
        sys::Forked::Child => {
            let _ = sys::unblock_all();
            let error = sys::execve(&program, &argv, &envp);
            say(&format!("{}: execve failed: {error}", path.display()));
            sys::exit_now(127)
        }
        sys::Forked::Parent(pid) => match sys::wait_for(pid, GENERATOR_PATIENCE) {
            Some(how) => Ok(how),
            None => {
                let _ = sys::kill(pid, libc::SIGKILL);
                let _ = sys::wait_for(pid, GENERATOR_PATIENCE);
                Err("did not finish within 5 s, and was killed".to_owned())
            }
        },
    }
}

/// Mount a `.mount` unit. A mount the kernel has made already is a
/// success (§8.1).
fn mount(spec: &MountSpec) -> Result<(), ferrix_svc::event::Errno> {
    let errno =
        |error: io::Error| ferrix_svc::event::Errno(error.raw_os_error().unwrap_or(libc::EIO));
    if mounted(&spec.r#where) {
        return Ok(());
    }
    let _ = fs::create_dir_all(&spec.r#where);
    let (flags, data) = mount_options(&spec.options);
    let c = |text: &str| CString::new(text).map_err(|_| ferrix_svc::event::Errno(libc::EINVAL));
    let what = c(&spec.what)?;
    let target = c(&spec.r#where)?;
    let fs_type = c(spec.fs_type.as_deref().unwrap_or("auto"))?;
    let data = if data.is_empty() {
        None
    } else {
        Some(c(&data)?)
    };
    match sys::mount(&what, &target, &fs_type, flags, data.as_deref()) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EBUSY) => Ok(()),
        Err(error) => Err(errno(error)),
    }
}

/// Unmount what a `.mount` unit mounted.
fn unmount(target: &str) -> Result<(), ferrix_svc::event::Errno> {
    let target = CString::new(target).map_err(|_| ferrix_svc::event::Errno(libc::EINVAL))?;
    sys::unmount(&target)
        .map_err(|error| ferrix_svc::event::Errno(error.raw_os_error().unwrap_or(libc::EIO)))
}

/// Whether something is mounted at `target`, by `/proc/self/mounts`.
fn mounted(target: &str) -> bool {
    fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(target))
}

/// `Options=`, split into the flags `mount(2)` takes and the rest, which
/// goes to the filesystem as its data.
fn mount_options(options: &str) -> (libc::c_ulong, String) {
    let mut flags = 0;
    let mut data = Vec::new();
    for option in options.split(',').filter(|option| !option.is_empty()) {
        match option {
            "ro" => flags |= libc::MS_RDONLY,
            "rw" | "defaults" => {}
            "nosuid" => flags |= libc::MS_NOSUID,
            "nodev" => flags |= libc::MS_NODEV,
            "noexec" => flags |= libc::MS_NOEXEC,
            "noatime" => flags |= libc::MS_NOATIME,
            "relatime" => flags |= libc::MS_RELATIME,
            other => data.push(other),
        }
    }
    (flags, data.join(","))
}

/// What a write to `target` after its read-only remount found, as a line to
/// say: `EROFS`, which is what the remount promised (`test-init` requires
/// the line), or a file made after all, which is then removed again.
fn read_only_probe(target: &str) -> String {
    let probe = format!("{}/.init-read-only-probe", target.trim_end_matches('/'));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Err(error) if error.raw_os_error() == Some(libc::EROFS) => {
            format!("{target} is read-only")
        }
        Err(error) => format!("remounting {target} read-only: a write found {error}"),
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            format!("remounting {target} read-only: it still takes writes")
        }
    }
}

/// §8.2's steps 2 and 3: `sync`, the rest unmounted in reverse order, the
/// audit record read a last time onto the volume, `sync` again, `/`,
/// `/data` and `/home` read-only, and `reboot(2)`. A remount the kernel
/// refuses is said and passed over: `reboot(2)` commits all three itself
/// (K7).
///
/// The audit record's last read is as late as a write to `/` can be, so
/// that as little as possible is made after it: what is, the kernel prints
/// on the console with the power action's own record (AUDIT.md §4).
fn power(action: PowerAction, audit: Option<&mut audit::Reader>) -> ! {
    sys::sync();
    let table = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let targets: Vec<&str> = table
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    let mut kept = Vec::new();
    for target in targets.iter().rev() {
        if matches!(*target, "/" | "/data" | "/home") {
            continue;
        }
        let Ok(path) = CString::new(*target) else {
            continue;
        };
        if sys::unmount(&path).is_err() {
            kept.push(*target);
        }
    }
    if !kept.is_empty() {
        say(&format!("still mounted, and left so: {}", kept.join(" ")));
    }
    if let Some(reader) = audit {
        match reader.read() {
            Ok(()) => say(&reader.said()),
            Err(error) => say(&format!("audit: the last read failed: {error}")),
        }
    }
    sys::sync();
    for target in ["/", "/data", "/home"] {
        if !targets.contains(&target) {
            continue;
        }
        let Ok(path) = CString::new(target) else {
            continue;
        };
        let flags = libc::MS_REMOUNT | libc::MS_RDONLY;
        match sys::mount(c"none", &path, c"none", flags, None) {
            Err(error) => say(&format!("remounting {target} read-only: {error}")),
            Ok(()) => say(&read_only_probe(target)),
        }
    }
    let command = match action {
        PowerAction::Poweroff => libc::RB_POWER_OFF,
        PowerAction::Reboot => libc::RB_AUTOBOOT,
    };
    let error = sys::reboot(command);
    say(&format!("reboot(2) failed: {error}"));
    sys::exit_now(1)
}
