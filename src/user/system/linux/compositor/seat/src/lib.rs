//! seat0's devices, as the session's compositor gets them
//! (`docs/AUTH.md` §6.2, P2.4 and P2.5).
//!
//! The card, the render node and the `event*` nodes stay `0660 root`. A
//! session runs as its user, so its compositor cannot open them; `sessiond`,
//! which is root and owns seat0, opens them for it and hands the descriptor
//! over with `SCM_RIGHTS`, as seatd and logind do. The channel is one end of
//! a socket pair `sessiond` made before it started the compositor, inherited
//! as descriptor [`FD_VARIABLE`] names: no path, so no other program of the
//! user can ask for the keyboard, which would let it read the lock screen's
//! keystrokes.
//!
//! [`open`] is the one call: through the channel when the process has one,
//! and a plain `open(2)` otherwise -- a compositor started as root, or under
//! a test, opens its devices itself as it always did.
//!
//! The protocol is one line each way. The compositor writes `open <flags>
//! <path>\n`, `<flags>` the `open(2)` flags in hex, and `sessiond` answers
//! `ok\n` with the descriptor beside it, or `err <errno>\n`. Only the
//! devices [`allowed`] names are opened, with only the access mode and
//! `O_NONBLOCK` taken from the flags.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use compositor_socket::{Connection, RecvError};
use compositor_wire::Fd;

pub mod lock;
#[cfg(feature = "settings-prompt")]
pub mod prompt;

/// The variable naming the channel's descriptor.
pub const FD_VARIABLE: &str = "FERRIX_SEAT_FD";

/// The descriptor `sessiond` gives the channel in the compositor.
pub const CHANNEL_FD: i32 = 3;

/// The devices a session may be handed, by the prefix of their path; what
/// follows the prefix is a number.
const DEVICES: [&str; 3] = ["/dev/dri/card", "/dev/dri/renderD", "/dev/input/event"];

/// How long the compositor waits for an answer.
const PATIENCE: Duration = Duration::from_secs(5);

/// The channel, once [`adopt`] has looked for one: `None` before, and
/// `Some(None)` in a process without one.
static CHANNEL: Mutex<Option<Option<Connection>>> = Mutex::new(None);

/// Whether `path` is a device of seat0 a session may be handed: a card, a
/// render node or an input node, by its name and nothing else.
#[must_use]
pub fn allowed(path: &str) -> bool {
    DEVICES.iter().any(|prefix| {
        path.strip_prefix(prefix).is_some_and(|number| {
            !number.is_empty() && number.len() <= 4 && number.bytes().all(|b| b.is_ascii_digit())
        })
    })
}

/// Take the channel `sessiond` left this process, if it left one: mark it
/// close-on-exec, so no program the compositor starts inherits it, and take
/// [`FD_VARIABLE`] out of the environment, so none of them takes another
/// descriptor for it. Gives whether there was one.
///
/// Call it first thing in `main`, before any thread or child: it changes the
/// environment. [`open`] calls it too, for a program that does not.
pub fn adopt() -> bool {
    let Ok(mut channel) = CHANNEL.lock() else {
        return false;
    };
    if let Some(found) = channel.as_ref() {
        return found.is_some();
    }
    let found = take_channel();
    let there = found.is_some();
    *channel = Some(found);
    there
}

/// The channel, from the environment.
fn take_channel() -> Option<Connection> {
    let named = std::env::var(FD_VARIABLE).ok()?;
    #[expect(
        unsafe_code,
        reason = "AUDIT: the environment is changed before the compositor starts a thread; adopt's caller promises it"
    )]
    // SAFETY: `adopt` is called at the start of `main`, before any other
    // thread exists to read the environment at the same time.
    unsafe {
        std::env::remove_var(FD_VARIABLE);
    }
    let fd: i32 = named.parse().ok().filter(|fd| *fd > 2)?;
    if !is_socket(fd) {
        return None;
    }
    set_cloexec(fd);
    #[expect(
        unsafe_code,
        reason = "AUDIT: the descriptor sessiond gave this process, named by the variable just removed; nothing else here owns it"
    )]
    // SAFETY: `fd` is open (fstat said so) and is the channel's, which no
    // other owner in this process holds.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    Connection::new(stream).ok()
}

/// Open `path` with `flags`, as `open(2)` would: through `sessiond` when
/// this process has a channel to it, and itself otherwise. The descriptor is
/// close-on-exec either way.
///
/// # Errors
///
/// What `open(2)` said, here or in `sessiond`, and an error of the channel's
/// own when `sessiond` did not answer.
pub fn open(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let _ = adopt();
    let mut channel = CHANNEL
        .lock()
        .map_err(|_| io::Error::other("the seat channel's lock is poisoned"))?;
    match channel.as_mut().and_then(Option::as_mut) {
        Some(connection) => ask(connection, path, flags),
        None => open_here(path, flags | libc::O_CLOEXEC),
    }
}

/// `open(2)`.
///
/// # Errors
///
/// What it said.
pub fn open_here(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: `path` is NUL-terminated and held across the call.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    #[expect(
        unsafe_code,
        reason = "AUDIT: a descriptor open(2) just made, which nothing else owns"
    )]
    // SAFETY: as the reason says.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Ask `sessiond` for `path`.
fn ask(connection: &mut Connection, path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let path = path
        .to_str()
        .map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
    let request = format!("open {flags:x} {path}\n");
    connection
        .send(request.as_bytes(), &[])
        .map_err(|error| io::Error::other(format!("asking sessiond: {error:?}")))?;
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(end) = connection.bytes().iter().position(|&b| b == b'\n') {
            let line =
                String::from_utf8_lossy(connection.bytes().get(..end).unwrap_or(&[])).into_owned();
            return answer(connection, &line, end + 1);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sessiond did not answer",
            ));
        }
        wait_readable(connection.as_raw_fd(), left);
        match connection.receive() {
            Ok(_) | Err(RecvError::WouldBlock) => {}
            Err(RecvError::Closed) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "sessiond closed the seat channel",
                ));
            }
            Err(error) => return Err(io::Error::other(format!("{error:?}"))),
        }
    }
}

/// Make `sessiond`'s answer `line`, `used` bytes with its newline, into a
/// descriptor or the error it names.
fn answer(connection: &mut Connection, line: &str, used: usize) -> io::Result<OwnedFd> {
    if line == "ok" {
        let Some(Fd(raw)) = connection.fds().first().copied() else {
            connection.consume(used, 0);
            return Err(io::Error::other("sessiond said ok and sent no descriptor"));
        };
        connection.consume(used, 1);
        #[expect(
            unsafe_code,
            reason = "AUDIT: the descriptor came with sessiond's answer, and consume handed it on without closing it"
        )]
        // SAFETY: `raw` arrived with this answer and the connection has let
        // go of it; this is its one owner.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        set_cloexec(fd.as_raw_fd());
        return Ok(fd);
    }
    connection.consume(used, 0);
    let errno = line
        .strip_prefix("err ")
        .and_then(|number| number.parse().ok())
        .unwrap_or(libc::EIO);
    Err(io::Error::from_raw_os_error(errno))
}

/// Wait until `fd` is readable or `left` has gone.
fn wait_readable(fd: i32, left: Duration) {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one pollfd, which lives for the call.
    let _ = unsafe { libc::poll(&raw mut poll, 1, millis.max(1)) };
}

/// Whether `fd` is an open socket.
fn is_socket(fd: i32) -> bool {
    // SAFETY: stat is a plain C structure of integers, for which all-zero
    // is a value.
    let mut stat: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: fstat writes at most one stat, into one that lives for the call.
    let found = unsafe { libc::fstat(fd, &raw mut stat) };
    found == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFSOCK
}

/// Mark `fd` close-on-exec: a descriptor that arrived with `SCM_RIGHTS`, or
/// was inherited, is not.
fn set_cloexec(fd: i32) {
    // SAFETY: fcntl on a descriptor this process holds, with constants.
    let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
}

/// `sessiond`'s half: the answer to one request line, and the descriptor
/// that goes with it.
///
/// The path must be one [`allowed`] names, and is opened with the access
/// mode and `O_NONBLOCK` of the flags asked for, plus `O_CLOEXEC`,
/// `O_NOCTTY` and `O_NOFOLLOW`; an `event*` node that will not open for
/// writing is opened for reading, as a compositor does for itself.
#[must_use]
pub fn serve(line: &str) -> (String, Option<OwnedFd>) {
    let refuse = |errno: i32| (format!("err {errno}\n"), None);
    let Some(rest) = line.strip_prefix("open ") else {
        return refuse(libc::EINVAL);
    };
    let Some((flags, path)) = rest.split_once(' ') else {
        return refuse(libc::EINVAL);
    };
    let Ok(flags) = libc::c_int::from_str_radix(flags, 16) else {
        return refuse(libc::EINVAL);
    };
    if !allowed(path) {
        return refuse(libc::EPERM);
    }
    let Ok(name) = std::ffi::CString::new(path) else {
        return refuse(libc::EINVAL);
    };
    let fixed = libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NOFOLLOW | (flags & libc::O_NONBLOCK);
    let access = flags & libc::O_ACCMODE;
    let mut tried = open_here(&name, fixed | access);
    if tried.is_err() && access == libc::O_RDWR && path.starts_with("/dev/input/") {
        tried = open_here(&name, fixed | libc::O_RDONLY);
    }
    match tried {
        Ok(fd) => ("ok\n".to_owned(), Some(fd)),
        Err(error) => refuse(error.raw_os_error().unwrap_or(libc::EIO)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_seat_devices_by_number_are_allowed() {
        assert!(allowed("/dev/dri/card0"));
        assert!(allowed("/dev/dri/renderD128"));
        assert!(allowed("/dev/input/event12"));
        assert!(!allowed("/dev/dri/card"));
        assert!(!allowed("/dev/input/event1/../../sda"));
        assert!(!allowed("/dev/input/mice"));
        assert!(!allowed("/dev/snd/pcmC0D0p"));
        assert!(!allowed("/etc/shadow"));
        assert!(!allowed("/dev/dri/card0 "));
        assert!(!allowed("/dev/dri/card123456"));
    }

    #[test]
    fn a_request_for_anything_else_is_refused() {
        let (said, fd) = serve("open 2 /etc/passwd");
        assert_eq!(said, format!("err {}\n", libc::EPERM));
        assert!(fd.is_none());
        assert_eq!(serve("close 3").0, format!("err {}\n", libc::EINVAL));
        assert_eq!(
            serve("open zz /dev/dri/card0").0,
            format!("err {}\n", libc::EINVAL)
        );
    }

    #[test]
    fn a_device_that_is_not_there_says_why() {
        let (said, fd) = serve("open 2 /dev/input/event9999");
        assert_eq!(said, format!("err {}\n", libc::ENOENT));
        assert!(fd.is_none());
    }

    /// The next request line on `connection`, waiting for it.
    fn next_line(connection: &mut Connection) -> String {
        loop {
            if let Some(end) = connection.bytes().iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&connection.bytes()[..end]).into_owned();
                connection.consume(end + 1, 0);
                return line;
            }
            wait_readable(connection.as_raw_fd(), Duration::from_secs(5));
            let _ = connection.receive();
        }
    }

    /// Both halves over a socket pair: the request, the refusal's errno,
    /// and an answer with a descriptor that arrives close-on-exec.
    #[test]
    fn the_channel_carries_a_descriptor_and_a_refusal() {
        let (ours, theirs) = UnixStream::pair().expect("a socket pair");
        let mut client = Connection::new(ours).expect("the client's end");
        let server = std::thread::spawn(move || {
            let mut connection = Connection::new(theirs).expect("the server's end");
            for _ in 0..2 {
                let line = next_line(&mut connection);
                let (said, _) = serve(&line);
                // A stand-in for a device: the test cannot open one.
                let file = std::fs::File::open("/dev/null").expect("/dev/null");
                if said == format!("err {}\n", libc::EPERM) {
                    connection.send(said.as_bytes(), &[]).expect("the refusal");
                } else {
                    let fds = [Fd(file.as_raw_fd())];
                    connection.send(b"ok\n", &fds).expect("the answer");
                }
            }
        });
        let refused = ask(&mut client, c"/etc/shadow", libc::O_RDONLY).unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(libc::EPERM));
        let fd = ask(&mut client, c"/dev/dri/card0", libc::O_RDWR).expect("a descriptor");
        // SAFETY: fcntl on a descriptor this test holds.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        server.join().expect("the server");
    }
}
