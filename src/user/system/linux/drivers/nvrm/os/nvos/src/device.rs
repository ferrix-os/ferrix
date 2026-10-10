//! The GPU: its configuration space through N0d's window, its apertures
//! through `device_aperture` and `IoMapping`s, its interrupt through the
//! native interrupt object (`docs/NVIDIA.md` §4.2, §4.3, §12.1).
//!
//! nvrm hands this layer its device handle once, at start, with
//! [`nvos_device_attach`]. Until then -- and always in `nvrm-link-test`,
//! and on the host, where no native call exists -- every device question is
//! answered "no device": `os_pci_init_handle` finds nothing, and
//! `os_map_kernel_space` maps nothing.
//!
//! nvrm is given its own function and nothing else. RM also asks for other
//! functions' configuration space -- the upstream bridge, the chipset's host
//! bridge, NVSwitches -- and those answers are "not found", which RM treats
//! as an unknown chipset or a missing bridge.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_native::device::{Device, Interrupt, IoMapping};
use ferrix_native::port::{self, Port};
use ferrix_native::{Deadline, Handle, IoMappingSpec, OwnedHandle};
use ferrix_native_abi::types::{APERTURE_PREFETCHABLE, ApertureInfo, DeviceInfo, PACKET_INTERRUPT};

use crate::futex::Kernel;
use crate::log::say;
use crate::mappings::{Entry, Table};
use crate::status::{self, NvBool, NvStatus};
use crate::sync::Mutex;
use crate::thread;

/// The most apertures remembered.
const APERTURES: usize = 16;
/// The most kernel mappings alive at once. RM maps a page of BAR1 for each
/// channel and unmaps it when the channel goes, so this bounds the channels
/// of every client together with what nvrm itself keeps mapped; each costs
/// the kernel a region and one of the process's 4096 handles, and a quarter
/// of those is the most this layer takes.
const MAPPINGS: usize = 1024;
/// What `os_map_kernel_space` says when that many are alive.
const FULL: &str = "all 1024 kernel mappings are in use; none is free until RM unmaps one";
const _: () = assert!(MAPPINGS == 1024, "FULL names the number");
/// How many unmaps of an address that is no mapping's are said, of however
/// many there are.
const STRAYS_SAID: u32 = 8;
/// A page.
const PAGE: u64 = 4096;
/// The end of PCI Express configuration space.
const CONFIG_END: u32 = 0x1000;

/// `NV_MEMORY_WRITECOMBINED`.
const WRITECOMBINED: u32 = 2;

/// What `nvos_device_attach` learned, written once before any reader.
struct Attached {
    /// The device as enumeration found it.
    info: DeviceInfo,
    /// Its apertures, whole.
    apertures: [Option<ApertureInfo>; APERTURES],
}

/// The device's state.
struct State {
    /// The device handle, 0 before attach.
    handle: AtomicU32,
    /// What attach learned.
    attached: UnsafeCell<Option<Attached>>,
    /// Guards `mappings`.
    lock: Mutex,
    /// The kernel mappings alive: each made by `os_map_kernel_space` and
    /// ended by `os_unmap_kernel_space`, as `ioremap` and `iounmap` do
    /// (`mappings.rs`).
    mappings: UnsafeCell<Table<IoMapping<Kernel>, MAPPINGS>>,
    /// Unmaps of an address that is no mapping's, so far.
    strays: AtomicU32,
}

// SAFETY: `attached` is written once, before `handle` is published with
// release ordering, and only read after `handle` is seen with acquire;
// `mappings` is only touched under `lock`.
unsafe impl Sync for State {}

/// The device.
static STATE: State = State {
    handle: AtomicU32::new(0),
    attached: UnsafeCell::new(None),
    lock: Mutex::new(),
    mappings: UnsafeCell::new(Table::new()),
    strays: AtomicU32::new(0),
};

/// A device handle nobody closes: the handle is nvrm's for its life.
fn device() -> Option<ManuallyDrop<Device<Kernel>>> {
    let handle = STATE.handle.load(Ordering::Acquire);
    (handle != 0).then(|| {
        ManuallyDrop::new(Device::from_owned(OwnedHandle::from_raw(
            Kernel,
            Handle(handle),
        )))
    })
}

/// The attached device's location word, 0 before attach.
pub(crate) fn location() -> u32 {
    attached().map_or(0, |attached| attached.info.location)
}

/// The attached device's handle, 0 before attach.
pub(crate) fn handle() -> u32 {
    STATE.handle.load(Ordering::Acquire)
}

/// What attach learned, once attached.
fn attached() -> Option<&'static Attached> {
    if STATE.handle.load(Ordering::Acquire) == 0 {
        return None;
    }
    // SAFETY: written before `handle` was published, never again.
    unsafe { (*STATE.attached.get()).as_ref() }
}

/// Give this layer nvrm's device: read what it is and its apertures.
/// `NV_OK`, or why not. Called once, before RM runs.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_device_attach(handle: u32) -> NvStatus {
    if STATE.handle.load(Ordering::Acquire) != 0 {
        say!("nvos_device_attach: a device is already attached");
        return status::INVALID_STATE;
    }
    let device = ManuallyDrop::new(Device::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(handle),
    )));
    let info = match device.info() {
        Ok(info) => info,
        Err(why) => {
            say!("nvos_device_attach: device_info refused: {why:?}");
            return status::INVALID_ARGUMENT;
        }
    };
    let mut apertures = [None; APERTURES];
    for (index, slot) in apertures
        .iter_mut()
        .enumerate()
        .take(usize::try_from(info.apertures).unwrap_or(0))
    {
        match device.aperture(index) {
            Ok(aperture) => *slot = Some(aperture),
            Err(why) => {
                say!("nvos_device_attach: device_aperture {index} refused: {why:?}");
                return status::INVALID_ARGUMENT;
            }
        }
    }
    // SAFETY: nothing reads `attached` until `handle` is published below.
    unsafe { *STATE.attached.get() = Some(Attached { info, apertures }) };
    STATE.handle.store(handle, Ordering::Release);
    say!(
        "device {:02x}:{:02x}.{} {:04x}:{:04x} attached, {} apertures, {} vectors",
        (info.location >> 8) & 0xff,
        (info.location >> 3) & 0x1f,
        info.location & 7,
        info.vendor_id,
        info.device_id,
        info.apertures,
        info.vectors
    );
    status::OK
}

/// The attached device as nvrm's probe needs it (`struct nvos_device_desc`
/// in `include/nvos.h`): where it is, what it is, and each aperture whole,
/// by the BAR it came from.
#[repr(C)]
#[derive(Debug)]
pub struct DeviceDesc {
    /// The PCI address: segment in bits 31:16, bus in 15:8, devfn in 7:0.
    pub location: u32,
    /// The class code: base class in bits 23:16, subclass in 15:8.
    pub class: u32,
    /// The vendor identifier.
    pub vendor: u16,
    /// The device identifier.
    pub device: u16,
    /// Vectors `nvos_interrupt_start` can claim.
    pub vectors: u32,
    /// Apertures written below.
    pub apertures: u32,
    /// Each aperture's BAR.
    pub bar: [u8; APERTURES],
    /// Each aperture's flags (`APERTURE_*`).
    pub flags: [u8; APERTURES],
    /// Each aperture's physical address.
    pub phys: [u64; APERTURES],
    /// Each aperture's length in bytes.
    pub len: [u64; APERTURES],
}

/// Describe the attached device into `out`: `NV_OK`, or
/// `NV_ERR_INVALID_STATE` before `nvos_device_attach`.
///
/// # Safety
///
/// `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_device_describe(out: *mut DeviceDesc) -> NvStatus {
    let Some(attached) = attached() else {
        return status::INVALID_STATE;
    };
    let info = &attached.info;
    let mut desc = DeviceDesc {
        location: info.location,
        class: info.class,
        vendor: info.vendor_id,
        device: info.device_id,
        vectors: info.vectors,
        apertures: 0,
        bar: [0; APERTURES],
        flags: [0; APERTURES],
        phys: [0; APERTURES],
        len: [0; APERTURES],
    };
    let mut count = 0;
    for aperture in attached.apertures.iter().flatten() {
        if let (Some(bar), Some(flags), Some(phys), Some(len)) = (
            desc.bar.get_mut(count),
            desc.flags.get_mut(count),
            desc.phys.get_mut(count),
            desc.len.get_mut(count),
        ) {
            *bar = aperture.bar;
            *flags = aperture.flags;
            *phys = aperture.phys;
            *len = aperture.len;
            count += 1;
        }
    }
    desc.apertures = u32::try_from(count).unwrap_or(0);
    // SAFETY: the caller vouches for `out`.
    unsafe { out.write(desc) };
    status::OK
}

/// The handle `os_pci_init_handle` returns for nvrm's own function: any
/// non-null value RM hands back; this one is the device state's address.
fn pci_handle() -> *mut c_void {
    ptr::from_ref(&STATE).cast_mut().cast()
}

/// `os_pci_init_handle`: nvrm's own function, or null for any other.
///
/// # Safety
///
/// `vendor` and `device` are each null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_pci_init_handle(
    domain: u32,
    bus: u8,
    slot: u8,
    function: u8,
    vendor: *mut u16,
    device: *mut u16,
) -> *mut c_void {
    let Some(attached) = attached() else {
        return ptr::null_mut();
    };
    let info = &attached.info;
    let location =
        (domain << 16) | (u32::from(bus) << 8) | (u32::from(slot) << 3) | u32::from(function & 7);
    if location != info.location {
        return ptr::null_mut();
    }
    if !vendor.is_null() {
        // SAFETY: the caller vouches for it.
        unsafe { vendor.write(info.vendor_id) };
    }
    if !device.is_null() {
        // SAFETY: the caller vouches for it.
        unsafe { device.write(info.device_id) };
    }
    pci_handle()
}

/// Read `width` bytes of configuration space at `offset`.
fn config_read(handle: *mut c_void, offset: u32, width: u8) -> Result<u32, NvStatus> {
    if handle != pci_handle() || offset >= CONFIG_END {
        return Err(status::NOT_SUPPORTED);
    }
    let device = device().ok_or(status::NOT_SUPPORTED)?;
    device
        .config_read(u16::try_from(offset).unwrap_or(u16::MAX), width)
        .map_err(|why| {
            say!("configuration read of {width} at {offset:#x} refused: {why:?}");
            status::INVALID_ARGUMENT
        })
}

/// Write `width` bytes of configuration space at `offset`. The window
/// takes writes only inside a vendor capability's body (§12.1); anything
/// else is refused whole, and said here, since RM's own register writes
/// after a reset are among what is refused (AoU-22).
fn config_write(handle: *mut c_void, offset: u32, width: u8, value: u32) -> NvStatus {
    if handle != pci_handle() || offset >= CONFIG_END {
        return status::NOT_SUPPORTED;
    }
    let Some(device) = device() else {
        return status::NOT_SUPPORTED;
    };
    match device.config_write(u16::try_from(offset).unwrap_or(u16::MAX), width, value) {
        Ok(()) => status::OK,
        Err(why) => {
            say!("configuration write of {width} at {offset:#x} ({value:#x}) refused: {why:?}");
            status::INSUFFICIENT_RESOURCES
        }
    }
}

/// Define `os_pci_read_*` for one width.
macro_rules! pci_read {
    ($name:ident, $ty:ty, $width:expr) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        ///
        /// # Safety
        ///
        /// `value` is writable.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            handle: *mut c_void,
            offset: u32,
            value: *mut $ty,
        ) -> NvStatus {
            let (read, status) = match config_read(handle, offset, $width) {
                Ok(read) => (read as $ty, status::OK),
                Err(status) => (<$ty>::MAX, status),
            };
            // SAFETY: the caller vouches for it.
            unsafe { value.write(read) };
            status
        }
    };
}

pci_read!(os_pci_read_byte, u8, 1);
pci_read!(os_pci_read_word, u16, 2);
pci_read!(os_pci_read_dword, u32, 4);

/// `os_pci_write_byte`.
#[unsafe(no_mangle)]
pub extern "C" fn os_pci_write_byte(handle: *mut c_void, offset: u32, value: u8) -> NvStatus {
    config_write(handle, offset, 1, u32::from(value))
}

/// `os_pci_write_word`.
#[unsafe(no_mangle)]
pub extern "C" fn os_pci_write_word(handle: *mut c_void, offset: u32, value: u16) -> NvStatus {
    config_write(handle, offset, 2, u32::from(value))
}

/// `os_pci_write_dword`.
#[unsafe(no_mangle)]
pub extern "C" fn os_pci_write_dword(handle: *mut c_void, offset: u32, value: u32) -> NvStatus {
    config_write(handle, offset, 4, value)
}

/// `os_pci_remove_supported`: nvrm cannot remove a function from the bus.
#[unsafe(no_mangle)]
pub extern "C" fn os_pci_remove_supported() -> NvBool {
    status::FALSE
}

/// `os_enable_pci_req_atomics`: enabling atomics needs the root port's
/// configuration space, which nvrm does not have; Linux answers the same
/// where the platform cannot.
#[unsafe(no_mangle)]
pub extern "C" fn os_enable_pci_req_atomics(_: *mut c_void, _: u32) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_map_kernel_space`: map `[start, start + size)` of one of the
/// device's apertures into nvrm, uncached or write-combining, and return
/// where; null for anything that is not wholly inside an aperture, which
/// includes all of system memory (`docs/NVIDIA.md` §4.3). Each call makes a
/// mapping of its own, which lasts until `os_unmap_kernel_space` is given
/// the address returned here.
#[unsafe(no_mangle)]
pub extern "C" fn os_map_kernel_space(start: u64, size: u64, mode: u32) -> *mut c_void {
    match map(start, size, mode == WRITECOMBINED) {
        Ok(address) => address as *mut c_void,
        Err(why) => {
            say!("os_map_kernel_space({start:#x}, {size:#x}, mode {mode}): {why}");
            ptr::null_mut()
        }
    }
}

/// Map `[start, start + size)`.
fn map(start: u64, size: u64, combining: bool) -> Result<usize, &'static str> {
    let attached = attached().ok_or("no device is attached")?;
    let end = start.checked_add(size).ok_or("the range wraps")?;
    let aperture = attached
        .apertures
        .iter()
        .flatten()
        .find(|a| start >= a.phys && end <= a.phys.saturating_add(a.len))
        .ok_or("not inside one of the device's apertures")?;
    let combining = combining && aperture.flags & APERTURE_PREFETCHABLE != 0;
    let phys = start & !(PAGE - 1);
    let len = (end - phys).div_ceil(PAGE) * PAGE;
    let offset = usize::try_from(start - phys).map_err(|_| "offset")?;
    STATE.lock.lock();
    let mapped = map_new(phys, len, combining);
    STATE.lock.unlock();
    Ok(mapped? + offset)
}

/// Under the lock: a new mapping of `len` bytes at `phys`, remembered.
fn map_new(phys: u64, len: u64, combining: bool) -> Result<usize, &'static str> {
    // SAFETY: under `STATE.lock`.
    let mappings = unsafe { &mut *STATE.mappings.get() };
    if !mappings.has_room() {
        // Said by number, since it means RM holds this many at once -- a
        // leak, or more channels than nvrm serves -- and not a slot short.
        return Err(FULL);
    }
    let device = device().ok_or("no device is attached")?;
    let window: IoMapping<Kernel> = device
        .io_mapping(IoMappingSpec { phys, len })
        .map_err(|_| "io_mapping_create refused")?;
    let mapped = if combining {
        window.map_combining(None)
    } else {
        window.map(None)
    };
    let address = mapped.map_err(|_| "io_mapping_map refused")?;
    // The handle is kept with the mapping and closed when it is unmapped.
    let entry = Entry {
        phys,
        len,
        address,
        window,
    };
    let peak = mappings.peak();
    if let Err(entry) = mappings.insert(entry) {
        // Not reached: there was room, and the lock is held.
        release(entry);
        return Err("no room for the mapping");
    }
    if peak < MAPPINGS / 2 && mappings.live() == MAPPINGS / 2 {
        say!(
            "{} of {MAPPINGS} kernel mappings are in use, for the first time",
            mappings.live()
        );
    }
    Ok(address)
}

/// Unmap a mapping the table no longer holds, and close its handle.
fn release(entry: Entry<IoMapping<Kernel>>) {
    let length = usize::try_from(entry.len).unwrap_or(0);
    // SAFETY: the mapping's own pages, which `os_unmap_kernel_space`'s
    // caller says nothing uses any more.
    let unmapped = unsafe { crate::libc::munmap(entry.address as *mut c_void, length) };
    if unmapped != 0 {
        say!(
            "munmap of the mapping of {:#x}, {:#x} bytes at {:#x}, refused; it stays mapped",
            entry.phys,
            entry.len,
            entry.address
        );
    }
    drop(entry.window);
}

/// `os_unmap_kernel_space`: end the mapping `os_map_kernel_space` returned
/// `address` for -- its pages leave nvrm's address space and its slot and
/// handle are free for the next map. As `iounmap` does, the mapping goes
/// whole whatever `size` says, and an address that is no mapping's is left
/// alone, here with a line.
#[unsafe(no_mangle)]
pub extern "C" fn os_unmap_kernel_space(address: *mut c_void, size: u64) {
    if address.is_null() {
        return;
    }
    STATE.lock.lock();
    // SAFETY: under `STATE.lock`.
    let mappings = unsafe { &mut *STATE.mappings.get() };
    let entry = mappings.remove(address as usize);
    STATE.lock.unlock();
    match entry {
        // Outside the lock: the entry is out of the table, so nothing else
        // can name it, and a map meanwhile may take its slot.
        Some(entry) => release(entry),
        None => {
            if STATE.strays.fetch_add(1, Ordering::Relaxed) < STRAYS_SAID {
                say!("os_unmap_kernel_space({address:p}, {size:#x}): no mapping starts there");
            }
        }
    }
}

/// The device, for the page allocator's pins, if attached.
pub(crate) fn for_pins() -> Option<ManuallyDrop<Device<Kernel>>> {
    device()
}

/// What an interrupt thread runs: `handler(argument)` per interrupt.
struct Isr {
    /// The claimed interrupt.
    interrupt: Interrupt<Kernel>,
    /// The port it is delivered to.
    port: Port<Kernel>,
    /// The handler.
    handler: extern "C" fn(*mut c_void),
    /// Its argument.
    argument: *mut c_void,
}

/// The interrupt thread: wait, handle, acknowledge.
extern "C" fn isr_thread(isr: *mut c_void) {
    // SAFETY: `nvos_interrupt_start` passes a malloc'd `Isr` and gives it
    // up to this thread.
    let isr_block = isr;
    // SAFETY: as above.
    let isr = unsafe { isr_block.cast::<Isr>().read() };
    // SAFETY: read just now; the block is used no more.
    unsafe { crate::libc::free(isr_block) };
    crate::os::isr_thread_is_this();
    loop {
        match isr.port.wait(Deadline::Never) {
            Ok(packet) if packet.kind == PACKET_INTERRUPT => {
                (isr.handler)(isr.argument);
                if let Err(why) = isr.interrupt.ack() {
                    say!("interrupt_ack refused: {why:?}; the interrupt thread stops");
                    return;
                }
            }
            Ok(_) => {}
            Err(why) => {
                say!("port_wait refused: {why:?}; the interrupt thread stops");
                return;
            }
        }
    }
}

/// Claim the device's interrupt `index` (N0b's MSI on the 3060), bind it
/// to a port, and start a thread that calls `handler(argument)` for each
/// delivery and then acknowledges it. `NV_OK`, or why not.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_interrupt_start(
    index: u32,
    handler: extern "C" fn(*mut c_void),
    argument: *mut c_void,
) -> NvStatus {
    let Some(device) = device() else {
        return status::NOT_SUPPORTED;
    };
    let interrupt = match device.interrupt(usize::try_from(index).unwrap_or(usize::MAX)) {
        Ok(interrupt) => interrupt,
        Err(why) => {
            say!("interrupt_create {index} refused: {why:?}");
            return status::INSUFFICIENT_RESOURCES;
        }
    };
    let port = match port::create(Kernel) {
        Ok(port) => port,
        Err(why) => {
            say!("port_create refused: {why:?}");
            return status::INSUFFICIENT_RESOURCES;
        }
    };
    if let Err(why) = interrupt.bind(&port, u64::from(index)) {
        say!("interrupt_bind {index} refused: {why:?}");
        return status::INSUFFICIENT_RESOURCES;
    }
    // SAFETY: a fresh block for one `Isr`; malloc's alignment suffices.
    let isr = unsafe { crate::libc::malloc(size_of::<Isr>()) }.cast::<Isr>();
    if isr.is_null() {
        return status::NO_MEMORY;
    }
    // SAFETY: a fresh, aligned block, handed to the thread.
    unsafe {
        isr.write(Isr {
            interrupt,
            port,
            handler,
            argument,
        });
    }
    if thread::spawn(isr_thread, isr.cast()) {
        status::OK
    } else {
        // SAFETY: the thread did not start, so the `Isr` is still ours:
        // drop it, closing its handles, and free its block.
        unsafe {
            drop(isr.read());
        }
        // SAFETY: as above.
        unsafe { crate::libc::free(isr.cast()) };
        status::INSUFFICIENT_RESOURCES
    }
}
