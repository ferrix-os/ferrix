//! Feeding the kernel's watchdog (`docs/POSTMORTEM.md` §6.11,
//! `docs/WATCHDOG-STAGES.md` §4): the policy half of it, ring 3's.
//!
//! With `ferrix.init.watchdog=<seconds>` on the command line, init opens
//! `/dev/watchdog`, sets that timeout and feeds it every third of it from
//! **a thread of its own**, the feeder, and not from its event loop. The
//! loop reaches the disk and the console in several places
//! (`WATCHDOG-STAGES.md` §1), and one slow read there must not reset the
//! machine. So a feed by init proves that a thread of pid 1 was scheduled
//! and its clock sleep woke, and that init's loop has not been stuck in one
//! call for longer than the stall bound below. It does not prove that the
//! rest of user space makes progress: a root filesystem that stops can stop
//! every other program while the feeder feeds on. Unlike a kernel timer it
//! needs ring 3 to be scheduled and pid 1 not to be stopped: `kill -STOP 1`
//! stops the feeder too, and FX-9008 follows.
//!
//! **The loop's liveness.** The loop marks each turn ([`turn`]) and names the
//! call it is in when that call may wait on something outside init
//! ([`Phase`]); `epoll_wait` is [`Phase::Waiting`], since an idle init is not
//! a stuck one. On each feed the feeder looks: a loop that has not turned
//! for the watchdog's timeout, in a call other than `epoll_wait`, is said on
//! `/dev/kmsg`, again at each doubling, and said again when it turns. Past
//! `ferrix.init.watchdog.stall=<seconds>` (300 by default, at least three
//! timeouts; 0 reports only) the feeder says the watchdog is no longer fed
//! and stops feeding, so the kernel reports FX-9008 a timeout later: a
//! supervisor stuck that long restarts nothing, answers no `svc` and runs no
//! service watchdog (the consultant's ruling R-1, ledger 8355). A loop that
//! turns again before the report is fed again.
//!
//! **What the feeder may do**, after its first feed, and nothing else:
//! `write(2)` to the device and to `/dev/kmsg`, both opened before it
//! starts; `clock_nanosleep(2)` to an absolute deadline; read the loop's two
//! atomics. It allocates nothing and takes no lock, not even standard
//! output's, so a child init forks holds none of its locks (`sys::fork_into`)
//! and a stuck loop cannot stop it. Its lines are formatted on its own
//! stack. A line `/dev/kmsg` refuses is dropped; the feed comes first.
//!
//! Init never writes the magic `V`: a stop on purpose disarms the watchdog
//! in the kernel (`reboot(2)`, init's exit). Without the option, or without
//! the device (`ferrix.watchdog=on` not given), nothing is opened, no thread
//! starts and nothing is fed.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd as _, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use crate::sys;

/// Init's option, read from the kernel's command line.
const OPTION: &str = "ferrix.init.watchdog=";

/// The stall bound's option.
const STALL_OPTION: &str = "ferrix.init.watchdog.stall=";

/// The stall bound when none is given, in seconds.
const DEFAULT_STALL: u64 = 300;

/// The device.
const DEVICE: &str = "/dev/watchdog";

/// Where the feeder's lines go: the kernel's log, which reaches the console
/// without waiting on it.
const KMSG: &str = "/dev/kmsg";

/// The longest timeout the kernel takes.
const MAX_SECONDS: u32 = 600;

/// A nanosecond count of one second.
const SECOND: u64 = 1_000_000_000;

/// The call the loop is in, for the feeder's line about a stuck loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Phase {
    /// In `epoll_wait`: idle, never a stall.
    Waiting = 0,
    /// Stepping the manager and carrying out what it asks, in no call below.
    Turn = 1,
    /// `mount(2)` or `umount2(2)` of a unit.
    Mount = 2,
    /// Reading the unit directories.
    Units = 3,
    /// Writing the audit record's file.
    Audit = 4,
    /// Starting a process.
    Spawn = 5,
    /// Reading a native service's program.
    Native = 6,
    /// Talking to lode.
    Lode = 7,
    /// Answering a control client.
    Control = 8,
    /// Reading `/proc` about a service.
    Proc = 9,
    /// Looking a user up.
    Users = 10,
}

impl Phase {
    const ALL: [Phase; 11] = [
        Phase::Waiting,
        Phase::Turn,
        Phase::Mount,
        Phase::Units,
        Phase::Audit,
        Phase::Spawn,
        Phase::Native,
        Phase::Lode,
        Phase::Control,
        Phase::Proc,
        Phase::Users,
    ];

    /// Its word in the feeder's line.
    fn word(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Turn => "turn",
            Phase::Mount => "mount",
            Phase::Units => "units",
            Phase::Audit => "audit",
            Phase::Spawn => "spawn",
            Phase::Native => "native",
            Phase::Lode => "lode",
            Phase::Control => "control",
            Phase::Proc => "proc",
            Phase::Users => "users",
        }
    }

    fn of(code: u8) -> Phase {
        Phase::ALL
            .into_iter()
            .find(|phase| *phase as u8 == code)
            .unwrap_or(Phase::Turn)
    }

    /// Enter this phase until the guard drops, then back to the one before.
    pub(crate) fn enter(self) -> InPhase {
        InPhase(PHASE.swap(self as u8, Ordering::AcqRel))
    }
}

/// While it lives, the loop is in a [`Phase`]; it holds the one before.
#[derive(Debug)]
pub(crate) struct InPhase(u8);

impl Drop for InPhase {
    fn drop(&mut self) {
        PHASE.store(self.0, Ordering::Release);
    }
}

/// The loop's phase, as [`Phase`]'s number.
static PHASE: AtomicU8 = AtomicU8::new(Phase::Turn as u8);

/// When the loop last turned, on `CLOCK_MONOTONIC`.
static TURNED: AtomicU64 = AtomicU64::new(0);

/// Whether the feeder has fed once: the loop forks nothing before.
static FED: AtomicBool = AtomicBool::new(false);

/// The loop turned: called at the top of each turn and after `epoll_wait`.
pub(crate) fn turn() {
    TURNED.store(sys::monotonic(), Ordering::Release);
    PHASE.store(Phase::Turn as u8, Ordering::Release);
}

/// The loop is about to wait in `epoll_wait`.
pub(crate) fn waiting() {
    PHASE.store(Phase::Waiting as u8, Ordering::Release);
}

/// What the option asked for, or why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Asked {
    seconds: u32,
    /// The stall bound in seconds; 0 reports only.
    stall: u64,
}

/// Read the two options from `command_line`: `None` without the first,
/// `Err` with what to say for a value that is not one.
fn asked(command_line: &str) -> Option<Result<(Asked, Option<String>), String>> {
    let value = command_line
        .split_whitespace()
        .find_map(|word| word.strip_prefix(OPTION))?;
    let seconds = match value.parse::<u32>() {
        Ok(seconds) if (1..=MAX_SECONDS).contains(&seconds) => seconds,
        _ => {
            return Some(Err(format!(
                "{OPTION}{value} is not a number of seconds from 1 to {MAX_SECONDS}; the watchdog is not fed"
            )));
        }
    };
    let least = u64::from(seconds) * 3;
    let mut said = None;
    let stall = match command_line
        .split_whitespace()
        .find_map(|word| word.strip_prefix(STALL_OPTION))
    {
        None => DEFAULT_STALL.max(least),
        Some(text) => match text.parse::<u64>() {
            Ok(0) => 0,
            Ok(stall) if stall >= least => stall,
            Ok(stall) => {
                said = Some(format!(
                    "{STALL_OPTION}{stall} is less than three timeouts; the stall bound is {least} s"
                ));
                least
            }
            Err(_) => {
                said = Some(format!(
                    "{STALL_OPTION}{text} is not a number of seconds; the stall bound is {}",
                    DEFAULT_STALL.max(least)
                ));
                DEFAULT_STALL.max(least)
            }
        },
    };
    Some(Ok((Asked { seconds, stall }, said)))
}

/// Open and arm the device if `command_line` asks, start the feeder and
/// wait for its first feed, saying what was done.
pub(crate) fn start(command_line: &str, say: &impl Fn(&str)) {
    let (asked, said) = match asked(command_line) {
        None => return,
        Some(Err(why)) => {
            say(&why);
            return;
        }
        Some(Ok(asked)) => asked,
    };
    if let Some(said) = said {
        say(&said);
    }
    let Asked { seconds, stall } = asked;
    let file = match OpenOptions::new().write(true).open(DEVICE) {
        Ok(file) => file,
        Err(error) => {
            say(&format!(
                "{DEVICE} could not be opened ({error}); the watchdog is not fed"
            ));
            return;
        }
    };
    if let Err(error) = sys::watchdog_timeout(file.as_raw_fd(), seconds) {
        say(&format!(
            "{DEVICE}: its timeout could not be set to {seconds} s ({error}); fed at its own"
        ));
    }
    // Opened now, so the feeder opens nothing. Without it the feeder's
    // lines are lost and the feed goes on.
    let kmsg = OpenOptions::new().write(true).open(KMSG).ok();
    let every = u64::from(seconds) * SECOND / 3;
    let feeder = Feeder {
        device: file,
        kmsg,
        every,
        timeout: u64::from(seconds) * SECOND,
        stall: stall * SECOND,
    };
    turn();
    let started = std::thread::Builder::new()
        .name(String::from("watchdog"))
        .stack_size(64 * 1024)
        .spawn(move || feeder.run());
    if let Err(error) = started {
        say(&format!(
            "the watchdog's feeder could not be started ({error}); the watchdog is armed and not fed: FX-9008 follows in {seconds} s"
        ));
        return;
    }
    // Its first feed ends its start-up, which allocates: nothing forks
    // before (`sys::fork_into`).
    while !FED.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let bound = if stall == 0 {
        String::from("a stuck loop is reported only")
    } else {
        format!("a loop stuck for {stall} s is no longer fed")
    };
    say(&format!(
        "feeding {DEVICE} every {} ms from a thread of its own, its timeout {seconds} s; {bound}",
        every / 1_000_000
    ));
}

/// The feeder thread's state, all of it opened before it starts.
#[derive(Debug)]
struct Feeder {
    device: File,
    kmsg: Option<File>,
    /// Between feeds, in nanoseconds.
    every: u64,
    /// The watchdog's timeout, in nanoseconds: the first report's age.
    timeout: u64,
    /// The stall bound, in nanoseconds; 0 reports only.
    stall: u64,
}

/// A line on the feeder's stack: no allocation.
struct Line {
    bytes: [u8; 192],
    len: usize,
}

impl Line {
    fn new() -> Line {
        Line {
            bytes: [0; 192],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
}

impl std::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.len.saturating_add(text.len()).min(self.bytes.len());
        let room = end - self.len;
        if let (Some(to), Some(from)) = (
            self.bytes.get_mut(self.len..end),
            text.as_bytes().get(..room),
        ) {
            to.copy_from_slice(from);
        }
        self.len = end;
        Ok(())
    }
}

impl Feeder {
    /// Feed, look at the loop, sleep to the next deadline; for ever.
    fn run(self) -> ! {
        let device = self.device.as_raw_fd();
        let kmsg = self.kmsg.as_ref().map(|file| file.as_raw_fd());
        let mut next = sys::monotonic();
        // The age at which a stuck loop is next said, and since when it is.
        let mut report_at = self.timeout;
        let mut stuck_since: Option<u64> = None;
        let mut unfed = false;
        loop {
            if !unfed {
                // Any byte but `V`, which would let a close disarm it.
                let _ = sys::write_raw(device, b"1");
            }
            FED.store(true, Ordering::Release);
            let now = sys::monotonic();
            let late = now.saturating_sub(next);
            if late > self.every / 2 {
                let mut line = Line::new();
                let _ = writeln!(
                    line,
                    "<4>init: the watchdog's feed came {} ms late",
                    late / 1_000_000
                );
                say(kmsg, &line);
            }
            let phase = Phase::of(PHASE.load(Ordering::Acquire));
            let turned = TURNED.load(Ordering::Acquire);
            let age = now.saturating_sub(turned);
            if phase != Phase::Waiting && age >= report_at {
                let _ = stuck_since.get_or_insert(turned);
                let mut line = Line::new();
                let _ = writeln!(
                    line,
                    "<4>init: the loop has not turned for {} s, in `{}`; the watchdog is {}",
                    age / SECOND,
                    phase.word(),
                    if unfed { "not fed" } else { "still fed" }
                );
                say(kmsg, &line);
                report_at = report_at.saturating_mul(2);
            }
            if let Some(since) = stuck_since
                && (phase == Phase::Waiting || turned != since)
            {
                let mut line = Line::new();
                let _ = writeln!(
                    line,
                    "<4>init: the loop turns again after {} s{}",
                    now.saturating_sub(since) / SECOND,
                    if unfed {
                        "; the watchdog is fed again"
                    } else {
                        ""
                    }
                );
                say(kmsg, &line);
                stuck_since = None;
                report_at = self.timeout;
                unfed = false;
            }
            if self.stall != 0 && !unfed && phase != Phase::Waiting && age >= self.stall {
                let mut line = Line::new();
                let _ = writeln!(
                    line,
                    "<3>init: the loop has not turned for {} s, in `{}`; the watchdog is no longer fed",
                    age / SECOND,
                    phase.word()
                );
                say(kmsg, &line);
                let _ = stuck_since.get_or_insert(turned);
                unfed = true;
            }
            next = next.saturating_add(self.every).max(now);
            sys::sleep_until(next);
        }
    }
}

/// Write `line` to `/dev/kmsg`; a refusal drops it.
fn say(kmsg: Option<RawFd>, line: &Line) {
    if let Some(fd) = kmsg {
        let _ = sys::write_raw(fd, line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{Asked, asked};

    #[test]
    fn the_options_read_as_designed() {
        assert_eq!(asked("quiet"), None);
        assert!(matches!(asked("ferrix.init.watchdog=0"), Some(Err(_))));
        assert_eq!(
            asked("ferrix.init.watchdog=5"),
            Some(Ok((
                Asked {
                    seconds: 5,
                    stall: 300
                },
                None
            )))
        );
        assert_eq!(
            asked("ferrix.init.watchdog=200"),
            Some(Ok((
                Asked {
                    seconds: 200,
                    stall: 600
                },
                None
            )))
        );
        assert!(matches!(
            asked("ferrix.init.watchdog=5 ferrix.init.watchdog.stall=0"),
            Some(Ok((Asked { stall: 0, .. }, None)))
        ));
        assert!(matches!(
            asked("ferrix.init.watchdog=5 ferrix.init.watchdog.stall=15"),
            Some(Ok((Asked { stall: 15, .. }, None)))
        ));
        assert!(matches!(
            asked("ferrix.init.watchdog=5 ferrix.init.watchdog.stall=10"),
            Some(Ok((Asked { stall: 15, .. }, Some(_))))
        ));
    }
}
