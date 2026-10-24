//! The IVRS table: where AMD's IOMMUs (AMD-Vi) are, and which devices each
//! one translates for (`docs/AMD-VI.md` §1.1).
//!
//! Reference: AMD I/O Virtualization Technology (IOMMU) Specification,
//! #48882, "ACPI Tables"; checked against Linux's `drivers/iommu/amd/init.c`
//! and QEMU's `build_amd_iommu` (`hw/i386/acpi-build.c`), which the boot
//! row runs.
//!
//! The table is a 48-byte header -- the system description header, `IVinfo`
//! and eight reserved bytes -- then blocks, each a type, flags and a 16-bit
//! length. An **IVHD** (10h, 11h, 40h) is one unit: its register block, its
//! own PCI function, and device entries naming the requester IDs (devids)
//! behind it. An **IVMD** (20h, 21h, 22h) is memory some device keeps
//! reaching: AMD's RMRR.
//!
//! # One IVHD type per unit
//!
//! Firmware describes each unit once per IVHD type it supports: QEMU writes
//! a 10h and an 11h for its one unit, nazuna's firmware the same. Only the
//! blocks of one type are read, the highest of 10h, 11h and 40h among those
//! naming the first unit's devid (Linux's `get_highest_supported_ivhd_type`),
//! so no unit is counted twice and no device entry is read twice.
//!
//! # What cannot be read is said, not guessed
//!
//! A block whose length runs past the table, an IVHD too short for its
//! header, a device entry of a type with no known length, an end of range
//! with no start, a range ending below its start, and a unit an IVHD of
//! another type names that the chosen type does not, each make the table
//! [`Ivrs::whole`] answer false. The kernel then places nothing behind any
//! unit: every function it might have named is unresolved, never bypassing.

use crate::{AcpiError, Table, u8_at, u16_at, u32_at, u64_at};

#[cfg(test)]
mod tests;

/// The IVRS's signature.
pub const IVRS_SIGNATURE: [u8; 4] = *b"IVRS";

/// Bytes before the first block: the header, `IVinfo` and eight reserved.
pub const IVRS_HEADER_LEN: usize = 48;

/// An IVHD with the 24-byte header and the IOMMU feature reporting field.
pub const IVHD_10: u8 = 0x10;
/// An IVHD with the 40-byte header: attributes and the EFR image.
pub const IVHD_11: u8 = 0x11;
/// As [`IVHD_11`], and it may hold ACPI-HID entries.
pub const IVHD_40: u8 = 0x40;
/// An IVMD for every peripheral.
pub const IVMD_ALL: u8 = 0x20;
/// An IVMD for one devid.
pub const IVMD_ONE: u8 = 0x21;
/// An IVMD for a devid range.
pub const IVMD_RANGE: u8 = 0x22;

/// IVMD flags: a unity map.
pub const IVMD_UNITY: u8 = 1 << 0;
/// IVMD flags: reads are allowed.
pub const IVMD_READ: u8 = 1 << 1;
/// IVMD flags: writes are allowed.
pub const IVMD_WRITE: u8 = 1 << 2;
/// IVMD flags: an exclusion range.
pub const IVMD_EXCLUSION: u8 = 1 << 3;

/// IVHD 11h/40h attributes: the unit does no host address translation.
pub const ATTRIBUTE_HAT_DISABLED: u32 = 1 << 0;

/// A special device entry's variety: an I/O APIC.
pub const SPECIAL_IOAPIC: u8 = 1;
/// A special device entry's variety: an HPET.
pub const SPECIAL_HPET: u8 = 2;

/// Bytes of a block's type, flags and length.
const BLOCK_HEADER_LEN: usize = 4;
/// Bytes of an IVHD 10h's header, before its device entries.
const IVHD_10_LEN: usize = 24;
/// Bytes of an IVHD 11h's or 40h's header.
const IVHD_11_LEN: usize = 40;
/// Bytes of an IVMD.
const IVMD_LEN: usize = 32;

/// Device entry: padding.
const ENTRY_PAD: u8 = 0x00;
/// Device entry: every devid.
const ENTRY_ALL: u8 = 0x01;
/// Device entry: one devid.
const ENTRY_SELECT: u8 = 0x02;
/// Device entry: the start of a range.
const ENTRY_RANGE_START: u8 = 0x03;
/// Device entry: the end of the range a start opened.
const ENTRY_RANGE_END: u8 = 0x04;
/// Device entry: one devid, arriving as another.
const ENTRY_ALIAS_SELECT: u8 = 0x42;
/// Device entry: the start of a range arriving as one devid.
const ENTRY_ALIAS_RANGE: u8 = 0x43;
/// Device entry: one devid, with an extended byte.
const ENTRY_EXTENDED_SELECT: u8 = 0x46;
/// Device entry: the start of a range, with an extended byte.
const ENTRY_EXTENDED_RANGE: u8 = 0x47;
/// Device entry: an I/O APIC or HPET, by its handle.
const ENTRY_SPECIAL: u8 = 0x48;
/// Device entry: an ACPI namespace device, of variable length.
const ENTRY_ACPI_HID: u8 = 0xF0;
/// Bytes of an ACPI-HID entry before its UID.
const ACPI_HID_FIXED_LEN: usize = 22;

/// The IVRS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ivrs<'a> {
    /// The table everything is read out of.
    table: Table<'a>,
}

impl<'a> Ivrs<'a> {
    /// Interpret `table` as an IVRS.
    ///
    /// # Errors
    ///
    /// [`AcpiError::BadSignature`] if it is not one, and
    /// [`AcpiError::TooShort`] if it ends inside the fixed header.
    pub fn parse(table: Table<'a>) -> Result<Self, AcpiError> {
        table.expect_signature(IVRS_SIGNATURE)?;
        if table.bytes().len() < IVRS_HEADER_LEN {
            return Err(AcpiError::TooShort {
                got: table.bytes().len(),
                need: IVRS_HEADER_LEN,
            });
        }
        Ok(Ivrs { table })
    }

    /// `IVinfo`: the virtualization information common to every unit.
    #[must_use]
    pub fn info(&self) -> u32 {
        u32_at(self.table.bytes(), 36).unwrap_or(0)
    }

    /// Every block, in table order, read by its length.
    #[must_use]
    pub fn blocks(&self) -> Blocks<'a> {
        Blocks {
            rest: self.table.bytes().get(IVRS_HEADER_LEN..).unwrap_or(&[]),
            broken: false,
        }
    }

    /// The IVHD type read: the highest of 10h, 11h and 40h among the IVHDs
    /// naming the first IVHD's devid; `None` with no IVHD.
    #[must_use]
    pub fn chosen_type(&self) -> Option<u8> {
        let first = self.ivhds().next()?;
        self.ivhds()
            .filter(|unit| unit.devid == first.devid && unit.segment == first.segment)
            .map(|unit| unit.kind)
            .max()
    }

    /// Every IVHD of any supported type.
    fn ivhds(&self) -> impl Iterator<Item = Ivhd<'a>> + 'a {
        self.blocks().filter_map(|block| match block {
            Block::Ivhd(unit) => Some(unit),
            Block::Ivmd(_) | Block::Other { .. } | Block::Broken => None,
        })
    }

    /// The units: the IVHDs of [`Ivrs::chosen_type`], in table order.
    pub fn units(&self) -> impl Iterator<Item = Ivhd<'a>> + 'a {
        let chosen = self.chosen_type();
        self.ivhds().filter(move |unit| Some(unit.kind) == chosen)
    }

    /// Every IVMD, in table order.
    pub fn memory(&self) -> impl Iterator<Item = Ivmd> + 'a {
        self.blocks().filter_map(|block| match block {
            Block::Ivmd(region) => Some(region),
            Block::Ivhd(_) | Block::Other { .. } | Block::Broken => None,
        })
    }

    /// Whether the whole table can be read: no block runs past it or is too
    /// short for its header, every unit's device entries read to their end,
    /// and every unit an IVHD of another type names is also named at the
    /// chosen type. While it cannot, nothing may be placed behind any unit.
    #[must_use]
    pub fn whole(&self) -> bool {
        let mut blocks = self.blocks();
        if blocks.by_ref().any(|block| block == Block::Broken) || blocks.broken {
            return false;
        }
        if self.units().any(|unit| !unit.entries_whole()) {
            return false;
        }
        self.ivhds().all(|named| {
            self.units()
                .any(|unit| unit.devid == named.devid && unit.segment == named.segment)
        })
    }

    /// The underlying table, for its header and checksum.
    #[must_use]
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }
}

/// One IVRS block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Block<'a> {
    /// A unit.
    Ivhd(Ivhd<'a>),
    /// A memory definition.
    Ivmd(Ivmd),
    /// A block of a type this crate does not read, stepped over by its
    /// length.
    Other {
        /// Its type.
        kind: u8,
    },
    /// A block too short for its header, or running past the table: the
    /// walk ends here.
    Broken,
}

/// Iterator over the blocks.
#[derive(Clone, Copy, Debug)]
pub struct Blocks<'a> {
    /// What is left of the table.
    rest: &'a [u8],
    /// Whether the walk ended on a broken block.
    broken: bool,
}

impl<'a> Iterator for Blocks<'a> {
    type Item = Block<'a>;

    fn next(&mut self) -> Option<Block<'a>> {
        if self.rest.is_empty() || self.broken {
            return None;
        }
        let decoded = (|| {
            let kind = u8_at(self.rest, 0)?;
            let span = usize::from(u16_at(self.rest, 2)?);
            let block = self.rest.get(..span).filter(|_| span >= BLOCK_HEADER_LEN)?;
            let decoded = match kind {
                IVHD_10 | IVHD_11 | IVHD_40 => Block::Ivhd(decode_ivhd(kind, block)?),
                IVMD_ALL | IVMD_ONE | IVMD_RANGE => Block::Ivmd(decode_ivmd(kind, block)?),
                _ => Block::Other { kind },
            };
            Some((decoded, span))
        })();
        let Some((block, span)) = decoded else {
            self.broken = true;
            self.rest = &[];
            return Some(Block::Broken);
        };
        self.rest = self.rest.get(span..).unwrap_or(&[]);
        Some(block)
    }
}

/// An IVHD of `kind`, read out of `block`.
fn decode_ivhd(kind: u8, block: &[u8]) -> Option<Ivhd<'_>> {
    let header = if kind == IVHD_10 {
        IVHD_10_LEN
    } else {
        IVHD_11_LEN
    };
    let (efr, attributes, feature_reporting) = if kind == IVHD_10 {
        (None, 0, u32_at(block, 20)?)
    } else {
        (Some(u64_at(block, 24)?), u32_at(block, 20)?, 0)
    };
    Some(Ivhd {
        kind,
        flags: u8_at(block, 1)?,
        devid: u16_at(block, 4)?,
        cap_offset: u16_at(block, 6)?,
        base: u64_at(block, 8)?,
        segment: u16_at(block, 16)?,
        info: u16_at(block, 18)?,
        feature_reporting,
        attributes,
        efr,
        entries: block.get(header..)?,
    })
}

/// An IVMD of `kind`, read out of `block`.
fn decode_ivmd(kind: u8, block: &[u8]) -> Option<Ivmd> {
    if block.len() < IVMD_LEN {
        return None;
    }
    Some(Ivmd {
        kind,
        flags: u8_at(block, 1)?,
        devid: u16_at(block, 4)?,
        aux: u16_at(block, 6)?,
        start: u64_at(block, 16)?,
        length: u64_at(block, 24)?,
    })
}

/// One AMD-Vi unit, as its IVHD describes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ivhd<'a> {
    /// 10h, 11h or 40h.
    pub kind: u8,
    /// The IVHD flags (`HtTunEn`, `PassPW`, `ResPassPW`, `Isoc`, ...).
    pub flags: u8,
    /// The unit's own PCI function.
    pub devid: u16,
    /// Its capability block's offset in that function's configuration space.
    pub cap_offset: u16,
    /// Physical address of its registers.
    pub base: u64,
    /// The PCI segment it serves.
    pub segment: u16,
    /// The IOMMU info field.
    pub info: u16,
    /// 10h only: the IOMMU feature reporting field; 0 otherwise.
    pub feature_reporting: u32,
    /// 11h and 40h: the attributes ([`ATTRIBUTE_HAT_DISABLED`]); 0 for 10h.
    pub attributes: u32,
    /// 11h and 40h: the EFR image; `None` for 10h, whose EFR is read from
    /// the unit.
    pub efr: Option<u64>,
    /// The device entries, undecoded.
    entries: &'a [u8],
}

/// What a unit's device entries say of one devid ([`Ivhd::names`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Named {
    /// A select, range or extended entry names it: its DMA arrives as itself.
    Own,
    /// An alias entry names it: its DMA arrives as `source`.
    Aliased {
        /// The devid it arrives as.
        source: u16,
    },
    /// No entry names it.
    No,
}

/// One device entry, ranges joined ([`Ivhd::entries`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    /// Every devid on the segment.
    All,
    /// Devids `first..=last`, each arriving as itself.
    Devices {
        /// The first devid.
        first: u16,
        /// The last devid.
        last: u16,
    },
    /// Devids `first..=last`, each arriving as `source`.
    Alias {
        /// The first devid.
        first: u16,
        /// The last devid.
        last: u16,
        /// The devid they arrive as.
        source: u16,
    },
    /// An I/O APIC ([`SPECIAL_IOAPIC`]) or HPET ([`SPECIAL_HPET`]) by its
    /// handle -- the I/O APIC's MADT identifier -- and the devid its
    /// interrupt requests arrive with.
    Special {
        /// The variety.
        variety: u8,
        /// The handle.
        handle: u8,
        /// The devid.
        devid: u16,
    },
    /// An ACPI namespace device: no PCI function, skipped.
    AcpiHid {
        /// The devid it is given.
        devid: u16,
    },
    /// What cannot be read: the walk ends, and [`Ivhd::entries_whole`]
    /// answers false.
    Unreadable,
}

impl<'a> Ivhd<'a> {
    /// The device entries, in order, a range's start and end joined into
    /// one entry. Padding is skipped. The walk ends at the first entry
    /// that cannot be read, after yielding [`Entry::Unreadable`].
    #[must_use]
    pub const fn entries(&self) -> Entries<'a> {
        Entries {
            rest: self.entries,
            ended: false,
        }
    }

    /// Whether every device entry can be read to the block's end.
    #[must_use]
    pub fn entries_whole(&self) -> bool {
        !self.entries().any(|entry| entry == Entry::Unreadable)
    }

    /// What the explicit entries -- select, range, extended and alias --
    /// say of `devid`: **the last that names it decides**, as Linux's
    /// in-order walk of the entries leaves its device and alias tables.
    /// Entries overlap in practice: nazuna's names 0x0003-0xFFFE as their
    /// own and then 0xFF00-0xFFFF as arriving as 00:14.5, which the later
    /// entry wins. [`Entry::All`] is not explicit: [`Ivhd::includes_all`]
    /// answers for it.
    #[must_use]
    pub fn names(&self, devid: u16) -> Named {
        names_among(self.entries(), devid)
    }

    /// Whether the unit has an ALL entry: every devid on its segment no
    /// other entry names is behind it.
    #[must_use]
    pub fn includes_all(&self) -> bool {
        self.entries().any(|entry| entry == Entry::All)
    }

    /// The devid the special entry of `variety` with `handle` gives, if the
    /// unit has one.
    #[must_use]
    pub fn special(&self, variety: u8, handle: u8) -> Option<u16> {
        self.entries().find_map(|entry| match entry {
            Entry::Special {
                variety: seen,
                handle: of,
                devid,
            } if seen == variety && of == handle => Some(devid),
            _ => None,
        })
    }

    /// Whether the unit does no host address translation
    /// ([`ATTRIBUTE_HAT_DISABLED`], 11h and 40h only).
    #[must_use]
    pub const fn translation_disabled(&self) -> bool {
        self.attributes & ATTRIBUTE_HAT_DISABLED != 0
    }
}

/// What `entries` say of `devid`, as [`Ivhd::names`] reads a unit's: the
/// last entry naming it decides. For a caller that keeps a unit's entries
/// past the table, as the kernel does after bring-up.
pub fn names_among(entries: impl IntoIterator<Item = Entry>, devid: u16) -> Named {
    let mut named = Named::No;
    for entry in entries {
        match entry {
            Entry::Devices { first, last } if (first..=last).contains(&devid) => {
                named = Named::Own;
            }
            Entry::Alias {
                first,
                last,
                source,
            } if (first..=last).contains(&devid) => named = Named::Aliased { source },
            _ => {}
        }
    }
    named
}

/// What `docs/AMD-VI.md` decision D10 (ledger 3923) makes of a PCI
/// function: the unit's own, or the root complex.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exempt {
    /// The unit at this position among `units`: its address is the unit's
    /// IVHD devid and its class 08h/06h, an IOMMU.
    Unit(usize),
    /// The root complex: a host bridge (class 06h/00h) on bus 0 that no
    /// entry names, with bus mastering off as enumeration found it.
    RootComplex,
}

/// Whether the function at `devid` on `segment`, of class code `class`
/// (base class in 23:16) and bus mastering `bus_master`, is one of D10's
/// two, among `units`: each unit's segment, its own devid and its device
/// entries. Anything else -- a host bridge some entry names, one off bus
/// 0, one with bus mastering on, the unit's devid with another class -- is
/// `None`, and placement decides it as any function.
pub fn exempt<'e>(
    units: impl IntoIterator<Item = (u16, u16, &'e [Entry])> + Clone,
    segment: u16,
    devid: u16,
    class: u32,
    bus_master: bool,
) -> Option<Exempt> {
    let base_sub = class >> 8;
    if base_sub == 0x0806
        && let Some(at) = units
            .clone()
            .into_iter()
            .position(|(of, own, _)| of == segment && own == devid)
    {
        return Some(Exempt::Unit(at));
    }
    let named = units.into_iter().any(|(of, _, entries)| {
        of == segment
            && (names_among(entries.iter().copied(), devid) != Named::No
                || entries.contains(&Entry::All))
    });
    let bus = devid >> 8;
    (bus == 0 && base_sub == 0x0600 && !named && !bus_master).then_some(Exempt::RootComplex)
}

/// Iterator over a unit's device entries.
#[derive(Clone, Copy, Debug)]
pub struct Entries<'a> {
    /// What is left of the entries.
    rest: &'a [u8],
    /// Whether an unreadable entry ended the walk.
    ended: bool,
}

impl Entries<'_> {
    /// Step over one entry, answering its type and bytes.
    fn take(&mut self) -> Option<(u8, &[u8])> {
        let kind = u8_at(self.rest, 0)?;
        let span = match kind {
            ENTRY_ACPI_HID => ACPI_HID_FIXED_LEN + usize::from(u8_at(self.rest, 21)?),
            0x00..=0x7F => 4 << (kind >> 6),
            _ => return None,
        };
        let entry = self.rest.get(..span)?;
        self.rest = self.rest.get(span..).unwrap_or(&[]);
        Some((kind, entry))
    }

    /// End the walk on an unreadable entry.
    fn unreadable(&mut self) -> Option<Entry> {
        self.ended = true;
        self.rest = &[];
        Some(Entry::Unreadable)
    }
}

impl Iterator for Entries<'_> {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        loop {
            if self.ended || self.rest.is_empty() {
                return None;
            }
            let Some((kind, entry)) = self.take() else {
                return self.unreadable();
            };
            let devid = u16_at(entry, 1).unwrap_or(0);
            return match kind {
                ENTRY_PAD => continue,
                ENTRY_ALL => Some(Entry::All),
                ENTRY_SELECT | ENTRY_EXTENDED_SELECT => Some(Entry::Devices {
                    first: devid,
                    last: devid,
                }),
                ENTRY_ALIAS_SELECT => match u16_at(entry, 5) {
                    Some(source) => Some(Entry::Alias {
                        first: devid,
                        last: devid,
                        source,
                    }),
                    None => self.unreadable(),
                },
                ENTRY_RANGE_START | ENTRY_EXTENDED_RANGE | ENTRY_ALIAS_RANGE => {
                    let source = u16_at(entry, 5);
                    let end = match self.take() {
                        Some((ENTRY_RANGE_END, end)) => u16_at(end, 1),
                        _ => None,
                    };
                    match end {
                        Some(last) if last >= devid && kind == ENTRY_ALIAS_RANGE => match source {
                            Some(source) => Some(Entry::Alias {
                                first: devid,
                                last,
                                source,
                            }),
                            None => self.unreadable(),
                        },
                        Some(last) if last >= devid => Some(Entry::Devices { first: devid, last }),
                        _ => self.unreadable(),
                    }
                }
                ENTRY_SPECIAL => match (u8_at(entry, 4), u16_at(entry, 5), u8_at(entry, 7)) {
                    (Some(handle), Some(devid), Some(variety)) => Some(Entry::Special {
                        variety,
                        handle,
                        devid,
                    }),
                    _ => self.unreadable(),
                },
                ENTRY_ACPI_HID => Some(Entry::AcpiHid { devid }),
                // A lone end of range, and every reserved fixed-size type:
                // one firmware meant something by, which this cannot say.
                _ => self.unreadable(),
            };
        }
    }
}

/// A memory definition: a unity map or an exclusion range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ivmd {
    /// 20h, 21h or 22h.
    pub kind: u8,
    /// [`IVMD_UNITY`] and its neighbours.
    pub flags: u8,
    /// 21h: the devid; 22h: the first devid.
    pub devid: u16,
    /// 22h: the last devid.
    pub aux: u16,
    /// Physical address of the first byte.
    pub start: u64,
    /// Bytes.
    pub length: u64,
}

impl Ivmd {
    /// Whether it names `devid`: 20h every devid, 21h its own, 22h its range.
    #[must_use]
    pub fn names(&self, devid: u16) -> bool {
        match self.kind {
            IVMD_ALL => true,
            IVMD_ONE => self.devid == devid,
            _ => (self.devid..=self.aux).contains(&devid),
        }
    }

    /// Whether it asks for memory to stay reachable: a unity map or an
    /// exclusion range, which `docs/AMD-VI.md` §4.1 treats alike.
    #[must_use]
    pub const fn kept(&self) -> bool {
        self.flags & (IVMD_UNITY | IVMD_EXCLUSION) != 0
    }

    /// Its last byte, or `None` for an empty region or one past the address
    /// space.
    #[must_use]
    pub const fn limit(&self) -> Option<u64> {
        match self.length.checked_sub(1) {
            Some(span) => self.start.checked_add(span),
            None => None,
        }
    }
}
