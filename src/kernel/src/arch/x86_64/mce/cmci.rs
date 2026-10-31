//! Corrected machine check interrupts (`docs/MACHINE-CHECK-CMCI.md`).
//!
//! Each processor's poll task claims, on its first round, the banks it can
//! be interrupted for (`MCi_CTL2.CMCI_EN`, [`claim`], `L.x86_64.249`) and
//! points its local APIC's CMCI entry at [`apic::CMCI_VECTOR`]. The
//! interrupt masks the entry and wakes that task ([`on_cmci`],
//! `L.x86_64.250`), which runs its round at once and unmasks the entry
//! after it ([`after_round`]); a storm leaves the entry masked and the
//! round once a second until a round finds nothing ([`Storm`],
//! `L.x86_64.251`). `ferrix.mce.cmci=off` claims nothing (`L.x86_64.252`).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::super::{apic, cpu};
use super::{MAX_BANKS, PROCESSORS, Proc, decode, poll_takes, read_bank};
use crate::console::println;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;

/// `IA32_MC0_CTL2`; bank `i`'s is this plus `i`.
const MC0_CTL2: u32 = 0x280;
/// `MCi_CTL2`: the bank signals CMCI.
pub(super) const CMCI_EN: u64 = 1 << 30;
/// `MCi_CTL2`: the error count threshold.
pub(super) const THRESHOLD: u64 = 0x7fff;
/// `MCG_CAP`: the processor has CMCI.
const CMCI_P: u64 = 1 << 10;
/// The lowest highest-LVT index a local APIC with the CMCI entry names.
const CMCI_LVT: u32 = 6;
/// The kernel option that turns CMCI off.
pub(super) const OPTION: &str = "ferrix.mce.cmci";
/// The check builds' option that holds the masked window open
/// ([`hold_window`]).
pub(super) const WINDOW_OPTION: &str = "ferrix.mce.cmci-window";
/// Interrupt-driven rounds within one second past which the entry stays
/// masked (Linux's `CMCI_STORM_THRESHOLD`).
pub(super) const STORM_ROUNDS: u32 = 15;
/// A second, the storm's window and its round's period, in nanoseconds.
pub(super) const SECOND: u64 = 1_000_000_000;

/// Whether `ferrix.mce.cmci` leaves CMCI on.
static ON: AtomicBool = AtomicBool::new(true);
/// Whether [`on_cmci`] is registered on the vector.
static REGISTERED: AtomicBool = AtomicBool::new(false);
/// Each processor's claimed banks, a bit each.
static OWNED: [AtomicU32; PROCESSORS] = [const { AtomicU32::new(0) }; PROCESSORS];
/// Each processor's wake from its CMCI, taken by its poll task.
static KICK: [AtomicBool; PROCESSORS] = [const { AtomicBool::new(false) }; PROCESSORS];
/// Each processor's poll task waits here between rounds.
static WAKE: [WaitQueue; PROCESSORS] = [const { WaitQueue::new() }; PROCESSORS];
/// Each processor's CMCI taken while it was not in service, its wake left
/// to its return ([`returned`]).
static HELD: [AtomicBool; PROCESSORS] = [const { AtomicBool::new(false) }; PROCESSORS];
/// Each processor's CMCIs whose wake was held for its return.
static HELDS: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
/// Each processor's CMCIs taken.
static INTERRUPTS: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
/// Each processor's storms begun.
static STORMS: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
/// Makes a bank's read, write and read-back one step for the processors
/// that share it, so exactly one claims it (`docs/MACHINE-CHECK-CMCI.md`
/// §3.1).
static CLAIM: SpinLock<()> = SpinLock::new(());

/// Bank `bank`'s `MCi_CTL2`.
const fn ctl2(bank: usize) -> u32 {
    MC0_CTL2 + bank as u32
}

/// `ferrix.mce.cmci`, from `start`: `off` turns CMCI off, said; any other
/// value is not understood, said, and CMCI stays on.
pub(super) fn read_option(asked: Option<&str>) {
    match asked {
        None => {}
        Some("off") => {
            ON.store(false, Ordering::Relaxed);
            println!("mce: {OPTION}=off: no bank claimed, the poll alone");
        }
        Some(other) => println!("mce: {OPTION}={other} is not understood; cmci used"),
    }
}

/// Seconds the first interrupt-driven round holds the masked window open
/// before its unmask, from `ferrix.mce.cmci-window`; 0 for none. Check
/// builds only: what test-mce-cmci's window mode injects into, so that
/// the look after the unmask is what finds the record (`L.x86_64.253`).
#[cfg(ferrix_checks = "on")]
static WINDOW: AtomicU64 = AtomicU64::new(0);

/// `ferrix.mce.cmci-window=<seconds>` (1 to 60), check builds only; said.
#[cfg(ferrix_checks = "on")]
pub(super) fn read_window(asked: Option<&str>) {
    let Some(seconds) = asked.and_then(|value| value.parse::<u64>().ok()) else {
        return;
    };
    let seconds = seconds.clamp(1, 60);
    WINDOW.store(seconds, Ordering::Relaxed);
    println!("mce: {WINDOW_OPTION}={seconds}: the first woken round holds its window open");
}

/// A product build holds no window and reads no option.
#[cfg(not(ferrix_checks = "on"))]
pub(super) const fn read_window(_asked: Option<&str>) {}

/// Hold processor `index`'s masked window open once, where the check
/// build's option asks: said, so the gate injects into it.
#[cfg(ferrix_checks = "on")]
fn hold_window(index: usize) {
    let seconds = WINDOW.swap(0, Ordering::Relaxed);
    if seconds > 0 {
        println!("mce: cpu {index} cmci window held {seconds} s");
        crate::sched::sleep_for(seconds.saturating_mul(SECOND));
    }
}

/// A product build holds no window.
#[cfg(not(ferrix_checks = "on"))]
const fn hold_window(_index: usize) {}

/// Register [`on_cmci`] on the vector, once, on the boot processor before
/// any poll task runs; a refusal is said and no processor claims a bank.
pub(super) fn register() {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    match crate::irq::register(apic::cmci_irq(), on_cmci) {
        Ok(()) => REGISTERED.store(true, Ordering::Release),
        Err(why) => println!("mce: no cmci: {why}"),
    }
}

/// What one bank's claim found (`L.x86_64.249`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Claim {
    /// A read or the write was refused: not claimed, counted.
    Refused,
    /// `CMCI_EN` was already set: another processor's, or firmware's.
    Taken,
    /// The write did not stick: the bank cannot interrupt.
    Unsupported,
    /// Claimed: `CMCI_EN` read back set.
    Claimed,
}

/// One bank's claim through `read` and `write` (its `MCi_CTL2`'s), as the
/// design's steps 1-4 say; the caller holds [`CLAIM`].
pub(super) fn claim_bank(
    mut read: impl FnMut() -> Option<u64>,
    write: impl FnOnce(u64) -> bool,
) -> Claim {
    let Some(was) = read() else {
        return Claim::Refused;
    };
    if was & CMCI_EN != 0 {
        return Claim::Taken;
    }
    if !write((was & !THRESHOLD) | CMCI_EN | 1) {
        return Claim::Refused;
    }
    match read() {
        None => Claim::Refused,
        Some(now) if now & CMCI_EN != 0 => Claim::Claimed,
        Some(_) => Claim::Unsupported,
    }
}

/// Whether this processor may claim: Intel's rules, `CMCI_P`, the CMCI LVT
/// entry, the option on and the handler registered.
pub(super) fn offered(proc: Proc) -> bool {
    ON.load(Ordering::Relaxed)
        && REGISTERED.load(Ordering::Acquire)
        && matches!(proc.vendor, decode::Vendor::Intel)
        && cpu::read_msr_checked(super::MCG_CAP).is_some_and(|cap| cap & CMCI_P != 0)
        && apic::max_lvt() >= CMCI_LVT
}

/// Processor `proc`'s claim, from its poll task on it before its first
/// round: each bank through [`claim_bank`] under [`CLAIM`], then the entry
/// unmasked where anything was claimed; said where anything was claimed
/// or refused.
pub(super) fn claim(proc: Proc) {
    if !offered(proc) {
        return;
    }
    let mut owned = 0_u32;
    let mut refused = 0_u32;
    {
        let _held = CLAIM.lock();
        for bank in 0..proc.banks.min(MAX_BANKS) {
            let found = claim_bank(
                || cpu::read_msr_checked(ctl2(bank)),
                // SAFETY: (SYSREG) `MCi_CTL2` with `CMCI_EN` and threshold 1,
                // its other bits as read: the bank signals CMCI on its next
                // record, and nothing else changes (SDM vol. 3B 16.5.1).
                |value| unsafe { cpu::write_msr_checked(ctl2(bank), value) },
            );
            match found {
                Claim::Claimed => owned |= 1 << bank,
                Claim::Refused => refused += 1,
                Claim::Taken | Claim::Unsupported => {}
            }
        }
    }
    if let Some(slot) = OWNED.get(proc.index) {
        slot.store(owned, Ordering::Release);
    }
    if refused > 0 {
        println!(
            "mce: cpu {} refused {refused} MCi_CTL2 accesses",
            proc.index
        );
    }
    if owned != 0 {
        println!("mce: cpu {} cmci on banks {owned:#x}", proc.index);
        apic::unmask_cmci();
    }
}

/// The CMCI (`L.x86_64.250`): mask the entry, count it, and on a processor
/// in service wake its poll task. Reads no bank; takes no lock but the wait
/// queue's.
///
/// In service means its park state is `ACTIVE_STATE`. The wake is held
/// ([`HELD`]) before that state is read, and taken back by a swap only when
/// it is active: so a return that stores the state after the read finds the
/// hold in [`returned`], and exactly one of the two wakes the task. A wake
/// that lands as the processor begins to park is the period timer firing
/// then, as today.
fn on_cmci(_irq: u32) {
    apic::mask_cmci();
    let Some(index) = crate::smp::this_cpu().map(|cpu| cpu.logical) else {
        return;
    };
    if let Some(count) = INTERRUPTS.get(index) {
        let _ = count.fetch_add(1, Ordering::Relaxed);
    }
    let Some(held) = HELD.get(index) else {
        return;
    };
    held.store(true, Ordering::SeqCst);
    if crate::sched::park_state(index) == crate::sched::ACTIVE_STATE
        && held.swap(false, Ordering::SeqCst)
    {
        kick(index);
    } else if let Some(count) = HELDS.get(index) {
        // Held, or taken first by a return that raced this read: either
        // way the return woke, or will wake, the task.
        let _ = count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Hold processor `index`'s wake for its return, then take the hold back if
/// it is in service (its park state active); whether it was taken back --
/// false leaves the return to wake the poll task. The handler's pattern
/// (`on_cmci`): the `SeqCst` store and swap order the hold against the park
/// state, whose return stores it on this processor or, in the Cancelled
/// undo, on the waiter's, so exactly one of this and [`returned`] finds the
/// hold set.
fn take_back_hold(index: usize) -> bool {
    let Some(held) = HELD.get(index) else {
        return false;
    };
    held.store(true, Ordering::SeqCst);
    crate::sched::park_state(index) == crate::sched::ACTIVE_STATE
        && held.swap(false, Ordering::SeqCst)
}

/// Processor `index` is back in service (`sched`'s park and its undo, through
/// `arch::returned_to_service`): a CMCI it took meanwhile wakes its poll task
/// now, whose round unmasks the entry.
pub(super) fn returned(index: usize) {
    if HELD
        .get(index)
        .is_some_and(|held| held.swap(false, Ordering::SeqCst))
    {
        kick(index);
    }
}

/// Wake processor `index`'s poll task for a round at once.
fn kick(index: usize) {
    if let (Some(kick), Some(wake)) = (KICK.get(index), WAKE.get(index)) {
        kick.store(true, Ordering::Release);
        wake.wake_all();
    }
}

/// Wait up to `nanos` for processor `index`'s CMCI; whether it was the
/// reason.
pub(super) fn wait(index: usize, nanos: u64) -> bool {
    let (Some(kick), Some(wake)) = (KICK.get(index), WAKE.get(index)) else {
        crate::sched::sleep_for(nanos);
        return false;
    };
    let deadline = crate::timer::now_nanos().saturating_add(nanos);
    wake.wait_trusting_until(|| kick.swap(false, Ordering::AcqRel), deadline)
}

/// What a round leaves the entry as (`L.x86_64.251`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum After {
    /// Unmasked, the poll's own period next.
    Unmask,
    /// A storm began: masked, a round a second.
    Began,
    /// The storm goes on: masked, a round a second.
    Storming,
    /// The storm is over: unmasked, the poll's own period next.
    Ended,
}

/// A processor's storm rule: its interrupt-driven rounds in the current
/// one-second window, and whether it is storming.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Storm {
    /// When the window began, in nanoseconds since boot.
    window: u64,
    /// Interrupt-driven rounds in it.
    rounds: u32,
    /// Whether the entry is left masked.
    storming: bool,
}

impl Storm {
    /// A round at `now` has ended, woken by the CMCI (`woken`) or by the
    /// period, having found a record (`found`) or not.
    pub(super) fn after(&mut self, now: u64, woken: bool, found: bool) -> After {
        if self.storming {
            if found {
                return After::Storming;
            }
            *self = Self::default();
            return After::Ended;
        }
        if !woken {
            return After::Unmask;
        }
        if now.saturating_sub(self.window) >= SECOND || self.rounds == 0 {
            self.window = now;
            self.rounds = 0;
        }
        self.rounds = self.rounds.saturating_add(1);
        if self.rounds > STORM_ROUNDS {
            self.storming = true;
            return After::Began;
        }
        After::Unmask
    }

    /// Whether the entry is left masked.
    pub(super) const fn storming(self) -> bool {
        self.storming
    }
}

/// After processor `proc`'s round: the storm rule, said where a storm
/// begins or ends, then, where the entry is to be unmasked and the
/// processor is in service, the unmask and one more look at each claimed
/// bank, a record there kicking the next round at once (the masked window,
/// `docs/MACHINE-CHECK-CMCI.md` §3.4). Nothing where nothing is claimed.
pub(super) fn after_round(proc: Proc, storm: &mut Storm, woken: bool, found: bool) {
    let owned = OWNED
        .get(proc.index)
        .map_or(0, |owned| owned.load(Ordering::Acquire));
    if owned == 0 {
        return;
    }
    match storm.after(crate::timer::now_nanos(), woken, found) {
        After::Storming => return,
        After::Began => {
            if let Some(count) = STORMS.get(proc.index) {
                let _ = count.fetch_add(1, Ordering::Relaxed);
            }
            println!("mce: cpu {} cmci storm: polling every 1 s", proc.index);
            return;
        }
        After::Ended => println!("mce: cpu {} cmci storm over", proc.index),
        After::Unmask if woken => hold_window(proc.index),
        After::Unmask => {}
    }
    // In service as the handler decides it, by the park state, and by the
    // same hold: a round that ends as its processor parks leaves the unmask
    // to the return, whose kick runs a round that makes it (ledger 5418 (1)).
    if !take_back_hold(proc.index) {
        return;
    }
    apic::unmask_cmci();
    // Only a record the round would take: one it leaves is the machine
    // check's, and kicking on it would spin the task.
    let pending = (0..proc.banks.min(MAX_BANKS))
        .filter(|&bank| owned & (1 << bank) != 0)
        .any(|bank| poll_takes(proc, read_bank(bank, 1)));
    if pending && let Some(kick) = KICK.get(proc.index) {
        kick.store(true, Ordering::Release);
    }
}

/// The wait before processor `index`'s next round: a second in a storm,
/// else the poll's `period` in nanoseconds.
pub(super) const fn next_wait(storm: Storm, period: u64) -> u64 {
    if storm.storming() { SECOND } else { period }
}

/// `/proc/ferrix-mce`'s CMCI line for processor `index`, after its counts'
/// line: `cmci<n> interrupts <n> held <n> storms <n> owned <mask>`, named apart from
/// the counts' `cpu<n>` so a reader of that line finds it alone.
pub(super) fn render(out: &mut impl core::fmt::Write, index: usize) -> core::fmt::Result {
    let load = |counts: &[AtomicU64; PROCESSORS]| {
        counts
            .get(index)
            .map_or(0, |count| count.load(Ordering::Relaxed))
    };
    writeln!(
        out,
        "cmci{index} interrupts {} held {} storms {} owned {:#x}",
        load(&INTERRUPTS),
        load(&HELDS),
        load(&STORMS),
        OWNED
            .get(index)
            .map_or(0, |owned| owned.load(Ordering::Relaxed))
    )
}
