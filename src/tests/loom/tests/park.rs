//! The park protocol of step 4's fast path for `channel_write_read`
//! (`docs/OPAQUE-KERNEL.md` §9.7, parts 2 and 3; the consultant's condition
//! 9). Nothing of the fast path is built yet: these model the protocol as
//! the design orders it, so that the code that comes is written against
//! models that already pass, and each control shows the one ordering its
//! case rests on.
//!
//! A model checks the protocol restated here, not the kernel's code; that
//! the code matches it is a matter of review, and each model names the
//! design's steps it restates under *Design sites*, to become kernel sites
//! when the code lands.
//!
//! The allowed results are the general path's (part 6's criterion:
//! refinement, plus liveness). Every model asserts both:
//! - *Nothing lost*: every message sent is in the reader's reply cell or its
//!   inbox, never in neither.
//! - *Nothing stranded*: no reader stays blocked while a message, a close or
//!   an end that should have woken it is there to see.
//!
//! The preemption bound is the models' file's, 3, and each model is a few
//! hundred iterations at most.

use loom::sync::atomic::{AtomicBool, AtomicU8, Ordering, fence};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// The preemption bound every model runs under.
const PREEMPTIONS: usize = 3;

fn model(body: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(PREEMPTIONS);
    builder.check(body);
}

/// A reader's state, as `sched::task` has it: an atomic, which the park
/// stores under its half's lock and a wake reads under the run queue's.
const RUNNING: u8 = 0;
const BLOCKED: u8 = 1;

/// The reader's half of the channel and its task, under the half's inbox
/// lock (`Half::inbox`, with the park's record beside it), and the run
/// queue's lock for its state.
struct Reader {
    /// Under the half's lock: messages queued (the general writer's), the
    /// park record, and whether the other end has closed.
    half: Mutex<Half>,
    /// The reader's home run queue's lock, which every wake and the commit
    /// take; the state itself is an atomic beside it.
    queue: Mutex<()>,
    /// The reader's task state (`Task::state`).
    state: AtomicU8,
    /// The reply cell, filled only by a commit.
    reply: Mutex<Option<u8>>,
    /// The reader's `END` bit (`sched::work`).
    end: AtomicBool,
}

/// What a channel half holds, for the models.
#[derive(Default)]
struct Half {
    /// Messages waiting, by tag.
    inbox: Vec<u8>,
    /// Whether the reader is parked here.
    parked: bool,
    /// Whether the other end has closed.
    closed: bool,
}

impl Reader {
    fn new() -> Reader {
        Reader {
            half: Mutex::new(Half::default()),
            queue: Mutex::new(()),
            state: AtomicU8::new(RUNNING),
            reply: Mutex::new(None),
            end: AtomicBool::new(false),
        }
    }

    /// The reader parks: with its inbox empty, nothing parked and the other
    /// end open, it sets the record and its blocked state in one hold of its
    /// half's lock (part 2's *park*, state P1). Then the receive half's last
    /// look: a `SeqCst` fence and `END` (`wait_trusting`'s order, condition
    /// 1). Answers whether it parked and is still waiting after its look.
    ///
    /// Design sites: part 2, *The park* and *The receive half alone
    /// commits*; part 3, state P1.
    fn park(&self, fenced: bool) -> bool {
        {
            let mut half = self.half.lock().unwrap();
            if !half.inbox.is_empty() || half.closed || half.parked {
                return false;
            }
            half.parked = true;
            self.state.store(BLOCKED, Ordering::Relaxed);
        }
        if fenced {
            fence(Ordering::SeqCst);
        }
        if self.end.load(Ordering::Relaxed) {
            // It must leave: it un-parks, under its half's lock, and runs.
            let mut half = self.half.lock().unwrap();
            half.parked = false;
            self.state.store(RUNNING, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Wake the reader if it is blocked, under its run queue's lock: a
    /// general wake (`sched::wake_at_home`).
    fn wake(&self) {
        let _queue = self.queue.lock().unwrap();
        if self.state.load(Ordering::Relaxed) == BLOCKED {
            self.state.store(RUNNING, Ordering::Relaxed);
        }
    }

    /// The result checks: nothing lost of `sent` messages, and nothing
    /// stranded.
    fn check(&self, sent: usize) {
        let half = self.half.lock().unwrap();
        let replied = usize::from(self.reply.lock().unwrap().is_some());
        assert_eq!(half.inbox.len() + replied, sent, "a message was lost");
        let blocked = self.state.load(Ordering::Relaxed) == BLOCKED;
        let cause = !half.inbox.is_empty() || half.closed || self.end.load(Ordering::Relaxed);
        assert!(
            !(blocked && cause && replied == 0),
            "a reader stayed blocked with a message, a close or an end to see"
        );
        assert!(
            !(blocked && replied == 1),
            "a reader handed a reply was left blocked"
        );
    }
}

/// A general writer (`Endpoint::write_small` with the park's one new test):
/// it queues under the half's lock and, unless `leaves_record` (a control),
/// takes the record there too, then wakes the reader once the lock is let
/// go. With `leaves_record`, every general write of the model leaves it,
/// the fast send's fallback included.
///
/// Design sites: part 2, *The general path gains one test*.
fn general_write(reader: &Reader, tag: u8, leaves_record: bool) {
    let found = {
        let mut half = reader.half.lock().unwrap();
        half.inbox.push(tag);
        let found = half.parked && !leaves_record;
        if found {
            half.parked = false;
        }
        found
    };
    if found {
        reader.wake();
    }
}

/// A fast commit (the send half): under the reader's half's lock taken by
/// `try_lock`, and with the reader parked, its reply is filled, its record
/// cleared and it is set runnable, all under its run queue's lock. A lock
/// held, or no record, declines to the general writer's path.
///
/// Design sites: part 2, T9 and *The commit*, steps 1 and 2; part 3, P2 to
/// H.
fn fast_send(reader: &Reader, tag: u8, leaves_record: bool) {
    let committed = match reader.half.try_lock() {
        Ok(mut half) => {
            if half.parked && half.inbox.is_empty() && !half.closed {
                let _queue = reader.queue.lock().unwrap();
                *reader.reply.lock().unwrap() = Some(tag);
                half.parked = false;
                reader.state.store(RUNNING, Ordering::Relaxed);
                true
            } else {
                false
            }
        }
        Err(_) => false,
    };
    if !committed {
        general_write(reader, tag, leaves_record);
    }
}

/// A fast commit and a general writer race a parking reader: every message
/// arrives, one perhaps by the reply cell, and the reader is never left
/// waiting on a message it could see.
#[test]
fn a_commit_and_a_general_write_against_a_park() {
    a_commit_and_a_write(false);
}

/// The control: the general writer leaves the record, and `loom` must find
/// the reader stranded beside its message.
#[test]
#[should_panic(expected = "stayed blocked with a message")]
fn control_a_writer_that_leaves_the_record() {
    a_commit_and_a_write(true);
}

fn a_commit_and_a_write(leaves_record: bool) {
    model(move || {
        let reader = Arc::new(Reader::new());
        let fast = {
            let reader = Arc::clone(&reader);
            thread::spawn(move || fast_send(&reader, 1, leaves_record))
        };
        let general = {
            let reader = Arc::clone(&reader);
            thread::spawn(move || general_write(&reader, 2, leaves_record))
        };
        let _ = reader.park(true);
        fast.join().unwrap();
        general.join().unwrap();
        reader.check(2);
    });
}

/// A close (`Endpoint::drop`): it marks the other end closed under the
/// reader's half's lock and takes the record there, then wakes.
///
/// Design sites: part 2, *The general path gains one test* (the close);
/// part 3, *A close*.
fn close(reader: &Reader) {
    let found = {
        let mut half = reader.half.lock().unwrap();
        half.closed = true;
        let found = half.parked;
        half.parked = false;
        found
    };
    if found {
        reader.wake();
    }
}

/// A close races a parking reader and a commit: the reader sees the message,
/// the close, or both, and is never left waiting on either.
#[test]
fn a_close_and_a_commit_against_a_park() {
    model(|| {
        let reader = Arc::new(Reader::new());
        let fast = {
            let reader = Arc::clone(&reader);
            thread::spawn(move || fast_send(&reader, 1, false))
        };
        let closer = {
            let reader = Arc::clone(&reader);
            thread::spawn(move || close(&reader))
        };
        let _ = reader.park(true);
        fast.join().unwrap();
        closer.join().unwrap();
        reader.check(1);
    });
}

/// A kill of the reader's process: `END` posted, then the wake, with
/// `wake_posted`'s fence between (`sched::work`).
///
/// Design sites: part 3, *A kill or an execve during the receive half
/// alone*.
fn kill(reader: &Reader) {
    reader.end.store(true, Ordering::Relaxed);
    fence(Ordering::SeqCst);
    reader.wake();
}

/// A kill races the receive half's park: the reader leaves, or is found
/// blocked and woken.
#[test]
fn a_kill_against_a_park() {
    a_kill_against_a_park_fenced(true);
}

/// The control: the park's fence dropped, and `loom` must find the reader
/// blocked past its end.
#[test]
#[should_panic(expected = "stayed blocked with a message, a close or an end")]
fn control_a_park_without_its_fence() {
    a_kill_against_a_park_fenced(false);
}

fn a_kill_against_a_park_fenced(fenced: bool) {
    model(move || {
        let reader = Arc::new(Reader::new());
        let killer = {
            let reader = Arc::clone(&reader);
            thread::spawn(move || kill(&reader))
        };
        let _ = reader.park(fenced);
        killer.join().unwrap();
        reader.check(0);
    });
}

/// T13, the send half's last look: the caller, about to commit and block
/// parked on its own half, reads its own `END` under its run queue's lock,
/// and blocks in the same hold. A kill posts the bit and then wakes under
/// that lock. With `outside_the_lock` (the control), the look is made
/// before the lock is taken.
///
/// Design sites: part 2, T13 and *The commit*, step 2; part 3, *A kill or
/// an execve during a full trip*; §9.8 2c's wake row.
fn t13(caller: &Reader, outside_the_lock: bool) {
    let early = outside_the_lock && caller.end.load(Ordering::Relaxed);
    let _queue = caller.queue.lock().unwrap();
    let ended = if outside_the_lock {
        early
    } else {
        caller.end.load(Ordering::Relaxed)
    };
    if !ended {
        // Committed: the caller blocks, parked on its own half.
        caller.half.lock().unwrap().parked = true;
        caller.state.store(BLOCKED, Ordering::Relaxed);
    }
}

/// A kill races T13: the caller declines, or blocks and is woken.
#[test]
fn a_kill_against_the_last_look() {
    a_kill_against_t13(false);
}

/// The control: T13 read outside the run queue's lock, and `loom` must find
/// the caller blocked past its end.
#[test]
#[should_panic(expected = "stayed blocked with a message, a close or an end")]
fn control_the_last_look_outside_the_lock() {
    a_kill_against_t13(true);
}

fn a_kill_against_t13(outside_the_lock: bool) {
    model(move || {
        let caller = Arc::new(Reader::new());
        let killer = {
            let caller = Arc::clone(&caller);
            // The poster's wake takes the lock before it reads the state
            // (the wake row), and needs no fence for T13: the lock orders it.
            thread::spawn(move || {
                caller.end.store(true, Ordering::Relaxed);
                caller.wake();
            })
        };
        t13(&caller, outside_the_lock);
        killer.join().unwrap();
        caller.check(0);
    });
}
