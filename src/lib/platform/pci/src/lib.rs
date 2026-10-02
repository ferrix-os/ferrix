//! PCI configuration space, as arithmetic over an abstract accessor.
//!
//! Stage 10 of `docs/ROADMAP.md` enumerates buses in the kernel and drives
//! devices from user processes. Both halves read configuration space: the
//! kernel to find what is there and what apertures it decodes, a driver to
//! find its device's capabilities. What they read was written by a device —
//! or, for a function behind a bridge nobody configured, by nothing at all —
//! so this is a parser of hostile bytes like `src/lib/platform/acpi` and `src/lib/platform/elf`, and
//! it lives here for the same reason they do.
//!
//! # Why configuration space is behind a trait
//!
//! Configuration space is not one buffer. Every function has its own 4 KiB,
//! reached on a modern machine through an ECAM window firmware described in
//! the MCFG table or a device tree node, and the walk that finds functions
//! decides which of those windows to read next from what the last one said.
//! So the caller implements [`ConfigSpace`] — the kernel over its mapping of
//! the ECAM window, the tests over a map of fake functions — and this crate
//! does the rest without a pointer, an allocation or `unsafe`.
//!
//! The trait answers the way hardware does. A function that does not exist
//! reads all ones, and so does an offset past the end of its space; a write
//! nowhere is discarded. Nothing here needs a second way of saying "absent".
//!
//! # What is here
//!
//! * [`Address`] and the [`ecam`] geometry that turns one into an offset.
//! * [`header`] — the type 0 and type 1 headers, and the class code.
//! * [`bar`] — decoding a base address register, and sizing one, which is a
//!   write-read-restore sequence with two ways to get it wrong that are both
//!   invisible until a second device lands in the same aperture.
//! * [`capability`] — the standard and extended capability lists, walked with
//!   a bound and a visited set, and MSI-X decoded from its capability.
//! * [`msix`] — MSI-X table entries, the messages the local APIC and a
//!   `GICv2m` frame expect, and the pages of a BAR a driver must not be given.
//! * [`walk`] — the bus walk: every function reachable from a root bus through
//!   bridges firmware configured, without recursion and without allocation.
//! * [`virtio`] — the virtio 1.x PCI transport's vendor capabilities, which
//!   are how a virtio device says where in its BARs its registers are.
//!
//! # Totality
//!
//! Every pointer a device supplies is range-checked before it is followed,
//! every list walk is bounded by the number of entries the space can hold,
//! and every arithmetic step on a device's value is checked. A device whose
//! capability list points at itself gets a [`PciError`], not a hang, and the
//! `pci_walk` fuzz target holds the crate to that over configuration spaces
//! nobody chose.
//!
//! ```
//! # use ferrix_pci::{ConfigSpace, walk::Walk};
//! # fn example<C: ConfigSpace>(space: &C) {
//! for found in Walk::new(space, 0, 0..=255) {
//!     match found {
//!         Ok(function) => {
//!             let _ = (function.address, function.identity.vendor);
//!         }
//!         Err(bridge) => {
//!             // A bridge the walk could not follow; the rest continues.
//!             let _ = bridge;
//!         }
//!     }
//! }
//! # }
//! ```

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

pub mod bar;
pub mod capability;
pub mod ecam;
pub mod header;
pub mod msix;
pub mod topology;
pub mod virtio;
pub mod walk;

#[cfg(test)]
mod tests;

/// Bytes of configuration space every function has on PCI Express.
///
/// Conventional PCI has 256; the extended space above that is reachable only
/// through ECAM, and reads all ones through any other mechanism.
pub const CONFIG_SPACE_SIZE: u16 = 4096;

/// Bytes of the conventional part of configuration space, where the header
/// and the standard capability list live.
pub const LEGACY_CONFIG_SPACE_SIZE: u16 = 256;

/// Devices on one bus.
pub const DEVICES_PER_BUS: u8 = 32;

/// Functions in one device.
pub const FUNCTIONS_PER_DEVICE: u8 = 8;

/// Reads and writes one function's configuration space.
///
/// Widths are separate methods rather than one generic read because hardware
/// does not promise that a four-byte read of a one-byte register is the same
/// as a one-byte read — the accessor performs exactly the access asked for.
///
/// Implementations answer as hardware does: a function that is not present,
/// and an offset at or beyond [`CONFIG_SPACE_SIZE`], read as all ones, and a
/// write to either is discarded.
pub trait ConfigSpace {
    /// Read the byte at `offset` in `function`'s space.
    fn read8(&self, function: Address, offset: u16) -> u8;

    /// Read the little-endian 16-bit register at `offset`.
    fn read16(&self, function: Address, offset: u16) -> u16;

    /// Read the little-endian 32-bit register at `offset`.
    fn read32(&self, function: Address, offset: u16) -> u32;

    /// Write the 16-bit register at `offset`.
    fn write16(&mut self, function: Address, offset: u16, value: u16);

    /// Write the 32-bit register at `offset`.
    fn write32(&mut self, function: Address, offset: u16, value: u32);
}

/// Where a function is: segment, bus, device and function.
///
/// The fields are private so that an `Address` always names a function that
/// could exist — a device number below 32 and a function number below 8 —
/// and every accessor can shift them into an ECAM offset without checking
/// again.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    /// The PCI segment group, which on every machine Ferrix boots on is 0.
    segment: u16,
    /// The bus number.
    bus: u8,
    /// The device number, below [`DEVICES_PER_BUS`].
    device: u8,
    /// The function number, below [`FUNCTIONS_PER_DEVICE`].
    function: u8,
}

impl Address {
    /// The function at `segment:bus:device.function`, or `None` if the device
    /// or function number is out of range.
    #[must_use]
    pub const fn new(segment: u16, bus: u8, device: u8, function: u8) -> Option<Self> {
        if device >= DEVICES_PER_BUS || function >= FUNCTIONS_PER_DEVICE {
            return None;
        }
        Some(Address {
            segment,
            bus,
            device,
            function,
        })
    }

    /// The segment group.
    #[must_use]
    pub const fn segment(self) -> u16 {
        self.segment
    }

    /// The bus number.
    #[must_use]
    pub const fn bus(self) -> u8 {
        self.bus
    }

    /// The requester ID the function's DMA and messages arrive as:
    /// `bus << 8 | device << 3 | function`, which is what a DMAR device scope,
    /// an IORT root complex and a device tree `iommu-map` all translate.
    #[must_use]
    pub const fn requester_id(self) -> u16 {
        (self.bus as u16) << 8 | (self.device as u16) << 3 | self.function as u16
    }

    /// The device number.
    #[must_use]
    pub const fn device(self) -> u8 {
        self.device
    }

    /// The function number.
    #[must_use]
    pub const fn function(self) -> u8 {
        self.function
    }
}

/// The notation Linux prints and `lspci` accepts: `0000:00:1f.2`.
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{}",
            self.segment, self.bus, self.device, self.function
        )
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Why a function's configuration space could not be read as what it claims.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PciError {
    /// A capability pointer is below the header, above the space, or not
    /// aligned to four bytes. Carries where the pointer was read and what it
    /// said.
    CapabilityPointer {
        /// The function.
        function: Address,
        /// The offset of the pointer.
        at: u16,
        /// The value it held.
        pointer: u16,
    },
    /// A capability list returns to an entry it has already visited, or is
    /// longer than its part of the space can hold.
    CapabilityLoop {
        /// The function.
        function: Address,
        /// The offset of the capability seen twice, or of the one past the
        /// bound.
        at: u16,
    },
    /// A capability is shorter than its own format requires.
    CapabilityTooShort {
        /// The function.
        function: Address,
        /// The offset of the capability.
        at: u16,
        /// The length it declared, or the space left before the end of
        /// configuration space.
        len: u16,
    },
    /// A memory BAR's type field holds a reserved value.
    ReservedBarType {
        /// The function.
        function: Address,
        /// Which BAR.
        index: u8,
    },
    /// A 64-bit memory BAR occupies the last BAR slot, so its upper half would
    /// be a register that is not a BAR.
    TruncatedBar {
        /// The function.
        function: Address,
        /// Which BAR.
        index: u8,
    },
    /// The BAR index is past the header's last BAR, or names the upper half of
    /// a 64-bit BAR rather than a BAR.
    NoSuchBar {
        /// The function.
        function: Address,
        /// The index asked for.
        index: u8,
    },
    /// Sizing a BAR read back address bits that are not one contiguous run,
    /// so they describe no size.
    BarMask {
        /// The function.
        function: Address,
        /// Which BAR.
        index: u8,
        /// The mask read back, both halves for a 64-bit BAR.
        mask: u64,
    },
    /// A BAR's assigned address is not a multiple of its size.
    BarPlacement {
        /// The function.
        function: Address,
        /// Which BAR.
        index: u8,
    },
    /// The function's header type is not one this operation reads.
    HeaderType {
        /// The function.
        function: Address,
        /// The header type's low seven bits.
        kind: u8,
    },
    /// A virtio capability names a BAR that does not exist, or a region that
    /// does not fit in the BAR it names.
    VirtioRegion {
        /// The function.
        function: Address,
        /// The offset of the capability.
        at: u16,
    },
}

impl fmt::Display for PciError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PciError::CapabilityPointer {
                function,
                at,
                pointer,
            } => write!(
                f,
                "{function}: capability pointer at {at:#x} says {pointer:#x}, outside the list's space"
            ),
            PciError::CapabilityLoop { function, at } => {
                write!(f, "{function}: capability list loops at {at:#x}")
            }
            PciError::CapabilityTooShort { function, at, len } => write!(
                f,
                "{function}: capability at {at:#x} is {len} bytes, shorter than its format"
            ),
            PciError::ReservedBarType { function, index } => {
                write!(f, "{function}: BAR {index} has a reserved memory type")
            }
            PciError::TruncatedBar { function, index } => write!(
                f,
                "{function}: BAR {index} is 64-bit but is the last BAR slot"
            ),
            PciError::NoSuchBar { function, index } => {
                write!(f, "{function}: there is no BAR {index}")
            }
            PciError::BarMask {
                function,
                index,
                mask,
            } => write!(
                f,
                "{function}: BAR {index} sized to mask {mask:#x}, which is no power of two"
            ),
            PciError::BarPlacement { function, index } => {
                write!(f, "{function}: BAR {index} is not aligned to its size")
            }
            PciError::HeaderType { function, kind } => {
                write!(f, "{function}: header type {kind:#x} is not one this reads")
            }
            PciError::VirtioRegion { function, at } => write!(
                f,
                "{function}: virtio capability at {at:#x} names a region outside its BAR"
            ),
        }
    }
}
