//! The segment format (`docs/LOGGING.md` §8.2): one file, a header and then
//! entries, appended and never rewritten.
//!
//! ```text
//! header   0  magic "FXJOURNL"         8  u32 version (1)
//!         12  u32 header length       16  boot id [16]
//!         32  machine id [16]         48  u64 first sequence number
//!         56  u64 created, us since 1970
//!         64  u16 host length         66  host bytes, then zero padding to 8
//!
//! entry    0  u32 length (the whole entry, trailer included)
//!          4  u32 CRC-32C of bytes 8 .. length
//!          8  u8 kind (1, a record)    9  u8 priority (8: none)   10  u16 zero
//!         12  u32 field count         16  u64 sequence number
//!         24  u64 realtime us         32  u64 monotonic us
//!         40  i32 UTC offset s        44  u32 zero
//!         48  fields: u16 name length, name, u32 value length, value
//!        end  u32 length again
//! ```
//!
//! A field name in [`NAMES`] is written as its index with the top bit set
//! (`0x8000 | index`) and no name bytes: the names every record carries
//! cost two bytes, not twenty (the write budget, `docs/LOGGING.md` §8.3).
//! A real name is at most 64 bytes, so the two never meet. The list only
//! grows: an index once given keeps its name.
//!
//! Every number is little-endian. The trailing length lets a reader walk a
//! segment backwards. A reader stops at the first entry whose length runs
//! past the file, whose trailer or checksum does not match, or whose
//! sequence number does not grow: a torn tail, which the service truncates
//! away on its next start ([`scan`] says where the good part ends). Nothing
//! makes a reader go backwards or round, so it cannot loop on a damaged
//! file.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::crc::crc32c;
use crate::record::{Field, Record, valid_name};

/// The header's first bytes.
pub const MAGIC: [u8; 8] = *b"FXJOURNL";

/// The format's version.
pub const VERSION: u32 = 1;

/// Bytes of the header before the host name.
const HEADER_FIXED: usize = 66;

/// Bytes of an entry with no fields.
const ENTRY_FIXED: usize = 52;

/// The kind byte of a record entry.
const KIND_RECORD: u8 = 1;

/// The longest entry a reader takes: a record's whole size limit and room.
pub const ENTRY_MAX: usize = 2 * 1024 * 1024;

/// What a segment's header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The boot every record in the segment belongs to.
    pub boot_id: [u8; 16],
    /// The machine's id, zero where it has none.
    pub machine_id: [u8; 16],
    /// The sequence number the segment's first record has, or would have.
    pub first_seqnum: u64,
    /// When the segment was made, in microseconds since 1970.
    pub created_us: u64,
    /// The machine's name.
    pub host: String,
}

impl Header {
    /// The header's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let host = self.host.as_bytes();
        let host = host.get(..usize::from(u16::MAX)).unwrap_or(host);
        let length = (HEADER_FIXED + host.len()).next_multiple_of(8);
        let mut out = Vec::with_capacity(length);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&u32::try_from(length).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.boot_id);
        out.extend_from_slice(&self.machine_id);
        out.extend_from_slice(&self.first_seqnum.to_le_bytes());
        out.extend_from_slice(&self.created_us.to_le_bytes());
        out.extend_from_slice(&u16::try_from(host.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(host);
        out.resize(length, 0);
        out
    }

    /// Read a header from the start of `bytes`: it and its length.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<(Header, usize)> {
        if bytes.get(..8)? != MAGIC || u32_at(bytes, 8)? != VERSION {
            return None;
        }
        let length = usize::try_from(u32_at(bytes, 12)?).ok()?;
        let host_len = usize::from(u16::from_le_bytes(array(bytes, 64)?));
        if length < HEADER_FIXED + host_len || length > bytes.len() {
            return None;
        }
        let host = bytes.get(HEADER_FIXED..HEADER_FIXED + host_len)?;
        Some((
            Header {
                boot_id: array(bytes, 16)?,
                machine_id: array(bytes, 32)?,
                first_seqnum: u64_at(bytes, 48)?,
                created_us: u64_at(bytes, 56)?,
                host: String::from_utf8_lossy(host).into_owned(),
            },
            length,
        ))
    }
}

/// Field names written as an index (see the module's documentation).
/// Append only: an index once given keeps its name.
pub const NAMES: [&str; 40] = [
    "MESSAGE",
    "SYSLOG_IDENTIFIER",
    "SYSLOG_PID",
    "SYSLOG_FACILITY",
    "_TRANSPORT",
    "_KERNEL_SEQNUM",
    "_SOURCE_MONOTONIC_TIMESTAMP",
    "_SOURCE_REALTIME_TIMESTAMP",
    "FERRIX_SUBSYSTEM",
    "FERRIX_PRIORITY_SOURCE",
    "FERRIX_PRIORITY_WORD",
    "UNIT",
    "_SYSTEMD_UNIT",
    "_SYSTEMD_SLICE",
    "_SYSTEMD_CGROUP",
    "_SYSTEMD_INVOCATION_ID",
    "_PID",
    "_UID",
    "_GID",
    "_COMM",
    "_EXE",
    "_CMDLINE",
    "_HOSTNAME",
    "_MACHINE_ID",
    "_STREAM",
    "_SESSION_ID",
    "MESSAGE_ID",
    "CODE_FILE",
    "CODE_LINE",
    "CODE_FUNC",
    "ERRNO",
    "MARK",
    "REPEATED",
    "N_SUPPRESSED",
    "N_DROPPED",
    "SYSLOG_TIMESTAMP",
    "_FERRIX_DRIVER",
    "_FERRIX_DEVICE",
    "TRACE_ID",
    "SPAN_ID",
];

/// The top bit of a name length that is an index into [`NAMES`].
const NAME_INDEX: u16 = 0x8000;

/// `record` as one entry's bytes, appended to `out`.
pub fn encode_record(record: &Record, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0; 8]);
    out.push(KIND_RECORD);
    out.push(record.priority.min(crate::record::UNSPECIFIED));
    out.extend_from_slice(&[0; 2]);
    let fields: Vec<&Field> = record
        .fields
        .iter()
        .filter(|field| valid_name(&field.name))
        .collect();
    out.extend_from_slice(&u32::try_from(fields.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&record.seqnum.to_le_bytes());
    out.extend_from_slice(&record.realtime_us.to_le_bytes());
    out.extend_from_slice(&record.monotonic_us.to_le_bytes());
    out.extend_from_slice(&record.utc_offset_s.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for field in fields {
        if let Some(index) = NAMES
            .iter()
            .position(|known| *known == field.name)
            .and_then(|index| u16::try_from(index).ok())
        {
            out.extend_from_slice(&(NAME_INDEX | index).to_le_bytes());
        } else {
            let name = field.name.as_bytes();
            out.extend_from_slice(&u16::try_from(name.len()).unwrap_or(0).to_le_bytes());
            out.extend_from_slice(name);
        }
        out.extend_from_slice(&u32::try_from(field.value.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&field.value);
    }
    let length = u32::try_from(out.len() - start + 4).unwrap_or(0);
    out.extend_from_slice(&length.to_le_bytes());
    let crc = crc32c(out.get(start + 8..).unwrap_or_default());
    if let Some(slot) = out.get_mut(start..start + 4) {
        slot.copy_from_slice(&length.to_le_bytes());
    }
    if let Some(slot) = out.get_mut(start + 4..start + 8) {
        slot.copy_from_slice(&crc.to_le_bytes());
    }
}

/// One entry found by [`scan`]: the record and where it lies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The record, with its segment's boot and host.
    pub record: Record,
    /// The entry's first byte in the file.
    pub offset: usize,
    /// Its length.
    pub length: usize,
}

/// What [`scan`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned {
    /// The header.
    pub header: Header,
    /// The records, oldest first.
    pub records: Vec<Found>,
    /// Where the good entries end: the file's length, or the first byte of
    /// a torn or damaged tail.
    pub good: usize,
}

/// Read a whole segment: its header and every good entry from `from` on (0
/// for the first). `None` for bytes with no header.
#[must_use]
pub fn scan(bytes: &[u8], from: usize) -> Option<Scanned> {
    let (header, header_len) = Header::decode(bytes)?;
    let host: Rc<str> = Rc::from(header.host.as_str());
    let mut at = from.max(header_len);
    let mut records = Vec::new();
    let mut last: Option<u64> = None;
    while let Some((mut record, length)) = entry_at(bytes, at) {
        if last.is_some_and(|last| record.seqnum <= last) {
            break;
        }
        last = Some(record.seqnum);
        record.boot_id = header.boot_id;
        record.host = Rc::clone(&host);
        records.push(Found {
            record,
            offset: at,
            length,
        });
        at += length;
    }
    Some(Scanned {
        header,
        records,
        good: at,
    })
}

/// The whole entries at the start of `bytes`, a segment's bytes from some
/// entry's first byte on (what a follower reads past the last it saw), and
/// how many bytes they take. The records have no boot or host: the caller
/// knows the segment's header.
#[must_use]
pub fn entries(bytes: &[u8]) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let mut at = 0;
    while let Some((record, length)) = entry_at(bytes, at) {
        records.push(record);
        at += length;
    }
    (records, at)
}

/// The entry at `at`, and its length, if it is whole and its checksum holds.
#[must_use]
pub fn entry_at(bytes: &[u8], at: usize) -> Option<(Record, usize)> {
    let length = usize::try_from(u32_at(bytes, at)?).ok()?;
    if !(ENTRY_FIXED..=ENTRY_MAX).contains(&length) {
        return None;
    }
    let entry = bytes.get(at..at.checked_add(length)?)?;
    if u32_at(entry, length - 4)? != u32::try_from(length).ok()?
        || u32_at(entry, 4)? != crc32c(entry.get(8..)?)
        || *entry.get(8)? != KIND_RECORD
    {
        return None;
    }
    let mut record = Record::new(*entry.get(9)?, u64_at(entry, 24)?, u64_at(entry, 32)?);
    record.seqnum = u64_at(entry, 16)?;
    record.utc_offset_s = i32::from_le_bytes(array(entry, 40)?);
    let count = u32_at(entry, 12)?;
    let mut place = 48;
    let end = length - 4;
    for _ in 0..count {
        let word = u16::from_le_bytes(array(entry, place)?);
        let name = if word & NAME_INDEX == 0 {
            let name_len = usize::from(word);
            let name = core::str::from_utf8(entry.get(place + 2..place + 2 + name_len)?).ok()?;
            place += 2 + name_len;
            name
        } else {
            place += 2;
            NAMES.get(usize::from(word & !NAME_INDEX))?
        };
        let value_len = usize::try_from(u32_at(entry, place)?).ok()?;
        let value = entry.get(place + 4..(place + 4).checked_add(value_len)?)?;
        place += 4 + value_len;
        if place > end {
            return None;
        }
        record.fields.push(Field {
            name: String::from(name),
            value: Vec::from(value),
        });
    }
    (place == end).then_some((record, length))
}

/// `N` bytes at `at`.
fn array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// The little-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    array(bytes, at).map(u32::from_le_bytes)
}

/// The little-endian `u64` at `at`.
fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    array(bytes, at).map(u64::from_le_bytes)
}

/// A segment's file name: `<class>@<boot>-<first seqnum>.fxj`, the boot
/// and the number in hex, so a class's names sort by boot and then by
/// sequence within it.
#[must_use]
pub fn file_name(class: &str, boot_id: &[u8; 16], first_seqnum: u64) -> String {
    let mut name = String::from(class);
    name.push('@');
    push_hex(&mut name, boot_id);
    name.push('-');
    name.push_str(&alloc::format!("{first_seqnum:016x}"));
    name.push_str(".fxj");
    name
}

/// Whether `name` is a segment's file name.
#[must_use]
pub fn is_segment_name(name: &str) -> bool {
    class_of_name(name).is_some() && name.ends_with(".fxj")
}

/// The class a segment's file name says.
#[must_use]
pub fn class_of_name(name: &str) -> Option<&str> {
    name.split_once('@')
        .map(|(class, _)| class)
        .filter(|class| !class.is_empty() && !class.starts_with('.'))
}

/// The first sequence number a segment's file name says, for ordering
/// segments without opening them.
#[must_use]
pub fn seqnum_of_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".fxj")?;
    let (_, hex) = stem.rsplit_once('-')?;
    u64::from_str_radix(hex, 16).ok()
}

/// A segment's bytes again without the records `remove` picks: its header
/// and every other good record, re-encoded, and how many were removed. A
/// torn tail is not carried over. `None` for bytes with no header.
#[must_use]
pub fn rewrite(bytes: &[u8], remove: impl Fn(&Record) -> bool) -> Option<(Vec<u8>, usize)> {
    let scanned = scan(bytes, 0)?;
    let mut out = scanned.header.encode();
    let mut removed = 0;
    for found in &scanned.records {
        if remove(&found.record) {
            removed += 1;
        } else {
            encode_record(&found.record, &mut out);
        }
    }
    Some((out, removed))
}

/// `bytes` in lower-case hex, appended to `out`.
pub fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        for nibble in [byte >> 4, byte & 15] {
            let digit = DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0');
            out.push(char::from(digit));
        }
    }
}

/// 32 hex digits as 16 bytes.
#[must_use]
pub fn parse_id(text: &str) -> Option<[u8; 16]> {
    let text = text.trim();
    let text: String = text.chars().filter(|c| *c != '-').collect();
    if text.len() != 32 {
        return None;
    }
    let mut id = [0_u8; 16];
    for (index, slot) in id.iter_mut().enumerate() {
        let pair = text.get(index * 2..index * 2 + 2)?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(id)
}
