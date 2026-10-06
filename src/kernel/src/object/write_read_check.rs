//! `channel_write_read` (0x1013): its answer in registers, the inbox's slot it
//! writes into, and the wait it trusts (`docs/OPAQUE-KERNEL.md` §9.4, items 3
//! and 4).
//!
//! Every call is made through the architecture's own system call entry
//! (`arch::drive_native_words`), by a thread of a check's process
//! (`syscall::check::spawn_in`). So what is read back is what a program's
//! registers hold after the call: the core's dispatch, the native call's
//! answer, and the entry's store of it, on each architecture.
//!
//! The registers (item 4):
//! 1. a message shorter than three words comes back as its bytes and then
//!    zeros, its size in the return register: one held in the inbox's slot,
//!    and one queued;
//! 2. a call that sends and finds its answer waiting sends its words, which
//!    the peer reads back as the bytes sent and then zeros;
//! 3. a call that fails -- a closed handle, a count past three words, a
//!    handle without WRITE, a message too big for registers, a closed peer --
//!    answers its status and leaves the second to fourth argument registers
//!    exactly as sent; nothing is sent, and the message too big stays queued.
//!
//! The slot (at the channel's own level): it is every reader's head. A
//! message held there is read before one queued after it, one queued is read
//! before a small one written after it, a read too small for it leaves it, a
//! message put back goes ahead of one the slot took since, and a small read
//! leaves a message too big for registers queued.
//!
//! The wait (item 3): a thread blocked in the call, listed on its end's
//! queue, is woken by each of what ends the wait -- a message, the peer's
//! close, and its process's kill -- within [`WOKEN_WITHIN_NANOS`]. The wait
//! files no recheck, so nothing but those wakes ends it: a wake that is
//! missing leaves the thread blocked past the bound, and the check says which.
//!
//! The Sync wake (at two processors or more): a reader blocked in the call
//! on one processor and woken by a writer on another is moved onto the
//! writer's processor, where its affinity allows, and answers there; one
//! pinned to its processor stays there and is still woken. The boot line
//! counts the moves, and none in [`SYNC_ROUNDS`] fails the check.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::{self, Errno};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;

use super::channel::{Endpoint, ReadError, WriteFailure};
use super::job::KILLED_STATUS;
use super::{Object, Transfer};
use crate::arch;
use crate::sched::Task;
use crate::sync::SpinLock;
use crate::syscall::check::spawn_in;
use crate::syscall::process::{self, Process};

/// How long a wake may take to end a blocked `channel_write_read`: the bound
/// past which a waiter still blocked fails the check. Generous against an
/// emulated processor on a loaded host, where a wake is milliseconds; a wake
/// that never comes is for ever.
const WOKEN_WITHIN_NANOS: u64 = 10_000_000_000;
/// How long the check waits for its own threads to get going.
const PATIENCE_NANOS: u64 = 120_000_000_000;
/// How often it looks.
const POLL_NANOS: u64 = 1_000_000;
/// A word of this processor's, as the call's registers carry it.
const WORD: usize = size_of::<usize>();
/// The words a call sends, or that a failing call must leave where they are:
/// values a 32-bit register holds whole, so they read back the same on every
/// architecture.
const SENT: [u64; 3] = [0x1111_1111, 0x2222_2222, 0x3333_3333];

/// What the check counted, for the boot line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Answers whose words past the message's bytes were all zero.
    pub(crate) zeroed: u32,
    /// Failing calls that left the second to fourth registers as sent.
    pub(crate) kept: u32,
    /// Messages read in the order written, through the slot and the queue.
    pub(crate) ordered: u32,
    /// Waits ended by the wake of what they waited for.
    pub(crate) woken: u32,
    /// The bound each wake was held to, in seconds.
    pub(crate) within_seconds: u64,
    /// Readers a Sync wake moved onto the writer's processor, of
    /// [`SYNC_ROUNDS`]; and whether the moves and the pinned reader were
    /// checked, which a machine with one processor cannot.
    pub(crate) moved: u32,
    /// See [`Report::moved`].
    pub(crate) sync_checked: bool,
}

/// What a call answered: the return register, then the second to fourth.
type Answer = (isize, [u64; 3]);

/// Run every case.
///
/// # Errors
///
/// The first thing that was not so.
///
/// Verifies: L.object.128, L.object.129, L.object.130, L.object.131
/// Verifies: L.object.132
/// Verifies: `L.object.141`, `L.sched.40`
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report {
        within_seconds: WOKEN_WITHIN_NANOS / 1_000_000_000,
        ..Report::default()
    };
    check_the_slot_is_every_readers_head(&mut report)?;
    check_the_registers(&mut report)?;
    for ending in [Ending::Message, Ending::Close, Ending::Kill] {
        check_a_wait_is_ended_by(ending)?;
        report.woken += 1;
    }
    if crate::smp::count() >= 2 {
        report.moved = check_a_sync_wake_moves_the_reader()?;
        check_a_sync_wake_keeps_a_pinned_reader()?;
        report.sync_checked = true;
    }
    Ok(report)
}

/// The call's words for `bytes`: the bytes as they lie in memory, then zeros,
/// one register's width each.
fn words_of(bytes: &[u8]) -> [u64; 3] {
    let mut padded = [0_u8; 3 * WORD];
    if let Some(start) = padded.get_mut(..bytes.len()) {
        start.copy_from_slice(bytes);
    }
    let mut words = [0_u64; 3];
    for (word, chunk) in words.iter_mut().zip(padded.chunks_exact(WORD)) {
        let mut raw = [0_u8; WORD];
        raw.copy_from_slice(chunk);
        *word = usize::from_ne_bytes(raw) as u64;
    }
    words
}

/// `value` as a register of this processor holds it.
const fn as_register(value: u64) -> u64 {
    value as usize as u64
}

/// Make the call on `handle` through the entry, sending `count` bytes of
/// `words`, from a thread of the process whose table holds it.
fn call(handle: Handle, count: usize, words: [u64; 3]) -> Answer {
    arch::drive_native_words(
        nr::CHANNEL_WRITE_READ,
        [
            u64::from(handle.0),
            count as u64,
            words[0],
            words[1],
            words[2],
            0,
        ],
    )
}

/// Write `bytes` on `end` as `channel_write` queues them: never into the slot.
fn queue(end: &Endpoint, bytes: &[u8]) -> Result<(), &'static str> {
    end.write(bytes.to_vec(), 0, || {
        Ok::<Vec<Transfer>, core::convert::Infallible>(Vec::new())
    })
    .map_err(|_| "the write_read check could not queue a message")
}

/// Write `bytes` on `end` as `channel_write_read` writes them.
fn write_small(end: &Endpoint, bytes: &[u8]) -> Result<(), &'static str> {
    end.write_small(bytes)
        .map_err(|_: WriteFailure<()>| "the write_read check could not write a small message")
}

/// Read the next message on `end`, which must be `bytes` with no handles.
fn expect_read(
    end: &Endpoint,
    bytes: &[u8],
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    match end.read(64, 4, false) {
        Ok(message) if message.bytes == bytes && message.handles.is_empty() => {
            report.ordered += 1;
            Ok(())
        }
        _ => Err(what),
    }
}

/// The slot is the head of the queue for every reader. See the module.
fn check_the_slot_is_every_readers_head(report: &mut Report) -> Result<(), &'static str> {
    let (reader, writer) =
        Endpoint::pair().map_err(|_| "no memory for the write_read check's channel")?;

    write_small(&writer, b"one")?;
    queue(&writer, b"two")?;
    expect_read(
        &reader,
        b"one",
        "slot: a message held in the slot was not read before one queued after it",
        report,
    )?;
    expect_read(
        &reader,
        b"two",
        "slot: a message queued behind the slot was lost",
        report,
    )?;

    queue(&writer, b"three")?;
    write_small(&writer, b"four")?;
    expect_read(
        &reader,
        b"three",
        "slot: a small message written behind a queued one overtook it",
        report,
    )?;
    expect_read(
        &reader,
        b"four",
        "slot: a small message behind a queued one was lost",
        report,
    )?;

    write_small(&writer, b"hello")?;
    if !matches!(
        reader.read(2, 0, false),
        Err(ReadError::TooSmall {
            bytes: 5,
            handles: 0
        })
    ) {
        return Err("slot: a read too small for the slot's message did not report its size");
    }
    expect_read(
        &reader,
        b"hello",
        "slot: a read too small for the slot's message did not leave it",
        report,
    )?;

    write_small(&writer, b"first")?;
    let taken = reader
        .read(64, 4, false)
        .map_err(|_| "slot: the slot's message could not be read")?;
    write_small(&writer, b"second")?;
    reader
        .unread(taken)
        .map_err(|_| "slot: a message taken from the slot could not be put back")?;
    expect_read(
        &reader,
        b"first",
        "slot: a message put back did not go ahead of one the slot took since",
        report,
    )?;
    expect_read(
        &reader,
        b"second",
        "slot: the message the slot took behind one put back was lost",
        report,
    )?;

    let big = [0x42_u8; 40];
    queue(&writer, &big)?;
    if !matches!(
        reader.read_small(),
        Err(ReadError::TooSmall {
            bytes: 40,
            handles: 0
        })
    ) {
        return Err("slot: a small read did not refuse a message too big for registers");
    }
    expect_read(
        &reader,
        &big,
        "slot: a small read refused a message and did not leave it queued",
        report,
    )?;
    Ok(())
}

/// What the registers thread is given: its end, the same end without WRITE,
/// a handle value it has closed, and the far end, which the check holds.
struct Setup {
    /// Its end, with every right a channel end carries.
    end: Handle,
    /// The same end, `READ` only.
    read_only: Handle,
    /// A value whose handle was closed.
    closed: Handle,
    /// Its end's object, to look at what stayed queued.
    own: Arc<Endpoint>,
    /// The other end.
    peer: Arc<Endpoint>,
}

/// Handed to the registers thread.
static SETUP: SpinLock<Option<Setup>> = SpinLock::new(None);
/// What the registers thread found.
static REGISTERS: SpinLock<Option<Result<Counted, &'static str>>> = SpinLock::new(None);

/// What the registers thread counted: answers zeroed past their message, and
/// refusals that kept the registers as sent.
#[derive(Debug, Default, Clone, Copy)]
struct Counted {
    /// See [`Report::zeroed`].
    zeroed: u32,
    /// See [`Report::kept`].
    kept: u32,
}

/// Item 4, on a thread of a check's process: answers in registers, and a
/// failing call's registers left as sent.
fn check_the_registers(report: &mut Report) -> Result<(), &'static str> {
    let process = process::new_for_check().map_err(|_| "no process for the write_read check")?;
    let (own, peer) =
        Endpoint::pair().map_err(|_| "no memory for the write_read check's channel")?;
    let insert = |rights: Rights| {
        process
            .with_handles(|table| table.insert(Object::Channel(Arc::clone(&own)), rights))
            .map_err(|_| "no room in the write_read check's table")
    };
    let end = insert(Rights::CHANNEL)?;
    let read_only = insert(Rights::READ)?;
    let closed = insert(Rights::CHANNEL)?;
    *REGISTERS.lock() = None;
    *SETUP.lock() = Some(Setup {
        end,
        read_only,
        closed,
        own: Arc::clone(&own),
        peer,
    });
    let task = spawn_in(
        &process,
        "write_read registers",
        registers_in_the_process,
        None,
    )?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if process.wait_for_exit(deadline).is_none() {
        return Err("the write_read check's registers thread never finished");
    }
    wait_dead(
        &task,
        deadline,
        "the write_read check's registers thread never ended",
    )?;
    let found = REGISTERS.lock().take();
    let counted = found.ok_or("the write_read check's registers thread found nothing")??;
    report.zeroed += counted.zeroed;
    report.kept += counted.kept;
    Ok(())
}

/// The registers thread: the cases, then the end of its process.
fn registers_in_the_process(_argument: usize) {
    let setup = SETUP.lock().take();
    let outcome = match setup {
        Some(setup) => registers(setup),
        None => Err("the write_read check's registers thread was given nothing"),
    };
    *REGISTERS.lock() = Some(outcome);
    process::exit_current(0)
}

/// An answer of `bytes` in registers: their size, their words, zeros after.
fn expect_answer(
    answer: Answer,
    bytes: &[u8],
    what: &'static str,
    counted: &mut Counted,
) -> Result<(), &'static str> {
    if answer != (bytes.len() as isize, words_of(bytes)) {
        return Err(what);
    }
    counted.zeroed += 1;
    Ok(())
}

/// A failing answer: `status` in the return register, and the second to
/// fourth registers as the call set them, `count` and the first two words.
fn expect_refusal(
    answer: Answer,
    count: usize,
    refused: Errno,
    what: &'static str,
    counted: &mut Counted,
) -> Result<(), &'static str> {
    let kept = [as_register(count as u64), SENT[0], SENT[1]];
    if answer != (errno::encode(Err(refused)), kept) {
        return Err(what);
    }
    counted.kept += 1;
    Ok(())
}

/// The register cases. See the module.
fn registers(setup: Setup) -> Result<Counted, &'static str> {
    let Setup {
        end,
        read_only,
        closed,
        own,
        peer,
    } = setup;
    let mut counted = Counted::default();

    // 1. Shorter than three words, from the slot and from the queue.
    write_small(&peer, b"hello")?;
    queue(&peer, b"queued!")?;
    expect_answer(
        call(end, nr::WRITE_READ_NOTHING, SENT),
        b"hello",
        "registers: a message shorter than three words held in the slot did not come back \
         with the rest zero",
        &mut counted,
    )?;
    expect_answer(
        call(end, nr::WRITE_READ_NOTHING, SENT),
        b"queued!",
        "registers: a queued message shorter than three words did not come back with the \
         rest zero",
        &mut counted,
    )?;

    // 2. Sent from registers, the answer already waiting.
    write_small(&peer, b"pong")?;
    expect_answer(
        call(end, 3, words_of(b"abc")),
        b"pong",
        "registers: a call that sent and found its answer waiting did not answer it",
        &mut counted,
    )?;
    match peer.read_small() {
        Ok(small) if small.as_bytes() == b"abc" && small.bytes.iter().skip(3).all(|b| *b == 0) => {}
        _ => return Err("registers: the words a call sent did not reach the peer as its bytes"),
    }

    // 3. Each refusal leaves the registers as they were sent, and sends nothing.
    let sent_nothing = |what| match peer.read_small() {
        Err(ReadError::Empty) => Ok(()),
        _ => Err(what),
    };
    let _ = arch::drive_system_call(
        crate::trap::Abi::Native,
        nr::HANDLE_CLOSE,
        [u64::from(closed.0), 0, 0, 0, 0, 0],
        0x40_1000,
    );
    expect_refusal(
        call(closed, 4, SENT),
        4,
        status::BAD_HANDLE,
        "registers: a call on a closed handle did not leave its registers as sent",
        &mut counted,
    )?;
    let too_many = nr::CHANNEL_WRITE_READ_BYTES + 1;
    expect_refusal(
        call(end, too_many, SENT),
        too_many,
        status::TOO_BIG,
        "registers: a count past three words did not leave its registers as sent",
        &mut counted,
    )?;
    sent_nothing("registers: a count past three words sent something")?;
    expect_refusal(
        call(read_only, 4, SENT),
        4,
        status::ACCESS_DENIED,
        "registers: a send on a handle without WRITE did not leave its registers as sent",
        &mut counted,
    )?;
    sent_nothing("registers: a send on a handle without WRITE sent something")?;
    let big = vec![0x42_u8; 40];
    queue(&peer, &big)?;
    expect_refusal(
        call(end, nr::WRITE_READ_NOTHING, SENT),
        nr::WRITE_READ_NOTHING,
        status::BUFFER_TOO_SMALL,
        "registers: a message too big for registers did not leave the registers as sent",
        &mut counted,
    )?;
    match own.read(64, 4, false) {
        Ok(message) if message.bytes == big => {}
        _ => return Err("registers: a message too big for registers did not stay queued"),
    }
    drop(peer);
    expect_refusal(
        call(end, 4, SENT),
        4,
        status::PEER_CLOSED,
        "registers: a send to a closed peer did not leave its registers as sent",
        &mut counted,
    )?;
    expect_refusal(
        call(end, nr::WRITE_READ_NOTHING, SENT),
        nr::WRITE_READ_NOTHING,
        status::PEER_CLOSED,
        "registers: a receive from a closed peer did not leave its registers as sent",
        &mut counted,
    )?;
    Ok(counted)
}

/// What ends a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// A message written on the peer.
    Message,
    /// The peer closed.
    Close,
    /// The waiter's process killed.
    Kill,
}

/// Handed to the waiting thread: the end it waits on.
static WAITER: SpinLock<Option<Handle>> = SpinLock::new(None);
/// What the waiting thread's call answered, when it came back to it.
static WAITED: SpinLock<Option<Answer>> = SpinLock::new(None);

/// The waiting thread: one receive-only call, then the end of its process.
fn wait_in_the_process(_argument: usize) {
    let handle = WAITER.lock().take();
    if let Some(handle) = handle {
        let answer = call(handle, nr::WRITE_READ_NOTHING, [0; 3]);
        *WAITED.lock() = Some(answer);
    }
    process::exit_current(0)
}

/// Wait until `task` has ended, by `deadline`.
fn wait_dead(task: &Task, deadline: u64, what: &'static str) -> Result<(), &'static str> {
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err(what);
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
    Ok(())
}

/// A thread blocked in `channel_write_read` is woken by `ending`, within the
/// bound, and answers what that ending means. See the module.
fn check_a_wait_is_ended_by(ending: Ending) -> Result<(), &'static str> {
    let process: Arc<Process> =
        process::new_for_check().map_err(|_| "no process for the write_read check's waiter")?;
    let (own, peer) =
        Endpoint::pair().map_err(|_| "no memory for the write_read check's channel")?;
    let handle = process
        .with_handles(|table| table.insert(Object::Channel(Arc::clone(&own)), Rights::CHANNEL))
        .map_err(|_| "no room in the write_read check's table")?;
    *WAITED.lock() = None;
    *WAITER.lock() = Some(handle);
    let woken_before = own.waiters().waits_ended_by_a_wake();
    let task = spawn_in(&process, "write_read waiter", wait_in_the_process, None)?;

    // Listed on the end's queue, or parked by the fast path's receive half:
    // blocked, or at its last look before it blocks. Either way only a wake
    // ends the wait from here.
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !own.reader_waiting() {
        if task.is_dead() || crate::timer::now_nanos() >= deadline {
            return Err("wait: the write_read check's waiter never waited");
        }
        crate::sched::sleep_for(POLL_NANOS);
    }

    let mut peer = Some(peer);
    match ending {
        Ending::Message => {
            if let Some(peer) = &peer {
                write_small(peer, b"wake")?;
            }
        }
        Ending::Close => drop(peer.take()),
        Ending::Kill => process::kill(&process, KILLED_STATUS),
    }
    let bound = crate::timer::now_nanos().saturating_add(WOKEN_WITHIN_NANOS);
    wait_dead(
        &task,
        bound,
        match ending {
            Ending::Message => {
                "wait case 1: a thread blocked in channel_write_read was not woken by a message \
                 within 10 s"
            }
            Ending::Close => {
                "wait case 2: a thread blocked in channel_write_read was not woken by its peer's \
                 close within 10 s"
            }
            Ending::Kill => {
                "wait case 3: a thread blocked in channel_write_read was not woken by its \
                 process's kill within 10 s"
            }
        },
    )?;
    let answer = WAITED.lock().take();
    let nothing = as_register(nr::WRITE_READ_NOTHING as u64);
    match ending {
        Ending::Message => {
            if answer != Some((4, words_of(b"wake"))) {
                return Err("wait case 1: a thread woken by a message did not answer it");
            }
            if own.waiters().waits_ended_by_a_wake() == woken_before {
                return Err("wait case 1: a wait for a message was not ended by its wake");
            }
        }
        Ending::Close => {
            if answer != Some((errno::encode(Err(status::PEER_CLOSED)), [nothing, 0, 0])) {
                return Err(
                    "wait case 2: a thread woken by its peer's close did not answer \
                            PEER_CLOSED",
                );
            }
        }
        // A kill ends the thread on its way back, where the entry's way out
        // takes it (x86-64), or once it reads the answer (the Arm entries):
        // either way the call answered EINTR, if anything.
        Ending::Kill => {
            if answer.is_some_and(|(value, _)| value != Errno::EINTR.as_return_value()) {
                return Err(
                    "wait case 3: a thread woken by its process's kill did not answer \
                            EINTR",
                );
            }
        }
    }
    drop(peer);
    Ok(())
}

/// Rounds of the Sync wake's move.
pub(crate) const SYNC_ROUNDS: usize = 8;

/// The Sync check's channel: the reader's end's handle in its process, and
/// the writer's end, which the writer task writes on.
static SYNC_READER: SpinLock<Option<Handle>> = SpinLock::new(None);
/// The end the writer task writes on.
static SYNC_PEER: SpinLock<Option<Arc<Endpoint>>> = SpinLock::new(None);
/// What the reader saw each round: its call's answer, and the processor it
/// ran on as the call returned, plus one; zero until it has answered.
static SYNC_SEEN: SpinLock<[(Option<Answer>, usize); SYNC_ROUNDS]> =
    SpinLock::new([(None, 0); SYNC_ROUNDS]);
/// How many rounds the reader makes.
static SYNC_READS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The reader: [`SYNC_READS`] receive-only calls, each answer and the
/// processor it came back on recorded, then the end of its process.
fn read_in_the_process(_argument: usize) {
    let handle = *SYNC_READER.lock();
    let rounds = SYNC_READS.load(core::sync::atomic::Ordering::Acquire);
    if let Some(handle) = handle {
        for round in 0..rounds {
            let answer = call(handle, nr::WRITE_READ_NOTHING, [0; 3]);
            let here = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical + 1);
            if let Some(seen) = SYNC_SEEN.lock().get_mut(round) {
                *seen = (Some(answer), here);
            }
        }
    }
    process::exit_current(0)
}

/// The writer: one small write on [`SYNC_PEER`], which wakes the reader with
/// `Wake::Sync`, from the processor it is pinned to; then it ends, as a
/// waker that blocks next would leave its processor.
fn write_from_another_processor(_argument: usize) {
    let peer = SYNC_PEER.lock().clone();
    if let Some(peer) = peer {
        let _ = peer.write_small(b"sync");
    }
}

/// A Sync writer on the processor `writer_for` names, given the one the
/// reader waits on, wakes the reader for round `round`: the writer's
/// processor, and the one the reader answered on, plus one.
fn wake_from(
    own: &Endpoint,
    reader: &Task,
    round: usize,
    writer_for: impl Fn(usize) -> usize,
) -> Result<(usize, usize), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !own.reader_waiting() {
        if reader.is_dead() || crate::timer::now_nanos() >= deadline {
            return Err("sync: the reader never waited");
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
    // Long enough for the reader to be switched out, asleep at home, the one
    // state a Sync wake moves a task from.
    crate::sched::sleep_for(2 * POLL_NANOS);
    let writer_cpu = writer_for(reader.cpu());
    let _writer = crate::sched::spawn_on(
        "write_read sync writer",
        write_from_another_processor,
        0,
        ferrix_sched::NICE_0_WEIGHT,
        writer_cpu,
        ferrix_sched::CpuSet::of(writer_cpu),
    )?;
    // Asleep while the writer writes: a move needs nothing else queued on
    // the writer's processor, and at two processors this check's own task,
    // woken to look, was what was queued there.
    crate::sched::sleep_for(10 * POLL_NANOS);
    let bound = crate::timer::now_nanos().saturating_add(WOKEN_WITHIN_NANOS);
    loop {
        let seen = SYNC_SEEN.lock().get(round).copied();
        if let Some((Some(answer), here)) = seen {
            if answer != (4, words_of(b"sync")) {
                return Err("sync: a reader woken by a Sync write did not answer it");
            }
            return Ok((writer_cpu, here));
        }
        if crate::timer::now_nanos() >= bound {
            return Err(
                "sync: a reader blocked in channel_write_read was not woken by a Sync \
                        write from another processor within 10 s",
            );
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
}

/// The Sync check's reader: its process, its end, and its thread.
struct Reader {
    /// Held for the check's length.
    _process: Arc<Process>,
    /// The end it reads.
    own: Arc<Endpoint>,
    /// Its thread.
    task: Arc<Task>,
}

/// A fresh channel with its reader end in a check's process, the reader
/// started there making `rounds` calls, placed as a program's thread or
/// pinned to `pinned`.
fn start_reader(rounds: usize, pinned: Option<usize>) -> Result<Reader, &'static str> {
    let process = process::new_for_check().map_err(|_| "no process for the sync check")?;
    let (own, peer) = Endpoint::pair().map_err(|_| "no memory for the sync check's channel")?;
    let handle = process
        .with_handles(|table| table.insert(Object::Channel(Arc::clone(&own)), Rights::CHANNEL))
        .map_err(|_| "no room in the sync check's table")?;
    *SYNC_SEEN.lock() = [(None, 0); SYNC_ROUNDS];
    *SYNC_READER.lock() = Some(handle);
    *SYNC_PEER.lock() = Some(peer);
    SYNC_READS.store(rounds, core::sync::atomic::Ordering::Release);
    let task = spawn_in(
        &process,
        "write_read sync reader",
        read_in_the_process,
        pinned,
    )?;
    Ok(Reader {
        _process: process,
        own,
        task,
    })
}

/// Let the check's channel and reader go once its rounds are answered.
fn finish_reader(task: &Task) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    wait_dead(task, deadline, "sync: the reader never ended")?;
    *SYNC_PEER.lock() = None;
    *SYNC_READER.lock() = None;
    Ok(())
}

/// A reader free to run anywhere, blocked on one processor and woken by a
/// Sync writer pinned to another, is moved onto the writer's processor in
/// some of [`SYNC_ROUNDS`] rounds -- one where the writer's processor has
/// something else queued keeps it home, by design -- and answers every time.
/// Answers how many rounds moved it.
///
/// Verifies: L.sched.8
fn check_a_sync_wake_moves_the_reader() -> Result<u32, &'static str> {
    let processors = crate::smp::count();
    let Reader {
        _process,
        own,
        task,
    } = start_reader(SYNC_ROUNDS, None)?;
    let mut moved = 0;
    for round in 0..SYNC_ROUNDS {
        // Wherever it waits now, the writer runs on the next processor.
        let (writer, answered_on) = wake_from(&own, &task, round, |home| (home + 1) % processors)?;
        if answered_on == writer + 1 {
            moved += 1;
        }
    }
    finish_reader(&task)?;
    if moved == 0 {
        return Err("sync: no Sync write from another processor moved a reader free to move");
    }
    Ok(moved)
}

/// A reader pinned to processor 1, woken by a Sync writer on processor 0,
/// stays on processor 1 and is still woken, in each of [`SYNC_ROUNDS`]
/// rounds: the move asks the reader's affinity first (`sched::may_place`).
/// As many rounds as the free reader's, because a round in which processor 0
/// has something else queued would not move it even if its affinity were
/// not asked.
///
/// Verifies: L.sched.8, H.SCHED.4
fn check_a_sync_wake_keeps_a_pinned_reader() -> Result<(), &'static str> {
    let Reader {
        _process,
        own,
        task,
    } = start_reader(SYNC_ROUNDS, Some(1))?;
    for round in 0..SYNC_ROUNDS {
        let (_, answered_on) = wake_from(&own, &task, round, |_| 0)?;
        if answered_on != 2 {
            return Err("sync: a Sync write moved a reader pinned to another processor off it");
        }
    }
    finish_reader(&task)
}
