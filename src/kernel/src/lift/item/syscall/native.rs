use crate::arch;
use crate::audit;
use crate::claim::StillServed;
use crate::device::DeviceNode;
use crate::fallible;
use crate::hooks::Full;
use crate::hooks::Hooks;
use crate::object;
use crate::object::HandleTable;
use crate::object::Object;
use crate::object::channel;
use crate::object::channel::ChannelMessage;
use crate::object::channel::Endpoint;
use crate::object::channel::ReadError;
use crate::object::channel::Sender;
use crate::object::channel::WriteFailure;
use crate::object::exception;
use crate::object::exception::Supervisor;
use crate::object::interrupt::Interrupt;
use crate::object::interrupt::InterruptError;
use crate::object::io_mapping::IoMapping;
use crate::object::io_mapping::IoMappingError;
use crate::object::job;
use crate::object::job::Job;
use crate::object::port::Observer;
use crate::object::port::Port;
use crate::object::port::PortError;
use crate::object::process::Bootstrap;
use crate::object::process::GiveRefused;
use crate::object::process::Host;
use crate::object::process::Process;
use crate::object::process::ProcessRef;
use crate::object::quota;
use crate::object::quota::Resource;
use crate::object::quota::Usage;
use crate::syscall::uaccess;
use crate::syscall::uaccess::UserError;
use crate::trap::SyscallArgs;
use crate::user::address_wait as waits;
use crate::user::address_wait::Key;
use crate::user::address_wait::Place;
use crate::user::address_wait::WaitError;
use crate::user::address_wait::Word;
use crate::user::space::Access;
use crate::user::space::AddressSpace;
use crate::user::space::SpaceError;
use crate::user::vmo::Vmo;
use crate::user::vmo::VmoError;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::exception as exception_abi;
use ferrix_native_abi::exception::Words;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types;
use ferrix_native_abi::types::ADDRESS_BY_VMO;
use ferrix_native_abi::types::ADDRESS_COMPARE;
use ferrix_native_abi::types::ADDRESS_SHARED;
use ferrix_native_abi::types::ADDRESS_WAIT_ANY_MAX;
use ferrix_native_abi::types::APERTURE_INFO_BYTES;
use ferrix_native_abi::types::AddressWord;
use ferrix_native_abi::types::ApertureInfo;
use ferrix_native_abi::types::CHANNEL_MAX_BYTES;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;
use ferrix_native_abi::types::DEVICE_INFO_BYTES;
use ferrix_native_abi::types::DIRTY_STATE_AWAITING_CLEAN;
use ferrix_native_abi::types::DIRTY_STATE_DIRTY;
use ferrix_native_abi::types::DeviceInfo;
use ferrix_native_abi::types::JOB_SEALED;
use ferrix_native_abi::types::JOB_SPECULATION_DOMAIN;
use ferrix_native_abi::types::MAP_EXECUTE;
use ferrix_native_abi::types::MAP_NO_PAGER;
use ferrix_native_abi::types::MAP_READ;
use ferrix_native_abi::types::MAP_WRITE;
use ferrix_native_abi::types::PAGER_OP_DECOMMIT;
use ferrix_native_abi::types::PAGER_OP_DETACH;
use ferrix_native_abi::types::PAGER_OP_DIRTY;
use ferrix_native_abi::types::PAGER_OP_FAIL;
use ferrix_native_abi::types::PAGER_OP_WRITEBACK_BEGIN;
use ferrix_native_abi::types::PAGER_OP_WRITEBACK_END;
use ferrix_native_abi::types::PAGER_VMO_NO_COPY_WAIT;
use ferrix_native_abi::types::PAGER_VMO_TRAP_DIRTY;
use ferrix_native_abi::types::POWER_READ_WORDS;
use ferrix_native_abi::types::PortPacket;
use ferrix_native_abi::types::ProcessStatus;
use ferrix_native_abi::types::ReadActual;
use ferrix_native_abi::types::ReadAttested;
use ferrix_native_abi::types::TREE_X86_POWER;
use ferrix_objects::message::Message;
use ferrix_objects::reach::Reach;
use ferrix_objects::table::TableError;
use ferrix_sync::Once;
use ferrix_vma::VmaFlags;
pub type Handler = fn(&dyn Host, &[u64; 6]) -> Result<usize, Errno>;
/// A core's entry: `crate::cores` calls it when devmgr opens that core.
pub fn serve(call: NativeCall, handler: Handler) -> Result<(), Full> {
    crate::cores::serve(call, handler);
    Ok(())
}
#[derive(Debug)]
pub struct Processes {
    /// A new process with the ELF `image` loaded, named `name`, made and not
    /// started. Refused with the native status the caller hears.
    pub load: LoadNative,
    /// A new process with an empty address space, named `name`, made and not
    /// started: `load` without the loader (`docs/microkernel/K15.md` §2.1).
    pub make: MakeNative,
    /// Claim the process's start and make its first task; then ask the
    /// argument for the value its first argument register starts with, and
    /// run it. A refused argument gives the start back with nothing run.
    pub start: StartNative,
    /// Whether the calling thread of `caller` is to leave a wait rather than
    /// sleep on: its process is ending, or another of its threads is
    /// replacing the program.
    pub must_leave: fn(caller: &dyn Host) -> bool,
}
pub type LoadNative =
    fn(creator: Option<&dyn Host>, image: &[u8], name: &[u8]) -> Result<Arc<dyn Host>, Errno>;
pub type Argument<'a> = &'a mut dyn FnMut() -> Result<u64, Errno>;
pub type StartNative = fn(
    process: &Arc<dyn Host>,
    at: Option<EntryPoint>,
    argument: Argument<'_>,
) -> Result<(), StartRefused>;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartRefused {
    Claimed,
    NoTask,
    Argument(Errno),
}
pub fn register_processes(processes: &'static Processes) {}
#[derive(Debug)]
pub struct Server {
    pub wait_until_unserved: WaitUnserved,
    pub release: Option<fn(node: &Arc<DeviceNode>)>,
}
pub type WaitUnserved =
    fn(node: &Arc<DeviceNode>, cancelled: &dyn Fn() -> bool) -> Result<(), StillServed>;
pub fn register_server(server: &'static Server) -> Result<(), Full> {
    crate::cores::note_server(server);
    Ok(())
}
pub fn fault(_: UserError) -> Errno {
    status::FAULT
}
/// A local object for a program's private table. A cgroup's job is the only one
/// that gets here (`job_for_cgroup`): a kernel job of its own under the partition's,
/// flat, made once, so that a native service runs somewhere the kernel can kill.
/// Its limits and its Linux view stay linuxd's (M5-LIFT.md §3a).
pub fn insert_new(_process: &Process, object: Object, rights: Rights) -> Result<usize, Errno> {
    let Object::Job(job) = object else {
        return Err(status::BAD_STATE);
    };
    let shared = crate::sys::kernel_job_of(Arc::as_ptr(&job) as usize).ok_or(status::NO_MEMORY)?;
    let own = crate::sys::duplicate_job(shared).map_err(status_of)?;
    let placed = ferrix_native::region::handle_to_private(
        ferrix_rt::Kernel,
        Handle(own),
        Requested::Exactly(rights),
    )
    .map_err(status_of)?;
    Ok(placed.0 as usize)
}
/// What the kernel said, for the program: the refusals its own calls can give.
fn status_of(why: ferrix_native::error::Error) -> Errno {
    why.errno().unwrap_or(status::BAD_STATE)
}

/// `PROCESS_GIVE`'s kernel half: the Linux half is judged (`launch::process_give`),
/// and the kernel moves the handle, which never enters the shared table
/// (ledger 6268, L7). A slot `execve` sealed is the Linux side's to refuse.
pub fn give_bootstrap(_from: &Process, to: &Process, handle: Handle) -> Result<usize, Errno> {
    if to.with_bootstrap(|slot| slot.refusal()) == Some(GiveRefused::Sealed) {
        return Err(status::BAD_STATE);
    }
    let target = crate::fill::kernel_process_of(to).ok_or(status::BAD_STATE)?;
    ferrix_native::lift_calls::bootstrap_move(ferrix_rt::Kernel, Handle(target), handle, false)
        .map(|()| 0)
        .map_err(status_of)
}
pub fn control_channel(
    caller: &dyn Host,
    device: u64,
    rights: Rights,
    create: impl FnOnce(&Arc<DeviceNode>) -> Result<Arc<Endpoint>, Errno>,
) -> Result<usize, Errno> {
    let node = crate::cores::node_of(device).ok_or(ferrix_native_abi::status::BAD_HANDLE)?;
    let driver_end = create(&node)?;
    crate::cores::hand_over(driver_end)
}
/// `PORT_FD`: a wait-only duplicate leaves the private table, kept there too
/// (K11 I6), and a thread mirrors it.
pub fn port_in(_process: &Process, port: Handle, needed: Rights) -> Result<Arc<Port>, Errno> {
    let kept = ferrix_native::region::handle_to_shared(
        ferrix_rt::Kernel,
        port,
        Requested::Exactly(needed),
        ferrix_native_abi::restricted::HANDLE_KEEP,
    )
    .map_err(status_of)?;
    crate::bridge::port_mirror(kept).ok_or(status::NO_MEMORY)
}

/// Where a program starts (K15's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryPoint {
    pub entry: u64,
    pub stack: u64,
}
pub type MakeNative = fn(creator: Option<&dyn Host>, name: &[u8]) -> Result<Arc<dyn Host>, Errno>;
