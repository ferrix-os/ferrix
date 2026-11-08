//! The watchdog: ring 3 feeds it, and a feed that does not come ends in a
//! report, and in a reset only under `ferrix.panic=reset`
//! (`docs/POSTMORTEM.md` §6.11, `ferrix.watchdog=on`).
//!
//! **What "stuck" means here.** A machine nobody can reach any more: its
//! supervisor, init, cannot turn its own loop within the timeout. That is
//! decided in ring 3, by what feeds -- init's `ferrix.init.watchdog=`, or any
//! privileged program that opens `/dev/watchdog` -- and never by a kernel
//! timer, which goes on ticking while all of user space waits on something
//! that will not come (the N150 box on 2026-10-31: ping answered, every
//! program stopped). A game or a compositor that hangs is not stuck while
//! init turns and sshd answers; a kernel that spins with every processor's
//! interrupts masked is, and so is one whose scheduler no longer runs init.
//!
//! **The kernel's part**, all of it, and the policy none of it: a deadline
//! on the kernel's clock, moved by each feed, that a task of its own checks
//! once a second; past it, the report (FX-9008) naming who armed it, its
//! timeout and how long since the last feed, and then `ferrix.panic`'s halt
//! or reset.
//!
//! **What it does not catch.** The task runs on the kernel's own scheduler
//! and timer, so it reports ring 3 that stopped feeding while the kernel
//! runs. A kernel that stops -- every processor masked, the scheduler gone
//! -- stops the task too, and nothing reports it: that is the lockup
//! detector's and a hardware timer's (S4a, the chipset's TCO), both parked
//! (`docs/POSTMORTEM.md` §6.11). A report resets the machine only under
//! `ferrix.panic=reset`; otherwise it halts.
//!
//! The timeout, whether to arm at all and what must be alive for the feed to
//! come are the opener's. The interface is typed and knows no Linux number:
//! `/dev/watchdog` (`fs::watchdog`, the load) turns `open`, `write`, `ioctl`
//! and the last release into the calls below.
//!
//! Off unless asked for: without `ferrix.watchdog=on` no node exists and no
//! task runs.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;

use crate::console::println;
use crate::panic::{catalog, fatal};
use crate::{sched, timer};

/// The command-line option, and the one value that asks for it.
pub(crate) const OPTION: &str = "ferrix.watchdog";
const ON: &str = "on";

/// The timeout an open arms with until one is set, as Linux's softdog's.
const DEFAULT_SECONDS: u32 = 60;

/// The longest timeout a program may set: ten minutes.
pub(crate) const MAX_SECONDS: u32 = 600;

/// How often the task looks at the deadline, in nanoseconds.
const LOOK_EVERY: u64 = 1_000_000_000;

/// Whether `ferrix.watchdog=on` was asked for.
static ASKED: AtomicBool = AtomicBool::new(false);

/// Whether the device is served: asked, and the task running.
static SERVED: AtomicBool = AtomicBool::new(false);

/// When an armed watchdog expires, on the kernel's clock; 0 while disarmed.
static DEADLINE: AtomicU64 = AtomicU64::new(0);

/// The timeout, in seconds.
static TIMEOUT: AtomicU32 = AtomicU32::new(DEFAULT_SECONDS);

/// When the last feed came, on the kernel's clock (the arm counts as one).
static LAST_FEED: AtomicU64 = AtomicU64::new(0);

/// Whether a file holds the device open: at most one at a time.
static OPEN: AtomicBool = AtomicBool::new(false);

/// Who armed it: the process id, and its name's first 16 bytes, copied at
/// [`open`].
static OPENER: AtomicU32 = AtomicU32::new(0);
static OPENER_NAME: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Read `ferrix.watchdog` from the loader's command line: once, at boot.
/// Exactly `on` asks for it; anything else is said and taken as off.
fn read_option(view: &BootView<'_>) {
    match view.option(OPTION) {
        None => {}
        Some(ON) => ASKED.store(true, Ordering::Release),
        Some(other) => println!("  watchdog {OPTION}={other} is not understood; it is off"),
    }
}

/// Read the option and start the task, if asked: once, after the device
/// nodes are published and the scheduler runs, before init. Says what it
/// does; with no task to look at the deadline the device is not served.
pub fn start(view: &BootView<'_>) {
    read_option(view);
    if !ASKED.load(Ordering::Acquire) {
        return;
    }
    if sched::spawn("watchdog", look_forever, 0, ferrix_sched::NICE_0_WEIGHT).is_err() {
        println!("  watchdog no task to look at the deadline: /dev/watchdog is not served");
        return;
    }
    SERVED.store(true, Ordering::Release);
    println!(
        "  watchdog {OPTION}={ON}: /dev/watchdog is served; once opened, a feed that does not come within its timeout is reported"
    );
}

/// Whether `/dev/watchdog` is served on this boot.
pub fn served() -> bool {
    SERVED.load(Ordering::Acquire)
}

/// The device is open already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

/// Arm the watchdog for process `pid` named `name`, with the current
/// timeout: one opener at a time.
///
/// # Errors
///
/// [`Busy`] when a file holds it open already.
pub fn open(pid: u32, name: &[u8]) -> Result<(), Busy> {
    if OPEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Busy);
    }
    OPENER.store(pid, Ordering::Relaxed);
    let mut bytes = [0_u8; 16];
    for (to, from) in bytes.iter_mut().zip(name) {
        *to = *from;
    }
    for (slot, half) in OPENER_NAME.iter().zip(bytes.as_chunks::<8>().0) {
        slot.store(u64::from_le_bytes(*half), Ordering::Relaxed);
    }
    arm();
    Ok(())
}

/// Arm, or move an armed deadline to a full timeout from now.
fn arm() {
    let now = timer::now_nanos();
    LAST_FEED.store(now, Ordering::Relaxed);
    DEADLINE.store(deadline_from(now), Ordering::Release);
}

/// A full timeout from `now`, never 0.
fn deadline_from(now: u64) -> u64 {
    let length = u64::from(TIMEOUT.load(Ordering::Relaxed)).saturating_mul(1_000_000_000);
    now.saturating_add(length).max(1)
}

/// Feed: move an armed deadline to a full timeout from now. Never arms a
/// disarmed one: the move is a compare-and-swap from a deadline that is not
/// 0. Whether it was armed.
pub fn feed() -> bool {
    let now = timer::now_nanos();
    let next = deadline_from(now);
    let fed = DEADLINE
        .try_update(Ordering::AcqRel, Ordering::Acquire, |deadline| {
            (deadline != 0).then_some(next)
        })
        .is_ok();
    if fed {
        LAST_FEED.store(now, Ordering::Relaxed);
    }
    fed
}

/// Set the timeout to `seconds`, 1 to [`MAX_SECONDS`], and re-arm an armed
/// one with it: the timeout set, or `None` for one out of range. The re-arm
/// is a [`feed`], so a deadline disarmed meanwhile stays disarmed.
pub fn set_timeout(seconds: u32) -> Option<u32> {
    if !(1..=MAX_SECONDS).contains(&seconds) {
        return None;
    }
    TIMEOUT.store(seconds, Ordering::Relaxed);
    let _ = feed();
    Some(seconds)
}

/// The timeout, in seconds.
pub fn timeout() -> u32 {
    TIMEOUT.load(Ordering::Relaxed)
}

/// The whole seconds left before an armed watchdog expires; `None` while
/// disarmed.
pub fn time_left() -> Option<u64> {
    let deadline = DEADLINE.load(Ordering::Acquire);
    (deadline != 0).then(|| deadline.saturating_sub(timer::now_nanos()) / 1_000_000_000)
}

/// Whether it is armed.
pub(crate) fn armed() -> bool {
    DEADLINE.load(Ordering::Acquire) != 0
}

/// Arm it again from the file that holds it open (`WDIOS_ENABLECARD`).
pub fn enable() {
    arm();
}

/// Disarm it on purpose for process `pid` (`WDIOS_DISABLECARD`), saying so.
pub fn disable(pid: u32) {
    disarm(pid, "asked to stop it");
}

/// The file that held it open is gone: its last reference, not each close
/// of a copy. After a magic close, disarmed; otherwise armed and counting,
/// said, and only a new opener can feed it.
pub fn release(pid: u32, magic: bool) {
    if magic {
        disarm(pid, "closed it with the magic `V`");
    } else if armed() {
        println!(
            "  watchdog process {pid} closed /dev/watchdog without the magic `V`: it stays armed and counting"
        );
    }
    OPEN.store(false, Ordering::Release);
}

/// Disarm before a stop on purpose -- `reboot(2)`, a power-off, init's exit
/// -- saying so, so that a shutdown slower than the timeout is not reported
/// as a starved feeder. Nothing while disarmed.
pub fn stop_for_shutdown(why: &str) {
    if DEADLINE.swap(0, Ordering::AcqRel) != 0 {
        println!("  watchdog disarmed for {why}");
    }
}

/// Disarm, naming who and why.
fn disarm(pid: u32, why: &str) {
    if DEADLINE.swap(0, Ordering::AcqRel) != 0 {
        println!("  watchdog process {pid} {why}: disarmed");
    }
}

/// The task: look at the deadline once a second, for ever.
fn look_forever(_: usize) {
    loop {
        let deadline = DEADLINE.load(Ordering::Acquire);
        let now = timer::now_nanos();
        if deadline != 0 && now >= deadline {
            expired(now);
        }
        sched::sleep_for(LOOK_EVERY);
    }
}

/// DIAG, never to land (fault-relanding): what the load prints of the
/// system's tasks before the report, set by the load when the watchdog opens.
pub static HANG_DUMP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The deadline passed: report it.
fn expired(now: u64) -> ! {
    let dump = HANG_DUMP.load(Ordering::Acquire);
    if dump != 0 {
        // SAFETY: only ever a `fn()` stored by the load's diag.
        let dump: fn() = unsafe { core::mem::transmute::<usize, fn()>(dump) };
        dump();
    }
    let pid = OPENER.load(Ordering::Relaxed);
    let mut name = [0_u8; 16];
    for (half, to) in OPENER_NAME.iter().zip(name.as_chunks_mut::<8>().0) {
        *to = half.load(Ordering::Relaxed).to_le_bytes();
    }
    let length = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(name.len());
    let name = core::str::from_utf8(name.get(..length).unwrap_or_default()).unwrap_or("?");
    let since = now.saturating_sub(LAST_FEED.load(Ordering::Relaxed)) / 1_000_000;
    fatal!(
        catalog::WATCHDOG_STARVED,
        "user space stopped feeding /dev/watchdog: armed by process {pid} ({name}) with a {} s timeout, last fed {since} ms ago",
        timeout()
    );
}
