//! ACPI table parser.
//!
//! Enough of ACPI to bring up the interrupt controllers and the timers on both
//! architectures this kernel targets: the RSDP, the XSDT/RSDT, the MADT, the
//! FADT's fixed fields and the GTDT. There is no AML interpreter here and there
//! will not be one; everything below is a pure function of bytes.
//!
//! # Why there are no pointers in this crate
//!
//! ACPI tables are not one buffer. The RSDP holds the physical address of the
//! XSDT, and the XSDT is an array of the physical addresses of everything else,
//! so a parser that borrowed a slice could only ever see one table. The usual
//! answer is to hand the parser raw pointers and let it dereference physical
//! addresses that firmware chose, which is exactly the code you do not want to
//! be writing in ring 0.
//!
//! Instead the caller implements [`Tables`], which maps a physical address to
//! bytes. The kernel implements it over its direct map; the test suite
//! implements it over a `BTreeMap`. That keeps this crate
//! `#![forbid(unsafe_code)]` and fully host-testable, and it moves the one
//! genuinely unsafe step — deciding that a physical address is readable — to
//! the component that owns the memory map and can answer it.
//!
//! # Totality
//!
//! This parses bytes firmware wrote, with the MMU on and nothing above the
//! kernel to contain a mistake. Every field is read with a bounds-checked
//! `get` and a `from_le_bytes`, every length is checked against what was
//! actually supplied, and every arithmetic step that could wrap is checked. No
//! input reaches a panic.
//!
//! Checksums are the one deliberate exception to "reject what is malformed":
//! shipping firmware does get them wrong, and a machine that refuses to boot
//! over a byte in an OEM table is worse than one that logs it. So a bad
//! checksum is *reported* — [`Table::checksum_valid`], [`Rsdp::checksum_valid`]
//! — and the caller decides.
//!
//! ```
//! # use ferrix_acpi::{Acpi, AcpiError, MADT_SIGNATURE, Madt, Rsdp, Tables};
//! # fn example<T: Tables>(memory: &T, rsdp_bytes: &[u8]) -> Result<(), AcpiError> {
//! let rsdp = Rsdp::parse(rsdp_bytes)?;
//! let acpi = Acpi::from_rsdp(memory, &rsdp)?;
//! let madt = Madt::parse(acpi.find(MADT_SIGNATURE)?)?;
//! for entry in madt.entries() {
//!     let _ = entry;
//! }
//! # Ok(())
//! # }
//! ```

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

// ---------------------------------------------------------------------------
// Signatures and sizes
// ---------------------------------------------------------------------------

/// The eight bytes an RSDP starts with. The trailing space is part of it.
pub const RSDP_SIGNATURE: [u8; 8] = *b"RSD PTR ";

/// Bytes of the RSDP that revision 0 defined, and the span its checksum covers.
///
/// Revision 2 kept those twenty bytes byte-for-byte and appended to them, so
/// the first checksum still has to come out right on a modern RSDP.
pub const RSDP_V1_LEN: usize = 20;

/// Bytes of a revision-2 RSDP, which is what `length` is on every machine that
/// has an XSDT.
pub const RSDP_V2_LEN: usize = 36;

/// Bytes of the header that begins every system description table.
pub const SDT_HEADER_LEN: usize = 36;

/// Bytes of the MADT's own fixed part, before the first controller entry.
pub const MADT_HEADER_LEN: usize = 44;

/// Bytes of a generic address structure (ACPI 6.5 section 5.2.3.2).
pub const GENERIC_ADDRESS_LEN: usize = 12;

/// Signature of the Extended System Description Table.
pub const XSDT_SIGNATURE: [u8; 4] = *b"XSDT";
/// Signature of the Root System Description Table.
pub const RSDT_SIGNATURE: [u8; 4] = *b"RSDT";
/// Signature of the Multiple APIC Description Table.
pub const MADT_SIGNATURE: [u8; 4] = *b"APIC";
/// Signature of the Fixed ACPI Description Table.
pub const FADT_SIGNATURE: [u8; 4] = *b"FACP";
/// Signature of the Generic Timer Description Table.
pub const GTDT_SIGNATURE: [u8; 4] = *b"GTDT";
/// Signature of the High Precision Event Timer table.
pub const HPET_SIGNATURE: [u8; 4] = *b"HPET";
/// Signature of the PCI Express memory-mapped configuration table.
pub const MCFG_SIGNATURE: [u8; 4] = *b"MCFG";

/// MADT flags bit 0: the machine has a pair of 8259 PICs that must be masked
/// before the I/O APIC is used.
pub const MADT_FLAG_PCAT_COMPAT: u32 = 1 << 0;

/// Local APIC flags bit 0: this processor is usable now.
pub const LOCAL_APIC_ENABLED: u32 = 1 << 0;
/// Local APIC flags bit 1: this processor is not usable now but may be hot
/// plugged later. Booting it is a firmware-assisted operation, so a kernel that
/// does not implement hotplug must ignore it rather than treat it as a CPU.
pub const LOCAL_APIC_ONLINE_CAPABLE: u32 = 1 << 1;

/// GICC flags bit 0: this CPU interface is usable.
pub const GICC_FLAG_ENABLED: u32 = 1 << 0;

/// GTDT timer flags bit 0: the interrupt is edge triggered rather than level.
pub const GTDT_FLAG_EDGE_TRIGGERED: u32 = 1 << 0;
/// GTDT timer flags bit 1: the interrupt is active low rather than active high.
pub const GTDT_FLAG_ACTIVE_LOW: u32 = 1 << 1;
/// GTDT timer flags bit 2: the timer keeps running in low-power states.
pub const GTDT_FLAG_ALWAYS_ON: u32 = 1 << 2;

/// A GTDT counter base address of all ones means the block is not implemented,
/// which is what QEMU's `virt` machine reports (ACPI 6.5 section 5.2.24).
pub const GTDT_NO_COUNTER_BASE: u64 = u64::MAX;

/// Upper bound on the interrupt-controller entries walked in one MADT.
///
/// The walk already cannot loop — every entry advances the cursor by at least
/// two bytes — but a table whose `length` firmware got wrong can still describe
/// tens of thousands of entries, and a boot-time loop that long is
/// indistinguishable from a hang. No real machine is near this: 2048 entries is
/// past the largest published x2APIC topology.
const MAX_MADT_ENTRIES: usize = 8192;

/// Bytes of a MADT entry's own header: `type` then `length`.
const MADT_ENTRY_HEADER_LEN: usize = 2;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a table was rejected.
///
/// Bad checksums are deliberately not in here; see the crate documentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AcpiError {
    /// Fewer bytes were supplied than the fixed part of this structure needs.
    TooShort {
        /// Bytes that were supplied.
        got: usize,
        /// Bytes the structure needs at minimum.
        need: usize,
    },
    /// The eight bytes at the start of the RSDP are not `RSD PTR `.
    BadRsdpSignature,
    /// A table's signature is not the one asked for. Carries what was found.
    BadSignature([u8; 4]),
    /// A table's `length` field is smaller than the header it sits in, so the
    /// table claims not to contain itself.
    LengthTooSmall {
        /// The `length` field as firmware wrote it.
        declared: u32,
        /// Bytes the header alone occupies.
        need: usize,
    },
    /// A table's `length` field runs past the bytes [`Tables`] supplied.
    LengthTooLarge {
        /// The `length` field as firmware wrote it.
        declared: u32,
        /// Bytes actually available at that address.
        available: usize,
    },
    /// [`Tables`] could not supply the bytes at this physical address, so the
    /// table is either outside the memory map or outside the direct map.
    Unreadable(u64),
    /// No table with this signature is listed in the XSDT or RSDT.
    NotFound([u8; 4]),
    /// Neither an XSDT nor an RSDT address is present in the RSDP.
    NoRootTable,
    /// A MADT entry declared a `length` of zero or one. Two is the minimum,
    /// because the type and length bytes are themselves part of the entry; a
    /// parser that trusts a zero here never advances.
    ZeroLengthEntry {
        /// Byte offset of the entry within the table.
        offset: usize,
    },
    /// A MADT entry's `length` runs past the end of the table, which is the
    /// truncation a bad `length` field in the SDT header produces.
    EntryOutOfBounds {
        /// Byte offset of the entry within the table.
        offset: usize,
    },
    /// The MADT describes more entries than [`MAX_MADT_ENTRIES`], so the walk
    /// was cut short rather than run for an unbounded time at boot.
    TooManyEntries,
}

impl fmt::Display for AcpiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AcpiError::TooShort { got, need } => {
                write!(f, "{got} bytes supplied where {need} are needed")
            }
            AcpiError::BadRsdpSignature => f.write_str("not an RSDP"),
            AcpiError::BadSignature(found) => {
                f.write_str("unexpected table signature ")?;
                write_signature(f, found)
            }
            AcpiError::LengthTooSmall { declared, need } => {
                write!(f, "table length {declared} is below the {need}-byte header")
            }
            AcpiError::LengthTooLarge {
                declared,
                available,
            } => write!(
                f,
                "table length {declared} exceeds {available} bytes available"
            ),
            AcpiError::Unreadable(address) => write!(f, "no readable table at {address:#x}"),
            AcpiError::NotFound(signature) => {
                f.write_str("no table with signature ")?;
                write_signature(f, signature)
            }
            AcpiError::NoRootTable => f.write_str("RSDP names neither an XSDT nor an RSDT"),
            AcpiError::ZeroLengthEntry { offset } => {
                write!(f, "MADT entry at offset {offset} has length below 2")
            }
            AcpiError::EntryOutOfBounds { offset } => {
                write!(f, "MADT entry at offset {offset} runs past the table")
            }
            AcpiError::TooManyEntries => f.write_str("MADT describes too many entries"),
        }
    }
}

/// Print a four-byte signature as text, falling back to hex for the bytes that
/// are not printable ASCII: a corrupt signature is exactly the case where the
/// message matters, so it must not turn into replacement characters.
fn write_signature(f: &mut fmt::Formatter<'_>, signature: [u8; 4]) -> fmt::Result {
    for byte in signature {
        if byte.is_ascii_graphic() {
            fmt::Write::write_char(f, char::from(byte))?;
        } else {
            write!(f, "\\x{byte:02x}")?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Little-endian field access
//
// ACPI is little-endian on every architecture, including big-endian ones, so
// these are the only way a field is read in this crate.
// ---------------------------------------------------------------------------

/// Read the byte at `offset`, or `None` past the end.
fn u8_at(bytes: &[u8], offset: usize) -> Option<u8> {
    bytes.get(offset).copied()
}

/// Read a little-endian `u16` at `offset`, or `None` past the end.
fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let field: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(field))
}

/// Read a little-endian `u32` at `offset`, or `None` past the end.
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let field: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(field))
}

/// Read a little-endian `u64` at `offset`, or `None` past the end.
fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    let field: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(field))
}

/// Read a fixed-size byte field at `offset`, or `None` past the end.
fn array_at<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

/// The sum of `bytes` modulo 256. Every ACPI structure is defined so that this
/// comes out zero over its whole declared length.
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().copied().fold(0u8, u8::wrapping_add)
}

// ---------------------------------------------------------------------------
// Physical memory access
// ---------------------------------------------------------------------------

/// Access to physical memory, so this crate never dereferences a pointer.
pub trait Tables {
    /// The bytes of a table at `physical_address`, at least `len` of them,
    /// or `None` if that address is not readable.
    fn table(&self, physical_address: u64, len: usize) -> Option<&[u8]>;
}

/// Read the table at `physical_address`: its header first, to learn how long it
/// claims to be, then the whole of it.
///
/// Two reads rather than one because the length is inside the bytes being
/// bounded, which is the shape of the problem, not a shortcut.
pub fn read_table<T: Tables>(tables: &T, physical_address: u64) -> Result<Table<'_>, AcpiError> {
    let head = tables
        .table(physical_address, SDT_HEADER_LEN)
        .ok_or(AcpiError::Unreadable(physical_address))?;
    let header = SdtHeader::parse(head)?;
    let length = usize::try_from(header.length).map_err(|_| AcpiError::LengthTooLarge {
        declared: header.length,
        available: head.len(),
    })?;
    let bytes = tables
        .table(physical_address, length)
        .ok_or(AcpiError::Unreadable(physical_address))?;
    Table::parse(bytes)
}

// ---------------------------------------------------------------------------
// RSDP
// ---------------------------------------------------------------------------

/// The Root System Description Pointer, decoded.
///
/// This is the one ACPI structure that is not an SDT: it has no signature-plus-
/// length header and its own checksum rules. The loader finds it (UEFI hands it
/// over in the configuration table) and the kernel starts here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rsdp {
    /// 0 for ACPI 1.0, 2 for everything since. Revision 1 was never shipped.
    pub revision: u8,
    /// The OEM's six-character identifier, space padded and not terminated.
    pub oem_id: [u8; 6],
    /// Physical address of the RSDT, whose entries are 32 bits wide.
    pub rsdt_address: u32,
    /// Physical address of the XSDT, whose entries are 64 bits wide. Zero on a
    /// revision-0 RSDP, where the field does not exist.
    pub xsdt_address: u64,
    /// Bytes the whole RSDP occupies. Forced to [`RSDP_V1_LEN`] on revision 0,
    /// which has no length field.
    pub length: u32,
    /// Whether the first twenty bytes sum to zero, as ACPI 1.0 requires.
    pub checksum_valid: bool,
    /// Whether all `length` bytes sum to zero, as revision 2 additionally
    /// requires. `true` on revision 0, where there is no extended checksum to
    /// disagree with.
    pub extended_checksum_valid: bool,
}

impl Rsdp {
    /// Decode an RSDP from the bytes at its address.
    ///
    /// Fails only if the signature is wrong or the bytes are too few. A bad
    /// checksum is recorded in the returned value instead, because firmware
    /// does ship them and refusing to boot is the worse failure.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        let head = bytes.get(..RSDP_V1_LEN).ok_or(AcpiError::TooShort {
            got: bytes.len(),
            need: RSDP_V1_LEN,
        })?;
        if array_at::<8>(head, 0) != Some(RSDP_SIGNATURE) {
            return Err(AcpiError::BadRsdpSignature);
        }

        let revision = u8_at(head, 15).unwrap_or(0);
        let mut rsdp = Rsdp {
            revision,
            oem_id: array_at::<6>(head, 9).unwrap_or([0; 6]),
            rsdt_address: u32_at(head, 16).unwrap_or(0),
            xsdt_address: 0,
            length: RSDP_V1_LEN as u32,
            checksum_valid: checksum(head) == 0,
            extended_checksum_valid: true,
        };
        if revision < 2 {
            return Ok(rsdp);
        }

        // From revision 2 the structure carries its own length, and the second
        // checksum covers that whole length rather than a fixed 36 bytes --
        // there is no guarantee a future revision does not grow again.
        let extended = bytes.get(..RSDP_V2_LEN).ok_or(AcpiError::TooShort {
            got: bytes.len(),
            need: RSDP_V2_LEN,
        })?;
        rsdp.length = u32_at(extended, 20).unwrap_or(0);
        rsdp.xsdt_address = u64_at(extended, 24).unwrap_or(0);
        let declared = usize::try_from(rsdp.length).unwrap_or(usize::MAX);
        rsdp.extended_checksum_valid = match bytes.get(..declared) {
            Some(whole) if declared >= RSDP_V2_LEN => checksum(whole) == 0,
            // A length below the structure it describes, or past the bytes
            // supplied, cannot be checksummed; that is a failure, not an
            // absence.
            _ => false,
        };
        Ok(rsdp)
    }

    /// Read and decode the RSDP at `physical_address`.
    pub fn read<T: Tables>(tables: &T, physical_address: u64) -> Result<Self, AcpiError> {
        // Ask for the larger form first so a revision-2 RSDP is decoded whole,
        // and fall back to the ACPI 1.0 size for the machines that only have
        // twenty bytes there.
        let bytes = tables
            .table(physical_address, RSDP_V2_LEN)
            .or_else(|| tables.table(physical_address, RSDP_V1_LEN))
            .ok_or(AcpiError::Unreadable(physical_address))?;
        Rsdp::parse(bytes)
    }

    /// Whether every checksum this RSDP's revision defines came out right.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.checksum_valid && self.extended_checksum_valid
    }

    /// The root table to walk: the XSDT when there is one, the RSDT otherwise.
    ///
    /// The XSDT wins whenever it is present, per ACPI 6.5 section 5.2.5.1: on a
    /// machine with both, firmware is only obliged to keep the XSDT complete.
    pub fn root_table(&self) -> Result<(u64, RootKind), AcpiError> {
        if self.revision >= 2 && self.xsdt_address != 0 {
            return Ok((self.xsdt_address, RootKind::Xsdt));
        }
        if self.rsdt_address != 0 {
            return Ok((u64::from(self.rsdt_address), RootKind::Rsdt));
        }
        Err(AcpiError::NoRootTable)
    }
}

// ---------------------------------------------------------------------------
// System description tables
// ---------------------------------------------------------------------------

/// The 36-byte header every system description table starts with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SdtHeader {
    /// The four characters naming the table, such as `APIC` or `FACP`.
    pub signature: [u8; 4],
    /// Bytes the whole table occupies, header included.
    pub length: u32,
    /// Revision of this table's own layout.
    pub revision: u8,
    /// The byte that makes all `length` bytes sum to zero.
    pub checksum: u8,
    /// The OEM's six-character identifier.
    pub oem_id: [u8; 6],
    /// The OEM's eight-character identifier for this particular table.
    pub oem_table_id: [u8; 8],
    /// The OEM's revision of this table.
    pub oem_revision: u32,
    /// Identifier of the utility that generated the table.
    pub creator_id: u32,
    /// Revision of that utility.
    pub creator_revision: u32,
}

impl SdtHeader {
    /// Decode the header at the start of `bytes`.
    ///
    /// Does not look at the rest of the table, so it is what [`read_table`]
    /// uses to learn a length before it asks for the whole thing.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        let head = bytes.get(..SDT_HEADER_LEN).ok_or(AcpiError::TooShort {
            got: bytes.len(),
            need: SDT_HEADER_LEN,
        })?;
        Ok(SdtHeader {
            signature: array_at::<4>(head, 0).unwrap_or([0; 4]),
            length: u32_at(head, 4).unwrap_or(0),
            revision: u8_at(head, 8).unwrap_or(0),
            checksum: u8_at(head, 9).unwrap_or(0),
            oem_id: array_at::<6>(head, 10).unwrap_or([0; 6]),
            oem_table_id: array_at::<8>(head, 16).unwrap_or([0; 8]),
            oem_revision: u32_at(head, 24).unwrap_or(0),
            creator_id: u32_at(head, 28).unwrap_or(0),
            creator_revision: u32_at(head, 32).unwrap_or(0),
        })
    }
}

/// A borrowed system description table whose extent has been checked.
///
/// `bytes` is exactly the `length` the header declares, never more, so anything
/// parsed out of a `Table` is inside the table firmware described even if
/// [`Tables`] handed over a larger window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Table<'a> {
    header: SdtHeader,
    bytes: &'a [u8],
    checksum_valid: bool,
}

impl<'a> Table<'a> {
    /// Validate the header and extent of a table, and borrow it.
    ///
    /// `bytes` may be longer than the table; the excess is dropped. It may not
    /// be shorter, and the table may not claim to be smaller than its own
    /// header.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        let header = SdtHeader::parse(bytes)?;
        let length = usize::try_from(header.length).map_err(|_| AcpiError::LengthTooLarge {
            declared: header.length,
            available: bytes.len(),
        })?;
        if length < SDT_HEADER_LEN {
            return Err(AcpiError::LengthTooSmall {
                declared: header.length,
                need: SDT_HEADER_LEN,
            });
        }
        let body = bytes.get(..length).ok_or(AcpiError::LengthTooLarge {
            declared: header.length,
            available: bytes.len(),
        })?;
        Ok(Table {
            header,
            bytes: body,
            checksum_valid: checksum(body) == 0,
        })
    }

    /// The decoded header.
    #[must_use]
    pub const fn header(&self) -> &SdtHeader {
        &self.header
    }

    /// The four characters naming this table.
    #[must_use]
    pub const fn signature(&self) -> [u8; 4] {
        self.header.signature
    }

    /// The whole table, exactly `length` bytes.
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Everything after the 36-byte header: the table-specific part.
    #[must_use]
    pub fn body(&self) -> &'a [u8] {
        self.bytes.get(SDT_HEADER_LEN..).unwrap_or(&[])
    }

    /// Whether the table's bytes sum to zero, as ACPI requires.
    ///
    /// Reported rather than enforced: see the crate documentation.
    #[must_use]
    pub const fn checksum_valid(&self) -> bool {
        self.checksum_valid
    }

    /// Reject a table that is not the one expected.
    pub fn expect_signature(&self, signature: [u8; 4]) -> Result<(), AcpiError> {
        if self.header.signature == signature {
            Ok(())
        } else {
            Err(AcpiError::BadSignature(self.header.signature))
        }
    }
}

/// Which root table an RSDP pointed at, and so how wide its entries are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootKind {
    /// The ACPI 1.0 table, whose entries are 32-bit physical addresses.
    Rsdt,
    /// The table every machine since ACPI 2.0 has, whose entries are 64-bit
    /// physical addresses.
    Xsdt,
}

impl RootKind {
    /// Bytes of one entry in this kind of root table.
    #[must_use]
    pub const fn entry_width(self) -> usize {
        match self {
            RootKind::Rsdt => 4,
            RootKind::Xsdt => 8,
        }
    }

    /// The signature this kind of root table must carry.
    #[must_use]
    pub const fn signature(self) -> [u8; 4] {
        match self {
            RootKind::Rsdt => RSDT_SIGNATURE,
            RootKind::Xsdt => XSDT_SIGNATURE,
        }
    }
}

/// An XSDT or RSDT: a header followed by an array of physical addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RootTable<'a> {
    table: Table<'a>,
    kind: RootKind,
}

impl<'a> RootTable<'a> {
    /// Interpret `table` as a root table of `kind`.
    ///
    /// The signature is checked, because following 64-bit entries out of a
    /// table that is really an RSDT would produce addresses made of two
    /// unrelated halves.
    pub fn parse(table: Table<'a>, kind: RootKind) -> Result<Self, AcpiError> {
        table.expect_signature(kind.signature())?;
        Ok(RootTable { table, kind })
    }

    /// The underlying table, for its header and checksum.
    #[must_use]
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// Which kind of root table this is.
    #[must_use]
    pub const fn kind(&self) -> RootKind {
        self.kind
    }

    /// How many whole entries the table holds.
    ///
    /// A trailing partial entry -- a `length` that is not a whole number of
    /// addresses past the header -- is ignored rather than rounded up.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.body().len() / self.kind.entry_width()
    }

    /// Whether the root table lists no tables at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The physical address of the table at `index`.
    #[must_use]
    pub fn entry(&self, index: usize) -> Option<u64> {
        self.entries().nth(index)
    }

    /// The physical address of every table listed, in order.
    #[must_use]
    pub fn entries(&self) -> RootEntries<'a> {
        RootEntries {
            body: self.table.body(),
            width: self.kind.entry_width(),
            index: 0,
        }
    }
}

/// Iterator over the physical addresses a root table lists.
#[derive(Clone, Copy, Debug)]
pub struct RootEntries<'a> {
    body: &'a [u8],
    width: usize,
    index: usize,
}

impl Iterator for RootEntries<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        let start = self.index.checked_mul(self.width)?;
        let end = start.checked_add(self.width)?;
        let field = self.body.get(start..end)?;
        self.index = self.index.checked_add(1)?;
        if self.width == 8 {
            u64_at(field, 0)
        } else {
            u32_at(field, 0).map(u64::from)
        }
    }
}

// ---------------------------------------------------------------------------
// The entry point: a root table plus the memory it points into
// ---------------------------------------------------------------------------

/// A root table and the physical memory its entries point into.
///
/// Nothing is read until it is asked for, so constructing this cannot fail on a
/// machine whose XSDT is unreadable; the failure surfaces at [`Acpi::find`],
/// where the caller has a table name to report with it.
pub struct Acpi<'t, T: Tables> {
    tables: &'t T,
    root_address: u64,
    kind: RootKind,
}

impl<T: Tables> fmt::Debug for Acpi<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Acpi")
            .field("root_address", &self.root_address)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl<'t, T: Tables> Acpi<'t, T> {
    /// Walk the root table at `root_address`, whose entries are `kind` wide.
    #[must_use]
    pub const fn new(tables: &'t T, root_address: u64, kind: RootKind) -> Self {
        Acpi {
            tables,
            root_address,
            kind,
        }
    }

    /// Walk whichever root table this RSDP names.
    pub fn from_rsdp(tables: &'t T, rsdp: &Rsdp) -> Result<Self, AcpiError> {
        let (root_address, kind) = rsdp.root_table()?;
        Ok(Acpi::new(tables, root_address, kind))
    }

    /// Physical address of the root table.
    #[must_use]
    pub const fn root_address(&self) -> u64 {
        self.root_address
    }

    /// Read and validate the root table itself.
    pub fn root(&self) -> Result<RootTable<'t>, AcpiError> {
        let table = read_table(self.tables, self.root_address)?;
        RootTable::parse(table, self.kind)
    }

    /// The first table with this signature.
    ///
    /// Entries the root table lists but [`Tables`] cannot supply are skipped:
    /// one unmapped OEM table must not hide the MADT listed after it. An
    /// address that yields no table at all therefore shows up as
    /// [`AcpiError::NotFound`], not as [`AcpiError::Unreadable`].
    pub fn find(&self, signature: [u8; 4]) -> Result<Table<'t>, AcpiError> {
        let root = self.root()?;
        for address in root.entries() {
            if let Ok(table) = read_table(self.tables, address)
                && table.signature() == signature
            {
                return Ok(table);
            }
        }
        Err(AcpiError::NotFound(signature))
    }

    /// The MADT, decoded.
    pub fn madt(&self) -> Result<Madt<'t>, AcpiError> {
        Madt::parse(self.find(MADT_SIGNATURE)?)
    }

    /// The FADT, decoded.
    pub fn fadt(&self) -> Result<Fadt<'t>, AcpiError> {
        Fadt::parse(self.find(FADT_SIGNATURE)?)
    }

    /// The GTDT, decoded. Present on AArch64 machines and nowhere else.
    pub fn gtdt(&self) -> Result<Gtdt<'t>, AcpiError> {
        Gtdt::parse(self.find(GTDT_SIGNATURE)?)
    }

    /// The HPET description table, decoded. Present on PCs and nowhere else:
    /// AArch64 counts time with a register in the CPU.
    pub fn hpet(&self) -> Result<Hpet<'t>, AcpiError> {
        Hpet::parse(self.find(HPET_SIGNATURE)?)
    }
}

// ---------------------------------------------------------------------------
// MADT
// ---------------------------------------------------------------------------

/// How an interrupt's polarity is described in the MPS INTI flags
/// (ACPI 6.5 table 5.26).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Polarity {
    /// Whatever the bus this source sits on specifies.
    BusDefault,
    /// Asserted high.
    ActiveHigh,
    /// A bit pattern the specification reserves; treat as unknown.
    Reserved,
    /// Asserted low.
    ActiveLow,
}

/// How an interrupt is triggered, from the same flags word as [`Polarity`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TriggerMode {
    /// Whatever the bus this source sits on specifies.
    BusDefault,
    /// Edge triggered.
    Edge,
    /// A bit pattern the specification reserves; treat as unknown.
    Reserved,
    /// Level triggered.
    Level,
}

/// Decode the polarity from an MPS INTI flags word.
#[must_use]
pub const fn polarity_of(flags: u16) -> Polarity {
    match flags & 0b11 {
        0 => Polarity::BusDefault,
        1 => Polarity::ActiveHigh,
        2 => Polarity::Reserved,
        _ => Polarity::ActiveLow,
    }
}

/// Decode the trigger mode from an MPS INTI flags word.
#[must_use]
pub const fn trigger_mode_of(flags: u16) -> TriggerMode {
    match (flags >> 2) & 0b11 {
        0 => TriggerMode::BusDefault,
        1 => TriggerMode::Edge,
        2 => TriggerMode::Reserved,
        _ => TriggerMode::Level,
    }
}

/// A processor's local APIC (MADT entry type 0).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LocalApic {
    /// The identifier AML uses for this processor, matching a `Processor`
    /// object in the DSDT.
    pub processor_uid: u8,
    /// The APIC's identifier, which is what an interrupt is addressed to.
    pub apic_id: u8,
    /// [`LOCAL_APIC_ENABLED`] and [`LOCAL_APIC_ONLINE_CAPABLE`].
    pub flags: u32,
}

impl LocalApic {
    /// Whether this processor can be started now.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.flags & LOCAL_APIC_ENABLED != 0
    }

    /// Whether this processor is absent but could be hot plugged.
    #[must_use]
    pub const fn is_online_capable(&self) -> bool {
        self.flags & LOCAL_APIC_ONLINE_CAPABLE != 0
    }
}

/// An I/O APIC (MADT entry type 1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IoApic {
    /// This I/O APIC's identifier.
    pub id: u8,
    /// Physical address of its register window.
    pub address: u32,
    /// The global system interrupt its first input pin corresponds to.
    pub gsi_base: u32,
}

/// A legacy ISA interrupt that is wired to a different global system interrupt
/// than its number would suggest (MADT entry type 2).
///
/// This is why the timer works: on nearly every PC, ISA IRQ 0 arrives as GSI 2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InterruptSourceOverride {
    /// Always 0, meaning ISA.
    pub bus: u8,
    /// The ISA IRQ number being redescribed.
    pub source: u8,
    /// The global system interrupt it actually arrives on.
    pub gsi: u32,
    /// MPS INTI flags; see [`polarity_of`] and [`trigger_mode_of`].
    pub flags: u16,
}

impl InterruptSourceOverride {
    /// Polarity this source is delivered with.
    #[must_use]
    pub const fn polarity(&self) -> Polarity {
        polarity_of(self.flags)
    }

    /// Trigger mode this source is delivered with.
    #[must_use]
    pub const fn trigger_mode(&self) -> TriggerMode {
        trigger_mode_of(self.flags)
    }
}

/// A non-maskable interrupt wired to a local APIC input (MADT entry type 4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LocalApicNmi {
    /// Which processor, or `0xFF` for every processor.
    pub processor_uid: u8,
    /// MPS INTI flags; see [`polarity_of`] and [`trigger_mode_of`].
    pub flags: u16,
    /// Which of the local APIC's two `LINT` pins the NMI arrives on.
    pub lint: u8,
}

/// A processor's local x2APIC (MADT entry type 9), used above 255 processors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LocalX2Apic {
    /// The x2APIC's 32-bit identifier.
    pub x2apic_id: u32,
    /// [`LOCAL_APIC_ENABLED`] and [`LOCAL_APIC_ONLINE_CAPABLE`], as for
    /// [`LocalApic`].
    pub flags: u32,
    /// The identifier AML uses for this processor.
    pub processor_uid: u32,
}

impl LocalX2Apic {
    /// Whether this processor can be started now.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.flags & LOCAL_APIC_ENABLED != 0
    }

    /// Whether this processor is absent but could be hot plugged.
    #[must_use]
    pub const fn is_online_capable(&self) -> bool {
        self.flags & LOCAL_APIC_ONLINE_CAPABLE != 0
    }
}

/// A GIC CPU interface (MADT entry type 11): one AArch64 processor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gicc {
    /// The CPU interface number, meaningful on `GICv1` and GICv2 only.
    pub cpu_interface_number: u32,
    /// The identifier AML uses for this processor.
    pub acpi_processor_uid: u32,
    /// [`GICC_FLAG_ENABLED`] and the two interrupt-mode bits.
    pub flags: u32,
    /// The parking protocol version, for the pre-PSCI boot method.
    pub parking_protocol_version: u32,
    /// Interrupt number of this processor's performance monitor overflow
    /// signal.
    pub performance_interrupt_gsiv: u32,
    /// Physical address of this processor's parking protocol mailbox.
    pub parked_address: u64,
    /// Physical address of the GICv2 CPU interface registers. Zero on GICv3,
    /// where the interface is reached through system registers instead.
    pub physical_base_address: u64,
    /// Physical address of the virtual CPU interface control block.
    pub gicv: u64,
    /// Physical address of the hypervisor control block.
    pub gich: u64,
    /// Interrupt number of the virtual GIC maintenance signal.
    pub vgic_maintenance_interrupt: u32,
    /// Physical address of this processor's redistributor, when the GICR
    /// entries do not describe a discovery range covering it.
    pub gicr_base_address: u64,
    /// The processor's `MPIDR_EL1`, which is how the kernel addresses it to
    /// start it through PSCI.
    pub mpidr: u64,
    /// Scheduling hint: lower is more efficient. Zero on firmware predating
    /// ACPI 6.0, which is indistinguishable from a homogeneous machine.
    pub power_efficiency_class: u8,
}

impl Gicc {
    /// Whether this processor can be started now.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.flags & GICC_FLAG_ENABLED != 0
    }
}

/// A GIC distributor (MADT entry type 12).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gicd {
    /// Hardware identifier of this distributor. Only one is permitted.
    pub gic_id: u32,
    /// Physical address of the distributor registers.
    pub physical_base_address: u64,
    /// The interrupt number this distributor's first interrupt corresponds to.
    /// Reserved and zero from GICv3 onwards.
    pub system_vector_base: u32,
    /// 1 for `GICv1`, 2 for GICv2, 3 for GICv3, 4 for `GICv4`, 0 when firmware
    /// leaves it to the OS to probe.
    pub gic_version: u8,
}

/// A GIC redistributor discovery range (MADT entry type 14).
///
/// GICv3 describes redistributors as one contiguous region to walk rather than
/// one entry per processor, so the kernel strides through it reading `GICR_TYPER`
/// until it finds the `LAST` bit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gicr {
    /// Physical address the region starts at.
    pub discovery_range_base: u64,
    /// Bytes the region spans.
    pub discovery_range_length: u32,
}

/// One interrupt controller structure from the MADT.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MadtEntry {
    /// Type 0: a processor's local APIC.
    LocalApic(LocalApic),
    /// Type 1: an I/O APIC.
    IoApic(IoApic),
    /// Type 2: an interrupt source override.
    InterruptSourceOverride(InterruptSourceOverride),
    /// Type 4: an NMI wired to a local APIC pin.
    LocalApicNmi(LocalApicNmi),
    /// Type 5: a 64-bit local APIC address that supersedes the 32-bit one in
    /// the MADT header. Carries that address.
    LocalApicAddressOverride(u64),
    /// Type 9: a processor's local x2APIC.
    LocalX2Apic(LocalX2Apic),
    /// Type 11: a GIC CPU interface.
    Gicc(Gicc),
    /// Type 12: a GIC distributor.
    Gicd(Gicd),
    /// Type 14: a GIC redistributor discovery range.
    Gicr(Gicr),
    /// A structure this parser does not decode, skipped by its own `length`.
    Unknown {
        /// The entry's type byte.
        kind: u8,
        /// The entry's length byte.
        length: u8,
    },
    /// A structure of a type this parser knows, whose `length` is too small to
    /// hold the fields that type defines. The walk steps over it by that
    /// length and carries on, because the rest of the table is usually fine.
    Malformed {
        /// The entry's type byte.
        kind: u8,
        /// The entry's length byte.
        length: u8,
    },
}

/// The Multiple APIC Description Table: every interrupt controller in the
/// machine, and on AArch64 every processor as well.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Madt<'a> {
    table: Table<'a>,
}

impl<'a> Madt<'a> {
    /// Interpret `table` as a MADT.
    pub fn parse(table: Table<'a>) -> Result<Self, AcpiError> {
        table.expect_signature(MADT_SIGNATURE)?;
        if table.bytes().len() < MADT_HEADER_LEN {
            return Err(AcpiError::TooShort {
                got: table.bytes().len(),
                need: MADT_HEADER_LEN,
            });
        }
        Ok(Madt { table })
    }

    /// The underlying table, for its header and checksum.
    #[must_use]
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// The 32-bit physical address of the local interrupt controller: the local
    /// APIC on x86-64, unused on AArch64.
    ///
    /// [`Madt::local_apic_address`] is what a caller wants, since a type 5
    /// entry may replace this.
    #[must_use]
    pub fn local_controller_address(&self) -> u32 {
        // `parse` established the 44 bytes, so the default is unreachable.
        u32_at(self.table.bytes(), 36).unwrap_or(0)
    }

    /// The MADT flags word; see [`MADT_FLAG_PCAT_COMPAT`].
    #[must_use]
    pub fn flags(&self) -> u32 {
        u32_at(self.table.bytes(), 40).unwrap_or(0)
    }

    /// Whether the machine has 8259 PICs that must be masked before the I/O
    /// APIC is trusted.
    #[must_use]
    pub fn has_legacy_pics(&self) -> bool {
        self.flags() & MADT_FLAG_PCAT_COMPAT != 0
    }

    /// Where the local APIC registers actually are.
    ///
    /// A type 5 entry supersedes the 32-bit header field, which is how a
    /// machine relocates its APIC above 4 GiB. The last override in the table
    /// wins.
    #[must_use]
    pub fn local_apic_address(&self) -> u64 {
        let mut address = u64::from(self.local_controller_address());
        for entry in self.entries() {
            if let MadtEntry::LocalApicAddressOverride(overridden) = entry {
                address = overridden;
            }
        }
        address
    }

    /// Every interrupt controller structure, in table order.
    ///
    /// The walk stops at the first entry that is truncated or declares a length
    /// below two; use [`Madt::check_entries`] to find out whether that
    /// happened.
    #[must_use]
    pub fn entries(&self) -> MadtEntries<'a> {
        MadtEntries {
            body: self.entry_bytes(),
            offset: 0,
            budget: MAX_MADT_ENTRIES,
        }
    }

    /// Whether the entry list is well formed all the way to the end of the
    /// table.
    ///
    /// Separate from [`Madt::entries`] so that iterating is infallible and the
    /// caller pays for the diagnosis only if it wants it.
    pub fn check_entries(&self) -> Result<(), AcpiError> {
        let body = self.entry_bytes();
        let mut offset = 0usize;
        let mut budget = MAX_MADT_ENTRIES;
        while let Some(rest) = body.get(offset..) {
            if rest.len() < MADT_ENTRY_HEADER_LEN {
                // A single trailing byte is a truncated entry, not padding.
                return if rest.is_empty() {
                    Ok(())
                } else {
                    Err(AcpiError::EntryOutOfBounds { offset })
                };
            }
            if budget == 0 {
                return Err(AcpiError::TooManyEntries);
            }
            budget = budget.saturating_sub(1);
            let length = usize::from(u8_at(rest, 1).unwrap_or(0));
            if length < MADT_ENTRY_HEADER_LEN {
                return Err(AcpiError::ZeroLengthEntry { offset });
            }
            if length > rest.len() {
                return Err(AcpiError::EntryOutOfBounds { offset });
            }
            offset = offset
                .checked_add(length)
                .ok_or(AcpiError::TooManyEntries)?;
        }
        Ok(())
    }

    /// The bytes after the MADT's fixed 44-byte part.
    fn entry_bytes(&self) -> &'a [u8] {
        self.table.bytes().get(MADT_HEADER_LEN..).unwrap_or(&[])
    }
}

/// Iterator over the MADT's interrupt controller structures.
#[derive(Clone, Copy, Debug)]
pub struct MadtEntries<'a> {
    body: &'a [u8],
    offset: usize,
    budget: usize,
}

impl Iterator for MadtEntries<'_> {
    type Item = MadtEntry;

    fn next(&mut self) -> Option<MadtEntry> {
        if self.budget == 0 {
            return None;
        }
        let rest = self.body.get(self.offset..)?;
        let kind = u8_at(rest, 0)?;
        let length = usize::from(u8_at(rest, 1)?);
        // The classic MADT parser bug: firmware writes a zero length, the
        // parser adds it to the cursor and the machine hangs before it has said
        // anything. Two is the smallest an entry can be, since these two bytes
        // are part of it.
        if length < MADT_ENTRY_HEADER_LEN || length > rest.len() {
            return None;
        }
        let entry = rest.get(..length)?;
        self.offset = self.offset.checked_add(length)?;
        self.budget = self.budget.saturating_sub(1);
        Some(decode_madt_entry(kind, entry))
    }
}

/// Decode one entry, given its type byte and its own bytes.
///
/// `entry` is exactly the entry's declared length, so a short entry of a known
/// type produces [`MadtEntry::Malformed`] rather than reading its neighbour.
fn decode_madt_entry(kind: u8, entry: &[u8]) -> MadtEntry {
    let length = u8_at(entry, 1).unwrap_or(0);
    let decoded = match kind {
        0 => decode_local_apic(entry),
        1 => decode_io_apic(entry),
        2 => decode_source_override(entry),
        4 => decode_local_apic_nmi(entry),
        5 => u64_at(entry, 4).map(MadtEntry::LocalApicAddressOverride),
        9 => decode_local_x2apic(entry),
        11 => decode_gicc(entry),
        12 => decode_gicd(entry),
        14 => decode_gicr(entry),
        _ => return MadtEntry::Unknown { kind, length },
    };
    decoded.unwrap_or(MadtEntry::Malformed { kind, length })
}

/// Type 0, eight bytes.
fn decode_local_apic(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::LocalApic(LocalApic {
        processor_uid: u8_at(entry, 2)?,
        apic_id: u8_at(entry, 3)?,
        flags: u32_at(entry, 4)?,
    }))
}

/// Type 1, twelve bytes. Byte 3 is reserved.
fn decode_io_apic(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::IoApic(IoApic {
        id: u8_at(entry, 2)?,
        address: u32_at(entry, 4)?,
        gsi_base: u32_at(entry, 8)?,
    }))
}

/// Type 2, ten bytes.
fn decode_source_override(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::InterruptSourceOverride(
        InterruptSourceOverride {
            bus: u8_at(entry, 2)?,
            source: u8_at(entry, 3)?,
            gsi: u32_at(entry, 4)?,
            flags: u16_at(entry, 8)?,
        },
    ))
}

/// Type 4, six bytes.
fn decode_local_apic_nmi(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::LocalApicNmi(LocalApicNmi {
        processor_uid: u8_at(entry, 2)?,
        flags: u16_at(entry, 3)?,
        lint: u8_at(entry, 5)?,
    }))
}

/// Type 9, sixteen bytes. Bytes 2 and 3 are reserved.
fn decode_local_x2apic(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::LocalX2Apic(LocalX2Apic {
        x2apic_id: u32_at(entry, 4)?,
        flags: u32_at(entry, 8)?,
        processor_uid: u32_at(entry, 12)?,
    }))
}

/// Type 11. Seventy-six bytes through ACPI 5.1, eighty from 6.0, which added
/// the power efficiency class; both are accepted and the missing byte reads as
/// zero, since that is also what a homogeneous machine reports.
fn decode_gicc(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::Gicc(Gicc {
        cpu_interface_number: u32_at(entry, 4)?,
        acpi_processor_uid: u32_at(entry, 8)?,
        flags: u32_at(entry, 12)?,
        parking_protocol_version: u32_at(entry, 16)?,
        performance_interrupt_gsiv: u32_at(entry, 20)?,
        parked_address: u64_at(entry, 24)?,
        physical_base_address: u64_at(entry, 32)?,
        gicv: u64_at(entry, 40)?,
        gich: u64_at(entry, 48)?,
        vgic_maintenance_interrupt: u32_at(entry, 56)?,
        gicr_base_address: u64_at(entry, 60)?,
        mpidr: u64_at(entry, 68)?,
        power_efficiency_class: u8_at(entry, 76).unwrap_or(0),
    }))
}

/// Type 12, twenty-four bytes.
fn decode_gicd(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::Gicd(Gicd {
        gic_id: u32_at(entry, 4)?,
        physical_base_address: u64_at(entry, 8)?,
        system_vector_base: u32_at(entry, 16)?,
        gic_version: u8_at(entry, 20)?,
    }))
}

/// Type 14, sixteen bytes.
fn decode_gicr(entry: &[u8]) -> Option<MadtEntry> {
    Some(MadtEntry::Gicr(Gicr {
        discovery_range_base: u64_at(entry, 4)?,
        discovery_range_length: u32_at(entry, 12)?,
    }))
}

// ---------------------------------------------------------------------------
// FADT
// ---------------------------------------------------------------------------

/// A generic address structure: where a register lives and how wide it is
/// (ACPI 6.5 section 5.2.3.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GenericAddress {
    /// 0 for system memory, 1 for I/O space, and so on.
    pub address_space_id: u8,
    /// Width of the register in bits.
    pub register_bit_width: u8,
    /// Offset of the register within the addressed unit, in bits.
    pub register_bit_offset: u8,
    /// Access width: 1 byte, 2 word, 3 dword, 4 qword.
    pub access_size: u8,
    /// The register's address in the given space.
    pub address: u64,
}

impl GenericAddress {
    /// `address_space_id` for system memory.
    pub const SYSTEM_MEMORY: u8 = 0;
    /// `address_space_id` for the x86 I/O port space.
    pub const SYSTEM_IO: u8 = 1;

    /// Decode a generic address at `offset`, or `None` past the end.
    #[must_use]
    pub fn parse(bytes: &[u8], offset: usize) -> Option<Self> {
        let field = bytes.get(offset..offset.checked_add(GENERIC_ADDRESS_LEN)?)?;
        Some(GenericAddress {
            address_space_id: u8_at(field, 0)?,
            register_bit_width: u8_at(field, 1)?,
            register_bit_offset: u8_at(field, 2)?,
            access_size: u8_at(field, 3)?,
            address: u64_at(field, 4)?,
        })
    }

    /// Whether the structure describes a register at all. Firmware zeroes the
    /// whole structure for a register that is not implemented.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        self.address != 0
    }
}

/// Everything the ACPI power management timer needs to be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PmTimer {
    /// I/O port of the timer's counter register.
    pub port: u32,
    /// Width of the register block in bytes; four when it exists.
    pub length: u8,
    /// Whether the counter is 32 bits wide rather than 24. Reading the wrong
    /// width means a wrap the kernel does not see, so this is not cosmetic.
    pub is_32_bit: bool,
    /// The 64-bit description of the same register, if the FADT is long enough
    /// to have one and firmware filled it in.
    pub extended: Option<GenericAddress>,
}

/// The Fixed ACPI Description Table, as far as its fixed fields.
///
/// The FADT's real payload is a pointer to the DSDT, which is AML; this crate
/// stops at the fields that can be read as numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fadt<'a> {
    table: Table<'a>,
}

impl<'a> Fadt<'a> {
    /// Interpret `table` as an FADT.
    ///
    /// Only the signature is required: the FADT grew field by field across
    /// revisions and a short one is a machine on an old specification, not a
    /// broken table, so every accessor below answers `None` instead.
    pub fn parse(table: Table<'a>) -> Result<Self, AcpiError> {
        table.expect_signature(FADT_SIGNATURE)?;
        Ok(Fadt { table })
    }

    /// The underlying table, for its header and checksum.
    #[must_use]
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// Physical address of the FACS, where the waking vector lives.
    #[must_use]
    pub fn firmware_ctrl(&self) -> Option<u32> {
        u32_at(self.table.bytes(), 36)
    }

    /// Physical address of the DSDT, preferring the 64-bit field when firmware
    /// filled it in.
    #[must_use]
    pub fn dsdt_address(&self) -> Option<u64> {
        match u64_at(self.table.bytes(), 140) {
            Some(extended) if extended != 0 => Some(extended),
            _ => u32_at(self.table.bytes(), 40).map(u64::from),
        }
    }

    /// The FADT flags word.
    #[must_use]
    pub fn flags(&self) -> Option<u32> {
        u32_at(self.table.bytes(), 112)
    }

    /// The offset into CMOS RAM of the century byte, or `None` when the FADT is
    /// too short to have the field.
    ///
    /// Zero means the machine has no century byte, which is why this is not
    /// flattened into an `Option`: absent field and absent register are
    /// different facts.
    #[must_use]
    pub fn century(&self) -> Option<u8> {
        u8_at(self.table.bytes(), 108)
    }

    /// The `IAPC_BOOT_ARCH` flags, which say whether legacy PC devices are
    /// present. Meaningless on AArch64.
    #[must_use]
    pub fn iapc_boot_arch(&self) -> Option<u16> {
        u16_at(self.table.bytes(), 109)
    }

    /// The `ARM_BOOT_ARCH` flags: bit 0 says PSCI is implemented, bit 1 that it
    /// is entered with `HVC` rather than `SMC`.
    #[must_use]
    pub fn arm_boot_arch(&self) -> Option<u16> {
        u16_at(self.table.bytes(), 129)
    }

    /// I/O port of the power management timer, if the FADT has the field.
    #[must_use]
    pub fn pm_timer_block(&self) -> Option<u32> {
        u32_at(self.table.bytes(), 76)
    }

    /// Width in bytes of the power management timer block.
    #[must_use]
    pub fn pm_timer_length(&self) -> Option<u8> {
        u8_at(self.table.bytes(), 91)
    }

    /// The 64-bit description of the power management timer, from the extended
    /// part of the FADT.
    #[must_use]
    pub fn x_pm_timer_block(&self) -> Option<GenericAddress> {
        GenericAddress::parse(self.table.bytes(), 208)
    }

    /// The power management timer, or `None` when the machine has none.
    ///
    /// AArch64 machines generally have none: the architected generic timer
    /// described by the GTDT takes its place.
    #[must_use]
    pub fn pm_timer(&self) -> Option<PmTimer> {
        let port = self.pm_timer_block()?;
        if port == 0 {
            return None;
        }
        Some(PmTimer {
            port,
            length: self.pm_timer_length().unwrap_or(4),
            // Flags bit 8 is TMR_VAL_EXT: set means the counter is 32 bits,
            // clear means it wraps at 24 (ACPI 6.5 table 5.11).
            is_32_bit: self.flags().unwrap_or(0) & (1 << 8) != 0,
            extended: self.x_pm_timer_block().filter(GenericAddress::is_present),
        })
    }
}

// ---------------------------------------------------------------------------
// GTDT
// ---------------------------------------------------------------------------

/// One of the architected generic timers and the interrupt it signals on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GenericTimer {
    /// The global system interrupt vector: a private peripheral interrupt, so
    /// in the range 16..32.
    pub gsiv: u32,
    /// The flags word; see the `GTDT_FLAG_*` constants.
    pub flags: u32,
}

impl GenericTimer {
    /// Whether the interrupt is edge triggered rather than level triggered.
    #[must_use]
    pub const fn is_edge_triggered(&self) -> bool {
        self.flags & GTDT_FLAG_EDGE_TRIGGERED != 0
    }

    /// Whether the interrupt is active low rather than active high.
    #[must_use]
    pub const fn is_active_low(&self) -> bool {
        self.flags & GTDT_FLAG_ACTIVE_LOW != 0
    }

    /// Whether the timer keeps counting through low-power states.
    #[must_use]
    pub const fn is_always_on(&self) -> bool {
        self.flags & GTDT_FLAG_ALWAYS_ON != 0
    }

    /// Whether the timer is described at all: firmware zeroes the vector of a
    /// timer it does not implement, and interrupt 0 is not a valid PPI.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        self.gsiv != 0
    }
}

/// Bytes of the GTDT through the last of the four EL1 and virtual timer fields.
const GTDT_MIN_LEN: usize = 80;

/// The Generic Timer Description Table: how the AArch64 architected timers are
/// wired to the interrupt controller.
///
/// There is no equivalent on x86-64; the FADT's power management timer and the
/// HPET play the part.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gtdt<'a> {
    table: Table<'a>,
}

impl<'a> Gtdt<'a> {
    /// Interpret `table` as a GTDT.
    ///
    /// Requires the table to reach the end of the four timer descriptions, so
    /// that the four accessors below cannot fail. A machine whose GTDT is
    /// shorter than that has not described its timers.
    pub fn parse(table: Table<'a>) -> Result<Self, AcpiError> {
        table.expect_signature(GTDT_SIGNATURE)?;
        if table.bytes().len() < GTDT_MIN_LEN {
            return Err(AcpiError::TooShort {
                got: table.bytes().len(),
                need: GTDT_MIN_LEN,
            });
        }
        Ok(Gtdt { table })
    }

    /// The underlying table, for its header and checksum.
    #[must_use]
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// Physical address of the counter control block, or `None` when the
    /// machine does not implement one (the all-ones encoding).
    #[must_use]
    pub fn counter_control_base(&self) -> Option<u64> {
        u64_at(self.table.bytes(), 36).filter(|base| *base != GTDT_NO_COUNTER_BASE)
    }

    /// Physical address of the counter read block, on the same encoding.
    #[must_use]
    pub fn counter_read_base(&self) -> Option<u64> {
        u64_at(self.table.bytes(), 80).filter(|base| *base != GTDT_NO_COUNTER_BASE)
    }

    /// The secure EL1 timer, which a non-secure kernel cannot use but should
    /// know about.
    #[must_use]
    pub fn secure_el1_timer(&self) -> GenericTimer {
        self.timer_at(48)
    }

    /// The non-secure EL1 timer: `CNTP` as the kernel sees it.
    #[must_use]
    pub fn non_secure_el1_timer(&self) -> GenericTimer {
        self.timer_at(56)
    }

    /// The virtual timer: `CNTV`, and the one a kernel under a hypervisor uses.
    #[must_use]
    pub fn virtual_el1_timer(&self) -> GenericTimer {
        self.timer_at(64)
    }

    /// The non-secure EL2 timer, for a kernel running as a hypervisor.
    #[must_use]
    pub fn non_secure_el2_timer(&self) -> GenericTimer {
        self.timer_at(72)
    }

    /// The virtual EL2 timer, added in ACPI 6.3 and absent from shorter tables.
    #[must_use]
    pub fn virtual_el2_timer(&self) -> Option<GenericTimer> {
        Some(GenericTimer {
            gsiv: u32_at(self.table.bytes(), 96)?,
            flags: u32_at(self.table.bytes(), 100)?,
        })
    }

    /// How many platform timer structures follow the fixed part.
    #[must_use]
    pub fn platform_timer_count(&self) -> Option<u32> {
        u32_at(self.table.bytes(), 88)
    }

    /// A timer whose vector is at `offset` and whose flags are four bytes
    /// later. `parse` established the length, so the zero default is
    /// unreachable and means "not described" in any case.
    fn timer_at(&self, offset: usize) -> GenericTimer {
        GenericTimer {
            gsiv: u32_at(self.table.bytes(), offset).unwrap_or(0),
            flags: u32_at(self.table.bytes(), offset.saturating_add(4)).unwrap_or(0),
        }
    }
}

// ---------------------------------------------------------------------------
// HPET
// ---------------------------------------------------------------------------

/// Bytes the fixed part of an HPET description table occupies.
///
/// The table has no variable part, so a shorter one has not described a timer
/// block at all and every accessor below would be reading off the end.
pub const HPET_MIN_LEN: usize = 56;

/// Hardware id: the comparator count, less one, lives in bits 8..13.
const HPET_COMPARATORS_SHIFT: u32 = 8;

/// Hardware id: five bits of comparator count.
const HPET_COMPARATORS_MASK: u32 = 0b1_1111;

/// Hardware id: the main counter is 64 bits wide rather than 32.
const HPET_COUNTER_64BIT: u32 = 1 << 13;

/// Hardware id: the block can take over the PIT and RTC interrupts.
const HPET_LEGACY_CAPABLE: u32 = 1 << 15;

/// Hardware id: the PCI vendor identifier occupies the top sixteen bits.
const HPET_VENDOR_SHIFT: u32 = 16;

/// The HPET description table: where the timer block's registers are.
///
/// Deliberately thin. This table says *where* to look and what the block
/// claims about itself; the authoritative width, period and comparator count
/// are in the block's own capability register, which is `MMIO` and therefore
/// not this crate's business. Firmware has been known to disagree with the
/// hardware here, and when it does the hardware is right — so a caller should
/// treat [`Hpet::comparators`] and [`Hpet::counter_is_64_bit`] as a hint and
/// the register as the answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hpet<'a> {
    /// The table these fields are read out of.
    table: Table<'a>,
}

impl<'a> Hpet<'a> {
    /// Interpret `table` as an HPET description table.
    ///
    /// # Errors
    ///
    /// [`AcpiError::WrongSignature`] if it is not an HPET, and
    /// [`AcpiError::TooShort`] if it does not reach the end of the fixed
    /// fields.
    pub fn parse(table: Table<'a>) -> Result<Self, AcpiError> {
        table.expect_signature(HPET_SIGNATURE)?;
        if table.bytes().len() < HPET_MIN_LEN {
            return Err(AcpiError::TooShort {
                got: table.bytes().len(),
                need: HPET_MIN_LEN,
            });
        }
        Ok(Hpet { table })
    }

    /// The underlying table, for its header and checksum.
    #[must_use]
    pub const fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// The hardware id word. `parse` established the length, so the zero
    /// default is unreachable.
    fn hardware_id(&self) -> u32 {
        u32_at(self.table.bytes(), 36).unwrap_or(0)
    }

    /// The block's revision, as firmware reports it.
    #[must_use]
    pub fn revision(&self) -> u8 {
        self.hardware_id() as u8
    }

    /// How many comparators the block claims, counting from one.
    ///
    /// Stored less one, so the encoding cannot express a block with none —
    /// which is correct, because a timer block with no comparators would have
    /// nothing to describe.
    #[must_use]
    pub fn comparators(&self) -> u8 {
        let encoded = (self.hardware_id() >> HPET_COMPARATORS_SHIFT) & HPET_COMPARATORS_MASK;
        (encoded as u8).saturating_add(1)
    }

    /// Whether the main counter is 64 bits wide.
    ///
    /// A 32-bit counter wraps every few minutes at typical frequencies, so a
    /// caller using it as a time base has to handle the wrap rather than
    /// subtracting two reads and believing the answer.
    #[must_use]
    pub fn counter_is_64_bit(&self) -> bool {
        self.hardware_id() & HPET_COUNTER_64BIT != 0
    }

    /// Whether the block can replace the PIT and RTC periodic interrupts.
    #[must_use]
    pub fn supports_legacy_replacement(&self) -> bool {
        self.hardware_id() & HPET_LEGACY_CAPABLE != 0
    }

    /// The PCI vendor identifier of whoever implemented the block.
    #[must_use]
    pub fn vendor(&self) -> u16 {
        (self.hardware_id() >> HPET_VENDOR_SHIFT) as u16
    }

    /// Where the block's registers are.
    ///
    /// A generic address rather than a bare number because the specification
    /// permits an I/O-space block, and a caller that assumed memory would map
    /// a window over whatever happens to be at that physical address.
    #[must_use]
    pub fn base_address(&self) -> Option<GenericAddress> {
        GenericAddress::parse(self.table.bytes(), 40)
    }

    /// Which timer block this is, on a machine with more than one.
    #[must_use]
    pub fn block_number(&self) -> Option<u8> {
        u8_at(self.table.bytes(), 52)
    }

    /// The smallest period, in main counter ticks, that the block can be
    /// programmed to in periodic mode without losing interrupts.
    #[must_use]
    pub fn minimum_tick(&self) -> Option<u16> {
        u16_at(self.table.bytes(), 53)
    }

    /// Page protection and OEM attributes, as firmware reports them.
    #[must_use]
    pub fn page_protection(&self) -> Option<u8> {
        u8_at(self.table.bytes(), 55)
    }
}

#[cfg(test)]
mod tests;
