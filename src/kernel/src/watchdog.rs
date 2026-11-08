//! The watchdog: ring 3 feeds it, and a feed that does not come ends in a
//! report and a reset (`docs/POSTMORTEM.md` §6.11, `ferrix.watchdog=on`).
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
//! **The kernel's part**, all of it, and the policy none of it:
//!
//! * a deadline on the kernel's clock, moved by each feed, that a task of its
//!   own checks once a second; past it, the report (FX-9008) naming who armed
//!   it, its timeout and how long since the last feed, and then
//!   `ferrix.panic`'s halt or reset;
//! * on x86-64, the chipset's TCO timer (`arch::x86_64::tco`) reloaded by the
//!   same feed, so that a kernel too stuck to report -- every processor
//!   masked, the scheduler gone, a report that itself never ends -- is reset
//!   by hardware [`HARDWARE_MARGIN`] seconds after the deadline;
//! * at a report, the timer cut short ([`on_report`]): whatever the report
//!   does next, the machine resets within the timer's panic count, which is
//!   `ferrix.panic=reset`'s fallback;
//! * at each feed, the pstore region written back from the caches, so that
//!   the record a hardware reset finds in RAM is at most one feed old.
//!
//! The timeout, whether to arm at all and what must be alive for the feed to
//! come are the opener's. The interface is typed and knows no Linux number:
//! `/dev/watchdog` (`fs::watchdog`, the load) turns `open`, `write`, `ioctl`
//! and the last release into the calls below.
//!
//! Off unless asked for: without `ferrix.watchdog=on` no node exists, the TCO
//! is not touched and no task runs.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;

use crate::console::{self, println};
use crate::panic::{catalog, fatal};
use crate::{sched, timer};

/// The command-line option, and the one value that asks for it.
pub const OPTION: &str = "ferrix.watchdog";
const ON: &str = "on";

/// The timeout an open arms with until one is set, as Linux's softdog's.
const DEFAULT_SECONDS: u32 = 60;

/// The longest timeout a program may set: ten minutes.
pub const MAX_SECONDS: u32 = 600;

/// How long after a missed deadline the hardware resets the machine, for a
/// kernel that could not report it: long enough for the report and
/// `ferrix.panic=reset`'s wait to finish first.
pub const HARDWARE_MARGIN: u32 = 30;

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

/// The hardware behind the deadline, where the architecture has one.
#[cfg(target_arch = "x86_64")]
mod hardware {
    pub(super) use crate::arch::x86_64::tco::{
        arm, armed, claimed, feed, halt, panic_backstop, panic_seconds, previous_reset,
    };

    /// Take the TCO, and say what was found.
    pub(super) fn claim() {
        use crate::arch::x86_64::tco::{self, Claimed};
        use crate::console::println;
        match tco::claim() {
            Claimed::Taken {
                base,
                version,
                previous_reset,
            } => {
                println!("  watchdog TCO version {version} at I/O {base:#x}, halted until opened");
                if previous_reset {
                    println!(
                        "  watchdog the TCO reset the previous boot: its feed stopped (SECOND_TO_STS, cleared)"
                    );
                }
            }
            Claimed::Absent => {
                println!("  watchdog no TCO of a known identity: the deadline alone");
            }
            Claimed::Refused(why) => {
                println!("  watchdog the TCO is left alone: {why}; the deadline alone");
            }
        }
    }
}

/// No hardware behind the deadline on this architecture.
#[cfg(not(target_arch = "x86_64"))]
mod hardware {
    pub(super) fn claim() {
        crate::console::println!(
            "  watchdog no hardware timer on this architecture: the deadline alone"
        );
    }
    pub(super) fn claimed() -> bool {
        false
    }
    pub(super) fn arm(_seconds: u32) -> Result<u16, &'static str> {
        Err("no hardware timer")
    }
    pub(super) fn armed() -> bool {
        false
    }
    pub(super) fn feed() {}
    pub(super) fn halt() {}
    pub(super) fn panic_backstop() -> bool {
        false
    }
    pub(super) const fn panic_seconds() -> u64 {
        0
    }
    pub(super) fn previous_reset() -> bool {
        false
    }
}

/// Read `ferrix.watchdog` from the loader's command line: once, at boot.
/// Exactly `on` asks for it; anything else is said and taken as off.
pub fn read_option(view: &BootView<'_>) {
    match view.option(OPTION) {
        None => {}
        Some(ON) => ASKED.store(true, Ordering::Release),
        Some(other) => println!("  watchdog {OPTION}={other} is not understood; it is off"),
    }
}

/// Take the hardware and start the task, if asked: once, after the device
/// nodes are published and the scheduler runs, before init. Says what it
/// does; with no task to look at the deadline the device is not served.
pub fn start() {
    if !ASKED.load(Ordering::Acquire) {
        return;
    }
    hardware::claim();
    if sched::spawn("watchdog", look_forever, 0, ferrix_sched::NICE_0_WEIGHT).is_err() {
        hardware::halt();
        println!("  watchdog no task to look at the deadline: /dev/watchdog is not served");
        return;
    }
    SERVED.store(true, Ordering::Release);
    println!(
        "  watchdog {OPTION}={ON}: /dev/watchdog is served; once opened, a feed that does not come within its timeout is reported{}",
        if hardware::claimed() {
            ", and the TCO resets a machine too stuck to report it"
        } else {
            ""
        }
    );
}

/// Whether `/dev/watchdog` is served on this boot.
pub fn served() -> bool {
    SERVED.load(Ordering::Acquire)
}

/// Whether the hardware reset the boot before this one, for
/// `WDIOC_GETBOOTSTATUS`.
pub fn previous_reset() -> bool {
    hardware::previous_reset()
}

/// Whether a hardware timer stands behind the deadline.
pub fn hardware() -> bool {
    hardware::claimed()
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
    for (slot, half) in OPENER_NAME.iter().zip(bytes.chunks_exact(8)) {
        let mut eight = [0_u8; 8];
        eight.copy_from_slice(half);
        slot.store(u64::from_le_bytes(eight), Ordering::Relaxed);
    }
    arm();
    Ok(())
}

/// Arm, or move an armed deadline to a full timeout from now, and the
/// hardware with it.
fn arm() {
    let now = timer::now_nanos();
    LAST_FEED.store(now, Ordering::Relaxed);
    DEADLINE.store(deadline_from(now), Ordering::Release);
    if hardware::claimed() {
        let seconds = TIMEOUT
            .load(Ordering::Relaxed)
            .saturating_add(HARDWARE_MARGIN);
        if let Err(why) = hardware::arm(seconds) {
            println!("  watchdog the TCO did not arm ({why}): the deadline alone");
        }
    }
}

/// A full timeout from `now`, never 0.
fn deadline_from(now: u64) -> u64 {
    let length = u64::from(TIMEOUT.load(Ordering::Relaxed)).saturating_mul(1_000_000_000);
    now.saturating_add(length).max(1)
}

/// Feed: move an armed deadline to a full timeout from now, reload the
/// hardware, and write the pstore region back from the caches. Never arms a
/// disarmed one. Whether it was armed.
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
        hardware::feed();
        // A hardware reset does not write the caches back: the record it
        // leaves in RAM is the one as of this feed.
        let _ = console::pstore::write_back();
    }
    fed
}

/// Set the timeout to `seconds`, 1 to [`MAX_SECONDS`], and re-arm an armed
/// one with it: the timeout set, or `None` for one out of range.
pub fn set_timeout(seconds: u32) -> Option<u32> {
    if !(1..=MAX_SECONDS).contains(&seconds) {
        return None;
    }
    TIMEOUT.store(seconds, Ordering::Relaxed);
    if armed() {
        arm();
    }
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
/// -- and halt the hardware, so the next system is not reset by a count it
/// never started. Nothing while disarmed.
pub fn stop_for_shutdown(why: &str) {
    let was = DEADLINE.swap(0, Ordering::AcqRel) != 0;
    let hardware = hardware::armed();
    hardware::halt();
    if was || hardware {
        println!("  watchdog disarmed for {why}");
    }
}

/// Disarm, naming who and why.
fn disarm(pid: u32, why: &str) {
    if DEADLINE.swap(0, Ordering::AcqRel) != 0 {
        hardware::halt();
        println!("  watchdog process {pid} {why}: disarmed");
    }
}

/// The first failure report has begun: cut the hardware's count short, so
/// that the machine resets whatever the report does next -- a lock it waits
/// on, a fault in it, a reset that does not take. Under
/// `ferrix.panic=reset`, or with the watchdog armed (which makes a halt a
/// reset in any case); nothing otherwise, and nothing without a claimed
/// timer. Port writes only, from the panic path: no lock, no allocation.
pub(crate) fn on_report(resetting: bool) -> Option<u64> {
    if !(resetting || armed() || hardware::armed()) {
        return None;
    }
    hardware::panic_backstop().then(hardware::panic_seconds)
}

/// The panic path's reset is about to be made: halt the hardware, whose
/// count [`on_report`] set to cover the report, not the next boot. Port
/// writes only.
pub(crate) fn before_reset() {
    hardware::halt();
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

/// The deadline passed: report it.
fn expired(now: u64) -> ! {
    let pid = OPENER.load(Ordering::Relaxed);
    let mut name = [0_u8; 16];
    for (half, to) in OPENER_NAME.iter().zip(name.chunks_exact_mut(8)) {
        to.copy_from_slice(&half.load(Ordering::Relaxed).to_le_bytes());
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
