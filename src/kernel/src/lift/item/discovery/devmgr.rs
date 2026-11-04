use crate::device;
use crate::fallible;
use crate::fallible::AllocError;
use crate::object::Object;
use crate::object::Transfer;
use crate::object::channel::Endpoint;
use crate::object::channel::ReadError;
use crate::object::channel::Sender;
use crate::object::job;
use crate::object::job::Job;
use crate::object::process::Exit;
use crate::object::process::Host;
use crate::object::process::Process;
use crate::object::process::ProcessRef;
use crate::sched;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::native;
use crate::syscall::native::StartRefused;
use crate::timer;
use crate::user::vmo::Vmo;
use crate::vmap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU16;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_blkring::Location;
use ferrix_blkring::control::DEVICE_RIGHTS;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_bootinfo::option_in;
use ferrix_devmgr_proto::ANSWER_BUSY;
use ferrix_devmgr_proto::ANSWER_DONE;
use ferrix_devmgr_proto::ANSWER_FAILED;
use ferrix_devmgr_proto::ANSWER_NO_DEVICE;
use ferrix_devmgr_proto::BUS_PCI;
use ferrix_devmgr_proto::BUS_PLATFORM;
use ferrix_devmgr_proto::DEVICES_MAX_BYTES;
use ferrix_devmgr_proto::Devices;
use ferrix_devmgr_proto::Message;
use ferrix_devmgr_proto::NAME_BYTES;
use ferrix_devmgr_proto::SHORT_BYTES;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::CHANNEL_MAX_BYTES;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;
use ferrix_sync::Once;
pub type ReadFile = fn(&[u8]) -> Result<vmap::Buffer, Errno>;
pub fn register_reader(read: ReadFile) {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bus {
    Pci,
    Platform,
}
/// A row's kill of a device's driver (`settings::driver_bind`).
pub const KILL_DRIVER: u16 = ferrix_devmgr_proto::KILL_DRIVER;
const REQUEST_PATIENCE_NANOS: u64 = 30_000_000_000;

/// What devmgr has said about its drivers, as in the hybrid's `Bindings`.
struct Bindings {
    names: Vec<[u8; NAME_BYTES]>,
    drivers: Vec<(u16, Bus)>,
    bound: Vec<(usize, u16)>,
}
static BINDINGS: SpinLock<Bindings> = SpinLock::new(Bindings {
    names: Vec::new(),
    drivers: Vec::new(),
    bound: Vec::new(),
});
static PENDING: SpinLock<Vec<(u16, Option<u32>)>> = SpinLock::new(Vec::new());
static ANSWERED: WaitQueue = WaitQueue::new();
static NEXT_TOKEN: AtomicU16 = AtomicU16::new(1);

fn name_bytes(name: &[u8; NAME_BYTES]) -> &[u8] {
    let end = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(NAME_BYTES);
    &name[..end]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverName([u8; NAME_BYTES]);
impl core::ops::Deref for DriverName {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        name_bytes(&self.0)
    }
}

/// The names of `/lib/drivers/MANIFEST`, in its order, which is a driver's number
/// in what devmgr tells: the kernel read the same archive to start it.
pub fn read_manifest(archive: &[u8]) {
    let Some(manifest) = ferrix_vfs::initramfs::file_named(archive, b"lib/drivers/MANIFEST") else {
        return;
    };
    let mut bindings = BINDINGS.lock();
    for line in manifest
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty() && line.len() <= NAME_BYTES)
    {
        let mut name = [0_u8; NAME_BYTES];
        name[..line.len()].copy_from_slice(line);
        bindings.names.push(name);
    }
}

/// A message about the drivers: `true` when it was one. On the thread that reads
/// devmgr's channel.
pub fn record(message: Message) -> bool {
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
            if !bindings.drivers.iter().any(|(at, _)| *at == driver) {
                bindings.drivers.push((driver, bus));
            }
        }
        Message::Bound { device, driver } => {
            let Ok(driver) = u16::try_from(driver) else {
                return true;
            };
            let device = device as usize;
            let mut bindings = BINDINGS.lock();
            bindings.bound.retain(|(at, _)| *at != device);
            bindings.bound.push((device, driver));
        }
        Message::Unbound { device } => {
            BINDINGS
                .lock()
                .bound
                .retain(|(at, _)| *at != device as usize);
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

pub fn drivers_on(bus: Bus) -> Result<Vec<(u16, DriverName)>, AllocError> {
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
pub fn driver_of(device: usize) -> Option<(u16, DriverName)> {
    let bindings = BINDINGS.lock();
    let (_, driver) = bindings.bound.iter().find(|(at, _)| *at == device)?;
    let name = bindings.names.get(usize::from(*driver))?;
    Some((*driver, DriverName(*name)))
}
pub fn bound_to(driver: u16) -> Result<Vec<usize>, AllocError> {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Bind,
    Unbind,
}
/// As the hybrid's: ask devmgr on its channel, and wait for DONE.
pub fn request(kind: Request, device: usize, driver: u16) -> Result<(), Errno> {
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
    PENDING.lock().push((token, None));
    let sent = crate::cores::tell_devmgr(&message.encode());
    let answered = sent
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
        (false, _, _) => Err(Errno::EIO),
        (true, false, _) => Err(Errno::ETIMEDOUT),
        (true, true, Some(ANSWER_DONE)) => Ok(()),
        (true, true, Some(ANSWER_NO_DEVICE)) => Err(Errno::ENODEV),
        (true, true, Some(ANSWER_BUSY)) => Err(Errno::EBUSY),
        (true, true, _) => Err(Errno::EIO),
    };
    report_request(kind, device, driver, result);
    result
}

/// The console line the hybrid's gives, which a row reads.
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
        (Some(at), Ok(())) => crate::println!(
            "  devmgr   {what} of {name} and the device at {at}, asked through sysfs: done"
        ),
        (Some(at), Err(errno)) => {
            let said = match errno {
                Errno::ENODEV => "ENODEV",
                Errno::EBUSY => "EBUSY",
                Errno::ETIMEDOUT => "ETIMEDOUT",
                _ => "EIO",
            };
            crate::println!(
                "  devmgr   {what} of {name} and the device at {at}, asked through sysfs: {said}"
            );
        }
        _ => {}
    }
}
#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub devices: usize,
    pub drivers: usize,
    pub started: u32,
    pub failed: u32,
}
pub fn by_init() -> bool {
    false
}
pub fn wait_reported(deadline: u64) -> bool {
    crate::cores::wait_reported(deadline)
}
pub fn location_of(node: &device::DeviceNode) -> Option<Location> {
    let device::Location::Pci(address) = node.location() else {
        return None;
    };
    Some(Location::new(
        address.segment(),
        address.bus(),
        (address.device() << 3) | address.function(),
    ))
}
pub fn driver_ending(location: Location) -> Option<(i32, Option<u32>)> {
    None
}
pub fn published(location: Location) {
    crate::cores::published(location.raw());
}

/// The kernel starts devmgr under `personality=user`: linuxd holds no starter (MICROKERNEL §5).
pub fn starter() -> Option<crate::object::Object> {
    None
}
