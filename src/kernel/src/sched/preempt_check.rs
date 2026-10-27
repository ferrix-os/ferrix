//! The preemption count's checks (OPAQUE-KERNEL.md §9.8, 2b): the count kept
//! in each processor's record without a locked operation survives tasks a
//! timer preempts and moves; an underflow is refused; `may_block` reads its
//! own processor's count; and a failed `try_lock` leaves the count as it
//! found it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched_class::CpuSet;
use ferrix_sched_fair::NICE_0_WEIGHT;
use ferrix_sync::IrqControl;

use super::Task;
use super::preempt::{self, word_here};
use crate::smp::Topology;

/// Lock pairs each task of the moving check takes and lets go.
const PAIRS: u64 = 100_000;

/// Every this many pairs, a raise and lower by hand as well.
const BY_HAND_EVERY: u64 = 1_024;

/// Every this many pairs, a short sleep: what lets processors go idle, steal
/// and place on waking, so that the tasks move.
const SLEEP_EVERY: u64 = 256;

/// How long that sleep is.
const SLEEP_NANOS: u64 = 50_000;

/// Tasks per processor in the moving check.
const TASKS_PER_CPU: usize = 2;

/// Locks the moving check's tasks take, one each, so that they contend only
/// with interrupts and moves, not with each other.
const LOCKS: usize = 32;

/// How long the moving check waits for its tasks.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// The moving check's locks.
static PAIR_LOCKS: [crate::sync::SpinLock<u64>; LOCKS] =
    [const { crate::sync::SpinLock::new(0) }; LOCKS];

/// Tasks of either check that have finished.
static FINISHED: AtomicU64 = AtomicU64::new(0);

/// Times a task of the moving check found itself on another processor than
/// at its last look.
static MOVES: AtomicU64 = AtomicU64::new(0);

/// Set by a task that read a count or locks held other than zero on its own
/// processor while holding nothing.
static RAISED: AtomicBool = AtomicBool::new(false);

/// What it read: processor, count and locks held, packed for the report.
static RAISED_WORD: AtomicU64 = AtomicU64::new(0);

/// Run them, in order.
///
/// # Errors
///
/// The first that fails, as a sentence.
pub(super) fn run(topology: &Topology) -> Result<(), &'static str> {
    an_underflow_is_refused()?;
    may_block_reads_its_own_count()?;
    a_failed_try_leaves_the_count()?;
    the_count_survives_preemption_and_moves(topology)
}

/// Record a count found raised on a processor whose task holds nothing.
fn note_raised(cpu: usize, count: u32, held: u32) {
    RAISED_WORD.store(
        ((cpu as u64) << 48) | (u64::from(held & 0xFFFF) << 32) | u64::from(count),
        Ordering::Relaxed,
    );
    RAISED.store(true, Ordering::Release);
}

/// Wait for `count` finishes, yielding meanwhile.
fn wait_finished(count: u64, what: &'static str) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while FINISHED.load(Ordering::Acquire) < count {
        if crate::timer::now_nanos() >= deadline {
            return Err(what);
        }
        super::sleep_for(1_000_000);
    }
    Ok(())
}

/// The decision an enable makes before it lowers anything: a word that does
/// not cover the release is refused, whichever half falls short, and one
/// that does is lowered.
///
/// Verifies: L.sched.21
fn an_underflow_is_refused() -> Result<(), &'static str> {
    let cases: [(u64, bool, bool); 6] = [
        (0, false, false),
        (0, true, false),
        // A by-hand raise, released as if it were a lock.
        (1, true, false),
        // A lock held, released by hand: the count covers it.
        ((1 << 32) | 1, false, true),
        ((1 << 32) | 1, true, true),
        ((2 << 32) | 3, true, true),
    ];
    for (word, by_lock, covered) in cases {
        if preempt::covers_for_check(word, by_lock) != covered {
            return Err("an enable's cover test answered wrongly for a count or locks held");
        }
    }
    Ok(())
}

/// `may_block` is false while this task holds a lock or has raised the count
/// by hand, and true once it holds neither.
///
/// Verifies: L.sched.22
fn may_block_reads_its_own_count() -> Result<(), &'static str> {
    static LOCK: crate::sync::SpinLock<u32> = crate::sync::SpinLock::new(0);
    if !super::may_block() {
        return Err("a task holding nothing, with interrupts on, may not block");
    }
    {
        let _held = LOCK.lock();
        if super::may_block() {
            return Err("a task holding a preemption-disabling lock was told it may block");
        }
    }
    super::preempt_disable();
    let raised = super::may_block();
    super::preempt_enable();
    if raised {
        return Err("a task with preemption disabled by hand was told it may block");
    }
    if !super::may_block() {
        return Err("a task that let go of its lock may still not block");
    }
    Ok(())
}

/// A `try_lock` that fails leaves this processor's count and locks held
/// where it found them, with interrupts on and masked, and with them masked
/// switches nothing; one that succeeds raises both by one until its guard
/// drops. `ferrix_sync::SpinLock::try_lock_manually`, which the run queues
/// will use, touches neither.
///
/// Verifies: L.sched.23
fn a_failed_try_leaves_the_count() -> Result<(), &'static str> {
    static LOCK: crate::sync::SpinLock<u32> = crate::sync::SpinLock::new(0);
    static PLAIN: ferrix_sync::SpinLock<u32> = ferrix_sync::SpinLock::new(0);
    let me = super::current().ok_or("the checking task is not running")?;
    let (_, count, held) = word_here().ok_or("this processor has no record")?;

    let guard = LOCK.lock();
    let raised = word_here().ok_or("this processor has no record")?;
    if (raised.1, raised.2) != (count + 1, held + 1) {
        return Err("a lock did not raise the count and the locks held by one each");
    }
    if LOCK.try_lock().is_some() {
        return Err("a try_lock took a lock its own task holds");
    }
    if word_here() != Some(raised) {
        return Err("a failed try_lock, with interrupts on, left the count changed");
    }
    let saved = <crate::arch::Irq as IrqControl>::disable();
    let switches = me.switches();
    let refused = LOCK.try_lock().is_none();
    let after = word_here();
    let switched = me.switches() != switches;
    <crate::arch::Irq as IrqControl>::restore(saved);
    if !refused {
        return Err("a try_lock, with interrupts masked, took a held lock");
    }
    if after != Some(raised) {
        return Err("a failed try_lock, with interrupts masked, left the count changed");
    }
    if switched {
        return Err("a failed try_lock, with interrupts masked, switched");
    }
    drop(guard);

    let plain = PLAIN.lock();
    // SAFETY: (SHARED) the lock is held by `plain`, so the attempt is refused
    // and hands out nothing to release.
    let taken = unsafe { PLAIN.try_lock_manually() }.is_some();
    drop(plain);
    if taken {
        return Err("try_lock_manually took a held lock");
    }
    if word_here().map(|(_, count, held)| (count, held)) != Some((count, held)) {
        return Err("letting go, or a refused try_lock_manually, left the count changed");
    }

    let taken = LOCK.try_lock().ok_or("a try_lock of a free lock failed")?;
    let during = word_here().map(|(_, count, held)| (count, held));
    drop(taken);
    if during != Some((count + 1, held + 1))
        || word_here().map(|(_, count, held)| (count, held)) != Some((count, held))
    {
        return Err("a successful try_lock did not raise the count by one for as long as it held");
    }
    Ok(())
}

/// Tasks a timer preempts and that move between processors take and let go
/// of locks [`PAIRS`] times each, and then every processor's count and locks
/// held read zero from a task there that holds nothing.
///
/// What it would catch: an update that lands on the record of a processor
/// the task has left, which leaves that processor's count raised for good
/// (and is then FX-0503 at its next switch), and one lost to an interrupt.
/// It reads the count from tasks that hold nothing, on each processor in
/// turn, because another processor's word read from here belongs to
/// whatever runs there at that instant.
///
/// Verifies: L.sched.20
fn the_count_survives_preemption_and_moves(topology: &Topology) -> Result<(), &'static str> {
    let online = topology.online();
    let started = crate::timer::now_nanos();
    FINISHED.store(0, Ordering::Release);
    MOVES.store(0, Ordering::Release);
    RAISED.store(false, Ordering::Release);

    let tasks = online.saturating_mul(TASKS_PER_CPU).min(LOCKS);
    let mut running: Vec<Arc<Task>> = Vec::new();
    for index in 0..tasks {
        running.push(super::spawn(
            "check-pairs",
            take_pairs,
            index,
            NICE_0_WEIGHT,
        )?);
    }
    wait_finished(
        tasks as u64,
        "a task of the moving lock check never finished",
    )?;

    let probes = online as u64;
    for cpu in 0..online {
        running.push(super::spawn_on(
            "check-count",
            probe_count,
            cpu,
            NICE_0_WEIGHT,
            cpu,
            CpuSet::of(cpu),
        )?);
    }
    wait_finished(
        tasks as u64 + probes,
        "a processor's count probe never finished",
    )?;
    for task in &running {
        super::wait_until_gone(task, super::REAPER_PATIENCE_NANOS)?;
    }
    drop(running);

    let moves = MOVES.load(Ordering::Acquire);
    crate::console::println!(
        "  preempt  {tasks} tasks x {PAIRS} lock pairs, {moves} moves, {} ms",
        crate::timer::now_nanos().saturating_sub(started) / 1_000_000,
    );
    if RAISED.load(Ordering::Acquire) {
        let word = RAISED_WORD.load(Ordering::Relaxed);
        crate::console::println!(
            "  preempt  processor {} read count {} and locks held {} with nothing held",
            word >> 48,
            word & 0xFFFF_FFFF,
            (word >> 32) & 0xFFFF,
        );
        return Err("a processor's preemption count was not zero after the lock pairs");
    }
    if online >= 2 && moves == 0 {
        return Err("the moving lock check's tasks never moved between processors");
    }
    Ok(())
}

/// One task of the moving check: [`PAIRS`] lock pairs, with raises by hand
/// and sleeps among them, then a look at its own processor's count.
fn take_pairs(index: usize) {
    let lock = PAIR_LOCKS.get(index % LOCKS);
    let mut last = word_here().map(|(cpu, _, _)| cpu);
    for pair in 1..=PAIRS {
        if let Some(lock) = lock {
            let mut guard = lock.lock();
            *guard = guard.wrapping_add(1);
        }
        if pair % BY_HAND_EVERY == 0 {
            super::preempt_disable();
            super::preempt_enable();
        }
        if pair % SLEEP_EVERY == 0 {
            super::sleep_for(SLEEP_NANOS);
            let now = word_here().map(|(cpu, _, _)| cpu);
            if now != last {
                let _ = MOVES.fetch_add(1, Ordering::Relaxed);
                last = now;
            }
        }
    }
    look_at_own_count();
    let _ = FINISHED.fetch_add(1, Ordering::AcqRel);
}

/// One probe: a task pinned to a processor, holding nothing, reads that
/// processor's count and locks held.
fn probe_count(_cpu: usize) {
    look_at_own_count();
    let _ = FINISHED.fetch_add(1, Ordering::AcqRel);
}

/// Require this processor's count and locks held to be zero, from a task that
/// holds nothing.
fn look_at_own_count() {
    if let Some((cpu, count, held)) = word_here()
        && (count != 0 || held != 0)
    {
        note_raised(cpu, count, held);
    }
}
