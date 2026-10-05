//! The preemption count's checks (OPAQUE-KERNEL.md §9.8, 2b): the count kept
//! in each processor's record without a locked operation survives tasks a
//! timer preempts and moves; an underflow is refused; `may_block` reads its
//! own processor's count; and a failed `try_lock` leaves the count as it
//! found it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
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

static FIRST_AT: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
static FIRST_CPU: [AtomicU64; 32] = [const { AtomicU64::new(9) }; 32];
static START_AT: AtomicU64 = AtomicU64::new(0);

/// Run them, in order.
///
/// # Errors
///
/// The first that fails, as a sentence.
pub(super) fn run(topology: &Topology) -> Result<(), &'static str> {
    an_underflow_is_refused()?;
    may_block_reads_its_own_count()?;
    a_failed_try_leaves_the_count()?;
    let mut zero = 0u32;
    let mut low = 0u32;
    for round in 0..400u32 {
        let moves = the_count_survives_preemption_and_moves(topology, round)?;
        if moves == 0 { zero += 1; }
        if moves < 10 { low += 1; }
    }
    crate::console::println!("  DIAG preempt 400 rounds: {zero} with no move, {low} under 10");
    Ok(())
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

/// Wait for the moving check's `count` tasks; until one of them has been
/// seen to move, stay on this processor rather than sleep, and move one
/// that waits its turn to another processor every millisecond.
///
/// **The first move is made, not hoped for.** Left to itself the scheduler
/// moves a task only when an idle processor steals one that waits, and
/// these tasks sleep far more than they run: all eight are less than one
/// processor's work. A boot under KVM on a loaded host (main 77783565a,
/// `batch-20261004T194624Z-b0-1`, 2026-10-04) put all eight on the
/// checker's processor: the processors they were placed on had not yet
/// woken from their kicks when the checker went to sleep, its processor
/// went idle and stole each one before it first ran, the three it robbed
/// halted again with nothing to wake them, and the tasks finished where
/// they had started, with no move to see. Starving two virtual processors
/// of host time does the same 2 rounds in 400.
///
/// So the checker takes a waiting task off one processor's queue and puts
/// it on the next one's, through the scheduler's own
/// [`super::steal_from`] -- the move a stealing processor makes, of a task
/// the timer may have preempted -- and has the receiver decide. It spins
/// meanwhile, because a sleeping checker leaves its processor idle, and an
/// idle processor steals the moved task back before the receiver wakes:
/// moved that way and slept on, the 2 rounds in 400 stayed 1. The moves
/// the scheduler makes are counted as before; the checker only stops
/// spinning once a task has seen itself on another processor.
#[allow(dead_code)]
fn wait_moving(count: u64, online: usize) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let mut from = 0;
    while FINISHED.load(Ordering::Acquire) < count {
        let now = crate::timer::now_nanos();
        if now >= deadline {
            return Err("a task of the moving lock check never finished");
        }
        if online < 2 || MOVES.load(Ordering::Acquire) > 0 {
            super::sleep_for(1_000_000);
            continue;
        }
        move_one(online, &mut from);
        let next = now.saturating_add(1_000_000);
        while MOVES.load(Ordering::Acquire) == 0
            && FINISHED.load(Ordering::Acquire) < count
            && crate::timer::now_nanos() < next
        {
            core::hint::spin_loop();
        }
    }
    Ok(())
}

/// Move one task waiting its turn on a processor's queue to the next
/// processor's, starting the search at `from`, which is left at the
/// processor after the one moved from; and have the receiver decide, as a
/// balancing pull does. Nothing moves when no task waits anywhere.
fn move_one(online: usize, from: &mut usize) {
    for _ in 0..online {
        let victim = *from % online;
        let to = (victim + 1) % online;
        *from = to;
        if super::steal_from(to, victim) {
            let saved = <crate::arch::Irq as IrqControl>::disable();
            if super::this_cpu() == Some(to) {
                super::resched_here(to);
            } else {
                super::kick(to);
            }
            <crate::arch::Irq as IrqControl>::restore(saved);
            return;
        }
    }
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
fn the_count_survives_preemption_and_moves(topology: &Topology, round: u32) -> Result<u64, &'static str> {
    let online = topology.online();
    let steals0 = super::summary().steals;
    let balanced0 = super::balanced_count();
    let placed0 = super::placed_elsewhere();
    let mut spawned_on = [0u8; 32];
    let mut before = [(0u64, 0u64, 0u64, false); 4];
    for (cpu, slot) in before.iter_mut().enumerate() { *slot = super::diag_cpu(cpu); }
    START_AT.store(crate::timer::now_nanos(), Ordering::Release);
    let evals0 = super::DIAG_EVALS.load(Ordering::Relaxed);
    let mut queues_before = [([("-", false, 0u32); 4], 0usize, false, false); 4];
    for (cpu, slot) in queues_before.iter_mut().enumerate() { *slot = super::diag_queue(cpu); }
    let started = crate::timer::now_nanos();
    FINISHED.store(0, Ordering::Release);
    MOVES.store(0, Ordering::Release);
    RAISED.store(false, Ordering::Release);

    let tasks = online.saturating_mul(TASKS_PER_CPU).min(LOCKS);
    let mut running: Vec<Arc<Task>> = Vec::new();
    for index in 0..tasks {
        let task = super::spawn(
            "check-pairs",
            take_pairs,
            index,
            NICE_0_WEIGHT,
        )?;
        if let Some(slot) = spawned_on.get_mut(index) { *slot = task.cpu() as u8; }
        running.push(task);
    }
    let mut ended_on = [0u8; 32];
    let spawned_ms100 = (crate::timer::now_nanos() - started) / 100_000;
    let mut idle_after_spawn = [false; 4];
    for (cpu, slot) in idle_after_spawn.iter_mut().enumerate() { *slot = super::diag_cpu(cpu).3; }
    wait_finished(tasks as u64, "a task of the moving lock check never finished")?;

    for (index, task) in running.iter().enumerate() {
        if let Some(slot) = ended_on.get_mut(index) { *slot = task.cpu() as u8; }
    }
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
    let ms = crate::timer::now_nanos().saturating_sub(started) / 1_000_000;
    if round % 100 == 0 {
        for cpu in 0..4 {
            let q = super::diag_queue(cpu);
            crate::console::println!("  DIAG    r{round} after cpu{cpu}: {} on fair {:?}, idle task current {}, IDLE bit {}", q.1, q.0, q.2, q.3);
        }
    }
    if moves < 10 || round % 50 == 0 {
        crate::console::println!(
            "  DIAG r{round} {tasks} tasks, {moves} moves, {ms} ms, steals {} balanced {} placed {} spawned {:?} ended {:?}",
            super::summary().steals - steals0,
            super::balanced_count() - balanced0,
            super::placed_elsewhere() - placed0,
            &spawned_on[..tasks],
            &ended_on[..tasks],
        );
        if moves < 10 {
            let mut first = [(0u64, 0u64); 8];
            for (index, slot) in first.iter_mut().enumerate() {
                *slot = (FIRST_CPU[index].load(Ordering::Relaxed), FIRST_AT[index].load(Ordering::Relaxed) / 100_000);
            }
            let mut per = [(0u64, 0u64, 0u64); 4];
            for (cpu, slot) in per.iter_mut().enumerate() {
                let now = super::diag_cpu(cpu);
                *slot = (now.0 - before[cpu].0, now.1 - before[cpu].1, now.2 - before[cpu].2);
            }
            let mine = super::DIAG_MINE.load(Ordering::Relaxed);
            let other = super::DIAG_OTHER.load(Ordering::Relaxed);
            crate::console::println!(
                "  DIAG    cpu0 balance evaluations {}; last: cpu0 queued {} average {}; cpu1 queued {} idle {} average {}",
                super::DIAG_EVALS.load(Ordering::Relaxed) - evals0,
                mine >> 32, mine & 0xFFFF_FFFF, other >> 40, (other >> 32) & 1, other & 0xFFFF_FFFF,
            );
            for (cpu, q) in queues_before.iter().enumerate() {
                crate::console::println!("  DIAG    before spawn cpu{cpu}: {} on fair {:?}, idle task current {}, IDLE bit {}", q.1, q.0, q.2, q.3);
            }
            crate::console::println!(
                "  DIAG    spawning took {spawned_ms100} x0.1ms; idle after spawn {:?}; first (cpu, x0.1ms) {:?}; per cpu (switches, in, out) {:?}",
                idle_after_spawn, first, per,
            );
        }
    }
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
    Ok(moves)
}

/// One task of the moving check: [`PAIRS`] lock pairs, with raises by hand
/// and sleeps among them, then a look at its own processor's count.
fn take_pairs(index: usize) {
    let lock = PAIR_LOCKS.get(index % LOCKS);
    let mut last = word_here().map(|(cpu, _, _)| cpu);
    if let (Some(at), Some(c)) = (FIRST_AT.get(index), FIRST_CPU.get(index)) {
        at.store(crate::timer::now_nanos() - START_AT.load(Ordering::Acquire), Ordering::Relaxed);
        c.store(last.unwrap_or(9) as u64, Ordering::Relaxed);
    }
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
