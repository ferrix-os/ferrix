//! The user watchdog: user space feeds the kernel, and a feed that does not
//! come within its timeout ends in a failure report (`docs/POSTMORTEM.md`
//! §6.9, `ferrix.watchdog=user`, H.FAIL.14).
//!
//! A machine whose processors all take interrupts can still have stopped:
//! every program waiting on something that never comes. Neither the lockup
//! detector (`lockup`) nor a hardware watchdog fed from the kernel's tick sees
//! that. Here a program -- init, which the policy of what must be alive for it
//! to feed belongs to -- arms a deadline and moves it on with each feed. The
//! kernel holds the deadline, the timeout, the time of the last feed and who
//! armed it; nothing else. Every watching processor's tick (`watch`) compares
//! the counter with the deadline, and one past it stores the stop word (a
//! lockup) and reports under FX-9008.
//!
//! The interface is typed and knows no Linux number: `/dev/watchdog`'s file
//! (`fs::watchdog`, the load) turns `open`, `write`, `ioctl` and the last
//! release into the calls below (`L.lockup.6`, `L.lockup.7`).
//!
//! * [`open`] arms, one opener at a time; [`feed`] moves an armed deadline
//!   and never arms; [`release`] after a magic close disarms, any other
//!   release leaves it armed; [`enable`] and [`disable`] are the explicit
//!   arm and disarm; [`set_timeout`] feeds an armed one with the new length.
//! * Every disarm says so, naming the process.
//! * [`stop_for_shutdown`] disarms before a stop on purpose (`reboot(2)`,
//!   init's exit), as Linux's softdog does on reboot (`L.lockup.8`).
//!
//! Off unless asked for: without `ferrix.watchdog=user` no node exists and
//! nothing here runs.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;

use crate::console::println;
use crate::panic::{catalog, fatal};
use crate::watch::{self, Watch};

/// The command-line option, and its words.
pub const OPTION: &str = "ferrix.watchdog";
const USER: &str = "user";
const TCO: &str = "tco";

/// The timeout an open arms with until one is set, as Linux's softdog's.
const DEFAULT_SECONDS: u32 = 60;

/// The longest timeout a program may set: ten minutes.
pub const MAX_SECONDS: u32 = 600;

/// Whether `ferrix.watchdog` named `user`.
static ASKED: AtomicBool = AtomicBool::new(false);

/// Whether the watch tick's check is registered: the node exists only then.
static STARTED: AtomicBool = AtomicBool::new(false);

/// When an armed watchdog expires, on the counter; 0 while disarmed.
static DEADLINE: AtomicU64 = AtomicU64::new(0);

/// The timeout, in seconds.
static TIMEOUT: AtomicU32 = AtomicU32::new(DEFAULT_SECONDS);

/// When the last feed came, on the counter (the arm counts as one).
static LAST_FEED: AtomicU64 = AtomicU64::new(0);

/// Whether a file holds the device open: at most one at a time.
static OPEN: AtomicBool = AtomicBool::new(false);

/// Who armed it: the process id, and its name's first 16 bytes, copied at
/// [`open`].
static OPENER: AtomicU32 = AtomicU32::new(0);
static OPENER_NAME: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether a report has begun: the first processor past the deadline
/// reports, the others stay quiet.
static REPORTED: AtomicBool = AtomicBool::new(false);

/// The check's watch.
static CHECK: Watch = Watch { tick: on_tick };

/// Read `ferrix.watchdog` from the loader's command line: once, at boot. A
/// comma-separated list: `user` asks for this; `tco` is S4b's, not in this
/// kernel; any other word is said and ignored.
pub fn read_option(view: &BootView<'_>) {
    let Some(value) = view.option(OPTION) else {
        return;
    };
    for word in value.split(',') {
        match word {
            USER => ASKED.store(true, Ordering::Release),
            TCO => println!(
                "  watchdog {OPTION}: `{TCO}` is not in this kernel (docs/POSTMORTEM.md 6.3); ignored"
            ),
            other => println!("  watchdog {OPTION}: `{other}` is not understood; ignored"),
        }
    }
}

/// Whether the user watchdog was asked for.
pub fn asked() -> bool {
    ASKED.load(Ordering::Acquire)
}

/// Whether the user watchdog runs: asked for and its check registered.
/// `/dev/watchdog` exists exactly then.
pub fn running() -> bool {
    STARTED.load(Ordering::Acquire)
}

/// Register the check on the watch tick, if asked: once every processor is
/// online, before init, before `watch::start`. Says what it does; a refused
/// registration says so and leaves the node absent. Whether it is
/// registered.
pub fn register() -> bool {
    if !asked() {
        return false;
    }
    if watch::register(&CHECK).is_err() {
        println!("  watchdog no room to register the user watchdog's check; it is off");
        return false;
    }
    STARTED.store(true, Ordering::Release);
    println!(
        "  watchdog {OPTION}={USER}: /dev/watchdog is served; once opened, a feed that does not come within its timeout is reported"
    );
    true
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
    let (low, high) = bytes.split_at(8);
    let word = |half: &[u8]| {
        let mut eight = [0_u8; 8];
        eight.copy_from_slice(half);
        u64::from_le_bytes(eight)
    };
    let [first, second] = &OPENER_NAME;
    first.store(word(low), Ordering::Relaxed);
    second.store(word(high), Ordering::Relaxed);
    arm();
    Ok(())
}

/// Arm, or move an armed deadline to a full timeout from now.
fn arm() {
    let now = crate::timer::now_nanos();
    LAST_FEED.store(now, Ordering::Relaxed);
    DEADLINE.store(deadline_from(now), Ordering::Release);
}

/// A full timeout from `now`, never 0.
fn deadline_from(now: u64) -> u64 {
    let length = u64::from(TIMEOUT.load(Ordering::Relaxed)).saturating_mul(1_000_000_000);
    now.saturating_add(length).max(1)
}

/// Feed: move an armed deadline to a full timeout from now. Never arms a
/// disarmed one (`L.lockup.7`). Whether it was armed.
pub fn feed() -> bool {
    let now = crate::timer::now_nanos();
    let next = deadline_from(now);
    let fed = DEADLINE
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |deadline| {
            (deadline != 0).then_some(next)
        })
        .is_ok();
    if fed {
        LAST_FEED.store(now, Ordering::Relaxed);
    }
    fed
}

/// Set the timeout to `seconds`, 1 to [`MAX_SECONDS`], and feed: the
/// timeout set, or `None` for one out of range.
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
    (deadline != 0)
        .then(|| deadline.saturating_sub(crate::timer::now_nanos()) / 1_000_000_000)
}

/// Whether it is armed.
pub fn armed() -> bool {
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
/// said, and only a new opener can feed it (`L.lockup.7`).
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

/// Disarm before a stop on purpose: `reboot(2)` and init's exit, before
/// their flush (`L.lockup.8`). Nothing while disarmed.
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

/// A watch tick on this processor: is an armed deadline past?
fn on_tick(now: u64) {
    if crate::smp::stopping() {
        return;
    }
    let deadline = DEADLINE.load(Ordering::Acquire);
    if deadline == 0 || now < deadline {
        return;
    }
    if REPORTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // How the boot stopped, before anything else (`L.lockup.8`).
    crate::console::pstore::locked_up();
    let pid = OPENER.load(Ordering::Relaxed);
    let mut name = [0_u8; 16];
    for (index, half) in OPENER_NAME.iter().enumerate() {
        let bytes = half.load(Ordering::Relaxed).to_le_bytes();
        if let Some(to) = name.get_mut(index * 8..index * 8 + 8) {
            to.copy_from_slice(&bytes);
        }
    }
    let length = name.iter().position(|&byte| byte == 0).unwrap_or(name.len());
    let name = core::str::from_utf8(name.get(..length).unwrap_or_default()).unwrap_or("?");
    let since = now.saturating_sub(LAST_FEED.load(Ordering::Relaxed)) / 1_000_000;
    fatal!(
        catalog::USER_LOCKUP,
        "user space stopped feeding /dev/watchdog: armed by process {pid} ({name}) with a {} s timeout, last fed {since} ms ago",
        timeout()
    );
}
