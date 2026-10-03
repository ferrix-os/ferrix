//! The native system call handlers.
//!
//! Stage 9 of `docs/ROADMAP.md`. [`super::dispatch`] sends every number in
//! `0x1000..=0x1FFF` here before it asks any Linux table, so the two ABIs
//! never have to agree about a number. Each handler takes the core's
//! [`Process`], for the reason `syscall::process` gives: the boot self-check
//! drives them against processes it built, long before a program can make a
//! native call.
//!
//! # What is decided where
//!
//! Which handles are valid and what rights they carry is `src/lib/kernel/objects` and
//! `src/lib/proto/native-abi`. What an object does is [`crate::object`]. This file does
//! what only a system call layer can: turn registers and user pointers into
//! those calls, and their failures into the status a program sees.
//!
//! # What is answered above the item
//!
//! Most calls act on the core's objects and are answered here, by a `match`
//! the compiler holds exhaustive. Five make a device's control channel for a
//! subsystem in the load ring -- the block and network rings, the display, the
//! renderer and input -- one, `job_for_cgroup`, reads a cgroupfs directory,
//! one, `process_give`, needs to know whose child a process is, and one,
//! `port_fd`, makes a file descriptor. Those are answered by whatever
//! registered for them with
//! [`serve`], from `main.rs`'s `register_load`, and the boot stops (FX-0006)
//! if any of them has nothing registered ([`unserved`]): the exhaustiveness
//! the `match` gave them at compile time is kept, at boot, on every boot.
//!
//! Two more things come from above. A native process is made and started by
//! the personality whose process it is ([`Processes`]), and a quiesce waits
//! out every subsystem that serves a device through a channel ([`Server`]).
//! So this file names no module above the item, and the native ABI can be
//! read without the load ring behind it (`docs/certification/FINDINGS.md`,
//! F-07).
//!
//! # Objects are dropped outside the handle lock
//!
//! Every handler takes what it needs out of the table — an `Arc` of the
//! object, or objects by value — and lets the table's lock go before it acts
//! on them or drops them. An object's drop can free frames and drain a queue
//! of other objects, and none of that belongs under the lock another thread of
//! the same process needs to look a handle up. [`object::dispose`] is where
//! the dropping happens.
//!
//! # What is not here yet
//!
//! The calls that act on a process beyond making, starting, giving it its
//! bootstrap and reading how it ended, in `0x1035..=0x1037`. Those numbers do
//! not decode yet, and answer `ENOSYS`.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr::{self, NativeCall};
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    self, APERTURE_INFO_BYTES, ApertureInfo, CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES,
    DEVICE_INFO_BYTES, DeviceInfo, JOB_SPECULATION_DOMAIN, MAP_READ, MAP_WRITE, PortPacket,
    ProcessStatus, ReadActual,
};
use ferrix_objects::message::Message;
use ferrix_objects::reach::Reach;
use ferrix_objects::table::TableError;
use ferrix_sync::Once;
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::audit;
use crate::claim::StillServed;
use crate::device::{ConfigRefusal, DeviceNode};
use crate::fallible;
use crate::hooks::{Full, Hooks};
use crate::object::channel::{self, ChannelMessage, Endpoint, ReadError, WriteFailure};
use crate::object::interrupt::{Interrupt, InterruptError};
use crate::object::io_mapping::{IoMapping, IoMappingError};
use crate::object::job::{self, Job};
use crate::object::port::{Observer, Port, PortError};
use crate::object::process::{Bootstrap, GiveRefused, Host, Process, ProcessRef};
use crate::object::quota::{self, Resource, Usage};
use crate::object::{self, HandleTable, Object};
use crate::syscall::uaccess::{self, UserError};
use crate::trap::SyscallArgs;
use crate::user::space::SpaceError;
use crate::user::vmo::{Vmo, VmoError};

/// The largest VMO a program may create, in pages: four gibibytes.
///
/// A reservation costs nothing until it is written, so this bounds what one
/// call can promise rather than what it costs. It is also what a 32-bit
/// caller's size register can express, so the limit is the same everywhere.
const MAX_VMO_PAGES: u64 = 1 << 20;

/// A handle's width in a user buffer.
const HANDLE_BYTES: usize = size_of::<u32>();

/// A pointer and a count, as two registers give them.
#[derive(Debug, Clone, Copy)]
struct Buffer {
    /// Where it starts in the caller's memory.
    at: u64,
    /// How many elements: bytes, or handles.
    count: u64,
}

/// The buffer at `at` of `count` elements, from two registers.
const fn buffer(at: u64, count: u64) -> Buffer {
    Buffer { at, count }
}

/// The call a native number names, or `None` for one outside the range or in
/// a gap.
///
/// Clamped into the range before the table is asked, for the reason
/// `arch::decode_syscall` gives: the table is a `match` the compiler makes a
/// jump table of, and the number is the program's (Spectre variant 1).
fn decode(number: usize) -> Option<NativeCall> {
    let offset = arch::nospec_index(number.wrapping_sub(nr::FIRST), nr::LAST - nr::FIRST + 1)?;
    nr::decode(nr::FIRST + offset)
}

/// A native call answered above the item, by what registered for it with
/// [`serve`].
///
/// Handed the caller as the personality's process, not the core's half of
/// it, so that a handler which needs more -- `job_for_cgroup` reads the
/// caller's descriptors and credentials -- can have its own type back
/// ([`object::process::downcast`]).
pub(crate) type Handler = fn(&dyn Host, &[u64; 6]) -> Result<usize, Errno>;

/// One call the item leaves to the load ring, and what answers it.
struct Served {
    /// The call.
    call: NativeCall,
    /// Its handler, once registered.
    handler: Once<Handler>,
}

impl Served {
    /// `call`, answered by nothing yet.
    const fn new(call: NativeCall) -> Served {
        Served {
            call,
            handler: Once::new(),
        }
    }
}

/// The calls answered above the item, each registered once at bring-up.
///
/// A short list searched by call rather than an array indexed by number: the
/// search is over a decoded [`NativeCall`], never over a number a program
/// chose, so no bound here can be mispredicted into memory past it; and each
/// of these calls makes a channel for a driver, not the path a driver's work
/// takes. A `Once` per call, as [`crate::hooks`] keeps them: written at
/// bring-up, read without a lock.
static SERVED: [Served; 15] = [
    Served::new(NativeCall::BlockRingCreate),
    Served::new(NativeCall::NetRingCreate),
    Served::new(NativeCall::DisplayControlCreate),
    Served::new(NativeCall::RenderControlCreate),
    Served::new(NativeCall::InputControlCreate),
    Served::new(NativeCall::SoundControlCreate),
    Served::new(NativeCall::LogControlCreate),
    Served::new(NativeCall::JobForCgroup),
    Served::new(NativeCall::ProcessGive),
    Served::new(NativeCall::PortFd),
    Served::new(NativeCall::ChardevControlCreate),
    Served::new(NativeCall::ChardevReply),
    Served::new(NativeCall::ChardevCopyIn),
    Served::new(NativeCall::ChardevCopyOut),
    Served::new(NativeCall::ChardevFile),
];

/// Answer `call` with `handler`. Called from `main.rs`'s `register_load`, by
/// the subsystem the call is about.
///
/// # Errors
///
/// [`Full`] when `call` is not one this table leaves to the load ring, or
/// already has a handler: either way the load registers more than the item
/// expects, and the boot says so.
pub(crate) fn serve(call: NativeCall, handler: Handler) -> Result<(), Full> {
    let served = SERVED
        .iter()
        .find(|served| served.call == call)
        .ok_or(Full)?;
    let mut taken = false;
    let _ = served.handler.call_once(|| {
        taken = true;
        handler
    });
    if taken { Ok(()) } else { Err(Full) }
}

/// The first call left to the load ring that nothing answers: what the boot
/// checks is `None` before it goes on, so that a call the `match` in
/// [`dispatch`] no longer answers itself cannot go unanswered unseen.
pub(crate) fn unserved() -> Option<NativeCall> {
    SERVED
        .iter()
        .find(|served| served.handler.get().is_none())
        .map(|served| served.call)
}

/// How many calls the load ring answers, for the boot's report.
pub(crate) fn served_count() -> usize {
    SERVED
        .iter()
        .filter(|served| served.handler.get().is_some())
        .count()
}

/// Answer `call` through the table.
fn served(call: NativeCall, caller: &dyn Host, a: &[u64; 6]) -> Result<usize, Errno> {
    let handler = SERVED
        .iter()
        .find(|served| served.call == call)
        .and_then(|served| served.handler.get())
        .ok_or(Errno::ENOSYS)?;
    handler(caller, a)
}

/// How a native process is made and started: what `process_create`,
/// `process_start` and `devmgr`'s own start need of the personality whose
/// process it is.
///
/// A native process is loaded from an ELF image into a process of the Linux
/// personality's -- it has descriptors and a thread like any other -- and
/// that loader and that process are above the item. So the item states what
/// it needs and the personality registers it, as it lends `init` its
/// `Launcher`.
pub(crate) struct Processes {
    /// A new process with the ELF `image` loaded, named `name`, made and not
    /// started. Refused with the native status the caller hears.
    pub(crate) load: LoadNative,
    /// Claim the process's start and make its first task; then ask the
    /// argument for the value its first argument register starts with, and
    /// run it. A refused argument gives the start back with nothing run.
    pub(crate) start: StartNative,
    /// Whether the calling thread of `caller` is to leave a wait rather than
    /// sleep on: its process is ending, or another of its threads is
    /// replacing the program.
    pub(crate) must_leave: fn(caller: &dyn Host) -> bool,
}

/// [`Processes::load`]: the process asking, an image and a name, and the
/// process made from them.
///
/// The new process runs as `creator` does, as a fork child runs as its
/// parent: making a process is not a way to become anyone else
/// (`docs/AUTH.md` §7, P0). With no creator -- one the kernel makes for
/// itself, as `devmgr` -- it is root's.
pub(crate) type LoadNative =
    fn(creator: Option<&dyn Host>, image: &[u8], name: &[u8]) -> Result<Arc<dyn Host>, Errno>;

/// What gives a starting process its first argument, or refuses to.
pub(crate) type Argument<'a> = &'a mut dyn FnMut() -> Result<u64, Errno>;

/// [`Processes::start`].
pub(crate) type StartNative =
    fn(process: &Arc<dyn Host>, argument: Argument<'_>) -> Result<(), StartRefused>;

/// Why [`Processes::start`] did not start a process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StartRefused {
    /// It has no program loaded, has already ended, or was already started.
    Claimed,
    /// No task could be made for it.
    NoTask,
    /// The argument could not be given; its status.
    Argument(Errno),
}

/// The registered [`Processes`].
static PROCESSES: Once<&'static Processes> = Once::new();

/// Make and start native processes with `processes`. The first registration
/// stands.
pub(crate) fn register_processes(processes: &'static Processes) {
    let _ = PROCESSES.call_once(|| processes);
}

/// The registered [`Processes`], if one is: what `devmgr` starts with.
pub(crate) fn processes() -> Option<&'static Processes> {
    PROCESSES.get().copied()
}

/// Whether the calling thread of `caller` is to leave its wait: as the
/// personality says, or, with none registered, once its process has ended.
fn must_leave(caller: &dyn Host) -> bool {
    match processes() {
        Some(processes) => (processes.must_leave)(caller),
        None => caller.core().is_terminated(),
    }
}

/// A subsystem that serves devices through a control channel, as a quiesce
/// sees it.
pub(crate) struct Server {
    /// Wait, bounded, until nothing of it serves `node` any more, or until
    /// `cancelled`; refused while a live driver still does.
    pub(crate) wait_until_unserved: WaitUnserved,
    /// Let go of a claim it keeps on `node` past its driver's end, once the
    /// device reaches nothing: the block ring's, which a driver started again
    /// would otherwise be refused by.
    pub(crate) release: Option<fn(node: &Arc<DeviceNode>)>,
}

/// [`Server::wait_until_unserved`].
pub(crate) type WaitUnserved =
    fn(node: &Arc<DeviceNode>, cancelled: &dyn Fn() -> bool) -> Result<(), StillServed>;

/// What a quiesce waits out, in the order it was registered: the block and
/// net rings, the display, the renderer, input and audio, with room for two
/// more.
static SERVERS: Hooks<Server, 8> = Hooks::new();

/// Have a quiesce wait out `server`.
///
/// # Errors
///
/// [`Full`] past eight, which is more subsystems serving devices than the
/// item expects.
pub(crate) fn register_server(server: &'static Server) -> Result<(), Full> {
    SERVERS.register(server)
}

/// How many subsystems a quiesce waits out, for the boot's report.
pub(crate) fn server_count() -> usize {
    SERVERS.len()
}

/// Answer one native system call for `caller`, the calling thread's process.
///
/// # Errors
///
/// `ENOSYS` for a gap in the range, a call not built yet, or one left to the
/// load ring that nothing answers; `ESRCH` with no process, as the Linux half
/// answers; otherwise the call's own status.
pub(crate) fn dispatch(args: &SyscallArgs, caller: Option<&dyn Host>) -> Result<usize, Errno> {
    let call = decode(args.number).ok_or(Errno::ENOSYS)?;
    let caller = caller.ok_or(Errno::ESRCH)?;
    let answered = answer(call, caller, args.args);
    record_call(call, args.number, caller.core(), &args.args, &answered);
    answered
}

/// The audit record of a call's answer, where it is one (finding F-21b,
/// `docs/certification/AUDIT.md` §1): every native call a handle's rights
/// refused -- here, where each call's answer passes, rather than at each of
/// the refusals -- and each handle given that carries authority, a job for
/// a cgroup or a device's control channel. Made with no lock held.
fn record_call(
    call: NativeCall,
    number: usize,
    process: &Process,
    a: &[u64; 6],
    answered: &Result<usize, Errno>,
) {
    let number = u32::try_from(number).unwrap_or(u32::MAX);
    let named = |id: u64| audit::Target {
        kind: audit::target::HANDLE,
        id,
    };
    let (event, target, detail) = match (call, answered) {
        (NativeCall::HandleDuplicate | NativeCall::HandleReplace, Err(status::ACCESS_DENIED)) => {
            let asked = a.get(1).copied().unwrap_or(0);
            (audit::WIDEN, named(a[0]), [number, asked as u32, 0])
        }
        // Audited by the call itself, with the device and the values.
        (NativeCall::DeviceSetLimit, _) => return,
        (_, Err(status::ACCESS_DENIED)) => (audit::RIGHTS, named(a[0]), [number, 0, 0]),
        (NativeCall::JobForCgroup, Ok(given)) => {
            (audit::DELEGATED, named(*given as u64), [number, 0, 0])
        }
        (
            NativeCall::BlockRingCreate
            | NativeCall::NetRingCreate
            | NativeCall::DisplayControlCreate
            | NativeCall::RenderControlCreate
            | NativeCall::InputControlCreate
            | NativeCall::SoundControlCreate
            | NativeCall::LogControlCreate
            | NativeCall::ChardevControlCreate,
            Ok(given),
        ) => (audit::CONTROL, named(*given as u64), [number, 0, 0]),
        _ => return,
    };
    let (outcome, status) = match answered {
        Err(refused) => (
            audit::Outcome::Refused,
            i16::try_from(refused.0).unwrap_or(i16::MAX),
        ),
        Ok(_) => (audit::Outcome::Done, 0),
    };
    audit::record(
        event,
        outcome,
        status,
        audit::Subject::of(process),
        target,
        detail,
    );
}

/// Answer `call` for `caller`: see [`dispatch`].
fn answer(call: NativeCall, caller: &dyn Host, a: [u64; 6]) -> Result<usize, Errno> {
    let process = caller.core();
    match call {
        NativeCall::HandleClose => handle_close(process, handle(a[0])),
        NativeCall::HandleDuplicate => handle_duplicate(process, handle(a[0]), a[1]),
        NativeCall::HandleReplace => handle_replace(process, handle(a[0]), a[1]),
        NativeCall::ChannelCreate => channel_create(process, a[0]),
        NativeCall::ChannelWrite => channel_write(
            process,
            handle(a[0]),
            buffer(a[1], a[2]),
            buffer(a[3], a[4]),
        ),
        NativeCall::ChannelRead => channel_read(
            process,
            handle(a[0]),
            buffer(a[1], a[2]),
            buffer(a[3], a[4]),
            a[5],
        ),
        NativeCall::VmoCreate => vmo_create(process, a[0]),
        NativeCall::VmoRead => vmo_read(process, handle(a[0]), buffer(a[1], a[2]), a[3]),
        NativeCall::VmoWrite => vmo_write(process, handle(a[0]), buffer(a[1], a[2]), a[3]),
        NativeCall::VmoGetSize => vmo_get_size(process, handle(a[0]), a[1]),
        NativeCall::ObjectWaitOne => object_wait_one(caller, handle(a[0]), a[1], a[2], a[3]),
        // A program makes this call through `dispatch_write_read`, which
        // hands the words back; a boot check here gets the count alone.
        NativeCall::ChannelWriteRead => channel_write_read(caller, &a).map(|(count, _)| count),
        NativeCall::JobCreate
        | NativeCall::JobKill
        | NativeCall::JobSetLimit
        | NativeCall::JobGetQuota
        | NativeCall::ProcessCreate
        | NativeCall::ProcessStart => job_call(call, caller, &a),
        NativeCall::InterruptCreate => interrupt_create(process, handle(a[0]), a[1]),
        NativeCall::InterruptAck => interrupt_ack(process, handle(a[0])),
        NativeCall::InterruptBind => interrupt_bind(process, handle(a[0]), handle(a[1]), a[2]),
        NativeCall::BlockRingCreate
        | NativeCall::NetRingCreate
        | NativeCall::DisplayControlCreate
        | NativeCall::RenderControlCreate
        | NativeCall::InputControlCreate
        | NativeCall::SoundControlCreate
        | NativeCall::LogControlCreate
        | NativeCall::JobForCgroup
        | NativeCall::ProcessGive
        | NativeCall::PortFd
        | NativeCall::ChardevControlCreate
        | NativeCall::ChardevReply
        | NativeCall::ChardevCopyIn
        | NativeCall::ChardevCopyOut
        | NativeCall::ChardevFile => served(call, caller, &a),
        NativeCall::DevmgrStart => crate::discovery::devmgr::devmgr_start(caller, &a),
        NativeCall::AuditRead => audit_read(process, handle(a[0]), a[1], a[2], a[3], a[4]),
        NativeCall::ProcessBootstrap => process_bootstrap(process),
        NativeCall::ProcessStatus => process_status(process, handle(a[0]), a[1]),
        NativeCall::IoMappingCreate
        | NativeCall::IoMappingMap
        | NativeCall::IoMappingMapCombining
        | NativeCall::DeviceInfo
        | NativeCall::DeviceAperture
        | NativeCall::DeviceConfigRead
        | NativeCall::DeviceConfigWrite
        | NativeCall::DeviceQuiesce
        | NativeCall::DeviceClock
        | NativeCall::DeviceSetLimit
        | NativeCall::DeviceGetLimit
        | NativeCall::DeviceIsolation => device_call(call, process, &a),
        NativeCall::VmoPin => vmo_pin(process, handle(a[0]), handle(a[1]), a[2], a[3], a[4]),
        NativeCall::VmoPinAddresses => vmo_pin_addresses(process, handle(a[0]), a[1], a[2]),
        NativeCall::PortCreate => port_create(process),
        NativeCall::PortQueue => port_queue(process, handle(a[0]), a[1]),
        NativeCall::PortWait => port_wait(caller, handle(a[0]), a[1], a[2]),
        NativeCall::ObjectWaitAsync => {
            object_wait_async(process, handle(a[0]), handle(a[1]), a[2], a[3])
        }
        NativeCall::VmoMap => vmo_map(process, handle(a[0]), a[1], a[2], a[3], a[4]),
    }
}

/// A handle from a register.
fn handle(register: u64) -> Handle {
    Handle::from_register(register)
}

/// A new handle as a return value.
///
/// Positive on every architecture: `src/lib/kernel/objects` keeps every handle value
/// below 2^31, so a 32-bit return register never reads one as an error.
fn returned(handle: Handle) -> usize {
    handle.0 as usize
}

/// The status a table refusal travels as.
fn table_error(error: TableError) -> Errno {
    match error {
        TableError::BadHandle => status::BAD_HANDLE,
        TableError::AccessDenied => status::ACCESS_DENIED,
        TableError::Full => status::NO_HANDLES,
        TableError::Repeated => status::INVALID_ARGS,
        TableError::NoMemory => status::NO_MEMORY,
    }
}

/// Every way a user copy fails is `EFAULT` to the program.
fn fault(_: UserError) -> Errno {
    status::FAULT
}

/// `handle_close`.
fn handle_close(process: &Process, handle: Handle) -> Result<usize, Errno> {
    let (object, _) = process
        .with_handles(|table| table.remove(handle))
        .map_err(table_error)?;
    object::dispose([object]);
    Ok(0)
}

/// `handle_duplicate`.
fn handle_duplicate(process: &Process, handle: Handle, rights: u64) -> Result<usize, Errno> {
    let requested = Requested::from_register(rights).ok_or(status::INVALID_ARGS)?;
    process
        .with_handles(|table| table.duplicate(handle, requested))
        .map(returned)
        .map_err(table_error)
}

/// `handle_replace`.
fn handle_replace(process: &Process, handle: Handle, rights: u64) -> Result<usize, Errno> {
    let requested = Requested::from_register(rights).ok_or(status::INVALID_ARGS)?;
    process
        .with_handles(|table| table.replace(handle, requested))
        .map(returned)
        .map_err(table_error)
}

/// `port_create`.
fn port_create(process: &Process) -> Result<usize, Errno> {
    let port = Port::new().map_err(|_| status::NO_MEMORY)?;
    insert_new(process, Object::Port(port), Rights::PORT)
}

/// `channel_create`.
fn channel_create(process: &Process, out: u64) -> Result<usize, Errno> {
    let (first, second) = Endpoint::pair().map_err(|_| status::NO_MEMORY)?;
    let mut ends = fallible::try_with_capacity(2).map_err(|_| status::NO_MEMORY)?;
    for end in [first, second] {
        let _ = fallible::push_within(&mut ends, (Object::Channel(end), Rights::CHANNEL));
    }
    let handles = insert_all(process, ends)?;
    let written = handle_bytes(&handles)
        .and_then(|bytes| uaccess::copy_to_user(process.space(), out, &bytes).map_err(fault));
    if let Err(problem) = written {
        // The program never learned the numbers, so nothing can close them
        // but this. Another thread may already have guessed one and closed
        // it, in which case the rest are that thread's to find.
        if let Ok(taken) = process.with_handles(|table| table.take_many(&handles, Rights::NONE)) {
            object::dispose(taken.into_iter().map(|(object, _)| object));
        }
        return Err(problem);
    }
    Ok(0)
}

/// Open a handle for each of `objects`, all or none; the objects are freed
/// when there is no room for all of them.
fn insert_all(process: &Process, objects: Vec<(Object, Rights)>) -> Result<Vec<Handle>, Errno> {
    let placed = process.with_handles(|table| {
        // FALLIBLE: the handle table's reserve refuses with `TableError::NoMemory`.
        if let Err(error) = table.reserve(objects.len()) {
            return Err((table_error(error), objects));
        }
        table
            .insert_many(objects)
            .map_err(|(why, objects)| (table_error(why), objects))
    });
    placed.map_err(|(status, objects)| {
        object::dispose(objects.into_iter().map(|(object, _)| object));
        status
    })
}

/// `channel_write`.
fn channel_write(
    process: &Process,
    channel: Handle,
    bytes: Buffer,
    handles: Buffer,
) -> Result<usize, Errno> {
    let byte_count = within(bytes.count, CHANNEL_MAX_BYTES)?;
    let handle_count = within(handles.count, CHANNEL_MAX_HANDLES)?;
    let data = copy_in(process, bytes.at, byte_count)?;
    let values = copy_in_handles(process, handles.at, handle_count)?;

    // The endpoints the message would carry, found first and let go of the
    // table again: the cycle check below takes the topology lock, which comes
    // before a handle table in the lock order, never inside one.
    let (writer, carried) = process.with_handles(|table| {
        let writer = channel_in(table, channel, Rights::WRITE)?;
        let carried = carried_endpoints(table, &values)?;
        refuse_the_writing_end(&writer, channel, &values, &carried)?;
        Ok::<_, Errno>((writer, carried))
    })?;

    // Held from the check to the push, so no other send can close a cycle in
    // between. Only a message carrying an endpoint takes it: nothing else can
    // add an edge to the graph it guards.
    let checked: Vec<*const Endpoint> =
        fallible::try_collect(carried.iter().map(Arc::as_ptr)).map_err(|_| status::NO_MEMORY)?;
    let _topology = if carried.is_empty() {
        None
    } else {
        let guard = object::TOPOLOGY.lock();
        reach_status(channel::check_carry(&writer, carried))?;
        Some(guard)
    };

    process.with_handles(|table| {
        // Looked up again rather than trusting `writer`: the handle may have
        // been closed since, and a closed handle must not write.
        let endpoint = channel_in(table, channel, Rights::WRITE)?;
        // And what is taken has to be what was checked. Between the two
        // lookups another thread of this process can close a handle and be
        // issued a new one, and handle values are predictable: a value that
        // named nothing at the first lookup, and so was not checked, could
        // name the writing end itself at the second. So the writing end and
        // the endpoints the message carries are compared, and a message whose
        // handles changed underneath it is refused as try-again.
        let carried_now = carried_endpoints(table, &values)?;
        if !Arc::ptr_eq(&endpoint, &writer)
            || carried_now
                .iter()
                .map(Arc::as_ptr)
                .ne(checked.iter().copied())
        {
            return Err(status::SHOULD_WAIT);
        }
        refuse_the_writing_end(&endpoint, channel, &values, &carried_now)?;
        endpoint
            .write(data, values.len(), || {
                table.take_many(&values, Rights::TRANSFER)
            })
            .map_err(write_status)
    })?;
    Ok(0)
}

/// What a cycle check's answer means for the send that asked.
fn reach_status(reach: Reach) -> Result<(), Errno> {
    match reach {
        Reach::Clear => Ok(()),
        Reach::Found => Err(status::INVALID_ARGS),
        Reach::TooFar => Err(status::TOO_BIG),
        Reach::NoMemory => Err(status::NO_MEMORY),
    }
}

/// The status a refused write travels as.
fn write_status(failure: WriteFailure<TableError>) -> Errno {
    match failure {
        WriteFailure::PeerClosed => status::PEER_CLOSED,
        WriteFailure::TooBig => status::TOO_BIG,
        WriteFailure::Full => status::SHOULD_WAIT,
        WriteFailure::Take(error) => table_error(error),
        WriteFailure::NoMemory => status::NO_MEMORY,
    }
}

/// Refuse a message carrying the end it is written through.
///
/// Zircon's rule, for the reason it has one: the handle a call is acting
/// through should not vanish half-way through the call. A duplicate of that
/// handle is the same end, so it is compared by object as well as by number.
/// The other end — the peer — needs no rule of its own: queued in its own
/// inbox it is the shortest cycle, and `channel::check_carry` refuses it with
/// the longer ones.
fn refuse_the_writing_end(
    writer: &Arc<Endpoint>,
    channel: Handle,
    values: &[Handle],
    carried: &[Arc<Endpoint>],
) -> Result<(), Errno> {
    if values.contains(&channel) || carried.iter().any(|other| Arc::ptr_eq(other, writer)) {
        return Err(status::INVALID_ARGS);
    }
    Ok(())
}

/// The channel endpoints among `values`.
///
/// A value that names nothing is skipped here; taking the handles refuses it
/// afterwards, with the status that says why.
fn carried_endpoints(table: &HandleTable, values: &[Handle]) -> Result<Vec<Arc<Endpoint>>, Errno> {
    fallible::try_collect(values.iter().filter_map(|&value| match table.get(value) {
        Ok((Object::Channel(endpoint), _)) => Some(Arc::clone(endpoint)),
        _ => None,
    }))
    .map_err(|_| status::NO_MEMORY)
}

/// `channel_read`.
fn channel_read(
    process: &Process,
    channel: Handle,
    bytes: Buffer,
    handles: Buffer,
    actual: u64,
) -> Result<usize, Errno> {
    let byte_capacity = capacity(bytes.count, CHANNEL_MAX_BYTES);
    let handle_capacity = capacity(handles.count, CHANNEL_MAX_HANDLES);
    let endpoint = process.with_handles(|table| channel_in(table, channel, Rights::READ))?;

    // A message carrying endpoints is taken only under the topology lock, and
    // the lock is held until it is delivered or put back: see
    // `Endpoint::read`. Everything else is read without it, so bulk traffic
    // never waits on it.
    //
    // And under the lock nothing may fault. Resolving a fault can copy a
    // copy-on-write page and then wait for every processor to drop the
    // translation it replaced, which no lock may be held across -- and a
    // reader's buffer is still shared copy-on-write after a `fork` until it is
    // first written. So a message delivered holding the lock is copied only
    // into pages already there to be written; when a buffer is not, the
    // message goes back, the lock goes, the buffers are faulted in, and the
    // read starts again. That ends unless another thread keeps undoing the
    // fault, as `AddressSpace::with_page`'s retry does.
    let mut topology = None;
    loop {
        let message = match endpoint.read(byte_capacity, handle_capacity, topology.is_some()) {
            Ok(message) => message,
            Err(ReadError::NeedsTopology) => {
                // Faulted in once before the lock, so that the usual message
                // -- small, into buffers the program has written -- is
                // delivered on the first round. Only as far as the end of the
                // byte buffer's first page, because the capacity can be far
                // larger than the message and faulting it all in would commit
                // memory nothing asked for. Best effort: a buffer this cannot
                // fault in is answered for when the message is delivered.
                let space = process.space();
                let first_page = usize::try_from(PAGE_SIZE - bytes.at % PAGE_SIZE).unwrap_or(0);
                let _ = uaccess::fault_in_for_write(space, bytes.at, byte_capacity.min(first_page));
                let _ = uaccess::fault_in_for_write(
                    space,
                    handles.at,
                    handle_capacity.saturating_mul(size_of::<u32>()),
                );
                let _ = uaccess::fault_in_for_write(space, actual, size_of::<ReadActual>());
                topology = Some(object::TOPOLOGY.lock());
                continue;
            }
            Err(ReadError::TooSmall { bytes, handles }) => {
                // Nothing was taken, so nothing needs the lock to go back.
                drop(topology);
                report_actual(process, actual, bytes, handles)?;
                return Err(status::BUFFER_TOO_SMALL);
            }
            Err(refused) => return Err(read_refusal(refused)),
        };
        let through = if topology.is_some() {
            UserCopy::Present
        } else {
            UserCopy::Faulting
        };
        let byte_count = message.bytes.len();
        let handle_count = message.handles.len();
        let at = Destination {
            bytes: bytes.at,
            handles: handles.at,
            actual,
        };
        match deliver(process, &endpoint, message, at, through) {
            Ok(()) => return Ok(0),
            Err(Undelivered::Refused(why)) => return Err(why),
            Err(Undelivered::WouldFault) => {
                let _ = FAULTED_ROUNDS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                drop(topology.take());
                let space = process.space();
                uaccess::fault_in_for_write(space, bytes.at, byte_count).map_err(fault)?;
                uaccess::fault_in_for_write(
                    space,
                    handles.at,
                    handle_count.saturating_mul(size_of::<u32>()),
                )
                .map_err(fault)?;
                uaccess::fault_in_for_write(space, actual, size_of::<ReadActual>())
                    .map_err(fault)?;
            }
        }
    }
}

/// `channel_write_read` for a program, with its audit record as
/// [`dispatch`] makes one: send up to three words from registers, then wait
/// for the next message on the same end and answer it in registers. See
/// [`NativeCall::ChannelWriteRead`].
///
/// # The round trip it replaces
///
/// A request and its answer used to be, on each side, a `channel_write`, an
/// `object_wait_one` and a `channel_read`, the bytes copied in and out of the
/// program's memory through a walk of its tables each way, and the message
/// queued in a heap buffer. Here they are one call, the bytes never touch the
/// program's memory, and a message the reader keeps up with is held in the
/// inbox's slot ([`Endpoint::write_small`]). The writer's wake puts the
/// reader on the writer's processor, and the writer's wait hands it the
/// processor: the switch is the only thing between the two programs.
///
/// # Errors
///
/// As the call's documentation says.
pub(crate) fn dispatch_write_read(
    args: &SyscallArgs,
    caller: &dyn Host,
) -> Result<(usize, [u64; 3]), Errno> {
    let answered = channel_write_read(caller, &args.args);
    let plain = answered.map(|(count, _)| count);
    record_call(
        NativeCall::ChannelWriteRead,
        args.number,
        caller.core(),
        &args.args,
        &plain,
    );
    answered
}

/// [`dispatch_write_read`]'s answer, before the audit record.
fn channel_write_read(caller: &dyn Host, a: &[u64; 6]) -> Result<(usize, [u64; 3]), Errno> {
    let process = caller.core();
    // A 32-bit program's all-ones register is `usize::MAX` there too.
    let count = usize::try_from(a[1]).unwrap_or(nr::WRITE_READ_NOTHING);
    let sending = count != nr::WRITE_READ_NOTHING;
    let needed = if sending {
        Rights::READ | Rights::WRITE
    } else {
        Rights::READ
    };
    let endpoint = process.with_handles(|table| channel_in(table, handle(a[0]), needed))?;
    if sending {
        send_words(&endpoint, count, a)?;
    }
    receive_words(&endpoint)
}

/// `channel_write_read`'s write: `count` bytes of the words in the third to
/// fifth argument registers, as they would lie in memory.
fn send_words(endpoint: &Endpoint, count: usize, a: &[u64; 6]) -> Result<(), Errno> {
    if count > nr::CHANNEL_WRITE_READ_BYTES {
        return Err(status::TOO_BIG);
    }
    let mut bytes = [0_u8; nr::CHANNEL_WRITE_READ_BYTES];
    for (chunk, word) in bytes.chunks_exact_mut(size_of::<usize>()).zip(&a[2..5]) {
        // The word as the program held it: a `usize` in its registers,
        // the low half of the register on a 32-bit processor.
        chunk.copy_from_slice(&(*word as usize).to_ne_bytes());
    }
    endpoint
        .write_small(bytes.get(..count).unwrap_or_default())
        .map_err(|failure| match failure {
            WriteFailure::PeerClosed => status::PEER_CLOSED,
            WriteFailure::TooBig => status::TOO_BIG,
            WriteFailure::Full => status::SHOULD_WAIT,
            WriteFailure::NoMemory | WriteFailure::Take(()) => status::NO_MEMORY,
        })
}

/// `channel_write_read`'s read: wait for the next message on `endpoint` and
/// answer its size and its words, the bytes after it zero.
fn receive_words(endpoint: &Endpoint) -> Result<(usize, [u64; 3]), Errno> {
    loop {
        match endpoint.read_small() {
            Ok(small) => {
                let mut words = [0_u64; 3];
                for (word, chunk) in words
                    .iter_mut()
                    .zip(small.bytes.chunks_exact(size_of::<usize>()))
                {
                    let mut raw = [0_u8; size_of::<usize>()];
                    raw.copy_from_slice(chunk);
                    *word = usize::from_ne_bytes(raw) as u64;
                }
                return Ok((small.len, words));
            }
            Err(ReadError::Empty) => {}
            Err(refused) => return Err(read_refusal(refused)),
        }
        // Trusting the queue: a message and the peer's close both wake it,
        // and its process's end wakes the task. Two loads (2e): the end's
        // word, and the caller's `END` (`sched::work`), which the core posts
        // for every case the personality's `must_leave` answers true.
        let _ = endpoint
            .waiters()
            .wait_trusting(|| endpoint.readable_or_closed() || crate::sched::work::own_end());
        if crate::sched::work::own_end() {
            return Err(Errno::EINTR);
        }
    }
}

/// The status a read that took nothing answers.
const fn read_refusal(refused: ReadError) -> Errno {
    match refused {
        ReadError::Empty => status::SHOULD_WAIT,
        ReadError::PeerClosed => status::PEER_CLOSED,
        ReadError::TooSmall { .. } | ReadError::NeedsTopology => status::BUFFER_TOO_SMALL,
    }
}

/// Deliveries under the topology lock that met a buffer page they could not
/// write in place, and went round again with the lock let go: counted for the
/// boot check, which has to see that path taken rather than assume it.
static FAULTED_ROUNDS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many times a `channel_read` has gone round again to fault a buffer in
/// with the topology lock let go: see [`FAULTED_ROUNDS`].
pub(crate) fn faulted_rounds() -> u64 {
    FAULTED_ROUNDS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Where in the reader's memory a delivery goes.
#[derive(Debug, Clone, Copy)]
struct Destination {
    /// The message's bytes.
    bytes: u64,
    /// The new handles' values.
    handles: u64,
    /// The [`ReadActual`] saying how much of each arrived.
    actual: u64,
}

/// How a delivery may reach the reader's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserCopy {
    /// Faulting pages in as it goes, holding no lock.
    Faulting,
    /// Only through pages already there to be written, holding the topology
    /// lock.
    Present,
}

/// Why a message was not delivered. It has been put back, unless another
/// thread of the reader closed one of its new handles first.
#[derive(Debug)]
enum Undelivered {
    /// With this status for the program.
    Refused(Errno),
    /// A buffer page would first have had to be faulted in, which
    /// [`UserCopy::Present`] may not do.
    WouldFault,
}

/// Put a message's handles in the reader's table and its bytes in the
/// reader's memory, or put the message back.
///
/// Handles first, because a full table is the failure a program can do
/// something about, and finding it after the bytes were copied would mean
/// taking back a delivery the program might already be reading.
fn deliver(
    process: &Process,
    endpoint: &Endpoint,
    message: ChannelMessage,
    at: Destination,
    through: UserCopy,
) -> Result<(), Undelivered> {
    let Message {
        bytes: data,
        handles: transfers,
    } = message;
    // FALLIBLE: the handle table's reserve refuses with `TableError::NoMemory`.
    let placed = process.with_handles(|table| match table.reserve(transfers.len()) {
        Ok(()) => table
            .insert_many(transfers)
            .map_err(|(why, transfers)| (table_error(why), transfers)),
        Err(error) => Err((table_error(error), transfers)),
    });
    let values = match placed {
        Ok(values) => values,
        Err((why, transfers)) => {
            unread(
                endpoint,
                Message {
                    bytes: data,
                    handles: transfers,
                },
            );
            return Err(Undelivered::Refused(why));
        }
    };

    let copied = put_user(process, at.bytes, &data, through)
        .and_then(|()| {
            let bytes = handle_bytes(&values).map_err(Undelivered::Refused)?;
            put_user(process, at.handles, &bytes, through)
        })
        .and_then(|()| {
            put_user(
                process,
                at.actual,
                &actual_bytes(data.len(), values.len()),
                through,
            )
        });
    if let Err(problem) = copied {
        // Taken back and requeued, so a bad buffer loses nothing. If another
        // thread of this process has already closed one of the new handles,
        // the rest stay where they are: that thread has seen them.
        // Taking them back needs a list; with no memory for one they stay,
        // as if that other thread had closed one.
        if let Ok(transfers) = process.with_handles(|table| table.take_many(&values, Rights::NONE))
        {
            unread(
                endpoint,
                Message {
                    bytes: data,
                    handles: transfers,
                },
            );
        }
        return Err(problem);
    }
    Ok(())
}

/// Put `message` back at the head of `endpoint`'s queue, or free what it
/// carries if the queue could not grow to take it: the read putting it back
/// was failing anyway.
fn unread(endpoint: &Endpoint, message: ChannelMessage) {
    if let Err(message) = endpoint.unread(message) {
        object::dispose(message.handles.into_iter().map(|(object, _)| object));
    }
}

/// Copy `data` to `at` in the reader's memory, as `through` allows.
fn put_user(process: &Process, at: u64, data: &[u8], through: UserCopy) -> Result<(), Undelivered> {
    match through {
        UserCopy::Faulting => uaccess::copy_to_user(process.space(), at, data)
            .map_err(|why| Undelivered::Refused(fault(why))),
        UserCopy::Present => match uaccess::copy_to_user_present(process.space(), at, data) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Undelivered::WouldFault),
            Err(why) => Err(Undelivered::Refused(fault(why))),
        },
    }
}

/// A [`ReadActual`], as the bytes a program reads.
fn actual_bytes(bytes: usize, handles: usize) -> [u8; 8] {
    let actual = ReadActual {
        bytes: u32::try_from(bytes).unwrap_or(u32::MAX),
        handles: u32::try_from(handles).unwrap_or(u32::MAX),
    };
    let [b0, b1, b2, b3] = actual.bytes.to_ne_bytes();
    let [h0, h1, h2, h3] = actual.handles.to_ne_bytes();
    [b0, b1, b2, b3, h0, h1, h2, h3]
}

/// Write a [`ReadActual`] to `at`.
fn report_actual(process: &Process, at: u64, bytes: usize, handles: usize) -> Result<(), Errno> {
    uaccess::copy_to_user(process.space(), at, &actual_bytes(bytes, handles)).map_err(fault)
}

/// The channel endpoint a handle names, if it carries `needed`.
///
/// The type is checked before the rights, so a VMO handle passed where a
/// channel belongs is `WRONG_TYPE` whatever rights it carries: the more
/// useful of the two answers to whoever made the mistake.
fn channel_in(
    table: &HandleTable,
    channel: Handle,
    needed: Rights,
) -> Result<Arc<Endpoint>, Errno> {
    let (object, rights) = table.get(channel).map_err(table_error)?;
    let Object::Channel(endpoint) = object else {
        return Err(status::WRONG_TYPE);
    };
    if !rights.contains(needed) {
        return Err(status::ACCESS_DENIED);
    }
    Ok(Arc::clone(endpoint))
}

/// The VMO a handle names, if it carries `needed`.
fn vmo_in(table: &HandleTable, vmo: Handle, needed: Rights) -> Result<Arc<Vmo>, Errno> {
    let (object, rights) = table.get(vmo).map_err(table_error)?;
    let Object::Vmo(vmo) = object else {
        return Err(status::WRONG_TYPE);
    };
    if !rights.contains(needed) {
        return Err(status::ACCESS_DENIED);
    }
    Ok(Arc::clone(vmo))
}

/// A count from a register, if it is at most `max`.
fn within(count: u64, max: usize) -> Result<usize, Errno> {
    usize::try_from(count)
        .ok()
        .filter(|&count| count <= max)
        .ok_or(status::TOO_BIG)
}

/// A capacity from a register, clamped to what any message can need.
///
/// Clamped rather than refused: offering more room than a message can use is
/// not a mistake, and it bounds nothing this side allocates.
fn capacity(count: u64, max: usize) -> usize {
    usize::try_from(count).map_or(max, |count| count.min(max))
}

/// `count` bytes from the caller's memory.
fn copy_in(process: &Process, at: u64, count: usize) -> Result<Vec<u8>, Errno> {
    let mut data = fallible::try_filled(0_u8, count).map_err(|_| status::NO_MEMORY)?;
    if count > 0 {
        uaccess::copy_from_user(process.space(), at, &mut data).map_err(fault)?;
    }
    Ok(data)
}

/// `count` handle values from the caller's memory.
fn copy_in_handles(process: &Process, at: u64, count: usize) -> Result<Vec<Handle>, Errno> {
    let bytes = copy_in(process, at, count * HANDLE_BYTES)?;
    let mut handles = fallible::try_with_capacity(count).map_err(|_| status::NO_MEMORY)?;
    for word in bytes.chunks_exact(HANDLE_BYTES) {
        let word = <[u8; HANDLE_BYTES]>::try_from(word).map_err(|_| status::INVALID_ARGS)?;
        fallible::push_within(&mut handles, Handle(u32::from_ne_bytes(word)))
            .map_err(|_| status::INVALID_ARGS)?;
    }
    Ok(handles)
}

/// Handle values as a user buffer holds them.
fn handle_bytes(handles: &[Handle]) -> Result<Vec<u8>, Errno> {
    fallible::try_collect(handles.iter().flat_map(|handle| handle.0.to_ne_bytes()))
        .map_err(|_| status::NO_MEMORY)
}

/// A 64-bit value read through a pointer argument.
fn read_u64(process: &Process, at: u64) -> Result<u64, Errno> {
    let mut word = [0_u8; 8];
    uaccess::copy_from_user(process.space(), at, &mut word).map_err(fault)?;
    Ok(u64::from_ne_bytes(word))
}

/// `vmo_create`.
fn vmo_create(process: &Process, bytes: u64) -> Result<usize, Errno> {
    let pages = bytes.div_ceil(PAGE_SIZE);
    if pages > MAX_VMO_PAGES {
        return Err(status::NO_MEMORY);
    }
    let vmo = Vmo::new_anonymous(pages).map_err(|_| status::NO_MEMORY)?;
    insert_new(process, Object::Vmo(vmo), Rights::VMO)
}

/// Which way a VMO copy goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Out of the VMO, into the caller's buffer.
    Read,
    /// Out of the caller's buffer, into the VMO.
    Write,
}

/// `vmo_read`.
fn vmo_read(process: &Process, vmo: Handle, buffer: Buffer, offset: u64) -> Result<usize, Errno> {
    vmo_copy(process, vmo, buffer, offset, Direction::Read)
}

/// `vmo_write`.
fn vmo_write(process: &Process, vmo: Handle, buffer: Buffer, offset: u64) -> Result<usize, Errno> {
    vmo_copy(process, vmo, buffer, offset, Direction::Write)
}

/// Copy between a VMO and the caller's memory, a page at a time.
///
/// Through a kernel page of scratch rather than frame to user page directly,
/// so the VMO's lock is never held across a user copy — which can fault a
/// page in, and take the address space's lock to do it.
fn vmo_copy(
    process: &Process,
    vmo: Handle,
    buffer: Buffer,
    offset: u64,
    direction: Direction,
) -> Result<usize, Errno> {
    let needed = match direction {
        Direction::Read => Rights::READ,
        Direction::Write => Rights::WRITE,
    };
    let vmo = process.with_handles(|table| vmo_in(table, vmo, needed))?;
    // The copy goes through the kernel's cached view of the frames, which an
    // object shared with a device past the caches must never have lines in.
    if vmo.is_coherent() {
        return Err(status::BAD_STATE);
    }
    let offset = read_u64(process, offset)?;
    let end = offset
        .checked_add(buffer.count)
        .ok_or(status::INVALID_ARGS)?;
    if end > vmo.len_bytes() || buffer.at.checked_add(buffer.count).is_none() {
        return Err(status::INVALID_ARGS);
    }

    let mut scratch =
        fallible::try_filled(0_u8, PAGE_SIZE as usize).map_err(|_| status::NO_MEMORY)?;
    let mut done = 0_u64;
    while done < buffer.count {
        let at = offset + done;
        let chunk = (PAGE_SIZE - at % PAGE_SIZE).min(buffer.count - done) as usize;
        let slot = scratch.get_mut(..chunk).ok_or(status::INVALID_ARGS)?;
        copy_page(process, &vmo, at, buffer.at + done, slot, direction)?;
        done += chunk as u64;
    }
    Ok(0)
}

/// Copy `slot.len()` bytes between the VMO at byte `at` and the caller's
/// memory at `user`, through `slot`, which lies within one page.
fn copy_page(
    process: &Process,
    vmo: &Vmo,
    at: u64,
    user: u64,
    slot: &mut [u8],
    direction: Direction,
) -> Result<(), Errno> {
    let within = (at % PAGE_SIZE) as usize;
    match direction {
        Direction::Read => {
            vmo.read_page(at / PAGE_SIZE, within, slot)
                .map_err(vmo_error)?;
            uaccess::copy_to_user(process.space(), user, slot).map_err(fault)
        }
        Direction::Write => {
            uaccess::copy_from_user(process.space(), user, slot).map_err(fault)?;
            vmo.write_page(at / PAGE_SIZE, within, slot)
                .map_err(vmo_error)
        }
    }
}

/// The status a VMO refusal travels as.
fn vmo_error(error: VmoError) -> Errno {
    match error {
        VmoError::OutOfRange { .. } => status::INVALID_ARGS,
        VmoError::OutOfMemory => status::NO_MEMORY,
    }
}

/// `vmo_get_size`.
fn vmo_get_size(process: &Process, vmo: Handle, out: u64) -> Result<usize, Errno> {
    let vmo = process.with_handles(|table| vmo_in(table, vmo, Rights::NONE))?;
    uaccess::copy_to_user(process.space(), out, &vmo.len_bytes().to_ne_bytes()).map_err(fault)?;
    Ok(0)
}

/// Open a handle to a new object, or free the object if there is no room.
pub(crate) fn insert_new(
    process: &Process,
    object: Object,
    rights: Rights,
) -> Result<usize, Errno> {
    // FALLIBLE: the handle table's reserve refuses with `TableError::NoMemory`.
    let placed = process.with_handles(|table| match table.reserve(1) {
        Ok(()) => table
            // FALLIBLE: the handle table's insert hands the object back.
            .insert(object, rights)
            .map_err(|object| (status::NO_HANDLES, object)),
        Err(error) => Err((table_error(error), object)),
    });
    match placed {
        Ok(handle) => Ok(returned(handle)),
        Err((why, object)) => {
            object::dispose([object]);
            Err(why)
        }
    }
}

/// `object_wait_one`.
///
/// Levels, not events: a signal already asserted ends the wait at once, which
/// is what lets a program look, find nothing, and wait, without losing a
/// message that arrived in between.
///
/// The wait also ends if the calling process is killed. `process::kill` wakes
/// the task, but a wait with a condition of its own would go back to sleep and
/// sleep out its deadline first; so the condition includes it, and the call
/// then answers `EINTR`, which no program sees — its task ends on the way back
/// to user mode.
fn object_wait_one(
    caller: &dyn Host,
    handle: Handle,
    signals: u64,
    deadline_at: u64,
    observed_at: u64,
) -> Result<usize, Errno> {
    let process = caller.core();
    let wanted = Signals::from_register(signals).ok_or(status::INVALID_ARGS)?;
    let object = process.with_handles(|table| {
        let (object, rights) = table.get(handle).map_err(table_error)?;
        if !rights.contains(Rights::WAIT) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(object.clone())
    })?;
    let deadline = if deadline_at == 0 {
        u64::MAX
    } else {
        read_u64(process, deadline_at)?
    };

    let satisfied = object.waiters().wait_until_deadline(
        || object.signals().intersects(wanted) || must_leave(caller),
        deadline,
    );
    let observed = object.signals();
    object::dispose([object]);

    if observed_at != 0 {
        uaccess::copy_to_user(process.space(), observed_at, &observed.0.to_ne_bytes())
            .map_err(fault)?;
    }
    if must_leave(caller) {
        return Err(Errno::EINTR);
    }
    if satisfied {
        Ok(0)
    } else {
        Err(status::TIMED_OUT)
    }
}

/// The job a handle names, if it carries `needed`.
fn job_in(process: &Process, job: Handle, needed: Rights) -> Result<Arc<Job>, Errno> {
    process.with_handles(|table| {
        let (object, rights) = table.get(job).map_err(table_error)?;
        let Object::Job(job) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(needed) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(job))
    })
}

/// The calls on a job and on the processes made in one, which `dispatch`
/// hands on as one.
fn job_call(call: NativeCall, caller: &dyn Host, a: &[u64; 6]) -> Result<usize, Errno> {
    let [first, second, third, fourth, ..] = *a;
    let process = caller.core();
    match call {
        NativeCall::JobCreate => job_create(process, handle(first), second),
        NativeCall::JobKill => job_kill(process, handle(first)),
        NativeCall::JobSetLimit => job_set_limit(process, handle(first), second, third),
        NativeCall::JobGetQuota => job_get_quota(process, handle(first), second, third),
        NativeCall::ProcessCreate => process_create(
            caller,
            handle(first),
            handle(second),
            Buffer {
                at: third,
                count: fourth,
            },
        ),
        NativeCall::ProcessStart => process_start(process, handle(first), handle(second)),
        _ => Err(Errno::ENOSYS),
    }
}

/// The calls on a device node -- what it is, its apertures, its
/// configuration space, its quiesce, its clock and its limits -- and on the I/O mappings
/// made from one, which `dispatch` hands on as one.
fn device_call(call: NativeCall, process: &Process, a: &[u64; 6]) -> Result<usize, Errno> {
    let [first, second, third, fourth, ..] = *a;
    let device = handle(first);
    match call {
        NativeCall::IoMappingCreate => io_mapping_create(process, device, second),
        NativeCall::IoMappingMap => io_mapping_map(process, device, second, false),
        NativeCall::IoMappingMapCombining => io_mapping_map(process, device, second, true),
        NativeCall::DeviceInfo => device_info(process, device, second),
        NativeCall::DeviceAperture => device_aperture(process, device, second, third),
        NativeCall::DeviceConfigRead => device_config_read(process, device, second, third, fourth),
        NativeCall::DeviceConfigWrite => {
            device_config_write(process, device, second, third, fourth)
        }
        NativeCall::DeviceQuiesce => device_quiesce(process, device),
        NativeCall::DeviceClock => device_clock(process, device, second, third),
        NativeCall::DeviceSetLimit => device_set_limit(process, device, second, third),
        NativeCall::DeviceGetLimit => device_get_limit(process, device, second),
        NativeCall::DeviceIsolation => device_isolation(process, device),
        _ => Err(Errno::ENOSYS),
    }
}

/// `job_create`.
fn job_create(process: &Process, parent: Handle, options: u64) -> Result<usize, Errno> {
    if options & !JOB_SPECULATION_DOMAIN != 0 {
        return Err(status::INVALID_ARGS);
    }
    // MANAGE on the parent is the authority both to make a job and to make
    // it one speculation domain (`docs/OPAQUE-KERNEL.md` §9.2): checked
    // before anything is made, so a refusal marks nothing.
    let parent = job_in(process, parent, Rights::MANAGE)?;
    let domain = options & JOB_SPECULATION_DOMAIN != 0;
    let made = if domain {
        parent.new_child_domain()
    } else {
        parent.new_child()
    };
    let child = made.map_err(|why| match why {
        job::JobError::NoMemory => status::NO_MEMORY,
        _ => status::BAD_STATE,
    })?;
    if domain {
        let (parent_id, domain) = (parent.id(), child.domain());
        audit::record(
            audit::DOMAIN,
            audit::Outcome::Done,
            0,
            audit::Subject::of(process),
            audit::Target {
                kind: audit::target::JOB,
                id: child.id(),
            },
            [parent_id as u32, (parent_id >> 32) as u32, domain as u32],
        );
    }
    insert_new(process, Object::Job(child), Rights::JOB)
}

/// `job_kill`.
///
/// Answers even when the caller is inside the job it kills: the kill ends the
/// caller's own process too, and its task stops on the way back to user mode
/// rather than here, with nothing held.
fn job_kill(process: &Process, job: Handle) -> Result<usize, Errno> {
    let job = job_in(process, job, Rights::MANAGE)?;
    let ended = job
        .kill(job::KILLED_STATUS)
        .map_err(|_| status::NO_MEMORY)?;
    audit::ended(
        audit::JOB_KILLED,
        audit::Subject::of(process),
        audit::Target {
            kind: audit::target::JOB,
            id: job.id(),
        },
        [u32::try_from(ended).unwrap_or(u32::MAX), 0, 0],
    );
    Ok(0)
}

/// What `job_set_limit` and `job_get_quota` name by `resource`: a quota, or
/// the processor weight.
enum Limited {
    /// A resource the job is charged for.
    Quota(Resource),
    /// `cpu.weight`.
    Weight,
}

/// The resource a register names.
fn limited(resource: u64) -> Result<Limited, Errno> {
    match resource {
        types::JOB_MEMORY => Ok(Limited::Quota(Resource::Memory)),
        types::JOB_OBJECTS => Ok(Limited::Quota(Resource::Objects)),
        types::JOB_TASKS => Ok(Limited::Quota(Resource::Tasks)),
        types::JOB_CPU_WEIGHT => Ok(Limited::Weight),
        _ => Err(status::INVALID_ARGS),
    }
}

/// `job_set_limit`: on a handle with [`Rights::SET_LIMIT`], not only
/// [`Rights::MANAGE`], so a delegated cgroup's own limits stay its
/// delegator's (`docs/CGROUPS.md` §5).
fn job_set_limit(process: &Process, job: Handle, resource: u64, at: u64) -> Result<usize, Errno> {
    let job = job_in(process, job, Rights::SET_LIMIT)?;
    let limit = read_u64(process, at)?;
    let limited = limited(resource)?;
    let number = match limited {
        Limited::Quota(resource) => audit::resource_number(resource),
        Limited::Weight => ferrix_audit::resource::CPU_WEIGHT,
    };
    let set = match limited {
        Limited::Quota(Resource::Memory) => job.set_limit(
            Resource::Memory,
            // Whole pages, as `memory.max` keeps it.
            if limit == types::UNLIMITED {
                quota::UNLIMITED
            } else {
                limit - limit % PAGE_SIZE
            },
        ),
        Limited::Quota(resource) => job.set_limit(resource, limit),
        Limited::Weight => {
            let weight = u32::try_from(limit)
                .ok()
                .filter(|weight| (quota::MIN_WEIGHT..=quota::MAX_WEIGHT).contains(weight))
                .ok_or(status::INVALID_ARGS)?;
            job.set_cpu_weight(weight)
        }
    };
    if !set {
        return Err(status::BAD_STATE);
    }
    audit::limit_set(
        audit::LIMIT_SET,
        audit::Subject::of(process),
        job.id(),
        number,
        limit,
    );
    Ok(0)
}

/// `job_get_quota`.
fn job_get_quota(process: &Process, job: Handle, resource: u64, out: u64) -> Result<usize, Errno> {
    let job = job_in(process, job, Rights::WAIT)?;
    let [used, limit, refused] = match limited(resource)? {
        Limited::Weight => [0, u64::from(job.cpu_weight()), 0],
        Limited::Quota(resource) => {
            let usage = job.usage(resource).unwrap_or(Usage {
                used: 0,
                limit: quota::UNLIMITED,
                refused: 0,
            });
            [usage.used, usage.limit, usage.refused]
        }
    };
    let mut bytes = [0u8; 24];
    for (chunk, value) in bytes.chunks_exact_mut(8).zip([used, limit, refused]) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    uaccess::copy_to_user(process.space(), out, &bytes).map_err(fault)?;
    Ok(0)
}

/// `audit_read`: copy records of an audit ring to the holder of the audit
/// record's handle (`docs/certification/AUDIT.md` §4). The records are read
/// under the ring's leaf lock into a buffer of the kernel's, and copied out
/// after it is let go; the cursor comes in, and goes back out, in the
/// answer's second word, so it is 64 bits on every architecture. A count
/// of zero reads nothing and answers the cursor it was given.
fn audit_read(
    process: &Process,
    handle: Handle,
    which: u64,
    buffer: u64,
    count: u64,
    answer: u64,
) -> Result<usize, Errno> {
    let which = audit_ring(process, handle, which)?;
    let mut words = [0_u8; types::AUDIT_ANSWER_WORDS * 8];
    uaccess::copy_from_user(process.space(), answer, &mut words).map_err(fault)?;
    let from = words
        .get(8..16)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map_or(0, u64::from_ne_bytes);
    let count = usize::try_from(count.min(nr::AUDIT_READ_MAX)).unwrap_or(0);
    let read = if count == 0 {
        audit::Read {
            copied: 0,
            next: from,
            lost: 0,
            id: audit::id(),
        }
    } else {
        copy_records(process, which, from, buffer, count)?
    };
    let told = [
        read.copied as u64,
        read.next,
        read.lost,
        read.id as u64,
        (read.id >> 64) as u64,
    ];
    for (chunk, value) in words.chunks_exact_mut(8).zip(told) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    uaccess::copy_to_user(process.space(), answer, &words).map_err(fault)?;
    Ok(0)
}

/// The ring `which` names, if `handle` is the audit record's with `READ`.
fn audit_ring(process: &Process, handle: Handle, which: u64) -> Result<audit::Which, Errno> {
    process.with_handles(|table| match table.get(handle) {
        Ok((Object::Audit, rights)) if rights.contains(Rights::READ) => Ok(()),
        Ok((Object::Audit, _)) => Err(status::ACCESS_DENIED),
        Ok(_) => Err(status::WRONG_TYPE),
        Err(_) => Err(status::BAD_HANDLE),
    })?;
    match which {
        types::AUDIT_HIGH => Ok(audit::Which::High),
        types::AUDIT_REFUSALS => Ok(audit::Which::Refusals),
        types::AUDIT_BOOT => Ok(audit::Which::Boot),
        _ => Err(status::INVALID_ARGS),
    }
}

/// Read up to `count` records of `which` from `from` into a buffer of the
/// kernel's, under the ring's lock, and copy them to `buffer` after it.
fn copy_records(
    process: &Process,
    which: audit::Which,
    from: u64,
    buffer: u64,
    count: usize,
) -> Result<audit::Read, Errno> {
    let mut records =
        fallible::try_filled(audit::Record::EMPTY, count).map_err(|_| status::NO_MEMORY)?;
    let read = audit::read(which, from, &mut records);
    let mut at = buffer;
    for record in records.iter().take(read.copied) {
        uaccess::copy_to_user(process.space(), at, &record.to_bytes()).map_err(fault)?;
        at = at.wrapping_add(ferrix_audit::RECORD_BYTES as u64);
    }
    // Only once every record is out: a copy that faulted gave the reader
    // none of them, and the power action must still print them as unread.
    if which == audit::Which::High {
        audit::read_through(read.next);
    }
    Ok(read)
}

/// The largest ELF image `process_create` reads out of a VMO: sixteen
/// mebibytes, copied into the kernel's own memory before it is loaded.
const MAX_IMAGE_BYTES: u64 = 16 << 20;

/// `process_create`.
///
/// The image is read out of the VMO into kernel memory and loaded from there,
/// so the new process's code is ordinary memory of its own and no VMO is ever
/// mapped executable. The process is in `job` before the caller hears of it,
/// so a kill of the job reaches a process that was never started. It runs as
/// its creator, the `caller`, runs ([`LoadNative`]).
fn process_create(
    caller: &dyn Host,
    job: Handle,
    image: Handle,
    name: Buffer,
) -> Result<usize, Errno> {
    let process = caller.core();
    let job = job_in(process, job, Rights::MANAGE)?;
    let vmo = process.with_handles(|table| vmo_in(table, image, Rights::READ))?;
    let name_len = usize::try_from(name.count)
        .ok()
        .filter(|&count| count <= nr::PROCESS_NAME_MAX)
        .ok_or(status::INVALID_ARGS)?;
    let name = copy_in(process, name.at, name_len)?;
    let processes = processes().ok_or(Errno::ENOSYS)?;
    let image = image_bytes(&vmo)?;
    // The child's first memory is its job's, not the caller's: loaded as a
    // task of that job, so a limit there bounds it (`object::quota`).
    let own = crate::sched::running_group();
    crate::sched::set_current_group(job.quota_index());
    let loaded = (processes.load)(Some(caller), &image, &name);
    crate::sched::set_current_group(own);
    let child = loaded?;
    drop(image);
    if let Err(why) = child.core().move_new_to(&job) {
        // A killed job takes nothing new, and one at its task limit no more
        // tasks. What was made is ended here, in the caller's task, where a
        // kill may run.
        child.kill(job::KILLED_STATUS);
        return Err(match why {
            job::JobError::Limited => status::SHOULD_WAIT,
            _ => status::BAD_STATE,
        });
    }
    let pid = child.core().pid();
    let created = ProcessRef::created(child).map_err(|_| status::NO_MEMORY)?;
    let handle = insert_new(process, Object::Process(created), Rights::PROCESS)?;
    let made_in = job.id();
    audit::record(
        audit::PROCESS_MADE,
        audit::Outcome::Done,
        0,
        audit::Subject::of(process),
        audit::Target {
            kind: audit::target::PROCESS,
            id: u64::from(pid),
        },
        [made_in as u32, (made_in >> 32) as u32, 0],
    );
    Ok(handle)
}

/// Every byte of a VMO, for `process_create` to load.
fn image_bytes(vmo: &Vmo) -> Result<Vec<u8>, Errno> {
    let len = vmo.len_bytes();
    if len > MAX_IMAGE_BYTES {
        return Err(status::TOO_BIG);
    }
    let len = usize::try_from(len).map_err(|_| status::TOO_BIG)?;
    let mut bytes = fallible::try_filled(0_u8, len).map_err(|_| status::NO_MEMORY)?;
    for (index, page) in bytes.chunks_mut(PAGE_SIZE as usize).enumerate() {
        vmo.read_page(index as u64, 0, page).map_err(vmo_error)?;
    }
    Ok(bytes)
}

/// `process_start`.
///
/// In the order that makes a race harmless and a failure clean:
/// 1. The start is claimed first, so a second start is refused before it has
///    moved anything.
/// 2. The task is made next, without running: everything about the start that
///    can fail, failing with nothing moved.
/// 3. The bootstrap is moved under both tables, so it is in exactly one of them
///    throughout.
/// 4. Only then is the task run, with the bootstrap's value in its first
///    argument register, which cannot fail.
///
/// A process that ended before the claim is refused by the claim. One killed
/// between the claim and the move has a closed table that refuses the handle,
/// which then never leaves the caller, and the prepared task is freed on the
/// way out, after both tables' locks are let go. The `Control` held here keeps
/// a last handle closed meanwhile from killing the process under a start that
/// is about to succeed.
fn process_start(process: &Process, target: Handle, bootstrap: Handle) -> Result<usize, Errno> {
    let control = process.with_handles(|table| {
        let (object, rights) = table.get(target).map_err(table_error)?;
        let Object::Process(child) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::MANAGE) {
            return Err(status::ACCESS_DENIED);
        }
        child.control().map(Arc::clone).ok_or(status::BAD_STATE)
    })?;
    let child = control.host().ok_or(status::BAD_STATE)?;
    let processes = processes().ok_or(status::BAD_STATE)?;
    let mut place = || {
        if bootstrap == Handle::default() {
            return Ok(0);
        }
        move_handle(process, child.core(), bootstrap).map(|placed| u64::from(placed.0))
    };
    (processes.start)(&child, &mut place).map_err(|why| match why {
        StartRefused::Claimed => status::BAD_STATE,
        StartRefused::NoTask => status::NO_MEMORY,
        StartRefused::Argument(status) => status,
    })?;
    control.started();
    Ok(0)
}

/// Move `handle` out of `from`'s table into `to`'s, and answer its value there.
///
/// Both tables are held for the move, `from`'s first, so the object is in
/// exactly one of them at every moment. A refusal checked before the move
/// leaves the handle where it was, under the value it had. The one refusal
/// found after it, an insert into `to` that fails although room was checked,
/// puts the object back into `from` under whatever value that insert gives,
/// or frees it if even that fails. Moving into a child whose start is claimed,
/// whose task has not run, cannot meet the same two locks taken the other way
/// round. Needs `TRANSFER`.
fn move_handle(from: &Process, to: &Process, handle: Handle) -> Result<Handle, Errno> {
    let outcome = from.with_handles(|source| {
        let (_, rights) = source.get(handle).map_err(table_error)?;
        if !rights.contains(Rights::TRANSFER) {
            return Err(status::ACCESS_DENIED);
        }
        to.with_handles(|target| {
            if target.is_closed() {
                return Err(status::BAD_STATE);
            }
            if target.room() == 0 {
                return Err(status::NO_HANDLES);
            }
            // FALLIBLE: the handle table's reserve refuses with `TableError::NoMemory`.
            target.reserve(1).map_err(table_error)?;
            let (object, rights) = source.remove(handle).map_err(table_error)?;
            // Room was there under this same lock, so the insert takes it; a
            // refusal anyway puts the object back rather than dropping it here.
            Ok(target
                // FALLIBLE: the handle table's insert hands the object back.
                .insert(object, rights)
                .map_err(|object| source.insert(object, rights))) // FALLIBLE: the handle table's insert.
        })
    })?;
    match outcome {
        Ok(placed) => Ok(placed),
        Err(Ok(_)) => Err(status::NO_HANDLES),
        Err(Err(object)) => {
            object::dispose([object]);
            Err(status::NO_HANDLES)
        }
    }
}

/// Move `handle` out of `from`'s table into `to`'s bootstrap slot, where
/// `process_bootstrap` finds it: the item's half of `process_give`. The
/// personality answers the call, since only it knows whose child `to` is and
/// whether it has completed an `execve`, and hands the move to this.
///
/// Under both locks, the table's first, so the object is in exactly one place
/// throughout, and the slot's state is judged in the same moment as the move:
/// an `execve` sealing the slot comes before the move or after it, never in
/// between. A refusal leaves the handle where it was, under its number.
///
/// # Errors
///
/// `BAD_HANDLE`, `ACCESS_DENIED` without `TRANSFER`, `ALREADY_BOUND` for a
/// slot given before, `BAD_STATE` for a sealed one.
pub(crate) fn give_bootstrap(from: &Process, to: &Process, handle: Handle) -> Result<usize, Errno> {
    from.with_handles(|source| {
        let (_, rights) = source.get(handle).map_err(table_error)?;
        if !rights.contains(Rights::TRANSFER) {
            return Err(status::ACCESS_DENIED);
        }
        to.with_bootstrap(|slot| {
            match slot.refusal() {
                Some(GiveRefused::Given) => return Err(status::ALREADY_BOUND),
                Some(GiveRefused::Sealed) => return Err(status::BAD_STATE),
                None => {}
            }
            let (object, rights) = source.remove(handle).map_err(table_error)?;
            *slot = Bootstrap::Held(object, rights);
            Ok(())
        })
    })?;
    Ok(0)
}

/// `process_bootstrap`.
///
/// The slot is emptied first and the handle placed after, so two threads
/// asking at once cannot both be given it. A table with no room puts it back
/// for a later call; one closed meanwhile, by the process's end, leaves it to
/// be disposed of here, since the slot is sealed by then too.
fn process_bootstrap(process: &Process) -> Result<usize, Errno> {
    let Some((object, rights)) = process.with_bootstrap(Bootstrap::take) else {
        return Ok(returned(Handle::INVALID));
    };
    // FALLIBLE: the handle table's reserve refuses with `TableError::NoMemory`.
    let placed = process.with_handles(|table| match table.reserve(1) {
        Ok(()) => table
            // FALLIBLE: the handle table's insert hands the object back.
            .insert(object, rights)
            .map_err(|object| (status::NO_HANDLES, object)),
        Err(error) => Err((table_error(error), object)),
    });
    match placed {
        Ok(handle) => Ok(returned(handle)),
        Err((why, object)) => {
            if let Err(object) = process.with_bootstrap(|slot| slot.put_back(object, rights)) {
                object::dispose([object]);
            }
            Err(why)
        }
    }
}

/// `process_status`: how the process a handle names ended, or that it has
/// not, as a [`ProcessStatus`] (K6).
///
/// Read from the handle's [`crate::object::process::Exit`], which the
/// personality records once, the signal before the status before the flag
/// that says it ended, so a reader that sees the end sees both. A status the
/// personality recorded with a signal is that signal's death; any other is an
/// exit, of which a program's code is the low byte, as `exit_group` keeps it.
fn process_status(process: &Process, target: Handle, out: u64) -> Result<usize, Errno> {
    let ended = process.with_handles(|table| {
        let (object, rights) = table.get(target).map_err(table_error)?;
        let Object::Process(ended) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::WAIT) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(ended.clone())
    })?;
    let exit = ended.exit();
    let answer = match (exit.status(), exit.signal()) {
        (None, _) => ProcessStatus {
            state: types::PROCESS_RUNNING,
            value: 0,
        },
        (Some(_), Some(signal)) => ProcessStatus {
            state: types::PROCESS_KILLED,
            value: signal,
        },
        (Some(code), None) => ProcessStatus {
            state: types::PROCESS_EXITED,
            value: code.cast_unsigned() & 0xFF,
        },
    };
    // Through `dispose`, outside the table's lock: a handle closed meanwhile
    // can make this the last reference to an unstarted process, whose drop
    // ends it.
    object::dispose([Object::Process(ended)]);
    let [s0, s1, s2, s3] = answer.state.to_ne_bytes();
    let [v0, v1, v2, v3] = answer.value.to_ne_bytes();
    uaccess::copy_to_user(process.space(), out, &[s0, s1, s2, s3, v0, v1, v2, v3])
        .map_err(fault)?;
    Ok(0)
}

/// The device node a handle names, if it carries `needed`.
///
/// A node `DeviceNode::verify_config` refused answers `BAD_STATE` to every
/// call that needs `MANAGE`, until reboot: its kernel-owned configuration
/// was rewritten behind the kernel, so nothing that drives it is given.
fn device_in(process: &Process, device: Handle, needed: Rights) -> Result<Arc<DeviceNode>, Errno> {
    let node = process.with_handles(|table| {
        let (object, rights) = table.get(device).map_err(table_error)?;
        let Object::Device(node) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(needed) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(node))
    })?;
    if needed.contains(Rights::MANAGE) && node.is_refused() {
        return Err(status::BAD_STATE);
    }
    Ok(node)
}

/// `interrupt_create`.
///
/// The vector is the device's own by index, so a driver names "my second
/// interrupt" and cannot name a line its device does not have.
/// The device node `device` names in `process`, with `MANAGE`, for a call
/// that gives it a vector or a pin: refused `ACCESS_DENIED` while its
/// isolated-interrupts mark is set and the machine's interrupts are not
/// isolated. The refusal is the kernel's, whatever `devmgr` does
/// (`L.device.27`).
fn device_isolated_in(process: &Process, device: Handle) -> Result<Arc<DeviceNode>, Errno> {
    let node = device_in(process, device, Rights::MANAGE)?;
    if node.may_take_vectors_and_pins() {
        Ok(node)
    } else {
        Err(status::ACCESS_DENIED)
    }
}

fn interrupt_create(process: &Process, device: Handle, index: u64) -> Result<usize, Errno> {
    let node = device_isolated_in(process, device)?;
    // Taking a device's interrupt is what its driver does, and devmgr and a
    // quiesce never do: noted, so that a check waiting for the driver's work
    // can say how the driver ended.
    crate::discovery::devmgr::note_driver(&node, process);
    let vector = usize::try_from(index)
        .ok()
        .and_then(|index| node.vector(index))
        .ok_or(status::INVALID_ARGS)?;
    let interrupt = Interrupt::new(vector).map_err(|why| match why {
        InterruptError::Taken | InterruptError::AlreadyBound => status::ALREADY_BOUND,
        InterruptError::NotMaskable => status::INVALID_ARGS,
        InterruptError::NoMemory => status::NO_MEMORY,
    })?;
    insert_new(process, Object::Interrupt(interrupt), Rights::INTERRUPT)
}

/// `interrupt_ack`.
fn interrupt_ack(process: &Process, interrupt: Handle) -> Result<usize, Errno> {
    let interrupt = process.with_handles(|table| {
        let (object, rights) = table.get(interrupt).map_err(table_error)?;
        let Object::Interrupt(interrupt) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::MANAGE) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(interrupt))
    })?;
    interrupt.acknowledge().map_err(|_| status::INVALID_ARGS)?;
    object::dispose([Object::Interrupt(interrupt)]);
    Ok(0)
}

/// `io_mapping_create`.
///
/// `ACCESS_DENIED` for a range that is not inside one of the device's
/// apertures: that is memory this device does not have, and saying so is the
/// whole purpose of the call.
fn io_mapping_create(process: &Process, device: Handle, spec: u64) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::MANAGE)?;
    // Mapping a device's registers, likewise (see `interrupt_create`).
    crate::discovery::devmgr::note_driver(&node, process);
    let phys = read_u64(process, spec)?;
    let len = read_u64(process, spec.checked_add(8).ok_or(status::FAULT)?)?;
    let aperture = node.aperture(phys, len).ok_or(status::ACCESS_DENIED)?;
    let mapping = IoMapping::new(aperture).map_err(|why| match why {
        IoMappingError::NotWholePages => status::INVALID_ARGS,
        IoMappingError::NoMemory => status::NO_MEMORY,
    })?;
    insert_new(process, Object::IoMapping(mapping), Rights::IO_MAPPING)
}

/// Make a device's control channel for a subsystem above the item: the
/// shape `block_ring_create`, `net_ring_create`, `display_control_create`,
/// `render_control_create` and `input_control_create` share, lent to the
/// handlers their subsystems register ([`serve`]).
///
/// The device handle in the first register needs `MANAGE`, as everything
/// that gives a driver the device does; the rights and the capability are
/// decided here, in the item, and only the channel is the subsystem's to
/// make. The driver's end comes back as a handle with `rights`.
///
/// # Errors
///
/// As the handle lookup, or `create`'s status, or `NO_HANDLES`.
pub(crate) fn control_channel(
    caller: &dyn Host,
    device: u64,
    rights: Rights,
    create: impl FnOnce(&Arc<DeviceNode>) -> Result<Arc<Endpoint>, Errno>,
) -> Result<usize, Errno> {
    let process = caller.core();
    let node = device_in(process, handle(device), Rights::MANAGE)?;
    let driver_end = create(&node)?;
    insert_new(process, Object::Channel(driver_end), rights)
}

/// `device_info`.
///
/// Any device handle will do: what enumeration found is not a capability, and
/// whoever holds the device at all may know what it is.
fn device_info(process: &Process, device: Handle, at: u64) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::NONE)?;
    let info = node.describe();
    uaccess::copy_to_user(process.space(), at, &info_bytes(&info)).map_err(fault)?;
    Ok(0)
}

/// `device_aperture`.
///
/// Any device handle will do, as for `device_info`. `INVALID_ARGS` for an
/// index at or past the device's apertures.
fn device_aperture(process: &Process, device: Handle, index: u64, at: u64) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::NONE)?;
    let info = usize::try_from(index)
        .ok()
        .and_then(|index| node.aperture_info(index))
        .ok_or(status::INVALID_ARGS)?;
    uaccess::copy_to_user(process.space(), at, &aperture_bytes(&info)).map_err(fault)?;
    Ok(0)
}

/// An `ApertureInfo` as its user buffer holds it: field by field, in the
/// order declared.
fn aperture_bytes(info: &ApertureInfo) -> [u8; APERTURE_INFO_BYTES] {
    let mut bytes = [0; APERTURE_INFO_BYTES];
    let mut at = 0;
    let mut put = |source: &[u8]| {
        if let Some(slot) = bytes.get_mut(at..at + source.len()) {
            slot.copy_from_slice(source);
        }
        at += source.len();
    };
    put(&info.phys.to_ne_bytes());
    put(&info.len.to_ne_bytes());
    put(&[info.bar, info.flags]);
    put(&info.reserved);
    put(&info.offset.to_ne_bytes());
    bytes
}

/// The status a refused configuration access travels as.
fn config_status(why: ConfigRefusal) -> Errno {
    match why {
        ConfigRefusal::Arguments => status::INVALID_ARGS,
        ConfigRefusal::NotPci => status::WRONG_TYPE,
        ConfigRefusal::Unmapped => status::BAD_STATE,
        ConfigRefusal::Denied => status::ACCESS_DENIED,
    }
}

/// `device_config_read`. Needs `MANAGE`; the value goes to `out` as a `u32`
/// rather than in the return register, where on a 32-bit machine a read of
/// all ones would be taken for an error.
fn device_config_read(
    process: &Process,
    device: Handle,
    offset: u64,
    width: u64,
    out: u64,
) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::MANAGE)?;
    let value = node.config_read(offset, width).map_err(config_status)?;
    uaccess::copy_to_user(process.space(), out, &value.to_ne_bytes()).map_err(fault)?;
    Ok(0)
}

/// `device_config_write`. Needs `MANAGE`; a write any byte of which is not
/// the driver's is `ACCESS_DENIED`, and nothing is written.
fn device_config_write(
    process: &Process,
    device: Handle,
    offset: u64,
    width: u64,
    value: u64,
) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::MANAGE)?;
    node.config_write(offset, width, value)
        .map_err(config_status)?;
    Ok(0)
}

/// A `DeviceInfo` as its user buffer holds it: field by field, in the order
/// declared, padded to `DEVICE_INFO_BYTES`.
fn info_bytes(info: &DeviceInfo) -> [u8; DEVICE_INFO_BYTES] {
    let mut bytes = [0; DEVICE_INFO_BYTES];
    let mut at = 0;
    let mut put = |source: &[u8]| {
        if let Some(slot) = bytes.get_mut(at..at + source.len()) {
            slot.copy_from_slice(source);
        }
        at += source.len();
    };
    for block in [info.common, info.notify, info.isr, info.device] {
        put(&block.phys.to_ne_bytes());
        put(&block.offset.to_ne_bytes());
        put(&block.length.to_ne_bytes());
    }
    put(&info.location.to_ne_bytes());
    put(&info.class.to_ne_bytes());
    put(&info.apertures.to_ne_bytes());
    put(&info.vectors.to_ne_bytes());
    put(&info.notify_off_multiplier.to_ne_bytes());
    put(&info.vendor_id.to_ne_bytes());
    put(&info.device_id.to_ne_bytes());
    put(&info.msix_table_size.to_ne_bytes());
    put(&info.virtio.to_ne_bytes());
    put(&info.subsystem_vendor_id.to_ne_bytes());
    put(&info.subsystem_id.to_ne_bytes());
    bytes
}

/// `device_quiesce`.
///
/// The driver is gone and the device must reach nothing: every subsystem
/// that serves devices through a channel waited out ([`Server`]), bus
/// mastering off, then any claim a subsystem keeps past its driver released
/// for the next one -- the block ring's.
/// `BAD_STATE` while a driver still serves the device through a ring, or if
/// the device's configuration space could not be reached; `TIMED_OUT` when
/// the driver is gone but its ring has not ended within the patience.
fn device_quiesce(process: &Process, device: Handle) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::MANAGE)?;
    quiesce_while(&node, &|| process.is_terminated())?;
    Ok(0)
}

/// Quiesce `node` as `device_quiesce` does, for the kernel itself: before a
/// `devmgr` pid 1 starts again is handed the devices its dead predecessor's
/// drivers served (`crate::discovery::devmgr`).
///
/// # Errors
///
/// `BAD_STATE` while a driver still serves the device, `TIMED_OUT` when its
/// core has not let go in time, `BAD_STATE` when its DMA cannot be turned off.
pub(crate) fn quiesce(node: &Arc<DeviceNode>) -> Result<(), Errno> {
    quiesce_while(node, &|| false)
}

/// The body of [`device_quiesce`], given when to give up waiting.
fn quiesce_while(node: &Arc<DeviceNode>, cancelled: &dyn Fn() -> bool) -> Result<(), Errno> {
    // A dead driver's ring, card or renderer may not have noticed the death
    // yet: wait for each, as long as the driver's end of its channel is
    // closed. A driver started again is refused its channel until they let
    // go (`crate::claim`).
    let still_served = |why| match why {
        // A driver still holds its end: refused for good.
        StillServed::ByADriver => status::BAD_STATE,
        // The driver is gone but the core has not let go in time: worth
        // asking again, and devmgr does.
        StillServed::Waiting => status::TIMED_OUT,
    };
    for server in SERVERS.iter() {
        (server.wait_until_unserved)(node, cancelled).map_err(still_served)?;
    }
    node.disable_dma().map_err(|_| status::BAD_STATE)?;
    // What the dead driver, or its device's firmware, may have rewritten
    // through a BAR: a node whose kernel-owned registers read back other
    // than minted is refused, and so is the quiesce, so that no driver is
    // started on it again (SAFETY-MANUAL AoU-22).
    node.verify_config().map_err(|_| status::BAD_STATE)?;
    for release in SERVERS.iter().filter_map(|server| server.release) {
        release(node);
    }
    audit::record(
        audit::QUIESCED,
        audit::Outcome::Done,
        0,
        audit::Subject::KERNEL,
        audit::Target {
            kind: audit::target::DEVICE,
            id: node.index() as u64,
        },
        [0; 3],
    );
    Ok(())
}

/// `device_clock`.
///
/// The one clock a driver needs changed that lives in a controller every
/// peripheral shares: an STM32MP15 DK board's pixel clock, which is PLL4's Q
/// output in the RCC. The kernel does the rounding and the setting, and
/// refuses while anything else runs from that output; the driver says only
/// the rate it wants. Which device has such a clock is the board's to say,
/// when it registers the device's binding (`discovery::board::board_clock`). Any
/// other device has no clock here: `WRONG_TYPE`.
fn device_clock(process: &Process, device: Handle, hz: u64, options: u64) -> Result<usize, Errno> {
    use ferrix_native_abi::types::CLOCK_SET;
    if options & !CLOCK_SET != 0 || hz == 0 {
        return Err(status::INVALID_ARGS);
    }
    let node = device_in(process, device, Rights::MANAGE)?;
    let rate = crate::discovery::board::board_clock(&node, hz, options & CLOCK_SET != 0)
        .ok_or(status::WRONG_TYPE)?
        .map_err(|_| status::BAD_STATE)?;
    usize::try_from(rate).map_err(|_| status::BAD_STATE)
}

/// `vmo_pin`.
///
/// Whole pages of the VMO, held and pinned into the device's domain. The device
/// handle needs `MANAGE`, as minting its interrupts and mappings does. The VMO
/// needs `READ`, and `WRITE` unless the pin is read-only, since a device
/// writing a page is a write through the VMO.
///
/// `PIN_COHERENT` asks for memory the program and the device see alike, as
/// descriptors a controller polls need. For a device that snoops the caches
/// that is every pin, and the option changes nothing. For one that does not,
/// the pin must cover the whole object, which no mapping may name yet: the
/// object is marked coherent, so every mapping of it bypasses the caches, and
/// each of its frames is cleaned and dropped from them before the pin is
/// answered, so no line the kernel left -- zeroing the frame, or a copy in
/// before the pin -- is written back later over what the device wrote.
fn vmo_pin(
    process: &Process,
    device: Handle,
    vmo: Handle,
    offset: u64,
    length: u64,
    options: u64,
) -> Result<usize, Errno> {
    use ferrix_native_abi::types::{PIN_COHERENT, PIN_CONTIGUOUS, PIN_READ_ONLY};
    let page = PAGE_SIZE;
    if options & !(PIN_READ_ONLY | PIN_COHERENT | PIN_CONTIGUOUS) != 0
        || length == 0
        || !offset.is_multiple_of(page)
        || !length.is_multiple_of(page)
    {
        return Err(status::INVALID_ARGS);
    }
    let read_only = options & PIN_READ_ONLY != 0;
    let node = device_isolated_in(process, device)?;
    // The first pin is what gives the device DMA, so it is where bus
    // mastering goes on; a device whose switch cannot be reached gets no pin.
    node.enable_dma().map_err(|_| status::BAD_STATE)?;
    let vmo = process.with_handles(|table| {
        if !read_only {
            let _ = vmo_in(table, vmo, Rights::WRITE)?;
        }
        vmo_in(table, vmo, Rights::READ)
    })?;
    let past_caches = options & PIN_COHERENT != 0 && !node.dma_shape().coherent;
    if past_caches && (offset != 0 || length != vmo.len_bytes()) {
        return Err(status::INVALID_ARGS);
    }
    let (first, pages) = (offset / page, length / page);
    if options & PIN_CONTIGUOUS != 0 {
        return contiguous_pin(process, &node, &vmo, (first, pages), read_only, past_caches);
    }
    let held = vmo.hold(first, pages).map_err(vmo_error)?;
    if past_caches {
        coherent_for_device(&vmo, &held)?;
    }
    let pin = pin_through(&node, held, dma_flags(read_only), process.exit_record())?;
    insert_new(process, Object::Pin(pin), Rights::PIN)
}

/// A pin's IOMMU flags: read-only for the device, or read and write.
fn dma_flags(read_only: bool) -> ferrix_paging::MapFlags {
    if read_only {
        ferrix_paging::MapFlags::DMA_READ_ONLY
    } else {
        ferrix_paging::MapFlags::DMA
    }
}

/// `PIN_COHERENT` on a device that does not snoop the caches: every mapping
/// of `vmo` bypasses them from now on, and what `held` holds is written back
/// for the device to read.
fn coherent_for_device(vmo: &Vmo, held: &crate::user::vmo::Held) -> Result<(), Errno> {
    if !vmo.make_coherent() {
        return Err(status::BAD_STATE);
    }
    for frame in held.frames() {
        arch::flush_for_device(crate::mm::direct_map(*frame * PAGE_SIZE), PAGE_SIZE);
    }
    Ok(())
}

/// The status a refused contiguous run travels as.
fn run_status(why: crate::user::vmo::RunError) -> Errno {
    use crate::user::vmo::RunError;
    match why {
        RunError::NotAnonymous => status::WRONG_TYPE,
        RunError::TooLong | RunError::OutOfRange => status::INVALID_ARGS,
        RunError::Committed => status::BAD_STATE,
        RunError::OutOfMemory => status::NO_MEMORY,
    }
}

/// `count` more pages of live pins fit `node`'s budget, as far as can be
/// told before any frame is taken; `Pin::new` reserves them atomically after.
fn budget_allows(node: &DeviceNode, count: usize) -> Result<(), Errno> {
    if let Some(domain) = node.domain_made() {
        let now = object::pin::counts(&domain);
        if now.live.saturating_add(count) > now.budget {
            return Err(status::LIMIT_REACHED);
        }
    }
    Ok(())
}

/// Whether `held` is `count` frames from `block` on, one after another.
fn is_one_run(held: &crate::user::vmo::Held, block: u64, count: usize) -> bool {
    held.frames().len() == count
        && held
            .frames()
            .iter()
            .zip(block..)
            .all(|(&frame, expected)| frame == expected)
}

/// `vmo_pin` with `PIN_CONTIGUOUS`: fill the uncommitted `range` with one
/// run of charged frames, hold it, check what is held is one run, and pin
/// it. Arguments, rights and isolation were checked by the caller; the
/// budget is checked here before any frame is taken, and every refusal after
/// the run went in gives back exactly the frames it put there, so the VMO's
/// committed pages and the job's charge end as they began
/// (`docs/NVIDIA.md` §4.3; consultant ledger 293, D1-D3).
fn contiguous_pin(
    process: &Process,
    node: &DeviceNode,
    vmo: &Arc<Vmo>,
    (first, pages): (u64, u64),
    read_only: bool,
    past_caches: bool,
) -> Result<usize, Errno> {
    let count = usize::try_from(pages).map_err(|_| status::INVALID_ARGS)?;
    budget_allows(node, count)?;
    let block = vmo.commit_run(first, pages).map_err(run_status)?;
    let give_back = |why: Errno| {
        vmo.give_back_run(first, block, pages);
        why
    };
    let held = vmo
        .hold(first, pages)
        .map_err(|why| give_back(vmo_error(why)))?;
    // What the requirement rests on: the frames held, not how they were had.
    let checked = if is_one_run(&held, block, count) {
        if past_caches {
            coherent_for_device(vmo, &held)
        } else {
            Ok(())
        }
    } else {
        Err(status::BAD_STATE)
    };
    if let Err(why) = checked {
        drop(held);
        return Err(give_back(why));
    }
    let pin =
        pin_through(node, held, dma_flags(read_only), process.exit_record()).map_err(give_back)?;
    insert_new(process, Object::Pin(pin), Rights::PIN)
}

/// Pin `held` into `node`'s IOMMU domain with `flags`, for the process whose
/// end is `owner` (`object::pin`'s quarantine).
fn pin_through(
    node: &DeviceNode,
    held: crate::user::vmo::Held,
    flags: ferrix_paging::MapFlags,
    owner: Arc<object::process::Exit>,
) -> Result<Arc<object::pin::Pin>, Errno> {
    let domain = node.domain().map_err(|_| status::NO_MEMORY)?;
    let pin = object::pin::Pin::new(node, &domain, held, flags, owner).map_err(pin_status)?;
    fallible::try_arc(pin).map_err(|_| status::NO_MEMORY)
}

/// The status a refused pin travels as.
fn pin_status(why: object::pin::PinError) -> Errno {
    match why {
        object::pin::PinError::Domain(why) => domain_status(why),
        object::pin::PinError::NoMemory => status::NO_MEMORY,
        object::pin::PinError::LimitReached => status::LIMIT_REACHED,
        object::pin::PinError::QuarantineFull => status::QUARANTINE_FULL,
    }
}

/// `device_set_limit`.
///
/// The device's pin budget, set by whoever holds `SET_LIMIT` on it: `devmgr`,
/// which hands its drivers the device without it, so the budget is the
/// delegator's as a job's limits are (`docs/NVIDIA.md` §12.2). The budget
/// belongs to the node, through its domain, and outlives every driver.
/// `BAD_STATE` while the device has live pins, and `NO_MEMORY` past the
/// kernel's ceiling, with nothing changed.
///
/// Every answer on a device handle is audited, as `job_set_limit`'s is: a
/// set as `DEVICE_LIMIT_SET`, a refusal -- `SET_LIMIT` missing included --
/// as `DEVICE_LIMIT`, each with the device, the limit, and the old and new
/// values. So the rights refusal is made here, with the device in hand,
/// rather than by `device_in`, and `record_call` leaves this call to it.
fn device_set_limit(
    process: &Process,
    device: Handle,
    which: u64,
    value: u64,
) -> Result<usize, Errno> {
    let (node, rights) = device_and_rights(process, device)?;
    let mut old = if which == types::DEVICE_LIMIT_ISOLATED_INTERRUPTS {
        u64::from(node.isolated_marked())
    } else {
        object::pin::budget_of(&node) as u64
    };
    let answered = set_limit_on(&node, rights, which, value).map(|was| {
        old = was as u64;
        0
    });
    audit::device_limit(
        audit::Subject::of(process),
        node.index(),
        which,
        [old, value],
        &answered,
    );
    answered
}

/// [`device_set_limit`]'s decision on `node` for a handle with `rights`:
/// the budget it replaced, or why not.
fn set_limit_on(node: &DeviceNode, rights: Rights, which: u64, value: u64) -> Result<usize, Errno> {
    if !rights.contains(Rights::SET_LIMIT) {
        return Err(status::ACCESS_DENIED);
    }
    if which == types::DEVICE_LIMIT_ISOLATED_INTERRUPTS {
        return set_isolated_mark(node, value);
    }
    if which != types::DEVICE_LIMIT_PIN_PAGES {
        return Err(status::INVALID_ARGS);
    }
    // A budget no `usize` holds is past any ceiling.
    let pages = usize::try_from(value).map_err(|_| status::NO_MEMORY)?;
    let domain = node.domain().map_err(|_| status::NO_MEMORY)?;
    object::pin::set_budget(&domain, pages).map_err(|why| match why {
        object::pin::BudgetError::LivePins => status::BAD_STATE,
        object::pin::BudgetError::PastCeiling => status::NO_MEMORY,
    })
}

/// The device node a handle names, and the rights the handle carries.
fn device_and_rights(
    process: &Process,
    device: Handle,
) -> Result<(Arc<DeviceNode>, Rights), Errno> {
    process.with_handles(|table| {
        let (object, rights) = table.get(device).map_err(table_error)?;
        let Object::Device(node) = object else {
            return Err(status::WRONG_TYPE);
        };
        Ok((Arc::clone(node), rights))
    })
}

/// `device_get_limit`. Any device handle will do: the numbers say what a
/// pin may take, which a driver may know.
/// The isolated-interrupts mark, set-once (`L.device.27`, G5): a value other
/// than 0 sets it, and 0 on a marked node would clear it, which is refused.
/// Answers what the mark was.
fn set_isolated_mark(node: &DeviceNode, value: u64) -> Result<usize, Errno> {
    let was = node.isolated_marked();
    if value == 0 {
        return if was {
            Err(status::ACCESS_DENIED)
        } else {
            Ok(0)
        };
    }
    node.mark_isolated();
    Ok(usize::from(was))
}

/// `device_isolation`: how the device is isolated, as
/// `DEVICE_ISOLATION_*` bits (`L.device.27`).
fn device_isolation(process: &Process, device: Handle) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::NONE)?;
    let translated = node.domain_made().is_some_and(|domain| domain.translated());
    let mut bits = 0;
    if translated {
        bits |= types::DEVICE_ISOLATION_DMA_TRANSLATED;
    }
    if node.interrupts_isolated() {
        bits |= types::DEVICE_ISOLATION_INTERRUPTS;
    }
    Ok(bits as usize)
}

fn device_get_limit(process: &Process, device: Handle, which: u64) -> Result<usize, Errno> {
    let node = device_in(process, device, Rights::NONE)?;
    match which {
        types::DEVICE_LIMIT_ISOLATED_INTERRUPTS => Ok(usize::from(node.isolated_marked())),
        types::DEVICE_LIMIT_PIN_PAGES => Ok(object::pin::budget_of(&node)),
        types::DEVICE_LIMIT_PIN_CEILING => Ok(object::pin::ceiling()),
        types::DEVICE_LIMIT_PIN_ROOM => Ok(node
            .domain_made()
            .map_or_else(object::pin::ceiling_room, |domain| {
                object::pin::room(&domain)
            })),
        _ => Err(status::INVALID_ARGS),
    }
}

/// The status a refused pin travels as.
fn domain_status(why: crate::iommu::DomainError) -> Errno {
    use crate::iommu::DomainError;
    match why {
        DomainError::Empty => status::INVALID_ARGS,
        DomainError::OutOfRange | DomainError::Tables => status::NO_MEMORY,
        DomainError::AlreadyPinned => status::ALREADY_BOUND,
        DomainError::Foreign | DomainError::Unit(_) => status::BAD_STATE,
    }
}

/// `vmo_pin_addresses`. Writes up to `capacity` device addresses, and answers
/// how many pages the pin holds.
fn vmo_pin_addresses(
    process: &Process,
    pin: Handle,
    at: u64,
    capacity: u64,
) -> Result<usize, Errno> {
    let pin = process.with_handles(|table| {
        let (object, rights) = table.get(pin).map_err(table_error)?;
        let Object::Pin(pin) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::READ) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(pin))
    })?;
    let addresses = pin.addresses();
    let count =
        usize::try_from(capacity).map_or(addresses.len(), |capacity| capacity.min(addresses.len()));
    let bytes = fallible::try_collect(
        addresses
            .iter()
            .take(count)
            .flat_map(|address| address.to_ne_bytes()),
    );
    let written = match &bytes {
        Err(_) => Err(status::NO_MEMORY),
        Ok(bytes) if bytes.is_empty() => Ok(()),
        Ok(bytes) => uaccess::copy_to_user(process.space(), at, bytes).map_err(fault),
    };
    drop(bytes);
    let pages = addresses.len();
    object::dispose([Object::Pin(pin)]);
    written.map(|()| pages)
}

/// `io_mapping_map`. A zero address means wherever it fits.
fn io_mapping_map(
    process: &Process,
    mapping: Handle,
    address: u64,
    combining: bool,
) -> Result<usize, Errno> {
    let mapping = process.with_handles(|table| {
        let (object, rights) = table.get(mapping).map_err(table_error)?;
        let Object::IoMapping(mapping) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::MAP) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(mapping))
    })?;
    // Gathered stores reach a register as one burst, or out of order: only
    // memory the device says reads have no side effects on may combine. The
    // driver is trusted to combine only memory: the prefetchable bit is the
    // device's own claim. And on x86-64 not before every processor's PAT has
    // its write-combining entry, without which the mapping would be
    // write-through.
    if combining && (!mapping.prefetchable() || !object::io_mapping::combining_ready()) {
        return Err(status::INVALID_ARGS);
    }
    let at = (address != 0).then_some(address);
    let mapped = mapping
        .map_into(process.space(), at, combining)
        .map_err(space_status)?;
    usize::try_from(mapped).map_err(|_| status::INVALID_ARGS)
}

/// `vmo_map`.
///
/// Always shared; [`crate::user::space::AddressSpace::map_object`] says why.
/// The protection needs the rights that grant it, so a read-only handle maps
/// read-only or not at all. Every mapping reads, because none of the
/// processors Ferrix runs on can make a user page writable and not readable.
fn vmo_map(
    process: &Process,
    vmo: Handle,
    address: u64,
    length: u64,
    protection: u64,
    offset_at: u64,
) -> Result<usize, Errno> {
    let write = match u32::try_from(protection) {
        Ok(MAP_READ) => false,
        Ok(bits) if bits == MAP_READ | MAP_WRITE => true,
        _ => return Err(status::INVALID_ARGS),
    };
    let needed = if write {
        Rights::MAP | Rights::READ | Rights::WRITE
    } else {
        Rights::MAP | Rights::READ
    };
    let vmo = process.with_handles(|table| vmo_in(table, vmo, needed))?;
    let offset = read_u64(process, offset_at)?;
    let flags = VmaFlags {
        write,
        ..VmaFlags::READ
    };
    let at = (address != 0).then_some(address);
    let mapped = process
        .space()
        .map_object(at, length, vmo, offset, flags)
        .map_err(space_status)?;
    usize::try_from(mapped).map_err(|_| status::INVALID_ARGS)
}

/// The status an address space's refusal travels as.
fn space_status(why: SpaceError) -> Errno {
    match why {
        SpaceError::OutOfMemory => status::NO_MEMORY,
        SpaceError::Refused(_) => status::ACCESS_DENIED,
        // Another mapping holds the pages with another memory type.
        SpaceError::OtherMemoryType => status::ALREADY_BOUND,
        SpaceError::NotUserRange(_)
        | SpaceError::BadRange
        | SpaceError::NotMapped(_)
        | SpaceError::Backing(_)
        | SpaceError::PastEnd(_)
        | SpaceError::Unreadable(_) => status::INVALID_ARGS,
    }
}

/// The port a handle names, if it carries `needed`: for the port calls, and
/// for `port_fd`, which the load ring answers.
pub(crate) fn port_in(process: &Process, port: Handle, needed: Rights) -> Result<Arc<Port>, Errno> {
    process.with_handles(|table| {
        let (object, rights) = table.get(port).map_err(table_error)?;
        let Object::Port(port) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(needed) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(port))
    })
}

/// `port_queue`. The kind and signals the caller wrote are ignored: a program
/// queues user packets, and may not forge a signal packet.
fn port_queue(process: &Process, port: Handle, packet: u64) -> Result<usize, Errno> {
    let port = port_in(process, port, Rights::WRITE)?;
    let key = read_u64(process, packet)?;
    let first = read_u64(process, packet.checked_add(16).ok_or(status::FAULT)?)?;
    let second = read_u64(process, packet.checked_add(24).ok_or(status::FAULT)?)?;
    crate::sched::trip::port_rung(port.waiters());
    port.queue_user(key, [first, second])
        .map_err(|_| status::SHOULD_WAIT)?;
    Ok(0)
}

/// `port_wait`.
///
/// A packet taken and then not delivered, because the caller's buffer
/// faulted, is put back at the head of the queue. The wait also ends when the
/// caller is killed, or another of its process's threads replaces the program,
/// for the reason `object_wait_one` gives.
fn port_wait(
    caller: &dyn Host,
    port: Handle,
    deadline_at: u64,
    packet_at: u64,
) -> Result<usize, Errno> {
    let process = caller.core();
    let port = port_in(process, port, Rights::READ)?;
    let deadline = if deadline_at == 0 {
        u64::MAX
    } else {
        read_u64(process, deadline_at)?
    };
    let packet = loop {
        let _ = port
            .waiters()
            .wait_until_deadline(|| !port.is_empty() || must_leave(caller), deadline);
        if must_leave(caller) {
            return Err(Errno::EINTR);
        }
        // Another waiter on the same port may have taken the packet that
        // woke this one; that is a spurious wake-up, not a timeout.
        if let Some(packet) = port.take() {
            crate::sched::trip::port_taken(port.waiters(), packet.kind == types::PACKET_INTERRUPT);
            break packet;
        }
        if crate::timer::now_nanos() >= deadline {
            return Err(status::TIMED_OUT);
        }
    };
    if let Err(problem) = uaccess::copy_to_user(process.space(), packet_at, &packet_bytes(&packet))
    {
        port.put_back(packet);
        return Err(fault(problem));
    }
    Ok(0)
}

/// A packet as the ABI lays it out: key, kind, signals, two data words.
fn packet_bytes(packet: &PortPacket) -> [u8; 32] {
    let [first, second] = packet.data;
    let fields = packet
        .key
        .to_ne_bytes()
        .into_iter()
        .chain(packet.kind.to_ne_bytes())
        .chain(packet.signals.to_ne_bytes())
        .chain(first.to_ne_bytes())
        .chain(second.to_ne_bytes());
    let mut bytes = [0_u8; 32];
    for (slot, byte) in bytes.iter_mut().zip(fields) {
        *slot = byte;
    }
    bytes
}

/// `object_wait_async`.
///
/// Channels and jobs, whose signals change in task context under a lock the
/// registration can share. An interrupt reaches a port by being bound to
/// it, which is a call of its own; a VMO, a device node, an I/O mapping and a
/// port have no signal that changes, and are refused rather than accepted
/// into a registration that could never fire.
fn object_wait_async(
    process: &Process,
    watched: Handle,
    port: Handle,
    signals: u64,
    key_at: u64,
) -> Result<usize, Errno> {
    let wanted = Signals::from_register(signals).ok_or(status::INVALID_ARGS)?;
    if wanted == Signals::NONE || wanted.intersects(Signals::WRITABLE) {
        return Err(status::INVALID_ARGS);
    }
    let key = read_u64(process, key_at)?;
    let port = port_in(process, port, Rights::WRITE)?;
    let target = process.with_handles(|table| {
        let (object, rights) = table.get(watched).map_err(table_error)?;
        if !rights.contains(Rights::WAIT) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(object.clone())
    })?;

    let Ok(observer) = Observer::new(&port, key, wanted) else {
        object::dispose([target]);
        return Err(status::NO_MEMORY);
    };
    let registered = observe(&target, observer);
    object::dispose([target]);
    match registered {
        None => Err(status::WRONG_TYPE),
        Some(Ok(())) => Ok(0),
        Some(Err(_)) => Err(status::NO_MEMORY),
    }
}

/// Register `observer` on `target`, or `None` for an object with no signal
/// that changes.
fn observe(target: &Object, observer: Observer) -> Option<Result<(), PortError>> {
    match target {
        Object::Channel(endpoint) => Some(endpoint.observe(observer)),
        Object::Job(job) => Some(job.observe(observer)),
        Object::Process(process) => Some(process.exit().observe(observer)),
        Object::Vmo(_)
        | Object::Port(_)
        | Object::Device(_)
        | Object::Interrupt(_)
        | Object::IoMapping(_)
        | Object::Pin(_)
        | Object::Starter
        | Object::Audit => None,
    }
}

/// The interrupt a handle names, if it carries `needed`.
fn interrupt_in(
    process: &Process,
    interrupt: Handle,
    needed: Rights,
) -> Result<Arc<Interrupt>, Errno> {
    process.with_handles(|table| {
        let (object, rights) = table.get(interrupt).map_err(table_error)?;
        let Object::Interrupt(interrupt) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(needed) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(interrupt))
    })
}

/// `interrupt_bind`.
fn interrupt_bind(
    process: &Process,
    interrupt: Handle,
    port: Handle,
    key_at: u64,
) -> Result<usize, Errno> {
    let key = read_u64(process, key_at)?;
    let port = port_in(process, port, Rights::WRITE)?;
    let interrupt = interrupt_in(process, interrupt, Rights::MANAGE)?;
    let bound = interrupt.bind(&port, key).map_err(|why| match why {
        InterruptError::AlreadyBound | InterruptError::Taken => status::ALREADY_BOUND,
        InterruptError::NotMaskable => status::INVALID_ARGS,
        InterruptError::NoMemory => status::NO_MEMORY,
    });
    object::dispose([Object::Interrupt(interrupt)]);
    bound.map(|()| 0)
}
