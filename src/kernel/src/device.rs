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
//! Masking an MSI-X vector is a write to its entry's mask bit, and masking an
//! MSI vector a write to its capability's mask bit, or its enable bit where it
//! has none. The interrupt controller can reach neither, so [`Vector::mask`]
//! knows which kind it is. It takes no lock: an interrupt handler calls it.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::{BootView, MemKind, PAGE_SIZE};
use ferrix_fdt::{GicInterrupt, Trigger as TreeTrigger};
use ferrix_native_abi::types::{
    DEVICE_NOT_PCI, DEVICE_TREE_BLOCKS, DEVICE_VIRTIO_PCI, DeviceBlock, DeviceInfo,
    TREE_GS201_DWC3, TREE_STM32_USBH, USB_INPUT_FUNCTIONS,
};
use ferrix_pci::Address;
use ferrix_pci::bar::{Bar, Region};
use ferrix_pci::capability::{Capability, MSIX_ENTRY_SIZE, MsiX};
use ferrix_pci::header::{
    COMMAND, COMMAND_BUS_MASTER, COMMAND_INTERRUPT_DISABLE, COMMAND_MEMORY_SPACE, Identity,
};
use ferrix_pci::msi::{self, Msi};
use ferrix_pci::msix::{
    self, CAPABILITY_CONTROL, CONTROL_ENABLE, CONTROL_FUNCTION_MASK, ENTRY_ADDRESS_HIGH,
    ENTRY_ADDRESS_LOW, ENTRY_DATA, ENTRY_VECTOR_CONTROL, VECTOR_CONTROL_MASKED,
};
use ferrix_pci::virtio::{Location as VirtioLocation, SharedMemory, Transport};
use ferrix_sync::{IrqSpinLock, Once};

use crate::discovery::description::{self, Description};
use crate::discovery::finder::{Context, Finder, OutOfMemory};
use crate::fallible::{self, AllocError};
use crate::mmio::Mmio;
use crate::sync::SpinLock;
use crate::{arch, iommu, irq, vmap};

pub(crate) mod check;

/// GIC interrupt identifiers below this are software-generated or private to
/// one core, and neither is a device's line.
const FIRST_SHARED_INTERRUPT: u32 = 32;

/// Bytes of a function's legacy configuration space, which holds every
/// standard capability.
const LEGACY_CONFIG_BYTES: u64 = 256;

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
    #[expect(dead_code, reason = "read by stage 9's IoMapping::new")]
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
                .and_then(|node| node.msi.as_ref())
                .and_then(MsiFunction::is_masked),
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
            Masking::Msi { node } => devices()
                .get(node)
                .and_then(|node| node.msi.as_ref())
                .ok_or("the vector's device is not published")?
                .set_masked(masked),
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
        if framebuffer.is_present() {
            reserved.add(framebuffer.phys, framebuffer.size);
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
    fn covers(&self, aperture: Aperture) -> bool {
        self.ranges
            .iter()
            .any(|&(start, end)| aperture.overlaps(start, end))
    }
}

/// A PCI function's MSI capability, for one that has no MSI-X table, and
/// the one vector minted from it.
///
/// One message, never more (`ferrix_pci::msi`). Minting turns `INTx` off in
/// the command register, so a function whose MSI is masked by its enable bit
/// cannot fall back to a pin nothing listens on, and leaves the message
/// masked. A function that can mask its vector is masked by its mask bit; one
/// that cannot is masked by turning MSI off, which drops what it would have
/// raised meanwhile. That is safe only for what [`Vector::coalesces`] masks:
/// a vector already pending, whose driver services the device anyway, and the
/// mask a holder's drop leaves.
struct MsiFunction {
    /// The function's requester ID, which its message writes carry.
    requester: u32,
    /// Physical address of the function's configuration space.
    config_phys: u64,
    /// The capability, decoded.
    msi: Msi,
    /// The function's legacy configuration space, mapped from the first mint
    /// for good. Read by interrupt handlers, and `Once::get` takes no lock.
    config: Once<Result<Mmio, &'static str>>,
    /// The vector minted, once one is.
    minted: IrqSpinLock<Option<u32>, arch::Irq>,
}

impl fmt::Debug for MsiFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MsiFunction")
            .field("requester", &self.requester)
            .field("config_phys", &self.config_phys)
            .field("msi", &self.msi)
            .finish_non_exhaustive()
    }
}

impl MsiFunction {
    /// The MSI capability `seen` found: only for a function with no MSI-X
    /// capability to offer instead, on a machine that does not signal by
    /// line.
    fn of(address: Address, seen: &Seen<'_>, has_msix: bool) -> Option<Self> {
        if has_msix || seen.intx.is_some() {
            return None;
        }
        Some(MsiFunction {
            requester: u32::from(address.requester_id()),
            config_phys: seen.config_phys?,
            msi: seen.msi?,
            config: Once::new(),
            minted: IrqSpinLock::new(None),
        })
    }

    /// The configuration space, once the first mint mapped it.
    fn mapped(&self) -> Option<Mmio> {
        match self.config.get() {
            Some(&Ok(config)) => Some(config),
            _ => None,
        }
    }

    /// Mask or unmask the vector. Takes no lock.
    fn set_masked(&self, masked: bool) -> Result<(), &'static str> {
        let config = self
            .mapped()
            .ok_or("the vector's MSI capability is not mapped")?;
        match self.msi.mask() {
            Some(at) => {
                // Bit 0 is the one message; the other bits are vectors this
                // never enables, kept as they are.
                let value = config.read32(u64::from(at));
                config.write32(u64::from(at), if masked { value | 1 } else { value & !1 });
            }
            None => {
                let at = u64::from(self.msi.capability + msi::CONTROL);
                let control = config.read16(at);
                config.write16(
                    at,
                    if masked {
                        control & !msi::CONTROL_ENABLE
                    } else {
                        Msi::enabled(control)
                    },
                );
            }
        }
        Ok(())
    }

    /// Whether the vector reads back masked.
    fn is_masked(&self) -> Option<bool> {
        let config = self.mapped()?;
        Some(match self.msi.mask() {
            Some(at) => config.read32(u64::from(at)) & 1 != 0,
            None => {
                config.read16(u64::from(self.msi.capability + msi::CONTROL)) & msi::CONTROL_ENABLE
                    == 0
            }
        })
    }

    /// The vector, minting it if nothing has.
    ///
    /// The first mint maps configuration space, turns `INTx` off, programs
    /// the message with the vector masked, and enables one message --
    /// masked by its enable bit where there is no mask bit, which means
    /// leaving MSI off until the holder unmasks it.
    fn mint(&self, node: usize) -> Result<Vector, &'static str> {
        let vector = |number| Vector {
            number,
            trigger: Some(Trigger::Edge),
            masking: Masking::Msi { node },
        };
        let config = (*self.config.call_once(|| {
            vmap::map_device(self.config_phys, LEGACY_CONFIG_BYTES)
                .map(Mmio::at)
                .map_err(|_| "the function's configuration space could not be mapped")
        }))?;
        let mut minted = self.minted.lock();
        if let Some(number) = *minted {
            return Ok(vector(number));
        }
        let control_at = u64::from(self.msi.capability + msi::CONTROL);
        let control = config.read16(control_at);
        // Off while it is programmed, so that no half-written message is
        // ever sent.
        config.write16(control_at, control & !msi::CONTROL_ENABLE);
        let command = config.read16(u64::from(COMMAND));
        config.write16(u64::from(COMMAND), command | COMMAND_INTERRUPT_DISABLE);
        let message = arch::msi_allocate(self.requester)?;
        if !self.msi.reaches(message.address) {
            return Err("the message's address is above what a 32-bit MSI capability holds");
        }
        let base = u64::from(self.msi.capability);
        config.write32(base + u64::from(msi::ADDRESS_LOW), message.address as u32);
        if self.msi.wide {
            config.write32(
                base + u64::from(msi::ADDRESS_HIGH),
                (message.address >> 32) as u32,
            );
        }
        config.write16(u64::from(self.msi.data()), message.data as u16);
        if let Some(at) = self.msi.mask() {
            let value = config.read32(u64::from(at));
            config.write32(u64::from(at), value | 1);
            config.write16(control_at, Msi::enabled(control));
        }
        *minted = Some(message.number);
        Ok(vector(message.number))
    }
}

/// A PCI function's MSI-X table, and the vectors minted from it.
struct MsixTable {
    /// The function's requester ID, which its message writes carry and a
    /// GICv3's ITS translates them by.
    requester: u32,
    /// Physical address of the function's configuration space.
    config_phys: u64,
    /// Offset of the MSI-X capability in it.
    capability: u16,
    /// Physical address of the table's first entry.
    table_phys: u64,
    /// Entries in the table.
    table_size: u16,
    /// The table, mapped with every entry masked and MSI-X on, from the first
    /// mint. Read by interrupt handlers, and `Once::get` takes no lock.
    table: Once<Result<Mmio, &'static str>>,
    /// The vector each entry was minted with.
    minted: IrqSpinLock<BTreeMap<u16, u32>, arch::Irq>,
}

impl fmt::Debug for MsixTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MsixTable")
            .field("requester", &self.requester)
            .field("config_phys", &self.config_phys)
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
    /// mask clear. Once per table; every entry stays masked until its holder
    /// unmasks it.
    fn open(&self) -> Result<Mmio, &'static str> {
        let len = u64::from(self.table_size) * MSIX_ENTRY_SIZE;
        let table = vmap::map_device(self.table_phys, len)
            .map(Mmio::at)
            .map_err(|_| "the MSI-X table could not be mapped")?;
        for entry in 0..self.table_size {
            let at = Self::control(entry);
            table.write32(at, table.read32(at) | VECTOR_CONTROL_MASKED);
        }

        let config = vmap::map_device(self.config_phys, LEGACY_CONFIG_BYTES)
            .map_err(|_| "the function's configuration space could not be mapped")?;
        let registers = Mmio::at(config);
        let at = u64::from(self.capability + CAPABILITY_CONTROL);
        let control = registers.read16(at);
        registers.write16(at, (control | CONTROL_ENABLE) & !CONTROL_FUNCTION_MASK);
        let _ = vmap::unmap_device(config);
        Ok(table)
    }

    /// The vector for `entry`, minting it if nothing has.
    fn mint(&self, node: usize, entry: u16) -> Result<Vector, &'static str> {
        let vector = |number| Vector {
            number,
            trigger: Some(Trigger::Edge),
            masking: Masking::MsiX { node, entry },
        };
        if entry >= self.table_size {
            return Err("there is no such MSI-X entry");
        }
        // Mapped outside the lock: mapping and unmapping take the address
        // space's locks and may wait on other processors.
        let table = (*self.table.call_once(|| self.open()))?;

        let mut minted = self.minted.lock();
        if let Some(&number) = minted.get(&entry) {
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
        let _ = fallible::insert_held(&held, &mut minted, entry, msi.number);
        Ok(vector(msi.number))
    }
}

/// One of a virtio PCI transport's register blocks, as physical memory: the
/// page-aligned start of the pages holding it, inside one of the function's
/// BARs, the block's first byte within those pages, and its length. What a
/// driver, which cannot walk configuration space, is told in START.
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
}

/// What enumeration read off a PCI function and kept, because whoever starts
/// a driver on it needs it and a driver cannot read configuration space.
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
}

impl DeviceNode {
    /// A node with nothing in it yet.
    pub(crate) const fn empty(location: Location) -> Self {
        DeviceNode {
            location,
            pci: None,
            dma_on: AtomicBool::new(false),
            index: 0,
            apertures: Vec::new(),
            vectors: Vec::new(),
            msix: None,
            msi: None,
            domain: SpinLock::new(None),
            withheld: 0,
            interrupt_tables: Vec::new(),
            undecoded: false,
            withheld_vectors: 0,
            binding: 0,
            dma: DmaShape::ORDINARY,
        }
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
    pub(crate) fn pci(
        address: Address,
        seen: &Seen<'_>,
        regions: &[Region],
        msix: Option<&(Capability, MsiX)>,
        decoding: bool,
        reserved: &Reserved,
    ) -> Self {
        let mut node = DeviceNode::empty(Location::Pci(address));
        let identity = seen.identity;
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
        if !decoding {
            node.undecoded = regions
                .iter()
                .any(|region| matches!(region.bar, Bar::Memory { .. }));
            return node;
        }
        let table = msix.map(|(_, table)| table);
        for region in regions {
            let Bar::Memory {
                address: base,
                prefetchable,
                ..
            } = region.bar
            else {
                continue;
            };
            if base == 0 {
                continue;
            }
            for &(offset, len) in msix::withheld(region, table, PAGE_SIZE).as_slice() {
                if let Some(start) = base.checked_add(offset) {
                    // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
                    node.interrupt_tables.push((start, len));
                }
            }
            for &(offset, len) in msix::mappable(region, table, PAGE_SIZE).as_slice() {
                if let Some(start) = base.checked_add(offset) {
                    node.mint(start, len, prefetchable, reserved);
                }
            }
        }
        node.msix = seen
            .config_phys
            .zip(offered)
            .and_then(|(config_phys, found)| {
                MsixTable::of(address, config_phys, found, regions, reserved)
            });
        node.msi = MsiFunction::of(address, seen, msix.is_some());
        node
    }

    /// Add an aperture of `len` bytes at `phys`, unless it is empty, runs off
    /// the address space, or overlaps reserved memory.
    pub(crate) fn mint(&mut self, phys: u64, len: u64, cacheable: bool, reserved: &Reserved) {
        if phys == 0 || len == 0 || phys.checked_add(len).is_none() {
            return;
        }
        let aperture = Aperture {
            phys,
            len,
            cacheable,
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
    pub(crate) const fn input_functions(&self) -> usize {
        match self.location {
            Location::Tree(_) if self.binding == TREE_STM32_USBH => USB_INPUT_FUNCTIONS,
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
    /// keeping memory decoding on either way, through a mapping of its
    /// configuration space held only for the write.
    fn set_bus_master(&self, on: bool) -> Result<(), &'static str> {
        let Some(config_phys) = self.pci.as_ref().and_then(|function| function.config_phys) else {
            return Ok(());
        };
        let config = vmap::map_device(config_phys, LEGACY_CONFIG_BYTES)
            .map_err(|_| "the function's configuration space could not be mapped")?;
        let registers = Mmio::at(config);
        let at = u64::from(COMMAND);
        let command = registers.read16(at);
        let value = if on {
            command | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER
        } else {
            command & !COMMAND_BUS_MASTER
        };
        registers.write16(at, value);
        let _ = vmap::unmap_device(config);
        Ok(())
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
        let mut held = self.domain.lock();
        if let Some(domain) = held.as_ref() {
            return Ok(Arc::clone(domain));
        }
        let domain = fallible::try_arc(match self.location {
            Location::Pci(address) => iommu::domain_for(address),
            Location::VirtioMmio(_) | Location::Tree(_) => iommu::Domain::untranslated(),
        })?;
        *held = Some(Arc::clone(&domain));
        Ok(domain)
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
            .map(|whole| Aperture {
                phys,
                len,
                cacheable: whole.cacheable,
            })
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
            (Some(table), _) => table.mint(self.index, u16::try_from(index).ok()?).ok(),
            (None, Some(msi)) if index == 0 => msi.mint(self.index).ok(),
            _ => None,
        }
    }
}

impl MsixTable {
    /// The table `msix` describes, if vectors can be minted from it: in an
    /// assigned memory BAR, whole, and clear of memory the kernel owns.
    fn of(
        address: Address,
        config_phys: u64,
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
            config_phys,
            capability: capability.offset,
            table_phys,
            table_size: msix.table_size,
            table: Once::new(),
            minted: IrqSpinLock::new(BTreeMap::new()),
        })
    }
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

    for node in &nodes {
        check::check_node(node, reserved, &mut report)?;
    }
    check::check_exclusive(&nodes)?;

    // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
    let published = DEVICES.call_once(|| nodes.into_iter().map(Arc::new).collect());
    report.nodes = published.len();
    check::check_vectors(published, &mut report)?;
    check::check_dma_switch(published)?;
    Ok(report)
}
