//! Flattened device tree (DTB) reader.
//!
//! On AArch64 the firmware hands the loader a device tree and that blob is the
//! only description of the machine the kernel gets: where RAM is, which UART is
//! the console, where the interrupt controller lives, which processors there
//! are. This crate turns those
//! bytes into answers and nothing else — it does not modify a tree or apply
//! overlays, and follows a phandle only when asked for the node it names.
//!
//! # Totality
//!
//! The input is chosen by firmware and parsed in ring 0, so every function here
//! either returns a value or an error for *any* byte string. Fields are pulled
//! out with a bounds-checked slice access and [`u32::from_be_bytes`], which is
//! why the crate is `#![forbid(unsafe_code)]`: casting the blob to a header
//! pointer would be shorter and would put the sharpest surface in the system
//! into an `unsafe` block for nothing.
//!
//! [`Fdt::parse`] walks the whole structure block once and rejects the blob if
//! anything in it is malformed, so the accessors afterwards cannot fail — they
//! answer `None` when a node or property is absent, never when the blob is bad.
//! The walk is iterative over an explicit [`MAX_DEPTH`] frame stack, so a tree
//! that nests forever is refused rather than recursed into.
//!
//! ```
//! # use ferrix_fdt::{Fdt, FdtError};
//! # fn example(blob: &[u8]) -> Result<(), FdtError> {
//! let fdt = Fdt::parse(blob)?;
//! for region in fdt.memory() {
//!     let _ = (region.address, region.size);
//! }
//! if let Some(console) = fdt.console() {
//!     let _ = (console.compatible(), console.reg().next());
//! }
//! # Ok(())
//! # }
//! ```

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

// ---------------------------------------------------------------------------
// Format constants
// ---------------------------------------------------------------------------

/// The word every device tree blob starts with.
pub const FDT_MAGIC: u32 = 0xd00d_feed;

/// Opens a node. A NUL-terminated name, padded to four bytes, follows.
pub const FDT_BEGIN_NODE: u32 = 1;
/// Closes the innermost open node.
pub const FDT_END_NODE: u32 = 2;
/// Introduces a property: a length, a name offset, then that many bytes of
/// value padded to four.
pub const FDT_PROP: u32 = 3;
/// Padding. Skipped wherever a token is expected.
pub const FDT_NOP: u32 = 4;
/// Ends the structure block.
pub const FDT_END: u32 = 9;

/// Size of the header: ten big-endian `u32` fields.
pub const HEADER_SIZE: usize = 40;

/// The structure block layout this parser understands.
///
/// Version 17 is what every producer in use emits (dtc, QEMU, U-Boot, EDK2),
/// and it is the first version carrying `size_dt_struct`. Accepting an older
/// layout would mean guessing where the structure block ends, which is exactly
/// the guess a hostile blob would want us to make.
pub const SUPPORTED_VERSION: u32 = 17;

/// Deepest node nesting accepted, the root included.
///
/// The walk is iterative and keeps one frame per open node, so this bounds both
/// the stack footprint (a little under a kilobyte) and the work a blob can ask
/// for. Real trees are five or six levels deep.
pub const MAX_DEPTH: usize = 64;

/// Most structure block tokens read before a blob is declared hostile.
///
/// Termination does not rest on this — every token advances the cursor by at
/// least four bytes inside a fixed slice — but a blob that is all padding still
/// should not cost more than a bounded amount of time.
pub const MAX_TOKENS: usize = 1 << 22;

/// `#address-cells` in effect when no ancestor declares one.
pub const DEFAULT_ADDRESS_CELLS: u32 = 2;

/// `#size-cells` in effect when no ancestor declares one.
///
/// The specification's stated default is 1. Two is used here because every tree
/// this kernel is handed declares both at the root, and on a 64-bit machine a
/// missing declaration is likelier to mean "as wide as the address" than "a
/// 32-bit size". A caller that cares should read the property itself.
pub const DEFAULT_SIZE_CELLS: u32 = 2;

/// Widest `#address-cells` or `#size-cells` honoured. A wider declaration is
/// ignored in favour of the inherited value.
pub const MAX_CELLS: u32 = 4;

/// `compatible` string of a GICv3 interrupt controller.
pub const GICV3_COMPATIBLE: &str = "arm,gic-v3";

/// `compatible` string of a GICv3's Interrupt Translation Service, a child of
/// the GIC's node.
pub const GICV3_ITS_COMPATIBLE: &str = "arm,gic-v3-its";

/// `compatible` strings of a GICv2. QEMU's `virt` machine says
/// `cortex-a15-gic` whatever the CPU, the STM32MP1 says `cortex-a7-gic`, and
/// `gic-400` is what most other boards of that generation carry. The register
/// layout is the same for all three.
pub const GICV2_COMPATIBLES: [&str; 3] = ["arm,cortex-a15-gic", "arm,cortex-a7-gic", "arm,gic-400"];

/// `compatible` string of the architected timer on a 64-bit CPU.
pub const TIMER_COMPATIBLE: &str = "arm,armv8-timer";

/// `compatible` string of the same timer on a 32-bit CPU. QEMU lists it after
/// [`TIMER_COMPATIBLE`] for a 64-bit CPU and alone for a 32-bit one, so a
/// reader that knew only the first would find no timer on a Cortex-A7.
pub const TIMER_V7_COMPATIBLE: &str = "arm,armv7-timer";

/// `compatible` strings of PSCI firmware, newest first.
pub const PSCI_COMPATIBLES: [&str; 3] = ["arm,psci-1.0", "arm,psci-0.2", "arm,psci"];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a blob was rejected.
///
/// Every variant means the same thing operationally — do not trust this tree —
/// but they are distinguished because a truncated blob and a blob that is not a
/// device tree at all call for very different responses from whoever reads the
/// message on the console.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FdtError {
    /// The blob is smaller than a device tree header.
    TooShort,
    /// The first word is not [`FDT_MAGIC`]. Carries what was found.
    BadMagic(u32),
    /// `totalsize` is smaller than a header or larger than the slice given.
    BadTotalSize,
    /// The blob is laid out for a version this parser does not read. Carries
    /// the `version` field.
    UnsupportedVersion(u32),
    /// A block offset or size violates the alignment the format requires.
    Misaligned,
    /// The structure or strings block does not lie inside `totalsize`.
    BlockOutOfBounds,
    /// The memory reservation block does not lie inside `totalsize`.
    ReservationOutOfBounds,
    /// A token that is none of the five defined ones. Carries its value.
    BadToken(u32),
    /// A node or property name runs to the end of its block with no NUL.
    UnterminatedString,
    /// A node or property name is not UTF-8.
    NotUtf8,
    /// A property's value runs past the end of the structure block.
    PropertyOutOfBounds,
    /// Nodes nest deeper than [`MAX_DEPTH`].
    DepthOverflow,
    /// A node ended that never began, or the tree ended with a node open.
    UnbalancedNode,
    /// The structure block runs out before [`FDT_END`].
    MissingEnd,
    /// The structure block holds more than [`MAX_TOKENS`] tokens.
    TokenBudget,
}

impl fmt::Display for FdtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FdtError::TooShort => f.write_str("blob is shorter than a device tree header"),
            FdtError::BadMagic(found) => write!(f, "magic {found:#010x} is not a device tree"),
            FdtError::BadTotalSize => f.write_str("totalsize does not fit the blob"),
            FdtError::UnsupportedVersion(found) => write!(f, "device tree version {found}"),
            FdtError::Misaligned => f.write_str("a block offset is misaligned"),
            FdtError::BlockOutOfBounds => f.write_str("a block lies outside the blob"),
            FdtError::ReservationOutOfBounds => {
                f.write_str("the reservation block lies outside the blob")
            }
            FdtError::BadToken(found) => write!(f, "token {found} is not a device tree token"),
            FdtError::UnterminatedString => f.write_str("a name has no terminator"),
            FdtError::NotUtf8 => f.write_str("a name is not UTF-8"),
            FdtError::PropertyOutOfBounds => {
                f.write_str("a property runs past the structure block")
            }
            FdtError::DepthOverflow => f.write_str("nodes nest too deeply"),
            FdtError::UnbalancedNode => f.write_str("node begin and end tokens are unbalanced"),
            FdtError::MissingEnd => f.write_str("the structure block has no end token"),
            FdtError::TokenBudget => f.write_str("the structure block has too many tokens"),
        }
    }
}

// ---------------------------------------------------------------------------
// Big-endian field access
// ---------------------------------------------------------------------------

/// Read a big-endian `u32` at `offset`, or `None` past the end.
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let field: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_be_bytes(field))
}

/// Read a big-endian `u64` at `offset`, or `None` past the end.
fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    let field: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_be_bytes(field))
}

/// The NUL-terminated string starting at `offset`, without its terminator.
fn cstr_at(bytes: &[u8], offset: usize) -> Result<&str, FdtError> {
    let tail = bytes.get(offset..).ok_or(FdtError::UnterminatedString)?;
    let end = tail
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(FdtError::UnterminatedString)?;
    let text = tail.get(..end).ok_or(FdtError::UnterminatedString)?;
    core::str::from_utf8(text).map_err(|_| FdtError::NotUtf8)
}

/// Round `len` up to the four-byte boundary the token stream pads to.
fn align4(len: usize) -> Option<usize> {
    Some(len.checked_add(3)? & !3)
}

/// The part of a node name before its unit address, so that `memory` names the
/// same node as `memory@40000000`.
fn base_name(name: &str) -> &str {
    name.split('@').next().unwrap_or(name)
}

/// Read `cells` consecutive big-endian words as one integer, advancing
/// `offset`.
///
/// `None` if the words run out or the value does not fit in 64 bits. Cell
/// counts other than one and two are read rather than refused, so that a
/// three-cell PCI-style address with a zero high word still yields an answer.
fn read_cells(value: &[u8], offset: &mut usize, cells: u32) -> Option<u64> {
    let mut accumulated: u64 = 0;
    for _ in 0..cells {
        let word = u32_at(value, *offset)?;
        if accumulated >> 32 != 0 {
            return None;
        }
        accumulated = (accumulated << 32) | u64::from(word);
        *offset = offset.checked_add(4)?;
    }
    Some(accumulated)
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// The device tree header, decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    /// Size of the whole blob, magic word included.
    pub totalsize: u32,
    /// Offset of the structure block from the start of the blob.
    pub off_dt_struct: u32,
    /// Offset of the strings block from the start of the blob.
    pub off_dt_strings: u32,
    /// Offset of the memory reservation block from the start of the blob.
    pub off_mem_rsvmap: u32,
    /// Layout version this blob was written to.
    pub version: u32,
    /// Oldest layout version this blob is still readable as.
    pub last_comp_version: u32,
    /// Physical ID of the CPU the firmware is running on.
    pub boot_cpuid_phys: u32,
    /// Size of the strings block in bytes.
    pub size_dt_strings: u32,
    /// Size of the structure block in bytes.
    pub size_dt_struct: u32,
}

impl Header {
    /// Decode and check the header at the start of `blob`.
    pub fn parse(blob: &[u8]) -> Result<Self, FdtError> {
        let magic = u32_at(blob, 0).ok_or(FdtError::TooShort)?;
        if magic != FDT_MAGIC {
            return Err(FdtError::BadMagic(magic));
        }
        if blob.len() < HEADER_SIZE {
            return Err(FdtError::TooShort);
        }
        let header = Header {
            totalsize: u32_at(blob, 4).ok_or(FdtError::TooShort)?,
            off_dt_struct: u32_at(blob, 8).ok_or(FdtError::TooShort)?,
            off_dt_strings: u32_at(blob, 12).ok_or(FdtError::TooShort)?,
            off_mem_rsvmap: u32_at(blob, 16).ok_or(FdtError::TooShort)?,
            version: u32_at(blob, 20).ok_or(FdtError::TooShort)?,
            last_comp_version: u32_at(blob, 24).ok_or(FdtError::TooShort)?,
            boot_cpuid_phys: u32_at(blob, 28).ok_or(FdtError::TooShort)?,
            size_dt_strings: u32_at(blob, 32).ok_or(FdtError::TooShort)?,
            size_dt_struct: u32_at(blob, 36).ok_or(FdtError::TooShort)?,
        };
        header.check(blob.len())?;
        Ok(header)
    }

    /// Reject a header whose own fields already contradict each other or the
    /// `available` bytes, before any block is sliced out of the blob.
    fn check(&self, available: usize) -> Result<(), FdtError> {
        let total = usize::try_from(self.totalsize).map_err(|_| FdtError::BadTotalSize)?;
        if total < HEADER_SIZE || total > available {
            return Err(FdtError::BadTotalSize);
        }
        if self.version < SUPPORTED_VERSION || self.last_comp_version > SUPPORTED_VERSION {
            return Err(FdtError::UnsupportedVersion(self.version));
        }
        // The reservation block holds 64-bit pairs and the structure block
        // 32-bit tokens; the format requires each block to be aligned for the
        // words it holds, and a misaligned one is a malformed blob rather than
        // something to read anyway.
        if !self.off_mem_rsvmap.is_multiple_of(8) {
            return Err(FdtError::Misaligned);
        }
        if !self.off_dt_struct.is_multiple_of(4) || !self.size_dt_struct.is_multiple_of(4) {
            return Err(FdtError::Misaligned);
        }
        // Even an empty reservation list is a terminating pair of zero words,
        // so those sixteen bytes always have to be there.
        let rsvmap_end = self
            .off_mem_rsvmap
            .checked_add(16)
            .ok_or(FdtError::ReservationOutOfBounds)?;
        if self.off_mem_rsvmap < HEADER_SIZE as u32 || rsvmap_end > self.totalsize {
            return Err(FdtError::ReservationOutOfBounds);
        }
        Ok(())
    }
}

/// The slice of `blob` a block header field describes.
fn block(blob: &[u8], offset: u32, size: u32) -> Result<&[u8], FdtError> {
    let start = usize::try_from(offset).map_err(|_| FdtError::BlockOutOfBounds)?;
    let len = usize::try_from(size).map_err(|_| FdtError::BlockOutOfBounds)?;
    let end = start.checked_add(len).ok_or(FdtError::BlockOutOfBounds)?;
    blob.get(start..end).ok_or(FdtError::BlockOutOfBounds)
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// One `address, size` pair out of a `reg` property.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    /// First address of the region, decoded with the parent's
    /// `#address-cells`.
    pub address: u64,
    /// Length of the region in bytes, decoded with the parent's
    /// `#size-cells`. Zero when the parent declares `#size-cells = <0>`.
    pub size: u64,
}

/// One entry of the memory reservation block: memory the kernel must not hand
/// to the frame allocator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemoryReservation {
    /// Physical address of the reserved range.
    pub address: u64,
    /// Length of the reserved range in bytes.
    pub size: u64,
}

/// One property of one node. The value is borrowed from the blob and is
/// uninterpreted: the accessors decode it, they never copy it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Property<'a> {
    /// Property name, from the strings block.
    pub name: &'a str,
    /// Raw value bytes, exactly as long as the blob says.
    pub value: &'a [u8],
}

impl<'a> Property<'a> {
    /// Length of the raw value in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.value.len()
    }

    /// True if the value is empty, which is how the format spells a boolean
    /// property that is simply present.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// The value as a single big-endian word, or `None` if it is not exactly
    /// one word long.
    #[must_use]
    pub fn as_u32(&self) -> Option<u32> {
        if self.value.len() != 4 {
            return None;
        }
        u32_at(self.value, 0)
    }

    /// The value as a single big-endian doubleword, or `None` if it is not
    /// exactly two words long.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        if self.value.len() != 8 {
            return None;
        }
        u64_at(self.value, 0)
    }

    /// The value as a string: the bytes up to the first NUL.
    #[must_use]
    pub fn as_str(&self) -> Option<&'a str> {
        cstr_at(self.value, 0).ok()
    }

    /// The value as a list of strings, which is how `compatible` and
    /// `stdout-path` alternatives are encoded.
    #[must_use]
    pub const fn strings(&self) -> Strings<'a> {
        Strings {
            value: self.value,
            offset: 0,
        }
    }

    /// True if the value is a string list holding exactly `needle`.
    #[must_use]
    pub fn contains_string(&self, needle: &str) -> bool {
        self.strings().any(|entry| entry == needle)
    }

    /// The value as a sequence of big-endian words, which is how `interrupts`
    /// and the cell properties are encoded.
    #[must_use]
    pub const fn cells(&self) -> Cells<'a> {
        Cells {
            value: self.value,
            offset: 0,
        }
    }
}

/// The strings in a string-list property, in order.
#[derive(Clone, Copy, Debug)]
pub struct Strings<'a> {
    value: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for Strings<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.offset >= self.value.len() {
            return None;
        }
        let text = cstr_at(self.value, self.offset).ok()?;
        self.offset = self.offset.checked_add(text.len())?.checked_add(1)?;
        Some(text)
    }
}

/// The big-endian words of a property value, in order.
#[derive(Clone, Copy, Debug)]
pub struct Cells<'a> {
    value: &'a [u8],
    offset: usize,
}

impl Iterator for Cells<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let word = u32_at(self.value, self.offset)?;
        self.offset = self.offset.checked_add(4)?;
        Some(word)
    }
}

/// The `address, size` pairs of a `reg` property, decoded with the cell counts
/// its parent node declared.
#[derive(Clone, Copy, Debug)]
pub struct Regions<'a> {
    value: &'a [u8],
    offset: usize,
    address_cells: u32,
    size_cells: u32,
}

impl Iterator for Regions<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        // A pair of zero-width fields would consume nothing and never end the
        // iteration, so a parent that declares both as zero has no regions.
        if self.address_cells == 0 && self.size_cells == 0 {
            return None;
        }
        let mut offset = self.offset;
        let address = read_cells(self.value, &mut offset, self.address_cells)?;
        let size = read_cells(self.value, &mut offset, self.size_cells)?;
        self.offset = offset;
        Some(Region { address, size })
    }
}

// ---------------------------------------------------------------------------
// The token cursor
// ---------------------------------------------------------------------------

/// A position in the structure block.
///
/// Every method advances the position by at least four bytes inside a fixed
/// slice, which is what makes every walk in this crate terminate without
/// needing to trust the blob.
#[derive(Clone, Copy, Debug)]
struct Cursor<'a> {
    structs: &'a [u8],
    strings: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of the structure block.
    const fn new(structs: &'a [u8], strings: &'a [u8]) -> Self {
        Cursor {
            structs,
            strings,
            position: 0,
        }
    }

    /// Read one token and step over it.
    fn token(&mut self) -> Result<u32, FdtError> {
        let token = u32_at(self.structs, self.position).ok_or(FdtError::MissingEnd)?;
        self.position = self.position.checked_add(4).ok_or(FdtError::MissingEnd)?;
        Ok(token)
    }

    /// Read the node name that follows [`FDT_BEGIN_NODE`] and step over it and
    /// its padding.
    fn node_name(&mut self) -> Result<&'a str, FdtError> {
        let name = cstr_at(self.structs, self.position)?;
        let length = name
            .len()
            .checked_add(1)
            .ok_or(FdtError::UnterminatedString)?;
        let step = align4(length).ok_or(FdtError::UnterminatedString)?;
        self.position = self
            .position
            .checked_add(step)
            .ok_or(FdtError::PropertyOutOfBounds)?;
        Ok(name)
    }

    /// Read the length, name and value that follow [`FDT_PROP`] and step over
    /// them and the value's padding.
    fn property(&mut self) -> Result<Property<'a>, FdtError> {
        let len = usize::try_from(self.token()?).map_err(|_| FdtError::PropertyOutOfBounds)?;
        let nameoff = usize::try_from(self.token()?).map_err(|_| FdtError::PropertyOutOfBounds)?;
        let name = cstr_at(self.strings, nameoff)?;
        let end = self
            .position
            .checked_add(len)
            .ok_or(FdtError::PropertyOutOfBounds)?;
        let value = self
            .structs
            .get(self.position..end)
            .ok_or(FdtError::PropertyOutOfBounds)?;
        self.position = self
            .position
            .checked_add(align4(len).ok_or(FdtError::PropertyOutOfBounds)?)
            .ok_or(FdtError::PropertyOutOfBounds)?;
        Ok(Property { name, value })
    }
}

// ---------------------------------------------------------------------------
// Nodes and their properties
// ---------------------------------------------------------------------------

/// One node of the tree.
///
/// A node borrows the blob and holds the offset its properties start at, so it
/// is cheap to copy and reading a property is a fresh walk of that node's own
/// property list rather than of the tree.
#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    /// The node's name, unit address included: `pl011@9000000`. The root's
    /// name is empty.
    pub name: &'a str,
    /// Nesting depth, the root being zero.
    pub depth: usize,
    /// `#address-cells` in effect for this node's own `reg`, which is the
    /// value its *parent* declares.
    pub address_cells: u32,
    /// `#size-cells` in effect for this node's own `reg`.
    pub size_cells: u32,
    structs: &'a [u8],
    strings: &'a [u8],
    properties: usize,
}

impl<'a> Node<'a> {
    /// Every property of this node, in blob order.
    #[must_use]
    pub const fn properties(&self) -> Properties<'a> {
        Properties {
            cursor: Cursor {
                structs: self.structs,
                strings: self.strings,
                position: self.properties,
            },
            done: false,
        }
    }

    /// The property called `name`, if the node has one.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<Property<'a>> {
        self.properties().find(|property| property.name == name)
    }

    /// The first entry of the node's `compatible` list, which is the most
    /// specific binding it claims.
    #[must_use]
    pub fn compatible(&self) -> Option<&'a str> {
        self.property("compatible")?.as_str()
    }

    /// True if the node's `compatible` list holds `binding`.
    #[must_use]
    pub fn is_compatible(&self, binding: &str) -> bool {
        self.property("compatible")
            .is_some_and(|property| property.contains_string(binding))
    }

    /// The node's `device_type`, the property `/memory` nodes are found by.
    #[must_use]
    pub fn device_type(&self) -> Option<&'a str> {
        self.property("device_type")?.as_str()
    }

    /// The node's name without its unit address: `pl011` for `pl011@9000000`.
    #[must_use]
    pub fn base_name(&self) -> &'a str {
        base_name(self.name)
    }

    /// The node's unit address as written, without the `@`.
    #[must_use]
    pub fn unit_address(&self) -> Option<&'a str> {
        let mut parts = self.name.split('@');
        let _ = parts.next()?;
        parts.next()
    }

    /// The node's `reg` ranges, decoded with the cell counts its parent
    /// declared. Empty if the node has no `reg`.
    #[must_use]
    pub fn reg(&self) -> Regions<'a> {
        let value = match self.property("reg") {
            Some(property) => property.value,
            None => &[],
        };
        Regions {
            value,
            offset: 0,
            address_cells: self.address_cells,
            size_cells: self.size_cells,
        }
    }

    /// The windows this node forwards, read from `ranges` as a PCI host
    /// bridge's.
    ///
    /// Empty unless the node declares the cell counts PCI requires -- three
    /// address cells and two size cells -- because a `ranges` decoded with
    /// the wrong widths is not a conservative error: it yields windows that
    /// look plausible and are not there.
    #[must_use]
    pub fn pci_windows(&self) -> PciWindows<'a> {
        let declares = |name: &str, want: u32| {
            self.property(name)
                .and_then(|property| property.as_u32())
                .is_some_and(|found| found == want)
        };
        let value = match self.property("ranges") {
            Some(property)
                if declares("#address-cells", PCI_ADDRESS_CELLS)
                    && declares("#size-cells", PCI_SIZE_CELLS) =>
            {
                property.value
            }
            _ => &[],
        };
        PciWindows {
            value,
            offset: 0,
            parent_address_cells: self.address_cells,
        }
    }

    /// Whether the node is in use: its `status` is absent, `okay` or `ok`.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.property("status")
            .is_none_or(|status| matches!(status.as_str(), Some("okay" | "ok")))
    }

    /// The node's `interrupts`, decoded as three-cell GIC specifiers.
    ///
    /// Only meaningful when the node's interrupt parent is a GIC, which the
    /// caller has to establish. Stops at the first specifier that does not
    /// name an interrupt a GIC delivers, and at a trailing partial one:
    /// skipping a bad entry would renumber the ones after it, and a driver
    /// asks for its interrupts by position.
    #[must_use]
    pub fn gic_interrupts(&self) -> GicInterrupts<'a> {
        GicInterrupts {
            cells: self.property("interrupts").map(|property| property.cells()),
        }
    }

    /// The words of the node's `interrupts` property. Their meaning is the
    /// interrupt controller's to define, so they are handed over undecoded.
    #[must_use]
    pub fn interrupts(&self) -> Option<Cells<'a>> {
        Some(self.property("interrupts")?.cells())
    }
}

/// The properties of one node, in blob order.
#[derive(Clone, Copy, Debug)]
pub struct Properties<'a> {
    cursor: Cursor<'a>,
    done: bool,
}

impl<'a> Iterator for Properties<'a> {
    type Item = Property<'a>;

    fn next(&mut self) -> Option<Property<'a>> {
        while !self.done {
            let Ok(token) = self.cursor.token() else {
                self.done = true;
                return None;
            };
            match token {
                FDT_NOP => {}
                FDT_PROP => {
                    let Ok(property) = self.cursor.property() else {
                        self.done = true;
                        return None;
                    };
                    return Some(property);
                }
                // Properties precede subnodes, so anything else ends the list.
                _ => {
                    self.done = true;
                    return None;
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Walking the tree
// ---------------------------------------------------------------------------

/// One open node on the walker's stack.
///
/// The name is kept as an offset rather than a `&str` so that a frame is twelve
/// bytes: sixty-four of them then cost under a kilobyte of the kernel's stack.
#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    name_offset: u32,
    /// `#address-cells` this node declares *for its children*, inherited from
    /// its parent when it declares none.
    address_cells: u32,
    size_cells: u32,
}

/// A depth-first walk of every node in the tree.
///
/// The iterator stops rather than panicking if it meets something malformed;
/// [`Fdt::parse`] has already rejected such a blob, so in practice it stops
/// only at the end of the tree.
#[derive(Clone, Copy, Debug)]
pub struct Nodes<'a> {
    cursor: Cursor<'a>,
    stack: [Frame; MAX_DEPTH],
    depth: usize,
    tokens: usize,
    done: bool,
}

impl<'a> Nodes<'a> {
    /// A walk positioned at the start of the structure block.
    const fn new(cursor: Cursor<'a>) -> Self {
        Nodes {
            cursor,
            stack: [Frame {
                name_offset: 0,
                address_cells: DEFAULT_ADDRESS_CELLS,
                size_cells: DEFAULT_SIZE_CELLS,
            }; MAX_DEPTH],
            depth: 0,
            tokens: 0,
            done: false,
        }
    }

    /// End the walk. Written as a function returning `None` so that the many
    /// failure paths below stay one line each.
    fn stop(&mut self) -> Option<Node<'a>> {
        self.done = true;
        None
    }

    /// Enter the node whose [`FDT_BEGIN_NODE`] was just read, leaving the
    /// cursor on the first token after its properties.
    fn begin_node(&mut self) -> Option<Node<'a>> {
        let index = self.depth;
        if index >= MAX_DEPTH {
            return self.stop();
        }
        let Ok(name_offset) = u32::try_from(self.cursor.position) else {
            return self.stop();
        };
        let Ok(name) = self.cursor.node_name() else {
            return self.stop();
        };
        // A node's own `reg` is decoded with the counts its parent declared;
        // the root falls back to the format's defaults.
        let (address_cells, size_cells) = match index.checked_sub(1).and_then(|p| self.stack.get(p))
        {
            Some(parent) => (parent.address_cells, parent.size_cells),
            None => (DEFAULT_ADDRESS_CELLS, DEFAULT_SIZE_CELLS),
        };
        let properties = self.cursor.position;
        let (child_address_cells, child_size_cells) =
            self.scan_properties(address_cells, size_cells)?;
        let Some(frame) = self.stack.get_mut(index) else {
            return self.stop();
        };
        *frame = Frame {
            name_offset,
            address_cells: child_address_cells,
            size_cells: child_size_cells,
        };
        self.depth = index.saturating_add(1);
        Some(Node {
            name,
            depth: index,
            address_cells,
            size_cells,
            structs: self.cursor.structs,
            strings: self.cursor.strings,
            properties,
        })
    }

    /// Step the cursor over the current node's properties, returning the cell
    /// counts that apply to its children.
    ///
    /// The counts start at the inherited values, which is what the format means
    /// by a node that declares neither.
    fn scan_properties(&mut self, address: u32, size: u32) -> Option<(u32, u32)> {
        let mut cells = (address, size);
        loop {
            let resume = self.cursor.position;
            let Ok(token) = self.cursor.token() else {
                self.done = true;
                return None;
            };
            match token {
                FDT_NOP => {}
                FDT_PROP => {
                    let Ok(property) = self.cursor.property() else {
                        self.done = true;
                        return None;
                    };
                    declared_cells(&property, &mut cells);
                }
                // Leave the token for the main loop: it opens a child node,
                // closes this one, or ends the tree.
                _ => {
                    self.cursor.position = resume;
                    return Some(cells);
                }
            }
        }
    }

    /// True if the node most recently returned sits exactly at `path`.
    ///
    /// A component written without a unit address matches a node that has one,
    /// so `/memory` finds `/memory@40000000`.
    fn path_is(&self, path: &str) -> bool {
        let mut components = path.split('/').filter(|part| !part.is_empty());
        // Level zero is the root, whose empty name answers to no component.
        for level in 1..self.depth {
            let (Some(want), Some(frame)) = (components.next(), self.stack.get(level)) else {
                return false;
            };
            let offset = frame.name_offset as usize;
            let Ok(have) = cstr_at(self.cursor.structs, offset) else {
                return false;
            };
            if have != want && (want.contains('@') || base_name(have) != want) {
                return false;
            }
        }
        components.next().is_none()
    }

    /// The first node at `path`, or `None` if the tree has none.
    fn find_path(mut self, path: &str) -> Option<Node<'a>> {
        loop {
            let node = self.next()?;
            if self.path_is(path) {
                return Some(node);
            }
        }
    }
}

/// Apply a node's `#address-cells` or `#size-cells` to the counts its children
/// will use.
///
/// A declaration wider than [`MAX_CELLS`] is ignored rather than honoured: it
/// cannot describe an address this machine has, and treating it as a length
/// would let a blob steer how much of a `reg` value is read per entry.
fn declared_cells(property: &Property<'_>, cells: &mut (u32, u32)) {
    let Some(value) = property.as_u32().filter(|value| *value <= MAX_CELLS) else {
        return;
    };
    if property.name == "#address-cells" {
        cells.0 = value;
    } else if property.name == "#size-cells" {
        cells.1 = value;
    }
}

impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        while !self.done {
            self.tokens = self.tokens.saturating_add(1);
            if self.tokens > MAX_TOKENS {
                return self.stop();
            }
            let Ok(token) = self.cursor.token() else {
                return self.stop();
            };
            match token {
                FDT_BEGIN_NODE => return self.begin_node(),
                FDT_END_NODE => match self.depth.checked_sub(1) {
                    Some(depth) => self.depth = depth,
                    None => return self.stop(),
                },
                FDT_NOP => {}
                // Only reachable in a tree that puts a property after a
                // subnode; step over it rather than giving up on the rest.
                FDT_PROP => {
                    if self.cursor.property().is_err() {
                        return self.stop();
                    }
                }
                _ => return self.stop(),
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Memory reservations
// ---------------------------------------------------------------------------

/// The entries of the memory reservation block, in order.
#[derive(Clone, Copy, Debug)]
pub struct Reservations<'a> {
    blob: &'a [u8],
    offset: usize,
    done: bool,
}

impl Iterator for Reservations<'_> {
    type Item = MemoryReservation;

    fn next(&mut self) -> Option<MemoryReservation> {
        if self.done {
            return None;
        }
        let (Some(address), Some(size)) = (
            u64_at(self.blob, self.offset),
            u64_at(self.blob, self.offset.checked_add(8)?),
        ) else {
            self.done = true;
            return None;
        };
        // A zero pair terminates the list.
        if address == 0 && size == 0 {
            self.done = true;
            return None;
        }
        match self.offset.checked_add(16) {
            Some(offset) => self.offset = offset,
            None => self.done = true,
        }
        Some(MemoryReservation { address, size })
    }
}

// ---------------------------------------------------------------------------
// Devices the kernel has to find
// ---------------------------------------------------------------------------

/// Which generic interrupt controller a tree describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GicVersion {
    /// A GICv2: the second `reg` range is the CPU interface.
    V2,
    /// A GICv3: the second `reg` range is the redistributor region.
    V3,
}

/// The interrupt controller node, with the version its `compatible` claimed.
#[derive(Clone, Copy, Debug)]
pub struct InterruptController<'a> {
    /// Which architecture version the controller implements.
    pub version: GicVersion,
    /// The node itself, for any property this type does not expose.
    pub node: Node<'a>,
}

impl InterruptController<'_> {
    /// How many cells an interrupt specifier naming this controller takes:
    /// three for every GIC binding, if the tree says so at all.
    #[must_use]
    pub fn interrupt_cells(&self) -> Option<u32> {
        self.node.property("#interrupt-cells")?.as_u32()
    }

    /// The distributor's `reg` range, which is the first one in both versions.
    #[must_use]
    pub fn distributor(&self) -> Option<Region> {
        self.node.reg().next()
    }

    /// The redistributor `reg` range, or `None` on a GICv2, which has none.
    #[must_use]
    pub fn redistributor(&self) -> Option<Region> {
        if self.version != GicVersion::V3 {
            return None;
        }
        self.node.reg().nth(1)
    }

    /// The CPU interface `reg` range, or `None` on a GICv3, where the CPU
    /// interface is a set of system registers rather than a memory range.
    #[must_use]
    pub fn cpu_interface(&self) -> Option<Region> {
        if self.version != GicVersion::V2 {
            return None;
        }
        self.node.reg().nth(1)
    }
}

/// One of the architected timer's four interrupts, in the order a timer node
/// lists them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimerInterrupt {
    /// The secure physical timer, which belongs to the secure world.
    SecurePhysical = 0,
    /// The non-secure physical timer.
    NonSecurePhysical = 1,
    /// The virtual timer: the one a kernel below any hypervisor owns.
    Virtual = 2,
    /// The hypervisor's own physical timer.
    Hypervisor = 3,
}

/// The instruction a PSCI call is made with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PsciConduit {
    /// `smc`: firmware is a secure monitor.
    Smc,
    /// `hvc`: firmware is a hypervisor, or is being emulated as one.
    Hvc,
}

/// The GIC interrupt identifier a three-cell GIC specifier names.
///
/// The binding numbers the two peripheral kinds from zero each: type 0 is a
/// shared peripheral, identifier 32 upwards; type 1 is a private one,
/// identifiers 16 to 31. Any other type, or a number past the end of its
/// range, is not an interrupt a GIC delivers.
#[must_use]
pub const fn gic_interrupt_id(kind: u32, number: u32) -> Option<u32> {
    match kind {
        0 if number < 988 => Some(32 + number),
        1 if number < 16 => Some(16 + number),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Fdt
// ---------------------------------------------------------------------------

/// A borrowed, validated device tree.
///
/// Nothing is copied and nothing is allocated: the type is three sub-slices of
/// the blob and the decoded header.
#[derive(Clone, Copy, Debug)]
pub struct Fdt<'a> {
    blob: &'a [u8],
    header: Header,
    structs: &'a [u8],
    strings: &'a [u8],
}

impl<'a> Fdt<'a> {
    /// Validate `blob` and borrow it.
    ///
    /// The header, the block extents and the entire token stream are checked
    /// here, so a tree that parses is one every accessor below can walk without
    /// trusting a single further byte of it. Bytes past `totalsize` are never
    /// read, which is what lets a caller hand over a whole page.
    pub fn parse(blob: &'a [u8]) -> Result<Self, FdtError> {
        let header = Header::parse(blob)?;
        let total = usize::try_from(header.totalsize).map_err(|_| FdtError::BadTotalSize)?;
        let blob = blob.get(..total).ok_or(FdtError::BadTotalSize)?;
        let structs = block(blob, header.off_dt_struct, header.size_dt_struct)?;
        let strings = block(blob, header.off_dt_strings, header.size_dt_strings)?;
        let fdt = Fdt {
            blob,
            header,
            structs,
            strings,
        };
        fdt.validate_structure()?;
        Ok(fdt)
    }

    /// Walk the token stream once, rejecting anything malformed.
    ///
    /// This is the reason the iterators can be infallible. It is also where a
    /// blob that nests forever, names a property past the end of the strings
    /// block or never ends is turned into an error instead of a fault.
    fn validate_structure(&self) -> Result<(), FdtError> {
        let mut cursor = Cursor::new(self.structs, self.strings);
        let mut depth = 0usize;
        let mut tokens = 0usize;
        loop {
            tokens = tokens.saturating_add(1);
            if tokens > MAX_TOKENS {
                return Err(FdtError::TokenBudget);
            }
            match cursor.token()? {
                FDT_BEGIN_NODE => {
                    let _ = cursor.node_name()?;
                    depth = depth.saturating_add(1);
                    if depth > MAX_DEPTH {
                        return Err(FdtError::DepthOverflow);
                    }
                }
                FDT_END_NODE => depth = depth.checked_sub(1).ok_or(FdtError::UnbalancedNode)?,
                FDT_PROP => {
                    let _ = cursor.property()?;
                }
                FDT_NOP => {}
                FDT_END => {
                    if depth != 0 {
                        return Err(FdtError::UnbalancedNode);
                    }
                    return Ok(());
                }
                other => return Err(FdtError::BadToken(other)),
            }
        }
    }

    /// The decoded header.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// The blob, truncated to the `totalsize` the header declared.
    #[must_use]
    pub const fn blob(&self) -> &'a [u8] {
        self.blob
    }

    /// Physical ID of the CPU the firmware booted on.
    #[must_use]
    pub const fn boot_cpu(&self) -> u32 {
        self.header.boot_cpuid_phys
    }

    /// Every node of the tree, depth first.
    #[must_use]
    pub const fn nodes(&self) -> Nodes<'a> {
        Nodes::new(Cursor::new(self.structs, self.strings))
    }

    /// The node at `path`, for example `/chosen` or `/soc/uart@9000000`.
    ///
    /// A component written without a unit address matches a node that has one,
    /// so `/memory` finds `/memory@40000000`.
    #[must_use]
    pub fn find_node(&self, path: &str) -> Option<Node<'a>> {
        self.nodes().find_path(path)
    }

    /// The root node.
    #[must_use]
    pub fn root(&self) -> Option<Node<'a>> {
        self.nodes().next()
    }

    /// The first node whose `compatible` list holds `binding`.
    #[must_use]
    pub fn find_compatible(&self, binding: &str) -> Option<Node<'a>> {
        self.nodes().find(|node| node.is_compatible(binding))
    }

    /// The memory reservation block: ranges the kernel must keep out of the
    /// frame allocator.
    #[must_use]
    pub fn reservations(&self) -> Reservations<'a> {
        Reservations {
            blob: self.blob,
            offset: self.header.off_mem_rsvmap as usize,
            done: false,
        }
    }

    /// Every `reg` range of every `/memory` node, in tree order.
    #[must_use]
    pub fn memory(&self) -> MemoryRegions<'a> {
        MemoryRegions {
            nodes: self.nodes(),
            current: None,
        }
    }

    /// The root's `model`, the machine's own name for itself.
    #[must_use]
    pub fn model(&self) -> Option<&'a str> {
        self.root()?.property("model")?.as_str()
    }

    /// The kernel command line from `/chosen`.
    #[must_use]
    pub fn bootargs(&self) -> Option<&'a str> {
        self.find_node("/chosen")?.property("bootargs")?.as_str()
    }

    /// The path an entry of `/aliases` stands for.
    #[must_use]
    pub fn alias(&self, name: &str) -> Option<&'a str> {
        self.find_node("/aliases")?.property(name)?.as_str()
    }

    /// The raw `stdout-path` from `/chosen`, suffix and all.
    ///
    /// `linux,stdout-path` is accepted as well: it is the older spelling, and
    /// some firmware still emits only that one.
    #[must_use]
    pub fn stdout_path(&self) -> Option<&'a str> {
        let chosen = self.find_node("/chosen")?;
        chosen
            .property("stdout-path")
            .or_else(|| chosen.property("linux,stdout-path"))?
            .as_str()
    }

    /// The console node `/chosen`'s `stdout-path` names.
    ///
    /// The path may carry a `:115200n8` options suffix, which is stripped, and
    /// may be an alias rather than a path, which is resolved through
    /// `/aliases`. Its `compatible` and first `reg` range are what a driver
    /// needs; both come from the returned node.
    #[must_use]
    pub fn console(&self) -> Option<Node<'a>> {
        let stdout = self.stdout_path()?;
        let target = stdout.split(':').next().unwrap_or(stdout);
        if target.starts_with('/') {
            return self.find_node(target);
        }
        self.find_node(self.alias(target)?)
    }

    /// The generic interrupt controller, whichever version the tree describes.
    #[must_use]
    pub fn interrupt_controller(&self) -> Option<InterruptController<'a>> {
        self.nodes().find_map(|node| {
            if node.is_compatible(GICV3_COMPATIBLE) {
                return Some(InterruptController {
                    version: GicVersion::V3,
                    node,
                });
            }
            if GICV2_COMPATIBLES
                .iter()
                .any(|binding| node.is_compatible(binding))
            {
                return Some(InterruptController {
                    version: GicVersion::V2,
                    node,
                });
            }
            None
        })
    }

    /// The architected timer node, whose `interrupts` carry the four PPIs the
    /// kernel programs. Found by either the 64-bit or the 32-bit binding.
    #[must_use]
    pub fn timer(&self) -> Option<Node<'a>> {
        self.nodes().find(|node| {
            node.is_compatible(TIMER_COMPATIBLE) || node.is_compatible(TIMER_V7_COMPATIBLE)
        })
    }

    /// The GIC identifier one of the architected timer's interrupts arrives on.
    ///
    /// `None` if there is no timer, if it lists fewer interrupts than `which`
    /// needs, or if the entry is not one a GIC could deliver. A timer's
    /// `interrupts` are GIC specifiers of three cells each, in the order
    /// [`TimerInterrupt`] gives — the binding's order, and not the order
    /// anybody would guess.
    #[must_use]
    pub fn timer_interrupt(&self, which: TimerInterrupt) -> Option<u32> {
        let mut cells = self.timer()?.interrupts()?.skip(which as usize * 3);
        let kind = cells.next()?;
        let number = cells.next()?;
        // The third cell is the trigger type and CPU mask. It changes how the
        // line is programmed, not which line it is — but it has to be there,
        // or the specifier was cut short and the two before it are suspect.
        let _flags = cells.next()?;
        gic_interrupt_id(kind, number)
    }

    /// How this machine's PSCI firmware is called, or `None` if the tree
    /// describes none or names a conduit this crate does not know.
    ///
    /// The difference is not cosmetic: `smc` on a machine with no secure
    /// monitor, or `hvc` on one with no hypervisor, is an undefined-instruction
    /// exception rather than a call.
    #[must_use]
    pub fn psci_conduit(&self) -> Option<PsciConduit> {
        let node = self.nodes().find(|node| {
            PSCI_COMPATIBLES
                .iter()
                .any(|binding| node.is_compatible(binding))
        })?;
        match node.property("method")?.as_str()? {
            "smc" => Some(PsciConduit::Smc),
            "hvc" => Some(PsciConduit::Hvc),
            _ => None,
        }
    }

    /// Every enabled node whose `compatible` list holds `binding`, in tree
    /// order.
    #[must_use]
    pub const fn compatible_nodes<'b>(&self, binding: &'b str) -> CompatibleNodes<'a, 'b> {
        CompatibleNodes {
            nodes: self.nodes(),
            binding,
        }
    }

    /// Every `GICv2m` frame: where PCI devices write to raise an SPI.
    #[must_use]
    pub const fn gicv2m_frames(&self) -> MsiFrames<'a> {
        MsiFrames {
            nodes: self.compatible_nodes(GICV2M_FRAME_COMPATIBLE),
        }
    }

    /// The first enabled GICv3 Interrupt Translation Service's registers: its
    /// control frame, with the translation frame devices write to 64 KiB
    /// above it. `None` on a machine whose GIC has none, which then has no
    /// message-signalled interrupts.
    #[must_use]
    pub fn gicv3_its(&self) -> Option<Region> {
        self.compatible_nodes(GICV3_ITS_COMPATIBLE)
            .find_map(|node| node.reg().next())
    }

    /// Every PCI host bridge whose configuration space is an ECAM window,
    /// in tree order. [`EcamHosts`] says what is read and what is skipped.
    #[must_use]
    pub const fn ecam_hosts(&self) -> EcamHosts<'a> {
        EcamHosts {
            nodes: self.nodes(),
        }
    }

    /// Every ECAM host bridge with the windows it forwards, in tree order.
    /// The same bridges [`Fdt::ecam_hosts`] yields.
    #[must_use]
    pub const fn ecam_hosts_with_windows(&self) -> EcamHostsWithWindows<'a> {
        EcamHostsWithWindows {
            nodes: self.nodes(),
        }
    }

    /// Every processor in `/cpus` that has not failed, in tree order.
    ///
    /// [`Cpus`] says which nodes count as processors and why.
    #[must_use]
    pub const fn cpus(&self) -> Cpus<'a> {
        Cpus {
            nodes: self.nodes(),
            inside: false,
            done: false,
        }
    }
}

/// One processor, from a `cpu` node under `/cpus`.
#[derive(Clone, Copy, Debug)]
pub struct Cpu<'a> {
    /// The node, for anything else a caller wants from it.
    pub node: Node<'a>,
    /// The processor's hardware identifier: its first `reg` entry, decoded
    /// with the `#address-cells` `/cpus` declares. On Arm this is the affinity
    /// fields of `MPIDR` — `Aff3` in the upper cell when there are two — and
    /// so the number PSCI's `CPU_ON` takes.
    pub id: u64,
}

impl<'a> Cpu<'a> {
    /// How the processor is started, if the tree says: `psci`, `spin-table`.
    #[must_use]
    pub fn enable_method(&self) -> Option<&'a str> {
        self.node.property("enable-method")?.as_str()
    }

    /// The node's `status`, or `None` when it has none, which means `okay`.
    ///
    /// **For a processor, `disabled` does not mean absent.** The
    /// specification defines it as quiescent — stopped, waiting to be started
    /// through its enable-method — which is the state every secondary is in
    /// before a kernel starts it. So it is reported rather than filtered, and
    /// the caller decides what to do with one.
    #[must_use]
    pub fn status(&self) -> Option<&'a str> {
        self.node.property("status")?.as_str()
    }
}

/// Every processor in `/cpus` whose `status` is not `fail`.
///
/// A processor is a *direct* child of `/cpus` that is named `cpu` or has
/// `device_type = "cpu"` — both spellings, for the reason [`MemoryRegions`]
/// accepts both of its own — and has a `reg` to be addressed by. Its own
/// children, the caches, are not processors; nor are `/cpus`' other children,
/// `cpu-map` and `idle-states`; nor is a node called `cpu` anywhere else in
/// the tree.
///
/// `fail` is the one status skipped: the specification's word for a processor
/// that is broken, as opposed to one that is merely stopped. Linux draws the
/// line in the same place.
#[derive(Clone, Copy, Debug)]
pub struct Cpus<'a> {
    nodes: Nodes<'a>,
    /// Whether the walk has reached `/cpus`.
    inside: bool,
    done: bool,
}

impl<'a> Iterator for Cpus<'a> {
    type Item = Cpu<'a>;

    fn next(&mut self) -> Option<Cpu<'a>> {
        while !self.done {
            let Some(node) = self.nodes.next() else {
                self.done = true;
                return None;
            };
            if !self.inside {
                self.inside = node.depth == 1 && node.base_name() == "cpus";
                continue;
            }
            // The walk is depth first, so the first node back at `/cpus`' own
            // depth is the end of it.
            if node.depth <= 1 {
                self.done = true;
                return None;
            }
            if let Some(cpu) = as_cpu(node) {
                return Some(cpu);
            }
        }
        None
    }
}

/// `node`, a descendant of `/cpus`, as a processor — if it is one.
fn as_cpu(node: Node<'_>) -> Option<Cpu<'_>> {
    let named = node.base_name() == "cpu";
    let typed = node.device_type() == Some("cpu");
    if node.depth != 2 || !(named || typed) {
        return None;
    }
    let failed = node
        .property("status")
        .and_then(|status| status.as_str())
        .is_some_and(|status| status.starts_with("fail"));
    if failed {
        return None;
    }
    let id = node.reg().next()?.address;
    Some(Cpu { node, id })
}

/// Every `reg` range of every `/memory` node.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegions<'a> {
    nodes: Nodes<'a>,
    current: Option<Regions<'a>>,
}

impl<'a> MemoryRegions<'a> {
    /// The next child of the root that describes memory.
    ///
    /// Both spellings are accepted: `device_type = "memory"`, which the
    /// specification requires, and a node simply named `memory`, which is what
    /// a hand-written tree tends to have.
    fn next_memory_node(&mut self) -> Option<Regions<'a>> {
        loop {
            let node = self.nodes.next()?;
            let named = node.base_name() == "memory";
            let typed = node.device_type() == Some("memory");
            if node.depth == 1 && (named || typed) {
                return Some(node.reg());
            }
        }
    }
}

impl Iterator for MemoryRegions<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        loop {
            if let Some(region) = self.current.as_mut().and_then(Iterator::next) {
                return Some(region);
            }
            self.current = Some(self.next_memory_node()?);
        }
    }
}

mod cpu_map;
pub use cpu_map::{CpuMap, CpuMapError};

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// PCI host bridges
// ---------------------------------------------------------------------------

/// The binding for a host bridge whose configuration space is plain ECAM,
/// which is what QEMU's `virt` machine describes.
pub const PCI_HOST_ECAM_COMPATIBLE: &str = "pci-host-ecam-generic";

/// The binding for a host bridge whose configuration space is conventional
/// PCI's memory-mapped CAM: 256 bytes a function, 64 KiB a bus. crosvm's
/// AArch64 machine describes this one.
pub const PCI_HOST_CAM_COMPATIBLE: &str = "pci-host-cam-generic";

/// Bytes of ECAM window one bus occupies.
const ECAM_BYTES_PER_BUS: u64 = 1 << 20;

/// Bytes of CAM window one bus occupies.
const CAM_BYTES_PER_BUS: u64 = 1 << 16;

/// A PCI host bridge whose configuration space is an ECAM window, or a CAM
/// one when [`EcamHost::cam`] says so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EcamHost {
    /// The window: the first region of `reg`. Its address is bus
    /// [`EcamHost::start_bus`]'s, which is *not* the MCFG's convention — an
    /// MCFG allocation's address is bus zero's.
    pub window: Region,
    /// `linux,pci-domain`, or `None` when the tree does not say — which is
    /// not the same as zero, because two hosts that both leave it out are
    /// still two segments.
    pub segment: Option<u16>,
    /// The first bus of `bus-range`, or zero when there is none.
    pub start_bus: u8,
    /// The last bus the window really reaches: `bus-range`'s last, or 255,
    /// cut down to what [`EcamHost::window`] is large enough to hold.
    pub end_bus: u8,
    /// Whether the window is laid out as CAM, a `pci-host-cam-generic`
    /// node, rather than ECAM.
    pub cam: bool,
}

/// Every `pci-host-ecam-generic` and `pci-host-cam-generic` node that can be
/// used, in tree order.
///
/// A node is skipped if its `status` is anything but `okay`, if it has no
/// `reg`, if its window is smaller than one bus, or if `bus-range` or
/// `linux,pci-domain` does not decode to numbers PCI can have. A `bus-range`
/// larger than the window is cut down to the buses the window holds, as
/// Linux does, rather than letting the kernel map configuration space past
/// the end of what firmware said is there.
#[derive(Clone, Copy, Debug)]
pub struct EcamHosts<'a> {
    /// The walk the hosts are found in.
    nodes: Nodes<'a>,
}

impl Iterator for EcamHosts<'_> {
    type Item = EcamHost;

    fn next(&mut self) -> Option<EcamHost> {
        loop {
            let node = self.nodes.next()?;
            if let Some(host) = ecam_host(&node) {
                return Some(host);
            }
        }
    }
}

/// Decode `node` as an ECAM host bridge, if it is a usable one.
fn ecam_host(node: &Node<'_>) -> Option<EcamHost> {
    let cam = node.is_compatible(PCI_HOST_CAM_COMPATIBLE);
    if !cam && !node.is_compatible(PCI_HOST_ECAM_COMPATIBLE) {
        return None;
    }
    if !node.is_enabled() {
        return None;
    }
    let window = node.reg().next()?;
    let (start_bus, last_bus) = match node.property("bus-range") {
        None => (0, u8::MAX),
        Some(range) => {
            if range.len() != 8 {
                return None;
            }
            let mut cells = range.cells();
            let start = u8::try_from(cells.next()?).ok()?;
            let end = u8::try_from(cells.next()?).ok()?;
            (start, end)
        }
    };
    if last_bus < start_bus {
        return None;
    }
    let segment = match node.property("linux,pci-domain") {
        None => None,
        Some(domain) => Some(u16::try_from(domain.as_u32()?).ok()?),
    };
    let per_bus = if cam {
        CAM_BYTES_PER_BUS
    } else {
        ECAM_BYTES_PER_BUS
    };
    let buses = window.size / per_bus;
    let last_held = u64::from(start_bus).checked_add(buses.checked_sub(1)?)?;
    let end_bus = u8::try_from(last_held).map_or(last_bus, |held| held.min(last_bus));
    Some(EcamHost {
        window,
        segment,
        start_bus,
        end_bus,
        cam,
    })
}

/// Which address space a host bridge forwards one of its windows into.
///
/// The two-bit space code of a `ranges` entry's `phys.hi`. Configuration
/// space is not a window and is never yielded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PciSpace {
    /// I/O space. A memory BAR may never be placed here, however much the
    /// addresses happen to line up.
    Io,
    /// 32-bit memory space.
    Memory32,
    /// 64-bit memory space.
    Memory64,
}

/// One window a PCI host bridge forwards, decoded from its `ranges`.
///
/// The two addresses are the point of this type. A BAR holds a *bus* address,
/// and on a machine whose bridge translates, that is not the address the
/// processor uses. So a BAR is matched against [`PciWindow::bus`] and the
/// aperture built from [`PciWindow::cpu`]. QEMU's `virt` translates by zero,
/// which makes the two equal and hides the distinction on every machine this
/// kernel currently boots -- which is exactly why it is kept in the type
/// rather than left to whoever reads the code next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PciWindow {
    /// Which space the window forwards into.
    pub space: PciSpace,
    /// Whether the window is prefetchable. A prefetchable BAR may sit in a
    /// non-prefetchable window; the reverse is not allowed, because the
    /// bridge may then combine or prefetch reads that have side effects.
    pub prefetchable: bool,
    /// First address of the window in PCI bus space: what a BAR holds.
    pub bus: u64,
    /// The address the processor reaches that same byte at.
    pub cpu: u64,
    /// Length of the window in bytes.
    pub size: u64,
}

impl PciWindow {
    /// Whether the whole of `len` bytes at bus address `at` is inside this
    /// window.
    ///
    /// A BAR that runs off the end of a window is not forwarded past it, so a
    /// partly covered BAR is no more trustworthy than an uncovered one.
    #[must_use]
    pub const fn holds(&self, at: u64, len: u64) -> bool {
        let Some(end) = at.checked_add(len) else {
            return false;
        };
        let Some(window_end) = self.bus.checked_add(self.size) else {
            return false;
        };
        len > 0 && at >= self.bus && end <= window_end
    }

    /// Where the processor reaches bus address `at`, if this window holds it.
    #[must_use]
    pub const fn translate(&self, at: u64, len: u64) -> Option<u64> {
        if !self.holds(at, len) {
            return None;
        }
        self.cpu.checked_add(at - self.bus)
    }
}

/// `phys.hi` bits 25:24, the space code.
const PCI_SPACE_SHIFT: u32 = 24;
/// `phys.hi` bit 30: the window is prefetchable.
const PCI_PREFETCHABLE: u32 = 1 << 30;
/// A PCI node addresses its children with three cells, always.
const PCI_ADDRESS_CELLS: u32 = 3;
/// And sizes them with two.
const PCI_SIZE_CELLS: u32 = 2;

/// The windows of one host bridge's `ranges`, in blob order.
///
/// Stops at the first entry that does not decode, rather than skipping it: a
/// malformed entry means the cell counts are not what they were taken to be,
/// and every entry after it would be read at the wrong offset.
#[derive(Clone, Copy, Debug)]
pub struct PciWindows<'a> {
    /// The `ranges` value, or empty when the node has none.
    value: &'a [u8],
    /// How far into it the walk has got.
    offset: usize,
    /// The parent's `#address-cells`: how wide the CPU address is.
    parent_address_cells: u32,
}

impl Iterator for PciWindows<'_> {
    type Item = PciWindow;

    fn next(&mut self) -> Option<PciWindow> {
        loop {
            let mut offset = self.offset;
            let high = u32_at(self.value, offset)?;
            offset = offset.checked_add(4)?;
            // The child address, whose low two cells are the bus address.
            let bus = read_cells(self.value, &mut offset, PCI_ADDRESS_CELLS - 1)?;
            let cpu = read_cells(self.value, &mut offset, self.parent_address_cells)?;
            let size = read_cells(self.value, &mut offset, PCI_SIZE_CELLS)?;
            self.offset = offset;
            let space = match (high >> PCI_SPACE_SHIFT) & 0b11 {
                0b01 => PciSpace::Io,
                0b10 => PciSpace::Memory32,
                0b11 => PciSpace::Memory64,
                // Configuration space: the ECAM window, not a window a BAR
                // can be placed in. Skipped rather than ended on.
                _ => continue,
            };
            if size == 0 {
                continue;
            }
            return Some(PciWindow {
                space,
                prefetchable: high & PCI_PREFETCHABLE != 0,
                bus,
                cpu,
                size,
            });
        }
    }
}

/// A host bridge and the windows it forwards.
#[derive(Clone, Copy, Debug)]
pub struct EcamHostWindows<'a> {
    /// The bridge, as [`EcamHosts`] decodes it.
    pub host: EcamHost,
    /// What it forwards.
    pub windows: PciWindows<'a>,
}

/// Every ECAM host bridge, with its windows, in tree order.
///
/// Separate from [`EcamHosts`] because the windows borrow the blob and the
/// bridge does not: a caller that only wants to find configuration space
/// should keep using the plain one.
#[derive(Clone, Copy, Debug)]
pub struct EcamHostsWithWindows<'a> {
    /// The walk the hosts are found in.
    nodes: Nodes<'a>,
}

impl<'a> Iterator for EcamHostsWithWindows<'a> {
    type Item = EcamHostWindows<'a>;

    fn next(&mut self) -> Option<EcamHostWindows<'a>> {
        loop {
            let node = self.nodes.next()?;
            if let Some(host) = ecam_host(&node) {
                return Some(EcamHostWindows {
                    host,
                    windows: node.pci_windows(),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IOMMUs, and what a PCI requester ID arrives at them as
// ---------------------------------------------------------------------------

/// The binding for an Arm `SMMUv3`.
pub const SMMU_V3_COMPATIBLE: &str = "arm,smmu-v3";

/// An `SMMUv3` the tree describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SmmuV3Node {
    /// Its register block: the first region of `reg`.
    pub region: Region,
    /// The phandle an `iommu-map` names it by, if it has one.
    pub phandle: Option<u32>,
    /// `#iommu-cells`: how many cells name one of its inputs. The binding
    /// requires one, the stream ID.
    pub iommu_cells: Option<u32>,
}

/// Every enabled `SMMUv3` with a `reg`. See [`Fdt::smmu_v3s`].
#[derive(Clone, Copy, Debug)]
pub struct SmmuV3s<'a> {
    /// The nodes.
    nodes: CompatibleNodes<'a, 'static>,
}

impl Iterator for SmmuV3s<'_> {
    type Item = SmmuV3Node;

    fn next(&mut self) -> Option<SmmuV3Node> {
        loop {
            let node = self.nodes.next()?;
            let Some(region) = node.reg().next() else {
                continue;
            };
            return Some(SmmuV3Node {
                region,
                phandle: node.phandle(),
                iommu_cells: node
                    .property("#iommu-cells")
                    .and_then(|cells| cells.as_u32()),
            });
        }
    }
}

/// One entry of an `iommu-map`: `length` requester IDs from `rid_base` arrive
/// at the IOMMU `phandle` names as the IDs from `id_base`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IdMapping {
    /// The first requester ID the entry covers.
    pub rid_base: u32,
    /// The IOMMU they arrive at.
    pub phandle: u32,
    /// The ID the first of them arrives as.
    pub id_base: u32,
    /// How many requester IDs the entry covers.
    pub length: u32,
}

impl IdMapping {
    /// The ID requester ID `rid` arrives as, if this entry covers it.
    #[must_use]
    pub const fn translate(self, rid: u32) -> Option<u32> {
        match rid.checked_sub(self.rid_base) {
            Some(offset) if offset < self.length => self.id_base.checked_add(offset),
            _ => None,
        }
    }
}

/// A PCI host's `iommu-map`, with its `iommu-map-mask`.
///
/// Only one-cell IOMMU specifiers are decoded, which is what an `SMMUv3`
/// takes, so an entry is four cells; a map whose length is not a whole number
/// of entries has none, rather than a last entry made of whatever follows.
#[derive(Clone, Copy, Debug)]
pub struct IdMap<'a> {
    /// The `iommu-map` property.
    map: Property<'a>,
    /// What a requester ID is masked with before it is matched: all ones when
    /// the tree gives no mask.
    mask: u32,
}

impl<'a> IdMap<'a> {
    /// Its entries, in the tree's order.
    #[must_use]
    pub const fn entries(&self) -> IdMappings<'a> {
        IdMappings {
            cells: self.map.cells(),
            whole: self.map.len().is_multiple_of(16),
        }
    }

    /// The IOMMU requester ID `rid` arrives at and the ID it arrives as, as
    /// `(phandle, id)`: the first entry covering the masked ID wins, as in
    /// Linux's `of_map_id`. `None` when no entry covers it.
    #[must_use]
    pub fn translate(&self, rid: u32) -> Option<(u32, u32)> {
        let masked = rid & self.mask;
        self.entries()
            .find_map(|entry| Some((entry.phandle, entry.translate(masked)?)))
    }
}

/// The entries of an [`IdMap`].
#[derive(Clone, Copy, Debug)]
pub struct IdMappings<'a> {
    /// The map's cells.
    cells: Cells<'a>,
    /// Whether the map is a whole number of entries.
    whole: bool,
}

impl Iterator for IdMappings<'_> {
    type Item = IdMapping;

    fn next(&mut self) -> Option<IdMapping> {
        if !self.whole {
            return None;
        }
        Some(IdMapping {
            rid_base: self.cells.next()?,
            phandle: self.cells.next()?,
            id_base: self.cells.next()?,
            length: self.cells.next()?,
        })
    }
}

impl Node<'_> {
    /// The node's `phandle`, or its older spelling `linux,phandle`.
    #[must_use]
    pub fn phandle(&self) -> Option<u32> {
        self.property("phandle")
            .or_else(|| self.property("linux,phandle"))?
            .as_u32()
    }
}

/// `compatible` string of ST's extended interrupt and event controller on an
/// STM32MP15, the interrupt parent its device tree gives each UART.
pub const STM32MP1_EXTI_COMPATIBLE: &str = "st,stm32mp1-exti";

/// Whether `node` is a GIC of either version this crate knows.
fn is_gic(node: &Node<'_>) -> bool {
    node.is_compatible(GICV3_COMPATIBLE)
        || GICV2_COMPATIBLES
            .iter()
            .any(|binding| node.is_compatible(binding))
}

/// One entry of an `interrupts-extended` property.
struct ExtendedEntry<'a> {
    /// The controller the entry names.
    controller: Node<'a>,
    /// Its specifier, `width` cells of it, the rest zero.
    specifier: [u32; 3],
    /// How many cells the controller declares, at most three.
    width: u32,
}

impl<'a> Fdt<'a> {
    /// Every enabled `SMMUv3`, in tree order.
    #[must_use]
    pub const fn smmu_v3s(&self) -> SmmuV3s<'a> {
        SmmuV3s {
            nodes: self.compatible_nodes(SMMU_V3_COMPATIBLE),
        }
    }

    /// The node whose phandle is `phandle`.
    #[must_use]
    pub fn node_by_phandle(&self, phandle: u32) -> Option<Node<'a>> {
        self.nodes().find(|node| node.phandle() == Some(phandle))
    }

    /// The GIC interrupt a device's `index`th interrupt reaches, however the
    /// tree routes it there.
    ///
    /// Three shapes are followed:
    ///
    /// * `interrupts`, when the tree's interrupt controller is a GIC taking
    ///   three cells and the node names no other `interrupt-parent` — the shape
    ///   QEMU's `virt` writes;
    /// * `interrupts-extended`, whose every entry is a phandle and as many cells
    ///   as the controller it names declares, when that controller is a GIC;
    /// * one level through ST's EXTI ([`STM32MP1_EXTI_COMPATIBLE`]), which is
    ///   how an STM32MP15 tree routes its UARTs. The EXTI line in the first
    ///   cell names an entry of the EXTI node's own `interrupts-extended`, and
    ///   that entry is the GIC interrupt the peripheral raises. On a DK1, UART4
    ///   is EXTI line 30, which is GIC SPI 52.
    ///
    /// `None` for any other shape — an `interrupt-map`, a parent inherited from
    /// an ancestor other than the GIC, an EXTI line with no GIC interrupt
    /// behind it — and for an entry that runs off the end of its property.
    #[must_use]
    pub fn gic_interrupt_of(&self, node: &Node<'a>, index: usize) -> Option<GicInterrupt> {
        if node.property("interrupts").is_some() {
            if let Some(parent) = node.property("interrupt-parent").and_then(|p| p.as_u32()) {
                let parent = self.node_by_phandle(parent)?;
                if !is_gic(&parent) {
                    return None;
                }
            }
            let gic = self.interrupt_controller()?;
            if gic.interrupt_cells() != Some(3) {
                return None;
            }
            return node.gic_interrupts().nth(index);
        }
        let entry = self.interrupts_extended_entry(node, index)?;
        self.gic_behind(&entry, true)
    }

    /// Entry `index` of `node`'s `interrupts-extended`: the controller it names
    /// and the cells after the phandle.
    ///
    /// Each entry's width is the named controller's `#interrupt-cells`, so the
    /// walk looks up every controller it passes. A zero phandle is a
    /// placeholder of no cells, which the EXTI binding uses for lines with no
    /// parent interrupt; landing on one is `None`.
    fn interrupts_extended_entry(
        &self,
        node: &Node<'a>,
        index: usize,
    ) -> Option<ExtendedEntry<'a>> {
        let mut cells = node.property("interrupts-extended")?.cells();
        let mut position = 0usize;
        loop {
            let phandle = cells.next()?;
            let controller = if phandle == 0 {
                None
            } else {
                Some(self.node_by_phandle(phandle)?)
            };
            let width = match &controller {
                Some(controller) => controller.property("#interrupt-cells")?.as_u32()?,
                None => 0,
            };
            let mut specifier = [0_u32; 3];
            let taken = specifier.get_mut(..usize::try_from(width).ok()?)?;
            for cell in taken {
                *cell = cells.next()?;
            }
            if position == index {
                return Some(ExtendedEntry {
                    controller: controller?,
                    specifier,
                    width,
                });
            }
            position = position.checked_add(1)?;
        }
    }

    /// The GIC interrupt an `interrupts-extended` entry comes to: the entry
    /// itself when it names a GIC, or, when `through_exti` allows, the GIC
    /// interrupt behind the EXTI line it names.
    fn gic_behind(&self, entry: &ExtendedEntry<'a>, through_exti: bool) -> Option<GicInterrupt> {
        let [first, second, third] = entry.specifier;
        if is_gic(&entry.controller) {
            return if entry.width == 3 {
                gic_interrupt(first, second, third)
            } else {
                None
            };
        }
        if through_exti
            && entry.width == 2
            && entry.controller.is_compatible(STM32MP1_EXTI_COMPATIBLE)
        {
            let line = usize::try_from(first).ok()?;
            let behind = self.interrupts_extended_entry(&entry.controller, line)?;
            return self.gic_behind(&behind, false);
        }
        None
    }

    /// The `iommu-map` of the ECAM host `host` was decoded from.
    ///
    /// `None` when the host has none — which the binding says means its
    /// devices are not behind an IOMMU — or when its `iommu-map-mask` is not
    /// one cell, since a map matched without its mask sends a requester ID to
    /// the wrong stream.
    #[must_use]
    pub fn ecam_iommu_map(&self, host: &EcamHost) -> Option<IdMap<'a>> {
        let node = self
            .nodes()
            .find(|node| ecam_host(node).as_ref() == Some(host))?;
        let map = node.property("iommu-map")?;
        let mask = match node.property("iommu-map-mask") {
            None => u32::MAX,
            Some(mask) => mask.as_u32()?,
        };
        Some(IdMap { map, mask })
    }

    /// The GIC interrupt a function's legacy `INTx` `pin` (1 for `INTA` to 4
    /// for `INTD`) reaches through `host`'s `interrupt-map`.
    ///
    /// The function is matched by its unit address, `bus << 16 | device <<
    /// 11 | function << 8` in the first cell, and the pin, each masked by
    /// `interrupt-map-mask` as the binding says. Only a map whose parent is
    /// a GIC taking three cells is followed: that is what crosvm writes, a
    /// line per slot on bus zero. A function behind a bridge is not
    /// swizzled here, so a caller should ask only for functions on the
    /// host's root bus. `None` when the host has no map, the pin is zero,
    /// or no entry matches.
    #[must_use]
    pub fn ecam_intx(
        &self,
        host: &EcamHost,
        bus: u8,
        device: u8,
        function: u8,
        pin: u8,
    ) -> Option<GicInterrupt> {
        if pin == 0 {
            return None;
        }
        let node = self
            .nodes()
            .find(|node| ecam_host(node).as_ref() == Some(host))?;
        let address_cells = node.property("#address-cells")?.as_u32()?;
        let interrupt_cells = node.property("#interrupt-cells")?.as_u32()?;
        // A PCI unit address is three cells and a pin one: nothing else is
        // a PCI host's map.
        if address_cells != 3 || interrupt_cells != 1 {
            return None;
        }
        let unit = (u32::from(bus) << 16) | (u32::from(device) << 11) | (u32::from(function) << 8);
        let wanted = [unit, 0, 0, u32::from(pin)];
        let mut mask = [u32::MAX; 4];
        if let Some(given) = node.property("interrupt-map-mask") {
            let mut cells = given.cells();
            for cell in &mut mask {
                *cell = cells.next()?;
            }
        }
        let mut cells = node.property("interrupt-map")?.cells();
        loop {
            let mut child = [0_u32; 4];
            for cell in &mut child {
                *cell = cells.next()?;
            }
            let parent = self.node_by_phandle(cells.next()?)?;
            let parent_address = match parent.property("#address-cells") {
                None => 0,
                Some(cells) => cells.as_u32()?,
            };
            for _ in 0..parent_address {
                let _ = cells.next()?;
            }
            let width = parent.property("#interrupt-cells")?.as_u32()?;
            if !is_gic(&parent) || width != 3 {
                return None;
            }
            let (kind, number, flags) = (cells.next()?, cells.next()?, cells.next()?);
            let matches = child
                .iter()
                .zip(wanted.iter())
                .zip(mask.iter())
                .all(|((have, want), mask)| have & mask == want & mask);
            if matches {
                return gic_interrupt(kind, number, flags);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Devices by binding, and their interrupts
// ---------------------------------------------------------------------------

/// The binding for a virtio device on its memory-mapped transport.
pub const VIRTIO_MMIO_COMPATIBLE: &str = "virtio,mmio";

/// Every enabled node claiming one binding. See [`Fdt::compatible_nodes`].
#[derive(Clone, Copy, Debug)]
pub struct CompatibleNodes<'a, 'b> {
    /// The walk the nodes are found in.
    nodes: Nodes<'a>,
    /// The binding asked for.
    binding: &'b str,
}

impl<'a> Iterator for CompatibleNodes<'a, '_> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        let binding = self.binding;
        self.nodes
            .find(|node| node.is_compatible(binding) && node.is_enabled())
    }
}

/// How an interrupt line signals, from a GIC specifier's third cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger {
    /// On a rising edge.
    EdgeRising,
    /// On a falling edge.
    EdgeFalling,
    /// While the line is high.
    LevelHigh,
    /// While the line is low.
    LevelLow,
}

/// One interrupt a GIC specifier names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GicInterrupt {
    /// The GIC interrupt identifier, as [`gic_interrupt_id`] computes it.
    pub id: u32,
    /// How the line signals, or `None` when the flags name no single sense —
    /// zero, which the binding leaves to the controller's default, or more
    /// than one bit.
    pub trigger: Option<Trigger>,
}

/// Decode one three-cell GIC specifier: kind, number, flags.
#[must_use]
pub const fn gic_interrupt(kind: u32, number: u32, flags: u32) -> Option<GicInterrupt> {
    let Some(id) = gic_interrupt_id(kind, number) else {
        return None;
    };
    let trigger = match flags & 0xF {
        1 => Some(Trigger::EdgeRising),
        2 => Some(Trigger::EdgeFalling),
        4 => Some(Trigger::LevelHigh),
        8 => Some(Trigger::LevelLow),
        _ => None,
    };
    Some(GicInterrupt { id, trigger })
}

/// A node's `interrupts`, three cells at a time. See [`Node::gic_interrupts`].
#[derive(Clone, Copy, Debug)]
pub struct GicInterrupts<'a> {
    /// What is left of the property, or `None` once finished.
    cells: Option<Cells<'a>>,
}

impl Iterator for GicInterrupts<'_> {
    type Item = GicInterrupt;

    fn next(&mut self) -> Option<GicInterrupt> {
        let cells = self.cells.as_mut()?;
        let decoded = match (cells.next(), cells.next(), cells.next()) {
            (Some(kind), Some(number), Some(flags)) => gic_interrupt(kind, number, flags),
            _ => None,
        };
        if decoded.is_none() {
            self.cells = None;
        }
        decoded
    }
}

// ---------------------------------------------------------------------------
// GICv2m frames
// ---------------------------------------------------------------------------

/// The binding for a `GICv2m` frame.
pub const GICV2M_FRAME_COMPATIBLE: &str = "arm,gic-v2m-frame";

/// A `GICv2m` frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MsiFrame {
    /// The frame's registers.
    pub region: Region,
    /// `arm,msi-base-spi`, if the tree overrides the frame's own register.
    pub spi_base: Option<u32>,
    /// `arm,msi-num-spis`, likewise.
    pub spi_count: Option<u32>,
}

/// Every enabled `GICv2m` frame with a `reg`. See [`Fdt::gicv2m_frames`].
#[derive(Clone, Copy, Debug)]
pub struct MsiFrames<'a> {
    /// The frames' nodes.
    nodes: CompatibleNodes<'a, 'static>,
}

impl Iterator for MsiFrames<'_> {
    type Item = MsiFrame;

    fn next(&mut self) -> Option<MsiFrame> {
        loop {
            let node = self.nodes.next()?;
            let Some(region) = node.reg().next() else {
                continue;
            };
            let cell = |name| node.property(name).and_then(|property| property.as_u32());
            return Some(MsiFrame {
                region,
                spi_base: cell("arm,msi-base-spi"),
                spi_count: cell("arm,msi-num-spis"),
            });
        }
    }
}
