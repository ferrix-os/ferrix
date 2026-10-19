//! Kernel-attested sender identity (`docs/microkernel/K6.md` §6): every
//! channel message carries the process and job that wrote it, stamped by the
//! kernel at the write, and a reader reads them with
//! `channel_read_attested` (0x1014) and matches them with `object_id`
//! (0x100A).
//!
//! Every call goes through the native dispatch, from check processes
//! ([`Side`]); the slot's write is `channel_write_read` made through the
//! architecture's own entry by a thread of the writing process. The cases
//! run in this order, so that each negative control of K6.md §7 fires on its
//! own case's sentence:
//!
//! 5. ids are fresh: every process made here -- plain, forked, and made by
//!    `process_create` -- has an id above every one seen before it, and none
//!    is zero; one made after another ended does not get its id;
//! 1. the writer, not the body: a message whose body holds the reader's own
//!    ids reads as its writer's, and those equal `object_id` on the writer
//!    and its job;
//! 2. the slot: a message `channel_write_read` wrote into the slot reads as
//!    its writer's;
//! 8. `channel_write_read` answers no identity: the writer's empty answer
//!    comes back as zero and three zero words;
//! 3. handle transfer: a message keeps its writer's stamp when the end it was
//!    written through moves, and when the end it waits in moves; a message
//!    written through a moved end is its new holder's;
//! 4. exit: a message written by a process that then ended reads as that
//!    process's, and `object_id` still answers its id;
//! 6. the job at the write: a process moved between two writes stamps each
//!    with the job it was in;
//! 7. the kernel: a message the kernel writes reads as zero and zero;
//! 9. the calls' refusals: `channel_read_attested` without `READ`, too small,
//!    and with an unwritable record, and `object_id` on the wrong object, a
//!    closed handle and an unwritable answer.
//!
//! Every record is read from an address that is not 8-byte aligned, since
//! the call promises none (L.object.199).

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;

use super::channel::{Endpoint, Sender};
use super::check::{SCRATCH, Side, Spawner, reg};
use super::job::{Job, KILLED_STATUS};
use super::process::ProcessRef;
use super::{Object, Transfer};
use crate::arch;
use crate::sync::SpinLock;
use crate::syscall::check::spawn_in;
use crate::syscall::process;

/// A message's body, staged in the writer's scratch region.
const BODY: u64 = SCRATCH + 0x400;
/// Where a read's bytes go.
const INBOX: u64 = SCRATCH + 0x500;
/// Where a read's handles go.
const HANDLES: u64 = SCRATCH + 0x600;
/// Where a read's record goes: one past an 8-byte boundary, on purpose.
const RECORD: u64 = SCRATCH + 0x701;
/// Where `object_id` answers, and `channel_create` writes its pair.
const OUT: u64 = SCRATCH + 0x740;
/// An address no check process maps: a record or answer written there
/// faults.
const UNMAPPED: u64 = 0x10;
/// The record's size, `ReadAttested`'s.
const RECORD_BYTES: usize = 24;
/// What the record is filled with before a read that must not write all of
/// it.
const UNTOUCHED: u8 = 0xEE;
/// How long the check waits for its writer thread.
const PATIENCE_NANOS: u64 = 120_000_000_000;
/// How often it looks.
const POLL_NANOS: u64 = 1_000_000;

/// What the boot line counts.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Messages read with their writer's stamp.
    pub(crate) read: u32,
    /// Processes whose ids were checked fresh.
    pub(crate) fresh: u32,
    /// Refusals answered as the calls promise.
    pub(crate) refusals: u32,
    /// Whether a process made after another ended was given its pid, which
    /// its id never is.
    pub(crate) pid_reused: bool,
}

/// What a `channel_read_attested` answered: the sizes, the writer, and the
/// bytes.
#[derive(Debug, PartialEq, Eq)]
struct Attested {
    /// Bytes.
    bytes: u32,
    /// Handles.
    handles: u32,
    /// The writer's process id.
    process: u64,
    /// The writer's job id.
    job: u64,
    /// The message's bytes.
    body: Vec<u8>,
}

/// A process's and its job's ids, as a stamp should read.
fn ids_of(side: &Side) -> (u64, u64) {
    (side.process.id(), side.process.job_id())
}

/// Read the next message on `end` in `side` with `channel_read_attested`.
fn read_attested(side: &Side, end: Handle) -> Result<Attested, Errno> {
    let _ = side.call(
        nr::CHANNEL_READ_ATTESTED,
        &[reg(end), INBOX, 64, HANDLES, 4, RECORD],
    )?;
    let record = side
        .get(RECORD, RECORD_BYTES)
        .map_err(|_| status::FAULT)?;
    let word = |at: usize, width: usize| {
        let mut raw = [0_u8; 8];
        if let (Some(to), Some(from)) = (raw.get_mut(..width), record.get(at..at + width)) {
            to.copy_from_slice(from);
        }
        u64::from_le_bytes(raw)
    };
    let bytes = word(0, 4) as u32;
    let body = side
        .get(INBOX, bytes as usize)
        .map_err(|_| status::FAULT)?;
    Ok(Attested {
        bytes,
        handles: word(4, 4) as u32,
        process: word(8, 8),
        job: word(16, 8),
        body,
    })
}

/// Read the next message on `end` and require it to be `body`, written by
/// `writer`.
fn expect_from(
    side: &Side,
    end: Handle,
    body: &[u8],
    writer: (u64, u64),
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    let read = read_attested(side, end).map_err(|_| what)?;
    if read.body != body || read.handles != 0 || (read.process, read.job) != writer {
        return Err(what);
    }
    report.read += 1;
    Ok(())
}

/// `channel_write` of `body` on `end` from `side`, carrying `handles`.
fn write(side: &Side, end: Handle, body: &[u8], handles: &[Handle]) -> Result<(), Errno> {
    side.put(BODY, body).map_err(|_| status::FAULT)?;
    let values: Vec<u8> = handles.iter().flat_map(|h| h.0.to_ne_bytes()).collect();
    side.put(HANDLES, &values).map_err(|_| status::FAULT)?;
    side.call(
        nr::CHANNEL_WRITE,
        &[
            reg(end),
            BODY,
            body.len() as u64,
            HANDLES,
            handles.len() as u64,
        ],
    )
    .map(|_| ())
}

/// `object_id` of `handle` in `side`.
fn object_id(side: &Side, handle: Handle) -> Result<u64, Errno> {
    let _ = side.call(nr::OBJECT_ID, &[reg(handle), OUT])?;
    let raw = side.get(OUT, 8).map_err(|_| status::FAULT)?;
    let mut word = [0_u8; 8];
    word.copy_from_slice(&raw);
    Ok(u64::from_ne_bytes(word))
}

/// A channel with one end in `a` and the other in `b`.
fn connect(a: &Side, b: &Side) -> Result<(Handle, Handle), &'static str> {
    let _ = a
        .call(nr::CHANNEL_CREATE, &[OUT])
        .map_err(|_| "sender check: channel_create failed")?;
    let near = Handle(a.get_u32(OUT)?);
    let far = Handle(a.get_u32(OUT + 4)?);
    let moved = a
        .process
        .with_handles(|table| table.take_many(&[far], Rights::NONE))
        .map_err(|_| "sender check: could not take an end out of its maker")?;
    let placed = b
        .process
        .with_handles(|table| table.insert_many(moved))
        .map_err(|_| "sender check: could not place an end in its reader")?;
    let far = placed
        .first()
        .copied()
        .ok_or("sender check: no end was placed")?;
    Ok((near, far))
}

/// A handle in `side` to `object`, with `rights`.
fn give(side: &Side, object: Object, rights: Rights) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(object, rights))
        .map_err(|_| "sender check: no room in a check's table")
}

/// Run every case. See the module.
///
/// # Errors
///
/// The first thing that was not so.
///
/// Verifies: H.OBJ.19, L.object.197, L.object.198, L.object.199
/// Verifies: L.object.200, L.object.201, L.object.202
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let a = Side::new()?;
    let b = Side::new()?;
    let c = Side::new()?;
    let highest = check_ids_are_fresh(&[&a, &b, &c], &mut report)?;
    check_the_writer_not_the_body(&a, &b, &mut report)?;
    check_the_slot_and_the_registers(&b, &mut report)?;
    check_a_transfer(&a, &b, &c, &mut report)?;
    check_an_ended_writer(&b, highest, &mut report)?;
    check_the_job_at_the_write(&b, &mut report)?;
    check_the_kernel(&b, &mut report)?;
    check_the_refusals(&a, &b, &mut report)?;
    for side in [&a, &b, &c] {
        side.close_everything();
    }
    Ok(report)
}

/// Case 5: every process made here has an id above every one seen before it,
/// and none is zero, made plainly, by a fork, or by `process_create`.
/// Answers the highest seen.
fn check_ids_are_fresh(sides: &[&Side], report: &mut Report) -> Result<u64, &'static str> {
    const STALE: &str = "sender case 5: a process was given an id not above every id before it";
    let mut highest = 0;
    for side in sides {
        let id = side.process.id();
        if id == 0 || id <= highest {
            return Err(STALE);
        }
        highest = id;
        report.fresh += 1;
    }
    let first = sides.first().ok_or("sender case 5: no process to fork")?;
    let forked = process::fork_for_check(&first.process)
        .map_err(|_| "sender case 5: a check process could not be forked")?;
    if forked.id() <= highest {
        return Err(STALE);
    }
    highest = forked.id();
    report.fresh += 1;
    process::kill(&forked, 0);
    drop(forked);
    if !arch::USER_ARGUMENT_PROGRAM.is_empty() {
        let maker = Side::new()?;
        if maker.process.id() <= highest {
            return Err(STALE);
        }
        highest = maker.process.id();
        let spawner = Spawner::new(&maker)?;
        let child = spawner.made(spawner.root, "sender case 5: process_create failed")?;
        let id = object_id(&maker, child)
            .map_err(|_| "sender case 5: object_id on a made process failed")?;
        if id <= highest {
            return Err(STALE);
        }
        highest = id;
        report.fresh += 2;
        // Closing the only handle ends the unstarted child.
        maker.close_everything();
    }
    Ok(highest)
}

/// Case 1: a body naming the reader reads as its writer's, which `object_id`
/// confirms on the writer and its job.
fn check_the_writer_not_the_body(
    a: &Side,
    b: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    const WRONG: &str = "sender case 1: a message did not read as its writer's process and job";
    let (near, far) = connect(a, b)?;
    let (b_process, b_job) = ids_of(b);
    let mut forged = Vec::new();
    forged.extend_from_slice(&b_process.to_le_bytes());
    forged.extend_from_slice(&b_job.to_le_bytes());
    write(a, near, &forged, &[]).map_err(|_| "sender case 1: channel_write failed")?;
    let writer = ids_of(a);
    if writer == (b_process, b_job) || writer.0 == 0 || writer.1 == 0 {
        return Err(WRONG);
    }
    expect_from(b, far, &forged, writer, WRONG, report)?;
    let process = give(b, Object::Process(ProcessRef::new(&a.process)), Rights::NONE)?;
    let job = give(b, Object::Job(a.process.job()), Rights::NONE)?;
    if object_id(b, process) != Ok(writer.0) || object_id(b, job) != Ok(writer.1) {
        return Err("sender case 1: object_id did not answer the writer's process and job ids");
    }
    Ok(())
}

/// Handed to the slot's writer thread: its end.
static SLOT_WRITER: SpinLock<Option<Handle>> = SpinLock::new(None);
/// What its `channel_write_read` answered.
static SLOT_ANSWER: SpinLock<Option<(isize, [u64; 3])>> = SpinLock::new(None);

/// The slot's writer: one `channel_write_read` of `"slot"`, then the end of
/// its process.
fn write_into_the_slot(_argument: usize) {
    let end = SLOT_WRITER.lock().take();
    if let Some(end) = end {
        let mut word = [0_u8; size_of::<usize>()];
        if let Some(head) = word.get_mut(..4) {
            head.copy_from_slice(b"slot");
        }
        let answer = arch::drive_native_words(
            nr::CHANNEL_WRITE_READ,
            [
                u64::from(end.0),
                4,
                usize::from_ne_bytes(word) as u64,
                0,
                0,
                0,
            ],
        );
        *SLOT_ANSWER.lock() = Some(answer);
    }
    process::exit_current(0)
}

/// Cases 2 and 8: a message `channel_write_read` wrote into the slot reads as
/// its writer's, and the writer's empty answer carries no identity.
fn check_the_slot_and_the_registers(
    b: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    // A process of its own, since the writer's thread ends it.
    let writer = Side::new()?;
    let (near, far) = connect(&writer, b)?;
    let reading = b
        .process
        .with_handles(|table| match table.get(far) {
            Ok((Object::Channel(end), _)) => Some(Arc::clone(end)),
            _ => None,
        })
        .ok_or("sender case 2: the reader's end is not a channel")?;
    *SLOT_ANSWER.lock() = None;
    *SLOT_WRITER.lock() = Some(near);
    let task = spawn_in(&writer.process, "sender slot", write_into_the_slot, None)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !reading.readable_or_closed() {
        if task.is_dead() || crate::timer::now_nanos() >= deadline {
            return Err("sender case 2: the slot's writer never wrote");
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
    expect_from(
        b,
        far,
        b"slot",
        ids_of(&writer),
        "sender case 2: the slot's message did not read as its writer's",
        report,
    )?;
    // The answer: nothing, which must come back as nothing.
    write(b, far, &[], &[]).map_err(|_| "sender case 8: the reply could not be written")?;
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err("sender case 8: the slot's writer was never answered");
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
    if SLOT_ANSWER.lock().take() != Some((0, [0, 0, 0])) {
        return Err("sender case 8: channel_write_read answered more than the empty reply");
    }
    drop(reading);
    writer.close_everything();
    Ok(())
}

/// Case 3: a stamp is the writer's, wherever either end goes.
fn check_a_transfer(a: &Side, b: &Side, c: &Side, report: &mut Report) -> Result<(), &'static str> {
    const MOVED: &str = "sender case 3: a message read as another than its writer after a transfer";
    // The end written through moves: A writes, sends its end to C, C writes.
    let (x_a, x_b) = connect(a, b)?;
    let (y_a, y_c) = connect(a, c)?;
    write(a, x_a, b"before", &[]).map_err(|_| "sender case 3: channel_write failed")?;
    write(a, y_a, b"end", &[x_a]).map_err(|_| "sender case 3: an end could not be sent")?;
    let _ = c
        .call(
            nr::CHANNEL_READ,
            &[reg(y_c), INBOX, 64, HANDLES, 4, RECORD],
        )
        .map_err(|_| "sender case 3: the sent end could not be read")?;
    let x_c = Handle(c.get_u32(HANDLES)?);
    write(c, x_c, b"after", &[]).map_err(|_| "sender case 3: C's write failed")?;
    expect_from(b, x_b, b"before", ids_of(a), MOVED, report)?;
    expect_from(b, x_b, b"after", ids_of(c), MOVED, report)?;

    // The end a message waits in moves: A writes to B, B sends that end to
    // C, and C reads A's message as A's.
    let (z_a, z_b) = connect(a, b)?;
    let (w_b, w_c) = connect(b, c)?;
    write(a, z_a, b"waiting", &[]).map_err(|_| "sender case 3: channel_write failed")?;
    write(b, w_b, b"end", &[z_b]).map_err(|_| "sender case 3: an end could not be sent")?;
    let _ = c
        .call(
            nr::CHANNEL_READ,
            &[reg(w_c), INBOX, 64, HANDLES, 4, RECORD],
        )
        .map_err(|_| "sender case 3: the sent end could not be read")?;
    let z_c = Handle(c.get_u32(HANDLES)?);
    expect_from(c, z_c, b"waiting", ids_of(a), MOVED, report)
}

/// Case 4: a writer that has ended is still the writer, and its id is not
/// given to the next process.
fn check_an_ended_writer(b: &Side, highest: u64, report: &mut Report) -> Result<(), &'static str> {
    let gone = Side::new()?;
    let (near, far) = connect(&gone, b)?;
    let ids = ids_of(&gone);
    let pid = gone.process.pid();
    let handle = give(b, Object::Process(ProcessRef::new(&gone.process)), Rights::NONE)?;
    write(&gone, near, b"last words", &[]).map_err(|_| "sender case 4: channel_write failed")?;
    gone.close_everything();
    process::kill(&gone.process, KILLED_STATUS);
    if !gone.process.exit().is_terminated() {
        return Err("sender case 4: the writer did not end");
    }
    drop(gone);
    expect_from(
        b,
        far,
        b"last words",
        ids,
        "sender case 4: an ended writer's message did not read as its writer's",
        report,
    )?;
    if object_id(b, handle) != Ok(ids.0) {
        return Err("sender case 4: object_id on an ended process did not answer its id");
    }
    let next = Side::new()?;
    if next.process.id() <= highest.max(ids.0) {
        return Err("sender case 5: a process made after another ended was given an old id");
    }
    report.pid_reused = next.process.pid() == pid;
    report.fresh += 1;
    next.close_everything();
    Ok(())
}

/// Case 6: a process moved between two writes stamps each with the job it
/// was in.
fn check_the_job_at_the_write(b: &Side, report: &mut Report) -> Result<(), &'static str> {
    const JOB: &str = "sender case 6: a message did not carry its writer's job at the write";
    let mover = Side::new()?;
    let (near, far) = connect(&mover, b)?;
    write(&mover, near, b"here", &[]).map_err(|_| "sender case 6: channel_write failed")?;
    let before = ids_of(&mover);
    let elsewhere = Job::new_root().map_err(|_| "sender case 6: no memory for a job")?;
    elsewhere
        .adopt(&mover.process)
        .map_err(|_| "sender case 6: a live job refused a process")?;
    write(&mover, near, b"there", &[]).map_err(|_| "sender case 6: channel_write failed")?;
    let after = (mover.process.id(), elsewhere.id());
    if before.1 == after.1 {
        return Err(JOB);
    }
    expect_from(b, far, b"here", before, JOB, report)?;
    expect_from(b, far, b"there", after, JOB, report)?;
    mover.close_everything();
    Ok(())
}

/// Case 7: what the kernel writes reads as zero and zero.
fn check_the_kernel(b: &Side, report: &mut Report) -> Result<(), &'static str> {
    let (kernels, readers) =
        Endpoint::pair().map_err(|_| "sender case 7: no memory for a channel")?;
    let end = give(b, Object::Channel(readers), Rights::CHANNEL)?;
    kernels
        .write(Sender::KERNEL, b"kernel".to_vec(), 0, || {
            Ok::<Vec<Transfer>, core::convert::Infallible>(Vec::new())
        })
        .map_err(|_| "sender case 7: the kernel's write failed")?;
    expect_from(
        b,
        end,
        b"kernel",
        (0, 0),
        "sender case 7: the kernel's message did not read as zero and zero",
        report,
    )
}

/// Case 9: the calls refuse as they promise, and a refused read keeps the
/// message and its stamp.
fn check_the_refusals(a: &Side, b: &Side, report: &mut Report) -> Result<(), &'static str> {
    let (near, far) = connect(a, b)?;
    let body = b"sixteen bytes!!!";
    write(a, near, body, &[]).map_err(|_| "sender case 9: channel_write failed")?;

    // Without READ: refused, nothing taken.
    let write_only = b
        .handle(
            nr::HANDLE_DUPLICATE,
            &[reg(far), u64::from(Rights::WRITE.0)],
            "sender case 9: handle_duplicate failed",
        )?;
    if read_attested(b, write_only) != Err(status::ACCESS_DENIED) {
        return Err("sender case 9: channel_read_attested without READ was not ACCESS_DENIED");
    }
    report.refusals += 1;

    // Too small: the sizes only; the writer's fields untouched.
    b.put(RECORD, &[UNTOUCHED; RECORD_BYTES])?;
    if b.call(
        nr::CHANNEL_READ_ATTESTED,
        &[reg(far), INBOX, 1, HANDLES, 4, RECORD],
    ) != Err(status::BUFFER_TOO_SMALL)
    {
        return Err("sender case 9: a read too small was not BUFFER_TOO_SMALL");
    }
    let record = b.get(RECORD, RECORD_BYTES)?;
    let mut sizes = Vec::new();
    sizes.extend_from_slice(&16_u32.to_ne_bytes());
    sizes.extend_from_slice(&0_u32.to_ne_bytes());
    if record.get(..8) != Some(sizes.as_slice())
        || record.get(8..).is_none_or(|rest| rest.iter().any(|&byte| byte != UNTOUCHED))
    {
        return Err("sender case 9: BUFFER_TOO_SMALL wrote more than the sizes");
    }
    report.refusals += 1;

    // An unwritable record: refused, and the message requeued with its stamp.
    if b.call(
        nr::CHANNEL_READ_ATTESTED,
        &[reg(far), INBOX, 64, HANDLES, 4, UNMAPPED],
    ) != Err(status::FAULT)
    {
        return Err("sender case 9: a read with an unwritable record was not FAULT");
    }
    report.refusals += 1;
    expect_from(
        b,
        far,
        body,
        ids_of(a),
        "sender case 9: a message whose record faulted was not read again whole, as its writer's",
        report,
    )?;

    // object_id: the wrong object, a closed handle, an unwritable answer.
    if object_id(b, far) != Err(status::WRONG_TYPE) {
        return Err("sender case 9: object_id on a channel was not WRONG_TYPE");
    }
    let process = give(b, Object::Process(ProcessRef::new(&a.process)), Rights::NONE)?;
    if b.call(nr::OBJECT_ID, &[reg(process), UNMAPPED]) != Err(status::FAULT) {
        return Err("sender case 9: object_id to an unwritable answer was not FAULT");
    }
    let _ = b
        .call(nr::HANDLE_CLOSE, &[reg(process)])
        .map_err(|_| "sender case 9: handle_close failed")?;
    if object_id(b, process) != Err(status::BAD_HANDLE) {
        return Err("sender case 9: object_id on a closed handle was not BAD_HANDLE");
    }
    report.refusals += 3;
    Ok(())
}
