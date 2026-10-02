//! Capability lists: how a function says what else it can do.
//!
//! Two singly linked lists, each threaded through the function's own
//! configuration space by pointers the device supplies. The standard list
//! lives in the first 256 bytes and starts at the header's capabilities
//! pointer; the extended list lives above them, starts at `0x100`, and exists
//! only on PCI Express functions reached through ECAM.
//!
//! A device's pointers are followed exactly as far as they can be checked.
//! Each list is walked with a visited set over every offset it could
//! possibly use, so a list that points back at itself — which is one stuck bit
//! in the wrong register away — ends in [`PciError::CapabilityLoop`] rather
//! than in a kernel that never finishes enumerating.

use crate::header::{HEADER_TYPE, HeaderKind, STATUS, STATUS_CAPABILITIES_LIST};
use crate::{Address, CONFIG_SPACE_SIZE, ConfigSpace, LEGACY_CONFIG_SPACE_SIZE, PciError};

/// Capability ID: power management.
pub const ID_POWER_MANAGEMENT: u8 = 0x01;
/// Capability ID: message signalled interrupts.
pub const ID_MSI: u8 = 0x05;
/// Capability ID: vendor-specific, which is what virtio's are.
pub const ID_VENDOR: u8 = 0x09;
/// Capability ID: PCI Express.
pub const ID_PCI_EXPRESS: u8 = 0x10;
/// Capability ID: MSI-X.
pub const ID_MSIX: u8 = 0x11;

/// The lowest offset a standard capability may start at: just past the
/// 64-byte header.
pub const FIRST_STANDARD: u16 = 0x40;

/// Where the extended capability list starts.
pub const FIRST_EXTENDED: u16 = LEGACY_CONFIG_SPACE_SIZE;

/// Extended capability ID: advanced error reporting.
pub const EXTENDED_ID_AER: u16 = 0x0001;
/// Extended capability ID: access control services, which is what an IOMMU
/// needs a switch to have before two functions behind it are isolated.
pub const EXTENDED_ID_ACS: u16 = 0x000D;
/// Extended capability ID: address translation services, with which a
/// device caches translations of its own (PCIe base 6.0 §10.5.1).
pub const EXTENDED_ID_ATS: u16 = 0x000F;
/// Where an ATS capability's control register is, from the capability.
pub const ATS_CONTROL: u16 = 0x06;
/// The ATS control register's enable bit.
pub const ATS_CONTROL_ENABLE: u16 = 1 << 15;

/// One entry in the standard list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capability {
    /// The function it belongs to.
    pub function: Address,
    /// What kind of capability it is.
    pub id: u8,
    /// Where it starts.
    pub offset: u16,
}

/// A set of offsets, one bit per four bytes of configuration space.
#[derive(Clone, Copy, Debug)]
struct Seen([u64; 16]);

impl Seen {
    /// Record `offset`, returning whether it was already recorded.
    fn insert(&mut self, offset: u16) -> bool {
        let slot = usize::from(offset / 4);
        let bit = 1_u64 << (slot % 64);
        match self.0.get_mut(slot / 64) {
            Some(word) => {
                let already = *word & bit != 0;
                *word |= bit;
                already
            }
            // Past the end of configuration space; the callers never ask.
            None => true,
        }
    }
}

/// The standard capability list of one function.
///
/// Ends after the first error, since a list with one bad link has no
/// trustworthy remainder.
#[derive(Debug)]
pub struct Capabilities<'s, C: ?Sized> {
    /// The space being read.
    space: &'s C,
    /// The function.
    function: Address,
    /// Where the next pointer was read, for errors.
    pointer_at: u16,
    /// The next capability's offset, or zero at the end.
    next: u16,
    /// Every offset already visited.
    seen: Seen,
}

impl<'s, C: ConfigSpace + ?Sized> Capabilities<'s, C> {
    /// The standard capabilities of `function`. Empty if its status register
    /// says there are none, or its header is of a layout with no known
    /// pointer.
    pub fn new(space: &'s C, function: Address) -> Self {
        let mut list = Capabilities {
            space,
            function,
            pointer_at: 0,
            next: 0,
            seen: Seen([0; 16]),
        };
        if space.read16(function, STATUS) & STATUS_CAPABILITIES_LIST == 0 {
            return list;
        }
        let kind = HeaderKind::from_register(space.read8(function, HEADER_TYPE));
        if let Some(pointer_at) = kind.capabilities_pointer() {
            list.pointer_at = pointer_at;
            list.next = standard_pointer(space.read8(function, pointer_at));
        }
        list
    }
}

/// A standard capability pointer, with the two low bits the specification
/// reserves masked off as it requires.
fn standard_pointer(raw: u8) -> u16 {
    u16::from(raw & !0x3)
}

impl<C: ConfigSpace + ?Sized> Iterator for Capabilities<'_, C> {
    type Item = Result<Capability, PciError>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.next;
        if at == 0 {
            return None;
        }
        self.next = 0;
        if at < FIRST_STANDARD {
            return Some(Err(PciError::CapabilityPointer {
                function: self.function,
                at: self.pointer_at,
                pointer: at,
            }));
        }
        if self.seen.insert(at) {
            return Some(Err(PciError::CapabilityLoop {
                function: self.function,
                at,
            }));
        }
        // `at` is at most 0xFC, so both bytes are inside the legacy space.
        let id = self.space.read8(self.function, at);
        self.pointer_at = at + 1;
        self.next = standard_pointer(self.space.read8(self.function, at + 1));
        Some(Ok(Capability {
            function: self.function,
            id,
            offset: at,
        }))
    }
}

/// The first standard capability of `function` with `id`, if there is one.
///
/// # Errors
///
/// Any error met in the list before a match.
pub fn find<C: ConfigSpace + ?Sized>(
    space: &C,
    function: Address,
    id: u8,
) -> Result<Option<Capability>, PciError> {
    for capability in Capabilities::new(space, function) {
        let capability = capability?;
        if capability.id == id {
            return Ok(Some(capability));
        }
    }
    Ok(None)
}

/// Where a PCI Express capability's capabilities register is, from the
/// capability: its device or port type is bits 7:4.
pub const EXPRESS_CAPABILITIES: u16 = 0x02;
/// Express device or port type: a root port of a root complex.
pub const EXPRESS_ROOT_PORT: u8 = 0x4;
/// Express device or port type: a switch's upstream port.
pub const EXPRESS_UPSTREAM_PORT: u8 = 0x5;
/// Express device or port type: a switch's downstream port.
pub const EXPRESS_DOWNSTREAM_PORT: u8 = 0x6;

/// Whether `function` is a PCI Express port, which forwards the requester ID
/// of what passes through it unchanged: a root port, or a switch's upstream
/// or downstream port. A PCIe-to-PCI bridge, a conventional bridge and
/// anything without the capability are not, since what they forward arrives
/// with their own alias (`crate::topology`).
///
/// # Errors
///
/// What the capability walk refused.
pub fn forwards_requester<C: ConfigSpace + ?Sized>(
    space: &C,
    function: Address,
) -> Result<bool, PciError> {
    let Some(express) = find(space, function, ID_PCI_EXPRESS)? else {
        return Ok(false);
    };
    let kind = (space.read16(function, express.offset + EXPRESS_CAPABILITIES) >> 4) & 0xF;
    Ok(matches!(
        kind as u8,
        EXPRESS_ROOT_PORT | EXPRESS_UPSTREAM_PORT | EXPRESS_DOWNSTREAM_PORT
    ))
}

/// One entry in the extended list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExtendedCapability {
    /// The function it belongs to.
    pub function: Address,
    /// What kind of capability it is.
    pub id: u16,
    /// The capability's own version number.
    pub version: u8,
    /// Where it starts.
    pub offset: u16,
}

/// The extended capability list of one function.
///
/// Empty on a function with no extended space, which reads its first header
/// as all ones through a conventional accessor and as zero on a device that
/// has the space but no capabilities in it.
#[derive(Debug)]
pub struct ExtendedCapabilities<'s, C: ?Sized> {
    /// The space being read.
    space: &'s C,
    /// The function.
    function: Address,
    /// Where the next pointer was read, for errors.
    pointer_at: u16,
    /// The next capability's offset, or zero at the end.
    next: u16,
    /// Every offset already visited.
    seen: Seen,
}

impl<'s, C: ConfigSpace + ?Sized> ExtendedCapabilities<'s, C> {
    /// The extended capabilities of `function`.
    pub const fn new(space: &'s C, function: Address) -> Self {
        ExtendedCapabilities {
            space,
            function,
            pointer_at: FIRST_EXTENDED,
            next: FIRST_EXTENDED,
            seen: Seen([0; 16]),
        }
    }
}

impl<C: ConfigSpace + ?Sized> Iterator for ExtendedCapabilities<'_, C> {
    type Item = Result<ExtendedCapability, PciError>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.next;
        if at == 0 {
            return None;
        }
        self.next = 0;
        // The pointer is twelve bits with the low two masked, so it is below
        // 4096 and aligned; only the lower bound can be wrong.
        if at < FIRST_EXTENDED {
            return Some(Err(PciError::CapabilityPointer {
                function: self.function,
                at: self.pointer_at,
                pointer: at,
            }));
        }
        if self.seen.insert(at) {
            return Some(Err(PciError::CapabilityLoop {
                function: self.function,
                at,
            }));
        }
        let header = self.space.read32(self.function, at);
        if header == 0 || header == u32::MAX {
            return None;
        }
        let [id_low, id_high, version_and_next, next_high] = header.to_le_bytes();
        self.pointer_at = at + 2;
        self.next = (u16::from_le_bytes([version_and_next, next_high]) >> 4) & !0x3;
        debug_assert!(self.next < CONFIG_SPACE_SIZE, "twelve bits, masked");
        Some(Ok(ExtendedCapability {
            function: self.function,
            id: u16::from_le_bytes([id_low, id_high]),
            version: version_and_next & 0xF,
            offset: at,
        }))
    }
}

/// Where in a BAR a structure is: a BAR index and an offset into it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BarOffset {
    /// The BAR.
    pub bar: u8,
    /// Bytes into it.
    pub offset: u32,
}

impl BarOffset {
    /// A register that packs a BAR index into its low three bits and an
    /// offset into the rest, as MSI-X's table and pending-bit registers do.
    #[must_use]
    pub const fn from_register(register: u32) -> Self {
        BarOffset {
            bar: (register & 0x7) as u8,
            offset: register & !0x7,
        }
    }
}

/// Bytes in one MSI-X table entry: address, upper address, data, control.
pub const MSIX_ENTRY_SIZE: u64 = 16;

/// Bytes of the MSI-X capability.
pub const MSIX_CAPABILITY_LEN: u16 = 12;

/// A function's MSI-X capability, decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MsiX {
    /// How many vectors the table holds, from 1 to 2048.
    pub table_size: u16,
    /// Whether MSI-X is enabled.
    pub enabled: bool,
    /// Whether every vector is masked at once.
    pub function_masked: bool,
    /// Where the vector table is.
    pub table: BarOffset,
    /// Where the pending-bit array is.
    pub pending: BarOffset,
}

impl MsiX {
    /// Decode the MSI-X capability `capability`.
    ///
    /// # Errors
    ///
    /// [`PciError::HeaderType`] is never returned; [`PciError::CapabilityTooShort`]
    /// if the capability would run past the legacy space, and
    /// [`PciError::NoSuchBar`] if the table or pending array names a BAR index
    /// above 5.
    pub fn read<C: ConfigSpace + ?Sized>(
        space: &C,
        capability: Capability,
    ) -> Result<Self, PciError> {
        let Capability {
            function, offset, ..
        } = capability;
        let room = LEGACY_CONFIG_SPACE_SIZE.saturating_sub(offset);
        if room < MSIX_CAPABILITY_LEN {
            return Err(PciError::CapabilityTooShort {
                function,
                at: offset,
                len: room,
            });
        }
        let control = space.read16(function, offset + 2);
        let table = BarOffset::from_register(space.read32(function, offset + 4));
        let pending = BarOffset::from_register(space.read32(function, offset + 8));
        for bar in [table.bar, pending.bar] {
            if bar > 5 {
                return Err(PciError::NoSuchBar {
                    function,
                    index: bar,
                });
            }
        }
        Ok(MsiX {
            table_size: (control & 0x7FF) + 1,
            enabled: control & (1 << 15) != 0,
            function_masked: control & (1 << 14) != 0,
            table,
            pending,
        })
    }

    /// Bytes the vector table occupies.
    #[must_use]
    pub const fn table_len(self) -> u64 {
        self.table_size as u64 * MSIX_ENTRY_SIZE
    }

    /// Bytes the pending-bit array occupies: one bit per vector, in whole
    /// 64-bit words.
    #[must_use]
    pub const fn pending_len(self) -> u64 {
        (self.table_size as u64).div_ceil(64) * 8
    }
}
