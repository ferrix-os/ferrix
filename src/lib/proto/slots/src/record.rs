//! The boot control record (`docs/AB-BOOT.md` §3).
//!
//! Two copies of one 512-byte record, 64 KiB apart in the control
//! partition. The current record is the valid copy with the higher
//! sequence; a write goes to the other copy, so a write torn by a power cut
//! damages only the older copy and the record reads as if it had never been
//! written. A writer then reads the copy back: only a write it read back
//! counts.

use crate::crc::crc32c;
use crate::{Slot, le16, le32, le64, put};

/// What every record begins with.
pub const MAGIC: [u8; 8] = *b"FXBOOTCR";
/// The format this crate writes and reads.
pub const VERSION: u16 = 1;
/// Bytes in a record.
pub const RECORD_LEN: usize = 512;
/// Where each copy starts, in bytes from the start of the control partition.
pub const COPY_OFFSETS: [u64; 2] = [0, 65536];
/// The smallest control partition: room for copy 1 and a 64 KiB block after it.
pub const MIN_CONTROL_BYTES: u64 = 131_072;
/// The most tries a trial can be armed with.
pub const MAX_TRIES: u8 = 7;
/// The record byte for "no trial".
const NO_TRIAL: u8 = 0xFF;
/// Where the CRC-32C is, covering every byte before it.
const CRC_AT: usize = RECORD_LEN - 4;
/// The first byte after the fields; every byte from here to the CRC is zero.
const FIELDS_END: usize = 27;

/// One record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Higher is newer.
    pub sequence: u64,
    /// The slot that is known good.
    pub good: Slot,
    /// The slot armed for a trial, never `good`.
    pub trial: Option<Slot>,
    /// Tries left for the trial; zero when there is none.
    pub tries: u8,
}

/// Why a copy is not a valid record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// Fewer than [`RECORD_LEN`] bytes.
    Short,
    /// Not [`MAGIC`]; a zeroed copy is this.
    Magic,
    /// Another version or length.
    Version,
    /// The CRC-32C does not match.
    Checksum,
    /// A reserved byte is not zero.
    Reserved,
    /// A field holds a value the format does not allow.
    Field,
}

impl Record {
    /// The first record `bootctl init` writes: `good`, nothing armed.
    #[must_use]
    pub const fn initial(good: Slot) -> Record {
        Record {
            sequence: 1,
            good,
            trial: None,
            tries: 0,
        }
    }

    /// Whether the fields are ones the format allows.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.tries <= MAX_TRIES
            && match self.trial {
                None => self.tries == 0,
                Some(trial) => trial != self.good,
            }
    }

    /// The record with one try taken off, as the loader writes it before it
    /// starts a trial; `None` when there is no try to take.
    #[must_use]
    pub fn after_try(&self) -> Option<Record> {
        let trial = self.trial?;
        let tries = self.tries.checked_sub(1)?;
        Some(Record {
            sequence: self.sequence.checked_add(1)?,
            good: self.good,
            trial: Some(trial),
            tries,
        })
    }

    /// The record arming `trial` with `tries`, as `bootctl try` writes it.
    #[must_use]
    pub fn armed(&self, trial: Slot, tries: u8) -> Option<Record> {
        let next = Record {
            sequence: self.sequence.checked_add(1)?,
            good: self.good,
            trial: Some(trial),
            tries,
        };
        (tries > 0 && next.is_consistent()).then_some(next)
    }

    /// The record with nothing armed, as `bootctl try` writes it before it
    /// overwrites an armed slot.
    #[must_use]
    pub fn disarmed(&self) -> Option<Record> {
        Some(Record {
            sequence: self.sequence.checked_add(1)?,
            good: self.good,
            trial: None,
            tries: 0,
        })
    }

    /// The record making the trial the good slot, as `bootctl good` writes it.
    #[must_use]
    pub fn marked_good(&self) -> Option<Record> {
        Some(Record {
            sequence: self.sequence.checked_add(1)?,
            good: self.trial?,
            trial: None,
            tries: 0,
        })
    }

    /// The record's 512 bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut out = [0u8; RECORD_LEN];
        put(&mut out, 0, &MAGIC);
        put(&mut out, 8, &VERSION.to_le_bytes());
        put(&mut out, 10, &(RECORD_LEN as u16).to_le_bytes());
        put(&mut out, 16, &self.sequence.to_le_bytes());
        put(&mut out, 24, &[self.good.code()]);
        put(&mut out, 25, &[self.trial.map_or(NO_TRIAL, Slot::code)]);
        put(&mut out, 26, &[self.tries]);
        let crc = crc32c(out.get(..CRC_AT).unwrap_or_default());
        put(&mut out, CRC_AT, &crc.to_le_bytes());
        out
    }

    /// Read a record from the first [`RECORD_LEN`] bytes of `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Record, RecordError> {
        let bytes = bytes.get(..RECORD_LEN).ok_or(RecordError::Short)?;
        if bytes.get(..8) != Some(&MAGIC[..]) {
            return Err(RecordError::Magic);
        }
        if le16(bytes, 8) != Some(VERSION) || le16(bytes, 10) != Some(RECORD_LEN as u16) {
            return Err(RecordError::Version);
        }
        let covered = bytes.get(..CRC_AT).ok_or(RecordError::Short)?;
        if le32(bytes, CRC_AT) != Some(crc32c(covered)) {
            return Err(RecordError::Checksum);
        }
        let reserved_zero = le32(bytes, 12) == Some(0)
            && bytes
                .get(FIELDS_END..CRC_AT)
                .is_some_and(|rest| rest.iter().all(|&byte| byte == 0));
        if !reserved_zero {
            return Err(RecordError::Reserved);
        }
        let sequence = le64(bytes, 16).ok_or(RecordError::Short)?;
        let field = |at: usize| bytes.get(at).copied().ok_or(RecordError::Short);
        let good = Slot::from_code(field(24)?).ok_or(RecordError::Field)?;
        let trial = match field(25)? {
            NO_TRIAL => None,
            code => Some(Slot::from_code(code).ok_or(RecordError::Field)?),
        };
        let record = Record {
            sequence,
            good,
            trial,
            tries: field(26)?,
        };
        if record.is_consistent() {
            Ok(record)
        } else {
            Err(RecordError::Field)
        }
    }
}

/// Which record is current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Current {
    /// Neither copy is valid: a new control partition, or both damaged.
    Uninitialised,
    /// The current record, and the copy it is in.
    Valid {
        /// 0 or 1.
        copy: usize,
        /// The record.
        record: Record,
    },
}

/// The current record of two copies' bytes.
#[must_use]
pub fn select(copy0: &[u8], copy1: &[u8]) -> Current {
    match (Record::decode(copy0), Record::decode(copy1)) {
        (Ok(first), Ok(second)) if second.sequence > first.sequence => Current::Valid {
            copy: 1,
            record: second,
        },
        (Ok(first), _) => Current::Valid {
            copy: 0,
            record: first,
        },
        (Err(_), Ok(second)) => Current::Valid {
            copy: 1,
            record: second,
        },
        (Err(_), Err(_)) => Current::Uninitialised,
    }
}

/// The copy a new record goes to: the one not holding the current record.
/// Copy 0 when there is none, as `bootctl init` writes it.
#[must_use]
pub const fn write_copy(current: &Current) -> usize {
    match current {
        Current::Uninitialised => 0,
        Current::Valid { copy, .. } => 1 - (*copy & 1),
    }
}

/// The largest logical block a record is written in.
pub const MAX_BLOCK: usize = 4096;

/// The control partition's device, as the loader (UEFI Block I/O) and
/// `bootctl` (the partition's node) each see it. Blocks are numbered from
/// the start of the disk.
pub trait ControlDisk {
    /// Bytes per logical block.
    fn block_size(&self) -> u32;
    /// Write one block.
    fn write_block(&mut self, lba: u64, bytes: &[u8]) -> Result<(), DeviceError>;
    /// Make what was written durable.
    fn flush(&mut self) -> Result<(), DeviceError>;
    /// Read one block.
    fn read_block(&mut self, lba: u64, bytes: &mut [u8]) -> Result<(), DeviceError>;
}

/// The device refused an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceError;

/// Why a record write did not count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The block size is under 512 or over [`MAX_BLOCK`], or the copy's
    /// block is not inside the control partition.
    Range,
    /// The record is not one the format allows.
    Record,
    /// The device refused the write, the flush or the read.
    Device,
    /// The block read back is not the block written.
    ReadBack,
}

/// Read both copies of the record and select the current one.
pub fn read_current(
    disk: &mut impl ControlDisk,
    control: &crate::gpt::Part,
) -> Result<Current, WriteError> {
    let size = usable_block(disk)?;
    let mut copies = [[0u8; MAX_BLOCK]; 2];
    for (copy, buffer) in copies.iter_mut().enumerate() {
        let lba = crate::plan::record_block(control, copy, size).ok_or(WriteError::Range)?;
        let block = buffer.get_mut(..size as usize).ok_or(WriteError::Range)?;
        if disk.read_block(lba, block).is_err() {
            // An unreadable copy is an invalid one: zeroes say so.
            block.fill(0);
        }
    }
    let [first, second] = &copies;
    Ok(select(first, second))
}

/// Write `next` as the new record over `current`: to the copy not holding
/// the current record, flushed, then read back and compared. `Ok` only when
/// the block read back is the block written -- the loader starts a trial
/// only after that.
pub fn write_record(
    disk: &mut impl ControlDisk,
    control: &crate::gpt::Part,
    current: &Current,
    next: &Record,
) -> Result<(), WriteError> {
    if !next.is_consistent() {
        return Err(WriteError::Record);
    }
    if let Current::Valid { record, .. } = current
        && next.sequence <= record.sequence
    {
        return Err(WriteError::Record);
    }
    let size = usable_block(disk)?;
    let copy = write_copy(current);
    let lba = crate::plan::record_block(control, copy, size).ok_or(WriteError::Range)?;
    let mut block = [0u8; MAX_BLOCK];
    let block = block.get_mut(..size as usize).ok_or(WriteError::Range)?;
    put(block, 0, &next.encode());
    disk.write_block(lba, block)
        .map_err(|_| WriteError::Device)?;
    disk.flush().map_err(|_| WriteError::Device)?;
    let mut back = [0u8; MAX_BLOCK];
    let back = back.get_mut(..size as usize).ok_or(WriteError::Range)?;
    disk.read_block(lba, back).map_err(|_| WriteError::Device)?;
    if back != block {
        return Err(WriteError::ReadBack);
    }
    Ok(())
}

/// The disk's block size, if a record can be written in it.
fn usable_block(disk: &impl ControlDisk) -> Result<u32, WriteError> {
    let size = disk.block_size();
    if (RECORD_LEN as u32..=MAX_BLOCK as u32).contains(&size) && size.is_power_of_two() {
        Ok(size)
    } else {
        Err(WriteError::Range)
    }
}
