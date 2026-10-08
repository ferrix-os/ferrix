//! Stage 8's check that every in-kernel waker of a pipe's waits ends them.
//!
//! A pipe's read and write waits trust their queues (po10-pipe P3,
//! `fs::pipe`'s `End::wait_to_read`): they file no recheck, so a waker that
//! forgot its wake would leave the waiter asleep for good, not five
//! milliseconds late. This blocks a kernel task in a pipe's read, or in a
//! write to a full pipe, and ends the wait by each waker the pipe has --
//! bytes written, room made by a read, the last writer and the last reader
//! closing, bytes put back, and a splice from another pipe -- and requires
//! each within [`PATIENCE_NANOS`], with the queue's count of waits a wake
//! ended one higher, so that no recheck could have ended it (the
//! consultant's W5). A wait that never ends fails by its waker's name.
//!
//! The wakers that end a wait through the caller's signal state -- a signal,
//! a kill, another thread's `execve`, a stop, a freeze -- need a program's
//! task to wait; they are argued beside `End::wait_to_read` and run by the
//! user-mode suites.

use alloc::sync::Arc;
use alloc::vec;

use ferrix_vfs::{Errno, OpenFile};

use crate::fs::pipe;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;

/// How long a waiter has to start waiting, and to come back once woken.
const PATIENCE_NANOS: u64 = 2_000_000_000;
/// How long a waiter must still be waiting before its waker runs.
const STILL_WAITING_NANOS: u64 = 10_000_000;
/// How often the check looks for the waiter on its queue.
const LISTED_LOOK_NANOS: u64 = 1_000_000;

/// The waiter's file.
static WAITER_FILE: SpinLock<Option<Arc<OpenFile>>> = SpinLock::new(None);
/// The waiter's answer: the count it read or wrote, or its refusal.
static WAITER_ANSWER: SpinLock<Option<Result<usize, Errno>>> = SpinLock::new(None);
/// Woken when the waiter answers.
static WAITER_DONE: WaitQueue = WaitQueue::new();

/// What the waiter does.
const READS: usize = 0;
/// See [`READS`].
const WRITES: usize = 1;

/// The waiting task: one blocking read of eight bytes from, or write of one
/// byte to, [`WAITER_FILE`].
fn waiter(what: usize) {
    let file = WAITER_FILE.lock().clone();
    let answer = match file {
        Some(file) if what == READS => {
            let mut buf = [0_u8; 8];
            pipe::read(&file, &mut buf, false)
        }
        Some(file) => pipe::write(&file, b"w", false),
        None => Err(Errno::ESRCH),
    };
    *WAITER_ANSWER.lock() = Some(answer);
    WAITER_DONE.wake_all();
}

/// The two ends of a new byte-stream pipe, read end first.
fn new_pipe() -> Result<(Arc<OpenFile>, Arc<OpenFile>), &'static str> {
    pipe::new_pipe(false, (0, 0)).map_err(|_| "a pipe for the wake check was refused")
}

/// Block a task on `waited` (`what` it does), let it be listed and still
/// waiting, run `wake`, and require the waiter back within the patience with
/// `want`, ended by a wake. `name` says which waker failed.
fn ended_by(
    name: &'static str,
    waited: &Arc<OpenFile>,
    what: usize,
    wake: impl FnOnce() -> Result<(), &'static str>,
    want: Result<usize, Errno>,
) -> Result<(), &'static str> {
    let listed = |file: &OpenFile| {
        pipe::waiting_on(file).map_or(
            0,
            |(readers, writers, _)| {
                if what == READS { readers } else { writers }
            },
        )
    };
    let ended = |file: &OpenFile| pipe::waiting_on(file).map_or(0, |(_, _, ended)| ended);
    *WAITER_ANSWER.lock() = None;
    *WAITER_FILE.lock() = Some(Arc::clone(waited));
    let ended_before = ended(waited);
    let task = crate::sched::spawn("pipe-waiter", waiter, what, ferrix_sched::NICE_0_WEIGHT)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while listed(waited) == 0
        && WAITER_ANSWER.lock().is_none()
        && crate::timer::now_nanos() < deadline
    {
        crate::sched::sleep_for(LISTED_LOOK_NANOS);
    }
    crate::sched::sleep_for(STILL_WAITING_NANOS);
    if WAITER_ANSWER.lock().is_some() {
        crate::console::println!("  pipewake {name}: the waiter did not wait");
        return Err("a pipe waiter of the wake check came back before its waker ran");
    }
    // The waiter holds its own reference while it waits; the check lets go
    // of the slot's, so that a waker that closes an end closes the last one.
    *WAITER_FILE.lock() = None;
    wake()?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let _ = WAITER_DONE.wait_until_deadline(|| WAITER_ANSWER.lock().is_some(), deadline);
    let Some(answer) = WAITER_ANSWER.lock().take() else {
        crate::console::println!("  pipewake {name}: the waiter was never woken");
        return Err("a pipe wait was never ended by its waker: a missing wake");
    };
    crate::sched::wait_until_gone(&task, crate::sched::REAPER_PATIENCE_NANOS)?;
    if answer != want {
        crate::console::println!("  pipewake {name}: the waiter answered {answer:?}, not {want:?}");
        return Err("a woken pipe waiter did not answer as its waker says");
    }
    if ended(waited) == ended_before {
        crate::console::println!("  pipewake {name}: the wait was not ended by a wake");
        return Err("a pipe wait ended without a wake ending it");
    }
    Ok(())
}

/// Every in-kernel waker of a pipe's waits ends them. Answers how many.
///
/// # Errors
///
/// A waker whose waiter was never woken, came back early or answered other
/// than its waker says.
pub(crate) fn run() -> Result<u32, &'static str> {
    let mut wakers = 0;

    // Bytes written.
    let (reader, writer) = new_pipe()?;
    ended_by(
        "bytes written",
        &reader,
        READS,
        || {
            pipe::write(&writer, b"12345678", false)
                .map(|_| ())
                .map_err(|_| "the wake check's write was refused")
        },
        Ok(8),
    )?;
    wakers += 1;

    // The last writer closing: end of file.
    let (reader, writer) = new_pipe()?;
    ended_by(
        "the last writer closing",
        &reader,
        READS,
        move || {
            drop(writer);
            Ok(())
        },
        Ok(0),
    )?;
    wakers += 1;

    // Bytes put back, as a read into a bad buffer puts them.
    let (reader, writer) = new_pipe()?;
    let back = Arc::clone(&reader);
    ended_by(
        "bytes put back",
        &reader,
        READS,
        move || {
            back.io().unread_stream(b"xy");
            Ok(())
        },
        Ok(2),
    )?;
    drop(writer);
    wakers += 1;

    // A splice from another pipe.
    let (reader, writer) = new_pipe()?;
    let (source, filler) = new_pipe()?;
    let _ =
        pipe::write(&filler, b"spliced!", false).map_err(|_| "the splice source was refused")?;
    let sink = Arc::clone(&writer);
    ended_by(
        "a splice into it",
        &reader,
        READS,
        move || {
            pipe::splice_pipes(&source, &sink, 8, false)
                .map(|_| ())
                .map_err(|_| "the wake check's splice was refused")
        },
        Ok(8),
    )?;
    drop((writer, filler));
    wakers += 1;

    // Room made by a read, for a writer waiting on a full pipe.
    let (reader, writer) = new_pipe()?;
    fill(&writer)?;
    let drain = Arc::clone(&reader);
    ended_by(
        "room made by a read",
        &writer,
        WRITES,
        move || {
            let mut buf = vec![0_u8; 4096];
            pipe::read(&drain, &mut buf, true)
                .map(|_| ())
                .map_err(|_| "the wake check's draining read was refused")
        },
        Ok(1),
    )?;
    drop(reader);
    wakers += 1;

    // The last reader closing, for a writer waiting on a full pipe: EPIPE.
    let (reader, writer) = new_pipe()?;
    fill(&writer)?;
    ended_by(
        "the last reader closing",
        &writer,
        WRITES,
        move || {
            drop(reader);
            Ok(())
        },
        Err(Errno::EPIPE),
    )?;
    wakers += 1;

    Ok(wakers)
}

/// Write into the pipe until it is full.
fn fill(writer: &OpenFile) -> Result<(), &'static str> {
    let chunk = [0x5A_u8; 4096];
    for _ in 0..1024 {
        match pipe::write(writer, &chunk, true) {
            Ok(_) => {}
            Err(Errno::EAGAIN) => return Ok(()),
            Err(_) => return Err("filling a pipe for the wake check was refused"),
        }
    }
    Err("a pipe for the wake check never filled")
}
