//! The terminal requests a program makes of the console.
//!
//! # What `sh -i` asks, measured
//!
//! With `strace`, on the host, against the same static busybox. On a
//! terminal, the shell asks `TCGETS` on descriptor 0 and, when that succeeds,
//! puts the terminal into raw mode with `TCSETS`, asks `TIOCGWINSZ` for the
//! width, and reads one byte at a time, doing its own echo and line editing.
//! For job control it asks `TIOCGPGRP` and requires the answer to be its own
//! group, then makes each job's group the foreground with `TIOCSPGRP` and
//! takes the terminal back afterwards.
//!
//! These used to be refused, because the console echoed and edited every line
//! itself and a shell doing the same doubled every character. They answer now
//! because `crate::fs::terminal` honours `ICANON` and `ECHO`: raw mode really
//! is raw.
//!
//! # `struct termios2`
//!
//! A newer glibc's `tcgetattr` and `tcsetattr` ask `TCGETS2` and `TCSETS2`
//! instead, whose structure adds the input and output speeds as numbers, and
//! Ubuntu's static busybox is linked against one. Refusing them made `isatty`
//! false, so its `stty`, `tty`, `login` and `less` all decided there was no
//! terminal. They take the same settings as their older twins; the speeds are
//! the ones `c_cflag` names, and a speed given as a number (`BOTHER`) becomes
//! the code for it -- see `Termios::with_speeds`.
//!
//! # The controlling terminal
//!
//! Linux gives a session leader with no controlling terminal the first
//! terminal it opens. The first process here does not open the console -- the
//! kernel hands it descriptors 0, 1 and 2 -- so the console is given instead
//! to a session leader the first time it asks about job control, provided no
//! live session holds it already. There is one terminal, so a process's
//! controlling terminal is the console exactly when the console's session is
//! the process's.
//!
//! Every request that needs a controlling terminal answers `ENOTTY` to a
//! process whose session does not hold the console, as Linux does.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    FIONREAD, TCFLSH, TCGETS, TCGETS2, TCIFLUSH, TCIOFLUSH, TCION, TCOFLUSH, TCSETS, TCSETS2,
    TCSETSF, TCSETSF2, TCSETSW, TCSETSW2, TCXONC, TERMIOS_BYTES, TERMIOS2_BYTES, TIOCGPGRP,
    TIOCGPTN, TIOCGSID, TIOCGWINSZ, TIOCNOTTY, TIOCSCTTY, TIOCSPGRP, TIOCSPTLCK, TIOCSWINSZ,
};
use ferrix_vfs::OpenFile;

use crate::fs::terminal::{self, Terminal, Termios, Winsize};
use crate::syscall::pidns;
use crate::syscall::process::Process;
use crate::syscall::{registry, uaccess};

/// `ioctl` on a descriptor that names the console.
///
/// The caller, `crate::syscall::fd::sys_ioctl`, has resolved the descriptor
/// and checked that it is the console. Every request not listed is `ENOTTY`,
/// which is what Linux's terminal layer answers for one it does not know.
pub(crate) fn ioctl(
    process: &Process,
    file: &Arc<OpenFile>,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    let _ = file;
    match request {
        TCGETS => {
            let termios = terminal::with(|terminal| terminal.discipline.termios());
            put(process, arg, &termios.to_bytes())
        }
        TCGETS2 => {
            let termios = terminal::with(|terminal| terminal.discipline.termios());
            put(process, arg, &termios.to_bytes2())
        }
        TCSETS | TCSETSW | TCSETSF => {
            let mut bytes = [0_u8; TERMIOS_BYTES];
            get(process, arg, &mut bytes)?;
            set(request == TCSETSF, |current| {
                let speeds = (current.input_speed(), current.output_speed());
                Termios::from_bytes(&bytes).with_speeds(speeds.0, speeds.1, current)
            });
            Ok(0)
        }
        TCSETS2 | TCSETSW2 | TCSETSF2 => {
            let mut bytes = [0_u8; TERMIOS2_BYTES];
            get(process, arg, &mut bytes)?;
            set(request == TCSETSF2, |current| {
                Termios::from_bytes2(&bytes, current)
            });
            Ok(0)
        }
        TIOCGWINSZ => {
            let size = terminal::with(|terminal| terminal.winsize);
            put(process, arg, &size.to_bytes())
        }
        TIOCSWINSZ => {
            let mut bytes = [0_u8; 8];
            get(process, arg, &mut bytes)?;
            // Linux raises `SIGWINCH` on the foreground group when the size
            // changes; nothing raises it here yet.
            terminal::with(|terminal| terminal.winsize = Winsize::from_bytes(bytes));
            Ok(0)
        }
        FIONREAD => {
            let count = i32::try_from(terminal::available()).unwrap_or(i32::MAX);
            put(process, arg, &count.to_le_bytes())
        }
        TCFLSH => match arg {
            TCIFLUSH | TCIOFLUSH => {
                terminal::with(|terminal| terminal.discipline.flush_input());
                Ok(0)
            }
            // Output is never queued, so there is none to discard.
            TCOFLUSH => Ok(0),
            _ => Err(Errno::EINVAL),
        },
        // Output is never held back, so suspending and restarting it are both
        // already true.
        TCXONC if arg <= TCION => Ok(0),
        TCXONC => Err(Errno::EINVAL),
        TIOCSCTTY | TIOCNOTTY | TIOCGPGRP | TIOCSPGRP | TIOCGSID => {
            job_control(process, request, arg)
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// Change the console's settings to what `settings` makes of the current
/// ones, discarding unread input first if `flush` -- the `F` requests.
///
/// Nothing is buffered on the way out, so "once output has drained" is now,
/// and the `W` requests are the plain ones.
fn set(flush: bool, settings: impl FnOnce(Termios) -> Termios) {
    terminal::with(|terminal| {
        if flush {
            terminal.discipline.flush_input();
        }
        let termios = settings(terminal.discipline.termios());
        terminal.discipline.set_termios(termios);
    });
}

/// The requests about sessions and process groups.
///
/// The live processes are listed before the terminal is locked and dropped
/// after it is released: dropping the last reference to a process frees its
/// memory, which is not something to do under a spin lock.
#[inline(never)]
fn job_control(process: &Process, request: u32, arg: u64) -> Result<usize, Errno> {
    let live = registry::live()?;
    let answer = match request {
        TIOCSCTTY => terminal::with(|terminal| take_controlling(process, terminal, &live, arg)),
        TIOCNOTTY => terminal::with(|terminal| {
            if terminal.session != process.sid() || terminal.session == 0 {
                return Err(Errno::ENOTTY);
            }
            // A session leader giving it up takes its session with it. Linux
            // also raises `SIGHUP` and `SIGCONT` on the foreground group.
            if process.pid() == process.sid() {
                terminal.session = 0;
                terminal.foreground = 0;
            }
            Ok(0)
        }),
        TIOCGPGRP => terminal::with(|terminal| controlling(process, terminal, &live)).and_then(
            |(_, foreground)| put_int(process, arg, pidns::pgrp_to_user(process, foreground)),
        ),
        TIOCGSID => terminal::with(|terminal| controlling(process, terminal, &live))
            .and_then(|(session, _)| put_int(process, arg, pidns::sid_to_user(process, session))),
        TIOCSPGRP => set_foreground(process, &live, arg),
        _ => Err(Errno::ENOTTY),
    };
    drop(live);
    answer
}

/// The console's session and foreground group, if the console is `process`'s
/// controlling terminal -- giving it to `process` first if it is a session
/// leader and nobody else holds it. See the module documentation.
fn controlling(
    process: &Process,
    terminal: &mut Terminal,
    live: &[Arc<Process>],
) -> Result<(u32, u32), Errno> {
    let sid = process.sid();
    if sid != 0 && terminal.session == sid {
        return Ok((terminal.session, terminal.foreground));
    }
    if sid != 0 && process.pid() == sid && session_is_gone(terminal.session, live) {
        terminal.session = sid;
        terminal.foreground = process.pgid();
        return Ok((terminal.session, terminal.foreground));
    }
    Err(Errno::ENOTTY)
}

/// `TIOCSCTTY`: Linux's rules. A session leader only; nothing to do if its
/// session already has the console; `EPERM` if another live session does,
/// unless `arg` is 1 and the caller may steal it -- which everyone may, since
/// everything runs as root.
fn take_controlling(
    process: &Process,
    terminal: &mut Terminal,
    live: &[Arc<Process>],
    arg: u64,
) -> Result<usize, Errno> {
    let sid = process.sid();
    if process.pid() != sid || sid == 0 {
        return Err(Errno::EPERM);
    }
    if terminal.session == sid {
        return Ok(0);
    }
    if !session_is_gone(terminal.session, live) && arg != 1 {
        return Err(Errno::EPERM);
    }
    terminal.session = sid;
    terminal.foreground = process.pgid();
    Ok(0)
}

/// `TIOCSPGRP`, in Linux's order: `ENOTTY` if the console is not the caller's
/// terminal, `EFAULT`, `EINVAL` for a negative group, `ESRCH` for a group
/// nobody is in, `EPERM` for one in another session.
///
/// A process in a background group changing the foreground would be sent
/// `SIGTTOU` on Linux unless it ignores it -- which a shell's children do not
/// yet, and nothing delivers a signal, so the change is simply allowed.
#[inline(never)]
fn set_foreground(process: &Process, live: &[Arc<Process>], arg: u64) -> Result<usize, Errno> {
    let (session, _) = terminal::with(|terminal| controlling(process, terminal, live))?;
    let mut bytes = [0_u8; 4];
    get(process, arg, &mut bytes)?;
    let group = u32::try_from(i32::from_le_bytes(bytes)).map_err(|_| Errno::EINVAL)?;
    // A number in the caller's namespace; a group it has no one in is no group.
    let group = pidns::pgrp_from_user(process, group).ok_or(Errno::ESRCH)?;
    let members: Vec<u32> = live
        .iter()
        .filter(|other| other.pgid() == group)
        .map(|other| other.sid())
        .chain((process.pgid() == group).then(|| process.sid()))
        .collect();
    if members.is_empty() {
        return Err(Errno::ESRCH);
    }
    if !members.contains(&session) {
        return Err(Errno::EPERM);
    }
    terminal::with(|terminal| {
        if terminal.session == session {
            terminal.foreground = group;
        }
    });
    Ok(0)
}

/// Whether no live process is in session `sid`, so the console it held is
/// free. Zero is nobody's session.
fn session_is_gone(sid: u32, live: &[Arc<Process>]) -> bool {
    sid == 0 || !live.iter().any(|process| process.sid() == sid)
}

/// Raise `signal` on the console's foreground process group: what `ISIG`
/// asks for when the interrupt, quit or suspend character is typed.
///
/// Raise `signal` on the console's foreground process group: what the
/// interrupt, quit and suspend characters do.
///
/// Every live process in the group is sent it, as the kernel's own, which is
/// what Linux's line discipline does; a shell that catches `SIGINT` carries on
/// and a program that does not ends. With no foreground group, nothing is
/// raised and the keystroke is simply consumed.
pub(crate) fn signal_foreground_group(signal: u32) {
    let foreground = terminal::with(|terminal| terminal.foreground);
    if foreground == 0 {
        return;
    }
    // With no memory for the list the signal is not sent: a keyboard's
    // interrupt under memory exhaustion is lost rather than the machine.
    for target in registry::live().unwrap_or_default() {
        if target.pgid() == foreground {
            crate::syscall::kill::send(&target, signal, crate::syscall::signal::Origin::Kernel);
        }
    }
}

/// Copy a request's structure out of the program.
fn get(process: &Process, at: u64, bytes: &mut [u8]) -> Result<(), Errno> {
    uaccess::copy_from_user(process.space(), at, bytes).map_err(|_| Errno::EFAULT)
}

/// Copy a request's answer into the program.
fn put(process: &Process, at: u64, bytes: &[u8]) -> Result<usize, Errno> {
    uaccess::copy_to_user(process.space(), at, bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// Copy a `pid_t` answer into the program.
fn put_int(process: &Process, at: u64, value: u32) -> Result<usize, Errno> {
    put(process, at, &value.to_le_bytes())
}

// ---------------------------------------------------------------------------
// Pseudoterminals
//
// The same requests, answered about a pair rather than about the console.
// `crate::syscall::fd::sys_ioctl` decides which by the object the descriptor
// holds: a master, a slave, or the console.
// ---------------------------------------------------------------------------

/// `ioctl` on a pseudoterminal's master.
///
/// Every request the slave takes, and the two the master has of its own:
/// `TIOCGPTN`, which says which pair it is, and `TIOCSPTLCK`, which unlocks
/// the slave. Linux answers the terminal requests on a master as well, and
/// they act on the pair -- `TCSETS` on the master is what `openpty` uses to
/// set a terminal up before anything opens the slave.
pub(crate) fn master_ioctl(
    process: &Process,
    master: &crate::fs::pty::MasterFile,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    match request {
        TIOCGPTN => put_int(process, arg, master.pty.number),
        TIOCSPTLCK => {
            let mut bytes = [0_u8; 4];
            get(process, arg, &mut bytes)?;
            // A zero unlocks, as `unlockpt` writes; anything else locks.
            crate::fs::pty::set_locked(&master.pty, i32::from_le_bytes(bytes) != 0);
            Ok(0)
        }
        _ => pty_ioctl(process, &master.pty, request, arg, false),
    }
}

/// `ioctl` on a pseudoterminal's slave, which is a terminal like any other.
pub(crate) fn slave_ioctl(
    process: &Process,
    slave: &crate::fs::pty::SlaveFile,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    pty_ioctl(process, &slave.pty, request, arg, true)
}

/// The requests both ends take.
///
/// `job_control` is the slave's alone: a master is not a controlling
/// terminal and `TIOCSCTTY` on one is `ENOTTY`, which is what Linux answers.
fn pty_ioctl(
    process: &Process,
    pty: &crate::fs::pty::Pty,
    request: u32,
    arg: u64,
    slave: bool,
) -> Result<usize, Errno> {
    match request {
        TCGETS => put(process, arg, &pty.termios().to_bytes()),
        TCGETS2 => put(process, arg, &pty.termios().to_bytes2()),
        TCSETS | TCSETSW | TCSETSF => {
            let mut bytes = [0_u8; TERMIOS_BYTES];
            get(process, arg, &mut bytes)?;
            let current = pty.termios();
            let speeds = (current.input_speed(), current.output_speed());
            let termios = Termios::from_bytes(&bytes).with_speeds(speeds.0, speeds.1, current);
            pty.set_termios(termios, request == TCSETSF);
            Ok(0)
        }
        TCSETS2 | TCSETSW2 | TCSETSF2 => {
            let mut bytes = [0_u8; TERMIOS2_BYTES];
            get(process, arg, &mut bytes)?;
            let termios = Termios::from_bytes2(&bytes, pty.termios());
            pty.set_termios(termios, request == TCSETSF2);
            Ok(0)
        }
        TIOCGWINSZ => put(process, arg, &pty.winsize().to_bytes()),
        TIOCSWINSZ => {
            let mut bytes = [0_u8; 8];
            get(process, arg, &mut bytes)?;
            pty.set_winsize(Winsize::from_bytes(bytes));
            Ok(0)
        }
        FIONREAD => {
            // Each end counts what it could read: the slave the typing, the
            // master what the program wrote.
            let waiting = if slave {
                pty.slave_available()
            } else {
                pty.master_available()
            };
            let count = i32::try_from(waiting).unwrap_or(i32::MAX);
            put(process, arg, &count.to_le_bytes())
        }
        TCFLSH => match arg {
            TCIFLUSH => {
                pty.flush_input();
                Ok(0)
            }
            TCOFLUSH => {
                pty.flush_output();
                Ok(0)
            }
            TCIOFLUSH => {
                pty.flush_input();
                pty.flush_output();
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        },
        // Output is never held back, so suspending and restarting it are
        // both already true.
        TCXONC if arg <= TCION => Ok(0),
        TCXONC => Err(Errno::EINVAL),
        TIOCSCTTY | TIOCNOTTY | TIOCGPGRP | TIOCSPGRP | TIOCGSID if slave => {
            pty_job_control(process, pty, request, arg)
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// The session and process-group requests, on a slave.
///
/// A pseudoterminal's rules are the console's: the session leader that asks
/// for it gets it if nobody else holds it, a process may only ask about the
/// terminal of its own session, and the foreground group must be a group in
/// that session.
#[inline(never)]
fn pty_job_control(
    process: &Process,
    pty: &crate::fs::pty::Pty,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    let live = registry::live()?;
    let answer = match request {
        TIOCSCTTY => {
            let session = pty.session();
            if session != 0 && session != process.sid() {
                return Err(Errno::EPERM);
            }
            if process.pid() != process.sid() {
                return Err(Errno::EPERM);
            }
            pty.set_session(process.sid(), process.pgid());
            Ok(0)
        }
        TIOCNOTTY => {
            if pty.session() != process.sid() || pty.session() == 0 {
                Err(Errno::ENOTTY)
            } else {
                if process.pid() == process.sid() {
                    pty.set_session(0, 0);
                }
                Ok(0)
            }
        }
        TIOCGPGRP => {
            if pty.session() != process.sid() {
                Err(Errno::ENOTTY)
            } else {
                put_int(process, arg, pidns::pgrp_to_user(process, pty.foreground()))
            }
        }
        TIOCGSID => {
            if pty.session() != process.sid() {
                Err(Errno::ENOTTY)
            } else {
                put_int(process, arg, pidns::sid_to_user(process, pty.session()))
            }
        }
        TIOCSPGRP => {
            let mut bytes = [0_u8; 4];
            get(process, arg, &mut bytes)?;
            let number = u32::from_le_bytes(bytes);
            // A number in the caller's namespace.
            let group = pidns::pgrp_from_user(process, number).unwrap_or(0);
            if pty.session() != process.sid() {
                return Err(Errno::ENOTTY);
            }
            // The group has to be one of this session's, which is what
            // `tcsetpgrp` promises a shell.
            let known = live
                .iter()
                .any(|target| target.pgid() == group && target.sid() == process.sid());
            if !known {
                return Err(Errno::EPERM);
            }
            pty.set_foreground(group);
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    };
    drop(live);
    answer
}
