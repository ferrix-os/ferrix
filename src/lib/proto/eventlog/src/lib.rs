//! The Ferrix event log: where a panic's record is kept across power cycles
//! (`docs/POSTMORTEM.md` §4).
//!
//! The panicked kernel writes nothing here. Its log is in RAM (the pstore,
//! `ferrix_bootinfo::pstore`); the machine resets itself (`ferrix.panic=reset`)
//! and the next boot's loader -- or the inspection Linux, when the boot
//! manager starts that instead -- appends the record to a partition of its
//! own, through firmware's (or Linux's) block layer, on a machine that is not
//! failing. This crate is everything both writers and every reader share:
//!
//! * the partition's [`Geometry`] and the one function every block address
//!   comes from ([`Geometry::lba`]), which refuses anything outside it (E1);
//! * the [`Header`], two copies, each with its own generation and CRC-32C,
//!   written after the record, the copy A first (E2, E4);
//! * the record ([`Meta`], [`encode_record`], [`decode_record`]): 32 KiB,
//!   the panic report first, then the log before it, a CRC-32C over all;
//! * [`compose`], which builds a record's text out of RAM a panicked kernel
//!   left, trusting none of it (E5);
//! * [`append`], the order of writes, idempotent by the pstore record's
//!   identity (E3), and a reset loop's copies collapsed into a count on the
//!   newest record (E8);
//! * [`BadFrames`], the frames the machine checks retired, kept in an
//!   extension of each header copy so that they outlive a power cycle
//!   (`docs/MACHINE-CHECK-RESTART.md` §9), with [`add_bad_frames`] and
//!   [`clear_bad_frames`].
//!
//! Records are ordered by their index, the log's own count of appends, kept
//! in the header: neither the pstore's boot sequence (it starts again at 1
//! after every cold boot) nor the firmware's clock (it may be wrong) orders
//! them (E7); the time is informative.
//!
//! What is logged in this version: a previous boot's record that holds a
//! panic report. A record without one (a clean reboot, or a hang a warm
//! reset ended) and a boot with no record at all (a cold boot, a power cut)
//! are not logged: telling them apart needs the kernel's clean-shutdown
//! mark, which is a change of its own (the consultant's E6).
//!
//! Pure functions of bytes over a block device the caller provides, no
//! allocation and no `unsafe`, so the loader runs them on firmware's pages
//! and the host tests reach all of them.

#![no_std]
#![forbid(unsafe_code)]

use ferrix_bootinfo::pstore::{self, HEADER_BYTES, Text};
use ferrix_slots::gpt::{GptError, Lookup, Part};
use ferrix_slots::record::{ControlDisk, DeviceError};

pub use ferrix_slots::gpt::EVENTS_TYPE;

#[cfg(test)]
mod tests;

/// Bytes each header copy takes: one page, of which the first
/// [`HEADER_USED`] are the header and the rest zero.
pub const HEADER_LEN: u64 = 4096;
/// Where the two header copies are, from the partition's start.
pub const COPY_OFFSETS: [u64; 2] = [0, HEADER_LEN];
/// Where the first record is.
pub const RECORDS_OFFSET: u64 = 2 * HEADER_LEN;
/// Bytes per record.
pub const RECORD_LEN: usize = 32 * 1024;
/// Bytes of a record's head, before its text.
pub const HEAD_LEN: usize = 96;
/// The most text a record holds.
pub const TEXT_MAX: usize = RECORD_LEN - HEAD_LEN;
/// The fewest records a partition must hold.
pub const MIN_RECORDS: u32 = 2;
/// The most records used, whatever the partition's size.
pub const MAX_RECORDS: u32 = 4096;
/// The largest block size the log is kept on: every place in it is a
/// multiple of a page, so a block must divide one.
pub const MAX_BLOCK: u32 = 4096;
/// Bytes of a header copy that mean something: the header, then the
/// bad-frame extension.
pub const HEADER_USED: usize = EXTENSION_CHECKED + 4;
/// The most frames the bad-frame set holds (`docs/MACHINE-CHECK-RESTART.md`
/// §9.1): the pstore table's bound.
pub const BAD_FRAMES_MOST: usize = 64;

/// The header's and a record's magic numbers.
const HEADER_MAGIC: &[u8; 8] = b"FXEVENTS";
const RECORD_MAGIC: &[u8; 8] = b"FXEVTREC";
/// The format's version.
const VERSION: u32 = 1;
/// The header bytes its CRC covers, and where the CRC is.
const HEADER_CHECKED: usize = 40;
/// Where the bad-frame extension starts in a header copy.
const EXTENSION_AT: usize = 64;
/// The extension's magic number and version.
const EXTENSION_MAGIC: &[u8; 8] = b"FXBADFRM";
const EXTENSION_VERSION: u32 = 1;
/// Where the extension's count and frames are.
const EXTENSION_COUNT: usize = 76;
const EXTENSION_RESERVED: usize = 80;
const EXTENSION_FRAMES: usize = 84;
/// Where the extension's CRC is: it covers the header's checked bytes
/// (0..40, so an extension cannot be paired with the other copy's header)
/// continued over the extension's (64..340). Not the header's own CRC with
/// them: a CRC-32C run over bytes and their CRC ends in the same state
/// whatever the bytes, which would bind nothing.
const EXTENSION_CHECKED: usize = EXTENSION_FRAMES + 4 * BAD_FRAMES_MOST;
/// A record head's bytes its CRC covers (with the text), and where it is.
const HEAD_CHECKED: usize = 92;
/// The most appends counted before the log refuses more: never reached,
/// and a bound on a hostile header.
const APPENDED_LIMIT: u64 = 1 << 48;

/// The line a failure report starts with.
pub const PANIC_MARKER: &[u8] = b"FERRIX-PANIC ";
/// Between a record's report and the log before it.
pub const SEPARATOR: &[u8] = b"\n-- the log before the report --\n";
/// The longest piece of the panic line that keys a run of the same panic.
pub const KEY_LINE: usize = 160;

/// What a record says happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The previous boot's kernel log ends in a panic report.
    Panic,
}

impl Kind {
    const fn code(self) -> u8 {
        match self {
            Kind::Panic => 1,
        }
    }

    const fn of(code: u8) -> Option<Kind> {
        match code {
            1 => Some(Kind::Panic),
            _ => None,
        }
    }

    /// The word a reader prints.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Kind::Panic => "panicked",
        }
    }
}

/// A record's flags.
pub mod flags {
    /// The text did not fit: the report, or the log before it, is cut.
    pub const TRUNCATED: u8 = 1;
    /// The text is unchecked: the pstore's ring carries no checksum, so
    /// firmware may have written over parts of it (`docs/POSTMORTEM.md`
    /// §3.1). Every record composed from a pstore carries it.
    pub const UNCHECKED: u8 = 2;
}

/// Why bytes are not a header, a record or a usable partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The block size is not 512 to 4096 bytes and a power of two, or the
    /// partition holds fewer than [`MIN_RECORDS`] records.
    Geometry,
    /// Not the magic number or the version.
    Magic,
    /// A field is out of its bounds, or does not fit this partition.
    Field,
    /// The CRC does not match.
    Crc,
}

/// Where the log is: its partition, in blocks of the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The partition's first block.
    first_lba: u64,
    /// Its bytes.
    bytes: u64,
    /// Bytes per block.
    block_size: u32,
    /// Records it holds.
    records: u32,
}

impl Geometry {
    /// The log on `part`, a partition of blocks of `block_size` bytes.
    pub fn of(part: &Part, block_size: u32) -> Result<Geometry, Refusal> {
        if !(512..=MAX_BLOCK).contains(&block_size) || !block_size.is_power_of_two() {
            return Err(Refusal::Geometry);
        }
        if part.first_lba > part.last_lba {
            return Err(Refusal::Geometry);
        }
        let bytes = part.bytes(block_size);
        let records = bytes.saturating_sub(RECORDS_OFFSET) / RECORD_LEN as u64;
        let records = u32::try_from(records.min(u64::from(MAX_RECORDS))).unwrap_or(MAX_RECORDS);
        if records < MIN_RECORDS {
            return Err(Refusal::Geometry);
        }
        Ok(Geometry {
            first_lba: part.first_lba,
            bytes,
            block_size,
            records,
        })
    }

    /// Records the log holds.
    #[must_use]
    pub const fn records(&self) -> u32 {
        self.records
    }

    /// Bytes per block.
    #[must_use]
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// The block at `offset` bytes into the partition, when `offset` is a
    /// whole number of blocks and the block lies wholly inside it: the one
    /// function every address written or read comes from (E1).
    #[must_use]
    pub fn lba(&self, offset: u64) -> Option<u64> {
        let size = u64::from(self.block_size);
        if !offset.is_multiple_of(size) || offset.checked_add(size)? > self.bytes {
            return None;
        }
        self.first_lba.checked_add(offset / size)
    }

    /// Where record `slot` starts, from the partition's start.
    #[must_use]
    pub fn record_offset(&self, slot: u32) -> Option<u64> {
        if slot >= self.records {
            return None;
        }
        RECORDS_OFFSET.checked_add(u64::from(slot).checked_mul(RECORD_LEN as u64)?)
    }
}

/// What a writer learned of the boot disk's partition table, for
/// [`classify`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scan {
    /// The disk could not be read, or its table could not: a firmware or
    /// device failure.
    Unreadable,
    /// The table was read, and `gpt::find_one` answered.
    Scanned(Result<Part, Lookup>),
}

/// Whether the boot disk has an event log, and whether it can take a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    /// No log: no GUID partition table at all, or none of the log's type.
    /// The pstore goes on as PM3's (`docs/POSTMORTEM.md` §3.3).
    Absent,
    /// One log, usable.
    Present(Geometry),
    /// A log may be there and cannot be used: the record must not be let go
    /// (fail closed).
    Unusable(Unusable),
}

/// Why a log that may be there cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// The disk or its table could not be read.
    Read,
    /// The table is damaged, or the log's entry is outside the usable blocks
    /// or overlaps another.
    Table(GptError),
    /// More than one partition of the log's type.
    Many,
    /// The partition is too small, or the disk's block size is not one the
    /// log is kept on.
    Geometry(Refusal),
}

/// Decide from `scan` whether the boot disk's log is absent, usable, or
/// present but unusable, on a disk of `block_size`-byte blocks. Only a disk
/// with no GPT signature and a table with no partition of the log's type
/// are absent; every other failure is [`Store::Unusable`], so that a record
/// is never dropped on the machine that has a log because the log could not
/// be read (the consultant's (2), ledger line 2195).
#[must_use]
pub fn classify(scan: Scan, block_size: u32) -> Store {
    match scan {
        Scan::Unreadable => Store::Unusable(Unusable::Read),
        Scan::Scanned(Err(Lookup::Missing | Lookup::Table(GptError::NoSignature))) => Store::Absent,
        Scan::Scanned(Err(Lookup::Many)) => Store::Unusable(Unusable::Many),
        Scan::Scanned(Err(Lookup::Table(why))) => Store::Unusable(Unusable::Table(why)),
        Scan::Scanned(Ok(part)) => match Geometry::of(&part, block_size) {
            Ok(geometry) => Store::Present(geometry),
            Err(why) => Store::Unusable(Unusable::Geometry(why)),
        },
    }
}

/// The frames kept out as bad across power cycles
/// (`docs/MACHINE-CHECK-RESTART.md` §9.1): at most [`BAD_FRAMES_MOST`]
/// frame numbers of 4 KiB pages, strictly ascending, so none is there
/// twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadFrames {
    /// The frames, the first `count` of them; the rest 0.
    frames: [u32; BAD_FRAMES_MOST],
    /// How many.
    count: usize,
}

/// What [`BadFrames::insert`] did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    /// It is in the set now.
    Added,
    /// It was in the set.
    Already,
    /// The set holds [`BAD_FRAMES_MOST`] others: not added.
    Full,
}

impl BadFrames {
    /// No frames.
    #[must_use]
    pub const fn empty() -> BadFrames {
        BadFrames {
            frames: [0; BAD_FRAMES_MOST],
            count: 0,
        }
    }

    /// The frames, ascending.
    #[must_use]
    pub fn frames(&self) -> &[u32] {
        self.frames.get(..self.count).unwrap_or_default()
    }

    /// How many.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Put `frame` in its place, keeping the order.
    pub fn insert(&mut self, frame: u32) -> Insert {
        let Err(at) = self.frames().binary_search(&frame) else {
            return Insert::Already;
        };
        if self.count >= BAD_FRAMES_MOST {
            return Insert::Full;
        }
        self.frames.copy_within(at..self.count, at + 1);
        if let Some(slot) = self.frames.get_mut(at) {
            *slot = frame;
        }
        self.count += 1;
        Insert::Added
    }

    /// Write the extension into `bytes` (a copy's [`HEADER_USED`] bytes, the
    /// header's written), unless the set is empty: then the extension's bytes
    /// stay zero, and the copy is byte for byte a PM4 header.
    fn encode_into(&self, bytes: &mut [u8; HEADER_USED]) {
        if self.is_empty() {
            return;
        }
        put(bytes, EXTENSION_AT, EXTENSION_MAGIC);
        put(bytes, EXTENSION_AT + 8, &EXTENSION_VERSION.to_le_bytes());
        put(bytes, EXTENSION_COUNT, &(self.count as u32).to_le_bytes());
        for (index, frame) in self.frames().iter().enumerate() {
            put(bytes, EXTENSION_FRAMES + 4 * index, &frame.to_le_bytes());
        }
        let crc = extension_crc(bytes);
        put(bytes, EXTENSION_CHECKED, &crc.to_le_bytes());
    }

    /// The set in a copy's `bytes`, its header already decoded: empty where
    /// the extension's bytes are all zero; refused where they are anything
    /// but a whole, valid extension, so that the copy is invalid as a whole
    /// and the other copy is taken -- never "no frames" in place of damage.
    fn decode(bytes: &[u8]) -> Result<BadFrames, Refusal> {
        let extension = bytes.get(EXTENSION_AT..HEADER_USED).ok_or(Refusal::Field)?;
        if extension.iter().all(|&byte| byte == 0) {
            return Ok(BadFrames::empty());
        }
        if bytes.get(EXTENSION_AT..EXTENSION_AT + 8) != Some(&EXTENSION_MAGIC[..])
            || le32(bytes, EXTENSION_AT + 8) != Some(EXTENSION_VERSION)
        {
            return Err(Refusal::Magic);
        }
        if le32(bytes, EXTENSION_CHECKED) != Some(extension_crc(bytes)) {
            return Err(Refusal::Crc);
        }
        let count = le32(bytes, EXTENSION_COUNT).ok_or(Refusal::Field)? as usize;
        if count == 0 || count > BAD_FRAMES_MOST || le32(bytes, EXTENSION_RESERVED) != Some(0) {
            return Err(Refusal::Field);
        }
        let mut set = BadFrames::empty();
        for index in 0..BAD_FRAMES_MOST {
            let frame = le32(bytes, EXTENSION_FRAMES + 4 * index).ok_or(Refusal::Field)?;
            if index >= count {
                if frame != 0 {
                    return Err(Refusal::Field);
                }
                continue;
            }
            // Strictly ascending: in order, and none twice.
            if set.frames().last().is_some_and(|&last| frame <= last) {
                return Err(Refusal::Field);
            }
            if let Some(slot) = set.frames.get_mut(index) {
                *slot = frame;
            }
            set.count += 1;
        }
        Ok(set)
    }
}

/// The extension's CRC in a copy's `bytes`: the header's checked bytes, then
/// the extension's up to its CRC.
fn extension_crc(bytes: &[u8]) -> u32 {
    crc32c_continue(
        pstore::crc32c(bytes.get(..HEADER_CHECKED).unwrap_or_default()),
        bytes
            .get(EXTENSION_AT..EXTENSION_CHECKED)
            .unwrap_or_default(),
    )
}

/// The log's header: where the next record goes, how many were ever
/// appended, and the bad-frame set its copy carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The header's generation: one more on every write; the newer valid
    /// copy is the current one.
    pub sequence: u64,
    /// The slot the next new record goes to.
    pub next: u32,
    /// Records ever appended: the next record's index.
    pub appended: u64,
    /// The frames kept out as bad, carried through every header write.
    pub bad: BadFrames,
}

impl Header {
    /// A log nothing was ever appended to.
    #[must_use]
    pub const fn empty() -> Header {
        Header {
            sequence: 1,
            next: 0,
            appended: 0,
            bad: BadFrames::empty(),
        }
    }

    /// Its bytes, for a log of `records` records.
    #[must_use]
    pub fn encode(&self, records: u32) -> [u8; HEADER_USED] {
        let mut bytes = [0u8; HEADER_USED];
        put(&mut bytes, 0, HEADER_MAGIC);
        put(&mut bytes, 8, &VERSION.to_le_bytes());
        put(&mut bytes, 12, &(RECORD_LEN as u32).to_le_bytes());
        put(&mut bytes, 16, &records.to_le_bytes());
        put(&mut bytes, 20, &self.next.to_le_bytes());
        put(&mut bytes, 24, &self.sequence.to_le_bytes());
        put(&mut bytes, 32, &self.appended.to_le_bytes());
        let crc = pstore::crc32c(bytes.get(..HEADER_CHECKED).unwrap_or_default());
        put(&mut bytes, HEADER_CHECKED, &crc.to_le_bytes());
        self.bad.encode_into(&mut bytes);
        bytes
    }

    /// The header in `bytes`, if it is one for a log of `records` records.
    pub fn decode(bytes: &[u8], records: u32) -> Result<Header, Refusal> {
        if bytes.get(..8) != Some(&HEADER_MAGIC[..]) || le32(bytes, 8) != Some(VERSION) {
            return Err(Refusal::Magic);
        }
        let checked = bytes.get(..HEADER_CHECKED).ok_or(Refusal::Field)?;
        if le32(bytes, HEADER_CHECKED) != Some(pstore::crc32c(checked)) {
            return Err(Refusal::Crc);
        }
        let header = Header {
            next: le32(bytes, 20).ok_or(Refusal::Field)?,
            sequence: le64(bytes, 24).ok_or(Refusal::Field)?,
            appended: le64(bytes, 32).ok_or(Refusal::Field)?,
            bad: BadFrames::empty(),
        };
        if le32(bytes, 12) != Some(RECORD_LEN as u32)
            || le32(bytes, 16) != Some(records)
            || header.next >= records
            || header.appended >= APPENDED_LIMIT
            || header.sequence == 0
        {
            return Err(Refusal::Field);
        }
        // Bytes 44..64 are zero in every PM4 header.
        if bytes
            .get(HEADER_CHECKED + 4..EXTENSION_AT)
            .is_none_or(|gap| gap.iter().any(|&byte| byte != 0))
        {
            return Err(Refusal::Field);
        }
        Ok(Header {
            bad: BadFrames::decode(bytes)?,
            ..header
        })
    }

    /// The slot of the newest record, if any was appended.
    #[must_use]
    pub const fn newest_slot(&self, records: u32) -> Option<u32> {
        if self.appended == 0 || records == 0 {
            return None;
        }
        Some(if self.next == 0 {
            records - 1
        } else {
            self.next - 1
        })
    }

    /// The header after one more record went to slot `next`.
    #[must_use]
    pub const fn after_append(&self, records: u32) -> Header {
        Header {
            sequence: self.sequence.wrapping_add(1),
            next: if self.next.saturating_add(1) >= records {
                0
            } else {
                self.next + 1
            },
            appended: self.appended.saturating_add(1),
            bad: self.bad,
        }
    }
}

/// The current header out of the two copies' decodings: the valid one, or
/// of two valid ones the newer.
#[must_use]
pub fn select(a: Result<Header, Refusal>, b: Result<Header, Refusal>) -> Option<Header> {
    match (a, b) {
        (Ok(a), Ok(b)) => Some(if b.sequence > a.sequence { b } else { a }),
        (Ok(one), Err(_)) | (Err(_), Ok(one)) => Some(one),
        (Err(_), Err(_)) => None,
    }
}

/// A record's head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Its index: the log's count of appends when it was appended.
    pub index: u64,
    /// What happened.
    pub kind: Kind,
    /// [`flags`].
    pub flags: u8,
    /// How many boots in a row ended this way: 1, and one more for each
    /// collapsed into it (E8).
    pub count: u32,
    /// The pstore's boot sequence of the first boot it describes.
    pub first_boot: u64,
    /// The pstore's boot sequence of the last boot collapsed into it.
    pub last_boot: u64,
    /// The identity of the last pstore record appended into it (E3).
    pub identity: u64,
    /// The CRC-32C of the panic line, which keys a run of the same panic.
    pub key: u32,
    /// Firmware's clock when the first boot was appended, as seconds since
    /// the Unix epoch, little-endian; 0 when firmware gave none.
    /// Informative only (E7).
    pub first_time: [u8; 8],
    /// Firmware's clock when the last boot was appended.
    pub last_time: [u8; 8],
    /// Bytes of text.
    pub text_len: u32,
}

/// Write `meta` and `text` as a record into `out`, [`RECORD_LEN`] bytes.
/// `None` when the text is longer than [`TEXT_MAX`] or `out` the wrong
/// length.
pub fn encode_record(meta: &Meta, text: &[u8], out: &mut [u8]) -> Option<()> {
    if out.len() != RECORD_LEN || text.len() > TEXT_MAX {
        return None;
    }
    out.fill(0);
    out.get_mut(HEAD_LEN..HEAD_LEN.checked_add(text.len())?)?
        .copy_from_slice(text);
    let meta = Meta {
        text_len: u32::try_from(text.len()).ok()?,
        ..*meta
    };
    seal(&meta, out)
}

/// Write `meta`'s head over `record`'s and the CRC over head and text, the
/// text as it is in `record`.
pub fn seal(meta: &Meta, record: &mut [u8]) -> Option<()> {
    let text_len = usize::try_from(meta.text_len).ok()?;
    if record.len() != RECORD_LEN || text_len > TEXT_MAX {
        return None;
    }
    let head = record.get_mut(..HEAD_LEN)?;
    head.fill(0);
    put(head, 0, RECORD_MAGIC);
    put(head, 8, &VERSION.to_le_bytes());
    put(head, 12, &[meta.kind.code(), meta.flags]);
    put(head, 16, &meta.index.to_le_bytes());
    put(head, 24, &meta.count.to_le_bytes());
    put(head, 28, &meta.text_len.to_le_bytes());
    put(head, 32, &meta.first_boot.to_le_bytes());
    put(head, 40, &meta.last_boot.to_le_bytes());
    put(head, 48, &meta.identity.to_le_bytes());
    put(head, 56, &meta.key.to_le_bytes());
    put(head, 64, &meta.first_time);
    put(head, 72, &meta.last_time);
    let crc = record_crc(record, text_len)?;
    put(
        record.get_mut(..HEAD_LEN)?,
        HEAD_CHECKED,
        &crc.to_le_bytes(),
    );
    Some(())
}

/// The CRC-32C over a record's checked head and its text.
fn record_crc(record: &[u8], text_len: usize) -> Option<u32> {
    let head = record.get(..HEAD_CHECKED)?;
    let text = record.get(HEAD_LEN..HEAD_LEN.checked_add(text_len)?)?;
    Some(crc32c_continue(pstore::crc32c(head), text))
}

/// The record in `bytes`: its head and its text, if it is one.
pub fn decode_record(bytes: &[u8]) -> Result<(Meta, &[u8]), Refusal> {
    if bytes.len() != RECORD_LEN {
        return Err(Refusal::Field);
    }
    if bytes.get(..8) != Some(&RECORD_MAGIC[..]) || le32(bytes, 8) != Some(VERSION) {
        return Err(Refusal::Magic);
    }
    let text_len = le32(bytes, 28).ok_or(Refusal::Field)?;
    let len = usize::try_from(text_len).map_err(|_| Refusal::Field)?;
    if len > TEXT_MAX {
        return Err(Refusal::Field);
    }
    if record_crc(bytes, len) != le32(bytes, HEAD_CHECKED) {
        return Err(Refusal::Crc);
    }
    let kind = bytes
        .get(12)
        .copied()
        .and_then(Kind::of)
        .ok_or(Refusal::Field)?;
    let meta = Meta {
        index: le64(bytes, 16).ok_or(Refusal::Field)?,
        kind,
        flags: bytes.get(13).copied().ok_or(Refusal::Field)?,
        count: le32(bytes, 24).ok_or(Refusal::Field)?,
        text_len,
        first_boot: le64(bytes, 32).ok_or(Refusal::Field)?,
        last_boot: le64(bytes, 40).ok_or(Refusal::Field)?,
        identity: le64(bytes, 48).ok_or(Refusal::Field)?,
        key: le32(bytes, 56).ok_or(Refusal::Field)?,
        first_time: eight(bytes, 64).ok_or(Refusal::Field)?,
        last_time: eight(bytes, 72).ok_or(Refusal::Field)?,
    };
    if meta.count == 0 || meta.index >= APPENDED_LIMIT {
        return Err(Refusal::Field);
    }
    let text = bytes
        .get(HEAD_LEN..HEAD_LEN.saturating_add(len))
        .ok_or(Refusal::Field)?;
    Ok((meta, text))
}

/// What a previous boot's pstore record makes of an event: what [`append`]
/// writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Incoming {
    /// What happened.
    pub kind: Kind,
    /// [`flags`].
    pub flags: u8,
    /// The pstore's boot sequence of that boot.
    pub boot: u64,
    /// The pstore record's identity: its boot sequence in the upper half,
    /// the CRC-32C of its whole header (the count of bytes written among
    /// it) in the lower.
    pub identity: u64,
    /// The CRC-32C of the panic line.
    pub key: u32,
    /// Firmware's clock now ([`Meta::first_time`]).
    pub time: [u8; 8],
    /// Bytes of text composed.
    pub text_len: usize,
}

/// Compose the event a previous boot's pstore region holds, if it holds a
/// panic report: the report first (from its `FERRIX-PANIC` line to the end
/// of the text), then [`SEPARATOR`] and as much of the log before the report
/// as fits, from a line's start. The text goes into `text`, at most
/// [`TEXT_MAX`] bytes, every byte as `pstore::shown` prints it and every
/// `FERRIX-` made `FERRIX_` (`pstore::defuse`), so a record printed on a
/// console is never read as a marker of the boot printing it.
///
/// `header` and `ring` are what the panicked kernel left in RAM, trusted in
/// nothing: the header goes through `pstore::validate`, which clamps the
/// count to the ring, and every place is bounded by the text's length and
/// `text`'s (E5). `None` when the header is refused, the text holds no panic
/// report, or `text` is shorter than [`TEXT_MAX`].
#[must_use]
pub fn compose(
    header: &[u8; HEADER_BYTES],
    ring: &[u8],
    time: [u8; 8],
    text: &mut [u8],
) -> Option<Incoming> {
    let found = pstore::validate(header, (ring.len() as u64).checked_add(pstore::HEADER)?).ok()?;
    let log = Text::of(ring, &found);
    let (line_start, line_end) = log.last_line_after(PANIC_MARKER)?;
    let report_start = line_start.checked_sub(PANIC_MARKER.len())?;
    let text = text.get_mut(..TEXT_MAX)?;
    let mut key_bytes = [0u8; KEY_LINE];
    let shown = log.shown_into((line_start, line_end), KEY_LINE, &mut key_bytes);
    let key = pstore::crc32c(key_bytes.get(..shown).unwrap_or_default());

    let report_len = log.len().saturating_sub(report_start);
    let report_kept = report_len.min(TEXT_MAX);
    let mut truncated = report_kept < report_len;
    let mut at = copy_shown(
        &log,
        report_start,
        report_start.saturating_add(report_kept),
        text,
        0,
    );
    let room = TEXT_MAX.saturating_sub(at).saturating_sub(SEPARATOR.len());
    if report_start > 0 && room > 0 {
        let mut tail_start = report_start.saturating_sub(room);
        if tail_start > 0 {
            truncated = true;
            // From a line's start: skip the piece of one.
            while tail_start < report_start && log.at(tail_start.saturating_sub(1)) != Some(b'\n') {
                tail_start += 1;
            }
        }
        if let Some(place) = text.get_mut(at..at.saturating_add(SEPARATOR.len())) {
            place.copy_from_slice(SEPARATOR);
            at = at.saturating_add(SEPARATOR.len());
        }
        at = copy_shown(&log, tail_start, report_start, text, at);
    } else if report_start > 0 {
        truncated = true;
    }
    pstore::defuse(text.get_mut(..at).unwrap_or_default());
    let identity = (found.sequence << 32) | u64::from(pstore::crc32c(header));
    Some(Incoming {
        kind: Kind::Panic,
        flags: flags::UNCHECKED | if truncated { flags::TRUNCATED } else { 0 },
        boot: found.sequence,
        identity,
        key,
        time,
        text_len: at,
    })
}

/// Copy the log's bytes `start..end`, each as `pstore::shown` prints it,
/// into `out` from `at`, as many as fit; where the copy ended.
fn copy_shown(log: &Text<'_>, start: usize, end: usize, out: &mut [u8], at: usize) -> usize {
    let mut place = at;
    for from in start..end {
        let (Some(slot), Some(byte)) = (out.get_mut(place), log.at(from)) else {
            break;
        };
        *slot = pstore::shown(byte);
        place += 1;
    }
    place
}

/// What [`append`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The newest record is this pstore record already (E3): nothing written.
    Skipped {
        /// The newest record's index.
        index: u64,
    },
    /// The newest record is the same panic: its count is one more (E8).
    Collapsed {
        /// The newest record's index.
        index: u64,
        /// Its count now.
        count: u32,
    },
    /// A new record.
    Appended {
        /// Its index.
        index: u64,
        /// The slot it went to.
        slot: u32,
    },
}

/// Why [`append`] did not make the record durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendError {
    /// A place outside the partition, or a buffer of the wrong length.
    Range,
    /// The device refused a read, a write or a flush.
    Device,
    /// Neither header copy decodes and one carries the header's magic:
    /// damage, or a newer format (`docs/MACHINE-CHECK-RESTART.md` §9.2).
    /// Nothing is written over it.
    Damaged,
}

impl From<DeviceError> for AppendError {
    fn from(_: DeviceError) -> Self {
        AppendError::Device
    }
}

/// The log's header on the disk, as [`read_header`] found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "no_std and nothing here allocates; read once or twice a boot, on the stack"
)]
pub enum Current {
    /// Neither copy carries the header's magic: a partition never written.
    /// The one case a fresh header is written in.
    Fresh,
    /// Neither copy decodes, and one carries the magic (or could not be
    /// read): damage, or a format newer than this one. Nothing is written
    /// over it, and its bad frames are not read.
    Damaged,
    /// The current header: the valid copy, or of two the newer.
    Header(Header),
}

/// The current header on the disk.
pub fn read_header(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
) -> Result<Current, AppendError> {
    let mut block = [0u8; MAX_BLOCK as usize];
    let size = geometry.block_size as usize;
    let mut copies = [Err(Refusal::Magic), Err(Refusal::Magic)];
    let mut marked = false;
    for (copy, offset) in copies.iter_mut().zip(COPY_OFFSETS) {
        let lba = geometry.lba(offset).ok_or(AppendError::Range)?;
        let bytes = block.get_mut(..size).ok_or(AppendError::Range)?;
        *copy = match disk.read_block(lba, bytes) {
            Ok(()) => {
                marked |= bytes.get(..8) == Some(&HEADER_MAGIC[..]);
                Header::decode(bytes, geometry.records)
            }
            // An unreadable copy is an invalid one, and may hold anything:
            // never taken for a partition never written.
            Err(DeviceError) => {
                marked = true;
                Err(Refusal::Magic)
            }
        };
    }
    let [a, b] = copies;
    Ok(match select(a, b) {
        Some(header) => Current::Header(header),
        None if marked => Current::Damaged,
        None => Current::Fresh,
    })
}

/// The header to write to: the current one, or on a partition never
/// written a fresh one, both copies written first. Damage is refused.
fn header_to_write(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
) -> Result<Header, AppendError> {
    match read_header(disk, geometry)? {
        Current::Header(header) => Ok(header),
        Current::Damaged => Err(AppendError::Damaged),
        Current::Fresh => {
            let fresh = Header::empty();
            write_header(disk, geometry, &fresh)?;
            Ok(fresh)
        }
    }
}

/// Read record `slot` into `buffer` ([`RECORD_LEN`] bytes) and decode it.
/// `None` when the slot does not hold a valid record.
pub fn read_record(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
    slot: u32,
    buffer: &mut [u8],
) -> Result<Option<Meta>, AppendError> {
    if buffer.len() != RECORD_LEN {
        return Err(AppendError::Range);
    }
    let offset = geometry.record_offset(slot).ok_or(AppendError::Range)?;
    if read_bytes(disk, geometry, offset, buffer).is_err() {
        return Ok(None);
    }
    Ok(decode_record(buffer).ok().map(|(meta, _)| meta))
}

/// Whether a valid record in a slot belongs to the log: its index is one
/// the header has counted. A record written whose header write never
/// happened (a reset between them) is not one, and is written over next.
#[must_use]
pub const fn counted(header: &Header, meta: &Meta) -> bool {
    meta.index < header.appended
}

/// Append `incoming`, whose text is `record[HEAD_LEN..][..incoming.text_len]`
/// (`record` is [`RECORD_LEN`] bytes, its head written here), to the log.
/// `scratch` is another [`RECORD_LEN`] bytes, for the newest record.
///
/// The order (E2): the newest record is read through the header; when it is
/// this pstore record already, nothing is written (E3); when it is the same
/// panic (kind and key), its count goes up by one and it is written again in
/// place, then flushed (E8); otherwise the record goes to the header's next
/// slot and is flushed, then the header's copy A, flushed, then copy B,
/// flushed. `Ok` only once that is all durable; then, and only then, the
/// caller may let the pstore record go. A partition never written gets a
/// fresh header first, both copies; damaged copies get nothing
/// ([`Current::Damaged`]). The bad-frame set is carried through.
pub fn append(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
    incoming: &Incoming,
    record: &mut [u8],
    scratch: &mut [u8],
) -> Result<Outcome, AppendError> {
    if record.len() != RECORD_LEN || scratch.len() != RECORD_LEN || incoming.text_len > TEXT_MAX {
        return Err(AppendError::Range);
    }
    let header = header_to_write(disk, geometry)?;
    let records = geometry.records;
    if let Some(slot) = header.newest_slot(records)
        && let Some(newest) = read_record(disk, geometry, slot, scratch)?
        && newest.index.saturating_add(1) == header.appended
    {
        if newest.identity == incoming.identity {
            return Ok(Outcome::Skipped {
                index: newest.index,
            });
        }
        if newest.kind == incoming.kind && newest.key == incoming.key {
            let count = newest.count.saturating_add(1);
            let collapsed = Meta {
                count,
                last_boot: incoming.boot,
                identity: incoming.identity,
                last_time: incoming.time,
                ..newest
            };
            seal(&collapsed, scratch).ok_or(AppendError::Range)?;
            let offset = geometry.record_offset(slot).ok_or(AppendError::Range)?;
            write_bytes(disk, geometry, offset, scratch)?;
            disk.flush()?;
            return Ok(Outcome::Collapsed {
                index: newest.index,
                count,
            });
        }
    }
    let meta = Meta {
        index: header.appended,
        kind: incoming.kind,
        flags: incoming.flags,
        count: 1,
        first_boot: incoming.boot,
        last_boot: incoming.boot,
        identity: incoming.identity,
        key: incoming.key,
        first_time: incoming.time,
        last_time: incoming.time,
        text_len: u32::try_from(incoming.text_len).map_err(|_| AppendError::Range)?,
    };
    seal(&meta, record).ok_or(AppendError::Range)?;
    let slot = header.next;
    let offset = geometry.record_offset(slot).ok_or(AppendError::Range)?;
    write_bytes(disk, geometry, offset, record)?;
    disk.flush()?;
    write_header(disk, geometry, &header.after_append(records))?;
    Ok(Outcome::Appended {
        index: meta.index,
        slot,
    })
}

/// What [`add_bad_frames`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Added {
    /// Frames new to the set.
    pub added: u32,
    /// Frames the set held already.
    pub already: u32,
    /// Frames not added: the set holds [`BAD_FRAMES_MOST`].
    pub full: u32,
    /// The set's frames now.
    pub held: u32,
}

/// Add `frames` to the log's bad-frame set (`docs/MACHINE-CHECK-RESTART.md`
/// §9.2): merged in order, none twice, at most [`BAD_FRAMES_MOST`]. When
/// none is new, nothing is written; otherwise copy A with the generation one
/// more, flushed, then copy B, flushed. `Ok` only once that is durable; then,
/// and only then, the caller may let the pstore table go. The set only
/// grows: a torn write leaves the older copy, a subset of the newer.
pub fn add_bad_frames(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
    frames: &[u32],
) -> Result<Added, AppendError> {
    let header = header_to_write(disk, geometry)?;
    let mut bad = header.bad;
    let mut added = Added {
        added: 0,
        already: 0,
        full: 0,
        held: 0,
    };
    for &frame in frames {
        match bad.insert(frame) {
            Insert::Added => added.added += 1,
            Insert::Already => added.already += 1,
            Insert::Full => added.full += 1,
        }
    }
    added.held = bad.len() as u32;
    if added.added > 0 {
        let next = Header {
            sequence: header.sequence.wrapping_add(1),
            bad,
            ..header
        };
        write_header(disk, geometry, &next)?;
    }
    Ok(added)
}

/// Empty the log's bad-frame set: what root does on a host after the memory
/// was replaced (`docs/MACHINE-CHECK-RESTART.md` §9.4); nothing in Ferrix
/// calls it. Copy A with the generation one more, flushed, then copy B,
/// flushed; nothing written when the set is empty already. The frames
/// dropped.
pub fn clear_bad_frames(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
) -> Result<BadFrames, AppendError> {
    let header = match read_header(disk, geometry)? {
        Current::Fresh => return Ok(BadFrames::empty()),
        Current::Damaged => return Err(AppendError::Damaged),
        Current::Header(header) => header,
    };
    if header.bad.is_empty() {
        return Ok(header.bad);
    }
    let next = Header {
        sequence: header.sequence.wrapping_add(1),
        bad: BadFrames::empty(),
        ..header
    };
    write_header(disk, geometry, &next)?;
    Ok(header.bad)
}

/// What became of one of the two things the pstore region holds for the
/// disk -- a panic record, the bad-frame table -- before the region is
/// renewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// It is in the log, durably.
    Durable,
    /// The log does not need it, or cannot take it and the boot goes on
    /// without (no log, an unusable or damaged one, no panic, no frames).
    NotNeeded,
    /// The log's write failed: the region is the only copy.
    Failed,
}

/// Whether the loader may give the pstore region a fresh header
/// (`docs/MACHINE-CHECK-RESTART.md` §9.2, the region rule; POSTMORTEM §4,
/// E2): not while the region is the only copy of a record or a frame the
/// log failed to take.
#[must_use]
pub const fn may_renew(record: Durability, frames: Durability) -> bool {
    !matches!(record, Durability::Failed) && !matches!(frames, Durability::Failed)
}

/// Write `header` to copy A, flush, then copy B, flush.
fn write_header(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
    header: &Header,
) -> Result<(), AppendError> {
    let mut block = [0u8; MAX_BLOCK as usize];
    let bytes = block
        .get_mut(..geometry.block_size as usize)
        .ok_or(AppendError::Range)?;
    put(bytes, 0, &header.encode(geometry.records));
    for offset in COPY_OFFSETS {
        let lba = geometry.lba(offset).ok_or(AppendError::Range)?;
        disk.write_block(lba, bytes)?;
        disk.flush()?;
    }
    Ok(())
}

/// Write `bytes`, a whole number of blocks, at `offset` into the partition,
/// every block's address from [`Geometry::lba`].
fn write_bytes(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
    offset: u64,
    bytes: &[u8],
) -> Result<(), AppendError> {
    let size = geometry.block_size as usize;
    if !bytes.len().is_multiple_of(size) {
        return Err(AppendError::Range);
    }
    // Every block checked before the first is written.
    let last = offset
        .checked_add(bytes.len() as u64)
        .and_then(|end| end.checked_sub(size as u64))
        .ok_or(AppendError::Range)?;
    let _ = geometry.lba(last).ok_or(AppendError::Range)?;
    for (index, block) in bytes.chunks_exact(size).enumerate() {
        let at = offset
            .checked_add((index * size) as u64)
            .ok_or(AppendError::Range)?;
        let lba = geometry.lba(at).ok_or(AppendError::Range)?;
        disk.write_block(lba, block)?;
    }
    Ok(())
}

/// Read `into`, a whole number of blocks, from `offset` into the partition.
fn read_bytes(
    disk: &mut impl ControlDisk,
    geometry: &Geometry,
    offset: u64,
    into: &mut [u8],
) -> Result<(), AppendError> {
    let size = geometry.block_size as usize;
    if !into.len().is_multiple_of(size) {
        return Err(AppendError::Range);
    }
    for (index, block) in into.chunks_exact_mut(size).enumerate() {
        let at = offset
            .checked_add((index * size) as u64)
            .ok_or(AppendError::Range)?;
        let lba = geometry.lba(at).ok_or(AppendError::Range)?;
        disk.read_block(lba, block)?;
    }
    Ok(())
}

/// The first line of a record's text, at most `most` bytes: the report's
/// panic line, `FERRIX_PANIC ...`.
#[must_use]
pub fn first_line(text: &[u8], most: usize) -> &[u8] {
    let end = text
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(text.len())
        .min(most);
    text.get(..end).unwrap_or_default()
}

/// The CRC-32C of `bytes` continued from `crc`, a finished CRC-32C of the
/// bytes before them.
fn crc32c_continue(crc: u32, bytes: &[u8]) -> u32 {
    let mut state = !crc;
    for &byte in bytes {
        state ^= u32::from(byte);
        for _ in 0..8 {
            state = if state & 1 != 0 {
                (state >> 1) ^ 0x82F6_3B78
            } else {
                state >> 1
            };
        }
    }
    !state
}

/// Copy `bytes` into `into` at `at`, as far as it reaches.
fn put(into: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(place) = into.get_mut(at..at.saturating_add(bytes.len())) {
        place.copy_from_slice(bytes);
    }
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn le64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn eight(bytes: &[u8], at: usize) -> Option<[u8; 8]> {
    bytes.get(at..at.checked_add(8)?)?.try_into().ok()
}
