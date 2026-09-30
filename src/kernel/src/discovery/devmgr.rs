//! Starting `devmgr`, and what the kernel says to it: `docs/DEVMGR.md` §2.
//!
//! After the boot checks and before `init`, the kernel reads `/sbin/devmgr`
//! and every driver the initramfs lists in `/lib/drivers/MANIFEST` out of the
//! root, puts each driver's image in an anonymous VMO, and starts `devmgr`
//! as a native process whose bootstrap channel already holds one DEVICES
//! message: a job, every device node twice, and the images by name. It then
//! waits for `devmgr`'s REPORT, which says what it started, and prints it.
//! From then on the kernel's end of the channel carries PUBLISHED to
//! `devmgr` whenever a ring on a device accepts a HELLO, and DIED from it,
//! which a task of the kernel's prints.
//!
//! The kernel reads the images; it never runs them. An image is memory
//! `process_create` loads, never a file mapping, which is what keeps a driver
//! from ever faulting on the disk it serves (`docs/DEVMGR.md` §5).
//!
//! The images are read through a [`ReadFile`] the filesystem registers at
//! bring-up, not by naming the filesystem: `devmgr` is part of the certified
//! item and the filesystem is not (`docs/certification/ITEM.md`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use ferrix_blkring::Location;
use ferrix_blkring::control::DEVICE_RIGHTS;
use ferrix_bootinfo::{BootView, PAGE_SIZE, option_in};
use ferrix_devmgr_proto::{
    ANSWER_BUSY, ANSWER_DONE, ANSWER_FAILED, ANSWER_NO_DEVICE, BUS_PCI, BUS_PLATFORM,
    DEVICES_MAX_BYTES, Devices, Message, NAME_BYTES, SHORT_BYTES,
};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES};
use ferrix_sync::Once;

use crate::fallible::{self, AllocError};
use crate::object::channel::{Endpoint, ReadError};
use crate::object::job::{self, Job};
use crate::object::process::{Exit, Host, Process, ProcessRef};
use crate::object::{Object, Transfer};
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::native::{self, StartRefused};
use crate::user::vmo::Vmo;
use crate::{device, sched, timer};

/// The program.
const PROGRAM: &[u8] = b"/sbin/devmgr";
/// Where the drivers are, and the file that lists them, one name per line.
const DRIVERS: &[u8] = b"/lib/drivers";
const MANIFEST: &[u8] = b"/lib/drivers/MANIFEST";
/// What `devmgr` is listed as.
const NAME: &[u8] = b"devmgr";

/// How long `devmgr` has to start every driver and report: a driver resets
/// and negotiates with its device, and the kernel's ring task publishes on
/// HELLO; generous under TCG.
const REPORT_PATIENCE_NANOS: u64 = 20_000_000_000;

/// The rights of the device handle `devmgr` keeps, the second of each pair:
/// a driver's, and `DUPLICATE`, so a driver that died can be started again
/// with a handle of its own while `devmgr` keeps this one for the next
/// quiesce (`docs/DEVMGR.md` §4).
const KEPT_DEVICE_RIGHTS: Rights = Rights(DEVICE_RIGHTS.0 | Rights::DUPLICATE.0);

/// Read a whole file from the root the initramfs was unpacked into: how
/// `devmgr`'s program, its drivers and their manifest are read.
///
/// The filesystem's to answer and the load ring's to register, which it does
/// from `main.rs` before [`start`] runs.
pub(crate) type ReadFile = fn(&[u8]) -> Result<Vec<u8>, Errno>;

/// The registered [`ReadFile`].
static READ_FILE: Once<ReadFile> = Once::new();

/// Read `devmgr`'s files with `read`. The first registration stands.
pub(crate) fn register_reader(read: ReadFile) {
    let _ = READ_FILE.call_once(|| read);
}

/// The [`ReadFile`] of a kernel whose filesystem registered none, which has
/// no `/sbin/devmgr` to start. `main.rs` checks that a boot of this kernel is
/// not one.
fn no_files(_path: &[u8]) -> Result<Vec<u8>, Errno> {
    Err(Errno::ENOENT)
}

/// Whether a [`ReadFile`] is registered: the boot's check that [`start`]
/// has something to read `devmgr` with.
pub(crate) fn has_reader() -> bool {
    READ_FILE.get().is_some()
}

/// The kernel's end of `devmgr`'s bootstrap channel, once it is started.
static CHANNEL: SpinLock<Option<Arc<Endpoint>>> = SpinLock::new(None);

/// How long a write to `bind` or `unbind` waits for `devmgr`'s DONE: a bind
/// starts a driver and waits for it to publish, which takes seconds under
/// TCG, as REPORT does.
const REQUEST_PATIENCE_NANOS: u64 = REPORT_PATIENCE_NANOS;

/// The bus a driver drives devices on, as sysfs lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bus {
    /// PCI functions: `/sys/bus/pci`.
    Pci,
    /// Device tree nodes: `/sys/bus/platform`.
    Platform,
}

/// What `devmgr` has said about drivers: which it has, and which drives
/// which device. `devmgr` owns these facts -- its table matches devices to
/// drivers, and it starts and stops them -- and the kernel only keeps what it
/// was told, for sysfs to show (`docs/SYSFS.md` §3).
#[derive(Debug)]
struct Bindings {
    /// The drivers the manifest names, in DEVICES' order: a driver's number
    /// in every message is its place here.
    names: Vec<[u8; NAME_BYTES]>,
    /// The drivers `devmgr` said it can start, each with its bus.
    drivers: Vec<(u16, Bus)>,
    /// Which driver drives which device, by the device's index in
    /// `device::devices()`.
    bound: Vec<(usize, u16)>,
}

static BINDINGS: SpinLock<Bindings> = SpinLock::new(Bindings {
    names: Vec::new(),
    drivers: Vec::new(),
    bound: Vec::new(),
});

/// Requests sent and not yet collected: each token, and `devmgr`'s answer
/// once DONE has come.
static PENDING: SpinLock<Vec<(u16, Option<u32>)>> = SpinLock::new(Vec::new());

/// Woken whenever an answer arrives, or `devmgr` goes.
static ANSWERED: WaitQueue = WaitQueue::new();

/// The next request's token.
static NEXT_TOKEN: AtomicU16 = AtomicU16::new(1);

/// A driver's name, as the manifest gave it: a fixed array, so handing one
/// out allocates nothing. Reads as the name without its padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DriverName([u8; NAME_BYTES]);

impl core::ops::Deref for DriverName {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        name_bytes(&self.0)
    }
}

/// The drivers `devmgr` can start on `bus`: each one's number and name.
///
/// # Errors
///
/// [`AllocError`] when there is no memory for the list.
pub(crate) fn drivers_on(bus: Bus) -> Result<Vec<(u16, DriverName)>, AllocError> {
    let bindings = BINDINGS.lock();
    fallible::try_collect(
        bindings
            .drivers
            .iter()
            .filter(|(_, on)| *on == bus)
            .filter_map(|&(driver, _)| {
                let name = bindings.names.get(usize::from(driver))?;
                Some((driver, DriverName(*name)))
            }),
    )
}

/// The driver that drives the device with index `device`, if one does.
pub(crate) fn driver_of(device: usize) -> Option<(u16, DriverName)> {
    let bindings = BINDINGS.lock();
    let (_, driver) = bindings.bound.iter().find(|(at, _)| *at == device)?;
    let name = bindings.names.get(usize::from(*driver))?;
    Some((*driver, DriverName(*name)))
}

/// The devices `driver` drives, by index, ascending.
///
/// # Errors
///
/// [`AllocError`] when there is no memory for the list.
pub(crate) fn bound_to(driver: u16) -> Result<Vec<usize>, AllocError> {
    let mut devices: Vec<usize> = fallible::try_collect(
        BINDINGS
            .lock()
            .bound
            .iter()
            .filter(|(_, by)| *by == driver)
            .map(|(device, _)| *device),
    )?;
    devices.sort_unstable();
    Ok(devices)
}

/// What a write to `bind` or `unbind` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    /// Start the driver on the device.
    Bind,
    /// Stop the driver the device has.
    Unbind,
}

/// Ask `devmgr` to bind `driver` to the device with index `device`, or to
/// unbind it, and wait for the answer. The caller has checked what Linux
/// checks before it asks a driver anything: that the device exists on the
/// driver's bus, and for an unbind that this driver drives it.
///
/// # Errors
///
/// `ENODEV` when `devmgr` will not -- no such device, or the driver does not
/// take it -- or is not there; `EBUSY` when the device already has a
/// driver; `EIO` when the driver was started and did not publish, or
/// `devmgr` went; `ETIMEDOUT` when no answer came within the patience.
pub(crate) fn request(kind: Request, device: usize, driver: u16) -> Result<(), Errno> {
    let channel = CHANNEL.lock().clone().ok_or(Errno::ENODEV)?;
    let wire_device = u32::try_from(device).map_err(|_| Errno::ENODEV)?;
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let message = match kind {
        Request::Bind => Message::Bind {
            device: wire_device,
            driver,
            token,
        },
        Request::Unbind => Message::Unbind {
            device: wire_device,
            driver,
            token,
        },
    };
    let bytes = fallible::try_to_vec(&message.encode()).map_err(|_| Errno::ENOMEM)?;
    fallible::try_push(&mut PENDING.lock(), (token, None)).map_err(|_| Errno::ENOMEM)?;
    let sent = channel.write(bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
    let answered = sent.is_ok()
        && ANSWERED.wait_until_deadline(
            || {
                PENDING
                    .lock()
                    .iter()
                    .any(|(at, answer)| *at == token && answer.is_some())
            },
            timer::now_nanos().saturating_add(REQUEST_PATIENCE_NANOS),
        );
    let answer = {
        let mut pending = PENDING.lock();
        let at = pending.iter().position(|(at, _)| *at == token);
        at.map(|at| pending.remove(at))
            .and_then(|(_, answer)| answer)
    };
    let result = match (sent, answered, answer) {
        (Err(_), _, _) => Err(Errno::EIO),
        (Ok(_), false, _) => Err(Errno::ETIMEDOUT),
        (Ok(_), true, Some(ANSWER_DONE)) => Ok(()),
        (Ok(_), true, Some(ANSWER_NO_DEVICE)) => Err(Errno::ENODEV),
        (Ok(_), true, Some(ANSWER_BUSY)) => Err(Errno::EBUSY),
        (Ok(_), true, _) => Err(Errno::EIO),
    };
    report_request(kind, device, driver, result);
    result
}

/// Say on the console how a request through sysfs went.
fn report_request(kind: Request, device: usize, driver: u16, result: Result<(), Errno>) {
    let what = match kind {
        Request::Bind => "bind",
        Request::Unbind => "unbind",
    };
    let at = device::devices().get(device).map(|node| node.location());
    let name = BINDINGS
        .lock()
        .names
        .get(usize::from(driver))
        .map_or(DriverName([0; NAME_BYTES]), |name| DriverName(*name));
    let name = core::str::from_utf8(&name).unwrap_or("?");
    match (at, result) {
        (Some(at), Ok(())) => crate::console::println!(
            "  devmgr   {what} of {name} and the device at {at}, asked through sysfs: done"
        ),
        (Some(at), Err(errno)) => {
            let said = match errno {
                Errno::ENODEV => "ENODEV",
                Errno::EBUSY => "EBUSY",
                Errno::ETIMEDOUT => "ETIMEDOUT",
                _ => "EIO",
            };
            crate::console::println!(
                "  devmgr   {what} of {name} and the device at {at}, asked through sysfs: {said}"
            );
        }
        (None, _) => {}
    }
}

/// Reports from `devmgr` that there was no memory to record: sysfs does not
/// show those bindings.
static UNRECORDED: AtomicU64 = AtomicU64::new(0);

/// Take in what a message from `devmgr` says about drivers. Answers whether
/// it was such a message.
fn record(message: Message) -> bool {
    match message {
        Message::Driver { driver, bus } => {
            let bus = match bus {
                BUS_PCI => Bus::Pci,
                BUS_PLATFORM => Bus::Platform,
                _ => return true,
            };
            let Ok(driver) = u16::try_from(driver) else {
                return true;
            };
            let mut bindings = BINDINGS.lock();
            if !bindings.drivers.iter().any(|(at, _)| *at == driver)
                && fallible::try_push(&mut bindings.drivers, (driver, bus)).is_err()
            {
                // sysfs will not list the driver: what memory running out
                // costs here, rather than the listener stopping.
                let _ = UNRECORDED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Message::Bound { device, driver } => {
            let (Ok(device), Ok(driver)) = (usize::try_from(device), u16::try_from(driver)) else {
                return true;
            };
            let mut bindings = BINDINGS.lock();
            bindings.bound.retain(|(at, _)| *at != device);
            if fallible::try_push(&mut bindings.bound, (device, driver)).is_err() {
                let _ = UNRECORDED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Message::Unbound { device } => {
            if let Ok(device) = usize::try_from(device) {
                BINDINGS.lock().bound.retain(|(at, _)| *at != device);
            }
        }
        Message::Done { token, answer } => {
            if let Some((_, slot)) = PENDING.lock().iter_mut().find(|(at, _)| *at == token) {
                *slot = Some(answer);
            }
            ANSWERED.wake_all();
        }
        _ => return false,
    }
    true
}

/// `devmgr` is gone: nothing it said about drivers holds any longer, and no
/// request will be answered.
fn forget_devmgr() {
    {
        let mut bindings = BINDINGS.lock();
        bindings.drivers.clear();
        bindings.bound.clear();
    }
    for (_, answer) in PENDING.lock().iter_mut() {
        if answer.is_none() {
            *answer = Some(ANSWER_FAILED);
        }
    }
    ANSWERED.wake_all();
}

/// What `devmgr` reported, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// Device nodes handed over.
    pub(crate) devices: usize,
    /// Driver images handed over.
    pub(crate) drivers: usize,
    /// Drivers started whose disks the kernel accepted.
    pub(crate) started: u32,
    /// Devices that matched a driver and got none.
    pub(crate) failed: u32,
}

/// Start `devmgr` and wait for its REPORT. `Ok(None)` when the image carries
/// no `/sbin/devmgr`, which a boot without native programs is.
///
/// # Errors
///
/// What did not happen, as a sentence: `devmgr` could not be started, or
/// said nothing in time, or something other than REPORT.
pub(crate) fn start() -> Result<Option<Report>, &'static str> {
    let Some(files) = read_files()? else {
        return Ok(None);
    };
    let drivers = files.names.len();
    let (kernel_end, devmgr_end) =
        Endpoint::pair().map_err(|_| "no memory for devmgr's channel")?;
    let devices = write_devices(&kernel_end, files.images, drivers_job()?)?;
    let process = load(&files.image)?;
    start_program(&process, devmgr_end, &kernel_end)?;

    let (started, failed) = report(&kernel_end)?;
    if sched::spawn("devmgr listener", listen, 0, ferrix_sched::NICE_0_WEIGHT).is_err() {
        return Err("no task to hear devmgr on");
    }
    Ok(Some(Report {
        devices,
        drivers,
        started,
        failed,
    }))
}

/// What `devmgr` is started with: its own program, and every driver the
/// manifest names, by name and in a VMO of its own.
struct Files {
    /// `/sbin/devmgr`.
    image: Vec<u8>,
    /// The manifest's names, in its order.
    names: Vec<[u8; NAME_BYTES]>,
    /// Each named driver's image, in the same order.
    images: Vec<Arc<Vmo>>,
}

/// Read `devmgr`, the manifest and every driver it names through the
/// registered reader, and note the names for sysfs. `Ok(None)` without
/// `/sbin/devmgr`.
fn read_files() -> Result<Option<Files>, &'static str> {
    let read = READ_FILE.get().copied().unwrap_or(no_files);
    let Ok(image) = read(PROGRAM) else {
        return Ok(None);
    };
    let names = manifest(read)?;
    {
        let mut bindings = BINDINGS.lock();
        bindings.names.clear();
        fallible::try_extend_from_slice(&mut bindings.names, &names)
            .map_err(|_| "no memory for the drivers' names")?;
    }
    let mut images = fallible::try_with_capacity(names.len())
        .map_err(|_| "no memory for the drivers' images")?;
    for name in &names {
        let mut path = fallible::try_with_capacity(DRIVERS.len() + 1 + NAME_BYTES)
            .map_err(|_| "no memory for a driver's path")?;
        // NOALLOC: the room for all three was had just above.
        path.extend_from_slice(DRIVERS);
        // NOALLOC: the room for all three was had just above.
        path.push(b'/');
        // NOALLOC: the room for all three was had just above.
        path.extend_from_slice(name_bytes(name));
        let bytes = read(&path).map_err(|_| "a driver the manifest names is not in the image")?;
        let _ = fallible::push_within(&mut images, vmo_of(&bytes)?);
    }
    Ok(Some(Files {
        image,
        names,
        images,
    }))
}

/// Write the DEVICES messages on `kernel_end`: `job` for the drivers, every
/// device node twice, and `images` by the names [`read_files`] noted.
/// Answers how many device nodes went.
fn write_devices(
    kernel_end: &Endpoint,
    images: Vec<Arc<Vmo>>,
    job: Arc<Job>,
) -> Result<usize, &'static str> {
    let names = {
        let bindings = BINDINGS.lock();
        fallible::try_to_vec(&bindings.names).map_err(|_| "no memory for the drivers' names")?
    };
    let nodes = device::devices();
    // As many DEVICES messages as the handles need: the first with the job
    // and the images, the rest with devices only. ARMv7-A publishes 36
    // device nodes, and a message carries 64 handles.
    let mut images = Some(images);
    let mut job = Some(job);
    let mut index = 0;
    let mut first = true;
    while first || index < nodes.len() {
        let extra = if first { 1 + names.len() } else { 0 };
        let room = CHANNEL_MAX_HANDLES.saturating_sub(extra) / 2;
        let take = room.min(nodes.len() - index);
        let message = Devices {
            devices: u32::try_from(take).map_err(|_| "too many devices")?,
            more: u32::try_from(nodes.len() - index - take).map_err(|_| "too many devices")?,
            first,
            names: if first { &names } else { &[] },
        };
        let mut bytes = [0_u8; DEVICES_MAX_BYTES];
        let len = message
            .encode(&mut bytes)
            .ok_or("more drivers than one DEVICES message names")?;
        let mut transfers: Vec<Transfer> = fallible::try_with_capacity(message.handles())
            .map_err(|_| "no memory for a DEVICES message's handles")?;
        if let Some(job) = job.take() {
            let _ = fallible::push_within(&mut transfers, (Object::Job(job), Rights::JOB));
        }
        for node in nodes.iter().skip(index).take(take) {
            let _ = fallible::push_within(
                &mut transfers,
                (Object::Device(Arc::clone(node)), DEVICE_RIGHTS),
            );
            let _ = fallible::push_within(
                &mut transfers,
                (Object::Device(Arc::clone(node)), KEPT_DEVICE_RIGHTS),
            );
        }
        if let Some(images) = images.take() {
            for vmo in images {
                let _ = fallible::push_within(
                    &mut transfers,
                    (
                        Object::Vmo(vmo),
                        Rights(Rights::READ.0 | Rights::TRANSFER.0),
                    ),
                );
            }
        }
        let handles = transfers.len();
        let body = fallible::try_to_vec(bytes.get(..len).unwrap_or(&[]))
            .map_err(|_| "no memory for a DEVICES message")?;
        kernel_end
            .write(body, handles, || Ok::<Vec<Transfer>, Infallible>(transfers))
            .map_err(|_| "DEVICES could not be written to devmgr's channel")?;
        index += take;
        first = false;
    }
    Ok(nodes.len())
}

/// Load `devmgr`'s `image` into a process of its own, not started. It runs
/// as root whoever asked (`native::LoadNative` with no creator): it is the
/// kernel's, however it was asked for.
fn load(image: &[u8]) -> Result<Arc<dyn Host>, &'static str> {
    let processes = native::processes().ok_or("nothing is registered to start devmgr with")?;
    (processes.load)(None, image, NAME).map_err(|_| "/sbin/devmgr does not load")
}

/// Put `devmgr_end` in `process`'s table as its bootstrap handle, and start
/// it with the handle's value.
///
/// Through what the personality registered ([`native::Processes`]): the ELF
/// loader and the process it loads into are above the item. The kernel's end
/// of the channel is published once devmgr's start is claimed and its task
/// made, and before it runs. The task runs for the life of the machine;
/// nothing here joins it.
fn start_program(
    process: &Arc<dyn Host>,
    devmgr_end: Arc<Endpoint>,
    kernel_end: &Arc<Endpoint>,
) -> Result<(), &'static str> {
    let processes = native::processes().ok_or("nothing is registered to start devmgr with")?;
    let bootstrap = process
        .core()
        // FALLIBLE: the handle table's insert hands the object back.
        .with_handles(|table| table.insert(Object::Channel(devmgr_end), Rights::CHANNEL))
        .map_err(|_| "no room for devmgr's bootstrap handle")?;
    let mut publish = || {
        *CHANNEL.lock() = Some(Arc::clone(kernel_end));
        Ok(u64::from(bootstrap.0))
    };
    (processes.start)(process, &mut publish).map_err(|why| match why {
        StartRefused::Claimed => "devmgr could not be claimed to start",
        StartRefused::NoTask | StartRefused::Argument(_) => "devmgr could not be started",
    })
}

/// The command-line option that has pid 1 start `devmgr` (`docs/INIT.md`
/// §7.3, L12): `init`. Without it, or with `kernel`, the kernel starts it at
/// bring-up, as [`start`] does -- the certified configuration
/// (`docs/certification/ITEM.md` §5).
const OPTION: &str = "ferrix.devmgr";

/// Whether the command line said `ferrix.devmgr=init`.
static BY_INIT: AtomicBool = AtomicBool::new(false);

/// Read [`OPTION`], once, early.
pub(crate) fn read_option(view: &BootView<'_>) {
    let tree = crate::discovery::fdt::open(view).ok();
    let value = view.option(OPTION).or_else(|| {
        tree.as_ref()
            .and_then(|tree| option_in(tree.bootargs()?, OPTION))
    });
    match value {
        None | Some("kernel") => {}
        Some("init") => {
            BY_INIT.store(true, Ordering::Relaxed);
            crate::console::println!(
                "  devmgr   {OPTION}=init: pid 1 starts devmgr, and / waits for its disk"
            );
        }
        Some(other) => crate::console::println!(
            "  devmgr   {OPTION}={other} is not understood; the kernel starts devmgr"
        ),
    }
}

/// Whether pid 1 starts `devmgr` on this boot.
pub(crate) fn by_init() -> bool {
    BY_INIT.load(Ordering::Relaxed)
}

/// Make the drivers' job, `drivers.slice`, at bring-up as [`start`] does,
/// for the `devmgr` pid 1 starts: init runs `devmgr.service` in it, and
/// takes it, as on the kernel path, for a cgroup the kernel made.
///
/// # Errors
///
/// When there was no memory for it.
pub(crate) fn make_drivers_job() -> Result<(), &'static str> {
    let job = drivers_job()?;
    *DRIVERS_KEPT.lock() = Some(job);
    Ok(())
}

/// `drivers.slice` under `ferrix.devmgr=init`, held for the life of the
/// machine: a job's parent holds it only weakly, and on the kernel path it is
/// `devmgr` that holds it.
static DRIVERS_KEPT: SpinLock<Option<Arc<Job>>> = SpinLock::new(None);

/// Whether the one starter there is has been given out.
static STARTER_GIVEN: AtomicBool = AtomicBool::new(false);

/// The starter, for pid 1's bootstrap channel: once, and only under
/// `ferrix.devmgr=init`. `None` otherwise.
pub(crate) fn starter() -> Option<Object> {
    if !by_init() || STARTER_GIVEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    Some(Object::Starter)
}

/// The `devmgr` a starter started, and the job it was started in.
struct Running {
    /// How it ended, once it has.
    exit: Arc<Exit>,
    /// Its job, which holds its drivers' jobs.
    job: Arc<Job>,
}

/// The last `devmgr` [`devmgr_start`] started.
static RUNNING: SpinLock<Option<Running>> = SpinLock::new(None);

/// Whether the `devmgr` a starter started has reported, and the queue
/// woken when it does, or when it could not.
static REPORTED: AtomicBool = AtomicBool::new(false);
static REPORTED_QUEUE: WaitQueue = WaitQueue::new();

/// What a started `devmgr` was handed, for its REPORT line: device nodes and
/// drivers.
static HANDED: SpinLock<(usize, usize)> = SpinLock::new((0, 0));

/// Wait until the `devmgr` pid 1 started has reported, or `deadline` passes.
/// Whether it reported.
pub(crate) fn wait_reported(deadline: u64) -> bool {
    REPORTED_QUEUE.wait_until_deadline(|| REPORTED.load(Ordering::Acquire), deadline)
}

/// `devmgr_start(starter, job)`: start `devmgr` in `job` for the holder of
/// the starter, and answer a handle to the process (`docs/INIT.md` §7.3).
/// Every start is checked as it is made ([`handed_only_to_devmgr`]).
///
/// The kernel does all of it: reads `devmgr` and the drivers, writes DEVICES
/// on a channel whose other end becomes `devmgr`'s bootstrap handle, makes
/// the process in `job` and starts it. The caller gets
/// [`Rights::PROCESS`] on the process, which starts, waits and reads how it
/// ended and reaches nothing inside it: no call reads or writes its memory
/// or takes its handles, and `process_start` on it is `BAD_STATE`, since it
/// has started. `ALREADY_BOUND` while a `devmgr` it started lives;
/// `BAD_STATE` after, until every process in that `devmgr`'s job has ended,
/// and then every device is quiesced before the next one is handed them.
pub(crate) fn devmgr_start(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [starter, job, ..] = *registers;
    let core = caller.core();
    let job = starter_and_job(core, starter, job)?;
    may_start_again()?;
    let drivers = job.id();
    let started = start_in(job)?;
    let handle = native::insert_new(core, Object::Process(started.created), Rights::PROCESS)?;
    if let Err(problem) = handed_only_to_devmgr(caller, &started.process, &started.end, handle) {
        crate::panic::fatal!(
            crate::panic::catalog::DEVMGR_BY_INIT,
            "devmgr started by pid 1 self-check failed: {problem}"
        );
    }
    crate::audit::record(
        crate::audit::DEVMGR_STARTED,
        crate::audit::Outcome::Done,
        0,
        crate::audit::Subject::of(core),
        crate::audit::Target {
            kind: crate::audit::target::PROCESS,
            id: u64::from(started.process.core().pid()),
        },
        [drivers as u32, (drivers >> 32) as u32, 0],
    );
    Ok(handle)
}

/// The job `devmgr_start` names, when the caller holds the starter with
/// `MANAGE` and the job with `MANAGE`.
fn starter_and_job(core: &Process, starter: u64, job: u64) -> Result<Arc<Job>, Errno> {
    core.with_handles(|table| {
        match table.get(Handle::from_register(starter)) {
            Ok((Object::Starter, rights)) if rights.contains(Rights::MANAGE) => {}
            Ok((Object::Starter, _)) => return Err(status::ACCESS_DENIED),
            Ok(_) => return Err(status::WRONG_TYPE),
            Err(_) => return Err(status::BAD_HANDLE),
        }
        match table.get(Handle::from_register(job)) {
            Ok((Object::Job(job), rights)) if rights.contains(Rights::MANAGE) => {
                Ok(Arc::clone(job))
            }
            Ok((Object::Job(_), _)) => Err(status::ACCESS_DENIED),
            Ok(_) => Err(status::WRONG_TYPE),
            Err(_) => Err(status::BAD_HANDLE),
        }
    })
}

/// A `devmgr` [`start_in`] started.
struct Started {
    /// The process.
    process: Arc<dyn Host>,
    /// What the caller's handle to it is made from.
    created: ProcessRef,
    /// Its end of the DEVICES channel, for the check.
    end: Arc<Endpoint>,
}

/// Read `devmgr` and its drivers, write DEVICES, make `devmgr` in `job` and
/// start it, and start the task that hears it.
fn start_in(job: Arc<Job>) -> Result<Started, Errno> {
    let files = read_files()
        .map_err(|_| status::NO_MEMORY)?
        .ok_or(status::BAD_STATE)?;
    let drivers = files.names.len();
    let (kernel_end, devmgr_end) = Endpoint::pair().map_err(|_| status::NO_MEMORY)?;
    let devices = write_devices(&kernel_end, files.images, Arc::clone(&job))
        .map_err(|_| status::NO_MEMORY)?;
    let process = load(&files.image).map_err(|_| status::BAD_STATE)?;
    let kill = |process: &Arc<dyn Host>| process.kill(job::KILLED_STATUS);
    if process.core().move_new_to(&job).is_err() {
        kill(&process);
        return Err(status::BAD_STATE);
    }
    let exit = process.core().exit_record();
    let Ok(created) = ProcessRef::created(Arc::clone(&process)) else {
        kill(&process);
        return Err(status::NO_MEMORY);
    };
    let end = Arc::clone(&devmgr_end);
    start_program(&process, devmgr_end, &kernel_end).map_err(|_| status::BAD_STATE)?;
    *HANDED.lock() = (devices, drivers);
    REPORTED.store(false, Ordering::Release);
    let replaced = RUNNING.lock().replace(Running { exit, job });
    drop(replaced);
    let heard = sched::spawn(
        "devmgr listener",
        report_then_listen,
        0,
        ferrix_sched::NICE_0_WEIGHT,
    );
    if heard.is_err() {
        crate::console::println!("  devmgr   no task to hear devmgr on");
    }
    Ok(Started {
        process,
        created,
        end,
    })
}

/// The check `devmgr_start` makes of every start, as the caller: the
/// channel's `devmgr` end is in `devmgr`'s table and in no handle of the
/// caller's, `process_start` on the handle the caller got is `BAD_STATE` and
/// leaves that so, and a second start while it lives is `ALREADY_BOUND`.
fn handed_only_to_devmgr(
    caller: &dyn Host,
    process: &Arc<dyn Host>,
    end: &Arc<Endpoint>,
    handle: usize,
) -> Result<(), &'static str> {
    let holds = |host: &dyn Host| {
        host.core().with_handles(|table| {
            table.handles().any(|(handle, _)| {
                matches!(table.get(handle), Ok((Object::Channel(held), _)) if Arc::ptr_eq(held, end))
            })
        })
    };
    if !holds(&**process) {
        return Err("devmgr does not hold its end of the DEVICES channel");
    }
    if holds(caller) {
        return Err("the caller holds devmgr's end of the DEVICES channel");
    }
    let args = crate::trap::SyscallArgs {
        abi: crate::trap::Abi::Native,
        number: ferrix_native_abi::nr::PROCESS_START,
        args: [handle as u64, 0, 0, 0, 0, 0],
        ip: 0,
    };
    if native::dispatch(&args, Some(caller)) != Err(status::BAD_STATE) {
        return Err("process_start on the handle to a started devmgr was not BAD_STATE");
    }
    if holds(caller) {
        return Err("a call on the process handle gave the caller devmgr's channel");
    }
    if may_start_again() != Err(status::ALREADY_BOUND) {
        return Err("a second devmgr_start while devmgr lives was not ALREADY_BOUND");
    }
    if !STARTS_CHECKED.swap(true, Ordering::Relaxed) {
        crate::console::println!(
            "  devmgr   pid 1 got only a process handle: devmgr holds its DEVICES end and pid 1 \
             none, process_start on it BAD_STATE, a second start ALREADY_BOUND"
        );
    }
    Ok(())
}

/// Whether [`handed_only_to_devmgr`] has said its line.
static STARTS_CHECKED: AtomicBool = AtomicBool::new(false);

/// Refuse a second `devmgr` while the first lives, or before its drivers
/// have all ended; and before a new one, quiesce every device, as `devmgr`
/// quiesces a device whose driver died -- it cannot, having died itself.
fn may_start_again() -> Result<(), Errno> {
    {
        let running = RUNNING.lock();
        let Some(running) = running.as_ref() else {
            return Ok(());
        };
        if !running.exit.is_closed() {
            return Err(status::ALREADY_BOUND);
        }
        if !running.job.signals().intersects(Signals::EMPTY) {
            return Err(status::BAD_STATE);
        }
    }
    for node in device::devices() {
        native::quiesce(node).map_err(|_| status::BAD_STATE)?;
    }
    Ok(())
}

/// The listener of a `devmgr` pid 1 started: its REPORT, said as the
/// kernel's own start says it, then the checks that the channel went only to
/// it, then every DIED and RESTARTED after.
fn report_then_listen(_: usize) {
    let Some(channel) = CHANNEL.lock().clone() else {
        return;
    };
    let (devices, drivers) = *HANDED.lock();
    match report(&channel) {
        Ok((started, failed)) => crate::console::println!(
            "  devmgr   started by pid 1: {devices} devices, {drivers} drivers, {started} started, \
             {failed} failed"
        ),
        Err(why) => crate::console::println!("  devmgr   started by pid 1, and then: {why}"),
    }
    REPORTED.store(true, Ordering::Release);
    REPORTED_QUEUE.wake_all();
    listen(0);
}

/// The process that last mapped each device's registers or took its interrupt,
/// as how it ends, by the device's PCI location: what a check that waits for a
/// driver's work looks at to say the driver died, and how, rather than that the
/// work never came. The record outlives the process, and the latest such
/// process replaces it: a boot check's own processes use a device before
/// devmgr's driver does.
static DRIVER_ENDINGS: SpinLock<Vec<(Location, Arc<Exit>)>> = SpinLock::new(Vec::new());

/// Note that `driver` mapped `node`'s registers or took its interrupt: it is
/// that device's driver now. Cheap when it is already noted, which every call
/// after its first is.
pub(crate) fn note_driver(node: &device::DeviceNode, driver: &Process) {
    let Some(location) = location_of(node) else {
        return;
    };
    let exit = driver.exit_record();
    let replaced = {
        let mut drivers = DRIVER_ENDINGS.lock();
        match drivers.iter_mut().find(|(at, _)| *at == location) {
            Some((_, noted)) if Arc::ptr_eq(noted, &exit) => None,
            Some((_, noted)) => Some(core::mem::replace(noted, exit)),
            // Not noted when there is no memory to: a check that asks how
            // the driver ended hears nothing, and nothing else depends on it.
            None => {
                let _ = fallible::try_push(&mut drivers, (location, exit));
                None
            }
        }
    };
    // A record let go of here may be the last reference to how an old driver
    // ended, and is dropped with the table unlocked.
    drop(replaced);
}

/// The PCI location `devmgr`'s messages and every ring's HELLO name `node`
/// by, if it is a PCI function.
pub(crate) fn location_of(node: &device::DeviceNode) -> Option<Location> {
    let device::Location::Pci(address) = node.location() else {
        return None;
    };
    Some(Location::new(
        address.segment(),
        address.bus(),
        (address.device() << 3) | address.function(),
    ))
}

/// How the driver of the device at `location` ended, if one was noted and
/// has.
pub(crate) fn driver_ending(location: Location) -> Option<(i32, Option<u32>)> {
    let exit = DRIVER_ENDINGS
        .lock()
        .iter()
        .find(|(at, _)| *at == location)
        .map(|(_, exit)| Arc::clone(exit))?;
    exit.status().map(|status| (status, exit.signal()))
}

/// Tell `devmgr` that the disk of the device at `location` is published.
/// Nothing, before `devmgr` is started or if it is gone.
pub(crate) fn published(location: Location) {
    let Some(channel) = CHANNEL.lock().clone() else {
        return;
    };
    let message = Message::Published {
        location: location.raw(),
    }
    .encode();
    // Not sent when there is no memory to: `devmgr` then treats the driver as
    // one that never published, as it would a driver that failed.
    let Ok(bytes) = fallible::try_to_vec(&message) else {
        return;
    };
    let _ = channel.write(bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
}

/// The name of the job `devmgr` is given, beneath the root job:
/// `docs/CGROUPS.md` §2.1, and the slice `docs/INIT.md` §5.1 shows drivers in.
const DRIVERS_JOB: &str = "drivers.slice";

/// The job `devmgr` makes each driver's job under, named [`DRIVERS_JOB`] in
/// the root job so that cgroupfs shows the drivers there.
///
/// A second start, which a boot never makes, finds the name taken and gets an
/// anonymous job in its place rather than none.
///
/// # Errors
///
/// When there was no memory for any job at all.
fn drivers_job() -> Result<Arc<Job>, &'static str> {
    let root = job::root();
    root.new_named_child(DRIVERS_JOB, job::Budget::Own)
        .or_else(|_| root.new_child())
        .or_else(|_| Job::new_root())
        .map_err(|_| "no memory for the drivers' job")
}

/// The manifest's names, each NUL-padded to a driver name; an image without
/// one lists no drivers.
fn manifest(read: ReadFile) -> Result<Vec<[u8; NAME_BYTES]>, &'static str> {
    let Ok(text) = read(MANIFEST) else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for line in text
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty() && line.len() <= NAME_BYTES)
    {
        let mut name = [0; NAME_BYTES];
        for (slot, byte) in name.iter_mut().zip(line) {
            *slot = *byte;
        }
        fallible::try_push(&mut names, name).map_err(|_| "no memory for the manifest")?;
    }
    Ok(names)
}

/// A name without its padding.
fn name_bytes(name: &[u8; NAME_BYTES]) -> &[u8] {
    let end = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(NAME_BYTES);
    name.get(..end).unwrap_or(&[])
}

/// `bytes` in a VMO of its own: anonymous memory the kernel filled.
fn vmo_of(bytes: &[u8]) -> Result<Arc<Vmo>, &'static str> {
    let pages = (bytes.len() as u64).div_ceil(PAGE_SIZE).max(1);
    let vmo = Vmo::new_anonymous(pages).map_err(|_| "no memory for a driver's image")?;
    for (index, chunk) in bytes.chunks(PAGE_SIZE as usize).enumerate() {
        vmo.write_page(index as u64, 0, chunk)
            .map_err(|_| "a driver's image did not fit its VMO")?;
    }
    Ok(vmo)
}

/// Wait for REPORT on `channel`.
fn report(channel: &Endpoint) -> Result<(u32, u32), &'static str> {
    let deadline = timer::now_nanos().saturating_add(REPORT_PATIENCE_NANOS);
    loop {
        match channel.read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => {
                crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
                // Its drivers, and each one it starts, come before REPORT.
                match Message::decode(&message.bytes) {
                    Ok(Message::Report { started, failed }) => return Ok((started, failed)),
                    Ok(said) if record(said) => continue,
                    _ => return Err("devmgr said something other than REPORT first"),
                }
            }
            Err(ReadError::Empty) => {}
            Err(_) => return Err("devmgr closed its channel before reporting"),
        }
        let ready = channel.waiters().wait_until_deadline(
            || {
                channel
                    .signals()
                    .intersects(Signals::READABLE | Signals::PEER_CLOSED)
            },
            deadline,
        );
        if !ready {
            return Err("devmgr reported nothing within the patience");
        }
    }
}

/// The kernel's task on `devmgr`'s channel after REPORT: prints each DIED
/// and RESTARTED.
fn listen(_: usize) {
    let Some(channel) = CHANNEL.lock().clone() else {
        return;
    };
    loop {
        match channel.read(SHORT_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => {
                crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
                match Message::decode(&message.bytes) {
                    Ok(Message::Died { location, status }) => crate::console::println!(
                        "  devmgr   the driver of {location:#010x} ended with status {status}; the device is quiesced"
                    ),
                    Ok(Message::Restarted { location, restarts }) => crate::console::println!(
                        "  devmgr   the driver of {location:#010x} was started again and published (restart {restarts})"
                    ),
                    Ok(said) => {
                        let _ = record(said);
                    }
                    Err(_) => {}
                }
            }
            Err(ReadError::Empty) => {
                let _ = channel.waiters().wait_until_deadline(
                    || {
                        channel
                            .signals()
                            .intersects(Signals::READABLE | Signals::PEER_CLOSED)
                    },
                    u64::MAX,
                );
            }
            Err(_) => {
                // A `devmgr` pid 1 started again has a channel of its own by
                // now; what this one knew is forgotten only if it is still
                // the one there is.
                let current = CHANNEL
                    .lock()
                    .as_ref()
                    .is_some_and(|now| Arc::ptr_eq(now, &channel));
                if current {
                    forget_devmgr();
                }
                crate::console::println!(
                    "  devmgr   devmgr is gone; no driver will be started again until it is"
                );
                return;
            }
        }
    }
}
