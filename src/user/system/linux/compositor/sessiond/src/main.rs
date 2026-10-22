//! `sessiond --user NAME -- PROGRAM [ARGUMENT...]`: seat0's owner
//! (`docs/AUTH.md` §6.2, P2.4).
//!
//! It runs as root, as a unit of `graphical.target`, and starts the
//! graphical session: `PROGRAM`, the compositor, as the account `NAME` --
//! its groups, its gid and its uid, its home as the working directory, and
//! `HOME`, `USER`, `LOGNAME`, `SHELL` and `XDG_RUNTIME_DIR` set, the last a
//! `/run/user/<uid>` it makes `0700` and the account's own. Every program the
//! compositor starts is then that account's too.
//!
//! The devices stay `0660 root`. The compositor inherits one end of a socket
//! pair as descriptor 3, named by `FERRIX_SEAT_FD`, and asks over it for a
//! card, a render node or an input node; this opens it and hands the
//! descriptor back (`compositor_seat`). No path leads to the channel, so no
//! other program of the account can ask for the keyboard.
//!
//! The lock is spoken of on a second socket pair, descriptor 4
//! (`compositor_seat::lock`), and relayed to and from `authd` over
//! `ferrix.auth.seat` (§3.7, P2.5): this arms each lock the compositor takes
//! for the session's uid, and passes on only a grant for that uid and that
//! lock ([`relay`]). Init gives the seat channel's bootstrap to this
//! process, the unit's own; the compositor cannot take it.
//!
//! With `--session-bus DAEMON CONFIG` the session has a D-Bus session bus,
//! started as the account beside the compositor, in the same scope and
//! under the same rules, and ending with it ([`bus`]; `docs/GNOME.md`,
//! stage G1).
//!
//! The session ends with its compositor (§6.4): this exits with the
//! compositor's status, and init's stop of the unit ends whatever the
//! session left running. An image that logs a named user in at once
//! (decision 8) is the one case built; a greeter would choose the account
//! instead of `--user`.
//!
//! A GPU booted without a monitor publishes a render node and no card0, and
//! init cannot wait for one node or the other (`docs/INIT.md` §4.3): with
//! `--headless WxH` this waits for card0 itself, and when none came within
//! `--display-wait` seconds but a render node is there, starts the
//! compositor with `--headless WxH`. The wait outlasts nvrm, which
//! publishes card0 half a minute into boot.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, chown};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use compositor_seat::lock::{LOCK_FD, LOCK_FD_VARIABLE, MAX_LINE};
use compositor_socket::{Connection, RecvError};
use compositor_wire::Fd;
use ferrix_auth_proto::Record;

use crate::authd::{Authd, Heard};
use crate::relay::{Act, Relay};

mod authd;
mod bus;
mod gate;
mod relay;
mod scope;

/// How often a seat channel that went is asked for again.
const ASK_AGAIN: Duration = Duration::from_secs(5);

const USAGE: &str = "usage: sessiond --user NAME [--headless WxH [--display-wait SECONDS]]\n\
    [--session-bus DAEMON CONFIG [--session-env NAME=VALUE]...] -- PROGRAM [ARGUMENT...]\n\
    \n\
    Start PROGRAM, the session's compositor, as the account NAME, and hand it\n\
    seat0's card, render node and input nodes when it asks.\n\
    \n\
    With --headless, wait for /dev/dri/card0; when none appears within\n\
    SECONDS (60) but a render node is there, the GPU has no display (no\n\
    monitor at boot), and PROGRAM is started with `--headless WxH`.\n\
    \n\
    With --session-bus, start DAEMON (dbus-daemon) as the account with\n\
    CONFIG, listening on $XDG_RUNTIME_DIR/bus, in the session's scope.\n";

/// Where the screen is published, and the render nodes' directory.
const CARD: &str = "/dev/dri/card0";
const DRI: &str = "/dev/dri";

/// What to do when no screen is published: the size of the headless output
/// and how long to wait for card0 first.
#[derive(Debug, PartialEq, Eq)]
struct Headless {
    size: String,
    wait: Duration,
}

/// The `--headless WxH` and `--display-wait SECONDS` words, taken out of
/// `args`; the rest is for [`parse`].
fn split_headless(args: &[String]) -> Result<(Option<Headless>, Vec<String>), String> {
    let (mut size, mut wait) = (None, 60);
    let mut rest = Vec::new();
    let mut words = args.iter();
    let mut options = true;
    while let Some(word) = words.next() {
        if options && (word == "--headless" || word == "--display-wait") {
            let value = words.next().ok_or_else(|| USAGE.to_owned())?;
            if word == "--headless" {
                size = Some(value.clone());
            } else {
                wait = value.parse::<u64>().map_err(|_| USAGE.to_owned())?;
            }
            continue;
        }
        options &= word != "--";
        rest.push(word.clone());
    }
    let headless = size.map(|size| Headless {
        size,
        wait: Duration::from_secs(wait),
    });
    Ok((headless, rest))
}

/// Whether `names` (the entries of /dev/dri) has a render node.
fn has_render_node(names: &[String]) -> bool {
    names.iter().any(|name| name.starts_with("renderD"))
}

/// Whether to start the compositor headless, given how long card0 has been
/// awaited: no card0 after the wait, and a render node, so a GPU driver is
/// up and has no display. Without a render node the wait goes on.
fn start_headless(card0: bool, names: &[String], waited: Duration, wait: Duration) -> bool {
    !card0 && waited >= wait && has_render_node(names)
}

/// Wait for card0 or the decision to go headless; the program with
/// `--headless WxH` after the compositor's name in the second case.
fn choose_display(headless: &Headless, program: Vec<String>) -> Vec<String> {
    let began = Instant::now();
    loop {
        let card0 = std::path::Path::new(CARD).exists();
        let names: Vec<String> = std::fs::read_dir(DRI)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        if card0 {
            return program;
        }
        if start_headless(card0, &names, began.elapsed(), headless.wait) {
            say(&format!(
                "no card0 after {} s, a render node: headless {}",
                headless.wait.as_secs(),
                headless.size
            ));
            let mut program = program;
            program.insert(1, headless.size.clone());
            program.insert(1, "--headless".to_owned());
            // The driver publishes its nodes together; let it finish.
            std::thread::sleep(Duration::from_secs(1));
            return program;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Where each account's runtime directory goes.
const RUNTIME: &str = "/run/user";

/// What a home starts with: each file copied in once, when it is not there.
const SKEL: &str = "/etc/skel";

/// What the image puts under `/home` itself -- the host paths a carried
/// configuration names -- which the home disk's mount hides: copied back
/// in place at every start.
const HOME_COPY: &str = "/usr/share/ferrix/home";

/// The deepest a seeded tree goes, against a loop of links.
const DEEPEST: usize = 16;

/// An account, from `/etc/passwd` and `/etc/group`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Account {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
    /// Its supplementary groups: every group naming it, and its own.
    groups: Vec<u32>,
}

/// Find `name` in the text of `/etc/passwd` and `/etc/group`.
fn account(name: &str, passwd: &str, group: &str) -> Option<Account> {
    let (uid, gid, home, shell) = passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        let [user, _, uid, gid, _, home, shell, ..] = fields.as_slice() else {
            return None;
        };
        (*user == name).then(|| {
            Some((
                uid.parse::<u32>().ok()?,
                gid.parse::<u32>().ok()?,
                (*home).to_owned(),
                (*shell).to_owned(),
            ))
        })?
    })?;
    let mut groups = vec![gid];
    for line in group.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [_, _, id, members, ..] = fields.as_slice() else {
            continue;
        };
        if members.split(',').any(|member| member == name)
            && let Ok(id) = id.parse::<u32>()
            && !groups.contains(&id)
        {
            groups.push(id);
        }
    }
    Some(Account {
        name: name.to_owned(),
        uid,
        gid,
        home,
        shell,
        groups,
    })
}

/// The command line: the account and the program with its arguments.
fn parse(args: &[String]) -> Option<(String, Vec<String>)> {
    let [flag, user, dashes, program @ ..] = args else {
        return None;
    };
    (flag == "--user" && dashes == "--" && !program.is_empty() && !user.is_empty())
        .then(|| (user.clone(), program.to_vec()))
}

fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "sessiond: {line}");
}

/// `/run/user/<uid>`, made if it is not there, `0700` and the account's.
fn runtime_dir(account: &Account) -> std::io::Result<String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(RUNTIME)?;
    let path = format!("{RUNTIME}/{}", account.uid);
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    chown(&path, Some(account.uid), Some(account.gid))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// Copy the tree at `from` into `into`: a file that is not there, or every
/// file when `refresh`; directories made as needed. With `owner`, everything
/// made is given to it, so a home's seeds are the account's. Gives how many
/// files were copied. What cannot be copied is said and passed over: a home
/// missing a default is still a session.
fn seed(
    from: &std::path::Path,
    into: &std::path::Path,
    owner: Option<(u32, u32)>,
    refresh: bool,
    depth: usize,
) -> usize {
    let Ok(entries) = std::fs::read_dir(from) else {
        return 0;
    };
    if depth > DEEPEST {
        return 0;
    }
    let give = |path: &std::path::Path| {
        if let Some((uid, gid)) = owner {
            let _ = std::os::unix::fs::lchown(path, Some(uid), Some(gid));
        }
    };
    let mut copied = 0;
    for entry in entries.flatten() {
        let source = entry.path();
        let target = into.join(entry.file_name());
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !target.exists() {
                if let Err(error) = std::fs::create_dir_all(&target) {
                    say(&format!("{}: {error}", target.display()));
                    continue;
                }
                give(&target);
            }
            copied += seed(&source, &target, owner, refresh, depth + 1);
        } else if kind.is_symlink() {
            if std::fs::symlink_metadata(&target).is_err()
                && let Ok(link) = std::fs::read_link(&source)
                && std::os::unix::fs::symlink(&link, &target).is_ok()
            {
                give(&target);
                copied += 1;
            }
        } else if refresh || std::fs::symlink_metadata(&target).is_err() {
            match std::fs::copy(&source, &target) {
                Ok(_) => {
                    give(&target);
                    copied += 1;
                }
                Err(error) => say(&format!("{}: {error}", target.display())),
            }
        }
    }
    copied
}

fn run(args: &[String]) -> Result<i32, String> {
    let (headless, args) = split_headless(args)?;
    let (bus, args) = bus::split(&args)?;
    let (name, mut program) = parse(&args).ok_or_else(|| USAGE.to_owned())?;
    if let Some(headless) = &headless {
        program = choose_display(headless, program);
    }
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let account =
        account(&name, &passwd, &group).ok_or_else(|| format!("{name} is not in /etc/passwd"))?;
    if account.uid == 0 {
        return Err(format!(
            "{name} is root, whose session needs no sessiond: start the compositor itself"
        ));
    }
    let runtime = runtime_dir(&account).map_err(|error| format!("{RUNTIME}: {error}"))?;
    let seeded = seed(
        std::path::Path::new(SKEL),
        std::path::Path::new(&account.home),
        Some((account.uid, account.gid)),
        false,
        0,
    );
    if seeded > 0 {
        say(&format!(
            "{seeded} file(s) from {SKEL} put in {}",
            account.home
        ));
    }
    let _ = seed(
        std::path::Path::new(HOME_COPY),
        std::path::Path::new("/home"),
        None,
        true,
        0,
    );
    // Before the compositor starts: init may have to start authd, and the
    // lock is spoken of as soon as the compositor takes one.
    let authd = Authd::open();
    let (ours, theirs) = UnixStream::pair().map_err(|error| format!("socketpair: {error}"))?;
    let (lock_ours, lock_theirs) =
        UnixStream::pair().map_err(|error| format!("socketpair: {error}"))?;
    let Started {
        child,
        bus: mut bus_child,
        scope,
    } = start(
        &account,
        &program,
        &runtime,
        (theirs, lock_theirs),
        bus.as_ref(),
    )?;
    say(&format!(
        "seat0's session for {} (uid {}): {}, pid {}, in user-{}.slice/{scope}",
        account.name,
        account.uid,
        program.join(" "),
        child.id(),
        account.uid
    ));
    let mut child = child;
    // Whatever way `serve` ends -- the compositor gone, or a failure of
    // sessiond's own -- the compositor is killed and the session's scope is
    // stopped before sessiond exits: once the compositor is in the scope, the
    // init's stop of this unit no longer reaches it (the certification
    // consultant's Q1).
    let served = serve(ours, lock_ours, authd, account.uid, &mut child);
    if served.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let ended = scope::end(account.uid, &scope);
    // The bus was in the scope, and went with it: reaped here, and said if
    // it had not gone, which the scope's end would then have said too.
    if let Some(bus) = bus_child.as_mut()
        && !bus::reap(bus, Duration::from_secs(5))
    {
        say("the session's bus was still running after the session's end");
    }
    ended?;
    served
}

/// What [`start`] started: the compositor, the session's bus when it was
/// asked for and its fork worked, and the scope's unit.
struct Started {
    child: std::process::Child,
    bus: Option<std::process::Child>,
    scope: String,
}

/// A gated child: its spawn running on a thread of its own, since `spawn`
/// returns only once the child has exec'd, and it execs only once the scope
/// is answered; the pipe its pid comes on; the pipe its answer goes down.
struct Gated {
    spawning: std::thread::JoinHandle<std::io::Result<std::process::Child>>,
    told: std::fs::File,
    hear: std::fs::File,
}

/// Spawn `command`, as the account `ids` with `moves` (descriptor, place)
/// put in place, gated on the scope: forked now, exec'd once [`gate::answer`]
/// says so. `keep` is held until the spawn returns, so a descriptor a move
/// names stays open until the fork has its copy.
fn gated(
    mut command: Command,
    moves: [(i32, i32); 2],
    ids: (u32, u32, Vec<u32>),
    keep: Vec<OwnedFd>,
) -> Result<Gated, String> {
    let (told, tell) = gate::pipe().map_err(|error| format!("pipe: {error}"))?;
    let (heard, hear) = gate::pipe().map_err(|error| format!("pipe: {error}"))?;
    let gates = (tell.as_raw_fd(), heard.as_raw_fd());
    #[expect(
        unsafe_code,
        reason = "AUDIT: pre_exec runs between fork and exec; the closure makes only async-signal-safe calls (getpid, write, read, fcntl, dup2, close, setgroups, setgid, setuid, getuid)"
    )]
    // SAFETY: the closure allocates nothing and calls only the system calls
    // the reason names, each on values captured before the fork.
    unsafe {
        let _ = command.pre_exec(move || {
            gate::wait_for_scope(gates)?;
            become_account(moves, &ids)
        });
    }
    let spawning = std::thread::spawn(move || {
        let spawned = command.spawn();
        // The child's copies are its own now: with these gone, a child that
        // died before it wrote is an end of file to the reader, not a wait
        // for ever.
        drop(tell);
        drop(heard);
        drop(keep);
        spawned
    });
    Ok(Gated {
        spawning,
        told,
        hear,
    })
}

/// Start the compositor as `account`, with `theirs` as its descriptor 3 and
/// `lock` as its descriptor 4, in its session's scope -- and with `bus`, the
/// session's bus beside it in the same scope ([`bus`]).
///
/// Both are in `session-<n>.scope` before either execs, so nothing either
/// starts can be outside it (the certification consultant's E1). Each is
/// forked and then waits, between fork and exec, for an answer down a pipe
/// after writing its pid down another ([`gate`]); one call to the init makes
/// the scope with both pids, and anything but yes stops both execs. A scope
/// the init refuses is not a session (§6.4): none is started, so the
/// session's end may rely on the scope.
///
/// The bus is forked first and let exec first. The compositor's two
/// socket pairs exist by then (`run` makes them), close-on-exec like every
/// descriptor `sessiond` holds, so the bus's exec drops them; only its
/// address pipe is moved to a place that survives it. Its address is read
/// before the compositor is let exec, for at most
/// [`bus::ADDRESS_PATIENCE`]. A bus whose fork fails, or that prints no
/// address, is said, and the session goes on without one.
fn start(
    account: &Account,
    program: &[String],
    runtime: &str,
    (theirs, lock): (UnixStream, UnixStream),
    bus: Option<&bus::Bus>,
) -> Result<Started, String> {
    let (first, rest) = program.split_first().ok_or_else(|| USAGE.to_owned())?;
    let ids = (account.uid, account.gid, account.groups.clone());
    let none = (-1, -1);
    let bus_side = match bus {
        Some(bus) => {
            let (read, write) = gate::pipe().map_err(|error| format!("pipe: {error}"))?;
            let write = OwnedFd::from(write);
            let mut command = Command::new(&bus.daemon);
            let _ = command
                .args(bus::arguments(bus, runtime))
                .current_dir(&account.home)
                .env_clear()
                .env("HOME", &account.home)
                .env("USER", &account.name)
                .env("LOGNAME", &account.name)
                .env("SHELL", &account.shell)
                .env("PATH", "/usr/bin:/bin")
                .env("XDG_RUNTIME_DIR", runtime)
                .envs(bus.env.iter().map(|(name, value)| (name, value)));
            let moves = [(write.as_raw_fd(), bus::ADDRESS_FD), none];
            let gated = gated(command, moves, ids.clone(), vec![write])?;
            let pid = gate::read_pid(&gated.told);
            Some((gated, pid, read))
        }
        None => None,
    };
    let channel = OwnedFd::from(theirs);
    let lock = OwnedFd::from(lock);
    let mut command = Command::new(first);
    let _ = command
        .args(rest)
        .current_dir(&account.home)
        .env("HOME", &account.home)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .env("SHELL", &account.shell)
        .env("XDG_RUNTIME_DIR", runtime)
        .env(
            compositor_seat::FD_VARIABLE,
            compositor_seat::CHANNEL_FD.to_string(),
        )
        .env(LOCK_FD_VARIABLE, LOCK_FD.to_string());
    let moves = [
        (channel.as_raw_fd(), compositor_seat::CHANNEL_FD),
        (lock.as_raw_fd(), LOCK_FD),
    ];
    let compositor = gated(command, moves, ids, vec![channel, lock])?;
    let compositor_pid = gate::read_pid(&compositor.told);
    let bus_pid = bus_side.as_ref().and_then(|(_, pid, _)| *pid);
    let pids: Vec<u32> = compositor_pid
        .iter()
        .chain(bus_pid.iter())
        .copied()
        .collect();
    let scope = match compositor_pid {
        Some(_) => scope::join(account.uid, &pids),
        None => Err("the compositor never said its pid".to_owned()),
    };
    // The bus first, and its address, then the compositor.
    let mut bus_child = None;
    if let Some((gated, pid, read)) = bus_side {
        gate::answer(&gated.hear, scope.is_ok());
        if scope.is_ok() && pid.is_some() {
            match bus::address(&read, bus::ADDRESS_PATIENCE) {
                Some(address) => say(&format!("the session's bus is up: {address}")),
                None => say(&format!(
                    "the session's bus printed no address within {} s; the session goes on                      without waiting for it, and nothing starts it again",
                    bus::ADDRESS_PATIENCE.as_secs()
                )),
            }
        }
        drop(read);
        match gated.spawning.join() {
            Ok(Ok(child)) => bus_child = Some(child),
            Ok(Err(error)) => say(&format!("the session's bus did not start: {error}")),
            Err(_) => say("the session's bus's thread panicked"),
        }
    }
    gate::answer(&compositor.hear, scope.is_ok());
    let spawned = compositor
        .spawning
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("the spawning thread panicked")));
    let stop = |child: Option<std::process::Child>| {
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    };
    match (spawned, scope) {
        (Ok(child), Ok(scope)) => Ok(Started {
            child,
            bus: bus_child,
            scope,
        }),
        (Ok(child), Err(why)) => {
            stop(Some(child));
            stop(bus_child);
            Err(format!(
                "no session scope: {why}; the session was not started"
            ))
        }
        (Err(_), Err(why)) => {
            stop(bus_child);
            Err(format!(
                "no session scope: {why}; the session was not started"
            ))
        }
        (Err(error), Ok(scope)) => {
            stop(bus_child);
            let _ = scope::end(account.uid, &scope);
            Err(format!("starting {first}: {error}"))
        }
    }
}

/// In the child, before `exec`: each of `moves`' descriptors put at its
/// place, not close-on-exec (a place of -1 is no move), then the account's
/// groups, gid and uid, in that order, checked.
fn become_account(
    moves: [(i32, i32); 2],
    (uid, gid, groups): &(u32, u32, Vec<u32>),
) -> std::io::Result<()> {
    let check = |result: libc::c_int| {
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(result)
        }
    };
    // Each first above 4, so that putting one at 3 or 4 cannot close the
    // other where it was; `dup2` then gives copies without close-on-exec.
    let mut high = [-1; 2];
    for (slot, (from, to)) in high.iter_mut().zip(moves) {
        if to >= 0 {
            // SAFETY: fcntl on a descriptor this process holds, with constants.
            *slot = check(unsafe { libc::fcntl(from, libc::F_DUPFD, 5) })?;
        }
    }
    for (from, (_, to)) in high.into_iter().zip(moves) {
        if to >= 0 {
            // SAFETY: dup2 and close of descriptors this process holds.
            let _ = check(unsafe { libc::dup2(from, to) })?;
            // SAFETY: as above.
            let _ = check(unsafe { libc::close(from) })?;
        }
    }
    // SAFETY: the pointer and length are the vector's own, read only.
    let _ = check(unsafe { libc::setgroups(groups.len(), groups.as_ptr()) })?;
    // SAFETY: setgid and setuid take plain integers.
    let _ = check(unsafe { libc::setgid(*gid) })?;
    // SAFETY: as above.
    let _ = check(unsafe { libc::setuid(*uid) })?;
    // SAFETY: getuid cannot fail.
    if unsafe { libc::getuid() } != *uid {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(())
}

/// The compositor's two channels and `authd`'s, until the compositor exits:
/// its status.
fn serve(
    ours: UnixStream,
    lock: UnixStream,
    mut authd: Option<Authd>,
    uid: u32,
    child: &mut std::process::Child,
) -> Result<i32, String> {
    let mut connection = Some(Connection::new(ours).map_err(|error| format!("{error}"))?);
    lock.set_nonblocking(true)
        .map_err(|error| format!("the lock channel: {error}"))?;
    let mut lock = Some(lock);
    let mut said = Vec::new();
    let mut relay = Relay::new(uid);
    let mut asked = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| format!("{error}"))? {
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            say(&format!(
                "the session ended: its compositor exited with {code}"
            ));
            let acts = relay.compositor_gone();
            let _ = act(&acts, authd.as_ref(), lock.as_ref());
            return Ok(code);
        }
        if let Some(seat) = authd.as_mut()
            && !seat.is_open()
            && asked.elapsed() >= ASK_AGAIN
        {
            asked = Instant::now();
            if let Err(error) = seat.ask() {
                say(&format!(
                    "asking for the seat channel again failed: {error:?}"
                ));
            }
        }
        let fds = [
            connection.as_ref().map(Connection::as_raw_fd),
            lock.as_ref().map(AsRawFd::as_raw_fd),
            authd.as_ref().map(|seat| seat.fd().as_raw_fd()),
        ];
        let ready = wait_any(fds, Duration::from_millis(200));
        if ready[0]
            && let Some(open) = connection.as_mut()
            && !answer_devices(open)
        {
            connection = None;
        }
        let mut acts = Vec::new();
        if ready[1]
            && let Some(stream) = lock.as_mut()
        {
            match read_lines(stream, &mut said) {
                Some(lines) => {
                    for line in lines {
                        acts.extend(relay.compositor_said(&line));
                    }
                }
                None => lock = None,
            }
        }
        if ready[2]
            && let Some(seat) = authd.as_mut()
        {
            for heard in seat.drain() {
                acts.extend(match heard {
                    Heard::Ready => {
                        say("ferrix.auth.seat is up: the session's locks are granted by authd");
                        relay.seat_ready()
                    }
                    Heard::Gone => {
                        asked = Instant::now();
                        relay.seat_gone()
                    }
                    Heard::Grant { uid, epoch } => relay.grant(uid, epoch),
                });
            }
        }
        if let Some(why) = act(&acts, authd.as_ref(), lock.as_ref()) {
            say(&format!("ending the session: {why}"));
            let _ = child.kill();
            let _ = child.wait();
            return Ok(1);
        }
    }
}

/// Do what the relay asked; the reason to end the session, if it asked that.
fn act(acts: &[Act], authd: Option<&Authd>, lock: Option<&UnixStream>) -> Option<String> {
    for step in acts {
        match step {
            Act::Arm { uid, epoch } => {
                let sent = authd.is_some_and(|seat| {
                    seat.send(&Record::Arm {
                        uid: *uid,
                        epoch: *epoch,
                    })
                });
                if !sent {
                    say(&format!("lock {epoch} could not be armed at authd"));
                }
            }
            Act::Disarm(epoch) => {
                let _ = authd.is_some_and(|seat| seat.send(&Record::Disarm { epoch: *epoch }));
            }
            Act::Tell(what) => {
                if let Some(mut stream) = lock
                    && let Err(error) = stream.write_all(what.line().as_bytes())
                {
                    say(&format!("telling the compositor failed: {error}"));
                }
            }
            Act::End(why) => return Some(why.clone()),
        }
    }
    None
}

/// Read what the compositor wrote on the lock channel: its whole lines, and
/// `None` once it has closed it. A line longer than any it may send ends it
/// too, as a line it may not send does.
fn read_lines(stream: &mut UnixStream, said: &mut Vec<u8>) -> Option<Vec<String>> {
    let mut chunk = [0_u8; 256];
    loop {
        match std::io::Read::read(stream, &mut chunk) {
            Ok(0) => return None,
            Ok(got) => said.extend_from_slice(chunk.get(..got).unwrap_or_default()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return None,
        }
    }
    let mut lines = Vec::new();
    while let Some(end) = said.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = said.drain(..=end).collect();
        lines.push(String::from_utf8_lossy(line.get(..end).unwrap_or_default()).into_owned());
    }
    if said.len() >= MAX_LINE {
        // Not a line the protocol has: the relay ends the session on it.
        lines.push(String::from_utf8_lossy(said).into_owned());
        said.clear();
    }
    Some(lines)
}

/// Answer the device requests waiting on the compositor's channel; false
/// once it has gone.
fn answer_devices(open: &mut Connection) -> bool {
    match open.receive() {
        Ok(_) | Err(RecvError::WouldBlock) => {}
        Err(RecvError::Closed) => return false,
        Err(error) => {
            say(&format!("the seat channel failed: {error:?}"));
            return false;
        }
    }
    while let Some(end) = open.bytes().iter().position(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(open.bytes().get(..end).unwrap_or(&[])).into_owned();
        open.consume(end + 1, 0);
        let (said, fd) = compositor_seat::serve(&line);
        if said != "ok\n" {
            say(&format!("refused `{line}`: {}", said.trim_end()));
        }
        let fds: Vec<Fd> = fd.iter().map(|fd| Fd(fd.as_raw_fd())).collect();
        if let Err(error) = open.send(said.as_bytes(), &fds) {
            say(&format!("answering the compositor failed: {error:?}"));
        }
    }
    true
}

/// Wait until any of `fds` is readable, or `left` has gone: which are.
fn wait_any(fds: [Option<i32>; 3], left: Duration) -> [bool; 3] {
    let mut polled = fds.map(|fd| libc::pollfd {
        fd: fd.unwrap_or(-1),
        events: libc::POLLIN,
        revents: 0,
    });
    let millis = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
    let count = libc::nfds_t::try_from(polled.len()).unwrap_or(0);
    // SAFETY: `polled` is valid for reads and writes of `count` entries; a
    // negative descriptor is ignored by poll.
    let _ = unsafe { libc::poll(polled.as_mut_ptr(), count, millis) };
    polled.map(|entry| entry.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "sessiond: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str =
        "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/home/ferrix:/bin/zsh\n";
    const GROUP: &str = "root:x:0:\nferrix:x:1000:\naudio:x:29:ferrix,other\nvideo:x:44:other\n";

    #[test]
    fn headless_options_are_split_off_before_the_program() {
        let words = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        let (headless, rest) = split_headless(&words(
            "--user ferrix --headless 1920x1200 --display-wait 5 -- /bin/hyprix --headless",
        ))
        .unwrap();
        assert_eq!(
            headless,
            Some(Headless {
                size: "1920x1200".to_owned(),
                wait: Duration::from_secs(5)
            })
        );
        assert_eq!(rest, words("--user ferrix -- /bin/hyprix --headless"));
        let (none, rest) = split_headless(&words("--user ferrix -- /bin/hyprix")).unwrap();
        assert_eq!(none, None);
        assert_eq!(rest.len(), 4);
        assert!(split_headless(&words("--user ferrix --headless")).is_err());
    }

    /// The nvrm case: card0 comes 30 s in, and nothing falls back before the
    /// wait is over, nor without a render node.
    #[test]
    fn headless_only_after_the_wait_with_a_render_node() {
        let render = vec!["renderD128".to_owned()];
        let wait = Duration::from_secs(60);
        let at = Duration::from_secs;
        assert!(!start_headless(false, &render, at(30), wait));
        assert!(!start_headless(true, &render, at(90), wait));
        assert!(!start_headless(false, &[], at(90), wait));
        assert!(start_headless(false, &render, at(60), wait));
    }

    #[test]
    fn an_account_has_its_ids_home_shell_and_groups() {
        assert_eq!(
            account("ferrix", PASSWD, GROUP),
            Some(Account {
                name: "ferrix".to_owned(),
                uid: 1000,
                gid: 1000,
                home: "/home/ferrix".to_owned(),
                shell: "/bin/zsh".to_owned(),
                groups: vec![1000, 29],
            })
        );
        assert_eq!(account("nobody", PASSWD, GROUP), None);
    }

    /// A seed is copied where nothing is, and an edited file is left.
    #[test]
    fn seeds_fill_gaps_and_keep_edits() {
        let root = std::env::temp_dir().join(format!("sessiond-seed-{}", std::process::id()));
        let skel = root.join("skel");
        let home = root.join("home");
        std::fs::create_dir_all(skel.join(".config/waybar")).unwrap();
        std::fs::write(skel.join(".config/waybar/config.jsonc"), "seed").unwrap();
        std::fs::write(skel.join(".zshrc"), "seed").unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".zshrc"), "edited").unwrap();
        assert_eq!(seed(&skel, &home, None, false, 0), 1);
        assert_eq!(
            std::fs::read_to_string(home.join(".config/waybar/config.jsonc")).unwrap(),
            "seed"
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".zshrc")).unwrap(),
            "edited"
        );
        assert_eq!(
            seed(&skel, &home, None, false, 0),
            0,
            "the second start copies nothing"
        );
        assert_eq!(
            seed(&skel, &home, None, true, 0),
            2,
            "a refresh copies everything"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_command_line_names_the_account_and_the_program() {
        let words = |text: &str| text.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            parse(&words(
                "--user ferrix -- /bin/hyprix --config /etc/hyprland.conf"
            )),
            Some((
                "ferrix".to_owned(),
                words("/bin/hyprix --config /etc/hyprland.conf")
            ))
        );
        assert_eq!(parse(&words("--user ferrix --")), None);
        assert_eq!(parse(&words("ferrix -- /bin/hyprix")), None);
    }
}
