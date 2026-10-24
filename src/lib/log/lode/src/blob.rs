//! The blob container (`docs/LOGGING.md` §21.4): the heavy parts of a
//! crash -- screenshots, backtraces, maps, descriptor lists -- kept beside
//! the segments so the records stay lean.
//!
//! A blob is named by the SHA-256 of its bytes. Blobs are appended to
//! packs, `pack-<16 hex>.fxb`, one file a few MiB, synced once a batch; a
//! sealed pack gets an index, `pack-<16 hex>.idx`, and a reader without one
//! scans the pack. A record names a blob in a field `BLOB_<PART>`, its
//! value `<64 hex id> <size> <type>`; a part under [`INLINE_MAX`] bytes is
//! not a blob but the field itself.
//!
//! ```text
//! pack:   "FXBPACK1"  then entries
//! entry:  "FXBE"  u8 packing (0 raw, 1 zstd)  u8 type length  u16 zero
//!         [32] id  u64 stored length  u64 raw length  u32 CRC-32C of the
//!         stored bytes  u32 zero  type  stored bytes
//! index:  "FXBINDX1"  u32 count  u32 zero  then per entry [32] id  u64 offset
//!         and a u32 CRC-32C of everything before it
//! ```
//!
//! An entry whose CRC or length does not match ends a scan: what follows a
//! torn append is not trusted, as a torn segment entry ends its scan.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use ruzstd::decoding::FrameDecoder;
use ruzstd::encoding::{CompressionLevel, compress_to_vec};

use crate::crc::crc32c;
use crate::record::Record;

/// A pack's first bytes.
pub const PACK_MAGIC: [u8; 8] = *b"FXBPACK1";
/// An index's first bytes.
pub const INDEX_MAGIC: [u8; 8] = *b"FXBINDX1";
/// An entry's first bytes.
const ENTRY_MAGIC: [u8; 4] = *b"FXBE";
/// An entry's fixed part.
const ENTRY_HEAD: usize = 64;
/// A part smaller than this is a field of its record, not a blob.
pub const INLINE_MAX: usize = 4096;
/// The largest blob: a 4K screenshot, uncompressed, and room.
pub const BLOB_MAX: usize = 64 * 1024 * 1024;
/// A pack is sealed past this many bytes.
pub const PACK_MAX: usize = 16 * 1024 * 1024;
/// A blob no record names is kept this long before a sweep removes it.
pub const GRACE_US: u64 = 3_600_000_000;
/// The prefix of the fields that name blobs.
pub const FIELD_PREFIX: &str = "BLOB_";

/// A blob's name: the SHA-256 of its bytes.
pub type BlobId = [u8; 32];

/// The id of `bytes`.
#[must_use]
pub fn id_of(bytes: &[u8]) -> BlobId {
    ferrix_slots::sha256::digest(bytes)
}

/// `id` in hex.
#[must_use]
pub fn hex(id: &BlobId) -> String {
    let mut text = String::new();
    crate::segment::push_hex(&mut text, id);
    text
}

/// An id read from 64 hex digits.
#[must_use]
pub fn parse_id(text: &str) -> Option<BlobId> {
    if text.len() != 64 {
        return None;
    }
    let mut id = [0_u8; 32];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(id)
}

/// What a record's `BLOB_<PART>` field says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The blob.
    pub id: BlobId,
    /// Its size, unpacked.
    pub size: u64,
    /// What it is: `qoi`, `text`, `json`.
    pub kind: String,
}

impl Reference {
    /// The field's value.
    #[must_use]
    pub fn text(&self) -> String {
        alloc::format!("{} {} {}", hex(&self.id), self.size, self.kind)
    }

    /// A field's value read back.
    #[must_use]
    pub fn parse(value: &[u8]) -> Option<Reference> {
        let text = core::str::from_utf8(value).ok()?;
        let mut words = text.split(' ');
        let id = parse_id(words.next()?)?;
        let size = words.next()?.parse().ok()?;
        let kind = String::from(words.next()?);
        Some(Reference { id, size, kind })
    }
}

/// Every blob `record` names, with the part's name.
#[must_use]
pub fn references(record: &Record) -> Vec<(String, Reference)> {
    record
        .fields
        .iter()
        .filter(|field| field.name.starts_with(FIELD_PREFIX))
        .filter_map(|field| Some((field.name.clone(), Reference::parse(&field.value)?)))
        .collect()
}

/// A record's field: its name and value.
pub type Field = (String, Vec<u8>);

/// A blob to be written: its id and bytes.
pub type Pending = (BlobId, Vec<u8>);

/// A part of `kind`: inline as `name` when small, else a blob. Gives the
/// field to put in the record, and the blob to write first when there is
/// one.
#[must_use]
pub fn part(name: &str, kind: &str, bytes: Vec<u8>) -> (Field, Option<Pending>) {
    if bytes.len() < INLINE_MAX {
        return ((String::from(name), bytes), None);
    }
    let id = id_of(&bytes);
    let reference = Reference {
        id,
        size: bytes.len() as u64,
        kind: String::from(kind),
    };
    (
        (
            alloc::format!("{FIELD_PREFIX}{name}"),
            reference.text().into_bytes(),
        ),
        Some((id, bytes)),
    )
}

/// One entry of a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The blob.
    pub id: BlobId,
    /// Where its entry begins in the pack.
    pub offset: u64,
    /// The entry's whole length in the pack.
    pub length: u64,
    /// Its size unpacked.
    pub size: u64,
    /// What it is.
    pub kind: String,
}

/// A blob as a pack entry: zstd-packed when that is smaller and the type
/// is not packed already.
#[must_use]
pub fn encode_entry(id: &BlobId, kind: &str, bytes: &[u8]) -> Vec<u8> {
    let packed_already = matches!(kind, "png" | "jpeg" | "zstd");
    let packed = (!packed_already)
        .then(|| compress_to_vec(bytes, CompressionLevel::Fastest))
        .filter(|packed| packed.len() < bytes.len());
    let (packing, stored): (u8, &[u8]) = match &packed {
        Some(packed) => (1, packed),
        None => (0, bytes),
    };
    let kind = kind.get(..kind.len().min(255)).unwrap_or_default();
    let mut out = Vec::with_capacity(ENTRY_HEAD + kind.len() + stored.len());
    out.extend_from_slice(&ENTRY_MAGIC);
    out.push(packing);
    out.push(u8::try_from(kind.len()).unwrap_or(0));
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(id);
    out.extend_from_slice(&(stored.len() as u64).to_le_bytes());
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&crc32c(stored).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind.as_bytes());
    out.extend_from_slice(stored);
    out
}

/// The entry at `offset` of `pack`: its description, packing and stored
/// bytes; `None` for one that does not read.
fn entry_at(pack: &[u8], offset: usize) -> Option<(Entry, u8, &[u8])> {
    let head = pack.get(offset..offset.checked_add(ENTRY_HEAD)?)?;
    if head.get(..4)? != ENTRY_MAGIC {
        return None;
    }
    let packing = *head.get(4)?;
    let kind_len = usize::from(*head.get(5)?);
    let id: BlobId = head.get(8..40)?.try_into().ok()?;
    let stored_len =
        usize::try_from(u64::from_le_bytes(head.get(40..48)?.try_into().ok()?)).ok()?;
    let size = u64::from_le_bytes(head.get(48..56)?.try_into().ok()?);
    let crc = u32::from_le_bytes(head.get(56..60)?.try_into().ok()?);
    let kind_at = offset + ENTRY_HEAD;
    let kind = core::str::from_utf8(pack.get(kind_at..kind_at + kind_len)?).ok()?;
    let stored_at = kind_at + kind_len;
    let stored = pack.get(stored_at..stored_at.checked_add(stored_len)?)?;
    if crc32c(stored) != crc || packing > 1 || size > BLOB_MAX as u64 {
        return None;
    }
    Some((
        Entry {
            id,
            offset: offset as u64,
            length: (ENTRY_HEAD + kind_len + stored_len) as u64,
            size,
            kind: String::from(kind),
        },
        packing,
        stored,
    ))
}

/// Every entry of `pack` that reads, in order, to the first that does not.
#[must_use]
pub fn scan(pack: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    if pack.get(..8) != Some(&PACK_MAGIC[..]) {
        return entries;
    }
    let mut at = 8;
    while let Some((entry, _, _)) = entry_at(pack, at) {
        at += usize::try_from(entry.length).unwrap_or(usize::MAX);
        entries.push(entry);
    }
    entries
}

/// The blob whose entry is at `offset` of `pack`, unpacked and checked
/// against its id; `None` for one that does not read or match.
#[must_use]
pub fn read(pack: &[u8], offset: u64) -> Option<(String, Vec<u8>)> {
    let (entry, packing, stored) = entry_at(pack, usize::try_from(offset).ok()?)?;
    let bytes = if packing == 1 {
        let mut out = Vec::with_capacity(usize::try_from(entry.size).ok()?);
        FrameDecoder::new()
            .decode_all_to_vec(stored, &mut out)
            .ok()?;
        out
    } else {
        Vec::from(stored)
    };
    (bytes.len() as u64 == entry.size && id_of(&bytes) == entry.id).then_some((entry.kind, bytes))
}

/// A sealed pack's index.
#[must_use]
pub fn encode_index(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + entries.len() * 40 + 4);
    out.extend_from_slice(&INDEX_MAGIC);
    out.extend_from_slice(&u32::try_from(entries.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    for entry in entries {
        out.extend_from_slice(&entry.id);
        out.extend_from_slice(&entry.offset.to_le_bytes());
    }
    let crc = crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// An index read back: ids and offsets; `None` for one that does not read.
#[must_use]
pub fn decode_index(bytes: &[u8]) -> Option<Vec<(BlobId, u64)>> {
    if bytes.get(..8)? != INDEX_MAGIC {
        return None;
    }
    let (body, crc) = bytes.split_at(bytes.len().checked_sub(4)?);
    if crc32c(body) != u32::from_le_bytes(crc.try_into().ok()?) {
        return None;
    }
    let count = usize::try_from(u32::from_le_bytes(body.get(8..12)?.try_into().ok()?)).ok()?;
    let mut out = Vec::with_capacity(count.min(1 << 20));
    for index in 0..count {
        let at = 16 + index * 40;
        let id: BlobId = body.get(at..at + 32)?.try_into().ok()?;
        let offset = u64::from_le_bytes(body.get(at + 32..at + 40)?.try_into().ok()?);
        out.push((id, offset));
    }
    Some(out)
}

/// What a sweep does with one pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sweep {
    /// Nothing in it is named any more: remove it.
    Remove,
    /// Less than half of it is named: write these entries to a new pack,
    /// then remove it.
    Rewrite(Vec<Entry>),
    /// Keep it.
    Keep,
}

/// Mark and sweep for one sealed pack: `live` the ids the remaining
/// records name (from the segments' summaries and the segment being
/// written), `young` whether the pack was written within [`GRACE_US`], so
/// a blob whose record is not yet written is not taken for an orphan.
#[must_use]
pub fn sweep(entries: &[Entry], live: &BTreeSet<BlobId>, young: bool) -> Sweep {
    if young {
        return Sweep::Keep;
    }
    let total: u64 = entries.iter().map(|entry| entry.length).sum();
    let kept: Vec<Entry> = entries
        .iter()
        .filter(|entry| live.contains(&entry.id))
        .cloned()
        .collect();
    let kept_bytes: u64 = kept.iter().map(|entry| entry.length).sum();
    if kept.is_empty() {
        Sweep::Remove
    } else if kept_bytes * 2 < total {
        Sweep::Rewrite(kept)
    } else {
        Sweep::Keep
    }
}
