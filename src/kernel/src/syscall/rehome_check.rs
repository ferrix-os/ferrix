//! The native calls' answers with kernel objects alone: a core `Process` beside
//! a host of the check's own, no personality and no file system. Its caller is
//! `main.rs`'s stages, through this file's `stage_` functions, and it builds in the item's crate, so that
//! boundary itself says it names no load (`docs/microkernel/B5.md`, class A).
//!
//! A quiesce is judged here against a server of the check's own, registered for
//! the boot and answering nothing for any device but the one it is armed with:
//! what serves a device is the block ring's concern, and what a quiesce makes
//! of a server's answer is the item's.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::JOB_SEALED;
use ferrix_vma::VmaFlags;

use crate::claim::StillServed;
use crate::console::println;
use crate::device::{self, DeviceNode};
use crate::discovery::devmgr;
use crate::object::Object;
use crate::object::job::Job;
use crate::object::process::{Host, Process};
use crate::panic::{catalog, fatal};
use crate::sync::SpinLock;
use crate::syscall::native::{self, Server};
use crate::trap::{Abi, Outcome, SyscallArgs};
use crate::user::space::{self, Access, AddressSpace};
use crate::user::vmo::Vmo;

/// A host of the check's own: a core process and what a personality would keep
/// beside it, which is nothing.
#[derive(Debug)]
struct Caller {
    /// The core process.
    core: Arc<Process>,
}

impl Host for Caller {
    fn core(&self) -> &Process {
        &self.core
    }

    fn core_arc(&self) -> &Arc<Process> {
        &self.core
    }

    fn kill(&self, _status: i32) {}

    fn thread_starting(&self) {}

    fn thread_gone(&self, _ended: bool) {}

    fn wait_interrupted(&self) -> bool {
        false
    }
}

impl Caller {
    /// A process of its own in a job of its own.
    fn new() -> Result<Caller, &'static str> {
        let job = Job::new_root().map_err(|_| "no job for the item's check")?;
        let space = AddressSpace::new().map_err(|_| "no space for the item's check")?;
        let core = Process::new(space, 0, job).map_err(|_| "no process for the item's check")?;
        let core = crate::fallible::try_arc(core).map_err(|_| "no memory for the item's check")?;
        Ok(Caller { core })
    }

    /// Make a native call with these registers.
    fn call(&self, number: usize, args: &[u64]) -> Result<usize, Errno> {
        let mut registers = [0_u64; 6];
        for (slot, value) in registers.iter_mut().zip(args) {
            *slot = *value;
        }
        let args = SyscallArgs {
            abi: Abi::Native,
            number,
            args: registers,
            ip: 0,
        };
        native::dispatch(&args, Some(self))
    }

    /// A handle to `object` with `rights`, as a register.
    fn holds(&self, object: Object, rights: Rights) -> Result<u64, &'static str> {
        native::insert_new(&self.core, object, rights)
            .map(|handle| handle as u64)
            .map_err(|_| "no room for a handle in the item's check")
    }

    /// The job a handle names.
    fn job(&self, handle: u64) -> Result<Arc<Job>, &'static str> {
        let handle = Handle(u32::try_from(handle).map_err(|_| "a handle out of range")?);
        self.core.with_handles(|table| match table.get(handle) {
            Ok((Object::Job(job), _)) => Ok(Arc::clone(job)),
            _ => Err("job_create answered a handle that names no job"),
        })
    }
}

/// A number no table carries is refused with `ENOSYS`, through the registered
/// entry, and never answered by a handler or a fault.
///
/// Verifies: H.TRAP.11, L.trap.4
fn check_an_unknown_number_is_enosys() -> Result<(), &'static str> {
    let enosys = Outcome::Return(Errno::ENOSYS.as_return_value());
    for number in [0xDEAD, usize::MAX, usize::MAX - 1] {
        let args = SyscallArgs {
            abi: Abi::Native,
            number,
            args: [0; 6],
            ip: 0,
        };
        if crate::syscall::dispatch(&args, None) != enosys {
            return Err("an unknown system call number was not refused with ENOSYS");
        }
    }
    // And one a table carries is passed to the registered entry: with no entry,
    // or an entry not asked, it would read as ENOSYS too. No process runs this
    // task, so the native table answers it `ESRCH`.
    let known = SyscallArgs {
        abi: Abi::Native,
        number: nr::VMO_CREATE,
        args: [0; 6],
        ip: 0,
    };
    if crate::syscall::dispatch(&known, None) == enosys {
        return Err("a known native number was not passed to the registered entry");
    }
    Ok(())
}

/// A job is sealed exactly when made with `JOB_SEALED` or inside a sealed job;
/// `job_create` refuses any other option bit before anything is made.
///
/// Verifies: L.object.250
fn check_job_create() -> Result<(), &'static str> {
    let caller = Caller::new()?;
    let parent = Job::new_root().map_err(|_| "no parent job")?;
    let held = caller.holds(Object::Job(Arc::clone(&parent)), Rights::MANAGE)?;
    if caller.call(nr::JOB_CREATE, &[held, 1 << 40]) != Err(status::INVALID_ARGS) {
        return Err("job_create took an option bit it does not know");
    }
    if !parent
        .children()
        .map_err(|_| "no memory to list a job's children")?
        .is_empty()
    {
        return Err("a refused job_create made a job");
    }
    let plain = caller
        .call(nr::JOB_CREATE, &[held, 0])
        .map_err(|_| "job_create refused no options")?;
    if caller.job(plain as u64)?.sealed() {
        return Err("job_create with no options made a sealed job");
    }
    let sealed = caller
        .call(nr::JOB_CREATE, &[held, JOB_SEALED])
        .map_err(|_| "job_create refused JOB_SEALED")?;
    let sealed = caller.job(sealed as u64)?;
    if !sealed.sealed() {
        return Err("job_create with JOB_SEALED made a job that is not sealed");
    }
    if parent.sealed() {
        return Err("the job a sealed job was made in reads as sealed");
    }
    let held_sealed = caller.holds(Object::Job(sealed), Rights::MANAGE)?;
    let inner = caller
        .call(nr::JOB_CREATE, &[held_sealed, 0])
        .map_err(|_| "job_create inside a sealed job was refused")?;
    if !caller.job(inner as u64)?.sealed() {
        return Err("a job job_create made inside a sealed job does not read as sealed");
    }
    Ok(())
}

/// The node's description is what the node holds, and what enumeration read.
///
/// Verifies: L.device.10
fn check_described() -> Result<(), &'static str> {
    for node in device::devices() {
        let Some(function) = node.pci_function() else {
            continue;
        };
        let info = node.describe();
        if info.vendor_id != function.vendor || info.device_id != function.device {
            return Err("a node's description named another vendor or device than enumeration");
        }
        if info.apertures as usize != node.apertures().len() {
            return Err("a node's description counted its apertures wrongly");
        }
        if info.vectors as usize != node.vector_count() {
            return Err("a node's description counted its vectors wrongly");
        }
        if let device::Location::Pci(address) = node.location() {
            let location = (u32::from(address.segment()) << 16)
                | (u32::from(address.bus()) << 8)
                | (u32::from(address.device()) << 3)
                | u32::from(address.function());
            if info.location != location {
                return Err("a node's description named another location than the node's");
            }
        }
        match function.virtio {
            Some(virtio) => {
                if info.common.phys != virtio.common.phys
                    || info.common.offset != virtio.common.offset
                    || info.notify_off_multiplier != virtio.notify_multiplier
                    || info.isr.length != virtio.isr.length
                {
                    return Err("a node's description placed virtio's blocks otherwise");
                }
            }
            None => {
                if info.common.length != 0 {
                    return Err("a node with no virtio placement described one");
                }
            }
        }
    }
    Ok(())
}

/// What the check's server is to answer for [`SERVED`].
const FREE: usize = 0;
/// A driver holds it.
const BY_A_DRIVER: usize = 1;
/// The driver is gone and the core has not let go.
const STILL_WAITING: usize = 2;
/// The node's address the server is armed for, or zero.
static SERVED: AtomicUsize = AtomicUsize::new(0);
/// What it answers.
static ANSWER: AtomicUsize = AtomicUsize::new(FREE);
/// How many times the quiesce asked it for the node it is armed with.
static WAITS: AtomicU64 = AtomicU64::new(0);
/// How many times the quiesce told it to let go of its claim.
static RELEASES: AtomicU64 = AtomicU64::new(0);
/// Whether the node's bus mastering read off as the claim was let go.
static OFF_AT_RELEASE: AtomicBool = AtomicBool::new(false);

/// Whether `node`'s function may master the bus, read from its command register.
fn bus_mastering(node: &DeviceNode) -> Option<bool> {
    /// The command register, and its Bus Master Enable.
    const COMMAND: u64 = 4;
    const BUS_MASTER: u32 = 1 << 2;
    node.config_read(COMMAND, 2)
        .ok()
        .map(|command| command & BUS_MASTER != 0)
}

/// [`Server::wait_until_unserved`] of the check's server.
fn check_server_wait(
    node: &Arc<DeviceNode>,
    _cancelled: &dyn Fn() -> bool,
) -> Result<(), StillServed> {
    if Arc::as_ptr(node) as usize != SERVED.load(Ordering::SeqCst) {
        return Ok(());
    }
    let _ = WAITS.fetch_add(1, Ordering::SeqCst);
    match ANSWER.load(Ordering::SeqCst) {
        BY_A_DRIVER => Err(StillServed::ByADriver),
        STILL_WAITING => Err(StillServed::Waiting),
        _ => Ok(()),
    }
}

/// [`Server::release`] of the check's server.
fn check_server_release(node: &Arc<DeviceNode>) {
    if Arc::as_ptr(node) as usize == SERVED.load(Ordering::SeqCst) {
        let _ = RELEASES.fetch_add(1, Ordering::SeqCst);
        OFF_AT_RELEASE.store(bus_mastering(node) == Some(false), Ordering::SeqCst);
    }
}

/// The check's server: nothing for every other device, ever.
static CHECK_SERVER: Server = Server {
    wait_until_unserved: check_server_wait,
    release: Some(check_server_release),
};

/// Whether the check's server is registered: once a boot.
static REGISTERED: SpinLock<bool> = SpinLock::new(false);

/// A node a quiesce can be tried on: a virtio function or QEMU's `edu` test
/// device, which no firmware is behind to be surprised by bus mastering, with
/// configuration space, that no one has been given yet and whose bus mastering
/// is off and goes on. OVMF leaves it on on every function it drove, virtio's
/// included, so on x86-64 under QEMU that is `edu`. On any other machine, none,
/// and the boot line's count says so. A function that does not take it is left
/// off.
fn subject() -> Option<Arc<DeviceNode>> {
    /// The virtio vendor.
    const VIRTIO: u16 = 0x1AF4;
    /// QEMU's `edu`.
    const EDU: (u16, u16) = (0x1234, 0x11E8);
    device::devices()
        .iter()
        .filter(|node| {
            matches!(node.location(), device::Location::Pci(_))
                && node
                    .pci_function()
                    .is_some_and(|f| f.vendor == VIRTIO || (f.vendor, f.device) == EDU)
                && node.config_phys().is_some()
                && !node.yielded()
                && node.framebuffer_mode().is_none()
                && !node.is_refused()
                && bus_mastering(node) == Some(false)
        })
        .find(|node| {
            let on = node.enable_dma().is_ok() && bus_mastering(node) == Some(true);
            if !on {
                let _ = node.disable_dma();
            }
            on
        })
        .map(Arc::clone)
}

/// Whether a quiesce of `node` through `handle` is refused as `wanted`.
fn quiesce(caller: &Caller, handle: u64) -> Result<usize, Errno> {
    caller.call(nr::DEVICE_QUIESCE, &[handle])
}

/// A quiesce through a handle without `MANAGE` is refused; one under a driver is
/// `BAD_STATE`, changing nothing; and one as the driver dies waits for the core,
/// turns the device's DMA off, lets go the core's claim and answers success.
///
/// Verifies: L.quiesce.1, L.quiesce.2, L.quiesce.3, H.DEV.2, H.DEV.4
fn check_quiesce() -> Result<bool, &'static str> {
    {
        let mut registered = REGISTERED.lock();
        if !*registered {
            native::register_server(&CHECK_SERVER)
                .map_err(|_| "no room for the quiesce check's server")?;
            *registered = true;
        }
    }
    let Some(node) = subject() else {
        return Ok(false);
    };
    let caller = Caller::new()?;
    let full = caller.holds(Object::Device(Arc::clone(&node)), Rights::MANAGE)?;
    let bare = caller.holds(Object::Device(Arc::clone(&node)), Rights::TRANSFER)?;
    SERVED.store(Arc::as_ptr(&node) as usize, Ordering::SeqCst);
    ANSWER.store(BY_A_DRIVER, Ordering::SeqCst);
    let outcome = (|| {
        if quiesce(&caller, bare) != Err(status::ACCESS_DENIED) {
            return Err("a device was quiesced through a handle without MANAGE");
        }
        if quiesce(&caller, full) != Err(status::BAD_STATE) {
            return Err("a device was quiesced under the driver serving it");
        }
        if bus_mastering(&node) != Some(true) {
            return Err("a quiesce refused for a live driver turned the device's DMA off");
        }
        if RELEASES.load(Ordering::SeqCst) != 0 {
            return Err("a quiesce refused for a live driver let go of the core's claim");
        }
        // Its driver gone and its core not yet let go: no success, the device
        // still on and the claim still held, and the quiesce may be asked again.
        ANSWER.store(STILL_WAITING, Ordering::SeqCst);
        if quiesce(&caller, full) != Err(status::TIMED_OUT) {
            return Err("a quiesce answered otherwise than TIMED_OUT while a core had not let go");
        }
        if bus_mastering(&node) != Some(true) {
            return Err("a quiesce turned the device's DMA off before every core had let go");
        }
        if RELEASES.load(Ordering::SeqCst) != 0 {
            return Err("a quiesce let go of the core's claim before every core had let go");
        }
        ANSWER.store(FREE, Ordering::SeqCst);
        let waited = WAITS.load(Ordering::SeqCst);
        if quiesce(&caller, full).is_err() {
            return Err("a quiesce as the driver died did not answer success");
        }
        if WAITS.load(Ordering::SeqCst) == waited {
            return Err("a quiesce did not wait for the core serving the device");
        }
        if bus_mastering(&node) != Some(false) {
            return Err("a device quiesced after its driver died still had bus mastering on");
        }
        if RELEASES.load(Ordering::SeqCst) != 1 {
            return Err("a quiesce did not let go of the claim the core keeps past its driver");
        }
        if !OFF_AT_RELEASE.load(Ordering::SeqCst) {
            return Err("a quiesce let go of the core's claim before the device's DMA was off");
        }
        Ok(())
    })();
    SERVED.store(0, Ordering::SeqCst);
    outcome.map(|()| true)
}

/// A write-combining object's mappings are write-combining, every copy through
/// the direct map is made coherent, `vmo_read` and `vmo_write` are refused, and
/// no core's view and write-combining ever meet.
///
/// Verifies: L.user.133
fn check_a_combining_object() -> Result<bool, &'static str> {
    let object = Vmo::new_anonymous(1).map_err(|_| "no object to make write-combining")?;
    if !object.make_combining() {
        return Ok(false);
    }
    let caller = Caller::new()?;
    let space = caller.core.own_space();
    let at = space
        .map_object(
            None,
            PAGE_SIZE,
            Arc::clone(&object),
            0,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
        )
        .map_err(|_| "a write-combining object would not map")?;
    space
        .fault(at, Access::WRITE)
        .map_err(|_| "a write-combining page would not fault in")?;
    let leaf = crate::mm::flags_in(space.root_table(), at)
        .ok_or("a write-combining page has no translation")?;
    if !leaf.write_combining || leaf.device {
        return Err("a mapping of a write-combining object was not write-combining");
    }
    let touches = space::coherent_touches();
    space
        .with_page(at, Access::WRITE, |byte| {
            // SAFETY: (FRAME) `with_page` passes the direct-map address of the
            // byte, translated under the space's lock, held meanwhile.
            unsafe { core::ptr::write(byte as *mut u8, 0x5A) }
        })
        .map_err(|_| "a kernel copy into a write-combining page was refused")?;
    let read = space
        .with_page(at, Access::READ, |byte| {
            // SAFETY: (FRAME) as above.
            unsafe { core::ptr::read(byte as *const u8) }
        })
        .map_err(|_| "a kernel copy from a write-combining page was refused")?;
    if read != 0x5A {
        return Err("a kernel copy from a write-combining page read otherwise");
    }
    if space::coherent_touches() != touches + 2 {
        return Err("a copy to or from a write-combining page was not made coherent");
    }
    let handle = caller.holds(
        Object::Vmo(Arc::clone(&object)),
        Rights::READ | Rights::WRITE,
    )?;
    if caller.call(nr::VMO_READ, &[handle, 0, 0, 0]) != Err(status::BAD_STATE) {
        return Err("vmo_read of a write-combining object was not refused");
    }
    if caller.call(nr::VMO_WRITE, &[handle, 0, 0, 0]) != Err(status::BAD_STATE) {
        return Err("vmo_write of a write-combining object was not refused");
    }
    if object.take_kernel_view() {
        return Err("a kernel core took a view of a write-combining object");
    }
    let viewed = Vmo::new_anonymous(1).map_err(|_| "no object for a core's view")?;
    if !viewed.take_kernel_view() {
        return Err("a kernel core was refused a view of a plain object");
    }
    if viewed.make_combining() {
        return Err("an object a kernel core reads was made write-combining");
    }
    Ok(true)
}

/// `drivers_job` makes `drivers.slice` sealed, and every job beneath it is. On a
/// boot whose devmgr bound a driver the slice is there already and its absence is
/// the failure; on one with no devmgr this makes it, as `ferrix.devmgr=init` does.
///
/// Verifies: L.device.54
fn check_drivers_sealed() -> Result<u64, &'static str> {
    let named = || -> Result<Option<Arc<Job>>, &'static str> {
        let children = crate::object::job::root()
            .children()
            .map_err(|_| "no memory to list the root job's children")?;
        Ok(children
            .into_iter()
            .find(|job| job.name() == Some("drivers.slice")))
    };
    let bound = (0..device::devices().len()).any(|at| devmgr::driver_of(at).is_some());
    let drivers = match named()? {
        Some(drivers) => drivers,
        None if bound => return Err("a boot whose devmgr bound a driver has no drivers.slice"),
        None => {
            devmgr::make_drivers_job()?;
            named()?.ok_or("make_drivers_job made no drivers.slice")?
        }
    };
    let made = drivers
        .new_child()
        .map_err(|_| "no job could be made beneath drivers.slice")?;
    if !made.sealed() {
        return Err("a job made beneath drivers.slice is not sealed");
    }
    drop(made);
    let mut pending = alloc::vec![drivers];
    let mut seen = 0;
    while let Some(job) = pending.pop() {
        if !job.sealed() {
            return Err("a job in drivers.slice is not sealed");
        }
        seen += 1;
        if seen > 4096 {
            return Err("drivers.slice holds more jobs than the check walks");
        }
        let below = job
            .children()
            .map_err(|_| "no memory to list a driver job's children")?;
        pending
            .try_reserve(below.len())
            .map_err(|_| "no memory to walk drivers.slice")?;
        pending.extend(below);
    }
    Ok(seen)
}

/// The cases that need nothing but the scheduler and the entry, before any
/// other check calls what they break, and answer how many there were.
///
/// # Errors
///
/// The first thing that was not so, by name.
fn run_early() -> Result<u64, &'static str> {
    check_an_unknown_number_is_enosys()?;
    check_job_create()?;
    let combined = check_a_combining_object()?;
    Ok(2 + u64::from(combined))
}

/// The cases on the device nodes, once they are published, and answer how many
/// there were: a machine with no device to quiesce says so by the count.
///
/// # Errors
///
/// The first thing that was not so, by name.
fn run_on_devices() -> Result<u64, &'static str> {
    check_described()?;
    let quiesced = check_quiesce()?;
    Ok(1 + u64::from(quiesced))
}

/// The shrinker's slot, ahead of the load's registering its own, which replaces
/// the check's for good. In every shape of the kernel, as the rest of this file.
pub fn stage_shrinker() {
    if !crate::checks::run() {
        return;
    }
    let cases = match crate::user::rehome_cache_check::check_the_shrinker() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::STAGE6_REHOMED_CACHE,
            "core shrinker self-check failed: {problem}"
        ),
    };
    println!(
        "  core shr {cases} cases of a reclaim's shrinker, on a slot before any is registered"
    );
}

/// The core's checks made of kernel objects alone, once the scheduler can run a
/// task and has passed its own check.
pub fn stage_core_objects() {
    let cases = match crate::user::rehome_check::run() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::STAGE6_REHOMED_MEMORY,
            "stage 6 core memory self-check failed: {problem}"
        ),
    };
    println!(
        "  core mem {cases} cases of spaces and objects with kernel objects alone, no frame kept"
    );
    let cases = match crate::object::rehome_check::run() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::STAGE6_REHOMED_OBJECTS,
            "stage 6 core object self-check failed: {problem}"
        ),
    };
    println!(
        "  core obj {cases} cases of jobs, quotas and a process's core with kernel objects alone"
    );
    let cases = match crate::user::rehome_cache_check::run() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::STAGE6_REHOMED_CACHE,
            "stage 6 core cache self-check failed: {problem}"
        ),
    };
    println!("  core pc  {cases} cases of the page cache with kernel objects alone");
    let cases = match crate::sched::rehome_check::run() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::STAGE6_REHOMED_OBJECTS,
            "stage 6 core switch-count self-check failed: {problem}"
        ),
    };
    println!("  core sch {cases} cases of the switch counts with kernel tasks alone");
    let cases = match crate::trap::rehome_check::run() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::STAGE6_REHOMED_OBJECTS,
            "stage 6 core trap self-check failed: {problem}"
        ),
    };
    println!("  core trp {cases} cases of the filter's slot and the entries' answers");
    let cases = match run_early() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::ITEM_REHOMED_CALLS,
            "stage 6 native calls' self-check failed: {problem}"
        ),
    };
    println!("  item     {cases} cases of the native calls with a core process alone");
}

/// The native calls' answers on the device nodes just published.
pub fn stage_on_devices() {
    if !crate::checks::run() {
        return;
    }
    let cases = match run_on_devices() {
        Ok(cases) => cases,
        Err(problem) => fatal!(
            catalog::ITEM_REHOMED_CALLS,
            "stage 9 native calls' self-check failed: {problem}"
        ),
    };
    println!("  item dev {cases} cases of the native calls on the device nodes");
}

/// `drivers.slice`, once what starts it has run.
pub fn stage_drivers_sealed() {
    if !crate::checks::run() {
        return;
    }
    let seen = match check_drivers_sealed() {
        Ok(seen) => seen,
        Err(problem) => fatal!(
            catalog::ITEM_REHOMED_CALLS,
            "stage 10 drivers' job self-check failed: {problem}"
        ),
    };
    println!("  drv jobs {seen} jobs of drivers.slice read as sealed");
}
