//! Stage 10's checks of a driver's configuration window and of
//! `device_aperture` (`docs/NVIDIA.md` §12.1, checks W1 to W7), on the
//! published nodes, through the native calls a driver makes.
//!
//! * W1: `device_aperture` answers every aperture of every node as minted,
//!   and an aperture above 4 GiB and longer than 4 GiB -- QEMU's
//!   `pci-testdev,membar=8G`, which xtask's x86-64 machine carries -- whole.
//! * W2: reads of every header field a node keeps answer what enumeration
//!   read, and reads of the MSI and MSI-X capabilities and of extended space
//!   are answered.
//! * W4: writes to the command register, a BAR, the MSI and MSI-X
//!   capabilities, PCI Express's Device Control, a device-dependent byte no
//!   capability covers and ATS, where the machine has it, are each refused
//!   and read back unchanged.
//! * W5: a write spanning a kernel-owned and a writable byte is refused
//!   whole; an unaligned access, a width of three and offset 0x1000 are
//!   malformed.
//! * W3: a write to a driver-writable field, virtio's
//!   `VIRTIO_PCI_CAP_PCI_CFG` `offset`, reads back as written; a write to
//!   its `pci_cfg_data` is refused.
//! * W6: bus mastering switched on and off on one processor while another
//!   writes a driver-writable byte of the same function, 1,000 rounds, reads
//!   back as `dma_on` says every time.
//! * W7: a command register rewritten behind the kernel, as a driver could
//!   through a configuration mirror in a BAR, is found by `verify_config`:
//!   bus mastering goes off, the node answers `BAD_STATE`, the line names
//!   `COMMAND`, and an `enable_dma` after it is refused and leaves bus
//!   mastering off. And before it, every node reads back as minted.
//!
//! W4 and W5 run before W3 so that a control loosening the allowlist is
//! caught by the check it is the control of.
#![allow(
    clippy::disallowed_methods,
    reason = "a raised check: it calls methods made pub for it alone"
)]

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{APERTURE_BAR_64, APERTURE_INFO_BYTES, ApertureInfo};
use ferrix_pci::ConfigSpace as _;
use ferrix_pci::capability::{ID_MSI, ID_MSIX, ID_PCI_EXPRESS, ID_VENDOR};
use ferrix_pci::header::{BAR0, COMMAND, COMMAND_BUS_MASTER, COMMAND_INTERRUPT_DISABLE};
use ferrix_pci::virtio;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::{ConfigWrites, DeviceNode, Register, devices};
use crate::mmio::Mmio;
use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};
use crate::sync::SpinLock;
use crate::syscall::process::Process;
use crate::vmap;

/// Where `device_aperture` and `device_config_read` write, in the side's
/// scratch region.
const OUT: u64 = SCRATCH + 0x100;
/// QEMU's `pci-testdev`, whose `membar=8G` BAR is a 64-bit prefetchable
/// BAR firmware can only place above 4 GiB.
const TESTDEV: (u16, u16) = (0x1B36, 0x0005);
/// 4 GiB.
const FOUR_GIB: u64 = 1 << 32;
/// Rounds of W6.
const ROUNDS: usize = 1_000;
/// How long W6 waits for its tasks.
const PATIENCE_NANOS: u64 = 30_000_000_000;
/// Extended capability ID: ATS.
const EXTENDED_ID_ATS: u16 = 0x000F;
/// Where virtio's `VIRTIO_PCI_CAP_PCI_CFG` `offset` field is, from the
/// capability.
const PCI_CFG_OFFSET: u16 = 8;
/// Where its `length` field is.
const PCI_CFG_LENGTH: u16 = 12;
/// Where its `pci_cfg_data` is.
const PCI_CFG_DATA: u16 = 16;
/// PCI Express's Device Control, from the capability.
const EXPRESS_DEVICE_CONTROL: u16 = 0x08;

/// The failures the negative controls require by name.
pub(crate) const APERTURE_NOT_AS_MINTED: &str =
    "device_aperture reported an aperture other than enumeration minted it";
/// W2.
pub(crate) const HEADER_NOT_AS_READ: &str =
    "device_config_read answered a header field other than enumeration read it";
/// W3.
pub(crate) const WRITABLE_REFUSED: &str =
    "a driver-writable configuration field was refused or did not read back as written";
/// W3's second control.
pub(crate) const PCI_CFG_DATA_WRITTEN: &str = "a write to virtio's pci_cfg_data was let through";
/// W4.
pub(crate) const KERNEL_OWNED_WRITTEN: &str =
    "a driver's write to a kernel-owned configuration register was let through";
/// W5.
pub(crate) const SPANNING_WRITTEN: &str =
    "a write spanning a kernel-owned and a writable byte was let through";
/// W6.
pub(crate) const BUS_MASTER_RACED: &str = "bus mastering read back other than dma_on said while a driver wrote the function's configuration";
/// W7.
pub(crate) const BREACH_MISSED: &str =
    "a command register rewritten behind the kernel was not found and the node not refused";
/// W7, the consultant's condition 1.
pub(crate) const REFUSED_REENABLED: &str =
    "a refused node's bus mastering was turned on again by enable_dma";

/// What the checks did, for the boot's `config` lines.
#[derive(Debug, Default)]
pub struct Report {
    /// W1: apertures `device_aperture` answered as minted.
    pub apertures: usize,
    /// W1: of those, above 4 GiB and longer than 4 GiB.
    pub above_4_gib: usize,
    /// W2: header fields read as enumeration read them, and capability and
    /// extended reads answered.
    pub reads: usize,
    /// W2: functions read.
    pub functions: usize,
    /// W4 and W5: writes refused whole and read back unchanged.
    pub refused: usize,
    /// W5: malformed accesses refused.
    pub malformed: usize,
    /// W3: driver-writable fields written and read back.
    pub written: usize,
    /// Why W3, W5 and W6 did not run, where they did not.
    pub skipped: Option<&'static str>,
    /// W6: rounds raced.
    pub rounds: usize,
    /// W6: why it did not run, where it did not.
    pub race_skipped: Option<&'static str>,
    /// W7: the register a provoked breach was found at.
    pub breach: Option<Register>,
}

/// Run W1 to W7.
///
/// # Errors
///
/// The first check that failed, by the message its control requires.
pub fn run() -> Result<Report, &'static str> {
    let side = Side::new()?;
    let mut report = Report::default();
    let result = (|| {
        apertures(&side, &mut report)?;
        reads(&side, &mut report)?;
        kernel_owned(&side, &mut report)?;
        match pci_cfg(&side)? {
            Some((node, handle, cap)) => {
                spanning(&side, handle, cap, &mut report)?;
                writable(&side, handle, cap, &mut report)?;
                race(&side, &node, handle, cap, &mut report)?;
            }
            None => {
                report.skipped = Some("no virtio function with a PCI configuration capability");
            }
        }
        malformed(&side, &mut report)?;
        breach(&side, &mut report)
    })();
    side.close_everything();
    result.map(|()| report)
}

/// A function with virtio's `VIRTIO_PCI_CAP_PCI_CFG`: its node, a handle to
/// it, and where the capability is.
type Subject = (Arc<DeviceNode>, Handle, u16);

/// What W6's tasks share: the node, the process and handle a driver's
/// writes go through, and the capability whose `offset` field they write.
type Race = (Arc<DeviceNode>, Arc<Process>, Handle, u16);

/// A device handle to `node` in `side`, with `MANAGE`.
fn handle(side: &Side, node: &Arc<DeviceNode>) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Device(Arc::clone(node)), Rights::DEVICE))
        .map_err(|_| "no room for a device handle")
}

/// `device_config_read` through `side`.
fn read(side: &Side, device: Handle, offset: u16, width: u16) -> Result<u32, Errno> {
    let _ = side.call(
        nr::DEVICE_CONFIG_READ,
        &[reg(device), u64::from(offset), u64::from(width), OUT],
    )?;
    side.get_u32(OUT).map_err(|_| status::FAULT)
}

/// `device_config_write` through `side`.
fn write(side: &Side, device: Handle, offset: u16, width: u16, value: u32) -> Result<(), Errno> {
    side.call(
        nr::DEVICE_CONFIG_WRITE,
        &[
            reg(device),
            u64::from(offset),
            u64::from(width),
            u64::from(value),
        ],
    )
    .map(|_| ())
}

/// The first standard capability `id` of the function `device` names, read
/// through `side` as a driver would walk it.
fn find(side: &Side, device: Handle, id: u8) -> Result<Option<u16>, Errno> {
    let mut at = read(side, device, 0x34, 1)? as u16 & !0x3;
    for _ in 0..48 {
        if at < 0x40 {
            return Ok(None);
        }
        if read(side, device, at, 1)? as u8 == id {
            return Ok(Some(at));
        }
        at = read(side, device, at + 1, 1)? as u16 & !0x3;
    }
    Ok(None)
}

/// W1.
///
/// Verifies: L.device.24
fn apertures(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    for node in devices() {
        let device = handle(side, node)?;
        for (index, aperture) in node.apertures().iter().enumerate() {
            let _ = side
                .call(nr::DEVICE_APERTURE, &[reg(device), index as u64, OUT])
                .map_err(|_| APERTURE_NOT_AS_MINTED)?;
            let bytes = side.get(OUT, APERTURE_INFO_BYTES)?;
            if parse(&bytes) != Some(aperture.info()) {
                return Err(APERTURE_NOT_AS_MINTED);
            }
            report.apertures += 1;
        }
        let past = node.apertures().len() as u64;
        if side.call(nr::DEVICE_APERTURE, &[reg(device), past, OUT]) != Err(status::INVALID_ARGS) {
            return Err("device_aperture answered an index past the device's apertures");
        }
        let testdev = node
            .pci_function()
            .is_some_and(|function| (function.vendor, function.device) == TESTDEV);
        if testdev {
            let big = node.apertures().iter().any(|aperture| {
                let info = aperture.info();
                info.phys >= FOUR_GIB && info.len > FOUR_GIB && info.flags & APERTURE_BAR_64 != 0
            });
            if !big {
                return Err(
                    "pci-testdev's 64-bit BAR was not minted above 4 GiB and longer than it",
                );
            }
            report.above_4_gib += 1;
        }
    }
    Ok(())
}

/// An `ApertureInfo` from the bytes `device_aperture` wrote.
fn parse(bytes: &[u8]) -> Option<ApertureInfo> {
    let long = |at: usize| {
        bytes
            .get(at..at + 8)
            .and_then(|word| <[u8; 8]>::try_from(word).ok())
            .map(u64::from_ne_bytes)
    };
    Some(ApertureInfo {
        phys: long(0)?,
        len: long(8)?,
        bar: *bytes.get(16)?,
        flags: *bytes.get(17)?,
        reserved: <[u8; 6]>::try_from(bytes.get(18..24)?).ok()?,
        offset: long(24)?,
    })
}

/// W2.
///
/// Verifies: L.device.25
fn reads(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    for node in devices() {
        let Some(function) = node.pci_function().filter(|f| f.config_phys.is_some()) else {
            continue;
        };
        let device = handle(side, node)?;
        let field =
            |offset, width| read(side, device, offset, width).map_err(|_| HEADER_NOT_AS_READ);
        let ids = u32::from(function.vendor) | u32::from(function.device) << 16;
        let class = field(0x08, 4)?;
        let mut same = field(0x00, 4)? == ids
            && field(0x00, 2)? == u32::from(function.vendor)
            && field(0x02, 2)? == u32::from(function.device)
            && class >> 8 == function.class
            && field(0x08, 1)? == u32::from(function.revision);
        if field(0x0E, 1)? & 0x7F == 0 {
            let subsystem =
                u32::from(function.subsystem_vendor) | u32::from(function.subsystem) << 16;
            same &= field(0x2C, 4)? == subsystem;
        }
        for (slot, bar) in (0_u16..).zip(crate::device::node_bars(node).iter()) {
            let Some(bar) = bar else { continue };
            let mut address = u64::from(field(BAR0 + 4 * slot, 4)? & !0xF);
            if bar.wide {
                address |= u64::from(field(BAR0 + 4 * slot + 4, 4)?) << 32;
            }
            same &= address == bar.address;
            report.reads += 1;
        }
        if !same {
            return Err(HEADER_NOT_AS_READ);
        }
        report.reads += 6;
        // The kernel's own capabilities are read in full (ruling 5b).
        if let Some(msi) = crate::device::node_msi(node) {
            if field(msi.msi.capability, 1)? != u32::from(ID_MSI) {
                return Err("the MSI capability did not read as itself");
            }
            report.reads += 1;
        }
        if let Some(table) = crate::device::node_msix(node) {
            if field(table.capability, 1)? != u32::from(ID_MSIX) {
                return Err("the MSI-X capability did not read as itself");
            }
            report.reads += 1;
        }
        // Extended space answers, all ones where the function has none.
        let _ = field(0x100, 4)?;
        let _ = field(0xFFC, 4)?;
        report.reads += 2;
        report.functions += 1;
    }
    Ok(())
}

/// Require a write of `width` bytes at `offset`, `flip` toggled, to be
/// refused and to leave the register reading as it did; `what` names the
/// failure.
fn refused(
    side: &Side,
    device: Handle,
    offset: u16,
    width: u16,
    flip: u32,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    let before = read(side, device, offset, width).map_err(|_| what)?;
    if write(side, device, offset, width, before ^ flip) != Err(status::ACCESS_DENIED) {
        return Err(what);
    }
    if read(side, device, offset, width).map_err(|_| what)? != before {
        return Err(what);
    }
    report.refused += 1;
    Ok(())
}

/// W4: each kind of kernel-owned register once, on the first endpoint that
/// has it, so the boot prints one refusal line for each.
///
/// Verifies: L.device.25
fn kernel_owned(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let owned = KERNEL_OWNED_WRITTEN;
    // Command and BAR 0, MSI, MSI-X, Device Control, an uncovered byte, ATS.
    let mut done = [false; 6];
    for node in devices() {
        if !node.pci_function().is_some_and(|f| f.config_phys.is_some()) {
            continue;
        }
        let device = handle(side, node)?;
        if read(side, device, 0x0E, 1).map_err(|_| owned)? & 0x7F != 0 {
            continue;
        }
        let mut targets: [Option<(u16, u16, u32)>; 7] = [None; 7];
        if !done[0] {
            targets[0] = Some((COMMAND, 2, u32::from(COMMAND_INTERRUPT_DISABLE)));
            targets[1] = Some((BAR0, 4, 1 << 20));
            done[0] = true;
        }
        if let Some(msi) = crate::device::node_msi(node).filter(|_| !done[1]) {
            targets[2] = Some((msi.msi.capability + 2, 2, 1));
            done[1] = true;
        }
        if let Some(table) = crate::device::node_msix(node).filter(|_| !done[2]) {
            targets[3] = Some((table.capability + 2, 2, 1 << 14));
            done[2] = true;
        }
        if !done[3]
            && let Some(express) = find(side, device, ID_PCI_EXPRESS).map_err(|_| owned)?
        {
            targets[4] = Some((express + EXPRESS_DEVICE_CONTROL, 2, 1 << 4));
            done[3] = true;
        }
        if !done[4]
            && let Some(at) = uncovered(side, node, device).map_err(|_| owned)?
        {
            targets[5] = Some((at, 1, 0xFF));
            done[4] = true;
        }
        if !done[5]
            && let Some(ats) = extended(side, device, EXTENDED_ID_ATS).map_err(|_| owned)?
        {
            targets[6] = Some((ats + 6, 2, 0x1F));
            done[5] = true;
        }
        for (offset, width, flip) in targets.into_iter().flatten() {
            refused(side, device, offset, width, flip, owned, report)?;
        }
    }
    Ok(())
}

/// A byte of `node`'s legacy space no capability can cover: 0xFF, when its
/// last standard capability is not a vendor one, which says its own length,
/// and starts low enough that no other format the kernel knows -- PCI
/// Express's, at 0x3C bytes, is the longest -- reaches it.
fn uncovered(side: &Side, node: &DeviceNode, device: Handle) -> Result<Option<u16>, Errno> {
    let mut at = read(side, device, 0x34, 1)? as u16 & !0x3;
    let mut last = None;
    for _ in 0..48 {
        if at < 0x40 {
            break;
        }
        last = Some((at, read(side, device, at, 1)? as u8));
        at = read(side, device, at + 1, 1)? as u16 & !0x3;
    }
    Ok(last
        .filter(|&(start, id)| start + 0x40 <= 0xFF && id != ID_VENDOR)
        .filter(|_| !crate::device::node_writable(node).allows_byte(0xFF))
        .map(|_| 0xFF))
}

/// The first extended capability `id`, walked through `side`.
fn extended(side: &Side, device: Handle, id: u16) -> Result<Option<u16>, Errno> {
    let mut at = 0x100_u16;
    for _ in 0..960 {
        if at < 0x100 {
            return Ok(None);
        }
        let header = read(side, device, at, 4)?;
        if header == 0 || header == u32::MAX {
            return Ok(None);
        }
        if header as u16 == id {
            return Ok(Some(at));
        }
        at = (header >> 20) as u16 & !0x3;
    }
    Ok(None)
}

/// The first virtio function with a `VIRTIO_PCI_CAP_PCI_CFG` capability,
/// a handle to it, and the capability's offset.
fn pci_cfg(side: &Side) -> Result<Option<Subject>, &'static str> {
    for node in devices() {
        let virtio = node.pci_function().is_some_and(|function| {
            function.vendor == virtio::VENDOR && function.config_phys.is_some()
        });
        if !virtio {
            continue;
        }
        let device = handle(side, node)?;
        let mut at = read(side, device, 0x34, 1).map_err(|_| HEADER_NOT_AS_READ)? as u16 & !0x3;
        for _ in 0..48 {
            if at < 0x40 {
                break;
            }
            let id = read(side, device, at, 1).map_err(|_| HEADER_NOT_AS_READ)? as u8;
            let kind = read(side, device, at + 3, 1).map_err(|_| HEADER_NOT_AS_READ)? as u8;
            if id == ID_VENDOR && kind == virtio::CFG_PCI {
                return Ok(Some((Arc::clone(node), device, at)));
            }
            at = read(side, device, at + 1, 1).map_err(|_| HEADER_NOT_AS_READ)? as u16 & !0x3;
        }
    }
    Ok(None)
}

/// W5: two bytes at the capability's length byte, which is its header's and
/// the kernel's, and its `cfg_type`, which is the body's and writable.
///
/// Verifies: L.device.25
fn spanning(
    side: &Side,
    device: Handle,
    cap: u16,
    report: &mut Report,
) -> Result<(), &'static str> {
    refused(side, device, cap + 2, 2, 0x0100, SPANNING_WRITTEN, report)
}

/// W5's malformed accesses, on the first PCI node.
///
/// Verifies: L.device.25
fn malformed(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let Some(node) = devices()
        .iter()
        .find(|node| node.pci_function().is_some_and(|f| f.config_phys.is_some()))
    else {
        return Ok(());
    };
    let device = handle(side, node)?;
    let cases: [(u16, u16); 4] = [(0x41, 2), (0x42, 4), (0x40, 3), (0x1000, 1)];
    for (offset, width) in cases {
        if write(side, device, offset, width, 0) != Err(status::INVALID_ARGS)
            || read(side, device, offset, width) != Err(status::INVALID_ARGS)
        {
            return Err("a malformed configuration access was not refused as one");
        }
        report.malformed += 2;
    }
    Ok(())
}

/// W3.
///
/// Verifies: L.device.25, L.device.26
fn writable(
    side: &Side,
    device: Handle,
    cap: u16,
    report: &mut Report,
) -> Result<(), &'static str> {
    let field = |offset| read(side, device, offset, 4).map_err(|_| WRITABLE_REFUSED);
    let offset = field(cap + PCI_CFG_OFFSET)?;
    let length = field(cap + PCI_CFG_LENGTH)?;
    // Length zero first: then even a pci_cfg_data write a broken allowlist
    // let through reaches nothing behind it.
    write(side, device, cap + PCI_CFG_LENGTH, 4, 0).map_err(|_| WRITABLE_REFUSED)?;
    let wanted = offset ^ 0x1230;
    write(side, device, cap + PCI_CFG_OFFSET, 4, wanted).map_err(|_| WRITABLE_REFUSED)?;
    if field(cap + PCI_CFG_OFFSET)? != wanted || field(cap + PCI_CFG_LENGTH)? != 0 {
        return Err(WRITABLE_REFUSED);
    }
    report.written += 2;
    if write(side, device, cap + PCI_CFG_DATA, 4, 0x5A5A_5A5A) != Err(status::ACCESS_DENIED) {
        return Err(PCI_CFG_DATA_WRITTEN);
    }
    report.refused += 1;
    write(side, device, cap + PCI_CFG_OFFSET, 4, offset).map_err(|_| WRITABLE_REFUSED)?;
    write(side, device, cap + PCI_CFG_LENGTH, 4, length).map_err(|_| WRITABLE_REFUSED)?;
    Ok(())
}

/// W6's subject, while it runs.
static RACE: SpinLock<Option<Race>> = SpinLock::new(None);
/// Rounds the switching task found bus mastering other than `dma_on` said.
static WRONG: AtomicUsize = AtomicUsize::new(0);
/// Raised when the writing task has made every write.
static WRITES_DONE: AtomicBool = AtomicBool::new(false);
/// Writes the writing task had refused.
static WRITES_REFUSED: AtomicUsize = AtomicUsize::new(0);

/// W6.
///
/// Verifies: L.object.117
fn race(
    side: &Side,
    node: &Arc<DeviceNode>,
    device: Handle,
    cap: u16,
    report: &mut Report,
) -> Result<(), &'static str> {
    let online = crate::smp::topology().map_or(1, crate::smp::Topology::online);
    if online < 2 {
        report.race_skipped = Some("one processor");
        return Ok(());
    }
    let offset = read(side, device, cap + PCI_CFG_OFFSET, 4).map_err(|_| WRITABLE_REFUSED)?;
    let (found, state) = {
        let config = node.config_writes()?;
        let found = config.read16(ConfigWrites::FUNCTION, COMMAND);
        (found, (config.state.memory, config.state.bus_master))
    };
    let was_on = crate::device::node_dma_on(node).load(Ordering::Acquire);
    WRONG.store(0, Ordering::SeqCst);
    WRITES_DONE.store(false, Ordering::SeqCst);
    WRITES_REFUSED.store(0, Ordering::SeqCst);
    *RACE.lock() = Some((Arc::clone(node), Arc::clone(&side.process), device, cap));

    let switcher = crate::sched::spawn_on("config-switch", switch, 0, NICE_0_WEIGHT, 0, only(0)?)?;
    let writer = crate::sched::spawn_on("config-write", writes, 0, NICE_0_WEIGHT, 1, only(1)?)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let mut late = false;
    while !(switcher.is_dead() && writer.is_dead()) {
        if crate::timer::now_nanos() >= deadline {
            late = true;
            break;
        }
        crate::sched::sleep_for(1_000_000);
    }
    let _ = RACE.lock().take();
    if let Ok(mut config) = node.config_writes() {
        config.write16(ConfigWrites::FUNCTION, COMMAND, found);
        (config.state.memory, config.state.bus_master) = state;
    }
    crate::device::node_dma_on(node).store(was_on, Ordering::Release);
    let _ = write(side, device, cap + PCI_CFG_OFFSET, 4, offset);
    if late {
        return Err("the configuration race's tasks never finished");
    }
    if WRONG.load(Ordering::SeqCst) != 0 {
        return Err(BUS_MASTER_RACED);
    }
    if !WRITES_DONE.load(Ordering::SeqCst) || WRITES_REFUSED.load(Ordering::SeqCst) != 0 {
        return Err("the configuration race's driver writes were refused");
    }
    report.rounds = ROUNDS;
    Ok(())
}

/// A set of the one processor `cpu`.
fn only(cpu: usize) -> Result<CpuSet, &'static str> {
    let mut set = CpuSet::empty();
    set.insert(cpu)
        .map_err(|_| "the configuration race names a processor out of range")?;
    Ok(set)
}

/// W6's switching task: bus mastering on and off, read back each time.
fn switch(_argument: usize) {
    let Some((node, ..)) = RACE.lock().clone() else {
        return;
    };
    for _ in 0..ROUNDS {
        for on in [true, false] {
            let switched = if on {
                node.enable_dma()
            } else {
                node.disable_dma()
            };
            let read = node
                .mapped_config()
                .map(|config| config.read16(COMMAND) & COMMAND_BUS_MASTER != 0);
            if switched.is_err()
                || read != Some(crate::device::node_dma_on(&node).load(Ordering::Acquire))
            {
                let _ = WRONG.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// W6's writing task: the capability's `offset` field, through the native
/// call a driver makes.
fn writes(_argument: usize) {
    let Some((_, process, device, cap)) = RACE.lock().clone() else {
        return;
    };
    let side = Side { process };
    for round in 0..ROUNDS {
        if write(&side, device, cap + PCI_CFG_OFFSET, 4, (round as u32) << 2).is_err() {
            let _ = WRITES_REFUSED.fetch_add(1, Ordering::SeqCst);
        }
    }
    WRITES_DONE.store(true, Ordering::SeqCst);
}

/// W7, and before it every node read back as minted.
///
/// Verifies: L.device.25
fn breach(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    if let Some(node) = devices().iter().find(|node| node.breach().is_some()) {
        let _ = node.verify_config();
        return Err("a node's kernel-owned configuration read back other than minted at boot");
    }
    // edu where there is one, whose MSI mint turned INTx off; else
    // virtio-rng, as on the Arm machines; else the first function with
    // configuration space.
    let pci =
        |node: &&Arc<DeviceNode>| node.pci_function().is_some_and(|f| f.config_phys.is_some());
    let is = |ids: fn(u16, u16) -> bool| {
        move |node: &&Arc<DeviceNode>| {
            node.pci_function()
                .is_some_and(|function| ids(function.vendor, function.device))
        }
    };
    let Some(node) = devices()
        .iter()
        .filter(pci)
        .find(is(|vendor, device| (vendor, device) == super::check::EDU))
        .or_else(|| {
            devices()
                .iter()
                .filter(pci)
                .find(is(|vendor, _| vendor == virtio::VENDOR))
        })
        .or_else(|| devices().iter().find(pci))
    else {
        return Ok(());
    };
    let device = handle(side, node)?;
    let config_phys = node.config_phys().ok_or(BREACH_MISSED)?;
    let (found, state) = {
        let config = node.config_writes()?;
        let found = config.read16(ConfigWrites::FUNCTION, COMMAND);
        (found, (config.state.memory, config.state.bus_master))
    };
    let was_on = crate::device::node_dma_on(node).load(Ordering::Acquire);
    node.enable_dma()?;

    // As firmware, or a driver through a mirror in a BAR, could: through a
    // mapping of the kernel's own, outside the configuration lock.
    let mapping = vmap::map_device(config_phys, 256)
        .map_err(|_| "the breach check could not map configuration space")?;
    let registers = Mmio::at(mapping);
    let at = u64::from(COMMAND);
    let rewritten = registers.read16(at) ^ COMMAND_INTERRUPT_DISABLE;
    registers.write16(at, rewritten);

    let found_breach = node.verify_config();
    let bus_master_off = registers.read16(at) & COMMAND_BUS_MASTER == 0;
    let refused = read(side, device, 0, 4) == Err(status::BAD_STATE);
    // And it stays off: a pin, or any kernel caller, that asks for bus
    // mastering after the refusal is refused itself (condition 1).
    let reenabled = node.enable_dma().is_ok();
    let stays_off = registers.read16(at) & COMMAND_BUS_MASTER == 0;

    // Put back what the check rewrote, and clear the refusal through this
    // path, which only the check has.
    registers.write16(at, found);
    let _ = vmap::unmap_device(mapping);
    if let Ok(mut config) = node.config_writes() {
        config.write16(ConfigWrites::FUNCTION, COMMAND, found);
        (config.state.memory, config.state.bus_master) = state;
    }
    crate::device::node_dma_on(node).store(was_on, Ordering::Release);
    crate::device::node_refused(node).store(false, Ordering::Release);

    let named = found_breach.err().map(|breach| breach.register);
    if named != Some(Register::Command) || !bus_master_off || !refused {
        return Err(BREACH_MISSED);
    }
    if reenabled || !stays_off {
        return Err(REFUSED_REENABLED);
    }
    if node.breach().is_some() {
        return Err("the breach check left its node other than minted");
    }
    report.breach = named;
    Ok(())
}
