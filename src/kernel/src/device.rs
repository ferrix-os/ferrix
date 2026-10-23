//! Stage 10: device nodes, and the only way to name a device's registers or
//! interrupts.
//!
//! A driver in ring 3 gets an `IoMapping` for each of its device's apertures
//! "and nothing outside it", and an `Interrupt` for each of its vectors
//! (`docs/ARCHITECTURE.md` §7). Checking a driver's request against what the
//! device has is one comparison, and a comparison is a thing somebody can
//! forget to make. So the comparison is a type instead: an [`Aperture`] or a
//! [`Vector`] can only be constructed here, from what enumeration found, and
//! the objects a driver is handed take those rather than addresses and
//! numbers. A request for memory the device does not decode has no way to
//! become an `Aperture`, so it has no way to become a mapping.
//!
//! # Which devices get a node
//!
//! Every PCI function enumeration found, and every `virtio,mmio` node of a
//! device tree on a machine without ACPI tables. The device tree is an
//! allowlist rather than a walk of every node with a `reg`, because most of
//! those nodes are devices the kernel drives itself — the console, the
//! interrupt controller, the timer — and a node for one of them would let a
//! driver map the kernel's own registers.
//!
//! The rest of the allowlist is board support's: a peripheral a board has to
//! clock and take out of reset before a driver can have it, prepared by a
//! [`BoardBinding`] the board registered at bring-up. The registry does not
//! name any board -- board support is outside the certified item -- and mints
//! what a binding hands back under the same rules as any other node.
//!
//! # What an aperture may not overlap
//!
//! A BAR's address is whatever the register holds, and a device tree's `reg`
//! is whatever somebody wrote; neither is proof the range is the device's. So
//! an aperture is withheld rather than minted when it overlaps anything
//! [`Reserved`] names — every region of the memory map except firmware's own
//! descriptions of device memory, every ECAM window, every device window the
//! kernel has mapped for itself, every IOMMU's registers, the device tree's
//! console and the boot framebuffer — or an aperture another node already
//! holds. A BAR of a
//! function whose memory decoding is off is not an aperture at all: nothing
//! says firmware placed it.
//!
//! # Vectors
//!
//! A device tree node's vectors are its interrupt specifiers, screened: only
//! shared peripheral interrupts, none the kernel has registered a handler on,
//! and none another node already holds.
//!
//! A PCI function's vectors are its MSI-X table entries, minted the first time
//! one is asked for rather than at boot, because each takes a vector from the
//! architecture's allocator and a machine has few to spend on devices nobody
//! drives. The first mint masks every entry and turns MSI-X on; each mint
//! programs its entry, still masked, and the vector belongs to that entry for
//! the life of the machine. Minting does not turn bus mastering on, and a
//! message is a write the device makes, so nothing arrives until whoever gives
//! the device DMA does that.
//!
//! A PCI function with no MSI-X table but an MSI capability gets one vector
//! from it, minted the same way: `INTx` off, the message programmed and
//! masked. Ampere GPUs signal this way (`docs/NVIDIA.md` §2.3).
//!
//! So does a function in `ferrix_pci::msix::MSI_PREFERRED` -- the Motorcomm
//! YT6801, whose MSI-X table shares a page with registers its driver needs.
//! Stage 10 holds its MSI-X off before the node is built (Enable 0,
//! Function Mask 1, read back), and the kernel never enables it: the device
//! then ignores its table (PCIe 6.0 §7.7.2.2), so the table's page is granted
//! as aperture and its MSI capability's one vector offered.
//! [`DeviceNode::verify_config`] refuses the node if Enable reads 1 at a
//! read-back, and an MSI mint made while it reads 1 is refused (`L.device.35`-`L.device.37`; the
//! consultant's review of 2026-10-15).
//!
//! Masking an MSI-X vector is a write to its entry's mask bit, and masking an
//! MSI vector a write to its capability's mask bit, or its enable bit where it
//! has none. The interrupt controller can reach neither, so [`Vector::mask`]
//! knows which kind it is. It takes no lock: an interrupt handler calls it.
//!
//! # The Intel integrated GPU
//!
//! The function at `00:02.0` of vendor `8086` and display class `03` is the
//! Intel integrated GPU, which the kernel tells apart by what enumeration
//! read, never by what a driver says ([`DeviceNode::intel_gpu`],
//! `docs/INTEL-GPU.md` §4.4 and §5; the consultant's review, ledger 2352,
//! K3 (2)). Its driver runs the GPU on commands its programs hand it, and
//! until its unit translates (K2) the GPU then reaches all of memory. So it
//! is not driven unless the command line says [`INTEL_GPU_OPTION`]: every
//! call that needs `MANAGE` on it answers `BAD_STATE`, as for the function
//! holding a lent framebuffer (`L.device.44`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferrix_bootinfo::{BootView, MemKind, PAGE_SIZE};
use ferrix_fdt::{GicInterrupt, Trigger as TreeTrigger};
use ferrix_native_abi::types::{
    APERTURE_BAR_64, APERTURE_NOT_BAR, APERTURE_PREFETCHABLE, APERTURE_WHOLE_PAGES, ApertureInfo,
    DEVICE_NOT_PCI, DEVICE_TREE_BLOCKS, DEVICE_VIRTIO_PCI, DeviceBlock, DeviceInfo, PCI_CLASS_XHCI,
    TREE_FIRMWARE_FB, TREE_GS201_DWC3, TREE_STM32_USBH, TREE_X86_POWER, USB_INPUT_FUNCTIONS,
};
use ferrix_pci::Address;
use ferrix_pci::ConfigSpace as _;
use ferrix_pci::bar::{Bar, Region};
use ferrix_pci::capability::{Capability, MSIX_ENTRY_SIZE, MsiX};
use ferrix_pci::header::{
    BAR0, COMMAND, COMMAND_BUS_MASTER, COMMAND_INTERRUPT_DISABLE, COMMAND_MEMORY_SPACE, Identity,
};
use ferrix_pci::msi::{self, Msi};
use ferrix_pci::msix::{
    self, CAPABILITY_CONTROL, CONTROL_ENABLE, CONTROL_FUNCTION_MASK, ENTRY_ADDRESS_HIGH,
    ENTRY_ADDRESS_LOW, ENTRY_DATA, ENTRY_VECTOR_CONTROL, VECTOR_CONTROL_MASKED,
};
use ferrix_pci::virtio::{Location as VirtioLocation, SharedMemory, Transport};
use ferrix_pci::window::Writable;
use ferrix_sync::{IrqSpinLock, IrqSpinLockGuard, Once};

use crate::discovery::description::{self, Description};
use crate::discovery::finder::{Context, Finder, OutOfMemory};
use crate::fallible::{self, AllocError};
use crate::mmio::Mmio;
use crate::sync::SpinLock;
use crate::{arch, iommu, irq, vmap};

pub(crate) mod check;
#[cfg(ferrix_linux_abi = "on")]
pub(crate) mod config_check;

/// GIC interrupt identifiers below this are software-generated or private to
/// one core, and neither is a device's line.
const FIRST_SHARED_INTERRUPT: u32 = 32;

/// A range of device memory a driver may be given, and nothing else.
///
/// Constructible only in this module. `len` is not always a whole number of
/// pages — QEMU's virtio-mmio transports are 0x200 bytes each, packed into
/// shared pages — so a mapping must not round an aperture outward, or it maps
/// a neighbouring device's registers. [`Aperture::whole_pages`] says whether
/// a page mapping of it is exact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Aperture {
    /// Physical address of the first byte.
    phys: u64,
    /// Bytes long; never zero, and never past the end of the address space.
    len: u64,
    /// Whether reads have no side effects, so it may be mapped cacheable.
    cacheable: bool,
    /// Where it came from: a BAR, by its slot and the aperture's first
    /// byte's offset in it, and whether the BAR is 64 bits wide; or a device
    /// tree window.
    source: Source,
}

/// Where an [`Aperture`] came from, as `device_aperture` reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    /// A device tree node's `reg` window.
    Tree,
    /// A PCI function's BAR.
    Bar {
        /// The BAR's slot.
        slot: u8,
        /// Bytes from the BAR's start to the aperture's first byte: not zero
        /// when the MSI-X table's pages were cut out before it.
        offset: u64,
        /// Whether the BAR is 64 bits wide.
        wide: bool,
    },
    /// The firmware's framebuffer, lent to the node `ferrix.efifb`
    /// publishes for it ([`DeviceNode::firmware_framebuffer`]): the one
    /// aperture [`Reserved`] lets overlap a range it holds.
    Framebuffer,
}

impl Aperture {
    /// Physical address of the first byte.
    pub(crate) const fn phys(self) -> u64 {
        self.phys
    }

    /// Length in bytes.
    pub(crate) const fn len(self) -> u64 {
        self.len
    }

    /// Whether the range may be mapped cacheable.
    pub(crate) const fn cacheable(self) -> bool {
        self.cacheable
    }

    /// Whether the aperture starts on a page boundary and is a whole number
    /// of pages, so a page mapping covers it and nothing else.
    pub(crate) const fn whole_pages(self) -> bool {
        self.phys.is_multiple_of(PAGE_SIZE) && self.len.is_multiple_of(PAGE_SIZE)
    }

    /// One past the last byte. Cannot overflow: nothing mints an aperture
    /// that would.
    const fn end(self) -> u64 {
        self.phys + self.len
    }

    /// Whether this aperture and `start..end` share a byte.
    const fn overlaps(self, start: u64, end: u64) -> bool {
        self.phys < end && start < self.end()
    }

    /// The aperture as `device_aperture` writes it: the address and length
    /// whole, the BAR it came from and where in it, and its flags.
    pub(crate) fn info(self) -> ApertureInfo {
        let (bar, offset, wide) = match self.source {
            Source::Tree | Source::Framebuffer => (APERTURE_NOT_BAR, 0, false),
            Source::Bar { slot, offset, wide } => (slot, offset, wide),
        };
        let mut flags = 0;
        if self.cacheable {
            flags |= APERTURE_PREFETCHABLE;
        }
        if self.whole_pages() {
            flags |= APERTURE_WHOLE_PAGES;
        }
        if wide {
            flags |= APERTURE_BAR_64;
        }
        ApertureInfo {
            phys: self.phys,
            len: self.len,
            bar,
            flags,
            reserved: [0; 6],
            offset,
        }
    }

    /// The part of this aperture `len` bytes at `phys`, which the caller has
    /// checked lies inside it: the same source, its offset moved with it.
    const fn part(self, phys: u64, len: u64) -> Aperture {
        let source = match self.source {
            Source::Tree => Source::Tree,
            Source::Framebuffer => Source::Framebuffer,
            Source::Bar { slot, offset, wide } => Source::Bar {
                slot,
                offset: offset + (phys - self.phys),
                wide,
            },
        };
        Aperture {
            phys,
            len,
            cacheable: self.cacheable,
            source,
        }
    }
}

/// How an interrupt line signals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Trigger {
    /// On a transition.
    Edge,
    /// For as long as the line is asserted.
    Level,
}

/// Where a vector is masked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Masking {
    /// At the interrupt controller: a device tree node's line.
    Controller,
    /// At an MSI-X table entry of a published node.
    MsiX {
        /// The node's index in [`devices`].
        node: usize,
        /// The entry.
        entry: u16,
    },
    /// At the MSI capability of a published node: its vector's mask bit
    /// where it has one, and its enable bit where it has not.
    Msi {
        /// The node's index in [`devices`].
        node: usize,
    },
}

/// An interrupt a driver may be given, and nothing else.
///
/// Constructible only in this module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Vector {
    /// The number `irq::register` takes: a GIC identifier on the Arm
    /// machines, a vector on x86-64.
    number: u32,
    /// How the line signals, where firmware said.
    trigger: Option<Trigger>,
    /// Where it is masked.
    masking: Masking,
}

impl Vector {
    /// The number `irq::register` takes.
    pub(crate) const fn number(self) -> u32 {
        self.number
    }

    /// How the line signals, where firmware said.
    pub(crate) const fn trigger(self) -> Option<Trigger> {
        self.trigger
    }

    /// Stop the vector being raised, wherever that is decided.
    ///
    /// Takes no lock, so an interrupt handler may call it.
    ///
    /// # Errors
    ///
    /// What the interrupt controller says for a line. An MSI-X vector's
    /// table is mapped before the vector exists, so it cannot fail.
    pub(crate) fn mask(self) -> Result<(), &'static str> {
        self.set_masked(true)
    }

    /// Let the vector be raised again.
    ///
    /// # Errors
    ///
    /// As [`Vector::mask`].
    pub(crate) fn unmask(self) -> Result<(), &'static str> {
        self.set_masked(false)
    }

    /// Whether a delivery may leave it unmasked: an edge-triggered MSI-X or
    /// MSI vector, whose message does not stay asserted and whose mask is a
    /// write to the device (`object::interrupt`'s module documentation). A line
    /// the controller holds is masked per delivery whatever its trigger.
    pub(crate) const fn coalesces(self) -> bool {
        matches!(self.masking, Masking::MsiX { .. } | Masking::Msi { .. })
            && matches!(self.trigger, Some(Trigger::Edge))
    }

    /// Whether it reads back masked, where that can be read: an MSI-X
    /// entry's vector control, or an MSI capability's mask or enable bit.
    /// `None` for a controller line.
    pub(crate) fn reads_masked(self) -> Option<bool> {
        match self.masking {
            Masking::Controller => None,
            Masking::MsiX { node, entry } => devices()
                .get(node)
                .and_then(|node| node.msix.as_ref())
                .and_then(|table| table.is_masked(entry)),
            Masking::Msi { node } => devices()
                .get(node)
                .and_then(|node| node.msi.as_ref()?.is_masked(node)),
        }
    }

    /// Mask or unmask it.
    fn set_masked(self, masked: bool) -> Result<(), &'static str> {
        match self.masking {
            Masking::Controller if masked => arch::mask_interrupt(self.number),
            Masking::Controller => arch::unmask_interrupt(self.number),
            Masking::MsiX { node, entry } => devices()
                .get(node)
                .and_then(|node| node.msix.as_ref())
                .ok_or("the vector's device is not published")?
                .set_masked(entry, masked),
            Masking::Msi { node } => {
                let node = devices()
                    .get(node)
                    .ok_or("the vector's device is not published")?;
                node.msi
                    .as_ref()
                    .ok_or("the vector's device is not published")?
                    .set_masked(node, masked)
            }
        }
    }
}

/// Where a device node came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Location {
    /// A PCI function.
    Pci(Address),
    /// A `virtio,mmio` device tree node, by the address of its registers.
    VirtioMmio(u64),
    /// A device tree node of a binding the kernel knows, by the address of
    /// its first registers.
    Tree(u64),
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Pci(address) => write!(f, "pci {address}"),
            Location::VirtioMmio(base) => write!(f, "virtio,mmio@{base:#x}"),
            Location::Tree(base) => write!(f, "tree@{base:#x}"),
        }
    }
}

/// Physical memory no aperture may overlap, as `start..end` ranges.
#[derive(Debug, Default)]
pub(crate) struct Reserved {
    /// The ranges, unsorted: there are a few dozen, and they are searched
    /// once per aperture at boot.
    ranges: Vec<(u64, u64)>,
    /// Which of `ranges` came from the memory map, as `start..end` indices:
    /// they move with the loader's allocations, so [`check::reserved`]
    /// counts them and leaves them out of its digest.
    map: (usize, usize),
    /// Which of `ranges` is the boot framebuffer, when firmware left one:
    /// reserved like the rest, except to the one aperture
    /// [`DeviceNode::firmware_framebuffer`] mints inside it.
    framebuffer: Option<usize>,
}

impl Reserved {
    /// Everything the kernel owns on this machine, given the ranges the
    /// finders are about to read: the PCI walk's ECAM windows.
    ///
    /// Taken before enumeration maps anything, so the device windows it
    /// records are the controllers the kernel drives — the local and I/O
    /// APICs, the HPET, the GIC — and not the buses about to be walked, which
    /// `reads` names whole.
    pub(crate) fn of(view: &BootView<'_>, reads: &[&[(u64, u64)]]) -> Self {
        let mut reserved = Reserved::default();
        let framebuffer = view.raw().framebuffer;
        if framebuffer.is_present() && framebuffer.size > 0 {
            reserved.framebuffer = Some(reserved.ranges.len());
            reserved.add(framebuffer.phys, framebuffer_span(framebuffer.size));
        }
        // Firmware's own `MMIO` descriptions are left out: they are where it
        // says device memory is, and a BAR is device memory.
        let first = reserved.ranges.len();
        for region in view.regions() {
            if region.kind != MemKind::Mmio {
                reserved.add(region.base, region.len);
            }
        }
        reserved.map = (first, reserved.ranges.len());
        for read in reads {
            // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
            reserved.ranges.extend_from_slice(read);
        }
        // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
        reserved.ranges.extend(vmap::device_windows());
        // An IOMMU is programmed by the kernel alone: a driver that could map
        // its registers could hand its own device the whole of memory.
        for unit in iommu::units(view) {
            reserved.add(unit.phys, unit.len);
        }
        if let Description::Tree(tree) = description::of(view)
            && let Some(console) = tree.console()
        {
            for region in console.reg() {
                reserved.add(region.address, region.size);
            }
        }
        reserved
    }

    /// Reserve `len` bytes at `start`.
    fn add(&mut self, start: u64, len: u64) {
        if len > 0 {
            // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
            self.ranges.push((start, start.saturating_add(len)));
        }
    }

    /// Whether `len` bytes at `start` overlap anything reserved.
    pub(crate) fn overlaps(&self, start: u64, len: u64) -> bool {
        let end = start.saturating_add(len);
        self.ranges
            .iter()
            .any(|&(low, high)| start < high && low < end)
    }

    /// Whether `aperture` overlaps anything reserved.
    ///
    /// The boot framebuffer is reserved for every aperture but one: the
    /// firmware framebuffer node's ([`Source::Framebuffer`]), and only while
    /// that one lies wholly inside it. It still overlaps nothing else
    /// reserved -- the memory map's RAM, the kernel's own device windows --
    /// or it is withheld like any other.
    fn covers(&self, aperture: Aperture) -> bool {
        self.ranges
            .iter()
            .enumerate()
            .any(|(index, &(start, end))| {
                aperture.overlaps(start, end) && !self.lends(index, aperture)
            })
    }

    /// Whether range `index` is the boot framebuffer and `aperture` the
    /// firmware framebuffer node's, wholly inside it.
    fn lends(&self, index: usize, aperture: Aperture) -> bool {
        let Some(&(start, end)) = self.ranges.get(index) else {
            return false;
        };
        self.framebuffer == Some(index)
            && aperture.source == Source::Framebuffer
            && start <= aperture.phys
            && aperture.end() <= end
    }

    /// A made-up set of reserved ranges, the first of them a framebuffer at
    /// `framebuffer` when one is given: what stage 10's check of the
    /// exemption ([`check::check_lending`]) mints against on every boot,
    /// whether or not the command line lends anything.
    fn synthetic(
        framebuffer: Option<(u64, u64)>,
        others: &[(u64, u64)],
    ) -> Result<Self, AllocError> {
        let mut reserved = Reserved::default();
        if let Some((start, len)) = framebuffer {
            reserved.framebuffer = Some(0);
            fallible::try_push(&mut reserved.ranges, (start, start + framebuffer_span(len)))?;
        }
        for &(start, len) in others {
            fallible::try_push(&mut reserved.ranges, (start, start + len))?;
        }
        Ok(reserved)
    }

    /// The boot framebuffer's range, `start..end`, when there is one.
    fn framebuffer(&self) -> Option<(u64, u64)> {
        self.framebuffer
            .and_then(|index| self.ranges.get(index))
            .copied()
    }
}

/// How much the boot framebuffer of `size` bytes reserves: its size in whole
/// pages. Firmware may report exactly its visible bytes, which at 800 x 600
/// end inside a page (1,920,000 bytes), while an aperture is whole pages;
/// the rest of that last page is the framebuffer's to lend, and any other
/// range reaching into it -- the memory map's RAM, a device window -- still
/// withholds the aperture ([`Reserved::covers`]).
const fn framebuffer_span(size: u64) -> u64 {
    size.div_ceil(PAGE_SIZE).saturating_mul(PAGE_SIZE)
}

/// A PCI function's MSI capability, for one that has no MSI-X table, and
/// the one vector minted from it.
///
/// One message, never more (`ferrix_pci::msi`). Minting turns `INTx` off in
/// the command register, so a function whose MSI is masked by its enable bit
/// cannot fall back to a pin nothing listens on, and leaves the message
/// masked. A function that can mask its vector is masked by its mask bit; one
/// that cannot is masked by turning MSI off, which drops -- rather than
/// defers, as MSI-X's pending bit would -- what it raises meanwhile. The
/// registers are `ferrix_pci::msi`'s to choose. `object::interrupt` masks a
/// coalescing vector only past `STORM_BOUND` unacknowledged deliveries,
/// before a claim, and at a holder's drop, so what is dropped is a message
/// raised while its driver had not acknowledged the last: a driver of such a
/// function acknowledges before it drains its device's status, and services
/// the device on claim (SAFETY-MANUAL AoU-19).
///
/// Every write goes through the node's configuration lock
/// ([`DeviceNode::config_writes`]), which bus mastering's writes and a
/// driver's take too.
struct MsiFunction {
    /// The function's requester ID, which its message writes carry.
    requester: u32,
    /// The capability, decoded.
    msi: Msi,
    /// The vector minted, once one is.
    minted: IrqSpinLock<Option<u32>, arch::Irq>,
}

impl fmt::Debug for MsiFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MsiFunction")
            .field("requester", &self.requester)
            .field("msi", &self.msi)
            .finish_non_exhaustive()
    }
}

impl MsiFunction {
    /// The MSI capability `seen` found: only for a function with no MSI-X
    /// capability to offer instead, on a machine that does not signal by
    /// line, whose configuration space the host window placed.
    fn of(address: Address, seen: &Seen<'_>, has_msix: bool) -> Option<Self> {
        if has_msix || seen.intx.is_some() {
            return None;
        }
        let _placed = seen.config_phys?;
        Some(MsiFunction {
            requester: u32::from(address.requester_id()),
            msi: seen.msi?,
            minted: IrqSpinLock::new(None),
        })
    }

    /// Mask or unmask the vector, under `node`'s configuration lock. Maps
    /// nothing, so an interrupt handler may call it: the first mint mapped
    /// the space.
    fn set_masked(&self, node: &DeviceNode, masked: bool) -> Result<(), &'static str> {
        let mut config = node
            .config_writes_mapped()
            .ok_or("the vector's MSI capability is not mapped")?;
        self.msi
            .set_masked(&mut config, ConfigWrites::FUNCTION, masked);
        Ok(())
    }

    /// Whether the vector reads back masked.
    fn is_masked(&self, node: &DeviceNode) -> Option<bool> {
        let config = node.config_writes_mapped()?;
        Some(self.msi.is_masked(&config, ConfigWrites::FUNCTION))
    }

    /// Whether the command register reads back with `INTx` off.
    fn intx_off(node: &DeviceNode) -> Option<bool> {
        let config = node.mapped_config()?;
        Some(config.read16(COMMAND) & COMMAND_INTERRUPT_DISABLE != 0)
    }

    /// The vector, minting it if nothing has.
    ///
    /// The first mint maps configuration space, turns `INTx` off under the
    /// node's configuration lock, and programs the message, masked
    /// (`ferrix_pci::msi::Msi::program`), under it again. An architecture
    /// whose message address a 32-bit capability cannot hold is refused
    /// before a vector is allocated, since an allocated vector is never
    /// given back.
    fn mint(&self, node: &DeviceNode) -> Result<Vector, &'static str> {
        let vector = |number| Vector {
            number,
            trigger: Some(Trigger::Edge),
            masking: Masking::Msi { node: node.index },
        };
        if let Some(doorbell) = arch::msi_doorbell()
            && !self.msi.reaches(doorbell)
        {
            return Err("the message's address is above what a 32-bit MSI capability holds");
        }
        {
            let mut config = node.config_writes()?;
            // Never MSI and MSI-X on at once: a function whose MSI-X is held
            // off and reads enabled has been rewritten behind the kernel.
            if let Some(capability) = node.msix_off
                && config.read16(ConfigWrites::FUNCTION, capability + CAPABILITY_CONTROL)
                    & CONTROL_ENABLE
                    != 0
            {
                return Err("the function's MSI-X reads enabled where the kernel holds it off");
            }
            let value = config.read16(ConfigWrites::FUNCTION, COMMAND);
            config.write16(
                ConfigWrites::FUNCTION,
                COMMAND,
                value | COMMAND_INTERRUPT_DISABLE,
            );
            config.state.intx_off = true;
        }
        let mut minted = self.minted.lock();
        if let Some(number) = *minted {
            return Ok(vector(number));
        }
        let message = arch::msi_allocate(self.requester)?;
        if !self.msi.reaches(message.address) {
            return Err("the message's address is above what a 32-bit MSI capability holds");
        }
        // Under `minted`, as the allocation is: the configuration lock is a
        // leaf, taken inside it and nothing inside it.
        let mut config = node.config_writes()?;
        self.msi.program(
            &mut config,
            ConfigWrites::FUNCTION,
            message.address,
            message.data as u16,
        );
        config.state.msi = Some((message.address, message.data as u16));
        drop(config);
        *minted = Some(message.number);
        Ok(vector(message.number))
    }
}

/// What the kernel has made a function's configuration space hold, and the
/// refusals it has printed: everything under the node's configuration lock.
///
/// [`DeviceNode::verify_config`] reads the registers back against the first
/// five, so each is updated in the same hold as the write that makes it so.
#[derive(Debug)]
struct ConfigState {
    /// One bit per dword of the 4 KiB at which a driver's write has been
    /// refused and the refusal printed, so each is printed once. 1,024 bits,
    /// made with the node at stage 10.
    refused: [u64; 16],
    /// Whether memory decoding is on: as found, and on from the first time
    /// bus mastering is.
    memory: bool,
    /// Whether bus mastering is on: as found, then as `enable_dma` and
    /// `disable_dma` last wrote it.
    bus_master: bool,
    /// Whether `INTx` is off: as found, and on from an MSI mint.
    intx_off: bool,
    /// The MSI message programmed, address and data, once one is.
    msi: Option<(u64, u16)>,
    /// Whether MSI-X has been turned on with its function mask clear.
    msix_on: bool,
}

impl ConfigState {
    /// The state of a function whose command register read `command`.
    const fn found(command: u16) -> Self {
        ConfigState {
            refused: [0; 16],
            memory: command & COMMAND_MEMORY_SPACE != 0,
            bus_master: command & COMMAND_BUS_MASTER != 0,
            intx_off: command & COMMAND_INTERRUPT_DISABLE != 0,
            msi: None,
            msix_on: false,
        }
    }

    /// Mark the dword holding `offset` refused, answering whether it was
    /// not already.
    fn first_refusal(&mut self, offset: u16) -> bool {
        let dword = usize::from(offset / 4);
        let bit = 1_u64 << (dword % 64);
        self.refused.get_mut(dword / 64).is_some_and(|word| {
            let first = *word & bit == 0;
            *word |= bit;
            first
        })
    }
}

/// One function's configuration space, mapped, for reading only: what a
/// driver's `device_config_read` and the checks read through. A read outside
/// the function's space, or not aligned to its width, answers all ones, as
/// hardware answers for a register that is not there.
#[derive(Clone, Copy)]
struct MappedConfig {
    /// The mapping.
    registers: Mmio,
    /// Bytes of it that are the function's: 4 KiB under ECAM, 256 under CAM.
    bytes: u16,
}

impl MappedConfig {
    /// `width` bytes at `offset`, widened.
    fn read(self, offset: u16, width: u16) -> u32 {
        let inside = offset.is_multiple_of(width.max(1))
            && offset
                .checked_add(width)
                .is_some_and(|end| end <= self.bytes);
        if !inside {
            return match width {
                1 => u32::from(u8::MAX),
                2 => u32::from(u16::MAX),
                _ => u32::MAX,
            };
        }
        let at = u64::from(offset);
        match width {
            1 => u32::from(self.registers.read8(at)),
            2 => u32::from(self.registers.read16(at)),
            _ => self.registers.read32(at),
        }
    }

    /// The 16-bit register at `offset`.
    fn read16(self, offset: u16) -> u16 {
        self.read(offset, 2) as u16
    }

    /// The 32-bit register at `offset`.
    fn read32(self, offset: u16) -> u32 {
        self.read(offset, 4)
    }
}

/// A published node's configuration space with its configuration lock held:
/// the one type that writes it after stage 10.
///
/// Made only by [`DeviceNode::config_writes`] and
/// [`DeviceNode::config_writes_mapped`], which take the lock, so a write
/// without the lock does not compile. It implements `ferrix_pci::ConfigSpace`
/// for `ferrix_pci::msi`'s sequences, and holds the [`ConfigState`] those
/// writes change. The lock is a leaf and masks interrupts: an MSI vector is
/// masked from interrupt handlers. Nothing is mapped, allocated, waited for
/// or locked while it is held (MEMORY-AND-TIMING §2.2h).
struct ConfigWrites<'a> {
    /// The function's space.
    space: MappedConfig,
    /// The lock, and what it guards.
    state: IrqSpinLockGuard<'a, ConfigState, arch::Irq>,
}

impl ConfigWrites<'_> {
    /// The function every access stands for, which the accessor ignores:
    /// the mapping is one function's.
    const FUNCTION: Address = match Address::new(0, 0, 0, 0) {
        Some(address) => address,
        None => panic!("function 0:0.0 is in range"),
    };

    /// Whether `width` bytes at `offset` are the function's and aligned.
    fn inside(&self, offset: u16, width: u16) -> bool {
        offset.is_multiple_of(width)
            && offset
                .checked_add(width)
                .is_some_and(|end| end <= self.space.bytes)
    }

    /// Write `width` bytes, 1, 2 or 4, of `value` at `offset`; nothing for an
    /// access outside the function or not aligned.
    fn write(&mut self, offset: u16, width: u16, value: u32) {
        if !self.inside(offset, width) {
            return;
        }
        let at = u64::from(offset);
        match width {
            1 => self.space.registers.write8(at, value as u8),
            2 => self.space.registers.write16(at, value as u16),
            _ => self.space.registers.write32(at, value),
        }
    }
}

impl ferrix_pci::ConfigSpace for ConfigWrites<'_> {
    fn read8(&self, _: Address, offset: u16) -> u8 {
        self.space.read(offset, 1) as u8
    }

    fn read16(&self, _: Address, offset: u16) -> u16 {
        self.space.read16(offset)
    }

    fn read32(&self, _: Address, offset: u16) -> u32 {
        self.space.read32(offset)
    }

    fn write16(&mut self, _: Address, offset: u16, value: u16) {
        self.write(offset, 2, u32::from(value));
    }

    fn write32(&mut self, _: Address, offset: u16, value: u32) {
        self.write(offset, 4, value);
    }
}

/// An MSI-X message: its address and data.
type Message = (u64, u32);

/// A PCI function's MSI-X table, and the vectors minted from it.
struct MsixTable {
    /// The function's requester ID, which its message writes carry and a
    /// GICv3's ITS translates them by.
    requester: u32,
    /// Offset of the MSI-X capability in it.
    capability: u16,
    /// Physical address of the table's first entry.
    table_phys: u64,
    /// Entries in the table.
    table_size: u16,
    /// The table, mapped with every entry masked and MSI-X on, from the first
    /// mint. Read by interrupt handlers, and `Once::get` takes no lock.
    table: Once<Result<Mmio, &'static str>>,
    /// The vector each entry was minted with, and the message, address and
    /// data, programmed into it.
    minted: IrqSpinLock<BTreeMap<u16, (u32, u64, u32)>, arch::Irq>,
}

impl fmt::Debug for MsixTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MsixTable")
            .field("requester", &self.requester)
            .field("capability", &self.capability)
            .field("table_phys", &self.table_phys)
            .field("table_size", &self.table_size)
            .finish_non_exhaustive()
    }
}

impl MsixTable {
    /// Offset of `entry`'s vector control register in the table.
    fn control(entry: u16) -> u64 {
        u64::from(entry) * MSIX_ENTRY_SIZE + ENTRY_VECTOR_CONTROL
    }

    /// Mask or unmask `entry`. Takes no lock.
    fn set_masked(&self, entry: u16, masked: bool) -> Result<(), &'static str> {
        let Some(&Ok(table)) = self.table.get() else {
            return Err("the vector's MSI-X table is not mapped");
        };
        if entry >= self.table_size {
            return Err("there is no such MSI-X entry");
        }
        // The other thirty-one bits are reserved, and software keeps them.
        let at = Self::control(entry);
        let value = table.read32(at);
        let value = if masked {
            value | VECTOR_CONTROL_MASKED
        } else {
            value & !VECTOR_CONTROL_MASKED
        };
        table.write32(at, value);
        Ok(())
    }

    /// Whether `entry` reads back masked.
    fn is_masked(&self, entry: u16) -> Option<bool> {
        let Some(&Ok(table)) = self.table.get() else {
            return None;
        };
        (entry < self.table_size)
            .then(|| table.read32(Self::control(entry)) & VECTOR_CONTROL_MASKED != 0)
    }

    /// Map the table, mask every entry, and turn MSI-X on with the function
    /// mask clear, through `node`'s mapping of its configuration space and
    /// under its configuration lock. Once per table; every entry stays masked
    /// until its holder unmasks it.
    fn open(&self, node: &DeviceNode) -> Result<Mmio, &'static str> {
        let len = u64::from(self.table_size) * MSIX_ENTRY_SIZE;
        let table = vmap::map_device(self.table_phys, len)
            .map(Mmio::at)
            .map_err(|_| "the MSI-X table could not be mapped")?;
        for entry in 0..self.table_size {
            let at = Self::control(entry);
            table.write32(at, table.read32(at) | VECTOR_CONTROL_MASKED);
        }
        let mut config = node.config_writes()?;
        let at = self.capability + CAPABILITY_CONTROL;
        let control = config.read16(ConfigWrites::FUNCTION, at);
        config.write16(
            ConfigWrites::FUNCTION,
            at,
            (control | CONTROL_ENABLE) & !CONTROL_FUNCTION_MASK,
        );
        config.state.msix_on = true;
        Ok(table)
    }

    /// The vector for `entry`, minting it if nothing has.
    fn mint(&self, node: &DeviceNode, entry: u16) -> Result<Vector, &'static str> {
        let vector = |number| Vector {
            number,
            trigger: Some(Trigger::Edge),
            masking: Masking::MsiX {
                node: node.index,
                entry,
            },
        };
        if entry >= self.table_size {
            return Err("there is no such MSI-X entry");
        }
        // Mapped outside the lock: mapping and unmapping take the address
        // space's locks and may wait on other processors.
        let table = (*self.table.call_once(|| self.open(node)))?;

        let mut minted = self.minted.lock();
        if let Some(&(number, ..)) = minted.get(&entry) {
            return Ok(vector(number));
        }
        // Room to record the vector before it is allocated: an MSI vector is
        // not given back.
        let held = fallible::reserve().map_err(|_| "no memory to record an MSI-X vector")?;
        // Under `minted`: on a GICv2 this takes the distributor's lock, so
        // the order is `minted`, then that (`gicv2::DISTRIBUTOR_RMW`).
        let msi = arch::msi_allocate(self.requester)?;
        let at = u64::from(entry) * MSIX_ENTRY_SIZE;
        table.write32(at + ENTRY_ADDRESS_LOW, msi.address as u32);
        table.write32(at + ENTRY_ADDRESS_HIGH, (msi.address >> 32) as u32);
        table.write32(at + ENTRY_DATA, msi.data);
        let _ = fallible::insert_held(
            &held,
            &mut minted,
            entry,
            (msi.number, msi.address, msi.data),
        );
        Ok(vector(msi.number))
    }

    /// The first minted entry whose message reads back other than minted:
    /// the entry, and the address and data read and minted.
    fn rewritten(&self) -> Option<(u16, Message, Message)> {
        let Some(&Ok(table)) = self.table.get() else {
            return None;
        };
        let minted = self.minted.lock();
        minted.iter().find_map(|(&entry, &(_, address, data))| {
            let at = u64::from(entry) * MSIX_ENTRY_SIZE;
            let read = (
                u64::from(table.read32(at + ENTRY_ADDRESS_LOW))
                    | u64::from(table.read32(at + ENTRY_ADDRESS_HIGH)) << 32,
                table.read32(at + ENTRY_DATA),
            );
            (read != (address, data)).then_some((entry, read, (address, data)))
        })
    }
}

/// One of a virtio PCI transport's register blocks, as physical memory: the
/// page-aligned start of the pages holding it, inside one of the function's
/// BARs, the block's first byte within those pages, and its length. What a
/// driver is told in START, so that it need not walk configuration space
/// through `device_config_read` to find them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegisterBlock {
    /// Physical address of the first page.
    pub(crate) phys: u64,
    /// The block's first byte, from `phys`.
    pub(crate) offset: u32,
    /// Bytes in the block.
    pub(crate) length: u32,
}

/// A virtio PCI transport's blocks, from its vendor capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VirtioBlocks {
    /// Common configuration.
    pub(crate) common: RegisterBlock,
    /// The notification area.
    pub(crate) notify: RegisterBlock,
    /// Interrupt status.
    pub(crate) isr: RegisterBlock,
    /// Device-specific configuration, which some device types do not have.
    pub(crate) device: Option<RegisterBlock>,
    /// What a queue's notify offset is multiplied by.
    pub(crate) notify_multiplier: u32,
}

impl VirtioBlocks {
    /// The transport's blocks, placed through its BARs; `None` if a block
    /// names a BAR that is not an assigned memory BAR.
    fn of(transport: &Transport, regions: &[Region]) -> Option<VirtioBlocks> {
        let place = |location: &VirtioLocation| {
            let region = regions.iter().find(|region| region.index == location.bar)?;
            let Bar::Memory { address: base, .. } = region.bar else {
                return None;
            };
            if base == 0 {
                return None;
            }
            let start = base.checked_add(u64::from(location.offset))?;
            let phys = start - start % PAGE_SIZE;
            Some(RegisterBlock {
                phys,
                offset: u32::try_from(start - phys).ok()?,
                length: location.length,
            })
        };
        Some(VirtioBlocks {
            common: place(&transport.common)?,
            notify: place(&transport.notify)?,
            isr: place(&transport.isr)?,
            device: match transport.device.as_ref() {
                Some(location) => Some(place(location)?),
                None => None,
            },
            notify_multiplier: transport.notify_multiplier,
        })
    }
}

/// What enumeration read off a PCI function's configuration space and hands
/// [`DeviceNode::pci`], beyond its BARs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seen<'a> {
    /// Physical address of the function's configuration space, if the host
    /// window placed it.
    pub(crate) config_phys: Option<u64>,
    /// Its header.
    pub(crate) identity: &'a Identity,
    /// Its virtio transport, if it has one.
    pub(crate) transport: Option<&'a Transport>,
    /// A type 0 header's subsystem vendor and subsystem, or zeros for a
    /// bridge, which has none.
    pub(crate) subsystem: (u16, u16),
    /// A bridge's secondary bus: the bus behind it, whose functions sysfs
    /// shows inside its directory.
    pub(crate) secondary_bus: Option<u8>,
    /// A virtio GPU's host-visible window, if it has one.
    pub(crate) host_visible: Option<&'a SharedMemory>,
    /// The line its `INTx` pin drives, on a machine with no MSI controller:
    /// then its one vector, and its MSI-X table is not offered.
    pub(crate) intx: Option<GicInterrupt>,
    /// Its MSI capability, decoded, if it has one.
    pub(crate) msi: Option<Msi>,
    /// Whether enumeration held its MSI-X off -- Enable clear and Function
    /// Mask set, read back so -- because it is in
    /// `ferrix_pci::msix::MSI_PREFERRED` (or is stage 10's check case): its
    /// table and pending-bit pages are then granted as aperture and its MSI
    /// vector offered instead. Set only by `discovery::pci` before the node
    /// is built, never from anything a program supplies.
    pub(crate) msix_held_off: bool,
    /// What its configuration window starts from.
    pub(crate) config: &'a ConfigFound,
}

/// What enumeration hands [`DeviceNode::pci`] for a function's
/// configuration window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConfigFound {
    /// Bytes of configuration space the function has: 4 KiB under ECAM, 256
    /// under CAM.
    pub(crate) bytes: u16,
    /// Its command register, read after enumeration was done with it: what
    /// [`DeviceNode::verify_config`] holds its bits against until the
    /// kernel writes them.
    pub(crate) command: u16,
    /// The bytes its driver may write (`ferrix_pci::window`).
    pub(crate) writable: Writable,
}

/// What enumeration read off a PCI function and kept, because whoever starts
/// a driver on it needs it: a driver reads configuration space only through
/// `device_config_read`, which needs `MANAGE`, and whoever starts it holds
/// the device with less.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PciFunction {
    /// The vendor identifier.
    pub(crate) vendor: u16,
    /// The device identifier.
    pub(crate) device: u16,
    /// The class code: base class in bits 23:16, subclass in 15:8, the
    /// programming interface in 7:0.
    pub(crate) class: u32,
    /// The revision identifier.
    pub(crate) revision: u8,
    /// Who built the board, and the board's own identifier: zeros for a
    /// bridge. sysfs shows both, and libdrm reads both to name a card.
    pub(crate) subsystem_vendor: u16,
    pub(crate) subsystem: u16,
    /// For a bridge, the bus directly behind it.
    pub(crate) secondary_bus: Option<u8>,
    /// Physical address of the function's configuration space, if the host
    /// window placed it.
    pub(crate) config_phys: Option<u64>,
    /// Its virtio transport, if it has one.
    pub(crate) virtio: Option<VirtioBlocks>,
    /// Entries in its MSI-X table; zero without one.
    pub(crate) msix_table_size: u16,
    /// A virtio GPU's host-visible window, placed: where blob resources are
    /// mapped for programs to reach (`docs/GPU.md` §6.1).
    pub(crate) host_visible: Option<Window>,
}

/// A window of device memory in physical addresses: whole pages, clear of
/// memory the kernel keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    /// Where its first page is.
    pub(crate) phys: u64,
    /// How many bytes, a whole number of pages.
    pub(crate) len: u64,
}

impl Window {
    /// `window` placed through the BAR it names, or `None` if that BAR is not
    /// an assigned memory BAR, the window is not whole pages, or it overlaps
    /// memory the kernel keeps.
    fn of(window: &SharedMemory, regions: &[Region], reserved: &Reserved) -> Option<Window> {
        let region = regions.iter().find(|region| region.index == window.bar)?;
        let Bar::Memory { address: base, .. } = region.bar else {
            return None;
        };
        if base == 0 || !window.fits(region) {
            return None;
        }
        let phys = base.checked_add(window.offset)?;
        if !phys.is_multiple_of(PAGE_SIZE) || !window.length.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let whole = Aperture {
            phys,
            len: window.length,
            cacheable: true,
            source: Source::Tree,
        };
        (!reserved.covers(whole)).then_some(Window {
            phys,
            len: window.length,
        })
    }
}

/// How a device reaches memory, where it is not how a virtio device does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DmaShape {
    /// Whether a buffer the device reads must be one run of physical
    /// addresses: a scanout engine with one address register and nothing to
    /// translate through.
    pub(crate) contiguous: bool,
    /// Whether the device sees what the processors' caches hold. A device
    /// that does not reads memory, so whatever a program wrote for it has to
    /// be cleaned out of the caches first.
    pub(crate) coherent: bool,
}

impl DmaShape {
    /// Scatter-gather and coherent: every virtio device, and every PCI
    /// device on the machines Ferrix runs on.
    pub(crate) const ORDINARY: DmaShape = DmaShape {
        contiguous: false,
        coherent: true,
    };
}

/// A device a driver can be given, with exactly the apertures and vectors it
/// has.
#[derive(Debug)]
pub(crate) struct DeviceNode {
    /// Where it was found.
    location: Location,
    /// What its configuration space said, for a PCI function.
    pci: Option<PciFunction>,
    /// Whether the function's bus mastering has been turned on for a driver,
    /// which the first pin into its domain does and a quiesce undoes.
    dma_on: AtomicBool,
    /// Its index in [`devices`], once published.
    index: usize,
    /// Set when [`DeviceNode::verify_config`] found a kernel-owned register
    /// rewritten: from then on, until reboot, every call that needs `MANAGE`
    /// on the node answers `BAD_STATE`.
    refused: AtomicBool,
    /// The isolated-interrupts mark (`DEVICE_LIMIT_ISOLATED_INTERRUPTS`):
    /// set once by `devmgr` and never cleared. While it is set and the
    /// machine's interrupts are not isolated, the node's vectors and pins
    /// are refused (`L.device.27`).
    isolated_mark: AtomicBool,
    /// Its memory, in BAR or `reg` order.
    apertures: Vec<Aperture>,
    /// A device tree node's interrupts, in firmware's order, or a PCI
    /// function's `INTx` line on a machine with no MSI controller.
    vectors: Vec<Vector>,
    /// A PCI function's MSI-X table, when it has one vectors can be minted
    /// from.
    msix: Option<MsixTable>,
    /// A PCI function's MSI capability, when it has no MSI-X table to offer.
    msi: Option<MsiFunction>,
    /// For a function whose MSI-X the kernel holds off ([`Seen::msix_held_off`]),
    /// the offset of its MSI-X capability: Enable must read 0 there for
    /// good, which [`DeviceNode::verify_config`] holds it to and an MSI mint
    /// requires.
    msix_off: Option<u16>,
    /// The node's one configuration lock: every write of a published PCI
    /// function's configuration space, the kernel's and a driver's, is made
    /// holding it, through the [`ConfigWrites`] it guards -- bus mastering's
    /// ([`DeviceNode::enable_dma`], [`DeviceNode::disable_dma`]), an MSI
    /// mint's `INTx` off and message, an MSI vector's mask, MSI-X's enable
    /// and function mask, and `device_config_write`. With what those writes
    /// made the registers hold, which [`DeviceNode::verify_config`] reads
    /// them back against.
    config: IrqSpinLock<ConfigState, arch::Irq>,
    /// The function's configuration space, mapped once, at its first use,
    /// for the life of the machine. Read by interrupt handlers, and
    /// `Once::get` takes no lock.
    config_map: Once<Result<Mmio, &'static str>>,
    /// Bytes of configuration space the function has; zero for a node that
    /// is not a PCI function.
    config_bytes: u16,
    /// The bytes a driver may write (`ferrix_pci::window`), computed at
    /// stage 10.
    writable: Writable,
    /// Each memory BAR enumeration found assigned, by slot: what the BAR
    /// registers must read back.
    bars: [Option<MintedBar>; 6],
    /// The IOMMU domain its DMA goes through, made the first time it is asked
    /// for.
    domain: SpinLock<Option<Arc<iommu::Domain>>>,
    /// Apertures not minted because the kernel or another device has that
    /// memory.
    withheld: usize,
    /// The pages of the device's MSI-X table and pending-bit array, as
    /// `(phys, len)`: memory the device has that no aperture may reach,
    /// because whoever writes the table chooses which interrupt it raises.
    interrupt_tables: Vec<(u64, u64)>,
    /// Every assigned memory BAR of a PCI function, by slot, as `(base,
    /// size)`: decoded or not, its apertures published or not. What a page
    /// widened for another BAR must not reach ([`occupants`]).
    assigned: [Option<(u64, u64)>; 6],
    /// Whether memory BARs were left out because the function's memory
    /// decoding was off.
    undecoded: bool,
    /// Firmware's interrupts not minted as vectors.
    withheld_vectors: usize,
    /// For [`Location::Tree`], the binding `device_info` reports (see
    /// `DEVICE_TREE_BLOCKS`).
    binding: u16,
    /// How it reaches memory.
    dma: DmaShape,
    /// For the firmware framebuffer's node, the mode firmware left it in.
    mode: Option<FramebufferMode>,
    /// Whether one of its BARs holds the boot framebuffer: the PCI function
    /// whose memory firmware drew the boot on.
    holds_framebuffer: bool,
    /// Whether the firmware framebuffer is lent to its own node, so this
    /// function, which holds it, is not driven: every call that needs
    /// `MANAGE` on it answers `BAD_STATE` (`docs/DISPLAY.md` §8).
    yielded: bool,
}

/// The mode of the firmware's framebuffer, as `device_info` reports it for
/// its node (`TREE_FIRMWARE_FB`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct FramebufferMode {
    /// Visible width in pixels.
    pub(crate) width: u16,
    /// Visible height in pixels.
    pub(crate) height: u16,
    /// Pixels from one line's start to the next.
    pub(crate) stride: u32,
    /// `FRAMEBUFFER_BGRX` or `FRAMEBUFFER_RGBX`.
    pub(crate) layout: u16,
}

impl DeviceNode {
    /// A node with nothing in it yet.
    pub(crate) const fn empty(location: Location) -> Self {
        DeviceNode {
            location,
            pci: None,
            dma_on: AtomicBool::new(false),
            index: 0,
            refused: AtomicBool::new(false),
            isolated_mark: AtomicBool::new(false),
            apertures: Vec::new(),
            vectors: Vec::new(),
            msix: None,
            msi: None,
            msix_off: None,
            config: IrqSpinLock::new(ConfigState::found(0)),
            config_map: Once::new(),
            config_bytes: 0,
            writable: Writable::NONE,
            bars: [None; 6],
            domain: SpinLock::new(None),
            withheld: 0,
            interrupt_tables: Vec::new(),
            assigned: [None; 6],
            undecoded: false,
            withheld_vectors: 0,
            binding: 0,
            dma: DmaShape::ORDINARY,
            mode: None,
            holds_framebuffer: false,
            yielded: false,
        }
    }

    /// The node for the firmware's framebuffer, which `ferrix.efifb`
    /// asks for (`docs/DISPLAY.md` §8): one aperture, the framebuffer's
    /// visible lines in whole pages, uncached like the kernel's own x86-64
    /// mapping of them for the panic screen; no vector; and nothing that
    /// reaches memory, so no pin ([`DeviceNode::reaches_memory`]).
    ///
    /// `Err` says why there is none: a framebuffer the allocator will own
    /// (`docs/DISPLAY.md` §2.4), a layout other than four bytes a pixel, a
    /// first pixel off a page boundary, a size that does not hold its lines,
    /// or an aperture [`Reserved`] withholds.
    pub(crate) fn firmware_framebuffer(
        framebuffer: &ferrix_bootinfo::Framebuffer,
        reserved: &Reserved,
    ) -> Result<Self, &'static str> {
        let (len, mode) = lendable(framebuffer)?;
        let mut node = DeviceNode::empty(Location::Tree(framebuffer.phys));
        node.binding = TREE_FIRMWARE_FB;
        node.mode = Some(mode);
        // Prefetchable: a GOP framebuffer is linear memory, which a read
        // changes nothing in, so `io_mapping_map_combining` may map it --
        // and only it, since write-combining is the type the kernel's own
        // window takes when the node is lent on x86-64, the one architecture
        // it is lent on, and the hold it takes there keeps every user mapping
        // to (`discovery::firmware`).
        node.mint_from(framebuffer.phys, len, true, Source::Framebuffer, reserved);
        if node.apertures.len() != 1 {
            return Err("the framebuffer overlaps memory the kernel uses");
        }
        Ok(node)
    }

    /// The x86-64 power node (`docs/POWERCTL.md` §4.1, `L.device.51`): no
    /// aperture, no vector, no configuration space and nothing that reaches
    /// memory ([`DeviceNode::reaches_memory`] names it), so a pin, a mapping
    /// and an interrupt on it are refused (`L.device.53`); its one use is
    /// `power_control_create`. At `Location::Tree(0)`, which no other x86-64
    /// node has: the firmware framebuffer's is at its framebuffer.
    pub(crate) fn power() -> Self {
        let mut node = DeviceNode::empty(Location::Tree(0));
        node.binding = TREE_X86_POWER;
        node
    }

    /// Lend the firmware's framebuffer to this node, the Intel integrated GPU
    /// whose memory holds it, rather than to a node of its own (K4 of
    /// `docs/INTEL-GPU.md` §5; the consultant's K4 (1), ledger 2352): the same
    /// one aperture, the framebuffer's visible lines in whole pages, the one
    /// exemption [`Reserved`] makes, minted beside the function's own. Keyed
    /// to the command line and to what enumeration read, never to a driver:
    /// refused unless the node is the Intel GPU ([`DeviceNode::intel_gpu`]),
    /// `allowed` -- whether [`INTEL_GPU_OPTION`] was said -- and one of its
    /// BARs holds the framebuffer (`L.device.46`).
    ///
    /// # Errors
    ///
    /// Why not, as for [`DeviceNode::firmware_framebuffer`], and for a node
    /// that may not be lent it.
    pub(crate) fn lend_framebuffer_here(
        &mut self,
        framebuffer: &ferrix_bootinfo::Framebuffer,
        reserved: &Reserved,
        allowed: bool,
    ) -> Result<(), &'static str> {
        if !self.intel_gpu() || !allowed || !self.holds_framebuffer {
            return Err(
                "only the Intel GPU holding it is lent the framebuffer, under ferrix.intelgpu",
            );
        }
        let (len, _) = lendable(framebuffer)?;
        let before = self.apertures.len();
        self.mint_from(framebuffer.phys, len, true, Source::Framebuffer, reserved);
        if self.apertures.len() != before + 1 {
            return Err("the framebuffer overlaps memory the kernel uses");
        }
        Ok(())
    }

    /// [`DeviceNode::lend_framebuffer_here`], then `hold` over the aperture
    /// it minted. When `hold` refuses, the aperture is taken back, so a node
    /// whose lend failed holds none of it and is neither refused its
    /// configuration writes nor handed a copying card for it (`L.device.46`;
    /// the consultant's K4 change review, ledger 2026-10-18 line 2380 (1)).
    ///
    /// # Errors
    ///
    /// Why not, as for [`DeviceNode::lend_framebuffer_here`], or `hold`'s.
    pub(crate) fn lend_framebuffer_held(
        &mut self,
        framebuffer: &ferrix_bootinfo::Framebuffer,
        reserved: &Reserved,
        allowed: bool,
        hold: impl FnOnce(u64, u64) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        self.lend_framebuffer_here(framebuffer, reserved, allowed)?;
        let held = (0..self.apertures.len())
            .filter_map(|index| self.aperture_info(index))
            .find(|info| info.phys == framebuffer.phys)
            .map(|info| (info.phys, info.len))
            .ok_or("the framebuffer has no aperture")
            .and_then(|(phys, len)| hold(phys, len));
        if held.is_err() {
            self.apertures
                .retain(|aperture| aperture.source != Source::Framebuffer);
        }
        held
    }

    /// Whether the node is a PCI function lent the firmware framebuffer
    /// ([`DeviceNode::lend_framebuffer_here`]).
    pub(crate) fn holds_lent_framebuffer(&self) -> bool {
        matches!(self.location, Location::Pci(_))
            && self
                .apertures
                .iter()
                .any(|aperture| aperture.source == Source::Framebuffer)
    }

    /// Whether the node's driver may be handed a copying card over the lent
    /// framebuffer, by the node's identity alone: the Intel GPU lent it
    /// (`L.device.48`; the consultant's K4 (3)). What the display core ORs
    /// into its rule beside the firmware framebuffer's own node.
    pub(crate) fn may_copy_lent(&self) -> bool {
        self.intel_gpu() && self.holds_lent_framebuffer()
    }
}

/// The framebuffer's visible lines in whole pages, and its mode, if it may
/// be lent at all: what [`DeviceNode::firmware_framebuffer`] and
/// [`DeviceNode::lend_framebuffer_here`] lend.
fn lendable(
    framebuffer: &ferrix_bootinfo::Framebuffer,
) -> Result<(u64, FramebufferMode), &'static str> {
    {
        use ferrix_bootinfo::PixelFormat;
        use ferrix_native_abi::types::{FRAMEBUFFER_BGRX, FRAMEBUFFER_RGBX};
        if !framebuffer.is_present() {
            return Err("firmware left no framebuffer");
        }
        if framebuffer.is_reclaimable() {
            return Err("the framebuffer is in memory the allocator owns");
        }
        let layout = match framebuffer.format {
            PixelFormat::Bgrx8888 => FRAMEBUFFER_BGRX,
            PixelFormat::Rgbx8888 => FRAMEBUFFER_RGBX,
            PixelFormat::Unknown => return Err("the framebuffer's pixel layout is not one shown"),
        };
        let (Ok(width), Ok(height)) = (
            u16::try_from(framebuffer.width),
            u16::try_from(framebuffer.height),
        ) else {
            return Err("the framebuffer is larger than a mode can say");
        };
        if framebuffer.stride < framebuffer.width {
            return Err("the framebuffer's lines are shorter than its width");
        }
        if !framebuffer.phys.is_multiple_of(PAGE_SIZE) {
            return Err("the framebuffer does not start on a page");
        }
        let visible = u64::from(framebuffer.stride)
            .saturating_mul(u64::from(framebuffer.height))
            .saturating_mul(4);
        // The visible bytes against the size firmware gave, before either
        // is rounded to pages: GOP may report exactly the visible bytes
        // (the N150 box at 800 x 600 after systemd-boot), and the page
        // rounding is [`framebuffer_span`]'s, which `Reserved` lends.
        if visible > framebuffer.size {
            return Err("the framebuffer's size does not hold its lines");
        }
        let len = visible.div_ceil(PAGE_SIZE).saturating_mul(PAGE_SIZE);
        Ok((
            len,
            FramebufferMode {
                width,
                height,
                stride: framebuffer.stride,
                layout,
            },
        ))
    }
}

impl DeviceNode {
    /// Whether anything behind the node reaches memory, so that pinning
    /// pages for it means anything: every device but the firmware
    /// framebuffer's node, which is memory itself, and whose driver copies.
    pub(crate) const fn reaches_memory(&self) -> bool {
        !(matches!(self.location, Location::Tree(_))
            && (self.binding == TREE_FIRMWARE_FB || self.binding == TREE_X86_POWER))
    }

    /// Whether the node is the Intel integrated GPU: the PCI function at
    /// bus 0, device 2, function 0 whose vendor is Intel (`8086`) and whose
    /// base class is display (`03`), as enumeration read them
    /// ([`is_intel_gpu`]). Or the node a boot check names in
    /// [`FORCED_INTEL_GPU`], which no product path writes.
    pub(crate) fn intel_gpu(&self) -> bool {
        let forced = FORCED_INTEL_GPU.load(Ordering::Acquire);
        if forced != 0 && forced == self.index.saturating_add(1) {
            return true;
        }
        match (self.location, self.pci.as_ref()) {
            (Location::Pci(address), Some(function)) => {
                is_intel_gpu(address, function.vendor, function.class)
            }
            _ => false,
        }
    }

    /// Whether a driver of the node may publish nodes programs open (the
    /// chardev core's HELLO, `L.object.192`): not for the Intel integrated
    /// GPU while its domain does not translate, or cannot be made, since the
    /// GPU executes what programs hand it (`docs/INTEL-GPU.md` §4.4 (a)).
    /// Every other node may. Every domain counts as untranslated while a
    /// boot check sets [`FORCED_UNTRANSLATED`].
    pub(crate) fn may_have_client_nodes(&self) -> bool {
        if !self.intel_gpu() {
            return true;
        }
        if FORCED_UNTRANSLATED.load(Ordering::Acquire) {
            return false;
        }
        self.domain().is_ok_and(|domain| domain.translated())
    }

    /// The firmware framebuffer node's mode; `None` for any other node.
    pub(crate) const fn framebuffer_mode(&self) -> Option<FramebufferMode> {
        self.mode
    }

    /// The node for a PCI function, from its sized BARs.
    ///
    /// Memory BARs become apertures; I/O BARs do not, because an `IoMapping`
    /// is memory. A BAR firmware left unassigned, at address zero, is not an
    /// aperture either, and no BAR is when `decoding` says the function's
    /// memory decoding is off. The pages holding the MSI-X table and
    /// pending-bit array are cut out of whichever BAR holds them and recorded
    /// instead, so a BAR that is all table becomes no aperture at all.
    ///
    /// The MSI-X table is kept for minting vectors from when it lies whole in
    /// an assigned memory BAR, clear of reserved memory, and `config_phys`
    /// says where the function's configuration space is -- unless `seen`
    /// gives the function an `INTx` line, which is then its one vector.
    ///
    /// A function whose MSI-X enumeration held off ([`Seen::msix_held_off`])
    /// has no table: MSI-X stays disabled, so the device ignores it (PCIe
    /// 6.0 §7.7.2.2), and its pages are granted with the rest of the BAR.
    /// Its MSI capability's one vector is offered instead.
    pub(crate) fn pci(
        address: Address,
        seen: &Seen<'_>,
        regions: &[Region],
        msix: Option<&(Capability, MsiX)>,
        decoding: bool,
        reserved: &Reserved,
    ) -> Self {
        let mut node = DeviceNode::empty(Location::Pci(address));
        node.window(seen);
        let identity = seen.identity;
        // Held off, the table is neither offered nor withheld: the kernel
        // never enables MSI-X on the function, and every byte of the BAR is
        // the driver's.
        let held_off = seen.msix_held_off && msix.is_some();
        let found = msix;
        if held_off {
            node.msix_off = found.map(|(capability, _)| capability.offset);
        }
        let msix = if held_off { None } else { msix };
        // With a line, the table stays withheld from the apertures below but
        // is never offered: nothing on the machine could take its messages.
        let offered = if seen.intx.is_some() { None } else { msix };
        if let Some(line) = seen.intx {
            // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
            node.vectors.push(Vector {
                number: line.id,
                trigger: Some(trigger_of(line.trigger)),
                masking: Masking::Controller,
            });
        }
        node.pci = Some(PciFunction {
            vendor: identity.vendor,
            device: identity.device,
            class: (u32::from(identity.class.base) << 16)
                | (u32::from(identity.class.sub) << 8)
                | u32::from(identity.class.interface),
            revision: identity.revision,
            subsystem_vendor: seen.subsystem.0,
            subsystem: seen.subsystem.1,
            secondary_bus: seen.secondary_bus,
            config_phys: seen.config_phys,
            virtio: seen
                .transport
                .and_then(|transport| VirtioBlocks::of(transport, regions)),
            msix_table_size: offered.map_or(0, |(_, table)| table.table_size),
            host_visible: seen
                .host_visible
                .and_then(|window| Window::of(window, regions, reserved)),
        });
        for region in regions {
            if let Bar::Memory { address, .. } = region.bar
                && address != 0
                && let Some(slot) = node.assigned.get_mut(usize::from(region.index))
            {
                *slot = Some((address, region.size));
            }
        }
        if !decoding {
            node.undecoded = regions
                .iter()
                .any(|region| matches!(region.bar, Bar::Memory { .. }));
            return node;
        }
        // The table's pages are recorded either way; held off, they are
        // granted with the rest of the BAR.
        let table = found.map(|(_, table)| table);
        for region in regions {
            node.mint_bar(region, table, held_off, reserved);
        }
        node.msix = seen
            .config_phys
            .and(offered)
            .and_then(|found| MsixTable::of(address, found, regions, reserved));
        node.msi = MsiFunction::of(address, seen, msix.is_some());
        node
    }

    /// What the configuration window starts from, for a PCI function: the
    /// command register as enumeration left it, the bytes of configuration
    /// space it has, and the bytes its driver may write.
    fn window(&mut self, seen: &Seen<'_>) {
        self.config = IrqSpinLock::new(ConfigState::found(seen.config.command));
        self.config_bytes = seen.config_phys.map_or(0, |_| seen.config.bytes);
        self.writable = seen.config.writable;
    }

    /// Mint the apertures of one BAR of a function whose memory decoding is
    /// on, if it is an assigned memory BAR: everything but the pages of
    /// `table`'s MSI-X table and pending-bit array, which are recorded as
    /// withheld instead -- or, for a function whose MSI-X is `held_off`,
    /// everything, the table's pages recorded all the same; and record the
    /// BAR's address, for [`DeviceNode::verify_config`].
    fn mint_bar(
        &mut self,
        region: &Region,
        table: Option<&MsiX>,
        held_off: bool,
        reserved: &Reserved,
    ) {
        let Bar::Memory {
            address: base,
            prefetchable,
            wide,
        } = region.bar
        else {
            return;
        };
        if base == 0 {
            return;
        }
        if let Some((start, end)) = reserved.framebuffer()
            && base < end
            && start < base.saturating_add(region.size)
        {
            self.holds_framebuffer = true;
        }
        if let Some(slot) = self.bars.get_mut(usize::from(region.index)) {
            *slot = Some(MintedBar {
                address: base,
                size: region.size,
                wide,
            });
        }
        for &(offset, len) in msix::withheld(region, table, PAGE_SIZE).as_slice() {
            if let Some(start) = base.checked_add(offset) {
                // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
                self.interrupt_tables.push((start, len));
            }
        }
        let granted = if held_off { None } else { table };
        for &(offset, len) in msix::mappable(region, granted, PAGE_SIZE).as_slice() {
            if let Some(start) = base.checked_add(offset) {
                let source = Source::Bar {
                    slot: region.index,
                    offset,
                    wide,
                };
                self.mint_from(start, len, prefetchable, source, reserved);
            }
        }
    }

    /// Add a device tree window of `len` bytes at `phys` as an aperture,
    /// unless it is empty, runs off the address space, or overlaps reserved
    /// memory.
    pub(crate) fn mint(&mut self, phys: u64, len: u64, cacheable: bool, reserved: &Reserved) {
        self.mint_from(phys, len, cacheable, Source::Tree, reserved);
    }

    /// [`DeviceNode::mint`], for an aperture from `source`.
    fn mint_from(
        &mut self,
        phys: u64,
        len: u64,
        cacheable: bool,
        source: Source,
        reserved: &Reserved,
    ) {
        if phys == 0 || len == 0 || phys.checked_add(len).is_none() {
            return;
        }
        let aperture = Aperture {
            phys,
            len,
            cacheable,
            source,
        };
        if reserved.covers(aperture) {
            self.withheld += 1;
        } else {
            // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
            self.apertures.push(aperture);
        }
    }

    /// A virtio GPU's host-visible window, where the render core maps blob
    /// resources for programs, if the device has one.
    pub(crate) fn host_visible(&self) -> Option<Window> {
        self.pci.as_ref().and_then(|function| function.host_visible)
    }

    /// Where the device was found.
    pub(crate) const fn location(&self) -> Location {
        self.location
    }

    /// Its place in [`devices`]: what sysfs and `devmgr` call it by, since
    /// a device tree node has no PCI address to be told apart by.
    pub(crate) const fn index(&self) -> usize {
        self.index
    }

    /// For a device tree node of a binding the kernel knows, which one
    /// (`TREE_STM32_HDMI` and the rest); zero otherwise.
    pub(crate) const fn binding(&self) -> u16 {
        self.binding
    }

    /// How many input control channels the device may hold at once: one
    /// for an input device, and one per keyboard or mouse for a USB host,
    /// whose driver learns what is on its bus only as it enumerates it.
    ///
    /// A USB host is an STM32 board's node by its binding, or a PCI function
    /// whose class code, as enumeration read it, is exactly xHCI's
    /// ([`PCI_CLASS_XHCI`]). The class is the function's own claim, and all a
    /// function that lies about it gains is these claims on its own node,
    /// each of which still takes the node's `MANAGE` handle.
    pub(crate) fn input_functions(&self) -> usize {
        match self.location {
            Location::Tree(_) if self.binding == TREE_STM32_USBH => USB_INPUT_FUNCTIONS,
            Location::Pci(_)
                if self
                    .pci
                    .as_ref()
                    .is_some_and(|function| function.class == PCI_CLASS_XHCI) =>
            {
                USB_INPUT_FUNCTIONS
            }
            _ => 1,
        }
    }

    /// Whether the device's driver may read the kernel log over a log control
    /// channel (`log_control_create`): a device that carries it off the
    /// machine for its owner, which today is only the Pixel 7's USB device
    /// controller, whose serial port streams the boot to the host. The log
    /// holds every program's console output, so this is a capability of the
    /// binding, not of whoever holds a device.
    pub(crate) const fn reads_log(&self) -> bool {
        matches!(self.location, Location::Tree(_)) && self.binding == TREE_GS201_DWC3
    }

    /// How the device reaches memory.
    pub(crate) const fn dma_shape(&self) -> DmaShape {
        self.dma
    }

    /// How many apertures `mint` has added.
    pub(crate) fn apertures_minted(&self) -> usize {
        self.apertures.len()
    }

    /// Add a device tree interrupt as the node's next vector, masked at the
    /// interrupt controller, edge or level as the tree says.
    pub(crate) fn add_line(&mut self, interrupt: &GicInterrupt) {
        // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
        self.vectors.push(Vector {
            number: interrupt.id,
            trigger: interrupt.trigger.map(|trigger| match trigger {
                TreeTrigger::EdgeRising | TreeTrigger::EdgeFalling => Trigger::Edge,
                TreeTrigger::LevelHigh | TreeTrigger::LevelLow => Trigger::Level,
            }),
            masking: Masking::Controller,
        });
    }

    /// Count `lines` interrupts the tree names that the node was not given.
    pub(crate) fn withhold_lines(&mut self, lines: usize) {
        self.withheld_vectors += lines;
    }

    /// Make the node a board's peripheral: `binding` names it, and `dma` is
    /// the memory it may reach.
    pub(crate) fn bind_board(&mut self, binding: u16, dma: DmaShape) {
        self.binding = binding;
        self.dma = dma;
    }

    /// The binding of a device tree node the kernel knows, `None` for any
    /// other device.
    pub(crate) const fn tree_binding(&self) -> Option<u16> {
        match self.location {
            Location::Tree(_) => Some(self.binding),
            _ => None,
        }
    }

    /// What configuration space said, for a PCI function.
    pub(crate) const fn pci_function(&self) -> Option<&PciFunction> {
        self.pci.as_ref()
    }

    /// The device as `device_info` reports it.
    pub(crate) fn describe(&self) -> DeviceInfo {
        let block = |block: RegisterBlock| DeviceBlock {
            phys: block.phys,
            offset: block.offset,
            length: block.length,
        };
        let mut info = DeviceInfo {
            location: match self.location {
                Location::Pci(address) => {
                    (u32::from(address.segment()) << 16)
                        | (u32::from(address.bus()) << 8)
                        | (u32::from(address.device()) << 3)
                        | u32::from(address.function())
                }
                _ => DEVICE_NOT_PCI,
            },
            apertures: u32::try_from(self.apertures.len()).unwrap_or(u32::MAX),
            vectors: u32::try_from(self.vector_count()).unwrap_or(u32::MAX),
            ..DeviceInfo::default()
        };
        if let Location::Tree(_) = self.location {
            info.device_id = self.binding;
            info.virtio = DEVICE_TREE_BLOCKS;
            let window = |aperture: Option<&Aperture>| {
                aperture.map_or_else(DeviceBlock::default, |aperture| DeviceBlock {
                    phys: aperture.phys,
                    offset: 0,
                    length: u32::try_from(aperture.len).unwrap_or(u32::MAX),
                })
            };
            info.common = window(self.apertures.first());
            info.device = window(self.apertures.get(1));
            // The firmware framebuffer's mode, where `TREE_FIRMWARE_FB` says.
            if let Some(mode) = self.mode {
                info.subsystem_vendor_id = mode.width;
                info.subsystem_id = mode.height;
                info.notify_off_multiplier = mode.stride;
                info.msix_table_size = mode.layout;
            }
        }
        if let Some(function) = &self.pci {
            info.vendor_id = function.vendor;
            info.device_id = function.device;
            info.class = function.class;
            info.msix_table_size = function.msix_table_size;
            info.subsystem_vendor_id = function.subsystem_vendor;
            info.subsystem_id = function.subsystem;
            if let Some(virtio) = function.virtio {
                info.virtio = DEVICE_VIRTIO_PCI;
                info.notify_off_multiplier = virtio.notify_multiplier;
                info.common = block(virtio.common);
                info.notify = block(virtio.notify);
                info.isr = block(virtio.isr);
                info.device = virtio.device.map(block).unwrap_or_default();
            }
        }
        info
    }

    /// Turn the function's bus mastering on, the first time a driver pins a
    /// page into its domain: from then on the device reaches what its domain
    /// maps. Nothing to do for a device tree node, whose transport has no
    /// such switch.
    ///
    /// # Errors
    ///
    /// Configuration space that could not be mapped; bus mastering stays off.
    pub(crate) fn enable_dma(&self) -> Result<(), &'static str> {
        if self.dma_on.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.set_bus_master(true)
            .inspect_err(|_| self.dma_on.store(false, Ordering::Release))
    }

    /// Turn the function's bus mastering off: the device's driver is gone,
    /// and until the next one pins, the device reaches nothing.
    ///
    /// # Errors
    ///
    /// Configuration space that could not be mapped; the device is still on.
    pub(crate) fn disable_dma(&self) -> Result<(), &'static str> {
        self.set_bus_master(false)?;
        self.dma_on.store(false, Ordering::Release);
        Ok(())
    }

    /// Write the function's command register with bus mastering `on` or off,
    /// keeping memory decoding on either way, under the configuration lock.
    fn set_bus_master(&self, on: bool) -> Result<(), &'static str> {
        if self.config_phys().is_none() {
            return Ok(());
        }
        let mut config = self.config_writes()?;
        // A refused node never gets bus mastering back. Tested under the lock
        // `refuse` sets the flag and turns bus mastering off under, so an
        // `enable_dma` that passed `device_in` before the refusal cannot undo
        // it, nor can a kernel caller after.
        if on && self.refused.load(Ordering::Acquire) {
            return Err("the node was refused: its kernel-owned configuration was rewritten");
        }
        let command = config.read16(ConfigWrites::FUNCTION, COMMAND);
        let value = if on {
            command | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER
        } else {
            command & !COMMAND_BUS_MASTER
        };
        config.write16(ConfigWrites::FUNCTION, COMMAND, value);
        config.state.bus_master = on;
        config.state.memory |= on;
        Ok(())
    }

    /// Physical address of the function's configuration space, for a PCI
    /// function whose host window placed it.
    fn config_phys(&self) -> Option<u64> {
        self.pci.as_ref().and_then(|function| function.config_phys)
    }

    /// The function's configuration space, mapped at the first call and
    /// kept: never under the configuration lock, since mapping takes the
    /// address space's locks and may wait on other processors.
    fn config_mapping(&self) -> Result<MappedConfig, &'static str> {
        let config_phys = self
            .config_phys()
            .ok_or("the device has no configuration space")?;
        let registers = (*self.config_map.call_once(|| {
            vmap::map_device(config_phys, u64::from(self.config_bytes))
                .map(Mmio::at)
                .map_err(|_| "the function's configuration space could not be mapped")
        }))?;
        Ok(MappedConfig {
            registers,
            bytes: self.config_bytes,
        })
    }

    /// The function's configuration space for reading, if it is mapped
    /// already. Maps nothing.
    fn mapped_config(&self) -> Option<MappedConfig> {
        match self.config_map.get() {
            Some(&Ok(registers)) => Some(MappedConfig {
                registers,
                bytes: self.config_bytes,
            }),
            _ => None,
        }
    }

    /// The function's configuration space with the configuration lock held,
    /// mapping it first if nothing has: the only way to write it.
    fn config_writes(&self) -> Result<ConfigWrites<'_>, &'static str> {
        let space = self.config_mapping()?;
        Ok(ConfigWrites {
            space,
            state: self.config.lock(),
        })
    }

    /// [`DeviceNode::config_writes`] for a space that is mapped already, as
    /// an interrupt handler needs: maps nothing, and `None` if nothing has.
    fn config_writes_mapped(&self) -> Option<ConfigWrites<'_>> {
        let space = self.mapped_config()?;
        Some(ConfigWrites {
            space,
            state: self.config.lock(),
        })
    }

    /// The device's aperture `index` as `device_aperture` writes it.
    pub(crate) fn aperture_info(&self, index: usize) -> Option<ApertureInfo> {
        self.apertures.get(index).map(|aperture| aperture.info())
    }

    /// Whether [`DeviceNode::verify_config`] has refused the node.
    pub(crate) fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }

    /// Whether the node is not driven while the machine runs: it holds the
    /// firmware framebuffer lent to its own node, or it is the Intel
    /// integrated GPU and the command line did not say [`INTEL_GPU_OPTION`].
    pub(crate) const fn yielded(&self) -> bool {
        self.yielded
    }

    /// `width` bytes of the function's configuration space at `offset`, for
    /// `device_config_read`: every byte is readable, the kernel's own
    /// registers included, and an offset past a function with 256 bytes
    /// reads all ones.
    ///
    /// # Errors
    ///
    /// [`ConfigRefusal::Arguments`] for a width other than 1, 2 or 4, or an
    /// offset not a multiple of it or not below 4096;
    /// [`ConfigRefusal::NotPci`] for a node with no configuration space;
    /// [`ConfigRefusal::Unmapped`] when it could not be mapped.
    pub(crate) fn config_read(&self, offset: u64, width: u64) -> Result<u32, ConfigRefusal> {
        let (offset, width) = config_access(offset, width)?;
        if self.config_phys().is_none() {
            return Err(ConfigRefusal::NotPci);
        }
        let space = self.config_mapping().map_err(|_| ConfigRefusal::Unmapped)?;
        Ok(space.read(offset, width))
    }

    /// Write the low `width` bytes of `value` at `offset`, for
    /// `device_config_write`: only when every byte lies in the function's
    /// driver-writable ranges (`ferrix_pci::window`), under the
    /// configuration lock. Any other write is refused whole and changes
    /// nothing. The first refusal at each dword is printed once the lock is
    /// dropped, naming the register.
    ///
    /// # Errors
    ///
    /// As [`DeviceNode::config_read`], and [`ConfigRefusal::Denied`] for a
    /// byte the driver may not write.
    pub(crate) fn config_write(
        &self,
        offset: u64,
        width: u64,
        value: u64,
    ) -> Result<(), ConfigRefusal> {
        let (offset, width) = config_access(offset, width)?;
        // The function lent the framebuffer its screen scans out of takes no
        // configuration write at all while it holds it: no byte of a vendor
        // capability either, so no reset, function-level or other, is
        // reached through it; the kernel has no reset call of its own
        // (`L.device.47`; the consultant's K4 (2)).
        if self.holds_lent_framebuffer() {
            return Err(ConfigRefusal::Denied);
        }
        if self.config_phys().is_none() {
            return Err(ConfigRefusal::NotPci);
        }
        let allowed = self.writable.allows(offset, width);
        let mut config = self.config_writes().map_err(|_| ConfigRefusal::Unmapped)?;
        if allowed {
            config.write(offset, width, value as u32);
            return Ok(());
        }
        let first = config.state.first_refusal(offset);
        drop(config);
        if first {
            crate::println!(
                "  config   {} refused a {width}-byte write at {offset:#x} ({})",
                self.location,
                self.writable.name(offset),
            );
        }
        Err(ConfigRefusal::Denied)
    }

    /// Read back what the kernel made the function's configuration space
    /// hold, and refuse the node if a kernel-owned register reads otherwise:
    /// the command register's memory decoding, bus mastering and `INTx`
    /// bits, every assigned memory BAR, the MSI capability's control,
    /// address and data once a message is programmed, and MSI-X's control
    /// and every minted entry's message once the table is open.
    ///
    /// Run at each accepted HELLO, beside `object::pin::quarantine_release`,
    /// and at each quiesce. A driver, or its device's firmware, can rewrite
    /// those registers through a mirror of configuration space in a BAR,
    /// which the configuration window cannot refuse (SAFETY-MANUAL AoU-22);
    /// this finds it before the next driver is given the device. On a
    /// mismatch bus mastering goes off, the node is refused until reboot,
    /// and one line names the register.
    ///
    /// # Errors
    ///
    /// The register that read back other than the kernel made it.
    pub(crate) fn verify_config(&self) -> Result<(), Breach> {
        let Some(breach) = self.breach() else {
            return Ok(());
        };
        self.refuse();
        crate::println!(
            "  device   {} refused: {} reads {:#x} where {:#x} was minted (a driver or its \
             firmware rewrote a kernel-owned register)",
            self.location,
            breach.register,
            breach.read,
            breach.minted,
        );
        Err(breach)
    }

    /// Refuse the node and turn its bus mastering off, in one hold of the
    /// configuration lock: [`DeviceNode::set_bus_master`] tests the flag
    /// under the same lock, so no later `enable_dma` turns it on again.
    fn refuse(&self) {
        match self.config_writes() {
            Ok(mut config) => {
                self.refused.store(true, Ordering::Release);
                let command = config.read16(ConfigWrites::FUNCTION, COMMAND);
                config.write16(
                    ConfigWrites::FUNCTION,
                    COMMAND,
                    command & !COMMAND_BUS_MASTER,
                );
                config.state.bus_master = false;
            }
            // Unmapped, so nothing was read back and nothing can be written:
            // refused all the same.
            Err(_) => self.refused.store(true, Ordering::Release),
        }
        self.dma_on.store(false, Ordering::Release);
    }

    /// What a core does once it has accepted a new driver's HELLO for the
    /// node, which the driver sent after resetting the device: what the
    /// kernel made the configuration space hold is read back first
    /// ([`DeviceNode::verify_config`]), which refuses the node if a driver or
    /// its firmware rewrote it, and then the frames a dead driver's pins kept
    /// go back (`object::pin::quarantine_release`), whose device can no
    /// longer reach them either way.
    pub(crate) fn hello_accepted(&self) {
        let _ = self.verify_config();
        crate::object::pin::quarantine_release(self);
    }

    /// The first kernel-owned register that reads back other than the kernel
    /// made it, if any: [`DeviceNode::verify_config`]'s comparison.
    fn breach(&self) -> Option<Breach> {
        let _placed = self.config_phys()?;
        let breach = {
            // Under the lock, so no kernel sequence is half written, and so
            // the state read is the one the registers were last written to.
            let config = self.config_writes().ok()?;
            config_breach(&config, &self.bars, self.msi.as_ref().map(|msi| &msi.msi))
                .or_else(|| {
                    let table = self.msix.as_ref()?;
                    msix_control_breach(&config, table.capability)
                })
                .or_else(|| msix_off_breach(&config, self.msix_off?))
        };
        breach.or_else(|| {
            let (entry, read, minted) = self.msix.as_ref()?.rewritten()?;
            let register = if read.0 == minted.0 {
                Register::MsixData(entry)
            } else {
                Register::MsixAddress(entry)
            };
            let (read, minted) = if read.0 == minted.0 {
                (u64::from(read.1), u64::from(minted.1))
            } else {
                (read.0, minted.0)
            };
            Some(Breach {
                register,
                read,
                minted,
            })
        })
    }

    /// Every aperture the device has.
    pub(crate) fn apertures(&self) -> &[Aperture] {
        &self.apertures
    }

    /// How many vectors the device can be asked for: a device tree node's
    /// interrupts, a PCI function's MSI-X entries, or its one `INTx` line.
    pub(crate) fn vector_count(&self) -> usize {
        if self.msi.is_some() {
            return 1;
        }
        self.msix
            .as_ref()
            .map_or(self.vectors.len(), |table| usize::from(table.table_size))
    }

    /// The IOMMU domain the device's DMA goes through: one per node, the same
    /// one every time.
    ///
    /// Made on first use, under the node's lock so that only one is ever
    /// attached. A domain whose `Arc` could not be allocated is dropped,
    /// which detaches it, and the next use tries again.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn domain(&self) -> Result<Arc<iommu::Domain>, AllocError> {
        // The firmware framebuffer's node reaches no memory, so it never
        // gets a domain to pin into: every path to one fails here, whatever
        // its caller checked first.
        if !self.reaches_memory() {
            return Err(AllocError);
        }
        let mut held = self.domain.lock();
        if let Some(domain) = held.as_ref() {
            return Ok(Arc::clone(domain));
        }
        // A function whose RMRR identity domain is held already, or placed
        // elsewhere, gets none (`iommu::domain_for`); a failed allocation
        // drops the domain, which gives an identity domain's claim back.
        let domain = fallible::try_arc(match self.location {
            Location::Pci(address) => iommu::domain_for(address).ok_or(AllocError)?,
            Location::VirtioMmio(_) | Location::Tree(_) => iommu::Domain::untranslated(),
        })?;
        // The Intel integrated GPU's budget is the kernel's, set before any
        // pin can be counted against another (`object::pin`, ledger 2352 K3).
        if self.intel_gpu() {
            crate::object::pin::give_intel_gpu_budget(&domain);
        }
        *held = Some(Arc::clone(&domain));
        Ok(domain)
    }

    /// Set the isolated-interrupts mark. It is never cleared.
    pub(crate) fn mark_isolated(&self) {
        self.isolated_mark.store(true, Ordering::Release);
    }

    /// Whether the isolated-interrupts mark is set.
    pub(crate) fn isolated_marked(&self) -> bool {
        self.isolated_mark.load(Ordering::Acquire)
    }

    /// Whether the node's interrupts are isolated: a PCI function whose
    /// unit remaps, on a machine whose interrupts are isolated
    /// (`iommu::function_isolated`). Never a device tree or virtio-mmio
    /// node's, nor any on ARMv7-A, nor yet an AArch64 one behind a GICv3
    /// ITS.
    pub(crate) fn interrupts_isolated(&self) -> bool {
        match self.location {
            Location::Pci(address) => iommu::function_isolated(address),
            Location::VirtioMmio(_) | Location::Tree(_) => false,
        }
    }

    /// Whether the node may be given vectors and pins: not while its
    /// isolated-interrupts mark is set and its interrupts are not isolated
    /// (`L.device.27`).
    pub(crate) fn may_take_vectors_and_pins(&self) -> bool {
        !self.isolated_marked() || self.interrupts_isolated()
    }

    /// The device's IOMMU domain if one was ever made: [`DeviceNode::domain`]
    /// without making one, for a question about pins that may never have been.
    pub(crate) fn domain_made(&self) -> Option<Arc<iommu::Domain>> {
        self.domain.lock().as_ref().map(Arc::clone)
    }

    /// `len` bytes at `phys`, if they lie inside one of the device's
    /// apertures. The only way to make an [`Aperture`] after enumeration.
    pub(crate) fn aperture(&self, phys: u64, len: u64) -> Option<Aperture> {
        if len == 0 {
            return None;
        }
        let end = phys.checked_add(len)?;
        self.apertures
            .iter()
            .find(|whole| whole.phys <= phys && end <= whole.end())
            .map(|whole| whole.part(phys, len))
    }

    /// The device's vector at `index`, if it has one.
    ///
    /// For a PCI function, the vector of MSI-X entry `index`: minted the first
    /// time it is asked for, and the same vector every time after. A function
    /// given an `INTx` line has that as vector 0 instead. `None` if
    /// the table has no such entry, the node is not published yet, or the
    /// architecture has no vector left to give.
    pub(crate) fn vector(&self, index: usize) -> Option<Vector> {
        if self.msix.is_none() && self.msi.is_none() {
            return self.vectors.get(index).copied();
        }
        let published = devices()
            .get(self.index)
            .is_some_and(|node| core::ptr::eq(node.as_ref(), self));
        if !published {
            return None;
        }
        match (&self.msix, &self.msi) {
            (Some(table), _) => table.mint(self, u16::try_from(index).ok()?).ok(),
            (None, Some(msi)) if index == 0 => msi.mint(self).ok(),
            _ => None,
        }
    }
}

impl MsixTable {
    /// The table `msix` describes, if vectors can be minted from it: in an
    /// assigned memory BAR, whole, and clear of memory the kernel owns.
    fn of(
        address: Address,
        (capability, msix): &(Capability, MsiX),
        regions: &[Region],
        reserved: &Reserved,
    ) -> Option<Self> {
        let region = regions
            .iter()
            .find(|region| region.index == msix.table.bar)?;
        let Bar::Memory { address: base, .. } = region.bar else {
            return None;
        };
        let offset = u64::from(msix.table.offset);
        if base == 0 || !region.contains(offset, msix.table_len()) {
            return None;
        }
        let table_phys = base.checked_add(offset)?;
        if reserved.overlaps(table_phys, msix.table_len()) {
            return None;
        }
        Some(MsixTable {
            requester: u32::from(address.requester_id()),
            capability: capability.offset,
            table_phys,
            table_size: msix.table_size,
            table: Once::new(),
            minted: IrqSpinLock::new(BTreeMap::new()),
        })
    }
}

/// A memory BAR as enumeration found it assigned: what its register must
/// read back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct MintedBar {
    /// The address, both halves for a 64-bit BAR.
    address: u64,
    /// Its size in bytes, as sizing read it.
    size: u64,
    /// Whether it is 64 bits wide, so the next slot is its upper half.
    wide: bool,
}

/// Why a configuration access was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ConfigRefusal {
    /// A width other than 1, 2 or 4, or an offset not a multiple of it or
    /// not below 4096.
    Arguments,
    /// The node is not a PCI function whose configuration space the host
    /// window placed.
    NotPci,
    /// The function's configuration space could not be mapped.
    Unmapped,
    /// A byte the write would touch is not the driver's to write.
    Denied,
}

/// `offset` and `width` as a configuration access takes them, if they are
/// one.
fn config_access(offset: u64, width: u64) -> Result<(u16, u16), ConfigRefusal> {
    if !matches!(width, 1 | 2 | 4)
        || !offset.is_multiple_of(width)
        || offset >= u64::from(ferrix_pci::CONFIG_SPACE_SIZE)
    {
        return Err(ConfigRefusal::Arguments);
    }
    Ok((offset as u16, width as u16))
}

/// A kernel-owned register [`DeviceNode::verify_config`] can find rewritten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Register {
    /// The command register's decoding, bus mastering or `INTx` bits.
    Command,
    /// A memory BAR, by slot.
    Bar(u8),
    /// The MSI capability's message control.
    MsiControl,
    /// The MSI message's address.
    MsiAddress,
    /// The MSI message's data.
    MsiData,
    /// The MSI-X capability's message control.
    MsixControl,
    /// An MSI-X entry's address.
    MsixAddress(u16),
    /// An MSI-X entry's data.
    MsixData(u16),
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Register::Command => f.write_str("COMMAND"),
            Register::Bar(slot) => write!(f, "BAR{slot}"),
            Register::MsiControl => f.write_str("MSI control"),
            Register::MsiAddress => f.write_str("MSI address"),
            Register::MsiData => f.write_str("MSI data"),
            Register::MsixControl => f.write_str("MSI-X control"),
            Register::MsixAddress(entry) => write!(f, "MSI-X entry {entry}'s address"),
            Register::MsixData(entry) => write!(f, "MSI-X entry {entry}'s data"),
        }
    }
}

/// A kernel-owned register that read back other than the kernel made it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Breach {
    /// Which.
    pub(crate) register: Register,
    /// What it read.
    pub(crate) read: u64,
    /// What the kernel made it.
    pub(crate) minted: u64,
}

/// The command register's bits the kernel owns and checks.
const COMMAND_CHECKED: u16 = COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER | COMMAND_INTERRUPT_DISABLE;

/// The first of the command register, the BARs and the MSI capability that
/// reads back other than `config`'s state says the kernel made it.
fn config_breach(
    config: &ConfigWrites<'_>,
    bars: &[Option<MintedBar>; 6],
    msi: Option<&Msi>,
) -> Option<Breach> {
    command_breach(config)
        .or_else(|| bar_breach(config, bars))
        .or_else(|| msi_breach(config, msi?))
}

/// The command register's decoding, bus-mastering and `INTx` bits, if they
/// read other than the state says.
fn command_breach(config: &ConfigWrites<'_>) -> Option<Breach> {
    let state = &*config.state;
    let command = config.read16(ConfigWrites::FUNCTION, COMMAND);
    let mut want = 0;
    for (on, bit) in [
        (state.memory, COMMAND_MEMORY_SPACE),
        (state.bus_master, COMMAND_BUS_MASTER),
        (state.intx_off, COMMAND_INTERRUPT_DISABLE),
    ] {
        if on {
            want |= bit;
        }
    }
    (command & COMMAND_CHECKED != want).then_some(Breach {
        register: Register::Command,
        read: u64::from(command),
        minted: u64::from((command & !COMMAND_CHECKED) | want),
    })
}

/// The first assigned memory BAR that reads other than enumeration found it.
fn bar_breach(config: &ConfigWrites<'_>, bars: &[Option<MintedBar>; 6]) -> Option<Breach> {
    (0_u8..).zip(bars.iter()).find_map(|(slot, bar)| {
        let bar = bar.as_ref()?;
        let at = BAR0 + 4 * u16::from(slot);
        let mut read = u64::from(config.read32(ConfigWrites::FUNCTION, at) & !0xF);
        if bar.wide {
            read |= u64::from(config.read32(ConfigWrites::FUNCTION, at + 4)) << 32;
        }
        (read != bar.address).then_some(Breach {
            register: Register::Bar(slot),
            read,
            minted: bar.address,
        })
    })
}

/// The MSI capability's control, address or data, once a message is
/// programmed, if one reads other than programmed.
fn msi_breach(config: &ConfigWrites<'_>, msi: &Msi) -> Option<Breach> {
    let f = ConfigWrites::FUNCTION;
    let (address, data) = config.state.msi?;
    let control = config.read16(f, msi.capability + msi::CONTROL);
    let enabled = Msi::enabled(control);
    let control_minted = if msi.maskable {
        enabled
    } else {
        // Masked by its enable bit, which the mask therefore owns.
        (enabled & !msi::CONTROL_ENABLE) | (control & msi::CONTROL_ENABLE)
    };
    if control != control_minted {
        return Some(Breach {
            register: Register::MsiControl,
            read: u64::from(control),
            minted: u64::from(control_minted),
        });
    }
    let mut read = u64::from(config.read32(f, msi.capability + msi::ADDRESS_LOW));
    let address = if msi.wide {
        read |= u64::from(config.read32(f, msi.capability + msi::ADDRESS_HIGH)) << 32;
        address
    } else {
        address & u64::from(u32::MAX)
    };
    if read != address {
        return Some(Breach {
            register: Register::MsiAddress,
            read,
            minted: address,
        });
    }
    let read = config.read16(f, msi.data());
    (read != data).then_some(Breach {
        register: Register::MsiData,
        read: u64::from(read),
        minted: u64::from(data),
    })
}

/// The MSI-X capability's control, if the kernel turned MSI-X on and it
/// reads back off or function-masked.
fn msix_control_breach(config: &ConfigWrites<'_>, capability: u16) -> Option<Breach> {
    if !config.state.msix_on {
        return None;
    }
    let control = config.read16(ConfigWrites::FUNCTION, capability + CAPABILITY_CONTROL);
    let minted = (control | CONTROL_ENABLE) & !CONTROL_FUNCTION_MASK;
    (control != minted).then_some(Breach {
        register: Register::MsixControl,
        read: u64::from(control),
        minted: u64::from(minted),
    })
}

/// A function whose MSI-X the kernel holds off, read with Enable set: a
/// driver or its firmware turned it on through a mirror of configuration
/// space, and the table the driver can write would then direct messages.
/// Only Enable is compared: the Function Mask the kernel wrote may be
/// cleared by the chip's own reset, which leaves the table inert all the
/// same while Enable reads 0.
fn msix_off_breach(config: &ConfigWrites<'_>, capability: u16) -> Option<Breach> {
    let control = config.read16(ConfigWrites::FUNCTION, capability + CAPABILITY_CONTROL);
    (control & CONTROL_ENABLE != 0).then_some(Breach {
        register: Register::MsixControl,
        read: u64::from(control),
        minted: u64::from(control & !CONTROL_ENABLE),
    })
}

/// How a line signals, from what the tree says. A PCI `INTx` line is level
/// triggered whatever the tree omits, so a line it gives no trigger is level.
fn trigger_of(trigger: Option<TreeTrigger>) -> Trigger {
    match trigger {
        Some(TreeTrigger::EdgeRising | TreeTrigger::EdgeFalling) => Trigger::Edge,
        Some(TreeTrigger::LevelHigh | TreeTrigger::LevelLow) | None => Trigger::Level,
    }
}

/// Whether GIC line `id` may become a PCI function's vector, recording it in
/// `taken` if so: a shared peripheral interrupt no kernel handler and no
/// other function holds, as a device tree node's lines are screened.
pub(crate) fn claim_line(id: u32, taken: &mut BTreeSet<u32>) -> bool {
    // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
    id >= FIRST_SHARED_INTERRUPT && !irq::is_registered(id) && taken.insert(id)
}

/// Memory a page widened for a sub-page BAR could reach: an aperture, an
/// assigned memory BAR, or a withheld interrupt table, of any node.
#[derive(Clone, Copy, Debug)]
struct Occupant {
    /// Where it starts.
    start: u64,
    /// Where it ends, exclusive.
    end: u64,
    /// The node and BAR slot it belongs to, for a BAR's own aperture and
    /// span, which the BAR's page test leaves out; `None` for anything no
    /// widening may ever be granted over (an interrupt table, a device tree
    /// window).
    owner: Option<(usize, u8)>,
    /// What it is, for the refusal's line.
    what: &'static str,
}

/// Everything in `nodes` a widened page must not reach, taken before
/// `publish` drops clashing apertures: every aperture, every assigned memory
/// BAR of every PCI function -- decoded or left undecoded, its apertures
/// published, withheld as reserved or dropped as clashes -- and every
/// withheld MSI-X table and pending-bit array span.
fn occupants(nodes: &[DeviceNode]) -> Vec<Occupant> {
    let mut all = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        for aperture in &node.apertures {
            let owner = match aperture.source {
                Source::Bar { slot, .. } => Some((index, slot)),
                _ => None,
            };
            // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
            all.push(Occupant {
                start: aperture.phys,
                end: aperture.end(),
                owner,
                what: "another aperture",
            });
        }
        for (slot, bar) in (0u8..).zip(node.assigned.iter()) {
            if let Some((base, size)) = *bar {
                // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
                all.push(Occupant {
                    start: base,
                    end: base.saturating_add(size),
                    owner: Some((index, slot)),
                    what: "an assigned memory BAR",
                });
            }
        }
        for &(start, len) in &node.interrupt_tables {
            // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
            all.push(Occupant {
                start,
                end: start.saturating_add(len),
                owner: None,
                what: "a withheld MSI-X table or pending-bit array",
            });
        }
    }
    all
}

/// What else lies in the page `start..start + PAGE_SIZE` that BAR `own`
/// would be widened to, if anything: an occupant not the BAR's own, or a
/// reserved range.
fn page_neighbour(
    own: (usize, u8),
    start: u64,
    occupants: &[Occupant],
    reserved: &Reserved,
    widened: Aperture,
) -> Option<&'static str> {
    let end = start.saturating_add(PAGE_SIZE);
    if let Some(occupant) = occupants.iter().find(|occupant| {
        occupant.owner != Some(own) && occupant.start < end && start < occupant.end
    }) {
        return Some(occupant.what);
    }
    reserved.covers(widened).then_some("a reserved range")
}

/// Widen each PCI BAR aperture shorter than a page, starting on a page
/// boundary, to that whole page when nothing else lies in the page: none of
/// `occupants` ([`occupants`]: any node's aperture, any function's assigned
/// memory BAR, any withheld interrupt table) but the BAR's own, and nothing
/// reserved. Each widening or refusal is said on the console when `say`.
///
/// A driver maps whole pages only ([`Aperture::whole_pages`]), and real
/// hardware has BARs shorter than one: an Intel PCH's AHCI ABAR is 2 KiB,
/// where QEMU's is 4 KiB. A page that holds such a BAR and nothing else maps
/// no neighbour's registers or interrupt table; one that is shared stays
/// refused.
fn widen_lone_partial_bars(
    nodes: &mut [DeviceNode],
    occupants: &[Occupant],
    reserved: &Reserved,
    say: bool,
) {
    for (index, node) in nodes.iter_mut().enumerate() {
        let location = node.location;
        for aperture in &mut node.apertures {
            let Source::Bar {
                slot, offset: 0, ..
            } = aperture.source
            else {
                continue;
            };
            if aperture.whole_pages()
                || !aperture.phys.is_multiple_of(PAGE_SIZE)
                || aperture.len > PAGE_SIZE
            {
                continue;
            }
            let widened = Aperture {
                len: PAGE_SIZE,
                ..*aperture
            };
            if let Some(what) =
                page_neighbour((index, slot), aperture.phys, occupants, reserved, widened)
            {
                if say {
                    crate::println!(
                        "  device   {location} BAR of {} bytes at {:#x} shares its page with {what}: not mappable",
                        aperture.len,
                        aperture.phys
                    );
                }
                continue;
            }
            if say {
                crate::println!(
                    "  device   {location} BAR of {} bytes at {:#x} has its page alone: mapped as the page",
                    aperture.len,
                    aperture.phys
                );
            }
            *aperture = widened;
        }
    }
}

/// Every device node, once boot has published them.
static DEVICES: Once<Vec<Arc<DeviceNode>>> = Once::new();

/// Every device node, or none before boot publishes them.
pub(crate) fn devices() -> &'static [Arc<DeviceNode>] {
    DEVICES.get().map_or(&[], Vec::as_slice)
}

/// What publishing the nodes found.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Report {
    /// Nodes published.
    pub(crate) nodes: usize,
    /// Of those, device tree nodes.
    pub(crate) tree: usize,
    /// Apertures minted.
    pub(crate) apertures: usize,
    /// Of those, apertures a page mapping cannot cover exactly.
    pub(crate) partial_pages: usize,
    /// Apertures withheld because the kernel or another device has the
    /// memory.
    pub(crate) withheld: usize,
    /// MSI-X table and pending-bit ranges withheld from apertures.
    pub(crate) msix_withheld: usize,
    /// Functions whose BARs were left out because memory decoding was off.
    pub(crate) undecoded: usize,
    /// Device tree vectors minted.
    pub(crate) vectors: usize,
    /// Of those, edge-triggered.
    pub(crate) edge: usize,
    /// Firmware's interrupts not minted as vectors.
    pub(crate) vectors_withheld: usize,
    /// PCI functions with an MSI-X table vectors can be minted from.
    pub(crate) msix_tables: usize,
    /// MSI-X vectors the check minted: one, from the first such table, or
    /// none when the architecture has no vector to give.
    pub(crate) msix_minted: usize,
    /// PCI functions offered an MSI message, having no MSI-X table.
    pub(crate) msi_functions: usize,
    /// MSI vectors the check minted: one, from QEMU's `edu`, or none.
    pub(crate) msi_minted: usize,
    /// MSI deliveries the check saw `edu` make, unmasked.
    pub(crate) msi_delivered: usize,
    /// Requests refused, each exactly as the rule requires.
    pub(crate) refusals: usize,
}

/// A device node that broke the rule its tokens exist to enforce.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Failure {
    /// The node.
    pub(crate) location: Location,
    /// What it did.
    pub(crate) what: &'static str,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location, self.what)
    }
}

/// What stopped [`publish`].
pub(crate) enum Stopped<'f> {
    /// A finder failed; nothing was published.
    Finder {
        /// Which: `pci`, `tree`, `board`.
        name: &'static str,
        /// Why, as the finder says it.
        why: &'f dyn fmt::Display,
    },
    /// A node broke the rule every node is held to.
    Node(Failure),
    /// `publish` was called again; it ran once, and nothing was touched.
    Again,
}

/// Whether [`publish`] has been called: it runs once, at boot.
static PUBLISHING: AtomicBool = AtomicBool::new(false);

impl From<Failure> for Stopped<'_> {
    fn from(failure: Failure) -> Self {
        Stopped::Node(failure)
    }
}

/// The framebuffer lent to its own node is still the memory of the function
/// that holds it, which a driver of that function could reset or remap under
/// the screen: while it is lent, that function is not driven.
fn yield_framebuffer_holder(nodes: &mut [DeviceNode]) {
    if nodes.iter().any(|node| node.framebuffer_mode().is_some()) {
        for node in nodes.iter_mut().filter(|node| node.holds_framebuffer) {
            node.yielded = true;
        }
    }
}

/// The command-line word that lets the Intel integrated GPU be driven
/// (`docs/INTEL-GPU.md` §4.4 (b)): off unless said, and no profile a claim
/// is made of permits it.
pub(crate) const INTEL_GPU_OPTION: &str = "ferrix.intelgpu";

/// Whether the command line said [`INTEL_GPU_OPTION`], as stage 10 read it
/// before publishing.
static INTEL_GPU_ALLOWED: AtomicBool = AtomicBool::new(false);

/// The node a boot check treats as the Intel integrated GPU, as its index
/// in [`devices`] plus one; 0 for none. Written by the chardev core's check
/// alone, around the cases it runs, as `remapping::FORCED_UNISOLATED` is by
/// the IOMMU's: QEMU has no Intel GPU to show the rules on.
pub(crate) static FORCED_INTEL_GPU: AtomicUsize = AtomicUsize::new(0);

/// Set by the chardev core's check alone, around the cases it runs: every
/// domain counts as untranslated for [`DeviceNode::may_have_client_nodes`].
/// QEMU's machines translate every PCI function.
pub(crate) static FORCED_UNTRANSLATED: AtomicBool = AtomicBool::new(false);

/// Let the Intel integrated GPU be driven: stage 10 calls it before
/// [`publish`] when the command line said [`INTEL_GPU_OPTION`].
pub(crate) fn allow_intel_gpu() {
    INTEL_GPU_ALLOWED.store(true, Ordering::Release);
}

/// Whether the command line said [`INTEL_GPU_OPTION`].
pub(crate) fn intel_gpu_allowed() -> bool {
    INTEL_GPU_ALLOWED.load(Ordering::Acquire)
}

/// Whether a PCI function at `address`, of `vendor` and `class` (base,
/// sub-class and interface, as [`PciFunction::class`] holds them), is the
/// Intel integrated GPU: Intel's, display class, at `00:02.0` on the first
/// segment, where every Intel integrated GPU sits.
pub(crate) const fn is_intel_gpu(address: Address, vendor: u16, class: u32) -> bool {
    vendor == 0x8086
        && class >> 16 == 0x03
        && address.segment() == 0
        && address.bus() == 0
        && address.device() == 2
        && address.function() == 0
}

/// The node the firmware's framebuffer is lent to in place of a node of its
/// own: the Intel integrated GPU holding it, when `allowed`, which is whether
/// the command line said [`INTEL_GPU_OPTION`] (`L.device.46`). `None` for any
/// other machine, which lends it to a node of its own as before.
pub(crate) fn lend_target(nodes: &[DeviceNode], allowed: bool) -> Option<usize> {
    if !allowed {
        return None;
    }
    nodes
        .iter()
        .position(|node| node.intel_gpu() && node.holds_framebuffer)
}

/// The Intel integrated GPU is not driven unless `allowed`, which is
/// whether the command line said [`INTEL_GPU_OPTION`] (`L.device.44`).
fn yield_intel_gpu(nodes: &mut [DeviceNode], allowed: bool) {
    if allowed {
        return;
    }
    for node in nodes.iter_mut().filter(|node| node.intel_gpu()) {
        node.yielded = true;
    }
}

/// Run `finders` in order, printing each one's lines after it, then publish
/// every node they found after requiring each to hand out exactly what it
/// has, and no two to hand out the same memory or the same interrupt.
///
/// What a finder hands back is checked here like any other node: see
/// [`Finder`] for the contract.
///
/// It runs once. A second call is refused before any finder runs, so
/// nothing a finder would touch -- configuration space, device memory -- is
/// touched again, and no node can be published after boot.
///
/// # Errors
///
/// A finder that failed, before anything is published; or the first node
/// that mints an aperture or vector it should refuse, refuses one it should
/// mint, holds an aperture overlapping reserved memory, shares an aperture
/// or vector with another node, or mints an MSI-X vector that does not
/// behave as one.
pub(crate) fn publish<'f>(
    finders: &'f mut [&mut dyn Finder],
    reserved: &Reserved,
) -> Result<Report, Stopped<'f>> {
    if PUBLISHING.swap(true, Ordering::AcqRel) {
        return Err(Stopped::Again);
    }
    let mut nodes = Vec::new();
    let mut cx = Context::new(reserved);
    let mut failed = None;
    for (index, finder) in finders.iter_mut().enumerate() {
        if finder.find(&mut cx, &mut nodes).is_err() {
            failed = Some(index);
            break;
        }
        finder.report();
    }
    let finders: &'f [&mut dyn Finder] = finders;
    if let Some(finder) = failed.and_then(|index| finders.get(index)) {
        return Err(Stopped::Finder {
            name: finder.name(),
            why: finder.failure().unwrap_or(&OutOfMemory),
        });
    }
    let mut report = Report {
        tree: nodes
            .iter()
            .filter(|node| matches!(node.location, Location::VirtioMmio(_) | Location::Tree(_)))
            .count(),
        ..Report::default()
    };

    // What a page widened for a sub-page BAR must not reach, before clashes
    // are dropped: a dropped aperture is still the device's registers.
    let occupied = occupants(&nodes);
    // Two nodes with overlapping apertures are two drivers for one set of
    // registers, so a later node loses what an earlier one already holds.
    let mut claimed: Vec<(u64, u64)> = Vec::new();
    for (index, node) in nodes.iter_mut().enumerate() {
        node.index = index;
        let before = node.apertures.len();
        node.apertures.retain(|aperture| {
            let clash = claimed
                .iter()
                .any(|&(start, end)| aperture.overlaps(start, end));
            if !clash {
                // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
                claimed.push((aperture.phys, aperture.end()));
            }
            !clash
        });
        node.withheld += before - node.apertures.len();
    }
    widen_lone_partial_bars(&mut nodes, &occupied, reserved, true);

    yield_framebuffer_holder(&mut nodes);
    yield_intel_gpu(&mut nodes, intel_gpu_allowed());

    for node in &nodes {
        check::check_node(node, reserved, &mut report)?;
    }
    check::check_together(&nodes, reserved)?;

    // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
    let published = DEVICES.call_once(|| nodes.into_iter().map(Arc::new).collect());
    report.nodes = published.len();
    check::check_vectors(published, &mut report)?;
    check::check_dma_switch(published)?;
    Ok(report)
}
