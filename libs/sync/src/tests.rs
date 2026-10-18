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
use std::format;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use std::vec;
use std::vec::Vec;

use super::{IrqControl, IrqSpinLock, Once, RwSpinLock, SpinLock, SpinLockedCell};

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

#[test]
fn eight_threads_agree_on_the_count() {
    const PER_THREAD: usize = 10_000;

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
                    for _ in 0..PER_THREAD {
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
            THREADS * PER_THREAD,
            "attempt {attempt}: an increment was lost, so two threads held the lock at once"
        );
    }
}

#[test]
fn the_ticket_lock_starves_nobody() {
    // A test-and-set lock lets whichever CPU happens to win keep winning, and a
    // thread can wait indefinitely. A ticket lock hands out in arrival order,
    // so acquisitions by different threads must interleave heavily rather than
    // run in long unbroken stretches.
    //
    // That only holds while every thread is spinning at once, which needs a
    // core each. Past that the test measures the OS scheduler instead of the
    // lock: a thread handed a timeslice finishes all of its acquisitions before
    // the others have taken a ticket, and a perfectly fair lock scores a handful
    // of handovers. A CI runner with two vCPUs produced exactly that -- seven in
    // sixteen hundred -- so the thread count is capped at the parallelism the
    // process actually has.
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
    let start = Arc::new(Barrier::new(threads));

    let workers: Vec<_> = (0..threads)
        .map(|id| {
            let order = Arc::clone(&order);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let _ = start.wait();
                for _ in 0..PER_THREAD {
                    order.lock().push(id);
                }
            })
        })
        .collect();

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
