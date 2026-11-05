//! Who the kernel starts under `personality=user`, and with what
//! (`docs/MICROKERNEL.md` §5, `docs/microkernel/BOOT-AUTHORITY.md`): the
//! partition manager, `devmgr` and `linuxd`, from the initramfs, each with its
//! own row of handles, so that device authority never passes through init or
//! `linuxd`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::first::{self, READY_MAGIC};
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES};
use ferrix_partition_desc::{Description, Device as Described, KIND_PCI, ROLE_FILES, ROLE_LINUX};

use crate::console::println;
use crate::device::{self, DeviceNode, Location};
use crate::discovery::{devmgr, first_image};
use crate::fallible;
use crate::first_rows::{Held, MANAGER};
use crate::object::Object;
use crate::object::bare::Bare;
use crate::object::channel::{Endpoint, ReadError, Sender};
use crate::object::job;
use crate::object::process::{self, Host};
use crate::panic::{catalog, fatal};
use crate::sync::SpinLock;
use crate::syscall::native::{self, Argument, EntryPoint, Processes, StartRefused};
use crate::user::space::AddressSpace;
use crate::user::vmo::Vmo;
use crate::vmap;

/// The boot's check of regions and restricted mode, which `main.rs` hands in: product code
/// names no check (ledger 7184 B4.1, B4.3).
static REGION_CHECK: ferrix_sync::Once<fn()> = ferrix_sync::Once::new();

/// Hand in [`REGION_CHECK`].
pub fn set_region_check(check: fn()) {
    let _ = REGION_CHECK.call_once(|| check);
}

/// Start the first processes, in `MICROKERNEL.md` §5's order, `linuxd` last:
/// its first program is init, which expects its devices' drivers to be there.
pub fn run(view: &ferrix_bootinfo::BootView<'_>) {
    if crate::mm::keep_initrd(view).is_err() {
        println!("  initrd   outside the direct map: no first process can start");
    }
    native::register_processes(&PROCESSES);
    devmgr::register_reader(read_file);
    read_certified_list();
    flag_nodes();
    // Before K16's seal and before any process but its own: what it reads is the kernel's.
    if let Some(check) = REGION_CHECK.get() {
        check();
    }
    let served = start_manager();
    let (block_cores, files, further, netd_ends) = match served {
        Some(ends) => (ends.block_cores, ends.files, ends.further, ends.netd_ends),
        None => (None, None, Vec::new(), Vec::new()),
    };
    let detached = match devmgr::start_detached(block_cores, netd_ends) {
        Ok(Some(detached)) => {
            println!("  devmgr   started: its channels go to linuxd");
            Some(detached)
        }
        Ok(None) => {
            println!("  devmgr   not started: the image carries no /sbin/devmgr");
            None
        }
        Err(why) => {
            println!("  devmgr   did not start: {why}");
            None
        }
    };
    crate::linuxd_start::run_with(view, detached, files, &further);
}

/// Held at each start site rather than taken on trust (ledger 6564 Y.3): what the kernel
/// starts a first process or its drivers in holds them to sealed images.
pub(crate) fn must_be_measured(job: &job::Job) {
    if !job.measured() {
        not_measured("a first process's job is not measured");
    }
}

/// The stop for [`must_be_measured`], and its like for a space.
pub(crate) fn not_measured(what: &str) -> ! {
    fatal!(catalog::FIRST_PROCESSES, "first processes: {what}");
}

/// The seal's check, which the composition root hands in: product code names no check.
#[cfg(ferrix_kernel_memory = "retyped")]
pub type SealCheck = fn(&job::Job, &job::Job) -> Result<(), &'static str>;
#[cfg(ferrix_kernel_memory = "retyped")]
static SEAL_CHECK: ferrix_sync::Once<SealCheck> = ferrix_sync::Once::new();

/// Hand in the seal's check, before [`run`].
#[cfg(ferrix_kernel_memory = "retyped")]
pub fn set_seal_check(check: SealCheck) {
    let _ = SEAL_CHECK.call_once(|| check);
}

/// K16: from here on the shared heap refuses, after the first processes are made and
/// before they run, and the seal holds itself to its rule (`docs/microkernel/K16.md`).
#[cfg(ferrix_kernel_memory = "retyped")]
pub(crate) fn seal() {
    /// The kernel pool: its name, its size, and past it a refusal (K16.4).
    const KERNEL_POOL: u64 = 16 << 20;
    /// The check's own job's promise.
    const PROBE: u64 = 256 << 10;
    let pool = job::root().new_child().ok();
    let promised = job::root().new_child().ok();
    let bare = job::root().new_child().ok();
    let (Some(pool), Some(promised), Some(bare)) = (pool, promised, bare) else {
        fatal!(catalog::FIRST_PROCESSES, "no jobs for the seal");
    };
    if pool.set_reservation(KERNEL_POOL).is_err() || promised.set_reservation(PROBE).is_err() {
        fatal!(catalog::FIRST_PROCESSES, "no promise for the kernel pool");
    }
    crate::mm::seal::seal_after_boot(pool.draws_from());
    if let Some(check) = SEAL_CHECK.get()
        && let Err(why) = check(&promised, &bare)
    {
        fatal!(catalog::FIRST_PROCESSES, "the seal: {why}");
    }
    core::mem::forget(pool);
    fn report(_argument: usize) {
        loop {
            crate::sched::sleep_for(10_000_000_000);
            crate::mm::seal::report();
        }
    }
    if crate::sched::spawn("seal report", report, 0, ferrix_sched::NICE_0_WEIGHT).is_err() {
        println!("  retyped  no task to report on");
    }
    println!("  retyped  sealed");
}

/// A file of the initramfs, whole, for `devmgr`'s start.
fn read_file(path: &[u8]) -> Result<vmap::Buffer, Errno> {
    let name = path.strip_prefix(b"/").unwrap_or(path);
    crate::mm::with_initrd(|archive| {
        let file = ferrix_cpio::file_named(archive, name).ok_or(Errno::ENOENT)?;
        let mut buffer = vmap::Buffer::zeroed(file.len()).map_err(|_| Errno::ENOMEM)?;
        buffer.copy_from_slice(file);
        Ok(buffer)
    })
    .ok_or(Errno::ENOENT)?
}

/// A native process needs no personality here: a process of its own over an
/// empty space. `process_make` and the first images come through this.
static PROCESSES: Processes = Processes {
    load: refuse_load,
    make,
    start,
    must_leave,
};

/// The kernel parses no image a program names in this build (K15 §2.4).
fn refuse_load(
    _creator: Option<&dyn Host>,
    _image: &[u8],
    _name: &[u8],
) -> Result<Arc<dyn Host>, Errno> {
    Err(Errno::ENOSYS)
}

/// An unstarted process in its creator's job, or, for a first image, in a job
/// of its own under the root.
fn make(creator: Option<&dyn Host>, _name: &[u8]) -> Result<Arc<dyn Host>, Errno> {
    let job = match creator {
        Some(creator) => creator.core().job(),
        None => {
            // devmgr's: measured, as the manager's is, so that everything K14 says of a
            // driver is shown there too (BA.4, ledger 6316 P1.4).
            let job = job::root().new_child().map_err(|_| status::NO_MEMORY)?;
            job.make_measured();
            #[cfg(ferrix_kernel_memory = "retyped")]
            job.set_reservation(16 << 20)
                .map_err(|_| status::NO_MEMORY)?;
            job
        }
    };
    let space = AddressSpace::new().map_err(|_| status::NO_MEMORY)?;
    if creator.is_none() {
        space.measure();
    }
    let host = Bare::new(space, job, None).map_err(|_| status::NO_MEMORY)?;
    Ok(host)
}

/// Its first thread, where its creator or the first image says.
fn start(
    process: &Arc<dyn Host>,
    at: Option<EntryPoint>,
    argument: Argument<'_>,
) -> Result<(), StartRefused> {
    let bare = process::downcast::<Bare>(Arc::clone(process)).ok_or(StartRefused::Claimed)?;
    let at = at.ok_or(StartRefused::Claimed)?;
    let value = argument().map_err(StartRefused::Argument)?;
    bare.start(at.entry, at.stack, value)
        .map_err(|_| StartRefused::NoTask)
}

/// Whether its waiting thread is to leave: its process has ended.
fn must_leave(caller: &dyn Host) -> bool {
    caller.core().is_terminated()
}

/// Whether a description named a safety partition's device.
static HELD: SpinLock<Vec<Arc<DeviceNode>>> = SpinLock::new(Vec::new());

/// Whether `node` is a safety partition's: never in DEVICES.
pub(crate) fn held_by_manager(node: &Arc<DeviceNode>) -> bool {
    HELD.lock().iter().any(|held| Arc::ptr_eq(held, node))
}

/// A description past what its first message holds: refused, never a slot dropped
/// (ledger 6316 P1.1), since the manager's count of slots would then shift and a held
/// device be nobody's.
const TOO_MANY: &str = "a description names more than the manager's first message holds";

/// The manager's path in the archive.
const MANAGER_PATH: &[u8] = b"sbin/partmgr";
/// The description's.
const DESCRIPTION: &[u8] = b"etc/ferrix/partitions";
/// How long the manager has to say READY: a bound, and a miss stops the boot
/// (`BOOT-AUTHORITY.md`, Q1). READY means started, not healthy.
const READY_PATIENCE_NANOS: u64 = 20_000_000_000;
const NANOS_PER_MILLI: u64 = 1_000_000;

/// A copy of a file of the archive in a sealed VMO, or `None` without the file.
fn sealed_file(path: &[u8]) -> Result<Option<(Arc<Vmo>, usize)>, &'static str> {
    crate::mm::with_initrd(|archive| {
        let Some(file) = ferrix_cpio::file_named(archive, path) else {
            return Ok(None);
        };
        devmgr::vmo_of(file).map(|vmo| Some((vmo, file.len())))
    })
    .unwrap_or(Ok(None))
}

/// The node a described PCI device names: its address and its identity, whole
/// (BA.3). Any other kind stops the boot until K8b's closure is what it reads.
fn node_of(described: &Described<'_>) -> Result<Arc<DeviceNode>, &'static str> {
    if described.kind() != KIND_PCI {
        return Err("a device-tree device: its closure is not read yet, so it is refused");
    }
    let wanted = described.bdf();
    let node = device::devices()
        .iter()
        .find(|node| {
            let info = node.describe();
            matches!(node.location(), Location::Pci(_))
                && info.location == wanted
                && u32::from(info.vendor_id) | u32::from(info.device_id) << 16 == described.ids()
        })
        .ok_or("a device matches no node")?;
    // DMA that nothing translates is in reach of everything, and a function behind an
    // aliasing bridge shares its requester id: `iommu::would_translate` reads false for both
    // (`ferrix_pci::topology::claims`). On an `SMMUv3` the stream is requester arithmetic and
    // `domain_placed` asks no `claims`, so an own domain is no proof there; this test and the
    // two below are what refuse an aliased function (ledger 7166 F.2).
    let Location::Pci(function) = node.location() else {
        return Err("a device-tree device: its closure is not read yet, so it is refused");
    };
    if node.reaches_memory() && !crate::iommu::would_translate(function) {
        return Err("a device with no domain of its own: its DMA, or its requester id, is shared");
    }
    // Any other function on its bus can reach it peer to peer below the bridge, where no
    // translation stands, ACS or not, and a slot's functions may share a requester id:
    // refused, not read. Across root ports the root complex decides, AoU-45.
    let bus = wanted >> 8;
    if device::devices().iter().any(|other| {
        let info = other.describe();
        matches!(other.location(), Location::Pci(_))
            && info.location >> 8 == bus
            && info.location != wanted
    }) {
        return Err("a function that shares its bus with another");
    }
    // Inside a switch, peer-to-peer turns below any translation, from another bus.
    if !crate::iommu::directly_below_root_port(function) {
        return Err("a device not directly below a root port");
    }
    Ok(Arc::clone(node))
}

/// Start the partition manager, if the image describes partitions, and wait for
/// READY. With no description it is a machine with no safety partition: said in
/// a line. A malformed one stops the boot (BA.2).
fn start_manager() -> Option<Served> {
    match try_start_manager() {
        Ok(served) => served,
        Err(why) => fatal!(catalog::PARTITION_DESCRIPTION, "first processes: {why}"),
    }
}

/// The other ends of a file server's channels, for `devmgr` and `linuxd`.
struct Served {
    block_cores: Option<Arc<Endpoint>>,
    files: Option<Arc<Endpoint>>,
    /// Further Linux partitions: each one's image in the archive and its memory.
    further: Vec<crate::linuxd_start::Further>,
    /// `netd`'s end of each further partition's cores.
    netd_ends: Vec<Arc<Endpoint>>,
}

/// The manager's and its partitions' promise, one entry (K12's one level), each partition's
/// job drawing on it by inheritance; a file server keeps its heap there too (§1, K16 §4).
#[cfg(ferrix_kernel_memory = "retyped")]
fn promise_for_partitions(job: &job::Job, parsed: &Description<'_>) -> Result<(), &'static str> {
    const MANAGER_OWN: u64 = 16 << 20;
    const FILE_SERVER: u64 = 128 << 20;
    let mut bytes = MANAGER_OWN;
    for partition in parsed.partitions() {
        if partition.role() == ROLE_FILES {
            bytes += FILE_SERVER;
        } else if partition.role() != ROLE_LINUX {
            bytes += partition.reserve_pages().saturating_mul(PAGE_SIZE);
        }
    }
    job.set_reservation(bytes)
        .map_err(|_| "no promise for the partitions")
}

/// What the manager's first message carries, onto `transfers`, in the order of its row:
/// its job, the debug log, the description, the images, the devices kept, and a file
/// server's channels. Answers how many images and devices, and what stays the kernel's.
fn manager_transfers(
    parsed: &Description<'_>,
    job: &Arc<job::Job>,
    description: Arc<Vmo>,
    transfers: &mut Vec<(Object, Rights)>,
) -> Result<(u32, u32, Served), &'static str> {
    fallible::push_within(
        transfers,
        (
            Object::Job(Arc::clone(job)),
            const { MANAGER.of(Held::Job) },
        ),
    )
    .map_err(|_| TOO_MANY)?;
    fallible::push_within(
        transfers,
        (Object::DebugLog, const { MANAGER.of(Held::DebugLog) }),
    )
    .map_err(|_| TOO_MANY)?;
    fallible::push_within(
        transfers,
        (
            Object::Vmo(description),
            const { MANAGER.of(Held::Description) },
        ),
    )
    .map_err(|_| TOO_MANY)?;
    let mut images = 0_u32;
    for partition in parsed.partitions() {
        let (image, _) =
            sealed_file(partition.image())?.ok_or("a partition's image is not in the archive")?;
        fallible::push_within(
            transfers,
            (Object::Vmo(image), const { MANAGER.of(Held::Image) }),
        )
        .map_err(|_| TOO_MANY)?;
        images += 1;
    }
    let mut devices = 0_u32;
    for partition in parsed.partitions() {
        for described in partition.devices() {
            let node = node_of(&described)?;
            // Held to K14's grant from here, whatever the profile (ledger 6564 Y.1, 6803 (iv)).
            if described.flags() & ferrix_partition_desc::FLAG_NEEDS_CERTIFIED != 0 {
                node.require_certified();
                println!("  certified a description holds a device to K14's grant");
            }
            fallible::try_push(&mut HELD.lock(), Arc::clone(&node)).map_err(|_| "no memory")?;
            fallible::push_within(
                transfers,
                (Object::Device(node), const { MANAGER.of(Held::DeviceKept) }),
            )
            .map_err(|_| TOO_MANY)?;
            devices += 1;
        }
    }
    // A file server's channels: one end is for it, through the manager.
    let mut served = Served {
        block_cores: None,
        files: None,
        further: Vec::new(),
        netd_ends: Vec::new(),
    };
    for partition in parsed
        .partitions()
        .filter(|partition| partition.role() == ROLE_LINUX)
    {
        let (linux_end, netd_end) =
            Endpoint::pair().map_err(|_| "no memory for a partition's channel")?;
        fallible::try_push(&mut served.netd_ends, netd_end).map_err(|_| "no memory")?;
        let further = crate::linuxd_start::Further::of(
            partition.image(),
            partition.reserve_pages(),
            linux_end,
        )
        .ok_or("a further partition's image name")?;
        fallible::try_push(&mut served.further, further).map_err(|_| "no memory")?;
    }
    if parsed
        .partitions()
        .any(|partition| partition.role() == ROLE_FILES)
    {
        if !cfg!(ferrix_fs = "server") {
            return Err("a file server in a description of a kernel that has its own");
        }
        let (block_cores, server_cores) =
            Endpoint::pair().map_err(|_| "no memory for the block cores' channel")?;
        let (files, server_files) =
            Endpoint::pair().map_err(|_| "no memory for the files' channel")?;
        for end in [server_cores, server_files] {
            fallible::push_within(
                transfers,
                (Object::Channel(end), const { MANAGER.of(Held::Channel) }),
            )
            .map_err(|_| TOO_MANY)?;
        }
        served.block_cores = Some(block_cores);
        served.files = Some(files);
    }
    Ok((images, devices, served))
}

fn try_start_manager() -> Result<Option<Served>, &'static str> {
    let Some((description, _)) = sealed_file(DESCRIPTION)? else {
        println!("  partmgr  no description in the image: no safety partition, no manager");
        return Ok(None);
    };
    // Read back whole from the archive, and parsed by the one parser.
    let bytes = crate::mm::with_initrd(|archive| {
        ferrix_cpio::file_named(archive, DESCRIPTION)
            .and_then(|file| fallible::try_to_vec(file).ok())
    })
    .flatten()
    .ok_or("the description could not be read")?;
    let parsed = Description::parse(&bytes).map_err(|_| "the description is malformed")?;
    let mut transfers = Vec::new();
    fallible::try_reserve(&mut transfers, CHANNEL_MAX_HANDLES).map_err(|_| "no memory")?;
    let job = job::root()
        .new_child_as(false, true)
        .map_err(|_| "no job for the partitions")?;
    job.make_measured();
    must_be_measured(&job);
    #[cfg(ferrix_kernel_memory = "retyped")]
    promise_for_partitions(&job, &parsed)?;
    let (images, devices, served) = manager_transfers(&parsed, &job, description, &mut transfers)?;
    MANAGER.judge(transfers.iter().map(|(object, rights)| (object, *rights)))?;
    let (kernel_end, manager_end) = Endpoint::pair().map_err(|_| "no memory for its channel")?;
    let body = fallible::try_to_vec(&first::encode(images, devices)).map_err(|_| "no memory")?;
    let handles = transfers.len();
    kernel_end
        .write(Sender::KERNEL, body, handles, || {
            Ok::<_, Infallible>(transfers)
        })
        .map_err(|_| "its first message could not be written")?;
    let (host, core) = first_process(&job, MANAGER_PATH)?;
    if !core.own_space().is_measured() {
        not_measured("the manager's space is not measured");
    }
    let bootstrap = core        // FALLIBLE: the handle table's insert hands the object back.
        .with_handles(|table| table.insert(Object::Channel(manager_end), Rights::CHANNEL))
        .map_err(|_| "no room for its bootstrap")?;
    host.bare
        .start(host.at.entry, host.at.stack, u64::from(bootstrap.0))?;
    watch_manager(Arc::clone(&host.bare));
    let deadline = crate::timer::now_nanos().saturating_add(match parsed.ready_millis() {
        0 => READY_PATIENCE_NANOS,
        millis => u64::from(millis).saturating_mul(NANOS_PER_MILLI),
    });
    loop {
        match kernel_end.read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => {
                crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
                if message.bytes.get(..4) == Some(&READY_MAGIC.to_le_bytes()[..]) {
                    println!("  partmgr  ready: {images} partitions, {devices} devices held");
                    // AUDIT: never dropped, a leak of one endpoint once a boot. The
                    // manager lives as long as the machine, and dropping this end would
                    // close its channel: its peer closing is not its end, the watch is.
                    let _kept_for_good = Arc::into_raw(kernel_end);
                    return Ok(Some(served));
                }
            }
            Err(ReadError::Empty) => {
                if crate::timer::now_nanos() > deadline {
                    return Err("the manager said nothing in time");
                }
                crate::sched::sleep_for(10_000_000);
            }
            // A manager that ended is the watch's to say, FX-0954, and it runs at once;
            // one still alive that closed its channel is this.
            Err(_) => {
                crate::sched::sleep_for(1_000_000_000);
                return Err("the manager closed its channel before READY");
            }
        }
    }
}

/// A process started by the kernel: a measured space with the image in it, and its
/// digest on its record.
struct First {
    bare: Arc<Bare>,
    at: EntryPoint,
}

fn first_process(
    job: &Arc<job::Job>,
    path: &[u8],
) -> Result<(First, Arc<process::Process>), &'static str> {
    let (image, len) = sealed_file(path)?.ok_or("the manager's image is not in the archive")?;
    let space = AddressSpace::new().map_err(|_| "no memory for a space")?;
    space.measure();
    let at = load_sealed(&space, &image, len)?;
    let bare = Bare::new(space, Arc::clone(job), None).map_err(|_| "no memory for a process")?;
    let core = Arc::clone(bare.core_arc());
    Ok((First { bare, at }, core))
}

/// The kernel's own loader into a measured space, K14 §3's named exception: "executable
/// only from sealed" holds of what a program asks for, and a first process has no
/// creator to ask (ledger 6316 P1.3, L.object.334). Its pages are anonymous copies, made by
/// the kernel, of the bytes of the sealed `image`, read from that VMO and no other copy,
/// so that the digest on the space's record is of what runs.
pub(crate) fn load_sealed(
    space: &AddressSpace,
    image: &Vmo,
    len: usize,
) -> Result<EntryPoint, &'static str> {
    let digest = image.digest().ok_or("the image was not measured")?;
    let mut bytes = vmap::Buffer::zeroed(len).map_err(|_| "no memory for the image")?;
    for (index, page) in bytes.chunks_mut(PAGE_SIZE as usize).enumerate() {
        image
            .read_page(index as u64, 0, page)
            .map_err(|_| "the image would not read")?;
    }
    let at = first_image::load(space, &bytes).map_err(|_| "the image was refused")?;
    space
        .note_image(digest)
        .map_err(|_| "no room for its digest")?;
    Ok(at)
}

/// Whether the manager's watch has seen it end, for the check.
static MANAGER_ENDED: AtomicBool = AtomicBool::new(false);

/// The kernel's watch on the manager: its end is the machine's safe state.
fn watch_manager(bare: Arc<Bare>) {
    static WATCHED: SpinLock<Option<Arc<Bare>>> = SpinLock::new(None);
    *WATCHED.lock() = Some(bare);
    fn watch(_argument: usize) {
        let Some(bare) = WATCHED.lock().clone() else {
            return;
        };
        let core = bare.core();
        let _ = core.exited().wait_trusting(|| core.is_terminated());
        crate::audit::first_process_ended(core);
        MANAGER_ENDED.store(true, Ordering::Release);
        fatal!(
            catalog::PARTITION_MANAGER_ENDED,
            "the partition manager ended with {:?}",
            core.exit_status()
        );
    }
    if crate::sched::spawn("manager watch", watch, 0, ferrix_sched::NICE_0_WEIGHT).is_err() {
        fatal!(catalog::FIRST_PROCESSES, "no task to watch the manager on");
    }
}

/// The certified list's path in the archive.
const CERTIFIED: &[u8] = b"etc/ferrix/certified";

/// Read the certified list, once, before any process runs (K14 §5). A malformed
/// one is an empty list and a line, never a stop: a list names what may hold a
/// grant, so an empty one grants nothing.
fn read_certified_list() {
    let Some(Some(bytes)) = crate::mm::with_initrd(|archive| {
        ferrix_cpio::file_named(archive, CERTIFIED).and_then(|file| fallible::try_to_vec(file).ok())
    }) else {
        return;
    };
    let Some(list) = ferrix_partition_desc::certified::parse(&bytes) else {
        println!("  certified the list is malformed: nothing is on it");
        return;
    };
    let mut digests = Vec::new();
    if fallible::try_reserve(&mut digests, list.count()).is_err() {
        println!("  certified no memory for the list: nothing is on it");
        return;
    }
    for digest in list.digests() {
        let _ = fallible::push_within(&mut digests, digest);
    }
    println!("  certified {} images on the list", digests.len());
    // NOALLOC: `try_reserve` above made the capacity the count, so nothing moves.
    crate::user::space::certified::set(digests.into_boxed_slice());
}

/// Hold to K14's grant every node of a certified profile whose DMA nothing would
/// translate, before any handle exists (K14.4, G.5). An over-approximation of
/// "DMA-capable": every node but the two the kernel synthesises. `fast` and `max` flag them
/// from now, at level none until M7 too; another profile flags none, whatever
/// `ferrix.unisolated` says.
fn flag_nodes() {
    if !cfg!(any(
        ferrix_level = "sc2",
        ferrix_level = "sc4",
        ferrix_profile = "fast",
        ferrix_profile = "max",
    )) {
        return;
    }
    let mut flagged = 0_usize;
    for node in device::devices() {
        let translated = match node.location() {
            Location::Pci(function) => crate::iommu::would_translate(function),
            Location::VirtioMmio(_) | Location::Tree(_) => false,
        };
        if node.wants_certified(translated) {
            node.require_certified();
            flagged += 1;
        }
    }
    println!("  certified {flagged} nodes held to K14's grant by this profile");
}
