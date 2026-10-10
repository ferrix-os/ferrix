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
//! * a dmabuf (N3b; the consultant's B10, ledger 316): unknown flags, a
//!   handle that is not a VMO, a VMO handle without `TRANSFER`, and a
//!   writable dmabuf of a handle without `WRITE` are each refused; a second
//!   install for the cookie gives the same object and says it made nothing,
//!   the cookie over another VMO is refused; resolve gives the cookie back
//!   and refuses a descriptor that is not a dmabuf; install and resolve for
//!   an answered request are refused; the driver hears one release, only
//!   after the last of two descriptors and a mapping went; and a dmabuf
//!   outlives its driver with its bytes, its going after unheard (B7);
//! * a name-only dmabuf (ledger 632, N6): a VMO register that is not 0 --
//!   even one naming a VMO with `TRANSFER` --, a size of 0, not whole
//!   pages or over the bound, and a VMO install with a size are each
//!   refused; a live cookie asked in the other kind or with another size is
//!   `ALREADY_BOUND`; two installs give one object; `mmap`, shared or
//!   private, is `ENODEV` and `fstat` gives the size; resolve gives the
//!   cookie to its maker and `BAD_HANDLE` to another driver; one release
//!   after the last of two descriptors; and one kept past the driver's
//!   death still says its size, still maps nothing and closes unheard;
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
    /// dmabuf installs and resolves refused as specified (B10).
    pub(crate) dmabuf_refusals: u32,
    /// name-only dmabuf calls and mappings refused as specified (N6).
    pub(crate) name_refusals: u32,
    /// sync_file installs, signals and resolves refused as specified (S9).
    pub(crate) sync_refusals: u32,
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
    let other_core = nodes.get(1).map(control_of).transpose()?;

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
    let kept = dmabufs(&driver, control, &core, &program, &mut report)?;
    let foreign = other
        .as_ref()
        .zip(other_core.as_ref())
        .map(|((side, handle), core)| (side, *handle, core));
    let kept_name = names(&driver, control, &core, &program, foreign, &mut report)?;
    let fence = fences(&driver, control, &core, &program, &mut report)?;
    death(&driver, control, &core, &program, &mut report)?;
    fence_outlived(&program, fence)?;
    outlived(&program, kept)?;
    name_outlived(&program, kept_name, &mut report)?;
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

/// The cookie the check names its dmabuf by.
const COOKIE: u64 = 0xD3AB_0F;
/// The cookie the fences are named by.
const FENCE: u64 = 0xFE_0C3;
/// Its buffer: two pages.
const DMABUF_BYTES: u64 = 2 * 4096;
/// What the driver writes in it, and the program reads through a mapping.
const STAMP: [u8; 8] = *b"dmabuf<>";

/// A descriptor the check keeps open past the driver's death, with its
/// mapping's address.
struct Kept {
    fd: i32,
    at: u64,
}

/// The dmabuf calls (B10): the refusals, one object a cookie, resolve, and
/// one release after the last descriptor and mapping. Answers with a
/// descriptor and a mapping kept for [`outlived`].
fn dmabufs(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<Kept, &'static str> {
    use ferrix_native_abi::rights::Rights;
    use ferrix_native_abi::types::{DMABUF_MADE, DMABUF_TELL_MADE, DMABUF_WRITABLE};
    drain_channel(side, control);
    let vmo = side.handle(nr::VMO_CREATE, &[DMABUF_BYTES], "vmo_create failed")?;
    let other = side.handle(nr::VMO_CREATE, &[DMABUF_BYTES], "vmo_create failed")?;
    let held = |rights: u32| -> Result<Handle, &'static str> {
        side.handle(
            nr::HANDLE_DUPLICATE,
            &[reg(vmo), u64::from(rights)],
            "handle_duplicate of a VMO failed",
        )
    };
    let no_transfer = held(Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0)?;
    let read_only = held(Rights::READ.0 | Rights::TRANSFER.0 | Rights::MAP.0)?;
    side.put(BUFFER, &STAMP)?;
    // `vmo_write` reads its offset from memory.
    side.put(DEADLINE, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), BUFFER, 8, DEADLINE])
        .map_err(|_| "vmo_write failed")?;

    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let install = |handle: Handle, cookie: u64, flags: u64| {
        side.call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, reg(handle), cookie, flags],
        )
    };
    for (handle, flags, wanted, what) in [
        (
            vmo,
            1 << 9,
            status::INVALID_ARGS,
            "a dmabuf with an unknown flag was installed",
        ),
        (
            control,
            0,
            status::WRONG_TYPE,
            "a dmabuf of a handle that is no VMO was installed",
        ),
        (
            no_transfer,
            0,
            status::ACCESS_DENIED,
            "a dmabuf of a VMO without TRANSFER was installed",
        ),
        (
            read_only,
            DMABUF_WRITABLE,
            status::ACCESS_DENIED,
            "a writable dmabuf of a VMO without WRITE was installed",
        ),
    ] {
        if install(handle, COOKIE, flags) != Err(wanted) {
            return Err(what);
        }
        report.dmabuf_refusals += 1;
    }
    let first = install(vmo, COOKIE, DMABUF_TELL_MADE | DMABUF_WRITABLE)
        .map_err(|_| "a dmabuf of a whole anonymous VMO was refused")?;
    let second = install(vmo, COOKIE, DMABUF_TELL_MADE)
        .map_err(|_| "a second dmabuf install for a live cookie was refused")?;
    if first & DMABUF_MADE == 0 || second & DMABUF_MADE != 0 {
        return Err("a dmabuf install did not say rightly whether it made the object");
    }
    let first = (first & !DMABUF_MADE) as i32;
    let second = (second & !DMABUF_MADE) as i32;
    let objects: Vec<_> = [first, second]
        .iter()
        .filter_map(|fd| crate::syscall::fd::file(&program.process, *fd).ok())
        .filter_map(|file| super::dmabuf::of(file.io()))
        .collect();
    match objects.as_slice() {
        [one, two] if Arc::ptr_eq(one, two) => {}
        _ => return Err("two installs for one cookie did not give one dmabuf"),
    }
    drop(objects);
    if install(other, COOKIE, 0) != Err(status::ALREADY_BOUND) {
        return Err("a live cookie was installed over another VMO");
    }
    report.dmabuf_refusals += 1;
    let resolve = |fd: i32| {
        side.call(
            nr::CHARDEV_DMABUF_RESOLVE,
            &[reg(control), wire.id, fd as u64, BUFFER],
        )
    };
    let _ = resolve(second).map_err(|_| "a dmabuf this control made did not resolve")?;
    if side.get(BUFFER, 8)? != COOKIE.to_ne_bytes() {
        return Err("a dmabuf resolved to another cookie");
    }
    if resolve(i32::MAX - 1) != Err(status::BAD_HANDLE) {
        return Err("a descriptor that is no dmabuf resolved");
    }
    report.dmabuf_refusals += 1;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return after its dmabufs");
    }
    report.answered += 1;
    if install(vmo, COOKIE + 1, 0) != Err(status::BAD_STATE)
        || resolve(second) != Err(status::BAD_STATE)
    {
        return Err("a dmabuf call for an answered request was not refused");
    }
    report.dmabuf_refusals += 1;

    // One release, after the last of two descriptors and a mapping.
    let map = |fd: i32| map_dmabuf(&program.process, fd);
    let at = map(first)?;
    if read_mapped(&program.process, at)? != STAMP {
        return Err("a dmabuf's mapping did not show its VMO's bytes");
    }
    close_fd(&program.process, first)?;
    close_fd(&program.process, second)?;
    if released(side, control)?.is_some() {
        return Err("a dmabuf was released while a mapping of it lived");
    }
    let _ = crate::syscall::memory::sys_munmap(&program.process, at, DMABUF_BYTES)
        .map_err(|_| "unmapping a dmabuf failed")?;
    if released(side, control)? != Some(COOKIE) {
        return Err("a dmabuf's driver did not hear its release after its last mapping went");
    }
    if released(side, control)?.is_some() {
        return Err("a dmabuf's release was heard twice");
    }

    // One kept past the driver's death (B7), made for a request of its own.
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let fd = side
        .call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, reg(vmo), COOKIE + 2, 0],
        )
        .map_err(|_| "a dmabuf for a second cookie was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    let _ = finish()?;
    let at = map(fd)?;
    for handle in [vmo, other, no_transfer, read_only] {
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
    }
    if super::dmabuf::alive(core) != 1 {
        return Err("a control counted other than one live dmabuf");
    }
    Ok(Kept { fd, at })
}

/// The status of the fence `fd` of `process` names now.
fn fence_status(process: &Process, fd: i32) -> Result<Option<i32>, &'static str> {
    let file = crate::syscall::fd::file(process, fd).map_err(|_| "a fence's descriptor went")?;
    let fence = super::sync::of(file.io()).ok_or("a fence's descriptor is no sync_file")?;
    Ok(fence.status(timer::now_nanos()).map(|(code, _)| code))
}

/// Fences (N3b sync, the consultant's S9): each refusal refused with
/// nothing held, one signalled once, one past a short deadline read as
/// `ETIMEDOUT` and signalled by the thread, the deadlines clamped, and one
/// kept unsignalled for the driver's death.
fn fences(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<i32, &'static str> {
    use ferrix_native_abi::types::{SYNC_DEADLINE_DEFAULT_MS, SYNC_DEADLINE_MAX_MS};
    if super::sync::deadline_ms(0) != SYNC_DEADLINE_DEFAULT_MS
        || super::sync::deadline_ms(u64::MAX) != SYNC_DEADLINE_MAX_MS
        || super::sync::deadline_ms(3) != 3
    {
        return Err("a fence's deadline was not clamped as specified");
    }
    drain_channel(side, control);
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let install = |cookie: u64, ms: u64, flags: u64, unused: u64| {
        side.call(
            nr::CHARDEV_SYNC_INSTALL,
            &[reg(control), wire.id, cookie, ms, flags, unused],
        )
    };
    let signal = |cookie: u64, code: i64| {
        side.call(nr::CHARDEV_SYNC_SIGNAL, &[reg(control), cookie, code as u64])
    };
    let before = super::sync::alive(core);
    for (refused, wanted, what) in [
        (install(FENCE, 0, 1 << 5, 0), status::INVALID_ARGS, "a fence with an unknown flag was installed"),
        (install(FENCE, 0, 0, 1), status::INVALID_ARGS, "a fence with a stray register was installed"),
        (signal(FENCE, 1), status::INVALID_ARGS, "a fence was signalled with a positive status"),
        (signal(FENCE, -4096), status::INVALID_ARGS, "a fence was signalled past the errno range"),
        (signal(FENCE, 0), status::BAD_STATE, "a cookie with no fence was signalled"),
    ] {
        if refused != Err(wanted) {
            return Err(what);
        }
        report.sync_refusals += 1;
    }
    if super::sync::alive(core) != before {
        return Err("a refused fence call left a fence behind");
    }
    let once = install(FENCE, 0, 0, 0).map_err(|_| "a fence was refused")? as i32;
    if install(FENCE, 0, 0, 0) != Err(status::ALREADY_BOUND) {
        return Err("a live fence's cookie was installed again");
    }
    report.sync_refusals += 1;
    let resolve = |fd: i32| {
        side.call(nr::CHARDEV_SYNC_RESOLVE, &[reg(control), wire.id, fd as u64, BUFFER])
    };
    if resolve(once) != Ok(0) || side.get(BUFFER, 8)? != FENCE.to_ne_bytes() {
        return Err("an unsignalled fence did not resolve to its cookie");
    }
    if resolve(i32::MAX - 1) != Err(status::BAD_HANDLE) {
        return Err("a descriptor that is no fence resolved");
    }
    report.sync_refusals += 1;
    if fence_status(&program.process, once)?.is_some() {
        return Err("a fence read as signalled before it was");
    }
    let woken = fence_wakes(&program.process, once)?;
    if signal(FENCE, 0).is_err() || fence_status(&program.process, once)? != Some(0) {
        return Err("a fence was not signalled once with 0");
    }
    if fence_wakes(&program.process, once)? == woken {
        return Err("a fence's signal did not wake its queue");
    }
    file_info(program, once, report)?;
    if signal(FENCE, -5) != Err(status::BAD_STATE) || fence_status(&program.process, once)? != Some(0)
    {
        return Err("a fence was signalled twice");
    }
    report.sync_refusals += 1;
    if resolve(once) != Ok(1) {
        return Err("a signalled fence did not resolve as signalled");
    }
    // A short deadline: ETIMEDOUT, and the thread's signal takes the cookie
    // out of the table.
    let late = install(FENCE + 1, 1, 0, 0).map_err(|_| "a fence with a deadline was refused")? as i32;
    sched::sleep_for(STILL_NANOS);
    if fence_status(&program.process, late)? != Some(-(Errno::ETIMEDOUT.0 as i32)) {
        return Err("a fence past its deadline did not read as ETIMEDOUT");
    }
    if signal(FENCE + 1, 0) != Err(status::BAD_STATE) {
        return Err("a fence past its deadline took the driver's signal");
    }
    report.sync_refusals += 1;
    let kept = install(FENCE + 2, u64::MAX, 0, 0).map_err(|_| "a third fence was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return after its fences");
    }
    report.answered += 1;
    if install(FENCE + 3, 0, 0, 0) != Err(status::BAD_STATE) {
        return Err("a fence for an answered request was installed");
    }
    report.sync_refusals += 1;
    close_fd(&program.process, once)?;
    close_fd(&program.process, late)?;
    if super::sync::alive(core) != 1 {
        return Err("a control counted other than one unsignalled fence");
    }
    Ok(kept)
}

/// How often the fence `fd` of `process` names has woken its queue.
fn fence_wakes(process: &Process, fd: i32) -> Result<Option<u64>, &'static str> {
    use ferrix_vfs::Inode;
    let file = crate::syscall::fd::file(process, fd).map_err(|_| "a fence's descriptor went")?;
    let fence = super::sync::of(file.io()).ok_or("a fence's descriptor is no sync_file")?;
    Ok(fence.poll_changes())
}

/// `SYNC_IOC_FILE_INFO` through the program's memory, as Linux answers it
/// for one signalled fence, and its refusals (S6).
fn file_info(program: &Side, fd: i32, report: &mut Report) -> Result<(), &'static str> {
    const FILE_INFO: u32 = 0xC038_3E04;
    const MERGE: u32 = 0xC030_3E03;
    let file = crate::syscall::fd::file(&program.process, fd).map_err(|_| "a fence's descriptor went")?;
    let fence = super::sync::of(file.io()).ok_or("a fence's descriptor is no sync_file")?;
    let ask = |info: &[u8; 56]| -> Result<Result<usize, Errno>, &'static str> {
        program.put(BUFFER, info)?;
        Ok(super::sync::ioctl(&program.process, &fence, FILE_INFO, BUFFER))
    };
    let word = |bytes: &[u8], at: usize| -> u32 {
        let mut word = [0u8; 4];
        word.copy_from_slice(bytes.get(at..at + 4).unwrap_or(&[0; 4]));
        u32::from_ne_bytes(word)
    };
    // num_fences 0: the count, no array.
    if ask(&[0u8; 56])? != Ok(0) {
        return Err("SYNC_IOC_FILE_INFO with no array was refused");
    }
    let got = program.get(BUFFER, 56)?;
    if word(&got, 32) != 1 || word(&got, 40) != 1 || !got.starts_with(b"nvidia-drm") {
        return Err("SYNC_IOC_FILE_INFO did not say one signalled fence");
    }
    // num_fences 1: one entry, at an array in the program's memory.
    let mut info = [0u8; 56];
    info[40..44].copy_from_slice(&1u32.to_ne_bytes());
    info[48..56].copy_from_slice(&(BUFFER + 0x100).to_ne_bytes());
    if ask(&info)? != Ok(0) || word(&program.get(BUFFER + 0x100, 80)?, 64) != 1 {
        return Err("SYNC_IOC_FILE_INFO did not write its fence's entry");
    }
    let mut flagged = [0u8; 56];
    flagged[36] = 1;
    let mut stray = info;
    stray[48..56].copy_from_slice(&u64::MAX.to_ne_bytes());
    for (refused, wanted, what) in [
        (ask(&flagged)?, Errno::EINVAL, "SYNC_IOC_FILE_INFO with flags set was answered"),
        (ask(&stray)?, Errno::EFAULT, "SYNC_IOC_FILE_INFO wrote through a bad pointer"),
        (
            super::sync::ioctl(&program.process, &fence, MERGE, BUFFER),
            Errno::ENOTTY,
            "SYNC_IOC_MERGE was answered",
        ),
    ] {
        if refused != Err(wanted) {
            return Err(what);
        }
        report.sync_refusals += 1;
    }
    let mut byte = [0u8; 1];
    if ferrix_vfs::Inode::read_at(&*fence, 0, &mut byte) != Err(Errno::EINVAL)
        || ferrix_vfs::Inode::mapping_at(&*fence, 0).is_some()
    {
        return Err("a fence was read or mapped");
    }
    report.sync_refusals += 1;
    Ok(())
}

/// After its driver went, the fence it never signalled reads `ENODEV`.
fn fence_outlived(program: &Side, fd: i32) -> Result<(), &'static str> {
    if fence_status(&program.process, fd)? != Some(-(Errno::ENODEV.0 as i32)) {
        return Err("a fence its driver never signalled did not read ENODEV after it went");
    }
    close_fd(&program.process, fd)
}

/// After its driver went, a dmabuf's mapping still shows its bytes, a new
/// mapping still works, and closing it all is heard by nobody (B7).
fn outlived(program: &Side, kept: Kept) -> Result<(), &'static str> {
    if read_mapped(&program.process, kept.at)? != STAMP {
        return Err("a dmabuf's mapping lost its bytes when its driver went");
    }
    let again = map_dmabuf(&program.process, kept.fd)?;
    if read_mapped(&program.process, again)? != STAMP {
        return Err("a dmabuf could not be mapped again after its driver went");
    }
    for at in [kept.at, again] {
        let _ = crate::syscall::memory::sys_munmap(&program.process, at, DMABUF_BYTES)
            .map_err(|_| "unmapping a dmabuf failed")?;
    }
    close_fd(&program.process, kept.fd)
}

/// The cookie the check names its name-only dmabuf by, and the one it keeps
/// past the driver's death.
const NAME_COOKIE: u64 = 0x000E_D3AB;
/// The size the driver says its name-only dmabuf has: more than the check's
/// VMO, so nothing of that size could be mapped by mistake.
const NAME_BYTES: u64 = 16 * 4096;

/// The name-only dmabuf (ledger 632, N1-N6): its refusals, one object a
/// cookie, no mapping, resolve by its maker only, one release after the
/// last descriptor. Answers with a descriptor kept for [`name_outlived`].
fn names(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    foreign: Option<(&Side, Handle, &Arc<Control>)>,
    report: &mut Report,
) -> Result<i32, &'static str> {
    use ferrix_native_abi::types::{
        DMABUF_MADE, DMABUF_NAME_ONLY, DMABUF_TELL_MADE, DMABUF_WRITABLE,
    };
    drain_channel(side, control);
    let vmo = side.handle(nr::VMO_CREATE, &[DMABUF_BYTES], "vmo_create failed")?;
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let install = |vmo_register: u64, cookie: u64, flags: u64, size: u64| {
        side.call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, vmo_register, cookie, flags, size],
        )
    };
    let name = DMABUF_NAME_ONLY;
    register_refusals(&install, vmo, report)?;
    if super::dmabuf::alive(core) != 1 {
        return Err("a refused name-only install left a dmabuf behind");
    }
    let first = install(
        0,
        NAME_COOKIE,
        name | DMABUF_TELL_MADE | DMABUF_WRITABLE,
        NAME_BYTES,
    )
    .map_err(|_| "a name-only dmabuf was refused")?;
    let second = install(0, NAME_COOKIE, name | DMABUF_TELL_MADE, NAME_BYTES)
        .map_err(|_| "a second name-only install for a live cookie was refused")?;
    if first & DMABUF_MADE == 0 || second & DMABUF_MADE != 0 {
        return Err("a name-only install did not say rightly whether it made the object");
    }
    let first = (first & !DMABUF_MADE) as i32;
    let second = (second & !DMABUF_MADE) as i32;
    one_object(&program.process, first, second)?;
    kind_refusals(&install, vmo, report)?;
    let _ = side
        .call(
            nr::CHARDEV_DMABUF_RESOLVE,
            &[reg(control), wire.id, second as u64, BUFFER],
        )
        .map_err(|_| "a name-only dmabuf its control made did not resolve")?;
    if side.get(BUFFER, 8)? != NAME_COOKIE.to_ne_bytes() {
        return Err("a name-only dmabuf resolved to another cookie");
    }
    let kept = install(0, NAME_COOKIE + 1, name, NAME_BYTES)
        .map_err(|_| "a name-only dmabuf for a second cookie was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return after its name-only dmabufs");
    }
    report.answered += 1;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(vmo)]);

    unmappable(&program.process, first, report)?;

    if let Some(foreign) = foreign {
        foreign_refused(program, foreign, second, report)?;
    }

    // One release, after the last of two descriptors.
    close_fd(&program.process, first)?;
    if released(side, control)?.is_some() {
        return Err("a name-only dmabuf was released while a descriptor of it lived");
    }
    close_fd(&program.process, second)?;
    if released(side, control)? != Some(NAME_COOKIE) {
        return Err(
            "a name-only dmabuf's driver did not hear its release after its last descriptor",
        );
    }
    if released(side, control)?.is_some() {
        return Err("a name-only dmabuf's release was heard twice");
    }
    if super::dmabuf::alive(core) != 2 {
        return Err("a control counted other than two live dmabufs");
    }
    Ok(kept)
}

/// A name-only install's arguments: VMO register, cookie, flags, size.
type Install<'a> = &'a dyn Fn(u64, u64, u64, u64) -> Result<usize, Errno>;

/// The register refusals (N1): each `INVALID_ARGS`.
fn register_refusals(
    install: Install<'_>,
    vmo: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    use ferrix_native_abi::types::{DMABUF_NAME_MAX, DMABUF_NAME_ONLY};
    let name = DMABUF_NAME_ONLY;
    for (vmo_register, flags, size, what) in [
        (0, name, 0, "a name-only dmabuf of no bytes was installed"),
        (
            0,
            name,
            4096 + 1,
            "a name-only dmabuf of no whole pages was installed",
        ),
        (
            0,
            name,
            DMABUF_NAME_MAX + 4096,
            "a name-only dmabuf over the bound was installed",
        ),
        (
            reg(vmo),
            name,
            NAME_BYTES,
            "a name-only dmabuf naming a VMO was installed",
        ),
        (
            reg(vmo),
            0,
            NAME_BYTES,
            "a dmabuf of a VMO with a size was installed",
        ),
    ] {
        if install(vmo_register, NAME_COOKIE, flags, size) != Err(status::INVALID_ARGS) {
            return Err(what);
        }
        report.name_refusals += 1;
    }
    Ok(())
}

/// A live cookie asked in the other kind or with another size:
/// `ALREADY_BOUND` (N2).
fn kind_refusals(
    install: Install<'_>,
    vmo: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    use ferrix_native_abi::types::DMABUF_NAME_ONLY;
    let name = DMABUF_NAME_ONLY;
    for (vmo_register, cookie, flags, size, what) in [
        (
            0,
            NAME_COOKIE,
            name,
            NAME_BYTES * 2,
            "a live name was installed with another size",
        ),
        (
            reg(vmo),
            NAME_COOKIE,
            0,
            0,
            "a live name was installed over a VMO",
        ),
        (
            0,
            COOKIE + 2,
            name,
            DMABUF_BYTES,
            "a live VMO cookie was installed as a name",
        ),
    ] {
        if install(vmo_register, cookie, flags, size) != Err(status::ALREADY_BOUND) {
            return Err(what);
        }
        report.name_refusals += 1;
    }
    Ok(())
}

/// Two installs for one name gave one object.
fn one_object(process: &Process, first: i32, second: i32) -> Result<(), &'static str> {
    let objects: Vec<_> = [first, second]
        .iter()
        .filter_map(|fd| crate::syscall::fd::file(process, *fd).ok())
        .filter_map(|file| super::dmabuf::of(file.io()))
        .collect();
    match objects.as_slice() {
        [one, two] if Arc::ptr_eq(one, two) => {}
        _ => return Err("two name-only installs for one cookie did not give one dmabuf"),
    }
    drop(objects);
    Ok(())
}

/// Another driver's resolve of a name-only dmabuf it did not make is
/// `BAD_HANDLE` (B3), for a request of its own.
fn foreign_refused(
    program: &Side,
    (other_side, other_control, other_core): (&Side, Handle, &Arc<Control>),
    second: i32,
    report: &mut Report,
) -> Result<(), &'static str> {
    drain_channel(other_side, other_control);
    let request = admit(other_core, &program.process, IOCTL)
        .map_err(|_| "an ioctl was not taken in by the second driver")?;
    start(Job::Await(
        Arc::clone(other_core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(other_side, other_control)? else {
        return Err("an ioctl did not reach the second driver as a REQUEST");
    };
    let resolved = other_side.call(
        nr::CHARDEV_DMABUF_RESOLVE,
        &[reg(other_control), wire.id, second as u64, BUFFER],
    );
    reply(other_side, other_control, wire.id, VALUE)?;
    let _ = finish()?;
    if resolved != Err(status::BAD_HANDLE) {
        return Err("another driver resolved a name-only dmabuf it did not make");
    }
    report.name_refusals += 1;
    Ok(())
}

/// A name-only dmabuf says its size and maps nothing, shared or private
/// (N5), through the program's own `mmap`.
fn unmappable(process: &Process, fd: i32, report: &mut Report) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{MAP_PRIVATE, MAP_SHARED};
    use ferrix_vfs::Inode;
    let size = crate::syscall::fd::file(process, fd)
        .ok()
        .and_then(|file| super::dmabuf::of(file.io()))
        .map(|dmabuf| dmabuf.metadata().size);
    if size != Some(NAME_BYTES) {
        return Err("a name-only dmabuf did not say the size its driver gave");
    }
    for flags in [MAP_SHARED, MAP_PRIVATE] {
        match mmap_errno(process, fd, flags) {
            Err(Errno::ENODEV) => report.name_refusals += 1,
            Ok(at) => {
                let _ = crate::syscall::memory::sys_munmap(process, at, DMABUF_BYTES);
                return Err("a name-only dmabuf was mapped");
            }
            Err(_) => return Err("a name-only dmabuf's mapping was refused other than ENODEV"),
        }
    }
    Ok(())
}

/// After its driver went, a name-only dmabuf still says its size, still
/// maps nothing, and closes with nobody to hear it.
fn name_outlived(program: &Side, fd: i32, report: &mut Report) -> Result<(), &'static str> {
    unmappable(&program.process, fd, report)?;
    close_fd(&program.process, fd)
}

/// `mmap` of the dmabuf `fd` of `process`, read-only, with `flags`.
fn mmap_errno(process: &Process, fd: i32, flags: u32) -> Result<u64, Errno> {
    use crate::syscall::memory::{MmapRequest, OffsetUnit};
    use ferrix_linux_abi::types::PROT_READ;
    crate::syscall::memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: DMABUF_BYTES,
            prot: PROT_READ,
            flags,
            fd: i64::from(fd),
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map(|at| at as u64)
}

/// Map the whole dmabuf `fd` of `process`, shared and read-only.
fn map_dmabuf(process: &Process, fd: i32) -> Result<u64, &'static str> {
    use crate::syscall::memory::{MmapRequest, OffsetUnit};
    use ferrix_linux_abi::types::{MAP_SHARED, PROT_READ};
    crate::syscall::memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: DMABUF_BYTES,
            prot: PROT_READ,
            flags: MAP_SHARED,
            fd: i64::from(fd),
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map(|at| at as u64)
    .map_err(|_| "a dmabuf could not be mapped")
}

/// The first eight bytes at `at` in `process`.
fn read_mapped(process: &Process, at: u64) -> Result<[u8; 8], &'static str> {
    let mut bytes = [0u8; 8];
    crate::syscall::uaccess::copy_from_user(process.space(), at, &mut bytes)
        .map_err(|_| "a dmabuf's mapping could not be read")?;
    Ok(bytes)
}

/// Close descriptor `fd` of `process`.
fn close_fd(process: &Process, fd: i32) -> Result<(), &'static str> {
    crate::syscall::fd::sys_close(process, fd)
        .map(|_| ())
        .map_err(|_| "closing a dmabuf's descriptor failed")
}

/// The cookie of the next `DmabufRelease` on `control`, if one comes within
/// a short while; any other message is an error.
fn released(side: &Side, control: Handle) -> Result<Option<u64>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(STILL_NANOS * 4);
    loop {
        if let Some(bytes) = read(side, control) {
            return match Message::decode(&bytes) {
                Ok(Message::Request(wire)) if wire.op == Op::DmabufRelease => Ok(Some(wire.arg)),
                _ => Err("the chardev core wrote something other than a dmabuf's release"),
            };
        }
        if timer::now_nanos() > deadline {
            return Ok(None);
        }
        sched::sleep_for(1_000_000);
    }
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
