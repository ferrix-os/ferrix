//! MEASUREMENT ONLY (os07-prof, `docs/BOARD-BENCH.md` B6 step 6; never
//! lands): where one direction of a `channel_write_read` round trip spends
//! its time on ARMv7-A, stamped with the PMU's cycle counter.
//!
//! The port of the x86-64 timing build `po7/prof` (the IPC skill's §2,
//! "Profiling"; `docs/OPAQUE-KERNEL.md` §9.10). A direction runs from one
//! side's `svc` into `channel_write_read` (0x1013) to the other side's next
//! one. Every [`Point`] along it reads `PMCCNTR` after an `isb`, as
//! `board-bench.h` does. Only a direction whose stamps came in exactly the
//! order of [`Point`] (a point marked optional may be passed over), with no
//! switch barrier issued and no other exception taken in it, is kept: a
//! timer interrupt, a switch to a third task and anything else that leaves
//! the path drop out and are counted by why. So every span is of the same
//! path, and the spans add up to the direction.
//!
//! Per processor: the open direction's state and the kept directions are
//! kept per processor in fixed arrays, written with plain loads and stores
//! by that processor alone, with interrupts masked or on its own way through
//! a system call. Nothing on the path allocates or locks.
//!
//! Windows: nothing is stamped until a program opens a window with the
//! measurement-only call [`CALL`] (`ipc-bench`'s PMU mode opens one around
//! its `call` series and one around `domain-call`), and the window's slot
//! says which series a direction belongs to. The same call prints the
//! table. Ablations ([`Ablation`]) also act only inside a window, so a boot
//! with them pays them only for the series measured.
//!
//! Per span the table gives the mean and p90 over the directions kept, the
//! mean of the samples up to p90 (`m90`, the x86 build's figure), and that
//! net of the stamp's own cost, measured as each window opens.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::console::println;

/// The measurement-only native call: `r0` the operation ([`OP_START`],
/// [`OP_STOP`], [`OP_PRINT`]), `r1` the slot for a start. Answered in the
/// ARMv7-A trap path before anything else looks at the call; unused on
/// main's ABI (`ferrix_native_abi::nr` ends at `0x105E` below `LAST`).
pub(crate) const CALL: u32 = 0x1FF0;
/// Open a window on slot `r1`: forget the slot, measure the stamp, start.
pub(crate) const OP_START: u32 = 1;
/// Close the window.
pub(crate) const OP_STOP: u32 = 2;
/// Print every slot that kept anything.
pub(crate) const OP_PRINT: u32 = 3;

/// The points, in the order a direction passes them on ARMv7-A's general
/// path (there is no fast path there: `FAST_WRITE_READ` is false).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Point {
    /// `ferrix_trap_entry` of an `svc` with `r7` = 0x1013.
    Entry = 0,
    /// `arch::system_call` entered.
    EClassify,
    /// `filter_system_call` answered.
    EFilter,
    /// Interrupts open, about to call `trap::system_call`.
    EIrqOn,
    /// `trap::system_call`: `call_entered`.
    ECallEntered,
    /// `native_call`: `sched::current()`.
    ECurrent,
    /// `channel_write_read` entered.
    Dispatched,
    /// The handle looked up.
    LookedUp,
    /// `write_small`: the message is in the peer's inbox.
    Written,
    /// `wake_all_with`: the waiters drained.
    WDrained,
    /// `wake_with`: `wake_onto` answered (optional: no waiter, no wake).
    WOnto,
    /// `wake_at_home`: inserted (optional: `wake_onto` placed it).
    WInserted,
    /// `write_small` returned from the wake.
    Woken,
    /// `receive_words`: `read_small` found nothing.
    ReadEmpty,
    /// `wait_sliced`: listed, marked blocked, last look made.
    Listed,
    /// `choose_next`: the run queue locked.
    Locked,
    /// `choose_next`: `now_nanos`.
    ANow,
    /// `account_in`: `account_load`.
    ALoad,
    /// `account_in`: charge, runtime, `update_curr` (optional: no delta).
    ACurr,
    /// `account_in` returned.
    AAccount,
    /// `wake_sleepers`.
    Accounted,
    /// Detached and picked.
    Picked,
    /// `switch_chosen`: bookkeeping and `arm_timer`, before the space.
    Booked,
    /// `AddressSpace::install`: domains recorded, processor joined.
    Domain,
    /// `TTBR0` written (with `TTBCR` and its `isb`).
    Ttbr0,
    /// `TLBIASID`, `dsb`, `isb`.
    Tlbi,
    /// `entered_space`: the speculation record (and barrier).
    Barrier,
    /// The rest of the install: the trip count, the processor leaving.
    Spaced,
    /// Save: `TPIDRURW` read (`TPIDRURO` is the record's since 3b, F-66).
    STls,
    /// Save: USR's banked `sp` and `lr`.
    SBanked,
    /// Save: VFP, lazy since §9.15: for a task that has never used VFP
    /// (every soft-float bench program) only the look at its `vfp` bit.
    Saved,
    /// Restore: `TPIDRURO` and `TPIDRURW` written.
    RTls,
    /// Restore: banked `sp` and `lr`.
    RBanked,
    /// Restore: VFP, lazy since §9.15: for a task that has never used VFP,
    /// the look at this core's `FPEXC.EN` record, and a scrub only if it
    /// was set.
    Restored,
    /// `switch_to` returned, in the other context.
    Switched,
    /// `finish_switch`.
    Finished,
    /// `wait_sliced`: back from `block`, deadline taken, unqueued.
    Unblocked,
    /// `receive_words`: `read_small` found the message.
    Read,
    /// `dispatch_write_read`: `record_call`.
    Recorded,
    /// `trap::system_call`: regroup, throttle, `call_left`.
    Left,
    /// `trap::dispatch`: interrupts masked, the answer stored.
    Stored,
    /// `ferrix_trap_entry`: `trap::dispatch`'s tail, about to return.
    Exit,
}

/// How many points.
pub(crate) const POINTS: usize = 42;

const _: () = assert!(Point::Exit as usize == POINTS - 1);

/// Names for the print, in order: what the span *ending* at the point
/// covers.
const NAMES: [&str; POINTS] = [
    "user+rfe+svc vector+save frame",
    "entry: trap::dispatch+classify",
    "entry: filter_system_call",
    "entry: decode+regs+vdead+irq on",
    "system_call: call_entered",
    "dispatch+native_call current()",
    "native_call: thread/process",
    "handle lookup",
    "write_small: inbox+note+observ",
    "wake: fence+parked+drain",
    "wake: wake_onto",
    "wake: home lock+EEVDF insert",
    "wake: kick+irq restore",
    "read_small empty",
    "wait: list+preempt off+mark+look",
    "schedule: irq off+check+rq lock",
    "choose: now_nanos",
    "account: load",
    "account: charge+update_curr",
    "account: group share+measure",
    "choose: wake_sleepers",
    "choose: detach+pick (EEVDF)",
    "switch_chosen: bookkeeping+timer",
    "install: domains+join",
    "TTBR0 write",
    "TLBIASID",
    "speculation record",
    "install rest",
    "save: TPIDRURW read",
    "save: banked sp/lr",
    "save: VFP (lazy: vfp bit look)",
    "restore: TPIDRURO+RW write",
    "restore: banked sp/lr",
    "restore: VFP (lazy: EN look)",
    "switch_to",
    "finish_switch",
    "wait: unblock+unqueue+slept",
    "wait: ready+read_small",
    "words+record_call",
    "system_call: regroup+call_left",
    "irq off+store words",
    "dispatch tail+way out (Rust)",
];

/// Whether a point may be passed over: a wake that found no waiter, a wake
/// `wake_onto` placed itself, and an account that had no time to charge.
const fn optional(point: usize) -> bool {
    point == Point::WOnto as usize
        || point == Point::WInserted as usize
        || point == Point::ACurr as usize
}

/// Processors with a state of their own. The board has two; `nosmp` runs
/// one.
const CPUS: usize = 2;
/// Windows: `call` (client and server in two speculation domains) and
/// `domain-call` (one domain).
const SLOTS: usize = 2;
/// Slot names.
const SLOT_NAMES: [&str; SLOTS] = ["call", "domain-call"];
/// Directions kept per processor and slot: the last [`RING`] of a window.
const RING: usize = 1024;
/// No window.
const NONE: u8 = u8::MAX;
/// No direction open.
const BROKEN: u8 = u8::MAX;

/// The open direction: each span's cycles so far.
static OPEN: [[AtomicU32; POINTS]; CPUS] =
    [const { [const { AtomicU32::new(0) }; POINTS] }; CPUS];
/// The next point expected, or [`BROKEN`].
static NEXT: [AtomicU8; CPUS] = [const { AtomicU8::new(BROKEN) }; CPUS];
/// The last stamp's cycles.
static LAST: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];
/// A direction ended at [`Point::Exit`] and waits for the next entry.
static ENDED: [AtomicBool; CPUS] = [const { AtomicBool::new(false) }; CPUS];
/// The open direction issued a switch barrier.
static TAINTED: [AtomicBool; CPUS] = [const { AtomicBool::new(false) }; CPUS];
/// The open direction (or the user span before its end) took another
/// exception.
static INTERRUPTED: [AtomicBool; CPUS] = [const { AtomicBool::new(false) }; CPUS];
/// Which side the open direction started on: 0 or 1 by the order the
/// window met their roots, 2 for any other.
static SIDE: [AtomicU8; CPUS] = [const { AtomicU8::new(2) }; CPUS];
/// The two sides' roots (`TTBR0`'s low word), in the order met.
static ROOTS: [[AtomicU32; 2]; CPUS] = [const { [const { AtomicU32::new(0) }; 2] }; CPUS];

/// The directions kept.
static SAMPLES: [[[[AtomicU32; POINTS]; RING]; SLOTS]; CPUS] =
    [const { [const { [const { [const { AtomicU32::new(0) }; POINTS] }; RING] }; SLOTS] }; CPUS];
/// Their sides.
static SAMPLE_SIDE: [[[AtomicU8; RING]; SLOTS]; CPUS] =
    [const { [const { [const { AtomicU8::new(0) }; RING] }; SLOTS] }; CPUS];
/// Directions kept, in all.
static KEPT: [[AtomicU64; SLOTS]; CPUS] = [const { [const { AtomicU64::new(0) }; SLOTS] }; CPUS];
/// Directions dropped: out of order.
static BROKE: [[AtomicU64; SLOTS]; CPUS] = [const { [const { AtomicU64::new(0) }; SLOTS] }; CPUS];
/// Directions dropped: a switch barrier.
static TAINTS: [[AtomicU64; SLOTS]; CPUS] = [const { [const { AtomicU64::new(0) }; SLOTS] }; CPUS];
/// Directions dropped: another exception taken.
static INTERRUPTS: [[AtomicU64; SLOTS]; CPUS] =
    [const { [const { AtomicU64::new(0) }; SLOTS] }; CPUS];
/// Where directions broke: the point that came instead.
static BROKE_AT: [[AtomicU64; POINTS]; SLOTS] =
    [const { [const { AtomicU64::new(0) }; POINTS] }; SLOTS];
/// Each slot's stamp cost, cycles, measured as its window opened.
static OVERHEAD: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
/// `PMCR` as each slot's window opened.
static PMCR_AT: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

/// The window open, or [`NONE`].
static ACTIVE: AtomicU8 = AtomicU8::new(NONE);
/// Stamping: a window is open and `prof=off` was not given.
static STAMPING: AtomicBool = AtomicBool::new(false);
/// `prof=off`: windows open, ablations act, nothing is stamped.
static STAMPS_OFF: AtomicBool = AtomicBool::new(false);
/// The ablations asked for at boot.
static ABLATIONS: AtomicU32 = AtomicU32::new(0);
/// [`ABLATIONS`] while a window is open, zero otherwise: the one word an
/// ablation's site reads.
static LIVE: AtomicU32 = AtomicU32::new(0);

/// A piece of the path a boot may leave out or double, inside a window
/// only, so that its cost can be read from `ipc-bench`'s own figures
/// against a boot without it, the stamps off (`prof=off`) on both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Ablation {
    /// `prof.extra-flush`: a second `TLBIASID` (with its `dsb`, `isb`) as
    /// each 0x1013 returns, so the user span pays a second refill of its
    /// translations: the flush and the refill's cost together. Safe.
    ExtraFlush = 1 << 0,
    /// `prof.skip-vfp-UNSAFE`: no VFP save or restore, the lazy look and
    /// scrub included. Leaks one program's `d0`-`d31` and `FPSCR` to the
    /// next (FDP_RIP.2), and a VFP user would trap on a core left with
    /// `EN` clear. Boots one bench run because every program on it is
    /// soft-float (`armv7a-none-eabi`) and touches no VFP register. Since
    /// lazy VFP (§9.15) the spans it removes are only the looks.
    SkipVfp = 1 << 1,
    /// `prof.skip-tls-UNSAFE`: no `TPIDRURW` read at save nor `TPIDRURO`
    /// and `TPIDRURW` write at restore. Leaks one program's thread pointer
    /// and `TPIDRURW` to the next. Boots one bench run because no native
    /// program sets either (`set_tls` unused).
    SkipTls = 1 << 2,
    /// `prof.skip-tls-read-UNSAFE`: the `TPIDRURW` read at save left out,
    /// the writes kept (3b already left out the `TPIDRURO` read): a program
    /// that wrote `TPIDRURW` would lose it. As safe as
    /// [`Ablation::SkipTls`] for the bench.
    SkipTlsRead = 1 << 3,
    /// `prof.skip-cpu-charge-UNSAFE`: `account_in` leaves out the job's
    /// charge (`quota::charge_cpu`: `cpu.stat`, `cpu.max`), keeping the
    /// runtime and `update_curr`. The window's time is missing from the
    /// job's `cpu.stat` and its `cpu.max` is not enforced. Boots. (Leaving
    /// out the whole `account_in` stalled the `call` series under QEMU, so
    /// that is not offered.)
    SkipCpuCharge = 1 << 4,
    /// `prof.skip-spec-record-UNSAFE`: `entered_space` left out of the
    /// install. On the Cortex-A7, whose switch barrier is none, only the
    /// record of the last root and domain goes stale for the window; on a
    /// core that needs `BPIALL` it would leave the barrier out. Boots.
    SkipSpec = 1 << 5,
    /// `prof.skip-audit-UNSAFE`: no `record_call` for 0x1013: the audit
    /// trail loses the window's calls. Boots.
    SkipAudit = 1 << 6,
}

/// Every ablation with its option, for reading and printing.
const ABLATION_OPTIONS: [(Ablation, &str); 7] = [
    (Ablation::ExtraFlush, "prof.extra-flush"),
    (Ablation::SkipVfp, "prof.skip-vfp-UNSAFE"),
    (Ablation::SkipTls, "prof.skip-tls-UNSAFE"),
    (Ablation::SkipTlsRead, "prof.skip-tls-read-UNSAFE"),
    (Ablation::SkipCpuCharge, "prof.skip-cpu-charge-UNSAFE"),
    (Ablation::SkipSpec, "prof.skip-spec-record-UNSAFE"),
    (Ablation::SkipAudit, "prof.skip-audit-UNSAFE"),
];

/// Whether `ablation` acts now: asked for at boot and inside a window.
#[inline(always)]
pub(crate) fn ablate(ablation: Ablation) -> bool {
    LIVE.load(Ordering::Relaxed) & (ablation as u32) != 0
}

/// Read `prof=off` and the ablations, once, on the boot processor, from the
/// loader's line or `bootargs`, and say what was read.
pub(crate) fn read_options(view: &ferrix_bootinfo::BootView<'_>, bootargs: Option<&str>) {
    let args = bootargs.unwrap_or("");
    let off = view
        .option("prof")
        .or_else(|| ferrix_bootinfo::option_in(args, "prof"))
        .is_some_and(|value| value == "off" || value == "0");
    STAMPS_OFF.store(off, Ordering::Relaxed);
    let mut mask = 0_u32;
    for (ablation, name) in ABLATION_OPTIONS {
        if view.flag(name) || ferrix_bootinfo::flag_in(args, name) {
            mask |= ablation as u32;
            println!("  prof     ablation {name} on (inside windows only)");
        }
    }
    ABLATIONS.store(mask, Ordering::Relaxed);
    println!(
        "  prof     MEASUREMENT ONLY timing build (os07-prof): stamps {}, ablations {mask:#x}, \
         call {CALL:#x}",
        if off { "off (prof=off)" } else { "on" },
    );
}

/// Which processor this is, by `MPIDR`'s first affinity level.
#[inline(always)]
fn cpu() -> usize {
    #[cfg(target_arch = "arm")]
    {
        (crate::arch::armv7a_prof::mpidr() & 0xFF) as usize
    }
    #[cfg(not(target_arch = "arm"))]
    {
        0
    }
}

/// The cycle counter, after an `isb`.
#[inline(always)]
fn cycles() -> u32 {
    #[cfg(target_arch = "arm")]
    {
        crate::arch::armv7a_prof::cycles()
    }
    #[cfg(not(target_arch = "arm"))]
    {
        0
    }
}

/// The installed root's low word, which tells the two sides apart.
fn root() -> u32 {
    #[cfg(target_arch = "arm")]
    {
        crate::arch::armv7a_prof::root()
    }
    #[cfg(not(target_arch = "arm"))]
    {
        0
    }
}

/// Stamp `point`.
#[inline(always)]
pub(crate) fn stamp(point: Point) {
    if STAMPING.load(Ordering::Relaxed) {
        stamp_at(point as usize);
    }
}

/// [`stamp`], only where `point` is the next one expected: for a function
/// other paths call too.
#[inline(always)]
pub(crate) fn stamp_soft(point: Point) {
    if STAMPING.load(Ordering::Relaxed) {
        let cpu = cpu();
        if NEXT.get(cpu).is_some_and(|next| next.load(Ordering::Relaxed) == point as u8) {
            stamp_at(point as usize);
        }
    }
}

/// An exception other than a system call was taken on this processor: the
/// open direction, or the user span the next one opens with, is not the
/// path.
#[inline(always)]
pub(crate) fn interrupted() {
    if STAMPING.load(Ordering::Relaxed)
        && let Some(flag) = INTERRUPTED.get(cpu())
    {
        flag.store(true, Ordering::Relaxed);
    }
}

/// A switch barrier was issued on this processor.
pub(crate) fn taint() {
    if STAMPING.load(Ordering::Relaxed)
        && let Some(flag) = TAINTED.get(cpu())
    {
        flag.store(true, Ordering::Relaxed);
    }
}

/// [`stamp`], out of line.
#[inline(never)]
fn stamp_at(point: usize) {
    let cpu = cpu();
    let (Some(next_cell), Some(last), Some(open), Some(ended)) =
        (NEXT.get(cpu), LAST.get(cpu), OPEN.get(cpu), ENDED.get(cpu))
    else {
        return;
    };
    let now = cycles();
    let delta = now.wrapping_sub(last.load(Ordering::Relaxed));
    last.store(now, Ordering::Relaxed);
    if point == 0 {
        if ended.load(Ordering::Relaxed) && next_cell.load(Ordering::Relaxed) == 0 {
            if let Some(slot) = open.first() {
                slot.store(delta, Ordering::Relaxed);
            }
            commit(cpu);
            // The commit's own time is no span's.
            last.store(cycles(), Ordering::Relaxed);
        }
        ended.store(false, Ordering::Relaxed);
        if let Some(flag) = TAINTED.get(cpu) {
            flag.store(false, Ordering::Relaxed);
        }
        if let Some(flag) = INTERRUPTED.get(cpu) {
            flag.store(false, Ordering::Relaxed);
        }
        side_of(cpu);
        next_cell.store(1, Ordering::Relaxed);
        return;
    }
    let next = usize::from(next_cell.load(Ordering::Relaxed));
    if next == usize::from(BROKEN) {
        return;
    }
    if next != point {
        if point > next && (next..point).all(optional) {
            for skipped in open.get(next..point).unwrap_or_default() {
                skipped.store(0, Ordering::Relaxed);
            }
        } else {
            let slot = usize::from(ACTIVE.load(Ordering::Relaxed));
            if let Some(count) = BROKE.get(cpu).and_then(|row| row.get(slot)) {
                count.store(count.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
            }
            if let Some(at) = BROKE_AT.get(slot).and_then(|row| row.get(point)) {
                at.store(at.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
            }
            next_cell.store(BROKEN, Ordering::Relaxed);
            ended.store(false, Ordering::Relaxed);
            return;
        }
    }
    if let Some(slot) = open.get(point) {
        slot.store(delta, Ordering::Relaxed);
    }
    if point == POINTS - 1 {
        ended.store(true, Ordering::Relaxed);
        next_cell.store(0, Ordering::Relaxed);
    } else {
        next_cell.store(point as u8 + 1, Ordering::Relaxed);
    }
}

/// Note which side the direction opening now starts on.
fn side_of(cpu: usize) {
    let (Some(roots), Some(side)) = (ROOTS.get(cpu), SIDE.get(cpu)) else {
        return;
    };
    let root = root();
    let mut found = 2_u8;
    for (index, held) in roots.iter().enumerate() {
        let value = held.load(Ordering::Relaxed);
        if value == 0 {
            held.store(root, Ordering::Relaxed);
        }
        if value == 0 || value == root {
            found = index as u8;
            break;
        }
    }
    side.store(found, Ordering::Relaxed);
}

/// Keep the direction just closed, or count why not.
fn commit(cpu: usize) {
    let slot = usize::from(ACTIVE.load(Ordering::Relaxed));
    let bump = |table: &[[AtomicU64; SLOTS]; CPUS]| {
        if let Some(count) = table.get(cpu).and_then(|row| row.get(slot)) {
            count.store(count.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
    };
    if TAINTED.get(cpu).is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        bump(&TAINTS);
        return;
    }
    if INTERRUPTED.get(cpu).is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        bump(&INTERRUPTS);
        return;
    }
    let Some(kept) = KEPT.get(cpu).and_then(|row| row.get(slot)) else {
        return;
    };
    let n = kept.load(Ordering::Relaxed);
    let index = (n % RING as u64) as usize;
    let (Some(row), Some(open)) = (
        SAMPLES.get(cpu).and_then(|s| s.get(slot)).and_then(|s| s.get(index)),
        OPEN.get(cpu),
    ) else {
        return;
    };
    for (to, from) in row.iter().zip(open) {
        to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
    }
    if let Some(side) = SAMPLE_SIDE
        .get(cpu)
        .and_then(|s| s.get(slot))
        .and_then(|s| s.get(index))
    {
        side.store(
            SIDE.get(cpu).map_or(2, |s| s.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
    }
    kept.store(n + 1, Ordering::Relaxed);
}

/// The measurement-only call: `op` and its argument. Returns what `r0`
/// carries back: 0, or 1 for an operation or slot not understood.
pub(crate) fn control(op: u32, argument: u32) -> u32 {
    match op {
        OP_START => match u8::try_from(argument) {
            Ok(slot) if usize::from(slot) < SLOTS => {
                start(slot);
                0
            }
            _ => 1,
        },
        OP_STOP => {
            stop();
            0
        }
        OP_PRINT => {
            stop();
            print();
            0
        }
        _ => 1,
    }
}

/// Close any window and forget every open direction.
fn stop() {
    STAMPING.store(false, Ordering::Relaxed);
    LIVE.store(0, Ordering::Relaxed);
    ACTIVE.store(NONE, Ordering::Relaxed);
    for cpu in 0..CPUS {
        if let Some(next) = NEXT.get(cpu) {
            next.store(BROKEN, Ordering::Relaxed);
        }
        if let Some(ended) = ENDED.get(cpu) {
            ended.store(false, Ordering::Relaxed);
        }
    }
}

/// Open a window on `slot`.
fn start(slot: u8) {
    stop();
    let index = usize::from(slot);
    for table in [&KEPT, &BROKE, &TAINTS, &INTERRUPTS] {
        for row in table {
            if let Some(count) = row.get(index) {
                count.store(0, Ordering::Relaxed);
            }
        }
    }
    for at in BROKE_AT.get(index).into_iter().flatten() {
        at.store(0, Ordering::Relaxed);
    }
    for roots in &ROOTS {
        for root in roots {
            root.store(0, Ordering::Relaxed);
        }
    }
    #[cfg(target_arch = "arm")]
    {
        crate::arch::armv7a_prof::start_cycle_counter();
        if let Some(pmcr) = PMCR_AT.get(index) {
            pmcr.store(crate::arch::armv7a_prof::pmcr(), Ordering::Relaxed);
        }
    }
    // The stamp's cost: sixteen stamps in order through `stamp_at`, timed
    // together, the p50 of 4096 tries.
    let mut seen = [0_u32; 256];
    let cpu = cpu();
    for _ in 0..4096 {
        if let Some(next) = NEXT.get(cpu) {
            next.store(1, Ordering::Relaxed);
        }
        let begin = cycles();
        for point in 1..=16 {
            stamp_at(point);
        }
        let per = cycles().wrapping_sub(begin) / 16;
        if let Some(cell) = seen.get_mut((per as usize).min(255)) {
            *cell += 1;
        }
    }
    let total: u32 = seen.iter().sum();
    let mut running = 0_u32;
    let mut p50 = 0_u32;
    for (value, &count) in seen.iter().enumerate() {
        running += count;
        if running * 2 >= total {
            p50 = value as u32;
            break;
        }
    }
    if let Some(overhead) = OVERHEAD.get(index) {
        overhead.store(p50, Ordering::Relaxed);
    }
    stop();
    ACTIVE.store(slot, Ordering::Relaxed);
    LIVE.store(ABLATIONS.load(Ordering::Relaxed), Ordering::Relaxed);
    STAMPING.store(!STAMPS_OFF.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Mean, p90 and the mean up to p90 of `values`, sorted in place.
fn figures(values: &mut [u32]) -> (u64, u64, u64) {
    if values.is_empty() {
        return (0, 0, 0);
    }
    values.sort_unstable();
    let n = values.len();
    let sum: u64 = values.iter().map(|&v| u64::from(v)).sum();
    let cut = (n * 900 / 1000).min(n - 1);
    let p90 = values.get(cut).map_or(0, |&v| u64::from(v));
    let low: u64 = values.iter().take(cut + 1).map(|&v| u64::from(v)).sum();
    (sum / n as u64, p90, low / (cut as u64 + 1))
}

/// Print every slot that kept a direction, on every processor.
fn print() {
    let mut scratch = alloc::vec::Vec::new();
    if scratch.try_reserve_exact(RING).is_err() {
        println!("  prof     no memory for the print");
        return;
    }
    println!(
        "  prof     table (MEASUREMENT ONLY, os07-prof): cycles of PMCCNTR per span of one direction \
         of channel_write_read; m90 = mean of the samples up to p90; net = m90 less the stamp"
    );
    for cpu in 0..CPUS {
        for slot in 0..SLOTS {
            print_slot(cpu, slot, &mut scratch);
        }
    }
    // Printed once: a later print shows only the windows opened since.
    for table in [&KEPT, &BROKE, &TAINTS, &INTERRUPTS] {
        for row in table {
            for count in row {
                count.store(0, Ordering::Relaxed);
            }
        }
    }
    println!("  prof     end");
}

/// Copy span `point` (or the whole direction for `None`, or only side
/// `side`'s) of the held samples into `scratch`.
fn gather(
    cpu: usize,
    slot: usize,
    held: usize,
    point: Option<usize>,
    side: Option<u8>,
    scratch: &mut alloc::vec::Vec<u32>,
) {
    scratch.clear();
    let (Some(rows), Some(sides)) = (
        SAMPLES.get(cpu).and_then(|s| s.get(slot)),
        SAMPLE_SIDE.get(cpu).and_then(|s| s.get(slot)),
    ) else {
        return;
    };
    for (row, row_side) in rows.iter().zip(sides).take(held) {
        if side.is_some_and(|side| row_side.load(Ordering::Relaxed) != side) {
            continue;
        }
        let value = match point {
            Some(point) => row.get(point).map_or(0, |v| v.load(Ordering::Relaxed)),
            None => row
                .iter()
                .fold(0_u32, |sum, v| sum.wrapping_add(v.load(Ordering::Relaxed))),
        };
        if scratch.len() < scratch.capacity() {
            // NOALLOC: within the capacity reserved in `print`.
            scratch.push(value);
        }
    }
}

/// Print one processor's slot.
fn print_slot(cpu: usize, slot: usize, scratch: &mut alloc::vec::Vec<u32>) {
    let count = |table: &[[AtomicU64; SLOTS]; CPUS]| {
        table
            .get(cpu)
            .and_then(|row| row.get(slot))
            .map_or(0, |c| c.load(Ordering::Relaxed))
    };
    let kept = count(&KEPT);
    let (broke, tainted, interrupted) = (count(&BROKE), count(&TAINTS), count(&INTERRUPTS));
    if kept == 0 && broke == 0 && tainted == 0 && interrupted == 0 {
        return;
    }
    let held = usize::try_from(kept).unwrap_or(RING).min(RING);
    let overhead = u64::from(OVERHEAD.get(slot).map_or(0, |o| o.load(Ordering::Relaxed)));
    let name = SLOT_NAMES.get(slot).copied().unwrap_or("?");
    println!(
        "  prof     {name} cpu{cpu}: directions kept={kept} dropped: broken={broke} barrier={tainted} \
         interrupted={interrupted}; samples held={held}; stamp={overhead} cycles; PMCR={:#010x}",
        PMCR_AT.get(slot).map_or(0, |p| p.load(Ordering::Relaxed)),
    );
    let mut sum_mean = 0_u64;
    let mut sum_m90 = 0_u64;
    let mut sum_net = 0_u64;
    for (point, span) in NAMES.iter().enumerate() {
        gather(cpu, slot, held, Some(point), None, scratch);
        let (mean, p90, m90) = figures(scratch);
        let net = m90.saturating_sub(overhead);
        sum_mean += mean;
        sum_m90 += m90;
        sum_net += net;
        println!(
            "  prof     {name} span {point:>2} {span:<32} mean={mean:>6} p90={p90:>6} m90={m90:>6} \
             net={net:>6}"
        );
    }
    gather(cpu, slot, held, None, None, scratch);
    let (mean, p90, m90) = figures(scratch);
    let stamps = overhead * POINTS as u64;
    println!(
        "  prof     {name} direction mean={mean} p90={p90} m90={m90} net={} (less {POINTS} stamps); \
         sum of span means={sum_mean} m90s={sum_m90} nets={sum_net}",
        m90.saturating_sub(stamps),
    );
    for side in 0..2_u8 {
        gather(cpu, slot, held, None, Some(side), scratch);
        let n = scratch.len();
        let (mean, p90, m90) = figures(scratch);
        gather(cpu, slot, held, Some(0), Some(side), scratch);
        let (user_mean, _, user_m90) = figures(scratch);
        let root = ROOTS
            .get(cpu)
            .and_then(|r| r.get(usize::from(side)))
            .map_or(0, |r| r.load(Ordering::Relaxed));
        println!(
            "  prof     {name} side {side} (root {root:#010x}, starts the direction) n={n} \
             direction mean={mean} p90={p90} m90={m90}; span 0 mean={user_mean} m90={user_m90}"
        );
    }
    for (point, at) in BROKE_AT.get(slot).into_iter().flatten().enumerate() {
        let n = at.load(Ordering::Relaxed);
        if n != 0 {
            println!("  prof     {name} broke at point {point}: {n}");
        }
    }
}
