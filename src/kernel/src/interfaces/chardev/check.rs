//! Stage 10's self-check of the chardev core, with a fake driver played by
//! the kernel (the certification consultant's N12, ledger 294 and 297; the
//! fix of F-63).
//!
//! A process given a PCI function asks for a chardev control and says HELLO,
//! as `nvrm` does, every call through the native dispatcher from stage 9's
//! [`Side`]. The programs on the other side are kernel tasks acting for a
//! process of their own, through the core's own admission and wait, which is
//! what an open or ioctl on a node runs. No NVIDIA code is involved, and the
//! nodes are gone again when the check ends.
//!
//! What is required, on a machine with a PCI function:
//!
//! * a HELLO listing a minor no name is given for, one listing a minor
//!   twice, and one naming another device are each refused as the session
//!   says, and the kernel's end closes after each;
//! * the driver's end cannot be duplicated (N2);
//! * an accepted HELLO publishes `nvidia0`, mode 0666, and a second driver
//!   listing the same minor is refused as `Taken`;
//! * an ioctl goes to the driver with its command and argument, the driver
//!   copies the program's bytes in and its own out, and the program's call
//!   returns the driver's value; a copy after the reply is refused;
//! * a copy naming a request of another driver's control is refused;
//! * an abandoned request's copies are refused, and abandoning a request
//!   waits out a copy in flight (N4);
//! * an answered request's call waits out a copy in flight too (L1);
//! * the 257th outstanding request is `EBUSY`, and requests abandoned
//!   while the driver reads nothing never let the queue to it grow past
//!   its room (F-63);
//! * when the driver goes, a waiting program wakes with `ENODEV` and the
//!   node is unpublished (N9).
//!
//! A machine with one PCI function checks everything but the two cases that
//! need a second driver, and says so.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_chardevctl::message::{Hello, MAX_NODES, Message, Op, VERSION};
use ferrix_chardevctl::node::MODE;
use ferrix_chardevctl::session::Refusal;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;

use super::{
    Answer, Ask, CONTROLS, Control, DRIVER_RIGHTS, MAX_OUTSTANDING, Request, STARTING, abandon,
    admit, await_answer, copy_done, published,
};
use crate::device::{self, DeviceNode};
use crate::object::check::{SCRATCH, Side, device_handle, reg};
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::process::Process;
use crate::timer;

/// A message being sent.
const OUTBOX: u64 = SCRATCH;
/// A message received.
const INBOX: u64 = SCRATCH + 0x100;
/// A `ReadActual`.
const ACTUAL: u64 = SCRATCH + 0x200;
/// A wait's deadline.
const DEADLINE: u64 = SCRATCH + 0x210;
/// The signals a wait observed.
const OBSERVED: u64 = SCRATCH + 0x218;
/// Handles a read may carry.
const IN_HANDLES: u64 = SCRATCH + 0x220;
/// The driver's copy buffer, and the program's argument buffer.
const BUFFER: u64 = SCRATCH + 0x800;

/// What the program puts in its buffer before the ioctl.
const PING: [u8; 8] = *b"chardev>";
/// What the driver copies back.
const PONG: [u8; 8] = *b"<chardev";
/// The ioctl's command and the driver's answer.
const COMMAND: u32 = 0xC0DE_0001;
const VALUE: u64 = 7;
/// The program's ioctl, on file 1 of minor 0, its argument the buffer.
const IOCTL: Ask = Ask {
    op: Op::Ioctl,
    file: 1,
    minor: 0,
    cmd: COMMAND,
    arg: BUFFER,
    pages: 0,
};

/// How long the check waits for the control's task.
const PATIENCE_NANOS: u64 = 10_000_000_000;
/// How long a call that must still be waiting is given to return wrongly.
const STILL_NANOS: u64 = 50_000_000;
/// Rounds of taking requests in until `EBUSY` and abandoning them (F-63).
const ROUNDS: usize = 3;

/// What the check measured, for the boot log.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls and HELLOs refused with exactly the answer they had to get.
    pub(crate) refusals: u32,
    /// Requests answered, by the driver or with `ENODEV`.
    pub(crate) answered: u32,
    /// Requests taken in and abandoned across the F-63 rounds.
    pub(crate) abandoned: u32,
    /// The requests the queue held once answered ones filled it (F-63).
    pub(crate) most_queued: usize,
    /// Why the cases needing a second driver were not checked.
    pub(crate) one_device: bool,
    /// Why nothing was checked.
    pub(crate) skipped: Option<&'static str>,
}

/// What a helper task is to do, and what it found.
enum Job {
    /// Wait for the request's answer, as a program's call does.
    Await(Arc<Control>, Arc<Request>, Arc<Process>),
    /// Abandon the request, as a signal makes a program's call do.
    Abandon(Arc<Control>, Arc<Request>),
}

/// The helper's job.
static JOB: SpinLock<Option<Job>> = SpinLock::new(None);
/// The helper's outcome, once it has one.
static OUTCOME: SpinLock<Option<Result<usize, Errno>>> = SpinLock::new(None);

/// Run the check. `Err` names the first thing that was not true.
///
/// # Errors
///
/// What was not true.
pub(crate) fn run() -> Result<Report, &'static str> {
    let nodes: Vec<Arc<DeviceNode>> = device::devices()
        .iter()
        .filter(|node| matches!(node.location(), device::Location::Pci(_)))
        .take(2)
        .cloned()
        .collect();
    let Some(first) = nodes.first() else {
        return Ok(Report {
            skipped: Some("no PCI function to serve nodes for"),
            ..Report::default()
        });
    };
    let mut report = Report {
        one_device: nodes.len() < 2,
        ..Report::default()
    };
    let driver = Side::new()?;
    let program = Side::new()?;
    refusals(&driver, first, &mut report)?;
    let control = accepted(&driver, first, &[0])?;
    let core = control_of(first)?;
    duplicate_refused(&driver, control, &mut report)?;
    if super::file::metadata(0).permissions != MODE || published(0).is_none() {
        return Err("an accepted HELLO did not publish nvidia0, mode 0666");
    }
    let other = nodes.get(1).map(|second| -> Result<_, &'static str> {
        let side = Side::new()?;
        let device = device_handle(&side, second)?;
        let taken = create(&side, device)?;
        hello(&side, taken, location(second), &[0])?;
        expect_refused(&side, taken, Refusal::Taken, &mut report)?;
        let other = accepted(&side, second, &[1])?;
        Ok((side, other))
    });
    let other = other.transpose()?;

    round_trip(
        &driver,
        control,
        &core,
        &program,
        other.as_ref(),
        &mut report,
    )?;
    abandoned(&driver, control, &core, &program, &mut report)?;
    answered_drains(&driver, control, &core, &program, &mut report)?;
    bounded(&driver, control, &core, &program, &mut report)?;
    death(&driver, control, &core, &program, &mut report)?;
    if let Some((side, other)) = other {
        close(&side, other)?;
        side.close_everything();
    }
    driver.close_everything();
    program.close_everything();
    settle()?;
    Ok(report)
}

/// Each HELLO the session refuses, refused, each on a control of its own
/// whose kernel end closes after the refusal.
fn refusals(side: &Side, node: &Arc<DeviceNode>, report: &mut Report) -> Result<(), &'static str> {
    let device = device_handle(side, node)?;
    let at = location(node);
    for (minors, wanted, wrong) in [
        (&[256_u16][..], Refusal::Minor, at),
        (&[3, 3][..], Refusal::Duplicate, at),
        (&[0][..], Refusal::Location, at ^ 1),
    ] {
        let control = create(side, device)?;
        hello(side, control, wrong, minors)?;
        expect_refused(side, control, wanted, report)?;
        settle()?;
    }
    Ok(())
}

/// A control on `node` whose HELLO for `minors` was accepted.
fn accepted(side: &Side, node: &Arc<DeviceNode>, minors: &[u16]) -> Result<Handle, &'static str> {
    let device = device_handle(side, node)?;
    let control = create(side, device)?;
    hello(side, control, location(node), minors)?;
    match receive(side, control)? {
        Some(Message::Ready(count)) if usize::from(count) == minors.len() => Ok(control),
        Some(Message::Refused(_)) => Err("a HELLO as specified was refused"),
        _ => Err("a HELLO as specified was not answered READY"),
    }
}

/// The driver's end of the control cannot be duplicated, and carries no
/// right beyond [`DRIVER_RIGHTS`] (N2).
fn duplicate_refused(
    side: &Side,
    control: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let rights = side
        .process
        .with_handles(|table| table.get(control).map(|(_, rights)| rights))
        .map_err(|_| "the driver's end is not in its table")?;
    if rights != DRIVER_RIGHTS || side.call(nr::HANDLE_DUPLICATE, &[reg(control), 0]).is_ok() {
        return Err("a driver's end of its chardev control could be duplicated");
    }
    report.refusals += 1;
    Ok(())
}

/// An ioctl there and back with a copy each way; then a copy after the
/// reply, and one through another driver's control, refused.
fn round_trip(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    other: Option<&(Side, Handle)>,
    report: &mut Report,
) -> Result<(), &'static str> {
    program.put(BUFFER, &PING)?;
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    if wire.op != Op::Ioctl || wire.cmd != COMMAND || wire.arg != BUFFER || wire.minor != 0 {
        return Err("an ioctl reached its driver with another command or argument");
    }
    if let Some((other_side, other_control)) = other {
        let staged = copy(other_side, *other_control, wire.id, nr::CHARDEV_COPY_IN, 8);
        if staged != Err(status::BAD_STATE) {
            return Err("a copy naming another driver's request was not refused");
        }
        report.refusals += 1;
    }
    let _ = copy(side, control, wire.id, nr::CHARDEV_COPY_IN, 8)
        .map_err(|_| "the driver could not copy the program's bytes in")?;
    if side.get(BUFFER, 8)? != PING {
        return Err("the driver's copy-in did not bring the program's bytes");
    }
    side.put(BUFFER, &PONG)?;
    let _ = copy(side, control, wire.id, nr::CHARDEV_COPY_OUT, 8)
        .map_err(|_| "the driver could not copy its bytes out")?;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return the driver's value");
    }
    if program.get(BUFFER, 8)? != PONG {
        return Err("the driver's copy-out did not reach the program's buffer");
    }
    report.answered += 1;
    if copy(side, control, wire.id, nr::CHARDEV_COPY_OUT, 8) != Err(status::BAD_STATE) {
        return Err("a copy after the reply was not refused");
    }
    report.refusals += 1;
    Ok(())
}

/// An abandoned request waits out a copy in flight, and its copies are
/// refused from then on (N4).
fn abandoned(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    in_flight(&request);
    start(Job::Abandon(Arc::clone(core), Arc::clone(&request)))?;
    still_waiting("an abandoned request's call returned while a copy for it was in flight")?;
    copy_done(&request);
    let _ = finish()?;
    if copy(side, control, request.wire.id, nr::CHARDEV_COPY_IN, 8) != Err(status::BAD_STATE) {
        return Err("a copy for an abandoned request was not refused");
    }
    report.refusals += 1;
    drain_channel(side, control);
    Ok(())
}

/// An answered request's call waits out a copy in flight (the consultant's
/// L1, ledger 297).
fn answered_drains(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    in_flight(&request);
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    reply(side, control, request.wire.id, VALUE)?;
    still_waiting("an answered request's call returned while a copy for it was in flight")?;
    copy_done(&request);
    if finish()? != Ok(VALUE as usize) {
        return Err("an answered request's call did not return its answer after the copy");
    }
    report.answered += 1;
    drain_channel(side, control);
    Ok(())
}

/// The 257th request is refused, and with the driver reading nothing the
/// queue to it never holds more than [`MAX_OUTSTANDING`] requests (F-63):
/// requests abandoned before their turn leave the queue at once, and
/// requests the driver answers before their turn -- it may guess their ids
/// -- still count against the queue's room until they leave it.
fn bounded(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    for _ in 0..ROUNDS {
        let taken = fill(core, program)?;
        if taken.len() != MAX_OUTSTANDING {
            return Err("after every request was abandoned a new one was refused");
        }
        report.refusals += 1;
        for request in &taken {
            abandon(core, request);
        }
        report.abandoned += u32::try_from(taken.len()).unwrap_or(u32::MAX);
        if queued(core) != 0 {
            return Err("abandoned requests stayed in the queue to a driver that reads nothing");
        }
    }
    let answered = fill(core, program)?;
    for request in &answered {
        reply(side, control, request.wire.id, VALUE)?;
    }
    let more = fill(core, program)?;
    let most = queued(core);
    report.most_queued = most;
    if most > MAX_OUTSTANDING {
        return Err("the queue to a driver that reads nothing held more requests than its room");
    }
    for request in &more {
        abandon(core, request);
    }
    report.abandoned += u32::try_from(more.len()).unwrap_or(u32::MAX);
    drain_channel(side, control);
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while queued(core) != 0 {
        if timer::now_nanos() > deadline {
            return Err("answered requests stayed in the queue after its driver read again");
        }
        drain_channel(side, control);
        sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// Take requests in until `EBUSY`; a 257th taken is an error.
fn fill(core: &Arc<Control>, program: &Side) -> Result<Vec<Arc<Request>>, &'static str> {
    let mut taken = Vec::new();
    taken
        .try_reserve_exact(MAX_OUTSTANDING + 1)
        .map_err(|_| "no memory for the check's requests")?;
    loop {
        match admit(core, &program.process, IOCTL) {
            Ok(request) => taken.push(request),
            Err(Errno::EBUSY) => return Ok(taken),
            Err(_) => return Err("a request was refused other than EBUSY"),
        }
        if taken.len() > MAX_OUTSTANDING {
            return Err("a 257th request was admitted");
        }
    }
}

/// The driver goes: a waiting program wakes with `ENODEV`, the node is
/// unpublished (N9).
fn death(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&program.process),
    ))?;
    close(side, control)?;
    if finish()? != Err(Errno::ENODEV) {
        return Err("a program waiting on a driver that went did not wake with ENODEV");
    }
    report.answered += 1;
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while published(0).is_some() {
        if timer::now_nanos() > deadline {
            return Err("a node stayed published after its driver went");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The driver's side
// ---------------------------------------------------------------------------

/// `chardev_control_create` on `device`.
fn create(side: &Side, device: Handle) -> Result<Handle, &'static str> {
    side.handle(
        nr::CHARDEV_CONTROL_CREATE,
        &[reg(device)],
        "chardev_control_create on a device with MANAGE failed",
    )
}

/// The device's location word, which HELLO must name.
fn location(node: &DeviceNode) -> u32 {
    node.describe().location
}

/// Say HELLO for `minors` at `at`.
fn hello(side: &Side, control: Handle, at: u32, minors: &[u16]) -> Result<(), &'static str> {
    let mut list = [0_u16; MAX_NODES];
    for (slot, minor) in list.iter_mut().zip(minors) {
        *slot = *minor;
    }
    let encoded = Message::Hello(Hello {
        version: VERSION,
        location: at,
        count: minors.len(),
        minors: list,
    })
    .encode();
    side.put(OUTBOX, encoded.as_bytes())?;
    let _ = side
        .call(
            nr::CHANNEL_WRITE,
            &[reg(control), OUTBOX, encoded.as_bytes().len() as u64, 0, 0],
        )
        .map_err(|_| "sending a HELLO failed")?;
    Ok(())
}

/// The next message on `control`, or `None` once the kernel's end closed.
fn receive(side: &Side, control: Handle) -> Result<Option<Message>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    side.put(DEADLINE, &deadline.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[
                reg(control),
                u64::from((Signals::READABLE | Signals::PEER_CLOSED).0),
                DEADLINE,
                OBSERVED,
            ],
        )
        .map_err(|_| "the chardev core did not answer in time")?;
    match read(side, control) {
        Some(bytes) => Message::decode(&bytes)
            .map(Some)
            .map_err(|_| "the chardev core wrote something that is not a message"),
        None => Ok(None),
    }
}

/// One message's bytes off `control`, if one is there.
fn read(side: &Side, control: Handle) -> Option<Vec<u8>> {
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[
                reg(control),
                INBOX,
                ferrix_chardevctl::message::MAX_BYTES as u64,
                IN_HANDLES,
                0,
                ACTUAL,
            ],
        )
        .ok()?;
    let bytes = side.get_u32(ACTUAL).ok()? as usize;
    side.get(INBOX, bytes).ok()
}

/// Read and drop whatever REQUESTs are waiting on `control`.
fn drain_channel(side: &Side, control: Handle) {
    while read(side, control).is_some() {}
}

/// Require REFUSED with `wanted`, then the kernel's end to close.
fn expect_refused(
    side: &Side,
    control: Handle,
    wanted: Refusal,
    report: &mut Report,
) -> Result<(), &'static str> {
    match receive(side, control)? {
        Some(Message::Refused(reason)) if reason == wanted => {}
        Some(Message::Refused(_)) => return Err("a HELLO was refused for the wrong reason"),
        _ => return Err("a HELLO that had to be refused was not"),
    }
    if receive(side, control)?.is_some() {
        return Err("the chardev core wrote after refusing a HELLO");
    }
    if !Signals(side.get_u32(OBSERVED)?).intersects(Signals::PEER_CLOSED) {
        return Err("the chardev core kept its end of a refused control open");
    }
    close(side, control)?;
    report.refusals += 1;
    Ok(())
}

/// `chardev_copy_in` or `_out` of `len` bytes between the program's
/// [`BUFFER`] and the driver's.
fn copy(side: &Side, control: Handle, id: u64, number: usize, len: u64) -> Result<usize, Errno> {
    side.call(number, &[reg(control), id, BUFFER, BUFFER, len])
}

/// `chardev_reply` with success and `value`.
fn reply(side: &Side, control: Handle, id: u64, value: u64) -> Result<(), &'static str> {
    side.call(nr::CHARDEV_REPLY, &[reg(control), id, 0, value])
        .map(|_| ())
        .map_err(|_| "the driver's reply was refused")
}

/// Close the driver's end of `control`.
fn close(side: &Side, control: Handle) -> Result<(), &'static str> {
    side.call(nr::HANDLE_CLOSE, &[reg(control)])
        .map(|_| ())
        .map_err(|_| "closing a driver's end failed")
}

// ---------------------------------------------------------------------------
// The core's side, read and driven from here
// ---------------------------------------------------------------------------

/// The live control serving `node`.
fn control_of(node: &Arc<DeviceNode>) -> Result<Arc<Control>, &'static str> {
    CONTROLS
        .lock()
        .iter()
        .find(|control| Arc::ptr_eq(&control.device, node))
        .cloned()
        .ok_or("an accepted control is not among the live ones")
}

/// How many requests the queue to `control`'s driver holds.
fn queued(control: &Control) -> usize {
    let state = control.state.lock();
    state
        .outgoing
        .iter()
        .filter(|held| matches!(held, super::Outgoing::Request(_)))
        .count()
}

/// Mark a copy for `request` in flight, as `copy` does before its chunks,
/// so a wait for its end can be seen to wait.
fn in_flight(request: &Request) {
    let mut inner = request.inner.lock();
    inner.copying = inner.copying.saturating_add(1);
}

/// Start the helper on `job`.
fn start(job: Job) -> Result<(), &'static str> {
    *OUTCOME.lock() = None;
    *JOB.lock() = Some(job);
    let _ = sched::spawn("chardev-check", helper, 0, ferrix_sched::NICE_0_WEIGHT)?;
    Ok(())
}

/// The helper: run the job, record the outcome.
fn helper(_: usize) {
    let job = JOB.lock().take();
    let outcome = match job {
        // An ioctl is answered with a value; a mapping would be wrong, and
        // fails the check's comparison as EIO.
        Some(Job::Await(control, request, client)) => await_answer(&control, &request, &client)
            .and_then(|answer| match answer {
                Answer::Value(value) => Ok(value),
                Answer::Map(_) => Err(Errno::EIO),
            }),
        Some(Job::Abandon(control, request)) => {
            abandon(&control, &request);
            Ok(0)
        }
        None => Err(Errno::EINVAL),
    };
    *OUTCOME.lock() = Some(outcome);
}

/// Require the helper to have no outcome yet, a while after it started.
fn still_waiting(what: &'static str) -> Result<(), &'static str> {
    sched::sleep_for(STILL_NANOS);
    if OUTCOME.lock().is_some() {
        return Err(what);
    }
    Ok(())
}

/// The helper's outcome, within the patience.
fn finish() -> Result<Result<usize, Errno>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let outcome = OUTCOME.lock().take();
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
        if timer::now_nanos() > deadline {
            return Err("a program's call into the chardev core never returned");
        }
        sched::sleep_for(1_000_000);
    }
}

/// Wait until every control's task of the check has ended and let its
/// device go.
fn settle() -> Result<(), &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !CONTROLS.lock().is_empty() || !STARTING.lock().is_empty() {
        if timer::now_nanos() > deadline {
            return Err("a chardev control's task did not end after its driver went");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(())
}
