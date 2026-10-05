//! The models (`docs/OPAQUE-KERNEL.md` §9.8): 2c's case 9, the pending-work
//! word's clear against its post, 2e's condition 5, a channel waiter
//! listed and not yet blocked against its waker, for a message, a close and
//! an end, and 2f's regroup word, a move against a way out. Each `*_control`
//! drops the ordering its argument needs and is expected to fail: it passes
//! only when `loom` finds the interleaving.
//!
//! A model checks the protocol restated here, not the kernel's code: that each
//! model matches the kernel sites it names is a matter of review, and each
//! names them under *Kernel sites*.
//!
//! Every model is small enough to run without a bound, but the bound is set
//! (`PREEMPTIONS`) so that the run's time stays a few seconds as the models
//! grow: three preemptions find every failure the controls stand for.

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering, fence};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// The preemption bound every model runs under.
const PREEMPTIONS: usize = 3;

fn model(body: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(PREEMPTIONS);
    builder.check(body);
}

// ---------------------------------------------------------------------------
// 2c, case 9: the clear's order (`sched::work`)
// ---------------------------------------------------------------------------

/// `sched::work`'s `SIGNAL`, say.
const BIT: u32 = 1 << 2;

/// A poster writes the state (C) and then posts the bit (D, a `Release`
/// read-modify-write: `work::post`); the task clears the bit (A, an
/// `Acquire` read-modify-write: `work::look`) and then reads the state (B:
/// the personality's `needs_attention`). With `clear_first` the model is the
/// kernel's order; without it, the control, the task reads before it
/// clears. Either the task's read sees the state, or the bit is still set
/// for its next look.
///
/// Kernel sites: the poster's write then `sched::work::post` (or `notify`)
/// in `syscall::process` and `object::process::Process::end_record`; the
/// task's `sched::work::look`, from `trap::attention_due`, then
/// `syscall::deliver::needs_attention`.
fn case_9(clear_first: bool) {
    model(move || {
        let word = Arc::new(AtomicU32::new(0));
        let state = Arc::new(AtomicBool::new(false));
        let poster = {
            let (word, state) = (Arc::clone(&word), Arc::clone(&state));
            thread::spawn(move || {
                state.store(true, Ordering::Relaxed);
                let _ = word.fetch_or(BIT, Ordering::Release);
            })
        };
        let seen = if clear_first {
            let _ = word.fetch_and(!BIT, Ordering::Acquire);
            state.load(Ordering::Relaxed)
        } else {
            let seen = state.load(Ordering::Relaxed);
            let _ = word.fetch_and(!BIT, Ordering::Acquire);
            seen
        };
        poster.join().unwrap();
        assert!(
            seen || word.load(Ordering::Relaxed) & BIT != 0,
            "a posted state was neither read nor left announced"
        );
    });
}

#[test]
fn case_9_the_clear_before_the_read() {
    case_9(true);
}

#[test]
#[should_panic(expected = "neither read nor left announced")]
fn case_9_control_the_clear_after_the_read() {
    case_9(false);
}

// ---------------------------------------------------------------------------
// 2e, condition 5: listed but not yet blocked (`object::channel`)
// ---------------------------------------------------------------------------

/// A task's state, as `sched::task` has it.
const RUNNABLE: u8 = 0;
const BLOCKED: u8 = 1;

/// The waiter's side, shared with its wakers.
struct Wait {
    /// The wait queue, `WaitQueue::waiters`: whether the waiter is listed.
    listed: Mutex<bool>,
    /// The waiter's task state.
    state: AtomicU8,
    /// What the waiter's look reads: the end's state word for a message or a
    /// close (`Half::state`), the task's word for an end (`END`).
    word: AtomicU8,
    /// Set by a wake that found the waiter listed and blocked.
    woken: AtomicBool,
}

/// The waiter, `WaitQueue::wait_sliced` on its trusting path: list itself
/// under the queue's lock, store `BLOCKED`, fence, and read the word.
/// Answers whether the look found the word, so that it does not sleep.
///
/// Kernel sites: `sched::wait::WaitQueue::wait_sliced` (the listing, the
/// state, the fence on the trusting path) and its `ready`,
/// `syscall::native::receive_words`' `Endpoint::readable_or_closed` and
/// `sched::work::own_end`.
fn waiter(wait: &Wait) -> bool {
    *wait.listed.lock().unwrap() = true;
    wait.state.store(BLOCKED, Ordering::Relaxed);
    fence(Ordering::SeqCst);
    wait.word.load(Ordering::Acquire) != 0
}

/// How a waker is ordered: the kernel's, or one of the controls.
#[derive(Clone, Copy)]
enum Order {
    /// The word, a `SeqCst` fence, then the wake.
    Kernel,
    /// The word then the wake, with no fence: condition 5's control.
    Unfenced,
    /// The wake, then the word: the mark after the wake, the design's close
    /// control, moved where it can fire.
    MarkAfterWake,
}

/// A waker: store the word (`Half::note` under the inbox lock, or
/// `work::post`), fence, then the wake: drain the queue under its lock and
/// read the drained task's state, waking it only if it is blocked
/// (`sched::wake_at_home`). `order` takes the fence away, or puts the word
/// after the wake, for the controls.
///
/// Kernel sites: `object::channel::Endpoint::write`, `write_small`,
/// `unread` and its `Drop` (the close), each `Half::note` or
/// `note_peer_closed` then `fence(SeqCst)` then `WaitQueue::wake_all` or
/// `wake_all_with`; and `sched::work::wake_posted` after
/// `object::process::Process::post_to_tasks`' post, for an end.
fn waker(wait: &Wait, bit: u8, order: Order) {
    let wake = || {
        let drained = core::mem::take(&mut *wait.listed.lock().unwrap());
        if drained && wait.state.load(Ordering::Relaxed) == BLOCKED {
            wait.state.store(RUNNABLE, Ordering::Relaxed);
            wait.woken.store(true, Ordering::Relaxed);
        }
    };
    match order {
        Order::Kernel => {
            let _ = wait.word.fetch_or(bit, Ordering::Release);
            fence(Ordering::SeqCst);
            wake();
        }
        Order::Unfenced => {
            let _ = wait.word.fetch_or(bit, Ordering::Release);
            wake();
        }
        Order::MarkAfterWake => {
            wake();
            let _ = wait.word.fetch_or(bit, Ordering::Release);
        }
    }
}

/// The waiter against the wakers named by `bits`, each fenced or not.
/// After all of them, a waiter whose look missed every word must have been
/// woken: otherwise it sleeps for good, which is the lost wake.
fn condition_5(wakers: &'static [(u8, Order)]) {
    model(move || {
        let wait = Arc::new(Wait {
            listed: Mutex::new(false),
            state: AtomicU8::new(RUNNABLE),
            word: AtomicU8::new(0),
            woken: AtomicBool::new(false),
        });
        let threads: Vec<_> = wakers
            .iter()
            .map(|&(bit, order)| {
                let wait = Arc::clone(&wait);
                thread::spawn(move || waker(&wait, bit, order))
            })
            .collect();
        let found = waiter(&wait);
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(
            found || wait.woken.load(Ordering::Relaxed),
            "a waiter listed and not yet blocked missed its word and was not woken"
        );
    });
}

/// `Half::state`'s bits, and `END`'s stand-in.
const NONEMPTY: u8 = 1 << 0;
const PEER_CLOSED: u8 = 1 << 1;
const END: u8 = 1 << 2;

/// A writer (`Endpoint::write`, `write_small`, `unread`), with its fence.
#[test]
fn condition_5_a_message() {
    condition_5(&[(NONEMPTY, Order::Kernel)]);
}

/// A closer (`Endpoint::drop`), with its fence.
#[test]
fn condition_5_a_close() {
    condition_5(&[(PEER_CLOSED, Order::Kernel)]);
}

/// An end or a replacing thread (`sched::work::wake_posted`'s fence after
/// `post_to_tasks`' post).
#[test]
fn condition_5_an_end() {
    condition_5(&[(END, Order::Kernel)]);
}

/// A writer and a closer at once, each fenced.
#[test]
fn condition_5_a_message_and_a_close() {
    condition_5(&[(NONEMPTY, Order::Kernel), (PEER_CLOSED, Order::Kernel)]);
}

/// The control: the writer's fence dropped, and `loom` must find the wake
/// lost.
#[test]
#[should_panic(expected = "missed its word and was not woken")]
fn condition_5_control_the_writer_without_its_fence() {
    condition_5(&[(NONEMPTY, Order::Unfenced)]);
}

/// The control for the mark's place: a closer that marks `PEER_CLOSED`
/// after its wake rather than before, and `loom` must find the wake lost.
#[test]
#[should_panic(expected = "missed its word and was not woken")]
fn condition_5_control_the_mark_after_the_wake() {
    condition_5(&[(PEER_CLOSED, Order::MarkAfterWake)]);
}

// ---------------------------------------------------------------------------
// 2f, condition 6: the regroup word (`sched::regroup_current`)
// ---------------------------------------------------------------------------

/// A job, as a quota slot's index.
const OLD_JOB: u32 = 1;
const NEW_JOB: u32 = 2;

/// A move stores the process's new job, then increments `MOVES` (`SeqCst`:
/// `sched::note_moved`); a way out loads `MOVES`, compares it with what its
/// processor last saw (`RUNNING_SEEN`, a load and a store since 2f), and
/// only when they differ fences (`SeqCst`) and reads the job. With
/// `ordered` the model is the kernel's; without it, the control, the load of
/// `MOVES` is `Relaxed` and the fence is gone. A way out that sees the move
/// counted must see the job it moved to.
///
/// Kernel sites: `object::process` (the job's store, then
/// `sched::note_moved`); `sched::regroup_current` (the load of `MOVES`, the
/// compare with `RUNNING_SEEN`, the fence, then `quota_slot`).
fn regroup(ordered: bool) {
    model(move || {
        let job = Arc::new(AtomicU32::new(OLD_JOB));
        let moves = Arc::new(AtomicU32::new(0));
        let mover = {
            let (job, moves) = (Arc::clone(&job), Arc::clone(&moves));
            thread::spawn(move || {
                job.store(NEW_JOB, Ordering::Relaxed);
                let _ = moves.fetch_add(1, Ordering::SeqCst);
            })
        };
        let seen = 0;
        let now = moves.load(if ordered {
            Ordering::Acquire
        } else {
            Ordering::Relaxed
        });
        if now != seen {
            if ordered {
                fence(Ordering::SeqCst);
            }
            assert_eq!(
                job.load(Ordering::Relaxed),
                NEW_JOB,
                "a way out saw the move counted and read the old job"
            );
        }
        mover.join().unwrap();
    });
}

#[test]
fn regroup_a_counted_move_reads_its_job() {
    regroup(true);
}

#[test]
#[should_panic(expected = "saw the move counted and read the old job")]
fn regroup_control_without_the_order() {
    regroup(false);
}
