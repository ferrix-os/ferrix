//! Where every field of the ring VMO lives, and entries read and written
//! through a [`RingMemory`].
//!
//! Every multi-byte field is little-endian on every architecture, and nothing
//! is padded. The offsets below are the specification; the tests assert each
//! one against the bytes a side actually wrote.

use core::fmt;

use crate::RingMemory;

/// The ring header's magic number.
pub const MAGIC: [u8; 4] = *b"FXBR";

/// The one ring version this crate speaks. v1 refuses every other.
pub const VERSION: u16 = 1;

/// The fewest entries a ring may have.
pub const MIN_ENTRIES: u32 = 2;

/// The most entries a ring may have.
pub const MAX_ENTRIES: u32 = 4096;

/// Bytes of the ring header. Neither array may overlap it.
pub const HEADER_BYTES: usize = 64;

/// Bytes of one submission entry.
pub const SUBMISSION_BYTES: usize = 32;

/// Bytes of one completion entry.
pub const COMPLETION_BYTES: usize = 24;

/// Header flag bits v1 knows. None: any set bit is refused.
pub const KNOWN_HEADER_FLAGS: u16 = 0;

/// Submission flag bit 0: force unit access. Only on a write, and only if HELLO
/// announced [`crate::DeviceFlags::FUA`].
pub const FLAG_FUA: u8 = 1;

/// Byte offsets of the ring header's fields.
pub mod header {
    /// `magic`, 4 bytes, [`super::MAGIC`]. Driver, before HELLO.
    pub const MAGIC: usize = 0;
    /// `version`, `u16`. Driver, before HELLO.
    pub const VERSION: usize = 4;
    /// `flags`, `u16`, zero in v1. Driver, before HELLO.
    pub const FLAGS: usize = 6;
    /// `entries`, `u32`. Driver, before HELLO.
    pub const ENTRIES: usize = 8;
    /// `sub_offset`, `u32`: where the submission array starts. Driver, before
    /// HELLO.
    pub const SUB_OFFSET: usize = 12;
    /// `comp_offset`, `u32`: where the completion array starts. Driver, before
    /// HELLO.
    pub const COMP_OFFSET: usize = 16;
    /// `sub_tail`, `u32`: the next submission slot to fill. Written by the
    /// kernel.
    pub const SUB_TAIL: usize = 20;
    /// `sub_head`, `u32`: the next submission to consume. Written by the
    /// driver.
    pub const SUB_HEAD: usize = 24;
    /// `comp_tail`, `u32`: the next completion slot to fill. Written by the
    /// driver.
    pub const COMP_TAIL: usize = 28;
    /// `comp_head`, `u32`: the next completion to consume. Written by the
    /// kernel.
    pub const COMP_HEAD: usize = 32;
    /// `sub_want_bell`, `u32`: 1 asks the kernel to ring after publishing.
    /// Written by the driver.
    pub const SUB_WANT_BELL: usize = 36;
    /// `comp_want_bell`, `u32`: 1 asks the driver to ring after publishing.
    /// Written by the kernel.
    pub const COMP_WANT_BELL: usize = 40;
    /// The reserved bytes, zero, up to the end of the header.
    pub const RESERVED: usize = 44;
    /// How many reserved bytes there are.
    pub const RESERVED_BYTES: usize = 20;
}

/// Byte offsets of a submission entry's fields.
pub mod submission {
    /// `id`, `u64`: the kernel's choice, unique among outstanding submissions.
    pub const ID: usize = 0;
    /// `sector`, `u64`: the first logical sector.
    pub const SECTOR: usize = 8;
    /// `data_offset`, `u64`: where the payload starts in the data VMO.
    pub const DATA_OFFSET: usize = 16;
    /// `count`, `u32`: sectors; zero for a flush.
    pub const COUNT: usize = 24;
    /// `op`, `u8`: [`super::Op`].
    pub const OP: usize = 28;
    /// `flags`, `u8`: [`super::FLAG_FUA`].
    pub const FLAGS: usize = 29;
    /// `reserved`, `u16`, zero.
    pub const RESERVED: usize = 30;
}

/// Byte offsets of a completion entry's fields.
pub mod completion {
    /// `id`, `u64`: the submission this completes.
    pub const ID: usize = 0;
    /// `bytes_done`, `u64`: at most the submission's payload length.
    pub const BYTES_DONE: usize = 8;
    /// `status`, `u32`: [`super::Status`].
    pub const STATUS: usize = 16;
    /// `device_ticks`, `u32`: how long the device took over the request, in
    /// the ticks of the processor's free-running counter as the driver read
    /// it; zero when the driver did not measure. Informational: the kernel
    /// checks nothing of it and acts on nothing in it.
    pub const DEVICE_TICKS: usize = 20;
}

/// What a submission asks the device to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Op {
    /// Read `count` sectors into the payload region.
    Read = 1,
    /// Write `count` sectors from the payload region.
    Write = 2,
    /// Make everything written so far durable. No range and no payload.
    Flush = 3,
}

impl Op {
    /// The op a raw `op` byte names, if it names one.
    #[must_use]
    pub const fn from_raw(raw: u8) -> Option<Op> {
        match raw {
            1 => Some(Op::Read),
            2 => Some(Op::Write),
            3 => Some(Op::Flush),
            _ => None,
        }
    }

    /// The byte this op is written as.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self as u8
    }
}

/// How a request ended, as a completion reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Status {
    /// Done.
    Ok = 0,
    /// The device failed it.
    IoError = 1,
    /// The device does not do this, such as a flush on a device without a
    /// write cache.
    Unsupported = 2,
    /// The driver's validation refused it; the device never saw it.
    Refused = 3,
    /// A write to a read-only device.
    ReadOnly = 4,
}

impl Status {
    /// The status a raw `status` word names, if it names one.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Status> {
        match raw {
            0 => Some(Status::Ok),
            1 => Some(Status::IoError),
            2 => Some(Status::Unsupported),
            3 => Some(Status::Refused),
            4 => Some(Status::ReadOnly),
            _ => None,
        }
    }

    /// The word this status is written as.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self as u32
    }
}

/// One request, as the kernel means it and as the driver accepts it.
///
/// The payload is `count × block_size` bytes at `data_offset` in the data VMO;
/// [`crate::Device::payload_len`] computes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Submission {
    /// The kernel's name for it: `src/lib/fs/common/block`'s dispatch token, raw.
    pub id: u64,
    /// What to do.
    pub op: Op,
    /// The first logical sector. Zero for a flush.
    pub sector: u64,
    /// Sectors. Zero for a flush, and only for a flush.
    pub count: u32,
    /// Where the payload starts in the data VMO.
    pub data_offset: u64,
    /// Force unit access. Only on a write.
    pub fua: bool,
}

impl Submission {
    /// A read of `count` sectors from `sector` into the region at
    /// `data_offset`.
    #[must_use]
    pub const fn read(id: u64, sector: u64, count: u32, data_offset: u64) -> Self {
        Self::ranged(id, Op::Read, sector, count, data_offset)
    }

    /// A write of `count` sectors at `sector` from the region at
    /// `data_offset`.
    #[must_use]
    pub const fn write(id: u64, sector: u64, count: u32, data_offset: u64) -> Self {
        Self::ranged(id, Op::Write, sector, count, data_offset)
    }

    /// A flush.
    #[must_use]
    pub const fn flush(id: u64) -> Self {
        Self::ranged(id, Op::Flush, 0, 0, 0)
    }

    /// The same request with force unit access set.
    #[must_use]
    pub const fn with_fua(mut self) -> Self {
        self.fua = true;
        self
    }

    const fn ranged(id: u64, op: Op, sector: u64, count: u32, data_offset: u64) -> Self {
        Self {
            id,
            op,
            sector,
            count,
            data_offset,
            fua: false,
        }
    }

    /// The entry this request is written as.
    #[must_use]
    pub const fn raw(&self) -> RawSubmission {
        RawSubmission {
            id: self.id,
            sector: self.sector,
            data_offset: self.data_offset,
            count: self.count,
            op: self.op.raw(),
            flags: if self.fua { FLAG_FUA } else { 0 },
            reserved: 0,
        }
    }
}

/// A submission entry's fields exactly as they were read, nothing checked.
///
/// [`crate::geometry::check_submission`] turns one into a [`Submission`] or
/// says why not.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RawSubmission {
    /// `id`.
    pub id: u64,
    /// `sector`.
    pub sector: u64,
    /// `data_offset`.
    pub data_offset: u64,
    /// `count`.
    pub count: u32,
    /// `op`.
    pub op: u8,
    /// `flags`.
    pub flags: u8,
    /// `reserved`.
    pub reserved: u16,
}

impl RawSubmission {
    /// Copy the entry at byte offset `at` out of the ring, each field once.
    pub fn read_from<M: RingMemory>(memory: &M, at: usize) -> Self {
        Self {
            id: memory.read_u64(at.wrapping_add(submission::ID)),
            sector: memory.read_u64(at.wrapping_add(submission::SECTOR)),
            data_offset: memory.read_u64(at.wrapping_add(submission::DATA_OFFSET)),
            count: memory.read_u32(at.wrapping_add(submission::COUNT)),
            op: memory.read_u8(at.wrapping_add(submission::OP)),
            flags: memory.read_u8(at.wrapping_add(submission::FLAGS)),
            reserved: memory.read_u16(at.wrapping_add(submission::RESERVED)),
        }
    }

    /// Write the entry at byte offset `at`.
    pub fn write_to<M: RingMemory>(&self, memory: &mut M, at: usize) {
        memory.write_u64(at.wrapping_add(submission::ID), self.id);
        memory.write_u64(at.wrapping_add(submission::SECTOR), self.sector);
        memory.write_u64(at.wrapping_add(submission::DATA_OFFSET), self.data_offset);
        memory.write_u32(at.wrapping_add(submission::COUNT), self.count);
        memory.write_u8(at.wrapping_add(submission::OP), self.op);
        memory.write_u8(at.wrapping_add(submission::FLAGS), self.flags);
        memory.write_u16(at.wrapping_add(submission::RESERVED), self.reserved);
    }
}

/// A completion entry's fields exactly as they were read, nothing checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RawCompletion {
    /// `id`.
    pub id: u64,
    /// `bytes_done`.
    pub bytes_done: u64,
    /// `status`.
    pub status: u32,
    /// `device_ticks`.
    pub device_ticks: u32,
}

impl RawCompletion {
    /// Copy the entry at byte offset `at` out of the ring, each field once.
    pub fn read_from<M: RingMemory>(memory: &M, at: usize) -> Self {
        Self {
            id: memory.read_u64(at.wrapping_add(completion::ID)),
            bytes_done: memory.read_u64(at.wrapping_add(completion::BYTES_DONE)),
            status: memory.read_u32(at.wrapping_add(completion::STATUS)),
            device_ticks: memory.read_u32(at.wrapping_add(completion::DEVICE_TICKS)),
        }
    }

    /// Write the entry at byte offset `at`.
    pub fn write_to<M: RingMemory>(&self, memory: &mut M, at: usize) {
        memory.write_u64(at.wrapping_add(completion::ID), self.id);
        memory.write_u64(at.wrapping_add(completion::BYTES_DONE), self.bytes_done);
        memory.write_u32(at.wrapping_add(completion::STATUS), self.status);
        memory.write_u32(at.wrapping_add(completion::DEVICE_TICKS), self.device_ticks);
    }
}

/// Why a ring header, or a layout for one, was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderError {
    /// `magic` is not [`MAGIC`].
    BadMagic,
    /// `version` is not [`VERSION`].
    BadVersion,
    /// `flags` has a bit v1 does not know.
    UnknownFlags,
    /// `entries` is not a power of two in [`MIN_ENTRIES`]`..=`[`MAX_ENTRIES`].
    BadEntries,
    /// An array starts inside the header.
    OverlapsHeader,
    /// An array ends past the ring VMO, or past what this address space can
    /// name.
    OutsideRing,
    /// The two arrays overlap.
    ArraysOverlap,
    /// The reserved bytes are not zero.
    ReservedNotZero,
    /// A driver-written index or want-bell flag is not zero at setup.
    NotFresh,
}

impl fmt::Display for HeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            HeaderError::BadMagic => "the magic number is wrong",
            HeaderError::BadVersion => "the ring version is not 1",
            HeaderError::UnknownFlags => "the header has unknown flags",
            HeaderError::BadEntries => "entries is not a power of two in 2..=4096",
            HeaderError::OverlapsHeader => "an array starts inside the header",
            HeaderError::OutsideRing => "an array ends past the ring",
            HeaderError::ArraysOverlap => "the arrays overlap",
            HeaderError::ReservedNotZero => "the reserved bytes are not zero",
            HeaderError::NotFresh => "an index or want-bell flag is not zero at setup",
        })
    }
}

/// Where the two arrays of a ring are, checked against the ring's size.
///
/// A value of this type is a promise that both arrays lie inside a ring of the
/// size it was checked against, overlap neither each other nor the header, and
/// end at an offset this address space can name — which is what lets every
/// entry offset below be computed without a check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RingLayout {
    entries: u32,
    sub_offset: u32,
    comp_offset: u32,
}

impl RingLayout {
    /// The layout a driver normally chooses: the header, then the submission
    /// array, then the completion array, with no gaps.
    ///
    /// # Errors
    ///
    /// [`HeaderError::BadEntries`] for an `entries` out of range.
    pub fn standard(entries: u32) -> Result<Self, HeaderError> {
        if !entries_in_range(entries) {
            return Err(HeaderError::BadEntries);
        }
        let sub_offset = HEADER_BYTES as u32;
        // At most 64 + 4096 × 32, far below `u32::MAX`.
        let comp_offset = sub_offset + entries * SUBMISSION_BYTES as u32;
        let ends = u64::from(comp_offset) + u64::from(entries) * COMPLETION_BYTES as u64;
        Self::new(entries, sub_offset, comp_offset, ends)
    }

    /// Check a layout against a ring VMO of `ring_bytes` bytes.
    ///
    /// # Errors
    ///
    /// The [`HeaderError`] naming the first rule the layout breaks.
    pub fn new(
        entries: u32,
        sub_offset: u32,
        comp_offset: u32,
        ring_bytes: u64,
    ) -> Result<Self, HeaderError> {
        if !entries_in_range(entries) {
            return Err(HeaderError::BadEntries);
        }
        let header = HEADER_BYTES as u64;
        let (sub_start, comp_start) = (u64::from(sub_offset), u64::from(comp_offset));
        if sub_start < header || comp_start < header {
            return Err(HeaderError::OverlapsHeader);
        }
        // Offsets are `u32` and entries at most 4096, so neither sum can
        // overflow a `u64`.
        let sub_end = sub_start + u64::from(entries) * SUBMISSION_BYTES as u64;
        let comp_end = comp_start + u64::from(entries) * COMPLETION_BYTES as u64;
        let end = sub_end.max(comp_end);
        if end > ring_bytes || usize::try_from(end).is_err() {
            return Err(HeaderError::OutsideRing);
        }
        if sub_start < comp_end && comp_start < sub_end {
            return Err(HeaderError::ArraysOverlap);
        }
        Ok(Self {
            entries,
            sub_offset,
            comp_offset,
        })
    }

    /// Entries in each ring.
    #[must_use]
    pub const fn entries(&self) -> u32 {
        self.entries
    }

    /// Where the submission array starts.
    #[must_use]
    pub const fn sub_offset(&self) -> u32 {
        self.sub_offset
    }

    /// Where the completion array starts.
    #[must_use]
    pub const fn comp_offset(&self) -> u32 {
        self.comp_offset
    }

    /// The smallest ring VMO this layout fits in.
    #[must_use]
    pub const fn ring_bytes(&self) -> u64 {
        let sub_end = self.sub_offset as u64 + self.entries as u64 * SUBMISSION_BYTES as u64;
        let comp_end = self.comp_offset as u64 + self.entries as u64 * COMPLETION_BYTES as u64;
        if sub_end > comp_end {
            sub_end
        } else {
            comp_end
        }
    }

    /// The byte offset of the submission slot free-running `index` maps to.
    #[must_use]
    pub const fn submission_at(&self, index: u32) -> usize {
        let slot = (index & (self.entries - 1)) as usize;
        // In range by construction: see the type's documentation.
        (self.sub_offset as usize).wrapping_add(slot.wrapping_mul(SUBMISSION_BYTES))
    }

    /// The byte offset of the completion slot free-running `index` maps to.
    #[must_use]
    pub const fn completion_at(&self, index: u32) -> usize {
        let slot = (index & (self.entries - 1)) as usize;
        (self.comp_offset as usize).wrapping_add(slot.wrapping_mul(COMPLETION_BYTES))
    }
}

const fn entries_in_range(entries: u32) -> bool {
    entries >= MIN_ENTRIES && entries <= MAX_ENTRIES && entries.is_power_of_two()
}

/// The header fields the driver writes before HELLO, as the kernel copies them
/// out — once, and nothing else.
///
/// The kernel-written fields are deliberately absent. The kernel has no use
/// for their old values, and not reading them is how it keeps the rule that a
/// side never reads its own fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DriverHeader {
    /// `magic`.
    pub magic: [u8; 4],
    /// `version`.
    pub version: u16,
    /// `flags`.
    pub flags: u16,
    /// `entries`.
    pub entries: u32,
    /// `sub_offset`.
    pub sub_offset: u32,
    /// `comp_offset`.
    pub comp_offset: u32,
    /// `sub_head`, which must start at zero.
    pub sub_head: u32,
    /// `comp_tail`, which must start at zero.
    pub comp_tail: u32,
    /// `sub_want_bell`, which must start at zero.
    pub sub_want_bell: u32,
    /// The reserved bytes, which must be zero.
    pub reserved: [u8; header::RESERVED_BYTES],
}

impl DriverHeader {
    /// Copy the driver-written header fields out of the ring.
    pub fn read_from<M: RingMemory>(memory: &M) -> Self {
        let mut magic = [0; 4];
        for (at, byte) in magic.iter_mut().enumerate() {
            *byte = memory.read_u8(header::MAGIC.wrapping_add(at));
        }
        let mut reserved = [0; header::RESERVED_BYTES];
        for (at, byte) in reserved.iter_mut().enumerate() {
            *byte = memory.read_u8(header::RESERVED.wrapping_add(at));
        }
        Self {
            magic,
            version: memory.read_u16(header::VERSION),
            flags: memory.read_u16(header::FLAGS),
            entries: memory.read_u32(header::ENTRIES),
            sub_offset: memory.read_u32(header::SUB_OFFSET),
            comp_offset: memory.read_u32(header::COMP_OFFSET),
            sub_head: memory.read_u32(header::SUB_HEAD),
            comp_tail: memory.read_u32(header::COMP_TAIL),
            sub_want_bell: memory.read_u32(header::SUB_WANT_BELL),
            reserved,
        }
    }

    /// Apply every setup check to the copy, against a ring VMO of `ring_bytes`
    /// bytes.
    ///
    /// # Errors
    ///
    /// The [`HeaderError`] naming the first rule the header breaks.
    pub fn validate(&self, ring_bytes: u64) -> Result<RingLayout, HeaderError> {
        if self.magic != MAGIC {
            return Err(HeaderError::BadMagic);
        }
        if self.version != VERSION {
            return Err(HeaderError::BadVersion);
        }
        if self.flags & !KNOWN_HEADER_FLAGS != 0 {
            return Err(HeaderError::UnknownFlags);
        }
        if self.reserved != [0; header::RESERVED_BYTES] {
            return Err(HeaderError::ReservedNotZero);
        }
        if self.sub_head != 0 || self.comp_tail != 0 || self.sub_want_bell != 0 {
            return Err(HeaderError::NotFresh);
        }
        RingLayout::new(self.entries, self.sub_offset, self.comp_offset, ring_bytes)
    }
}

/// Write a fresh header for `layout`: the driver's six setup fields, and zero
/// in every index, want-bell flag and reserved byte.
pub fn write_header<M: RingMemory>(memory: &mut M, layout: &RingLayout) {
    for at in 0..HEADER_BYTES {
        memory.write_u8(at, 0);
    }
    for (at, byte) in MAGIC.iter().enumerate() {
        memory.write_u8(header::MAGIC.wrapping_add(at), *byte);
    }
    memory.write_u16(header::VERSION, VERSION);
    memory.write_u16(header::FLAGS, 0);
    memory.write_u32(header::ENTRIES, layout.entries);
    memory.write_u32(header::SUB_OFFSET, layout.sub_offset);
    memory.write_u32(header::COMP_OFFSET, layout.comp_offset);
}
