//! `/sbin/settingsd`: makes the typed changes a Settings app asks for, once
//! `authd` says the person may (`docs/SETTINGS-WRITER.md`).
//!
//! ```text
//! settingsd [--root DIR] [--socket PATH] [--authd PATH] [--control PATH]
//! ```
//!
//! Root, started by init on the first connection to `settings.socket`
//! (`LISTEN_FDS`), or binding `/run/ferrix/settings` itself. Each connection
//! carries one request and is served on a thread of its own: at most
//! [`MAX_CONNECTIONS`] at once, and [`MAX_PER_UID`] for any one uid, so a
//! person who leaves a password prompt open holds up nobody else. It never
//! runs a program.
//!
//! `--root` puts every file it reads and writes under another directory,
//! and the other options name the sockets, for its own tests.

mod atomic;
mod audit;
mod authority;
mod conversation;
mod requests;
#[cfg(test)]
mod tests;
mod wire;

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrix_auth_client::{seqpacket_socket, socklen, unix_address};
use ferrix_settings_proto::{RefusalKind, ToClient};

use crate::audit::say;
use crate::authority::Authd;
use crate::conversation::{Context, Peer};
use crate::requests::{Control, System};

/// Connections served at once.
const MAX_CONNECTIONS: usize = 8;

/// Connections served at once for one uid.
const MAX_PER_UID: usize = 2;

fn main() {
    if let Err(why) = run() {
        say(&format!("settingsd: {why}"));
        std::process::exit(1);
    }
}

/// What the command line asked for.
struct Options {
    root: PathBuf,
    socket: Option<PathBuf>,
    authd: Option<PathBuf>,
    control: Option<PathBuf>,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        root: PathBuf::from("/"),
        socket: None,
        authd: None,
        control: None,
    };
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = |name: &str| -> Result<PathBuf, String> {
            arguments
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match argument.to_str() {
            Some("--root") => options.root = value("--root")?,
            Some("--socket") => options.socket = Some(value("--socket")?),
            Some("--authd") => options.authd = Some(value("--authd")?),
            Some("--control") => options.control = Some(value("--control")?),
            _ => {
                return Err(format!("unknown argument {}", argument.to_string_lossy()));
            }
        }
    }
    Ok(options)
}

/// `path`, an absolute path on the machine, under `root`.
fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

fn run() -> Result<(), String> {
    let options = options()?;
    // SAFETY: `SIG_IGN` is a valid disposition for `SIGPIPE`: a client that
    // goes away is an `EPIPE`, not the end of settingsd.
    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let socket = options
        .socket
        .unwrap_or_else(|| under(&options.root, ferrix_settings_proto::SOCKET));
    let listener =
        listener(&socket).map_err(|e| format!("cannot listen on {}: {e}", socket.display()))?;
    let context = Arc::new(Context {
        system: System {
            root: options.root.clone(),
            live: options.root == Path::new("/"),
            init: Box::new(Control::at(
                options
                    .control
                    .unwrap_or_else(|| PathBuf::from(ferrix_svc_proto::control::SOCKET)),
            )),
        },
        authority: Box::new(Authd::at(
            options
                .authd
                .unwrap_or_else(|| PathBuf::from(ferrix_auth_proto::SOCKET)),
        )),
        passwd: under(&options.root, "/etc/passwd"),
        writing: Mutex::new(()),
    });
    say(&format!("settingsd: listening on {}", socket.display()));
    serve(&listener, &context);
    Ok(())
}

/// Accept connections for ever, each on a thread of its own.
fn serve(listener: &OwnedFd, context: &Arc<Context>) {
    let busy: Arc<Mutex<HashMap<u32, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    loop {
        // SAFETY: a null address asks for none; the call returns a new
        // descriptor or fails.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                say(&format!("settingsd: accept: {error}"));
            }
            continue;
        }
        // SAFETY: `accept4` just returned `fd`, and nothing else owns it.
        let connection = unsafe { OwnedFd::from_raw_fd(fd) };
        let Ok(peer) = peer(connection.as_raw_fd()) else {
            continue;
        };
        if !admit(&busy, peer.uid) {
            let _ = wire::send(
                connection.as_raw_fd(),
                &ToClient::Refused(ferrix_settings_proto::Refusal::new(
                    RefusalKind::Unavailable,
                    "settingsd is busy with other requests; try again",
                )),
            );
            continue;
        }
        let context = Arc::clone(context);
        let busy_there = Arc::clone(&busy);
        let spawned = std::thread::Builder::new()
            .name("conversation".to_owned())
            .spawn(move || {
                let _ = conversation::serve(connection.as_raw_fd(), peer, &context);
                drop(connection);
                leave(&busy_there, peer.uid);
            });
        if spawned.is_err() {
            leave(&busy, peer.uid);
        }
    }
}

/// Count a connection from `uid` in, if there is room for it.
fn admit(busy: &Mutex<HashMap<u32, usize>>, uid: u32) -> bool {
    let Ok(mut busy) = busy.lock() else {
        return false;
    };
    let total: usize = busy.values().sum();
    let own = busy.entry(uid).or_insert(0);
    if total >= MAX_CONNECTIONS || *own >= MAX_PER_UID {
        return false;
    }
    *own += 1;
    true
}

/// Count a connection from `uid` out.
fn leave(busy: &Mutex<HashMap<u32, usize>>, uid: u32) {
    if let Ok(mut busy) = busy.lock()
        && let Some(own) = busy.get_mut(&uid)
    {
        *own = own.saturating_sub(1);
        if *own == 0 {
            let _ = busy.remove(&uid);
        }
    }
}

/// Who connected on `fd`, as `SO_PEERCRED` recorded at `connect`.
fn peer(fd: RawFd) -> io::Result<Peer> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = socklen::<libc::ucred>();
    // SAFETY: `credentials` is valid for writes of `len` bytes, its own
    // size; `len` for a write of its own.
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut len,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Peer {
        pid: credentials.pid,
        uid: credentials.uid,
    })
}

/// The listening socket: init's, when init handed one over, else a new one
/// bound at `path`, which anyone may connect to.
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
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    // SAFETY: `fd` is a bound socket this owns.
    if unsafe { libc::listen(fd.as_raw_fd(), 16) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Descriptor 3, when `LISTEN_PID` is this process and `LISTEN_FDS` is at
/// least one: socket activation, as init hands it over.
fn inherited() -> Option<OwnedFd> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let count: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || count < 1 {
        return None;
    }
    // SAFETY: `fcntl` on a descriptor number reads nothing through memory.
    let _ = unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: init passed descriptor 3 to this process, and nothing here has
    // taken it.
    Some(unsafe { OwnedFd::from_raw_fd(3) })
}
