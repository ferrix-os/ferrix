//! The control record on its partition: reading both copies and writing the
//! next one, confirmed by reading it back (`docs/AB-BOOT.md` §3).
//!
//! Every decision about the bytes -- which copy is current, where the next
//! write goes, what a record may say -- is `ferrix-slots`', the crate the
//! loader uses too. This is only its file I/O.

use std::fs::File;

use ferrix_slots::record::{COPY_OFFSETS, Current, RECORD_LEN, Record, select, write_copy};

use crate::disk;

/// Both copies, as read.
pub struct Copies {
    pub bytes: [[u8; RECORD_LEN]; 2],
    pub current: Current,
}

impl Copies {
    /// The current record, if there is one.
    #[must_use]
    pub fn record(&self) -> Option<Record> {
        match self.current {
            Current::Valid { record, .. } => Some(record),
            Current::Uninitialised => None,
        }
    }

    /// Whether copy `i` is a valid record.
    #[must_use]
    pub fn valid(&self, i: usize) -> bool {
        Record::decode(&self.bytes[i]).is_ok()
    }
}

/// Read both copies from the control partition `base` bytes into `file`.
///
/// # Errors
///
/// A read error.
pub fn read(file: &mut File, base: u64) -> std::io::Result<Copies> {
    let mut bytes = [[0_u8; RECORD_LEN]; 2];
    for (copy, &at) in bytes.iter_mut().zip(&COPY_OFFSETS) {
        disk::read_at(file, base + at, copy)?;
    }
    let current = select(&bytes[0], &bytes[1]);
    Ok(Copies { bytes, current })
}

/// Write `next` over the copy `from` did not hold the current record in, as one whole
/// logical block of `block` bytes (the record, then zeros); flush; read it
/// back and compare. Only a confirmed write is returned as written.
///
/// # Errors
///
/// A write error, or a copy that does not read back as written.
pub fn write(
    file: &mut File,
    base: u64,
    from: &Copies,
    next: Record,
    block: usize,
) -> Result<Record, String> {
    let copy = write_copy(&from.current);
    let at = base + COPY_OFFSETS[copy];
    let mut sector = vec![0_u8; block.max(RECORD_LEN)];
    sector[..RECORD_LEN].copy_from_slice(&next.encode());
    disk::write_at(file, at, &sector).map_err(|e| format!("writing copy {copy}: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("flushing copy {copy}: {e}"))?;
    let mut back = vec![0_u8; sector.len()];
    disk::read_at(file, at, &mut back).map_err(|e| format!("reading copy {copy} back: {e}"))?;
    if back != sector {
        return Err(format!(
            "copy {copy} of the control record did not read back as written"
        ));
    }
    let after = read(file, base)
        .map(|c| c.record())
        .map_err(|e| e.to_string())?;
    if after != Some(next) {
        return Err(format!(
            "the control record reads {after:?} after writing {next:?} to copy {copy}"
        ));
    }
    Ok(next)
}
