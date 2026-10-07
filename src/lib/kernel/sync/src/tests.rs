//! Host tests for the synchronisation primitives.
//!
//! A lock that is only exercised by one thread is not exercised at all, so
//! these tests use real OS threads. That is the whole reason this crate is
//! worth testing on the host: the kernel's own CPUs cannot be started under
//! `cargo test`, but `std::thread` produces the same interleavings, and on a
//! multi-core developer machine it produces them on real cores.
//!
//! The host differs from the kernel in one way that matters here: threads are
//! preempted and kernel CPUs inside a critical section are not. Where a timing
//! bound is asserted below it is therefore generous, and the tolerances say so.

extern crate std;

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::boxed::Box;
use std::format;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use std::vec;
use std::vec::Vec;

use super::{
    HookedPreempt, IrqControl, IrqSpinLock, Once, Parker, Parking, PreemptControl, PreemptHooks,
    PreemptSpinLock, RwSpinLock, SleepLock, SpinLock, SpinLockedCell, SpinParker,
    install_preempt_hooks,
};

// ---------------------------------------------------------------------------
// SpinLock, single-threaded
// ---------------------------------------------------------------------------

#[test]
fn guard_mutation_is_visible_after_release() {
    let lock = SpinLock::new(1_u32);
    *lock.lock() += 41;
    assert_eq!(
        *lock.lock(),
        42,
        "the write through the guard must have stuck"
    );
}

#[test]
fn try_lock_fails_while_a_guard_is_alive() {
    let lock = SpinLock::new(0_u32);
    let guard = lock.lock();
    assert!(
        lock.try_lock().is_none(),
        "a held lock must refuse a second holder"
    );
    drop(guard);
    assert!(
        lock.try_lock().is_some(),
        "and must accept one once released"
    );
}

#[test]
fn is_locked_follows_the_guard() {
    let lock = SpinLock::new(0_u32);
    assert!(!lock.is_locked(), "a fresh lock is free");
    let guard = lock.lock();
    assert!(lock.is_locked(), "holding a guard means the lock is held");
    drop(guard);
    assert!(!lock.is_locked(), "dropping the guard releases it");
}

#[test]
fn try_lock_is_reusable_after_its_guard_drops() {
    let lock = SpinLock::new(0_u32);
    for round in 0..4 {
        let mut guard = lock.try_lock().unwrap();
        *guard += 1;
        drop(guard);
        assert_eq!(*lock.lock(), round + 1, "each round adds exactly one");
    }
}

#[test]
fn a_failed_manual_attempt_leaves_the_lock_as_it_found_it() {
    let lock = SpinLock::new(0_u32);
    let guard = lock.lock();
    // SAFETY: a refusal hands out nothing to release.
    let refused = unsafe { lock.try_lock_manually() }.is_none();
    assert!(refused, "a held lock refuses a manual attempt");
    assert!(lock.is_locked(), "and is still held by its guard");
    drop(guard);
    assert!(
        !lock.is_locked(),
        "the refused attempt took no ticket, so the guard's release frees it"
    );
    // SAFETY: released once below with `force_unlock`, and the reference is
    // not used after that.
    let data = unsafe { lock.try_lock_manually() }.unwrap();
    *data += 1;
    assert!(lock.is_locked(), "a successful attempt holds the lock");
    assert!(lock.try_lock().is_none(), "against every other holder");
    // SAFETY: taken manually above; `data` is not used again.
    unsafe { lock.force_unlock() };
    assert_eq!(*lock.lock(), 1, "and its write stuck");
}

#[test]
fn get_mut_reaches_the_data_without_locking() {
    let mut lock = SpinLock::new(7_u32);
    *lock.get_mut() = 9;
    assert!(!lock.is_locked(), "get_mut must not have taken the lock");
    assert_eq!(
        lock.into_inner(),
        9,
        "into_inner returns what get_mut wrote"
    );
}

#[test]
fn ticket_counters_wrap_together_rather_than_overflow() {
    // A lock acquired often enough will wrap its counters. The invariant is
    // equality of the two, not their absolute value, so wrapping is harmless
    // as long as both wrap the same way.
    let lock = SpinLock::new(0_u32);
    for _ in 0..1000 {
        *lock.lock() += 1;
    }
    assert_eq!(
        *lock.lock(),
        1000,
        "a thousand acquisitions, a thousand increments"
    );
    assert!(!lock.is_locked(), "and the lock ends free");
}

#[test]
fn debug_of_a_held_lock_does_not_deadlock() {
    let lock = SpinLock::new(5_u32);
    let guard = lock.lock();
    let text = format!("{lock:?}");
    assert!(
        text.contains("<locked>"),
        "a held lock must format as locked, got {text}"
    );
    drop(guard);
    let text = format!("{lock:?}");
    assert!(
        text.contains('5'),
        "a free lock must format its value, got {text}"
    );
}

// ---------------------------------------------------------------------------
// Contention
//
// The reason this crate is worth testing on a host at all: a lock that works
// when nothing is contending it is not a lock. Every test below starts its
// threads on a barrier so they genuinely race rather than run in sequence.
// ---------------------------------------------------------------------------

/// Threads used by the contention tests. More than the usual core count, so
/// some of them are descheduled while holding or waiting for the lock.
const THREADS: usize = 8;

/// Held by each contention test for its whole run, so they take turns.
///
/// The test harness runs as many tests at once as there are cores, and two
/// or three of these at once are twenty threads spinning on four cores: a
/// ticket lock's handover then waits whole timeslices for a descheduled
/// waiter, and on CI's four-core runners one run in a few took ten seconds
/// to nine minutes. One at a time, eight threads still outnumber the cores.
static CONTENTION: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Wait for [`CONTENTION`]; a test that failed holding it poisons nothing
/// the next one needs.
fn contend() -> std::sync::MutexGuard<'static, ()> {
    CONTENTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn eight_threads_agree_on_the_count() {
    let _turn = contend();
    // Past one thread per core, a ticket lock's handover waits for the next
    // ticket's holder to be scheduled, a timeslice at a time: on CI's
    // four-core runners the full count took nine minutes, the whole host
    // test step's length. Fewer increments there still race eight threads,
    // some descheduled, for the lock.
    let cores = thread::available_parallelism().map_or(1, usize::from);
    let per_thread = if cores >= THREADS { 10_000 } else { 2_000 };

    // Repeated, because a lost update is a race, and a race that shows up one
    // run in ten is still a bug.
    for attempt in 0..5 {
        let counter = Arc::new(SpinLock::new(0usize));
        let start = Arc::new(Barrier::new(THREADS));

        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let counter = Arc::clone(&counter);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    let _ = start.wait();
                    for _ in 0..per_thread {
                        *counter.lock() += 1;
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            *counter.lock(),
            THREADS * per_thread,
            "attempt {attempt}: an increment was lost, so two threads held the lock at once"
        );
    }
}

#[test]
fn the_ticket_lock_starves_nobody() {
    let _turn = contend();
    // A test-and-set lock lets whichever CPU happens to win keep winning, and a
    // thread can wait indefinitely. A ticket lock hands out in arrival order,
    // so acquisitions by different threads must interleave heavily rather than
    // run in long unbroken stretches.
    //
    // That only shows if the threads are actually queued on the lock. Released
    // from a barrier, they are not: two hundred acquisitions take microseconds,
    // waking a thread on a CI VM takes longer, and a correct ticket lock scored
    // one handover in four hundred because one thread had finished before the
    // other woke. So the lock is held here until every worker has taken a
    // ticket. From then on a thread that finishes an acquisition re-queues
    // behind the others already waiting, and a lock that serves in arrival
    // order has to rotate between them -- whatever the scheduler does.
    //
    // The cap is for speed, not correctness: past one thread per core the
    // rotation still holds, but each handover can wait out a timeslice.
    const PER_THREAD: usize = 200;

    let threads = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(THREADS);
    if threads < 2 {
        // One core cannot show fairness at all: nothing ever waits while
        // another thread holds the lock. No runner this tree uses is that small.
        return;
    }

    let order = Arc::new(SpinLock::new(Vec::new()));
    let held = order.lock();

    let workers: Vec<_> = (0..threads)
        .map(|id| {
            let order = Arc::clone(&order);
            thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    order.lock().push(id);
                }
            })
        })
        .collect();

    // Tickets outstanding: this thread's, plus one per worker waiting behind it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while order
        .next_ticket
        .load(Ordering::Relaxed)
        .wrapping_sub(order.now_serving.load(Ordering::Relaxed))
        != threads + 1
    {
        assert!(
            Instant::now() < deadline,
            "the workers never all queued for the lock"
        );
        thread::yield_now();
    }
    drop(held);

    for worker in workers {
        worker.join().unwrap();
    }

    let acquisitions = order.lock();
    assert_eq!(
        acquisitions.len(),
        threads * PER_THREAD,
        "every acquisition should have been recorded"
    );

    let mut seen = [0usize; THREADS];
    for &id in acquisitions.iter() {
        seen[id] += 1;
    }
    for (id, count) in seen.iter().take(threads).enumerate() {
        assert_eq!(*count, PER_THREAD, "thread {id} did not finish its work");
    }

    let switches = acquisitions
        .windows(2)
        .filter(|pair| pair[0] != pair[1])
        .count();
    assert!(
        switches > acquisitions.len() / 10,
        "only {switches} handovers in {} acquisitions: the lock is not handing out in \
         arrival order, which is how a waiter starves",
        acquisitions.len()
    );
}

// ---------------------------------------------------------------------------
// Once
// ---------------------------------------------------------------------------

#[test]
fn once_runs_its_initialiser_exactly_once() {
    let _turn = contend();
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    RUNS.store(0, Ordering::SeqCst);

    let cell: Arc<Once<usize>> = Arc::new(Once::new());
    let start = Arc::new(Barrier::new(THREADS));
    assert!(!cell.is_completed(), "a fresh Once holds nothing");
    assert_eq!(cell.get(), None);

    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let cell = Arc::clone(&cell);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let _ = start.wait();
                *cell.call_once(|| {
                    // Slow on purpose: without it the first caller finishes
                    // before the others arrive and nothing is actually raced.
                    thread::sleep(Duration::from_millis(20));
                    let _ = RUNS.fetch_add(1, Ordering::SeqCst);
                    99
                })
            })
        })
        .collect();

    for worker in workers {
        assert_eq!(
            worker.join().unwrap(),
            99,
            "every caller must see the one value that was produced"
        );
    }

    assert_eq!(
        RUNS.load(Ordering::SeqCst),
        1,
        "the initialiser ran twice, so two callers both believed they were first"
    );
    assert!(cell.is_completed());
    assert_eq!(cell.get(), Some(&99));
}

#[test]
fn a_late_caller_sees_the_value_without_rerunning_the_initialiser() {
    let runs = AtomicUsize::new(0);
    let cell: Once<usize> = Once::new();

    let mut initialise = || {
        let _ = runs.fetch_add(1, Ordering::SeqCst);
        7
    };
    assert_eq!(*cell.call_once(&mut initialise), 7);
    assert_eq!(
        *cell.call_once(&mut initialise),
        7,
        "the value is unchanged"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the second caller must not run the initialiser again"
    );
}

// ---------------------------------------------------------------------------
// Reader-writer
// ---------------------------------------------------------------------------

#[test]
fn readers_really_are_concurrent() {
    let _turn = contend();
    // The whole point of a reader-writer lock: several readers hold it at once.
    // A peak of one would mean it is an expensive mutex.
    let lock = Arc::new(RwSpinLock::new(0usize));
    let peak = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(THREADS));

    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let lock = Arc::clone(&lock);
            let peak = Arc::clone(&peak);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let _ = start.wait();
                let guard = lock.read();
                let now = lock.reader_count();
                let _ = peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(30));
                drop(guard);
            })
        })
        .collect();

    for worker in workers {
        worker.join().unwrap();
    }
    assert!(
        peak.load(Ordering::SeqCst) > 1,
        "readers never overlapped, so this is a mutex rather than a reader-writer lock"
    );
    assert_eq!(lock.reader_count(), 0, "every reader released");
}

#[test]
fn a_writer_excludes_everyone() {
    let lock = RwSpinLock::new(5usize);
    let guard = lock.write();
    assert!(lock.is_write_locked());
    assert!(lock.try_read().is_none(), "a reader must not join a writer");
    assert!(lock.try_write().is_none(), "nor may a second writer");
    drop(guard);
    assert!(lock.try_read().is_some(), "and both may proceed afterwards");
}

#[test]
fn a_reader_excludes_a_writer_but_not_another_reader() {
    let lock = RwSpinLock::new(5usize);
    let first = lock.read();
    let second = lock.try_read();
    assert!(second.is_some(), "a second reader may join the first");
    assert!(lock.try_write().is_none(), "a writer may not");
    drop(second);
    drop(first);
    assert!(lock.try_write().is_some());
}

#[test]
fn a_writer_is_not_starved_by_a_stream_of_readers() {
    let _turn = contend();
    // With reader preference, a continuous arrival of readers keeps a waiting
    // writer out forever. This asserts the writer gets in within a bound, which
    // is the property the kernel's mount table will depend on.
    let lock = Arc::new(RwSpinLock::new(0usize));
    let stop = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..THREADS)
        .map(|_| {
            let lock = Arc::clone(&lock);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let guard = lock.read();
                    thread::sleep(Duration::from_micros(200));
                    drop(guard);
                }
            })
        })
        .collect();

    // Let the readers get going, so the writer really does arrive behind them.
    thread::sleep(Duration::from_millis(20));

    let began = Instant::now();
    {
        let mut guard = lock.write();
        *guard = 1;
    }
    let waited = began.elapsed();

    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }

    assert!(
        waited < Duration::from_secs(2),
        "the writer waited {waited:?} behind a stream of readers, which is starvation"
    );
    assert_eq!(*lock.read(), 1, "and its write took effect");
}

// ---------------------------------------------------------------------------
// Interrupt-safe locking
// ---------------------------------------------------------------------------

/// Records how the lock masks and restores interrupts.
///
/// The real implementation writes a CPU flag; this one counts, so a test can
/// assert the calls are balanced and correctly ordered against the lock itself.
/// Taking a plain spin lock in a handler that interrupted its own holder is a
/// guaranteed self-deadlock, and this is the type that prevents it.
struct RecordingIrq;

static IRQ_DEPTH: AtomicUsize = AtomicUsize::new(0);
static IRQ_DISABLES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: this masks nothing at all -- there are no interrupts in a host test
// -- but it does honour the part of the contract the lock relies on: `restore`
// puts back exactly the state its own `disable` reported.
unsafe impl IrqControl for RecordingIrq {
    fn disable() -> usize {
        let _ = IRQ_DISABLES.fetch_add(1, Ordering::SeqCst);
        IRQ_DEPTH.fetch_add(1, Ordering::SeqCst)
    }

    fn restore(state: usize) {
        let previous = IRQ_DEPTH.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(
            previous.saturating_sub(1),
            state,
            "restore must return to the depth its own disable reported"
        );
    }
}

/// A preemption count that records every change, so a test can see that the
/// lock raised it before spinning and lowered it after releasing.
struct RecordingPreempt;

static PREEMPT_DEPTH: AtomicUsize = AtomicUsize::new(0);
static PREEMPT_DISABLES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: a test double; nothing here schedules, so "kept on its CPU" is
// vacuously true, and the count is what the test inspects.
unsafe impl PreemptControl for RecordingPreempt {
    fn disable() {
        let _ = PREEMPT_DEPTH.fetch_add(1, Ordering::SeqCst);
        let _ = PREEMPT_DISABLES.fetch_add(1, Ordering::SeqCst);
    }
    fn enable() {
        let _ = PREEMPT_DEPTH.fetch_sub(1, Ordering::SeqCst);
    }
}

#[test]
fn a_preempt_lock_keeps_the_task_for_exactly_the_time_it_is_held() {
    static LOCK: PreemptSpinLock<u32, RecordingPreempt> = PreemptSpinLock::new(0);
    let disables = PREEMPT_DISABLES.load(Ordering::SeqCst);
    {
        let mut guard = LOCK.lock();
        *guard += 1;
        assert!(
            PREEMPT_DEPTH.load(Ordering::SeqCst) >= 1,
            "the count is raised while the lock is held"
        );
        assert!(LOCK.is_locked(), "and the lock is held");
    }
    assert!(!LOCK.is_locked(), "released on drop");
    assert_eq!(
        PREEMPT_DISABLES.load(Ordering::SeqCst),
        disables + 1,
        "one disable per acquisition"
    );
    // A refused try_lock leaves the count where it found it.
    let held = LOCK.lock();
    let depth = PREEMPT_DEPTH.load(Ordering::SeqCst);
    assert!(LOCK.try_lock().is_none(), "refused while held");
    assert_eq!(
        PREEMPT_DEPTH.load(Ordering::SeqCst),
        depth,
        "and the count is untouched"
    );
    drop(held);
    assert_eq!(*LOCK.lock(), 1, "the data survived");
}

/// A count of its own for the masked acquisition's test, which no other
/// test shares.
struct MaskedPreempt;

static MASKED_TOUCHES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: a test double; nothing here schedules. It counts every call, which
// the test requires to stay at zero across a masked hold.
unsafe impl PreemptControl for MaskedPreempt {
    fn disable() {
        let _ = MASKED_TOUCHES.fetch_add(1, Ordering::SeqCst);
    }
    fn enable() {
        let _ = MASKED_TOUCHES.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_masked_try_lock_excludes_every_holder_and_leaves_the_count_alone() {
    static LOCK: PreemptSpinLock<u32, MaskedPreempt> = PreemptSpinLock::new(0);
    // SAFETY: a host test has no interrupts and nothing here blocks.
    let mut masked = unsafe { LOCK.try_lock_masked() }.expect("free");
    *masked += 1;
    assert_eq!(MASKED_TOUCHES.load(Ordering::SeqCst), 0, "no disable");
    assert!(LOCK.try_lock().is_none(), "a counted try_lock is refused");
    // SAFETY: as above.
    let second = unsafe { LOCK.try_lock_masked() };
    assert!(second.is_none(), "so is a masked one");
    let touched = MASKED_TOUCHES.load(Ordering::SeqCst);
    drop(masked);
    assert_eq!(MASKED_TOUCHES.load(Ordering::SeqCst), touched, "no enable");
    let counted = LOCK.lock();
    // SAFETY: as above.
    let under = unsafe { LOCK.try_lock_masked() };
    assert!(under.is_none(), "refused under a counted holder");
    assert_eq!(*counted, 1, "the data survived");
}

#[test]
fn an_irq_lock_masks_for_exactly_the_time_it_is_held() {
    IRQ_DEPTH.store(0, Ordering::SeqCst);
    IRQ_DISABLES.store(0, Ordering::SeqCst);

    let lock: IrqSpinLock<usize, RecordingIrq> = IrqSpinLock::new(0);
    assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 0, "nothing masked yet");

    {
        let mut guard = lock.lock();
        assert_eq!(
            IRQ_DEPTH.load(Ordering::SeqCst),
            1,
            "interrupts must be masked before the lock is taken"
        );
        *guard = 7;
    }

    assert_eq!(
        IRQ_DEPTH.load(Ordering::SeqCst),
        0,
        "and unmasked again when the guard drops"
    );
    assert_eq!(IRQ_DISABLES.load(Ordering::SeqCst), 1);
    assert_eq!(*lock.lock(), 7, "and the write took effect");
}

#[test]
fn nested_irq_locks_restore_in_order() {
    IRQ_DEPTH.store(0, Ordering::SeqCst);
    let outer: IrqSpinLock<usize, RecordingIrq> = IrqSpinLock::new(1);
    let inner: IrqSpinLock<usize, RecordingIrq> = IrqSpinLock::new(2);

    let first = outer.lock();
    let second = inner.lock();
    assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 2, "both are masking");
    drop(second);
    assert_eq!(
        IRQ_DEPTH.load(Ordering::SeqCst),
        1,
        "the inner one unmasked"
    );
    drop(first);
    assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// The convenience wrapper
// ---------------------------------------------------------------------------

#[test]
fn a_locked_cell_holds_nothing_until_it_is_set() {
    let cell: SpinLockedCell<Vec<usize>> = SpinLockedCell::new();
    assert!(!cell.is_set(), "a fresh cell is empty");
    assert_eq!(cell.with(Vec::len), None, "and reading it yields nothing");

    assert_eq!(
        cell.set(vec![1, 2, 3]),
        None,
        "setting an empty cell displaces nothing"
    );
    assert!(cell.is_set());
    assert_eq!(cell.with(Vec::len), Some(3));

    let _ = cell.with_mut(|values| values.push(4));
    assert_eq!(cell.with(Vec::len), Some(4), "with_mut writes through");

    assert_eq!(
        cell.set(vec![9]),
        Some(vec![1, 2, 3, 4]),
        "setting again returns the old value"
    );
    assert_eq!(cell.take(), Some(vec![9]));
    assert!(!cell.is_set(), "taking empties it");
    assert_eq!(cell.take(), None);
}

#[test]
fn a_locked_cell_is_shared_safely() {
    let _turn = contend();
    let cell: Arc<SpinLockedCell<usize>> = Arc::new(SpinLockedCell::with_value(0));
    let start = Arc::new(Barrier::new(THREADS));

    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let cell = Arc::clone(&cell);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let _ = start.wait();
                for _ in 0..1000 {
                    let _ = cell.with_mut(|value| *value += 1);
                }
            })
        })
        .collect();

    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        cell.with(|value| *value),
        Some(THREADS * 1000),
        "an increment was lost through the cell"
    );
}

// ---------------------------------------------------------------------------
// SleepLock
// ---------------------------------------------------------------------------

#[test]
fn a_sleep_lock_guards_its_data_like_a_spin_lock() {
    let lock = SleepLock::new(1_u32, &SpinParker);
    *lock.lock() += 41;
    assert_eq!(
        *lock.lock(),
        42,
        "the write through the guard must have stuck"
    );
    let guard = lock.lock();
    assert!(lock.is_locked(), "a held lock reports itself held");
    assert!(
        lock.try_lock().is_none(),
        "a held lock must refuse a second holder"
    );
    drop(guard);
    assert!(!lock.is_locked(), "and free once the guard drops");
    assert!(
        lock.try_lock().is_some(),
        "and must accept one once released"
    );
    assert_eq!(lock.into_inner(), 42);
}

#[test]
fn a_sleep_lock_debugs_without_waiting() {
    let lock = SleepLock::new(7_u8, &SpinParker);
    assert_eq!(format!("{lock:?}"), "SleepLock { data: 7 }");
    let guard = lock.lock();
    assert_eq!(
        format!("{lock:?}"),
        "SleepLock { data: <locked> }",
        "formatting a held lock must neither wait nor deadlock"
    );
    assert_eq!(format!("{guard:?}"), "7");
    assert_eq!(format!("{guard}"), "7");
}

#[test]
fn eight_threads_agree_on_a_sleep_locked_count() {
    let _turn = contend();
    const THREADS: usize = 8;
    const PER_THREAD: usize = 2_000;
    let lock = Arc::new(SleepLock::new(0_usize, &SpinParker));
    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let lock = Arc::clone(&lock);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let _ = start.wait();
                for _ in 0..PER_THREAD {
                    *lock.lock() += 1;
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("a counting thread panicked");
    }
    assert_eq!(*lock.lock(), THREADS * PER_THREAD);
}

/// A parker built on the host's condition variable: what the kernel's wait
/// queue does, on `std`, and counting how often a waiter actually slept.
#[derive(Debug, Default)]
struct CondvarParker {
    parks: Arc<AtomicUsize>,
    asked: Arc<AtomicUsize>,
    unparks: Arc<AtomicUsize>,
}

/// One lock's waiters. `generation` is the mutex the condition variable
/// waits on; it is taken around the last look at `ready` and around every
/// notify, which is the ordering the `Parking` contract asks for.
struct CondvarParking {
    generation: std::sync::Mutex<()>,
    woken: std::sync::Condvar,
    parks: Arc<AtomicUsize>,
    /// How often the lock asked whether its caller may park.
    asked: Arc<AtomicUsize>,
    /// How often a release woke the parking.
    unparks: Arc<AtomicUsize>,
}

impl Parker for CondvarParker {
    fn new_parking(&self) -> Option<Box<dyn Parking>> {
        Some(Box::new(CondvarParking {
            generation: std::sync::Mutex::new(()),
            woken: std::sync::Condvar::new(),
            parks: Arc::clone(&self.parks),
            asked: Arc::clone(&self.asked),
            unparks: Arc::clone(&self.unparks),
        }))
    }
}

impl Parking for CondvarParking {
    fn park_until(&self, ready: &mut dyn FnMut() -> bool) {
        let mut held = self.generation.lock().expect("no poison");
        while !ready() {
            let _ = self.parks.fetch_add(1, Ordering::Relaxed);
            held = self.woken.wait(held).expect("no poison");
        }
    }

    fn unpark_all(&self) {
        let _ = self.unparks.fetch_add(1, Ordering::Relaxed);
        let _held = self.generation.lock().expect("no poison");
        self.woken.notify_all();
    }

    fn may_park(&self) {
        let _ = self.asked.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn every_attempt_at_a_sleep_lock_asks_whether_it_may_park() {
    let parker = CondvarParker::default();
    let lock = SleepLock::new((), &parker);
    let held = lock.lock();
    assert_eq!(parker.asked.load(Ordering::Relaxed), 1, "lock asks");
    assert!(lock.try_lock().is_none());
    assert_eq!(
        parker.asked.load(Ordering::Relaxed),
        2,
        "try_lock asks too, whether or not it would have slept"
    );
    drop(held);
    assert!(lock.try_lock().is_some());
    assert_eq!(parker.asked.load(Ordering::Relaxed), 3);
    assert_eq!(parker.parks.load(Ordering::Relaxed), 0, "nothing slept");
}

#[test]
fn a_waiter_sleeps_on_the_parking_and_the_release_wakes_it() {
    let parker = CondvarParker::default();
    let lock = Arc::new(SleepLock::new(0_u32, &parker));
    let holder = lock.lock();
    let got_it = Arc::new(AtomicBool::new(false));
    let waiter = {
        let lock = Arc::clone(&lock);
        let got_it = Arc::clone(&got_it);
        thread::spawn(move || {
            let mut guard = lock.lock();
            got_it.store(true, Ordering::SeqCst);
            *guard += 1;
        })
    };
    // Generous: the waiter has to be scheduled, fail its exchange and sleep.
    let deadline = Instant::now() + Duration::from_secs(5);
    while parker.parks.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(
        parker.parks.load(Ordering::Relaxed) > 0,
        "a waiter for a held sleep lock must park, not spin"
    );
    assert!(
        !got_it.load(Ordering::SeqCst),
        "and must not have taken the lock while it was held"
    );
    drop(holder);
    waiter.join().expect("the waiter panicked");
    assert!(
        got_it.load(Ordering::SeqCst),
        "the release must wake the waiter"
    );
    assert!(
        parker.unparks.load(Ordering::Relaxed) > 0,
        "a release with a waiter announced must wake the parking"
    );
    assert_eq!(*lock.lock(), 1);
}

#[test]
fn an_uncontended_release_never_touches_the_parking() {
    let parker = CondvarParker::default();
    let lock = SleepLock::new(0_u32, &parker);
    for _ in 0..100 {
        *lock.lock() += 1;
    }
    assert_eq!(*lock.lock(), 100);
    assert_eq!(parker.parks.load(Ordering::Relaxed), 0, "nobody slept");
    assert_eq!(
        parker.unparks.load(Ordering::Relaxed),
        0,
        "a release that finds no waiter announced skips the parking"
    );
}

#[test]
fn sleeping_waiters_all_get_their_turn() {
    let _turn = contend();
    const THREADS: usize = 6;
    const PER_THREAD: usize = 200;
    let parker = CondvarParker::default();
    let lock = Arc::new(SleepLock::new(0_usize, &parker));
    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let lock = Arc::clone(&lock);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let _ = start.wait();
                for _ in 0..PER_THREAD {
                    let mut guard = lock.lock();
                    *guard += 1;
                    // Hold it long enough that the others have to sleep.
                    thread::yield_now();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("a counting thread panicked");
    }
    assert_eq!(*lock.lock(), THREADS * PER_THREAD);
}

/// A parker with no memory, as a kernel's is when the heap has run out.
#[derive(Debug)]
struct EmptyParker;

impl Parker for EmptyParker {
    fn new_parking(&self) -> Option<Box<dyn Parking>> {
        None
    }
}

#[test]
fn a_lock_whose_parking_could_not_be_made_still_excludes() {
    let lock = Arc::new(SleepLock::new(0_usize, &EmptyParker));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                for _ in 0..1000 {
                    *lock.lock() += 1;
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(*lock.lock(), 4000);
}

/// What the hooks below saw: the count, and the line of the last disable.
static HOOKED_DEPTH: AtomicUsize = AtomicUsize::new(0);
static HOOKED_LINE: AtomicUsize = AtomicUsize::new(0);

static RECORDING_HOOKS: PreemptHooks = PreemptHooks {
    disable: |site| {
        let _ = HOOKED_DEPTH.fetch_add(1, Ordering::SeqCst);
        HOOKED_LINE.store(site.line() as usize, Ordering::SeqCst);
    },
    enable: || {
        let _ = HOOKED_DEPTH.fetch_sub(1, Ordering::SeqCst);
    },
};

/// The one test that installs hooks: they are the process's, once.
#[test]
fn a_hooked_lock_raises_the_installed_count_with_the_line_that_took_it() {
    static LOCK: PreemptSpinLock<u32, HookedPreempt> = PreemptSpinLock::new(0);
    // SAFETY: the hooks only count, and no other test takes a `HookedPreempt`
    // lock, so none is held across the install.
    let first = unsafe { install_preempt_hooks(&RECORDING_HOOKS) };
    // SAFETY: as above; this one is refused.
    let second = unsafe { install_preempt_hooks(&RECORDING_HOOKS) };
    assert!(first, "the first install takes");
    assert!(!second, "a second is refused");
    {
        let line = line!() + 1;
        let mut guard = LOCK.lock();
        *guard += 1;
        assert_eq!(HOOKED_DEPTH.load(Ordering::SeqCst), 1, "raised while held");
        assert_eq!(
            HOOKED_LINE.load(Ordering::SeqCst),
            line as usize,
            "the hook is told the line that took the lock"
        );
    }
    assert_eq!(HOOKED_DEPTH.load(Ordering::SeqCst), 0, "lowered on release");
    assert!(LOCK.try_lock().is_some());
    assert_eq!(
        HOOKED_DEPTH.load(Ordering::SeqCst),
        0,
        "and after a try_lock's guard"
    );
}
