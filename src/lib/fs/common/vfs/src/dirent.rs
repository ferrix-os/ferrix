//! `getdents64`'s record layout.
//!
//! ```text
//! offset  size  field
//!      0     8  d_ino
//!      8     8  d_off     the cursor that resumes after this record
//!     16     2  d_reclen  this record's length, a multiple of eight
//!     18     1  d_type
//!     19     -  d_name    NUL-terminated, then zero padding
//! ```
//!
//! The same on all three architectures, because every field is fixed-width
//! and the record is padded explicitly — which is the point of the `64` in
//! the name, and why this is the only directory-reading call Ferrix answers.
//!
//! The name starts at byte 19, not at `size_of::<linux_dirent64>()`, which
//! Rust and C both round up to 24. A packer that used the size would skip five
//! bytes of every name, and every program would see its directory entries
//! missing their first letters.

use ferrix_linux_abi::types::DIRENT64_NAME_OFFSET;

/// Records are padded to a multiple of this.
const ALIGN: usize = 8;

/// The length of a record for a name of `name_len` bytes.
///
/// `None` if the length does not fit `d_reclen`, which no name of legal
/// length reaches.
#[must_use]
pub fn record_len(name_len: usize) -> Option<usize> {
    let unpadded = DIRENT64_NAME_OFFSET.checked_add(name_len)?.checked_add(1)?;
    let padded = unpadded.checked_add(ALIGN - 1)? & !(ALIGN - 1);
    u16::try_from(padded).ok().map(|_| padded)
}

/// Packs records into a caller's buffer.
#[derive(Debug)]
pub struct DirentWriter<'a> {
    buf: &'a mut [u8],
    used: usize,
}

impl<'a> DirentWriter<'a> {
    /// A writer over `buf`, with nothing written.
    pub fn new(buf: &'a mut [u8]) -> DirentWriter<'a> {
        DirentWriter { buf, used: 0 }
    }

    /// Bytes written so far: `getdents64`'s return value.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used
    }

    /// Append one record, or return `false` if it does not fit — in which
    /// case nothing was written and the entry has not been consumed.
    pub fn push(&mut self, ino: u64, next: u64, kind: u8, name: &[u8]) -> bool {
        let Some(len) = record_len(name.len()) else {
            return false;
        };
        let Some(end) = self.used.checked_add(len) else {
            return false;
        };
        let Some(record) = self.buf.get_mut(self.used..end) else {
            return false;
        };
        let Ok(reclen) = u16::try_from(len) else {
            return false;
        };
        record.fill(0);
        let fields: [(usize, &[u8]); 5] = [
            (0, &ino.to_le_bytes()),
            (8, &next.to_le_bytes()),
            (16, &reclen.to_le_bytes()),
            (18, &[kind]),
            (DIRENT64_NAME_OFFSET, name),
        ];
        for (at, bytes) in fields {
            if let Some(slot) = record.get_mut(at..at + bytes.len()) {
                slot.copy_from_slice(bytes);
            }
        }
        self.used = end;
        true
    }
}

/// One record, as read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// `d_ino`.
    pub ino: u64,
    /// `d_off`.
    pub next: u64,
    /// `d_type`.
    pub kind: u8,
    /// `d_name`, without its terminator.
    pub name: &'a [u8],
}

/// Read records back out of a buffer, as a C library's `readdir` would.
///
/// Stops at the first malformed record rather than guessing, which is what
/// lets a test or a fuzzer assert that the writer never produces one.
pub fn records(buf: &[u8]) -> impl Iterator<Item = Record<'_>> {
    let mut at = 0_usize;
    core::iter::from_fn(move || {
        let record = buf.get(at..)?;
        let word = |offset: usize| -> Option<u64> {
            let bytes = record.get(offset..offset + 8)?;
            Some(u64::from_le_bytes(bytes.try_into().ok()?))
        };
        let ino = word(0)?;
        let next = word(8)?;
        let reclen = usize::from(u16::from_le_bytes(record.get(16..18)?.try_into().ok()?));
        let kind = *record.get(18)?;
        if reclen < DIRENT64_NAME_OFFSET + 1 || !reclen.is_multiple_of(ALIGN) {
            return None;
        }
        let body = record.get(DIRENT64_NAME_OFFSET..reclen)?;
        let name_len = body.iter().position(|&b| b == 0)?;
        at = at.checked_add(reclen)?;
        Some(Record {
            ino,
            next,
            kind,
            name: body.get(..name_len)?,
        })
    })
}
