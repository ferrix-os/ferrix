//! The control record on its disk (`docs/AB-BOOT.md` §3).
//!
//! Every decision -- which copy is current, where the next write goes, which
//! block that is and that it lies inside the control partition, the flush
//! and the read-back -- is `ferrix-slots`' `read_current` and
//! `write_record`, the loader's own write path. This is only the disk they
//! run on: a whole disk open as a file, in blocks of its logical size.

use std::fs::File;

use ferrix_slots::gpt::Part;
use ferrix_slots::plan::record_block;
use ferrix_slots::record::{
    ControlDisk, Current, DeviceError, RECORD_LEN, Record, read_current, write_record,
};

use crate::disk;

/// A whole disk open as `file`, in blocks of `block` bytes.
pub struct Blocks<'a> {
    pub file: &'a mut File,
    pub block: u32,
}

impl Blocks<'_> {
    fn at(&self, lba: u64) -> Result<u64, DeviceError> {
        lba.checked_mul(u64::from(self.block)).ok_or(DeviceError)
    }
}

impl ControlDisk for Blocks<'_> {
    fn block_size(&self) -> u32 {
        self.block
    }

    fn write_block(&mut self, lba: u64, bytes: &[u8]) -> Result<(), DeviceError> {
        let at = self.at(lba)?;
        disk::write_at(self.file, at, bytes).map_err(|_| DeviceError)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        self.file.sync_all().map_err(|_| DeviceError)
    }

    fn read_block(&mut self, lba: u64, bytes: &mut [u8]) -> Result<(), DeviceError> {
        let at = self.at(lba)?;
        disk::read_at(self.file, at, bytes).map_err(|_| DeviceError)
    }
}

/// The current record of `control`.
///
/// # Errors
///
/// The control partition's blocks are out of range.
pub fn read(blocks: &mut Blocks<'_>, control: &Part) -> Result<Current, String> {
    read_current(blocks, control).map_err(|why| format!("reading the control record: {why:?}"))
}

/// Whether each copy is a valid record, for `status`.
#[must_use]
pub fn copies_valid(blocks: &mut Blocks<'_>, control: &Part) -> [bool; 2] {
    let mut valid = [false; 2];
    for (copy, slot) in valid.iter_mut().enumerate() {
        let mut bytes = vec![0_u8; (blocks.block as usize).max(RECORD_LEN)];
        *slot = record_block(control, copy, blocks.block)
            .is_some_and(|lba| blocks.read_block(lba, &mut bytes).is_ok())
            && Record::decode(&bytes).is_ok();
    }
    valid
}

/// Write `next` over `current` and confirm it, as the loader does.
///
/// # Errors
///
/// No next record (the sequence at its end), or why the write did not count.
pub fn write(
    blocks: &mut Blocks<'_>,
    control: &Part,
    current: &Current,
    next: Option<Record>,
) -> Result<Record, String> {
    let next = next.ok_or("the record has no next state for this")?;
    write_record(blocks, control, current, &next)
        .map_err(|why| format!("writing the control record: {why:?}; nothing changed"))?;
    match read(blocks, control)? {
        Current::Valid { record, .. } if record == next => Ok(next),
        other => Err(format!(
            "the control record reads {other:?} after writing {next:?}"
        )),
    }
}

/// The current record, if there is one.
#[must_use]
pub fn current(current: &Current) -> Option<Record> {
    match current {
        Current::Valid { record, .. } => Some(*record),
        Current::Uninitialised => None,
    }
}
