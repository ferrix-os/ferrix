//! Stage 4's exit criterion, and the checks on the way to it.
//!
//! Every check here runs on every processor at once, through
//! [`run_everywhere`], because that is the only way to test anything about a
//! multiprocessor: a property that holds on each processor alone is exactly
//! the kind that fails when they run together.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::MapFlags;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::Once;

use crate::sync::SpinLock;

use super::{PerCpu, SHOOTING, TLB_GENERATION, Topology, run_everywhere};
use crate::{arch, mm, vmap};

/// What the checks found, for the boot log.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Report {
    /// Rounds of work every processor ran.
    pub(crate) rounds: u64,
    /// Inter-processor interrupts the secondaries took doing it.
    pub(crate) ipis: u64,
    /// Times a page was moved to another frame under every processor.
    pub(crate) remaps: u64,
    /// Shootdowns run by interrupt while it was. None where the architecture
    /// invalidates every processor's TLB itself.
    pub(crate) shootdowns: u64,
    /// Grace periods the writer waited for.
    pub(crate) grace_periods: u64,
    /// Read-side sections the readers ran meanwhile.
    pub(crate) reads: u64,
    /// What the contended counter came to.
    pub(crate) counter: u64,
    /// What it had to come to.
    pub(crate) expected: u64,
    /// Processors whose increments overlapped another's in time.
    pub(crate) overlapping: u64,
    /// The round of the contended count that showed them overlapping, or its
    /// last: a count of its own, since [`Report::rounds`] is `everywhere`'s.
    pub(crate) counter_round: u64,
    /// Updates the same count lost without the lock.
    pub(crate) lost: u64,
}

/// Run the checks.
///
/// # Errors
///
/// The first one that fails, as a sentence.
pub(crate) fn run(topology: &Topology) -> Result<Report, &'static str> {
    let mut report = Report::default();
    everywhere(topology, &mut report)?;
    page_sets()?;
    tables_wait_for_their_shootdown()?;
    shootdown(&mut report)?;
    grace(topology, &mut report)?;
    contended(topology, &mut report)?;
    Ok(report)
}

/// The contended counter, kept under a ticket lock.
static COUNTER: SpinLock<u64> = SpinLock::new(0);

/// The same count kept with no lock at all: a load and a store, not an atomic
/// increment. It loses an update whenever two processors increment it at the
/// same moment, and how many it loses measures how much "at the same moment"
/// there really was.
static UNLOCKED: AtomicU64 = AtomicU64::new(0);

/// Processors at the start line.
static AT_START: AtomicU64 = AtomicU64::new(0);

/// The first and last value of [`COUNTER`] each processor produced, by
/// logical number.
static SPANS: Once<Vec<(AtomicU64, AtomicU64)>> = Once::new();

/// Increments each processor makes.
const INCREMENTS: u64 = 25_000;

/// How many rounds the contended count is given to show its processors
/// running at the same time before the check gives up on them.
///
/// One round was enough on a quiet host and not on a loaded one: an emulator
/// whose host deschedules whole virtual processors can run the rounds' shares
/// one after another, and the check then said the lock was never contended
/// when it was the host that never let the processors meet. The lock's
/// correctness is judged on every round; only the overlap, which is about the
/// host as much as the kernel, gets more than one chance.
const ROUNDS: usize = 5;

/// Increment the counter, from every processor at once.
fn count(me: &'static PerCpu) {
    // Everyone starts together, so the counter is contended from the first
    // increment rather than taken in turns as each processor wakes.
    let everyone = super::TOPOLOGY
        .get()
        .map_or(1, |topology| topology.online() as u64);
    let _ = AT_START.fetch_add(1, Ordering::SeqCst);
    while AT_START.load(Ordering::SeqCst) < everyone {
        core::hint::spin_loop();
    }

    let mut first = 0;
    let mut last = 0;
    for increment in 0..INCREMENTS {
        let value = {
            let mut counter = COUNTER.lock();
            *counter += 1;
            *counter
        };
        if increment == 0 {
            first = value;
        }
        last = value;

        let seen = UNLOCKED.load(Ordering::Relaxed);
        UNLOCKED.store(seen + 1, Ordering::Relaxed);
    }

    if let Some((start, end)) = SPANS.get().and_then(|spans| spans.get(me.logical)) {
        start.store(first, Ordering::Relaxed);
        end.store(last, Ordering::Relaxed);
    }
}

/// Whether two processors' shares of the count overlapped in time.
///
/// Measured in counter values rather than nanoseconds: the lock hands the
/// counter out one increment at a time, so its value *is* the order things
/// happened in, and needs no clock the processors would have to agree on.
const fn overlaps(one: (u64, u64), other: (u64, u64)) -> bool {
    one.0 < other.1 && other.0 < one.1
}

/// Stage 4's exit criterion: every processor increments one counter under one
/// lock, all at once, and the total has to come out right.
///
/// The total alone is not enough, because processors that took turns would
/// get it right too. So two more things are measured. Each processor's first
/// and last increment bound its share, and at least two shares have to
/// overlap — or the lock was never contended, and the test passed without
/// testing it. And the same count is kept beside it with no lock at all,
/// whose shortfall is reported: the updates the lock is what prevents losing.
fn contended(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    let spans = SPANS.call_once(|| {
        (0..topology.count())
            .map(|_| (AtomicU64::new(0), AtomicU64::new(0)))
            .collect()
    });

    let processors = topology.online() as u64;
    let expected = INCREMENTS * processors;

    for round in 1..=ROUNDS {
        *COUNTER.lock() = 0;
        UNLOCKED.store(0, Ordering::Relaxed);
        AT_START.store(0, Ordering::SeqCst);
        run_everywhere(count)?;

        // The lock is judged on every round: a wrong total is the kernel's
        // fault whatever the host did.
        let total = *COUNTER.lock();
        if total != expected {
            return Err(
                "the contended counter came out wrong: the lock let two processors in at once",
            );
        }

        let shares: Vec<(u64, u64)> = spans
            .iter()
            .map(|(first, last)| (first.load(Ordering::Relaxed), last.load(Ordering::Relaxed)))
            .collect();
        let overlapping = shares
            .iter()
            .enumerate()
            .filter(|&(index, &share)| {
                shares
                    .iter()
                    .enumerate()
                    .any(|(other, &theirs)| other != index && overlaps(share, theirs))
            })
            .count();

        report.counter = total;
        report.expected = expected;
        report.overlapping = overlapping as u64;
        report.lost = expected.saturating_sub(UNLOCKED.load(Ordering::Relaxed));
        report.counter_round = round as u64;
        if processors == 1 || overlapping > 0 {
            return Ok(());
        }
        // Every share, so the log shows the processors ran one after
        // another rather than the lock keeping them apart.
        crate::console::println!(
            "  counter  round {round}: no two shares overlapped, the host ran the processors one \
             after another; shares {shares:?}",
        );
    }
    Err("no two processors' increments overlapped in any round, so the lock was never contended")
}

/// What a grace-period reader finds through [`PUBLISHED`].
#[derive(Debug)]
struct Payload {
    /// [`LIVE`] while published, [`POISON`] once retired.
    value: AtomicU64,
}

/// The object the readers read, replaced by the writer every round.
static PUBLISHED: AtomicPtr<Payload> = AtomicPtr::new(core::ptr::null_mut());

/// Objects the writer has retired and poisoned, by address.
///
/// Kept, not freed, until every reader has stopped: a freed object's memory
/// would be the next one's, holding [`LIVE`] again, and a reader that should
/// have seen the poison would see a perfectly good value instead.
static RETIRED: SpinLock<Vec<usize>> = SpinLock::new(Vec::new());

/// Readers that have started reading.
static READERS_READY: AtomicU64 = AtomicU64::new(0);

/// Set by the writer when it is done.
static STOP: AtomicBool = AtomicBool::new(false);

/// Read-side sections run.
static READS: AtomicU64 = AtomicU64::new(0);

/// Reads that found a poisoned object.
static POISONED_READS: AtomicU64 = AtomicU64::new(0);

/// A published object's value.
const LIVE: u64 = 0x0B1E_C700_0000_0001;
/// A retired object's value.
const POISON: u64 = 0xDEAD_DEAD_DEAD_DEAD;

/// Grace periods the writer waits for.
const GRACE_ROUNDS: u64 = 100;

/// How long a reader holds what it loaded before it reads it.
///
/// Long enough that a grace period which ended early would find readers still
/// holding the object it had just poisoned; short enough that a section is
/// still the short thing a section has to be.
const HOLD_SPINS: u32 = 256;

/// The boot processor writes, and every other processor reads.
fn publish_and_read(me: &'static PerCpu) {
    if me.logical == 0 {
        write_side();
    } else {
        read_side();
    }
}

/// Read the published object, over and over, until the writer is done.
fn read_side() {
    let _ = READERS_READY.fetch_add(1, Ordering::SeqCst);
    while !STOP.load(Ordering::Acquire) {
        super::read_section(|| {
            let payload = PUBLISHED.load(Ordering::Acquire);
            if payload.is_null() {
                return;
            }
            for _ in 0..HOLD_SPINS {
                core::hint::spin_loop();
            }
            // SAFETY: (SHARED) every object this check ever publishes stays allocated
            // until every reader has stopped, so `payload` points at a live
            // `Payload` whatever the grace periods do. Whether it is still the
            // *current* one is what this check measures.
            let value = unsafe { &*payload }.value.load(Ordering::Relaxed);
            if value != LIVE {
                let _ = POISONED_READS.fetch_add(1, Ordering::Relaxed);
            }
        });
        let _ = READS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Replace the published object, wait a grace period, and poison the old one,
/// a hundred times.
fn write_side() {
    // Not until the readers are reading: a grace period with nothing to wait
    // for passes whether or not it works.
    let readers = super::TOPOLOGY
        .get()
        .map_or(0, |topology| topology.online() as u64 - 1);
    while READERS_READY.load(Ordering::SeqCst) < readers {
        core::hint::spin_loop();
    }

    for _ in 0..GRACE_ROUNDS {
        let fresh = Box::into_raw(Box::new(Payload {
            value: AtomicU64::new(LIVE),
        }));
        let old = PUBLISHED.swap(fresh, Ordering::AcqRel);
        super::synchronize();
        // SAFETY: (SHARED) `old` was published by this check and is still allocated —
        // nothing is freed while readers run — and after the grace period no
        // reader is holding it.
        unsafe { &*old }.value.store(POISON, Ordering::Relaxed);
        RETIRED.lock().push(old as usize);
    }
    STOP.store(true, Ordering::Release);
}

/// A writer replaces an object that every other processor is reading, waits
/// a grace period each time, and poisons the object it replaced — and no
/// reader may ever find the poison.
///
/// The failure this is built to see is a grace period that ends too soon. A
/// reader holds what it loaded for a while before reading it, so a writer
/// that poisoned early would find one still holding, and that reader would
/// read the poison.
///
/// Verifies: H.MEM.19, L.smp.29
fn grace(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    let first = Box::into_raw(Box::new(Payload {
        value: AtomicU64::new(LIVE),
    }));
    PUBLISHED.store(first, Ordering::Release);
    let before = super::grace_periods();

    run_everywhere(publish_and_read)?;

    // Every reader has stopped, so nothing can hold any of these now.
    let current = PUBLISHED.swap(core::ptr::null_mut(), Ordering::AcqRel);
    let mut retired = RETIRED.lock();
    for address in retired.drain(..).chain(core::iter::once(current as usize)) {
        // SAFETY: (KMEM) each address came from `Box::into_raw` in this module, is
        // freed exactly once, here, and no processor is reading: the work that
        // read them has returned on every one.
        drop(unsafe { Box::from_raw(address as *mut Payload) });
    }
    drop(retired);

    if POISONED_READS.load(Ordering::Relaxed) != 0 {
        return Err("a reader found an object poisoned by a grace period that should have waited");
    }
    let reads = READS.load(Ordering::Relaxed);
    if topology.count() > 1 && reads == 0 {
        return Err("no reader read anything, so the grace periods waited for nothing");
    }
    report.grace_periods = super::grace_periods() - before;
    report.reads = reads;
    Ok(())
}

/// The page [`read_probe`] reads, while [`shootdown`] runs.
static PROBE: AtomicU64 = AtomicU64::new(0);

/// What it must find there.
static EXPECTED: AtomicU64 = AtomicU64::new(0);

/// Reads that found something else.
static STALE: AtomicU64 = AtomicU64::new(0);

/// Where the markers [`shootdown`] writes start from.
const MARKER: u64 = 0x5407_D0A1_0000_0000;

/// Read the probe page and compare it with what it should hold.
fn read_probe(_me: &'static PerCpu) {
    let at = PROBE.load(Ordering::Acquire);
    // SAFETY: (PROBE) `shootdown` maps a page at `at` before handing this out, and
    // moves it only once every processor has finished reading it.
    let seen = unsafe { core::ptr::read_volatile(at as *const u64) };
    if seen != EXPECTED.load(Ordering::Acquire) {
        let _ = STALE.fetch_add(1, Ordering::Relaxed);
    }
}

/// A scoped shootdown's page set: pages up to the ceiling one by one, the
/// whole TLB past it, and a set merged into another carrying either along;
/// and the processor mask whose snapshot says where to send it.
///
/// A retirement merges the set its caller's own space already built into its
/// own before it sends one shootdown for both (`user::vmo`), and a merged set
/// that dropped the caller's "everything" would leave pages cached on the
/// processors that ran it.
///
/// Verifies: L.smp.26, L.smp.27
fn page_sets() -> Result<(), &'static str> {
    use super::{PAGE_FLUSH_CEILING, TlbPages};

    let mut listed = TlbPages::new();
    listed.add_range(0x10_0000, PAGE_FLUSH_CEILING as u64 * PAGE_SIZE);
    listed.add(0x10_0000 + 5);
    if listed.is_everything() || listed.addresses().len() != PAGE_FLUSH_CEILING {
        return Err("a page set at its ceiling did not list every page once");
    }
    let mut over = TlbPages::new();
    over.add_range(0x10_0000, (PAGE_FLUSH_CEILING as u64 + 1) * PAGE_SIZE);
    if !over.is_everything() {
        return Err("a page set past its ceiling did not ask for the whole TLB");
    }

    let mut merged = TlbPages::new();
    merged.add(0x20_0000);
    let mut other = TlbPages::new();
    other.add(0x30_0000);
    other.add(0x20_0000);
    merged.add_all(&mut other);
    if merged.is_everything() || merged.addresses() != [0x20_0000, 0x30_0000] {
        return Err("two merged page sets did not name each page once");
    }
    merged.add_all(&mut over);
    if !merged.is_everything() {
        return Err("a page set merged with one asking for the whole TLB did not ask for it too");
    }

    // The mask a shootdown snapshots, joined by two processors and left by
    // one, reads back as the one left in it -- as its snapshot and as the
    // words an address space's failure report prints.
    let mask = super::CpuMask::new();
    mask.join(0);
    mask.join(3);
    mask.leave(0);
    let snapshot = mask.snapshot();
    if snapshot.contains(0) || !snapshot.contains(3) {
        return Err("a processor mask did not keep the processors that joined and stayed");
    }
    let printed = alloc::format!("{mask:?}");
    let first_word = printed
        .strip_prefix("CpuMask { words: [8")
        .and_then(|rest| rest.chars().next());
    if !matches!(first_word, Some(',' | ']')) {
        return Err("a processor mask does not print as the words it holds");
    }
    Ok(())
}

/// A page table an unmap empties is held until the shootdown for the unmap
/// has returned, and given back by it (finding F-36).
///
/// A processor caches the walk as well as the leaf, so until its TLB is
/// invalidated it may walk through a table whose descriptor is already gone
/// from memory. A table that went back to the allocator before then could be
/// handed to anybody and filled with descriptors of their choosing, which
/// the stale walk would follow. A real walk landing in that window cannot be
/// arranged: under TCG QEMU caches no intermediate walk at all, and under KVM
/// the window is a few microseconds wide. So what is checked is the order:
/// every table the unmap unlinked is
/// still allocated when it returns, not one is counted given back, and the
/// shootdown gives back exactly those.
///
/// With the tables freed in the unmap, as user unmaps did before, this fails
/// at its first test.
///
/// Verifies: H.MEM.7
fn tables_wait_for_their_shootdown() -> Result<(), &'static str> {
    let root = mm::allocate_frames(0).ok_or("no frame for the table check's root")?;
    mm::zero_frame(root);
    let Some(page) = mm::allocate_frames(0) else {
        mm::deallocate_frames(root, 0);
        return Err("no frame for the table check's page");
    };
    let outcome = unlink_and_shoot(root * PAGE_SIZE, page * PAGE_SIZE);
    mm::deallocate_frames(page, 0);
    mm::deallocate_frames(root, 0);
    outcome
}

/// [`tables_wait_for_their_shootdown`] over the tree at `root`, mapping and
/// unmapping `phys` at a user address the tree has nothing else under.
///
/// Verifies: L.mm.23, L.mm.39, L.smp.24
fn unlink_and_shoot(root: u64, phys: u64) -> Result<(), &'static str> {
    use super::TlbPages;

    /// Somewhere in the user half on every architecture, alone in its tables.
    const AT: u64 = 0x4000_0000;

    mm::map_in(root, AT, phys, PAGE_SIZE, MapFlags::USER_DATA)
        .map_err(|_| "the table check could not map its page")?;
    let given_before = mm::user_tables_given_back();
    let mut pages = TlbPages::new();
    mm::unmap_in(root, AT, PAGE_SIZE, &mut pages)
        .map_err(|_| "the table check could not unmap its page")?;
    if mm::translate_in(root, AT).is_some() {
        return Err("the table check's page still translates after its unmap");
    }
    let held = pages.tables().len();
    if held == 0 || mm::user_tables_given_back() != given_before {
        return Err("an unmap gave back the tables it emptied before its shootdown");
    }
    for frame in pages.tables().frames() {
        if let Some(claimed) = mm::claim_frame(frame) {
            mm::deallocate_frames(claimed, 0);
            return Err("a table an unmap emptied was free before its shootdown");
        }
    }
    // No processor ever had this tree: the empty set, as for a space nobody
    // runs, which is still the call that has to give the tables back.
    super::flush_tlb_pages(&CpuSet::empty(), &mut pages);
    if !pages.tables().is_empty() || mm::user_tables_given_back() != given_before + held {
        return Err("a shootdown did not give back the tables its unmap emptied");
    }
    Ok(())
}

/// A page is moved to another frame, again and again, and every processor
/// has to see the frame it was moved to each time.
///
/// Each round has every processor read the page first, which is what puts a
/// translation for it into each TLB, and then moves it. The old frame keeps
/// its old contents and stays allocated, so a processor still translating
/// through a stale entry does not fault: it reads the old value, quietly —
/// which is what a missing shootdown looks like in a running kernel, and
/// exactly what this counts.
///
/// Verifies: H.MEM.12, L.mm.28, L.smp.14, `L.x86_64.108`
fn shootdown(report: &mut Report) -> Result<(), &'static str> {
    const ROUNDS: u64 = 20;

    let page =
        vmap::allocate(1, MapFlags::KERNEL_DATA).map_err(|_| "no page for the shootdown check")?;
    let mut mapped =
        mm::translate(page.base).ok_or("the shootdown check's page is not mapped")? / PAGE_SIZE;
    let mut spare = mm::allocate_frames(0).ok_or("no second frame for the shootdown check")?;
    PROBE.store(page.base, Ordering::Release);
    let shootdowns_before = super::shootdowns();

    // SAFETY: (PROBE) the page was allocated a moment ago, writable, and is nobody
    // else's.
    unsafe { core::ptr::write_volatile(page.base as *mut u64, MARKER) };

    for round in 0..ROUNDS {
        EXPECTED.store(MARKER + round, Ordering::Release);
        run_everywhere(read_probe)?;

        // The next round's marker goes into the spare frame, and the page is
        // moved onto it. `unmap_kernel` is told to release nothing: the old
        // frame has to stay, holding the old marker, for a stale read to find.
        let next = mm::direct_map(spare * PAGE_SIZE);
        // SAFETY: (FRAME) `spare` is a frame this check owns and nothing maps it; the
        // direct map makes it writable.
        unsafe { core::ptr::write_volatile(next as *mut u64, MARKER + round + 1) };
        let _removed = mm::unmap_kernel(page.base, PAGE_SIZE, |_, _| {})
            .map_err(|_| "the shootdown check's page could not be unmapped")?;
        mm::map_kernel(
            page.base,
            spare * PAGE_SIZE,
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .map_err(|_| "the shootdown check's page could not be remapped")?;
        core::mem::swap(&mut mapped, &mut spare);
    }
    EXPECTED.store(MARKER + ROUNDS, Ordering::Release);
    run_everywhere(read_probe)?;

    // `vmap::free` gives back whichever frame is mapped now; the other one is
    // this check's to return.
    vmap::free(page.base).map_err(|_| "the shootdown check's page could not be freed")?;
    mm::deallocate_frames(spare, 0);

    if STALE.load(Ordering::Relaxed) != 0 {
        return Err(
            "a processor read a page through a translation a shootdown should have dropped",
        );
    }
    report.remaps = ROUNDS;
    report.shootdowns = super::shootdowns() - shootdowns_before;
    Ok(())
}

/// How many times each processor ran [`tally_run`], by logical number.
static RUNS: Once<Vec<AtomicU64>> = Once::new();

/// Runs of [`tally_run`] on a processor whose record named another.
static MISPLACED: AtomicU64 = AtomicU64::new(0);

/// Count this run, and check the processor running it is the one its record
/// names.
///
/// The second half is the per-CPU register tested under load: `me` came
/// through that register, and `arch::hardware_id` asks the hardware.
fn tally_run(me: &'static PerCpu) {
    if me.hardware_id != arch::hardware_id() {
        let _ = MISPLACED.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(slot) = RUNS.get().and_then(|runs| runs.get(me.logical)) {
        let _ = slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// Every processor runs the work it is handed, once per round, a hundred
/// times over — and every secondary is woken for it by an interrupt.
///
/// A hundred rounds rather than one because each round is a secondary going
/// to sleep and being woken, and the way that goes wrong is a lost wake-up: an
/// interrupt that arrives between a processor deciding to sleep and sleeping.
/// That is a race, and one round would have to be lucky to lose it.
///
/// Verifies: H.SCHED.6, L.smp.3, L.smp.11
/// Verifies: `L.x86_64.18`, `L.x86_64.19`, `L.x86_64.86`, `L.x86_64.102`
fn everywhere(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    const ROUNDS: u64 = 100;

    let runs = RUNS.call_once(|| (0..topology.count()).map(|_| AtomicU64::new(0)).collect());
    let ipis_before: u64 = topology.cpus().iter().map(PerCpu::ipis_taken).sum();

    for _ in 0..ROUNDS {
        run_everywhere(tally_run)?;
    }

    if MISPLACED.load(Ordering::Relaxed) != 0 {
        return Err("work ran on a processor whose per-CPU record names another");
    }
    if runs
        .iter()
        .any(|count| count.load(Ordering::Relaxed) != ROUNDS)
    {
        return Err("a processor did not run its share of the work once per round");
    }
    // Not "once per round": two interrupts sent to a processor before it
    // takes the first are delivered as one, and that is correct behaviour.
    // What must hold is that each secondary takes them at all.
    if topology
        .cpus()
        .iter()
        .skip(1)
        .any(|cpu| cpu.ipis_taken() == 0)
    {
        return Err("a secondary processor never took an inter-processor interrupt");
    }

    report.rounds = ROUNDS;
    report.ipis = topology.cpus().iter().map(PerCpu::ipis_taken).sum::<u64>() - ipis_before;
    Ok(())
}

// ---------------------------------------------------------------------------
// A shootdown waited for on one processor and answered on another
//
// Run after the scheduler is up, because what it tests only exists once
// kernel code can be preempted and a task resumed somewhere else.
// ---------------------------------------------------------------------------

/// The processor the migrating task began waiting on, plus one; zero until it
/// has.
static MIGRANT_STARTED_ON: AtomicUsize = AtomicUsize::new(0);

/// Set once the check holds the turn, which is what the migrating task waits
/// for before it asks for a shootdown of its own.
static MIGRANT_GO: AtomicBool = AtomicBool::new(false);

/// Set once the migrating task's shootdown has returned.
static MIGRANT_DONE: AtomicBool = AtomicBool::new(false);

/// Tells the task keeping the migrant's first processor busy to stop.
static HOG_STOP: AtomicBool = AtomicBool::new(false);

/// Set once that task has stopped.
static HOG_DONE: AtomicBool = AtomicBool::new(false);

/// How long any one step of [`migrating_shootdown`] waits.
const MIGRATION_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long the migrant is left to settle on its new processor, so that no
/// interrupt sent while moving it is still on its way when the check starts
/// counting answers.
const MIGRATION_SETTLE_NANOS: u64 = 50_000_000;

/// Record where this started, wait for the check to hold the turn, then run
/// a shootdown, which waits for that turn.
///
/// The wait is a spin, like the wait for the turn it turns into: a runnable
/// task on a busy processor, worth stealing, and answering shootdowns once
/// it is in `take_turn`.
fn migrant(_argument: usize) {
    if let Some(me) = super::this_cpu() {
        MIGRANT_STARTED_ON.store(me.logical + 1, Ordering::Release);
    }
    while !MIGRANT_GO.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    super::flush_tlb_everywhere();
    MIGRANT_DONE.store(true, Ordering::Release);
}

/// Keep a processor busy, so that the task waiting beside it is worth taking.
fn hog(_argument: usize) {
    while !HOG_STOP.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    HOG_DONE.store(true, Ordering::Release);
}

/// Spin, with interrupts open, until `ready` or `patience` runs out.
///
/// A spin rather than a yield, because the caller may be holding the
/// shootdown turn, and a spin lock is not held across a switch.
fn spin_until(
    patience: u64,
    mut ready: impl FnMut() -> bool,
    what: &'static str,
) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(patience);
    while !ready() {
        if crate::timer::now_nanos() > deadline {
            return Err(what);
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Spin on the counter for `nanos`, with interrupts open.
fn spin_for(nanos: u64) {
    let until = crate::timer::now_nanos().saturating_add(nanos);
    while crate::timer::now_nanos() < until {
        core::hint::spin_loop();
    }
}

/// A task that moves to another processor while it waits for its turn at a
/// shootdown answers shootdowns for the processor it is on, and never for the
/// one it left.
///
/// The task waits because this holds the turn. A second task pinned beside it
/// makes it worth stealing, and broadcast interrupts wake the idle processors
/// to steal it -- each sent with a shootdown generation of its own, which
/// every processor answers by flushing, so the kicks vouch for nothing. Once
/// it has moved and settled, one more generation is requested with no
/// interrupt at all. Nothing then flushes for it except the waiting task, in
/// its loop: the processor it is on must come to answer, and the processor it
/// left, busy with the pinned task and interrupted by nobody, must not.
///
/// A task that kept the record it started with flushes where it is and
/// records the flush for where it was, so the processor it moved to never
/// answers and the one it left is recorded as flushed without flushing. That
/// is the case in which a real shootdown frees memory a stale translation
/// still reaches.
///
/// Returns the processors it moved between; `None` where the architecture's
/// invalidation is broadcast and no processor waits for another, or where
/// there are too few processors to keep this one out of the way and still
/// have two to move between.
///
/// # Errors
///
/// If the task never moves, never answers where it is, or answers for the
/// processor it left.
///
/// Verifies: L.smp.16
pub(crate) fn migrating_shootdown(
    topology: &Topology,
) -> Result<Option<(usize, usize)>, &'static str> {
    if arch::TLB_FLUSH_IS_BROADCAST || topology.online() < 3 {
        return Ok(None);
    }
    let here = super::this_cpu()
        .ok_or("no processor to run the migration check on")?
        .logical;
    let mut elsewhere = CpuSet::empty();
    for cpu in (0..topology.count()).filter(|cpu| *cpu != here) {
        elsewhere
            .insert(cpu)
            .map_err(|_| "more processors than a set of them holds")?;
    }
    let first = (here + 1) % topology.count();

    MIGRANT_STARTED_ON.store(0, Ordering::Release);
    MIGRANT_GO.store(false, Ordering::Release);
    MIGRANT_DONE.store(false, Ordering::Release);
    HOG_STOP.store(false, Ordering::Release);
    HOG_DONE.store(false, Ordering::Release);
    let arena_before = vmap::usage().allocations;

    // Both tasks are made before the turn is taken, not under it: a spawn
    // that fails part-way frees the stack it had, and that is a shootdown,
    // which may not be asked for under the turn -- it would wait for itself
    // until the timeout, and `smp` now refuses it outright. The migrant
    // waits on `MIGRANT_GO` until the turn is held, so its own shootdown
    // still queues behind this one.
    let migrant_task = crate::sched::spawn_on(
        "shootdown-migrant",
        migrant,
        0,
        NICE_0_WEIGHT,
        first,
        elsewhere,
    )?;
    spin_until(
        MIGRATION_PATIENCE_NANOS,
        || MIGRANT_STARTED_ON.load(Ordering::Acquire) != 0,
        "the task that was to wait for a shootdown never started",
    )?;
    let left = MIGRANT_STARTED_ON.load(Ordering::Acquire) - 1;
    let hog_task = crate::sched::spawn_on(
        "shootdown-hog",
        hog,
        0,
        NICE_0_WEIGHT,
        left,
        CpuSet::of(left),
    )?;
    let turn = SHOOTING.lock();
    MIGRANT_GO.store(true, Ordering::Release);

    // Wake the idle processors until one of them takes the waiting task.
    let deadline = crate::timer::now_nanos().saturating_add(MIGRATION_PATIENCE_NANOS);
    while migrant_task.cpu() == left {
        if crate::timer::now_nanos() > deadline {
            return Err("the task waiting for a shootdown was never moved off its processor");
        }
        let _ = TLB_GENERATION.fetch_add(1, Ordering::SeqCst);
        let _ = arch::send_ipi_to_others();
        spin_for(1_000_000);
    }
    spin_for(MIGRATION_SETTLE_NANOS);

    let generation = TLB_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let cpus = topology.cpus();
    let answered = |cpu: usize| {
        cpus.get(cpu)
            .is_some_and(|record| record.tlb_seen.load(Ordering::SeqCst) >= generation)
    };
    let moved_to_answered = spin_until(
        MIGRATION_PATIENCE_NANOS / 5,
        || {
            let now_on = migrant_task.cpu();
            now_on != left && answered(now_on)
        },
        "",
    );
    if moved_to_answered.is_err() {
        return Err(if answered(left) {
            "a task that moved while waiting for a shootdown answered for the processor it left"
        } else {
            "a task that moved while waiting for a shootdown never answered where it was"
        });
    }
    let moved_to = migrant_task.cpu();

    // Let both go: the migrant takes the turn and runs a real shootdown, which
    // brings every processor, this one included, up to date.
    HOG_STOP.store(true, Ordering::Release);
    drop(turn);
    spin_until(
        MIGRATION_PATIENCE_NANOS,
        || MIGRANT_DONE.load(Ordering::Acquire) && HOG_DONE.load(Ordering::Acquire),
        "the tasks of the migration check never finished",
    )?;
    drop(migrant_task);
    drop(hog_task);

    let deadline = crate::timer::now_nanos().saturating_add(MIGRATION_PATIENCE_NANOS);
    loop {
        let _ = crate::sched::reap();
        if vmap::usage().allocations <= arena_before {
            break;
        }
        if crate::timer::now_nanos() > deadline {
            return Err("a task of the migration check never gave its stack back");
        }
        crate::sched::yield_now();
    }
    Ok(Some((left, moved_to)))
}

// ---------------------------------------------------------------------------
// A processor that answers late is waited for, not called stuck
//
// Run after the scheduler is up, since the processor kept from answering is
// kept so by a task pinned to it.
// ---------------------------------------------------------------------------

/// Set by the wait the moment it counts the held processor late: the one
/// thing the holder waits on, so that it is let go by the wait itself.
static LATE_MARK: AtomicBool = AtomicBool::new(false);

/// Set by the holder once it is inside its read-side section.
static LATE_HOLDING: AtomicBool = AtomicBool::new(false);

/// Set by the holder once it has left its section.
static LATE_HELD_DONE: AtomicBool = AtomicBool::new(false);

/// Set by the holder if its guard, not the mark, let it go.
static LATE_GUARD_FIRED: AtomicBool = AtomicBool::new(false);

/// The late bound the check's waits are given: a wall-clock floor and a
/// count of polls far below a product wait's, so the check costs
/// milliseconds; the stuck bound stays the product's.
const LATE_CHECK_NANOS: u64 = 20_000_000;

/// The polls of the check's late bound.
const LATE_CHECK_POLLS: u64 = 1 << 16;

/// How long the holder keeps interrupts masked at the most when the wait
/// never marks it late: long enough for any machine to reach the check's
/// late bound, and short of the stuck bound, so a wait that never counts it
/// fails the check rather than stopping the machine.
const LATE_GUARD_NANOS: u64 = 5_000_000_000;

/// Keep this processor from answering anything, as a host that stops
/// running it would, until the wait has counted it late.
///
/// A read-side section masks interrupts, so neither a shootdown's nor a grace
/// period's interrupt is taken until it ends; taken then, it answers both.
fn late_holder(_argument: usize) {
    super::read_section(|| {
        LATE_HOLDING.store(true, Ordering::SeqCst);
        let guard = crate::timer::now_nanos().saturating_add(LATE_GUARD_NANOS);
        while !LATE_MARK.load(Ordering::SeqCst) {
            if crate::timer::now_nanos() > guard {
                LATE_GUARD_FIRED.store(true, Ordering::SeqCst);
                break;
            }
            core::hint::spin_loop();
        }
    });
    LATE_HELD_DONE.store(true, Ordering::SeqCst);
}

/// Which wait [`late_round`] runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LateWait {
    /// A grace period, [`super::synchronize_within`].
    Grace,
    /// A shootdown of every processor, [`super::flush_everywhere_within`].
    Shootdown,
}

/// Hold processor `held` from answering, run one wait, and return what it
/// counted late.
fn late_round(held: usize, wait: LateWait) -> Result<super::Late, &'static str> {
    LATE_MARK.store(false, Ordering::SeqCst);
    LATE_HOLDING.store(false, Ordering::SeqCst);
    LATE_HELD_DONE.store(false, Ordering::SeqCst);
    LATE_GUARD_FIRED.store(false, Ordering::SeqCst);
    let holder = crate::sched::spawn_on(
        "late-holder",
        late_holder,
        0,
        NICE_0_WEIGHT,
        held,
        CpuSet::of(held),
    )?;
    spin_until(
        MIGRATION_PATIENCE_NANOS,
        || LATE_HOLDING.load(Ordering::SeqCst),
        "the task that was to keep a processor from answering never started",
    )?;
    let bounds = super::Bounds::checked(LATE_CHECK_NANOS, LATE_CHECK_POLLS, &LATE_MARK);
    let late = match wait {
        LateWait::Grace => super::synchronize_within(bounds),
        LateWait::Shootdown => super::flush_everywhere_within(bounds),
    };
    spin_until(
        MIGRATION_PATIENCE_NANOS,
        || LATE_HELD_DONE.load(Ordering::SeqCst),
        "the task that kept a processor from answering never finished",
    )?;
    drop(holder);
    if LATE_GUARD_FIRED.load(Ordering::SeqCst) {
        return Err(
            "a wait never counted a processor kept from answering late, and its guard let it go",
        );
    }
    if late.count == 0 || !late.cpus.contains(held) {
        return Err("a processor that answered a wait late was not counted late");
    }
    Ok(late)
}

/// What [`late_answer`] saw.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LateAnswers {
    /// The processor kept from answering.
    pub(crate) held: usize,
    /// How long the grace period waited for it, in microseconds.
    pub(crate) grace_us: u64,
    /// How long the shootdown waited for it, in microseconds; `None` where
    /// the architecture's invalidation is broadcast and nothing waits.
    pub(crate) shootdown_us: Option<u64>,
}

/// A processor that answers a wait past its late bound -- here kept from
/// answering by a task inside a read-side section, which from the waiter's
/// side is a host that stopped running it -- is waited for and counted late,
/// and the wait returns once it answers, without stopping the machine.
///
/// A grace period on every architecture, and a shootdown of every processor
/// where invalidation is not broadcast, first, so that a processor that never
/// answers (the negative control) ends the boot in FX-0001 there. The check's
/// waits are given a late bound of 20 ms and 65,536 polls of their own; the
/// stuck bound is the product's.
///
/// The task running this cannot be on the held processor during a round:
/// it is running elsewhere when it sees the holder inside its section. A
/// running task moves only by being pulled, by a stealing or balancing
/// processor onto itself, and the held processor, interrupts masked, runs
/// neither until the holder lets go; and this task never sleeps during a
/// round -- every wait in it spins -- so no wakeup places it there either.
///
/// Returns `None` with fewer than two processors online.
///
/// # Errors
///
/// If the holder never starts or finishes, if a wait never counts the held
/// processor late, or if it returns without the held processor among those
/// counted.
///
/// Verifies: L.smp.33
pub(crate) fn late_answer(topology: &Topology) -> Result<Option<LateAnswers>, &'static str> {
    if topology.online() < 2 {
        return Ok(None);
    }
    let here = super::this_cpu()
        .ok_or("no processor to run the late-answer check on")?
        .logical;
    let held = topology
        .cpus()
        .iter()
        .find(|cpu| cpu.is_online() && cpu.logical != here)
        .ok_or("no other processor online to keep from answering")?
        .logical;
    let shootdown_us = if arch::TLB_FLUSH_IS_BROADCAST {
        None
    } else {
        Some(late_round(held, LateWait::Shootdown)?.longest_nanos / 1_000)
    };
    let grace_us = late_round(held, LateWait::Grace)?.longest_nanos / 1_000;
    Ok(Some(LateAnswers {
        held,
        grace_us,
        shootdown_us,
    }))
}
