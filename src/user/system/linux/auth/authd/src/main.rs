//! `/sbin/authd`: the one process on the machine that holds credentials
//! (`docs/AUTH.md` §3.2).
//!
//! ```text
//! authd [--root DIR] [--socket PATH]
//! ```
//!
//! It starts as root, however it is started: from `auth.socket` under init,
//! which hands it the listening socket (`LISTEN_FDS`), or from the desktop's
//! `exec-once` while hyprix is pid 1, when it binds `/run/ferrix/auth`
//! itself. As root it makes the store's directories, imports the image's
//! seeds, and gives the store and the log to the `auth` user. Then it
//! becomes `auth` with `setgroups`, `setresgid` and `setresuid`, before it
//! answers anyone. There is no path on which it answers as root: without an
//! `auth` account it refuses to start.
//!
//! `--root` puts every file it reads under another directory, and
//! `--socket` names where to listen, for its own tests.

mod accounts;
mod audit;
mod engine;
mod local;
mod notify;
mod password;
mod paths;
mod phantom;
mod policy;
mod sabotage;
mod seat;
mod server;
mod sha;
mod sha_crypt;
mod store;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ferrix_auth_client::{seqpacket_socket, socklen, unix_address};

use crate::audit::say;
use crate::engine::Engine;
use crate::password::Hasher;
use crate::paths::Paths;

/// The account `authd` runs as.
const USER: &str = "auth";

/// Set by `SIGTERM` and `SIGINT`.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn main() {
    sabotage::announce();
    match run() {
        Ok(()) => {}
        Err(why) => {
            say(&format!("authd: {why}"));
            std::process::exit(1);
        }
    }
}

/// What the command line asked for.
struct Options {
    root: PathBuf,
    socket: PathBuf,
}

fn options() -> Result<Options, String> {
    let mut root = PathBuf::from("/");
    let mut socket = None;
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = |name: &str| -> Result<OsString, String> {
            arguments
                .next()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match argument.to_str() {
            Some("--root") => root = PathBuf::from(value("--root")?),
            Some("--socket") => socket = Some(PathBuf::from(value("--socket")?)),
            _ => return Err(format!("unknown argument {}", argument.to_string_lossy())),
        }
    }
    let socket =
        socket.unwrap_or_else(|| root.join(ferrix_auth_proto::SOCKET.trim_start_matches('/')));
    Ok(Options { root, socket })
}

fn run() -> Result<(), String> {
    let options = options()?;
    let paths = Paths::under(&options.root);
    ignore_sigpipe_and_catch_stop();
    let listener = listener(&options.socket)
        .map_err(|e| format!("cannot listen on {}: {e}", options.socket.display()))?;
    let mut phantom_key = [0_u8; 32];
    password::random(&mut phantom_key).map_err(|e| format!("getrandom: {e}"))?;
    let mut engine = Engine::new(paths.clone(), Hasher::default(), phantom_key);
    engine.store().prepare().map_err(|e| {
        format!(
            "cannot make the store under {}: {e}",
            paths.root().display()
        )
    })?;
    engine.import_seeds(server::now_ms());
    // SAFETY: `geteuid` reads the caller's ids and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        become_auth(&paths)?;
    }
    say(&format!("authd: listening on {}", options.socket.display()));
    let mut seat = seat::Seat::offer();
    let mut notify = notify::Notify::from_environment();
    server::serve(&listener, &mut engine, seat.as_mut(), &STOP, &mut notify);
    say("authd: stopped");
    Ok(())
}

fn ignore_sigpipe_and_catch_stop() {
    // SAFETY: `SIG_IGN` is a valid disposition for `SIGPIPE`, and `stop` is
    // an `extern "C"` handler that only stores to an atomic.
    unsafe {
        let _ = libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let handler = stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: as above.
        let _ = unsafe { libc::signal(signal, handler) };
    }
}

/// The listening socket: init's, when init handed one over, else a new
/// one bound at `path`.
fn listener(path: &Path) -> io::Result<OwnedFd> {
    if let Some(fd) = inherited() {
        return Ok(fd);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let fd = seqpacket_socket()?;
    let address = unix_address(path)?;
    // SAFETY: `address` is an initialised `sockaddr_un` of the length given.
    let bound = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            socklen::<libc::sockaddr_un>(),
        )
    };
    if bound != 0 {
        return Err(io::Error::last_os_error());
    }
    // Anyone may connect: who they are decides what they may ask (§3.4).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    // SAFETY: `fd` is a bound socket this owns.
    if unsafe { libc::listen(fd.as_raw_fd(), 16) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Descriptor 3, when `LISTEN_PID` is this process and `LISTEN_FDS` is at
/// least one: socket activation, as systemd and init hand it over.
fn inherited() -> Option<OwnedFd> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let count: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || count < 1 {
        return None;
    }
    // SAFETY: `fcntl` on a descriptor number reads nothing through memory;
    // `FD_CLOEXEC` keeps the socket from anything authd might spawn.
    let _ = unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: init passed descriptor 3 to this process, and nothing here has
    // taken it.
    Some(unsafe { OwnedFd::from_raw_fd(3) })
}

/// Give the store and the log to `auth`, and become it.
fn become_auth(paths: &Paths) -> Result<(), String> {
    let account = accounts::by_name(&paths.passwd(), USER).ok_or_else(|| {
        format!("there is no `{USER}` account in /etc/passwd; authd will not answer as root")
    })?;
    let mut owned = vec![
        paths.store(),
        paths.store().join("users"),
        paths.store().join("state"),
    ];
    for dir in [paths.store().join("users"), paths.store().join("state")] {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            owned.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
        }
    }
    if let Some(log_dir) = paths.audit().parent() {
        owned.push(log_dir.to_owned());
    }
    owned.push(paths.audit());
    for path in &owned {
        if path.exists() {
            std::os::unix::fs::chown(path, Some(account.uid), Some(account.gid))
                .map_err(|e| format!("cannot give {} to {USER}: {e}", path.display()))?;
        }
    }
    let groups = [account.gid];
    // SAFETY: `groups` is valid for reads of its one entry.
    let status = unsafe { libc::setgroups(1, groups.as_ptr()) };
    if status != 0 {
        return Err(format!("setgroups: {}", io::Error::last_os_error()));
    }
    // SAFETY: plain ids; the call changes this process's credentials.
    if unsafe { libc::setresgid(account.gid, account.gid, account.gid) } != 0 {
        return Err(format!("setresgid: {}", io::Error::last_os_error()));
    }
    // SAFETY: as above.
    if unsafe { libc::setresuid(account.uid, account.uid, account.uid) } != 0 {
        return Err(format!("setresuid: {}", io::Error::last_os_error()));
    }
    // SAFETY: `geteuid` cannot fail.
    if unsafe { libc::geteuid() } != account.uid {
        return Err("the uid did not change; authd will not answer as root".to_owned());
    }
    say(&format!("authd: running as {USER} (uid {})", account.uid));
    Ok(())
}
