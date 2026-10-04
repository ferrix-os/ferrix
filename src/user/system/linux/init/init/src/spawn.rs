//! The Spawn backend (§5.2): a [`SpawnSpec`] turned into a process in its
//! unit's cgroup.
//!
//! Everything the child needs is worked out first, in the parent, where a
//! failure can be reported as [`Event::SpawnFailed`]: the program found on
//! the search path, `$VAR` expanded, the user looked up, the environment
//! files read, and every string made a C string. Then `clone3` puts the
//! child in the cgroup from its first instruction. The child only makes
//! system calls: it unblocks its signals, leads a session of its own, takes
//! its terminal, sets up its three streams, changes directory and user, and
//! calls `execve`.
//!
//! A pipe closed on exec tells the parent how that went (§5.2 step 5). The
//! child writes the step that failed and its error number to it; a success
//! closes it with nothing written, which is
//! [`Event::Execed`](ferrix_svc::event::Event). A child that fails ends with
//! systemd's exit code for the step, so its unit fails as the unit of a
//! program that could not run fails there.
//!
//! The sandboxing keys (§4.5) are [`crate::sandbox`]'s: planned in the
//! parent with the rest, entered after the streams are set up and before
//! the directory and the user change, and locked last, after init's
//! go-ahead and just before `execve`.
//!
//! [`Event::SpawnFailed`]: ferrix_svc::event::Event

use std::collections::BTreeMap;
use std::ffi::{CString, c_char};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::ptr;

use ferrix_svc::event::SpawnSpec;
use ferrix_svc::exec::Command;
use ferrix_svc::kind::{Input, Output, ServiceType};

use crate::sandbox::{self, Plan};
use crate::sys::{self, Forked};

/// The search path for a command that is not an absolute path, and the
/// `PATH` a service starts with: systemd's.
pub(crate) const SEARCH_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A step of the child's, and systemd's exit code for its failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Step {
    /// Changing directory: `EXIT_CHDIR`.
    Chdir = 200,
    /// `execve`: `EXIT_EXEC`.
    Exec = 203,
    /// Standard input: `EXIT_STDIN`.
    Stdin = 208,
    /// Standard output or error: `EXIT_STDOUT`.
    Stdout = 209,
    /// The user: `EXIT_USER`.
    User = 217,
    /// The session: `EXIT_SETSID`.
    Setsid = 220,
    /// The mount namespace and its mounts: `EXIT_NAMESPACE`.
    Namespace = 226,
    /// `PR_SET_NO_NEW_PRIVS`: `EXIT_NO_NEW_PRIVILEGES`.
    NoNewPrivileges = 227,
    /// The seccomp filter: `EXIT_SECCOMP`.
    Seccomp = 228,
}

impl Step {
    /// The step a report names.
    fn from_code(code: u32) -> Option<Step> {
        [
            Step::Chdir,
            Step::Exec,
            Step::Stdin,
            Step::Stdout,
            Step::User,
            Step::Setsid,
            Step::Namespace,
            Step::NoNewPrivileges,
            Step::Seccomp,
        ]
        .into_iter()
        .find(|step| *step as u32 == code)
    }

    /// What the step was doing, for the log.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Step::Chdir => "changing directory",
            Step::Exec => "execve",
            Step::Stdin => "opening standard input",
            Step::Stdout => "opening standard output",
            Step::User => "setting the user and groups",
            Step::Setsid => "setsid",
            Step::Namespace => "setting up the sandbox's mount namespace",
            Step::NoNewPrivileges => "setting no_new_privs",
            Step::Seccomp => "installing the seccomp filter",
        }
    }
}

/// What the report pipe said.
#[derive(Debug)]
pub(crate) enum Report {
    /// Nothing: `execve` succeeded.
    Execed,
    /// A step failed.
    Failed(Step, io::Error),
}

/// Read what a child wrote to its report pipe before it closed.
pub(crate) fn read_report(pipe: &OwnedFd) -> Report {
    let mut bytes = [0_u8; 8];
    let mut file = fs::File::from(match pipe.try_clone() {
        Ok(fd) => fd,
        Err(error) => return Report::Failed(Step::Exec, error),
    });
    let got = io::Read::read(&mut file, &mut bytes).unwrap_or(0);
    if got < bytes.len() {
        return Report::Execed;
    }
    let [a, b, c, d, e, f, g, h] = bytes;
    let code = u32::from_le_bytes([a, b, c, d]);
    let errno = i32::from_le_bytes([e, f, g, h]);
    Report::Failed(
        Step::from_code(code).unwrap_or(Step::Exec),
        io::Error::from_raw_os_error(errno),
    )
}

/// Where one of the child's streams comes from.
#[derive(Debug)]
enum Stream {
    /// Open this path with these flags.
    Open(CString, libc::c_int),
    /// The terminal the child took.
    Tty,
    /// A copy of a stream set up before it: standard error as output.
    Same(RawFd),
    /// The log's pipe (§10), made when the process is started.
    Log,
    /// The connection an `Accept=yes` socket took.
    Socket,
}

/// What a spawn is handed from a `.socket` unit: the listening sockets for
/// `LISTEN_FDS`, each with its unit's name, and an accepted connection.
#[derive(Debug, Default)]
pub(crate) struct Passed {
    pub(crate) sockets: Vec<(RawFd, String)>,
    pub(crate) connection: Option<RawFd>,
}

/// A spawn worked out in the parent: nothing the child does allocates.
#[derive(Debug)]
pub(crate) struct Prepared {
    path: CString,
    argv: Vec<CString>,
    envp: Vec<CString>,
    /// The terminal to take, and whether to take it from another session.
    tty: Option<(CString, bool)>,
    streams: [Stream; 3],
    directory: Option<(CString, bool)>,
    user: Option<Ids>,
    /// For `Type=notify`: the descriptor its readiness pipe goes on
    /// (`NotifyFd=`, 3 when the unit names none).
    notify: Option<libc::c_int>,
    /// Where `LISTEN_PID=` is in `envp`: the child writes its own pid there.
    listen_pid: Option<usize>,
    /// Whether the child waits, before `execve`, for init to have given it
    /// its bootstrap channel (§5.2 step 3).
    waits_for_bootstrap: bool,
    /// The sandboxing keys, worked out (§4.5).
    sandbox: Option<Plan>,
}

/// Why a spawn could not be worked out.
#[derive(Debug)]
pub(crate) struct Unprepared {
    /// The error number to report.
    pub(crate) errno: i32,
    /// What went wrong, for the log.
    pub(crate) why: String,
}

impl Unprepared {
    fn new(errno: i32, why: String) -> Unprepared {
        Unprepared { errno, why }
    }
}

/// A C string, or the reason a string with a NUL in it cannot be one.
fn c_string(text: &str) -> Result<CString, Unprepared> {
    CString::new(text).map_err(|_| Unprepared::new(libc::EINVAL, format!("{text:?} holds a NUL")))
}

/// Work out everything the child needs from `spec`, with `terminal` the
/// `TERM` a service on a terminal gets unless it sets its own.
pub(crate) fn prepare(
    spec: &SpawnSpec,
    terminal: &str,
    passed: &Passed,
) -> Result<Prepared, Unprepared> {
    // First, so a key the kernel cannot do refuses the unit before anything
    // else is looked at.
    let unprivileged = spec
        .user
        .as_deref()
        .is_some_and(|user| user != "root" && user != "0");
    let sandbox = sandbox::plan(&spec.sandbox, unprivileged, spec.bootstrap)?;
    let user = match &spec.user {
        Some(name) => Some(lookup_user(name)?),
        None => None,
    };
    let mut environment: BTreeMap<String, String> = BTreeMap::new();
    let _ = environment.insert("PATH".to_owned(), SEARCH_PATH.to_owned());
    if let Some(account) = &user {
        let _ = environment.insert("HOME".to_owned(), account.home.clone());
        let _ = environment.insert("USER".to_owned(), account.name.clone());
        let _ = environment.insert("LOGNAME".to_owned(), account.name.clone());
        let _ = environment.insert("SHELL".to_owned(), account.shell.clone());
    }
    if spec.tty.is_some() {
        let _ = environment.insert("TERM".to_owned(), terminal.to_owned());
    }
    let notify = match spec.service_type {
        ServiceType::Notify => {
            let fd = spec.notify_fd.unwrap_or(3);
            let fd = libc::c_int::try_from(fd)
                .ok()
                .filter(|fd| (3..64).contains(fd))
                .ok_or_else(|| {
                    Unprepared::new(libc::EINVAL, format!("NotifyFd={fd} is not from 3 to 63"))
                })?;
            // How a service learns where to write, as NOTIFY_SOCKET tells
            // a systemd service.
            let _ = environment.insert("NOTIFY_FD".to_owned(), fd.to_string());
            Some(fd)
        }
        _ => None,
    };
    let sockets = passed.sockets.len();
    if sockets > 0 {
        // sd_listen_fds(3)'s words: the count, whose names, and whose pid.
        let names: Vec<&str> = passed
            .sockets
            .iter()
            .map(|(_, name)| name.as_str())
            .collect();
        let _ = environment.insert("LISTEN_FDS".to_owned(), sockets.to_string());
        let _ = environment.insert("LISTEN_FDNAMES".to_owned(), names.join(":"));
        let _ = environment.insert("LISTEN_PID".to_owned(), String::new());
        let last = 3 + libc::c_int::try_from(sockets).unwrap_or(libc::c_int::MAX);
        if notify.is_some_and(|fd| fd < last) {
            return Err(Unprepared::new(
                libc::EINVAL,
                format!("NotifyFd= falls among the {sockets} sockets passed from 3"),
            ));
        }
    }
    for (name, value) in &spec.environment {
        let _ = environment.insert(name.clone(), value.clone());
    }
    for (path, missing_ok) in &spec.environment_files {
        match fs::read_to_string(path) {
            Ok(text) => environment.extend(environment_file(&text)),
            Err(_) if *missing_ok => {}
            Err(error) => {
                return Err(Unprepared::new(
                    error.raw_os_error().unwrap_or(libc::EIO),
                    format!("reading EnvironmentFile={path}: {error}"),
                ));
            }
        }
    }

    let argv = arguments(&spec.command, &environment);
    let path = find(&spec.command.path)?;
    let user_ids = ids(spec, user.as_ref())?;

    let tty = match &spec.tty {
        Some(path) => {
            let force = matches!(spec.stdin, Input::TtyForce);
            Some((c_string(path)?, force))
        }
        None => None,
    };
    let stdin = match &spec.stdin {
        Input::Null => Stream::Open(c"/dev/null".to_owned(), libc::O_RDONLY),
        Input::Tty | Input::TtyForce | Input::TtyFail => Stream::Tty,
        Input::File(path) => Stream::Open(c_string(path)?, libc::O_RDONLY),
        Input::Socket => Stream::Socket,
    };
    let stdin_is_tty = matches!(stdin, Stream::Tty);
    let stdout = output(&spec.stdout, stdin_is_tty, None)?;
    let stderr = output(&spec.stderr, stdin_is_tty, Some(1))?;

    let directory = match &spec.working_directory {
        Some(directory) => {
            let path = match (directory.path.as_str(), &user) {
                ("~", Some(account)) => account.home.clone(),
                ("~", None) => "/root".to_owned(),
                (path, _) => path.to_owned(),
            };
            Some((c_string(&path)?, directory.missing_ok))
        }
        None => Some((c"/".to_owned(), false)),
    };

    let listen_pid = environment.keys().position(|name| name == "LISTEN_PID");
    Ok(Prepared {
        listen_pid,
        waits_for_bootstrap: spec.bootstrap && spec.role == ferrix_svc::event::Role::Main,
        path: c_string(&path)?,
        argv: argv
            .iter()
            .map(|word| c_string(word))
            .collect::<Result<_, _>>()?,
        envp: environment
            .iter()
            .map(|(name, value)| c_string(&format!("{name}={value}")))
            .collect::<Result<_, _>>()?,
        tty,
        streams: [stdin, stdout, stderr],
        directory,
        user: user_ids,
        notify,
        sandbox,
    })
}

/// Where standard output or error goes. `Inherit` is standard input's
/// terminal if it has one, and otherwise the log, a pipe init reads and
/// says on the console (§10). Standard error's `Inherit` is standard
/// output.
fn output(
    output: &Output,
    stdin_is_tty: bool,
    inherit_from: Option<RawFd>,
) -> Result<Stream, Unprepared> {
    let write = libc::O_WRONLY | libc::O_NOCTTY;
    Ok(match output {
        Output::Inherit => match inherit_from {
            Some(fd) => Stream::Same(fd),
            None if stdin_is_tty => Stream::Tty,
            None => Stream::Log,
        },
        Output::Null => Stream::Open(c"/dev/null".to_owned(), write),
        Output::Tty => Stream::Tty,
        Output::Console => Stream::Open(c"/dev/console".to_owned(), write),
        Output::Log => Stream::Log,
        Output::File(path) => Stream::Open(c_string(path)?, write | libc::O_CREAT),
        Output::Append(path) => {
            Stream::Open(c_string(path)?, write | libc::O_CREAT | libc::O_APPEND)
        }
        Output::Truncate(path) => {
            Stream::Open(c_string(path)?, write | libc::O_CREAT | libc::O_TRUNC)
        }
        Output::Socket => Stream::Socket,
    })
}

/// A command's words, `$VAR` and `${VAR}` expanded unless `:` said not.
/// A word that is only `$VAR` becomes that variable's words, split at
/// whitespace; anywhere else the value goes in as it is (systemd's rule).
fn arguments(command: &Command, environment: &BTreeMap<String, String>) -> Vec<String> {
    if !command.expand_environment {
        return command.argv.clone();
    }
    let mut argv = Vec::new();
    for word in &command.argv {
        if let Some(name) = word.strip_prefix('$')
            && ferrix_svc::value::is_env_name(name)
        {
            if let Some(value) = environment.get(name) {
                argv.extend(value.split_whitespace().map(str::to_owned));
            }
            continue;
        }
        argv.push(expand(word, environment));
    }
    argv
}

/// `$VAR` and `${VAR}` in `word`, replaced by their values; an unset one is
/// empty. `$$` is one `$`.
fn expand(word: &str, environment: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = word;
    while let Some(at) = rest.find('$') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at + 1..).unwrap_or_default();
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
        } else if let Some(braced) = after.strip_prefix('{')
            && let Some(end) = braced.find('}')
        {
            let name = braced.get(..end).unwrap_or_default();
            out.push_str(environment.get(name).map_or("", String::as_str));
            rest = braced.get(end + 1..).unwrap_or_default();
        } else {
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            let name = after.get(..end).unwrap_or_default();
            if name.is_empty() {
                out.push('$');
            } else {
                out.push_str(environment.get(name).map_or("", String::as_str));
            }
            rest = after.get(end..).unwrap_or_default();
        }
    }
    out.push_str(rest);
    out
}

/// `KEY=value` lines, as `EnvironmentFile=` reads them: blank lines and
/// `#` or `;` comments skipped, one level of quotes taken off.
fn environment_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with(';'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| {
            let value = value.trim();
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            (name.trim().to_owned(), unquoted.to_owned())
        })
        .filter(|(name, _)| ferrix_svc::value::is_env_name(name))
        .collect()
}

/// The program: an absolute path as it is, a bare name looked for on
/// [`SEARCH_PATH`].
fn find(program: &str) -> Result<String, Unprepared> {
    if program.starts_with('/') {
        return Ok(program.to_owned());
    }
    SEARCH_PATH
        .split(':')
        .map(|dir| format!("{dir}/{program}"))
        .find(|path| {
            fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| Unprepared::new(libc::ENOENT, format!("{program} is not on {SEARCH_PATH}")))
}

/// An account from `/etc/passwd`.
#[derive(Debug)]
struct Account {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
}

/// `User=`: a name in `/etc/passwd`, or a number.
fn lookup_user(user: &str) -> Result<Account, Unprepared> {
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let found = passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, _, home, shell, ..] = fields.as_slice() else {
            return None;
        };
        (*name == user || *uid == user).then(|| Account {
            name: (*name).to_owned(),
            uid: uid.parse().unwrap_or(u32::MAX),
            gid: gid.parse().unwrap_or(u32::MAX),
            home: (*home).to_owned(),
            shell: (*shell).to_owned(),
        })
    });
    match (found, user.parse::<u32>()) {
        (Some(account), _) => Ok(account),
        (None, Ok(uid)) => Ok(Account {
            name: user.to_owned(),
            uid,
            gid: uid,
            home: "/".to_owned(),
            shell: "/bin/sh".to_owned(),
        }),
        (None, Err(_)) => Err(Unprepared::new(
            libc::ESRCH,
            format!("User={user} is not in /etc/passwd"),
        )),
    }
}

/// The ids a unit's processes run as: `User=`'s uid, `Group=`'s gid or else
/// the user's own, and `SupplementaryGroups=`. `None` when the unit names
/// neither a user nor a group, and runs as init does.
fn ids(spec: &SpawnSpec, user: Option<&Account>) -> Result<Option<Ids>, Unprepared> {
    let group_ids = match (&spec.group_name, user) {
        (Some(name), _) => lookup_group(name)?,
        (None, Some(account)) => account.gid,
        (None, None) => 0,
    };
    let supplementary = spec
        .supplementary_groups
        .iter()
        .map(|name| lookup_group(name))
        .collect::<Result<Vec<u32>, Unprepared>>()?;
    Ok(user
        .map(|account| (account.uid, group_ids, supplementary.clone()))
        .or_else(|| {
            spec.group_name
                .as_ref()
                .map(|_| (0, group_ids, supplementary))
        }))
}

/// A uid, a gid and the supplementary groups.
pub(crate) type Ids = (u32, u32, Vec<u32>);

/// [`ids`] for a unit whose process init does not `execve` itself: a
/// `Type=native` service, which a helper that has become these ids makes.
pub(crate) fn identity(spec: &SpawnSpec) -> Result<Option<Ids>, Unprepared> {
    let user = match &spec.user {
        Some(name) => Some(lookup_user(name)?),
        None => None,
    };
    ids(spec, user.as_ref())
}

/// The uid and gid of `User=`, for `Delegate=`.
pub(crate) fn account(user: &str) -> Option<(u32, u32)> {
    lookup_user(user)
        .ok()
        .map(|account| (account.uid, account.gid))
}

/// `Group=`: a name in `/etc/group`, or a number.
fn lookup_group(group: &str) -> Result<u32, Unprepared> {
    if let Ok(gid) = group.parse() {
        return Ok(gid);
    }
    fs::read_to_string("/etc/group")
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let gid = fields.nth(1)?;
            (name == group).then(|| gid.parse().ok()).flatten()
        })
        .ok_or_else(|| Unprepared::new(libc::ESRCH, format!("Group={group} is not in /etc/group")))
}

/// A started process: its pid, the read end of its report pipe, and the
/// read end of its log pipe when a stream goes to the log.
#[derive(Debug)]
pub(crate) struct Started {
    pub(crate) pid: u32,
    pub(crate) report: OwnedFd,
    pub(crate) log: Option<OwnedFd>,
    /// The read end of a `Type=notify` service's readiness pipe.
    pub(crate) notify: Option<OwnedFd>,
    /// For a child waiting for its bootstrap channel: the pipe to write
    /// one byte to once it is given, or to drop to let it run without.
    pub(crate) go: Option<OwnedFd>,
}

/// Start `prepared` in the cgroup `cgroup` is open on.
pub(crate) fn start(
    prepared: &Prepared,
    passed: &Passed,
    cgroup: BorrowedFd<'_>,
) -> io::Result<Started> {
    let (report, write_end) = sys::pipe()?;
    // Copies above anything the child moves them to, so one `dup2` to 3
    // cannot land on another socket before that one has moved.
    let sockets = passed
        .sockets
        .iter()
        .map(|(fd, _)| sys::duplicate_high(*fd))
        .collect::<io::Result<Vec<OwnedFd>>>()?;
    let connection = passed.connection.map(sys::duplicate_high).transpose()?;
    let log = if prepared
        .streams
        .iter()
        .any(|stream| matches!(stream, Stream::Log))
    {
        let (read, write) = sys::pipe()?;
        sys::nonblocking(read.as_raw_fd())?;
        Some((read, write))
    } else {
        None
    };
    let go = if prepared.waits_for_bootstrap {
        Some(sys::pipe()?)
    } else {
        None
    };
    let notify = match prepared.notify {
        Some(_) => {
            let (read, write) = sys::pipe()?;
            sys::nonblocking(read.as_raw_fd())?;
            Some((read, write))
        }
        None => None,
    };
    // Built before the clone: the child must not allocate.
    let argv = pointers(&prepared.argv);
    let mut envp = pointers(&prepared.envp);
    let pipes = Pipes {
        report: write_end.as_raw_fd(),
        log: log.as_ref().map(|(_, write)| write.as_raw_fd()),
        notify: notify.as_ref().map(|(_, write)| write.as_raw_fd()),
        sockets: sockets.iter().map(AsRawFd::as_raw_fd).collect(),
        connection: connection.as_ref().map(AsRawFd::as_raw_fd),
        go: go.as_ref().map(|(read, _)| read.as_raw_fd()),
    };
    match sys::fork_into(cgroup)? {
        Forked::Child => child(prepared, &argv, &mut envp, &pipes),
        Forked::Parent(pid) => {
            drop(write_end);
            drop(sockets);
            drop(connection);
            Ok(Started {
                pid,
                report,
                log: log.map(|(read, write)| {
                    drop(write);
                    read
                }),
                notify: notify.map(|(read, write)| {
                    drop(write);
                    read
                }),
                go: go.map(|(read, write)| {
                    drop(read);
                    write
                }),
            })
        }
    }
}

/// `LISTEN_PID=<pid>` and a NUL into `buffer`, without allocating.
fn write_pid(buffer: &mut [u8; 32], pid: u32) {
    const KEY: &[u8] = b"LISTEN_PID=";
    let mut digits = [0_u8; 10];
    let mut value = pid;
    let mut count = 0;
    loop {
        if let Some(digit) = digits.get_mut(count) {
            *digit = b'0' + u8::try_from(value % 10).unwrap_or(0);
        }
        count += 1;
        value /= 10;
        if value == 0 || count == digits.len() {
            break;
        }
    }
    let mut at = 0;
    for &byte in KEY.iter().chain(digits.iter().take(count).rev()) {
        if let Some(slot) = buffer.get_mut(at) {
            *slot = byte;
        }
        at += 1;
    }
    if let Some(slot) = buffer.get_mut(at) {
        *slot = 0;
    }
}

/// A null-terminated array of pointers to `strings`.
fn pointers(strings: &[CString]) -> Vec<*const c_char> {
    strings
        .iter()
        .map(|string| string.as_ptr())
        .chain(std::iter::once(ptr::null()))
        .collect()
}

/// The descriptors the child moves into place: the write ends of init's
/// pipes, and what a `.socket` unit passes.
#[derive(Debug)]
struct Pipes {
    report: RawFd,
    log: Option<RawFd>,
    notify: Option<RawFd>,
    sockets: Vec<RawFd>,
    connection: Option<RawFd>,
    /// The read end of the pipe init writes once the bootstrap channel is
    /// given.
    go: Option<RawFd>,
}

/// The child, from `clone3` to `execve`.
fn child(
    prepared: &Prepared,
    argv: &[*const c_char],
    envp: &mut [*const c_char],
    pipes: &Pipes,
) -> ! {
    let (report, log) = (pipes.report, pipes.log);
    let fail = |step: Step, error: io::Error| -> ! {
        let errno = error.raw_os_error().unwrap_or(libc::EIO);
        let mut bytes = [0_u8; 8];
        let (code, number) = bytes.split_at_mut(4);
        code.copy_from_slice(&(step as u32).to_le_bytes());
        number.copy_from_slice(&errno.to_le_bytes());
        sys::write_once(report, &bytes);
        sys::exit_now(step as i32)
    };

    // Init blocks the signals it reads through a signalfd and ignores the
    // rest; a program starts with neither.
    let _ = sys::unblock_all();
    for signal in 1..=64 {
        sys::disposition(signal, libc::SIG_DFL);
    }
    // Every service leads a session of its own, as under systemd.
    if let Err(error) = sys::setsid()
        && error.raw_os_error() != Some(libc::EPERM)
    {
        fail(Step::Setsid, error);
    }
    let mut terminal = None;
    if let Some((path, force)) = &prepared.tty {
        match sys::open(path, libc::O_RDWR) {
            Ok(fd) => {
                if let Err(error) = sys::take_terminal(fd, *force) {
                    fail(Step::Stdin, error);
                }
                terminal = Some(fd);
            }
            Err(error) => fail(Step::Stdin, error),
        }
    }
    for (target, stream) in (0..).zip(&prepared.streams) {
        let step = if target == 0 {
            Step::Stdin
        } else {
            Step::Stdout
        };
        let from = match stream {
            Stream::Open(path, flags) => match sys::open(path, *flags) {
                Ok(fd) => fd,
                Err(error) => fail(step, error),
            },
            Stream::Tty => match terminal {
                Some(fd) => fd,
                None => fail(step, io::Error::from_raw_os_error(libc::ENOTTY)),
            },
            Stream::Same(fd) => *fd,
            Stream::Log => match log {
                Some(fd) => fd,
                None => fail(step, io::Error::from_raw_os_error(libc::EBADF)),
            },
            Stream::Socket => match pipes.connection {
                Some(fd) => fd,
                None => fail(step, io::Error::from_raw_os_error(libc::ENOTSOCK)),
            },
        };
        if let Err(error) = sys::dup2(from, target) {
            fail(step, error);
        }
    }
    for (target, fd) in (3..).zip(&pipes.sockets) {
        if let Err(error) = sys::dup2(*fd, target) {
            fail(Step::Stdin, error);
        }
    }
    // `LISTEN_PID=` is this process's pid, which only it knows now: written
    // into a buffer on its own stack, which lives until `execve`.
    let mut listen_pid = [0_u8; 32];
    if let Some(slot) = prepared.listen_pid.and_then(|at| envp.get_mut(at)) {
        write_pid(&mut listen_pid, sys::own_pid());
        *slot = listen_pid.as_ptr().cast();
    }
    if let (Some(target), Some(write)) = (prepared.notify, pipes.notify)
        && let Err(error) = sys::dup2(write, target)
    {
        fail(Step::Stdout, error);
    }
    // While still root: the mount namespace and what is mounted in it.
    if let Some(plan) = &prepared.sandbox
        && let Err(error) = sandbox::enter(plan)
    {
        fail(Step::Namespace, error);
    }
    if let Some((path, missing_ok)) = &prepared.directory
        && let Err(error) = sys::chdir(path)
        && !*missing_ok
    {
        fail(Step::Chdir, error);
    }
    if let Some((uid, gid, groups)) = &prepared.user
        && let Err(error) = sys::become_user(*uid, *gid, groups)
    {
        fail(Step::User, error);
    }
    // The last step before the program: wait for init to have given the
    // bootstrap channel, since a give after `execve` is refused. A byte or
    // the pipe's end, either way the program runs.
    if let Some(go) = pipes.go {
        sys::wait_readable(go);
    }
    // The last steps: no new privileges, then the seccomp filter, which
    // must see nothing of init's but the `execve`. A program the filter
    // does not let `execve`, or a failed `execve` whose report the filter
    // does not let be written, ends by the filter's action, as under
    // systemd.
    if let Some(plan) = prepared.sandbox.as_ref().filter(|plan| plan.locks())
        && let Err(error) = sandbox::lock(plan)
    {
        fail(Step::NoNewPrivileges, error);
    }
    if let Some(plan) = prepared.sandbox.as_ref().filter(|plan| plan.confines())
        && let Err(error) = sandbox::confine(plan)
    {
        fail(Step::Seccomp, error);
    }
    let error = sys::execve(&prepared.path, argv, envp);
    fail(Step::Exec, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn command(argv: &[&str], expand: bool) -> Command {
        Command {
            path: argv
                .first()
                .map_or_else(String::new, |word| (*word).to_owned()),
            argv: argv.iter().map(|word| (*word).to_owned()).collect(),
            ignore_failure: false,
            expand_environment: expand,
            privilege: ferrix_svc::exec::Privilege::Unit,
        }
    }

    #[test]
    fn a_whole_word_variable_splits_and_a_braced_one_does_not() {
        let env = environment(&[("OPTS", "-a -b"), ("DIR", "/x y")]);
        let argv = arguments(&command(&["/bin/p", "$OPTS", "--in=${DIR}/z"], true), &env);
        assert_eq!(argv, ["/bin/p", "-a", "-b", "--in=/x y/z"]);
    }

    #[test]
    fn a_colon_prefix_expands_nothing() {
        let env = environment(&[("OPTS", "-a")]);
        let argv = arguments(&command(&["/bin/p", "$OPTS"], false), &env);
        assert_eq!(argv, ["/bin/p", "$OPTS"]);
    }

    #[test]
    fn an_unset_variable_is_empty_and_a_doubled_dollar_is_one() {
        let env = environment(&[]);
        assert_eq!(expand("a$NOPEb", &env), "a");
        assert_eq!(expand("cost $$5", &env), "cost $5");
        assert_eq!(expand("${NOPE}x", &env), "x");
    }

    #[test]
    fn environment_files_skip_comments_and_unquote() {
        let read = environment_file("# c\n; c\nA=1\nB=\"two words\"\n bad name=3\nC='x'\n");
        assert_eq!(
            read,
            [
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "two words".to_owned()),
                ("C".to_owned(), "x".to_owned()),
            ]
        );
    }

    #[test]
    fn listen_pid_is_written_without_allocating() {
        let mut buffer = [0xff_u8; 32];
        write_pid(&mut buffer, 4096);
        assert_eq!(&buffer[..16], b"LISTEN_PID=4096\0");
        write_pid(&mut buffer, 7);
        assert_eq!(&buffer[..13], b"LISTEN_PID=7\0");
    }

    #[test]
    fn a_report_names_its_step() {
        assert_eq!(Step::from_code(203), Some(Step::Exec));
        assert_eq!(Step::from_code(217), Some(Step::User));
        assert_eq!(Step::from_code(1), None);
    }
}
