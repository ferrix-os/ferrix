//! `/sbin/getty [--login] TTY`: give a terminal a session and a login or a
//! shell (`docs/INIT.md` §4.4, §8.1).
//!
//! It leads a session of its own, opens the terminal and makes it that
//! session's controlling terminal, taking it from any session that still
//! holds it, and puts it on standard input, output and error.
//!
//! Between the two it hangs the terminal up, as Linux's agetty does: open,
//! take, `vhangup`, close, open again, take again. Every open of the console
//! made before the hangup -- what a program the last login left running
//! still holds -- may no longer read it, so none of them reads the next
//! person's password (`docs/AUTH.md` §1). getty's own first open is one of
//! them, which is why it opens again.
//!
//! With `--login` it then becomes `/bin/login`, which asks who is there and
//! checks it with `authd` (`docs/AUTH.md` §6.2). Without it, it becomes
//! root's shell, as a login shell: that is an automatic login as root, which
//! decision 8 of `docs/AUTH.md` lets an image choose, and which the gate
//! images do, since every one of them types at that shell.
//!
//! `TTY` is a name under `/dev` (`console`, `ttyS0`) or a path. The shell
//! is `$SHELL`, or `/bin/sh`.

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The terminal `TTY` names.
fn terminal(name: &OsStr) -> PathBuf {
    let path = Path::new(name);
    if path.is_absolute() {
        path.to_owned()
    } else {
        Path::new("/dev").join(path)
    }
}

/// Say what went wrong on standard error, which is where init pointed it.
fn fail(what: &str, error: &io::Error) -> ! {
    let mut err = io::stderr().lock();
    let _ = writeln!(err, "getty: {what}: {error}");
    std::process::exit(1)
}

/// What `--login` execs.
const LOGIN: &str = "/bin/login";

/// Open the terminal at `c_path` and make it this session's controlling
/// terminal, taking it from any session that holds it: a shell that outlived
/// its getty's last run must not keep the next one from its terminal.
fn take(c_path: &CString, path: &Path) -> OwnedFd {
    // SAFETY: `c_path` is NUL-terminated; no `O_NOCTTY`, as a terminal that
    // is to be controlling is opened.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        fail(&path.display().to_string(), &io::Error::last_os_error());
    }
    // SAFETY: `open` returned a new descriptor that nothing else owns.
    let terminal = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `TIOCSCTTY` takes an integer.
    if unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCSCTTY, 1) } < 0 {
        fail("TIOCSCTTY", &io::Error::last_os_error());
    }
    terminal
}

fn main() {
    let mut arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let login = arguments.first().is_some_and(|first| first == "--login");
    if login {
        let _ = arguments.remove(0);
    }
    let Some(name) = arguments.first().cloned() else {
        fail("usage", &io::Error::other("getty [--login] TTY"));
    };
    let path = terminal(&name);

    // init makes every service a session leader already, and then `setsid`
    // is `EPERM`; started any other way, it makes getty one.
    // SAFETY: no pointers.
    let _ = unsafe { libc::setsid() };

    let c_path = CString::new(path.as_os_str().as_bytes())
        .unwrap_or_else(|_| fail("the terminal's name", &io::Error::other("holds a NUL")));
    let first = take(&c_path, &path);
    // SAFETY: no pointers.
    if unsafe { libc::vhangup() } < 0 {
        let error = io::Error::last_os_error();
        // A terminal the kernel cannot hang up yet -- a pty -- is used as it
        // is; anything else is said, since then a program left by the last
        // login may still read this one's.
        if error.raw_os_error() != Some(libc::ENOSYS) {
            let _ = writeln!(io::stderr().lock(), "getty: vhangup: {error}");
        }
    }
    drop(first);
    let terminal = take(&c_path, &path);
    for target in 0..3 {
        // SAFETY: no pointers; both descriptors are open.
        if unsafe { libc::dup2(terminal.as_raw_fd(), target) } < 0 {
            fail("dup2", &io::Error::last_os_error());
        }
    }
    drop(terminal);

    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "\nFerrix {} on {}\n", hostname.trim(), path.display());
    drop(out);

    if login {
        let error = Command::new(LOGIN).exec();
        fail(&format!("starting {LOGIN}"), &error);
    }
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
    let base = Path::new(&shell)
        .file_name()
        .map_or_else(|| b"sh".to_vec(), |name| name.as_bytes().to_vec());
    let mut login = b"-".to_vec();
    login.extend_from_slice(&base);
    let error = Command::new(&shell).arg0(OsString::from_vec(login)).exec();
    fail(&format!("starting {}", Path::new(&shell).display()), &error)
}
