//! The DMAR table: where Intel VT-d remapping hardware is, and which devices
//! each unit translates for.
//!
//! Stage 10 puts every device a ring-3 driver touches behind an IOMMU domain,
//! and on x86-64 the remapping hardware that enforces a domain is found only
//! here. The table is a short fixed header — the host's address width and
//! three flags — followed by a list of remapping structures, each a 16-bit
//! type and a 16-bit length. The two a kernel acts on are a DRHD, one
//! remapping unit with its register block and the devices behind it, and an
//! RMRR, memory some device must keep reaching whatever the kernel decides —
//! firmware's USB keyboard emulation, typically — which a domain has to leave
//! mapped.
//!
//! Devices are named by *scope*: a type, a start bus, and a path of
//! `(device, function)` hops through bridges. A one-hop path is a function on
//! the start bus. A longer one goes through bridges whose secondary bus
//! numbers are in configuration space rather than in this table, so resolving
//! it is the kernel's job; [`DeviceScope::endpoint`] answers only the case
//! this table can answer on its own.
//!
//! Every length is checked against the bytes it claims, so a zero-length
//! structure or scope ends its walk rather than looping, and one that runs
//! past its container ends it rather than reading a neighbour.

use crate::{AcpiError, Table, u8_at, u16_at, u64_at};

/// The DMAR's signature.
pub const DMAR_SIGNATURE: [u8; 4] = *b"DMAR";

/// Bytes before the first remapping structure: the system description
/// header, the host address width, the flags and ten reserved bytes.
pub const DMAR_HEADER_LEN: usize = 48;

/// Flags: the platform supports interrupt remapping.
pub const FLAG_INTR_REMAP: u8 = 1 << 0;
/// Flags: firmware asks the OS not to enable x2APIC mode.
pub const FLAG_X2APIC_OPT_OUT: u8 = 1 << 1;
/// Flags: firmware supports the OS taking DMA protection over from it.
pub const FLAG_DMA_CTRL_PLATFORM_OPT_IN: u8 = 1 << 2;

/// DRHD flags: the unit translates for every PCI function on its segment
/// that no other DRHD's scope names (VT-d 8.3, `INCLUDE_PCI_ALL`). Its
/// scopes then name only I/O APICs and HPETs, as the N150's `dmar1` does.
pub const DRHD_INCLUDE_PCI_ALL: u8 = 1 << 0;

/// Structure type: a DMA remapping hardware unit definition.
pub const STRUCTURE_DRHD: u16 = 0;
/// Structure type: a reserved memory region reporting structure.
pub const STRUCTURE_RMRR: u16 = 1;
/// Structure type: a root port ATS capability reporting structure.
pub const STRUCTURE_ATSR: u16 = 2;

/// Scope type: a PCI endpoint.
pub const SCOPE_PCI_ENDPOINT: u8 = 1;
/// Scope type: a PCI bridge, and everything beneath it.
pub const SCOPE_PCI_BRIDGE: u8 = 2;
/// Scope type: an I/O APIC, for interrupt remapping.
pub const SCOPE_IOAPIC: u8 = 3;
/// Scope type: an HPET, for interrupt remapping.
pub const SCOPE_HPET: u8 = 4;
/// Scope type: an ACPI namespace device.
pub const SCOPE_ACPI_NAMESPACE: u8 = 5;

/// Bytes of a remapping structure's type and length.
const STRUCTURE_HEADER_LEN: usize = 4;
/// Bytes of a DRHD before its device scopes.
const DRHD_FIXED_LEN: usize = 16;
/// Bytes of an RMRR before its device scopes.
const RMRR_FIXED_LEN: usize = 24;
/// Bytes of a device scope before its path.
const SCOPE_FIXED_LEN: usize = 6;

/// The DMA remapping reporting table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dmar<'a> {
    /// The table everything is read out of.
    table: Table<'a>,
}

impl<'a> Dmar<'a> {
    /// Interpret `table` as a DMAR.
    ///
    /// # Errors
    ///
    /// [`AcpiError::BadSignature`] if it is not one, and
    /// [`AcpiError::TooShort`] if it ends inside the fixed header.
    pub fn parse(table: Table<'a>) -> Result<Self, AcpiError> {
        table.expect_signature(DMAR_SIGNATURE)?;
        if table.bytes().len() < DMAR_HEADER_LEN {
            return Err(AcpiError::TooShort {
                got: table.bytes().len(),
                need: DMAR_HEADER_LEN,
            });
        }
        Ok(Dmar { table })
    }

    /// The segment's catch-all unit: the DRHD with
    /// [`DRHD_INCLUDE_PCI_ALL`] on `segment`, read wherever it stands in
    /// the table. The specification asks firmware to list it last; nothing
    /// here relies on that.
    #[must_use]
    pub fn catch_all(&self, segment: u16) -> CatchAll<'a> {
        let mut found = self.structures().filter_map(|structure| match structure {
            Structure::Drhd(unit) if unit.segment == segment && unit.includes_all() => Some(unit),
            _ => None,
        });
        match (found.next(), found.next()) {
            (None, _) => CatchAll::None,
            (Some(unit), None) => CatchAll::One(unit),
            (Some(_), Some(_)) => CatchAll::Several,
        }
    }

    /// Why `unit`'s translation must stay off until RMRRs are honoured: an
    /// RMRR on its segment names a function behind it, or names one this
    /// table alone cannot place, in which case every unit on the segment
    /// stays off. `None` when no RMRR reaches it.
    ///
    /// Decided before PCI enumeration, as translation goes on before any
    /// device is given DMA, so from this table alone: an RMRR scope must be
    /// a single-hop endpoint, and it is behind the unit whose single-hop
    /// endpoint scope names the same function, or else -- when no DRHD on
    /// the segment has a scope only enumeration can follow (a
    /// sub-hierarchy, a longer path) -- behind the segment's one catch-all.
    /// Anything else is [`RmrrHold::Unfollowed`].
    #[must_use]
    pub fn rmrr_hold(&self, unit: &Drhd<'a>) -> Option<RmrrHold> {
        for structure in self.structures() {
            let region = match structure {
                Structure::Rmrr(region) => region,
                // An RMRR too short for its fields names a region nobody
                // can read: every unit is held, as for one not followed.
                Structure::Malformed {
                    kind: STRUCTURE_RMRR,
                    ..
                } => return Some(RmrrHold::Unfollowed { base: 0, limit: 0 }),
                _ => continue,
            };
            if region.segment != unit.segment {
                continue;
            }
            for scope in region.device_scopes() {
                let held = match self.rmrr_owner(region.segment, &scope) {
                    Owner::Unit(base) if base == unit.register_base => RmrrHold::Named {
                        base: region.base,
                        limit: region.limit,
                        function: scope.endpoint().unwrap_or((0, 0, 0)),
                    },
                    Owner::Unit(_) => continue,
                    Owner::Unfollowed => RmrrHold::Unfollowed {
                        base: region.base,
                        limit: region.limit,
                    },
                };
                return Some(held);
            }
        }
        None
    }

    /// The unit an RMRR scope's function is behind, as [`Dmar::rmrr_hold`]
    /// decides it.
    fn rmrr_owner(&self, segment: u16, scope: &DeviceScope<'_>) -> Owner {
        let Some(named) = scope
            .endpoint()
            .filter(|_| scope.kind == SCOPE_PCI_ENDPOINT)
        else {
            return Owner::Unfollowed;
        };
        let mut open_scopes = false;
        for structure in self.structures() {
            let Structure::Drhd(unit) = structure else {
                continue;
            };
            if unit.segment != segment {
                continue;
            }
            for candidate in unit.device_scopes() {
                match candidate.kind {
                    SCOPE_PCI_ENDPOINT if candidate.endpoint() == Some(named) => {
                        return Owner::Unit(unit.register_base);
                    }
                    SCOPE_PCI_ENDPOINT if candidate.endpoint().is_none() => open_scopes = true,
                    SCOPE_PCI_BRIDGE => open_scopes = true,
                    _ => {}
                }
            }
        }
        match self.catch_all(segment) {
            CatchAll::One(unit) if !open_scopes => Owner::Unit(unit.register_base),
            _ => Owner::Unfollowed,
        }
    }

    /// The underlying table, for its header and checksum.
    #[must_use]
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// How many bits of address a device can put on the bus. Stored less one,
    /// and returned as the width itself.
    #[must_use]
    pub fn host_address_width(&self) -> u16 {
        u8_at(self.table.bytes(), 36).map_or(0, |stored| u16::from(stored) + 1)
    }

    /// The table's flags: [`FLAG_INTR_REMAP`] and its neighbours.
    #[must_use]
    pub fn flags(&self) -> u8 {
        u8_at(self.table.bytes(), 37).unwrap_or(0)
    }

    /// Every remapping structure, in table order.
    #[must_use]
    pub fn structures(&self) -> Structures<'a> {
        Structures {
            rest: self.table.bytes().get(DMAR_HEADER_LEN..).unwrap_or(&[]),
        }
    }
}

/// A segment's catch-all unit ([`Dmar::catch_all`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CatchAll<'a> {
    /// No DRHD on the segment includes every function.
    None,
    /// Exactly one does.
    One(Drhd<'a>),
    /// Two or more do, which the specification does not allow: none of
    /// them is believed.
    Several,
}

/// Why an RMRR keeps a unit's translation off ([`Dmar::rmrr_hold`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RmrrHold {
    /// The region is for a function behind this unit.
    Named {
        /// The region's first byte.
        base: u64,
        /// Its last byte.
        limit: u64,
        /// The function, as `(bus, device, function)`.
        function: (u8, u8, u8),
    },
    /// The region's scope cannot be placed from the table alone, so every
    /// unit on its segment is held.
    Unfollowed {
        /// The region's first byte.
        base: u64,
        /// Its last byte.
        limit: u64,
    },
}

/// Which unit an RMRR's function is behind.
enum Owner {
    /// The unit with these registers.
    Unit(u64),
    /// No unit this table alone can name.
    Unfollowed,
}

/// One remapping structure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Structure<'a> {
    /// A remapping hardware unit.
    Drhd(Drhd<'a>),
    /// A region a device must keep reaching.
    Rmrr(Rmrr<'a>),
    /// A structure this crate does not decode, stepped over by its length.
    Unknown {
        /// The structure's type.
        kind: u16,
        /// Its length.
        length: u16,
    },
    /// A structure of a known type too short for that type's fields.
    Malformed {
        /// The structure's type.
        kind: u16,
        /// Its length.
        length: u16,
    },
}

/// Iterator over a DMAR's remapping structures.
#[derive(Clone, Copy, Debug)]
pub struct Structures<'a> {
    /// What is left of the table after the structures already read.
    rest: &'a [u8],
}

impl<'a> Iterator for Structures<'a> {
    type Item = Structure<'a>;

    fn next(&mut self) -> Option<Structure<'a>> {
        let kind = u16_at(self.rest, 0)?;
        let length = u16_at(self.rest, 2)?;
        let span = usize::from(length);
        let Some(entry) = self
            .rest
            .get(..span)
            .filter(|_| span >= STRUCTURE_HEADER_LEN)
        else {
            self.rest = &[];
            return None;
        };
        self.rest = self.rest.get(span..).unwrap_or(&[]);
        let decoded = match kind {
            STRUCTURE_DRHD => decode_drhd(entry),
            STRUCTURE_RMRR => decode_rmrr(entry),
            _ => return Some(Structure::Unknown { kind, length }),
        };
        Some(decoded.unwrap_or(Structure::Malformed { kind, length }))
    }
}

/// A DRHD: sixteen fixed bytes, then device scopes.
fn decode_drhd(entry: &[u8]) -> Option<Structure<'_>> {
    Some(Structure::Drhd(Drhd {
        flags: u8_at(entry, 4)?,
        segment: u16_at(entry, 6)?,
        register_base: u64_at(entry, 8)?,
        scopes: entry.get(DRHD_FIXED_LEN..)?,
    }))
}

/// An RMRR: twenty-four fixed bytes, then device scopes.
fn decode_rmrr(entry: &[u8]) -> Option<Structure<'_>> {
    Some(Structure::Rmrr(Rmrr {
        segment: u16_at(entry, 6)?,
        base: u64_at(entry, 8)?,
        limit: u64_at(entry, 16)?,
        scopes: entry.get(RMRR_FIXED_LEN..)?,
    }))
}

/// A DMA remapping hardware unit: one IOMMU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Drhd<'a> {
    /// Unit flags.
    pub flags: u8,
    /// The PCI segment group the unit serves.
    pub segment: u16,
    /// Physical address of the unit's register block.
    pub register_base: u64,
    /// The device scopes, undecoded.
    scopes: &'a [u8],
}

impl<'a> Drhd<'a> {
    /// The devices behind this unit.
    #[must_use]
    pub const fn device_scopes(&self) -> DeviceScopes<'a> {
        DeviceScopes { rest: self.scopes }
    }

    /// Whether the unit is its segment's catch-all
    /// ([`DRHD_INCLUDE_PCI_ALL`]).
    #[must_use]
    pub const fn includes_all(&self) -> bool {
        self.flags & DRHD_INCLUDE_PCI_ALL != 0
    }

    /// The source ID the I/O APIC whose MADT identifier is `id` sends its
    /// interrupt requests to this unit with, if one of the unit's scopes
    /// names it: the [`SCOPE_IOAPIC`] scope whose enumeration ID is `id`,
    /// matched by that ID and never by the scopes' order.
    ///
    /// Only a one-hop path is followed, which names the I/O APIC's own bus,
    /// device and function: every platform this has been read against
    /// (QEMU's `q35` puts it on the pseudo-bus 0xFF, device 0, function 0)
    /// gives one. A longer path, through bridges, answers `None`, as no
    /// scope does.
    #[must_use]
    pub fn ioapic_source(&self, id: u8) -> Option<u16> {
        self.device_scopes()
            .find(|scope| scope.kind == SCOPE_IOAPIC && scope.enumeration_id == id)
            .and_then(|scope| scope.endpoint())
            .map(|(bus, device, function)| {
                u16::from(bus) << 8 | u16::from(device & 0x1F) << 3 | u16::from(function & 0x7)
            })
    }
}

/// A reserved memory region some device must keep reaching.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rmrr<'a> {
    /// The PCI segment group of the devices named.
    pub segment: u16,
    /// Physical address of the first byte.
    pub base: u64,
    /// Physical address of the last byte, inclusive.
    pub limit: u64,
    /// The device scopes, undecoded.
    scopes: &'a [u8],
}

impl<'a> Rmrr<'a> {
    /// The devices that use the region.
    #[must_use]
    pub const fn device_scopes(&self) -> DeviceScopes<'a> {
        DeviceScopes { rest: self.scopes }
    }

    /// Bytes the region spans, or `None` for a limit below its base or a
    /// region covering the whole address space.
    #[must_use]
    pub const fn size(&self) -> Option<u64> {
        match self.limit.checked_sub(self.base) {
            Some(span) => span.checked_add(1),
            None => None,
        }
    }
}

/// One device a DRHD or RMRR names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceScope<'a> {
    /// [`SCOPE_PCI_ENDPOINT`] and its neighbours.
    pub kind: u8,
    /// For an I/O APIC or HPET, which one; otherwise zero.
    pub enumeration_id: u8,
    /// The bus the path starts on.
    pub start_bus: u8,
    /// The path's bytes, undecoded.
    path: &'a [u8],
}

impl<'a> DeviceScope<'a> {
    /// The path's `(device, function)` hops, in order. A trailing odd byte is
    /// not a hop.
    pub fn path(&self) -> impl Iterator<Item = (u8, u8)> + 'a {
        self.path.chunks_exact(2).filter_map(|hop| match *hop {
            [device, function] => Some((device, function)),
            _ => None,
        })
    }

    /// Whether the path is a whole number of hops, at least one. A path with
    /// a trailing odd byte is malformed: [`DeviceScope::path`] would drop
    /// the byte and name the bridge above what firmware meant.
    #[must_use]
    pub const fn path_is_whole(&self) -> bool {
        !self.path.is_empty() && self.path.len().is_multiple_of(2)
    }

    /// `(bus, device, function)` when the path is a single hop, which names a
    /// function on the start bus. `None` for a longer path, whose later hops
    /// are on buses only configuration space can name.
    #[must_use]
    pub fn endpoint(&self) -> Option<(u8, u8, u8)> {
        match *self.path {
            [device, function] => Some((self.start_bus, device, function)),
            _ => None,
        }
    }
}

/// Iterator over a structure's device scopes.
#[derive(Clone, Copy, Debug)]
pub struct DeviceScopes<'a> {
    /// What is left of the scopes after those already read.
    rest: &'a [u8],
}

impl<'a> Iterator for DeviceScopes<'a> {
    type Item = DeviceScope<'a>;

    fn next(&mut self) -> Option<DeviceScope<'a>> {
        let kind = u8_at(self.rest, 0)?;
        let span = usize::from(u8_at(self.rest, 1)?);
        let Some(entry) = self.rest.get(..span).filter(|_| span >= SCOPE_FIXED_LEN) else {
            self.rest = &[];
            return None;
        };
        self.rest = self.rest.get(span..).unwrap_or(&[]);
        Some(DeviceScope {
            kind,
            enumeration_id: u8_at(entry, 4)?,
            start_bus: u8_at(entry, 5)?,
            path: entry.get(SCOPE_FIXED_LEN..)?,
        })
    }
}
