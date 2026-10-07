//! The card's GUID partition table, read only to decide where a write may
//! go.
//!
//! The card the DK1 boots from holds its firmware: TF-A twice (`fsbl1`,
//! `fsbl2`), OP-TEE and U-Boot with U-Boot's saved environment (`fip`), and
//! Ferrix's loader and kernel (`bootfs`) -- `docs/vendor/st/stm32mp157-dk.md`
//! step 1. A driver that wrote any of those could leave the board unable to
//! boot, so this driver writes only where the card's owner said it may: in
//! a partition whose GPT name begins [`PREFIX`]. Every other block -- the
//! protective MBR and both tables, every other partition, space no
//! partition covers -- is read-only, and a card whose table does not check
//! is read-only whole.
//!
//! The table is read the way `ferrix-partition` reads it for the kernel --
//! signature, header CRC, the entry array's CRC, every partition inside the
//! usable range -- but a sector at a time and with no allocation, since a
//! driver process has no heap. A `ferrix-` partition that overlaps any other
//! partition makes the table refused, so no name can open another
//! partition's blocks to writes.

use crate::BLOCK_BYTES;

/// The name a partition's must begin with to be written.
pub const PREFIX: &str = "ferrix-";

/// The most writable partitions kept; more are refused.
pub const MOST_WRITABLE: usize = 8;

/// The most entries a table may have that this reads: what every tool
/// writes.
pub const MOST_ENTRIES: u32 = 128;

/// Why a table was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The header's signature, size, CRC or ranges are wrong.
    Header,
    /// Entries larger than a block, or more than [`MOST_ENTRIES`].
    Shape,
    /// The entry array's CRC does not match the header's.
    EntriesCrc,
    /// A partition is outside the usable range, or ends before it starts.
    Partition,
    /// A writable partition overlaps another partition.
    Overlap,
    /// More than [`MOST_WRITABLE`] writable partitions.
    TooMany,
    /// The array was fed more or fewer blocks than the header names.
    Feed,
}

/// The ranges of blocks a write may touch: each a partition's first and
/// last block, inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Writable {
    ranges: [(u64, u64); MOST_WRITABLE],
    count: usize,
}

impl Writable {
    /// Nothing may be written.
    pub const NONE: Writable = Writable {
        ranges: [(0, 0); MOST_WRITABLE],
        count: 0,
    };

    /// Whether nothing may be written.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The writable ranges.
    #[must_use]
    pub fn ranges(&self) -> &[(u64, u64)] {
        self.ranges.get(..self.count).unwrap_or_default()
    }

    /// Whether `count` blocks from `first` lie wholly inside one writable
    /// partition. A write that crosses a partition's edge is refused whole,
    /// even into two writable partitions side by side: the ring's requests
    /// come from one filesystem on one partition.
    #[must_use]
    pub fn allows(&self, first: u64, count: u64) -> bool {
        let Some(last) = first.checked_add(count).and_then(|end| end.checked_sub(1)) else {
            return false;
        };
        count > 0
            && self
                .ranges()
                .iter()
                .any(|&(start, end)| first >= start && last <= end)
    }
}

/// A table whose header checked, being fed its entry array.
#[derive(Debug)]
pub struct Table {
    first_usable: u64,
    last_usable: u64,
    entries_at: u64,
    entries: u32,
    entry_size: u32,
    entries_crc: u32,
    /// The array's CRC so far.
    crc: Crc,
    /// Entries read so far.
    seen: u32,
    /// Blocks fed so far.
    fed: u64,
    writable: Writable,
    /// Every partition's range, writable or not, for the overlap check.
    all: [(u64, u64); MOST_ENTRIES as usize],
    partitions: usize,
    refused: Option<Refused>,
}

impl Table {
    /// Check the header in `block` (the card's block 1) of a card of
    /// `blocks` blocks.
    ///
    /// # Errors
    ///
    /// [`Refused::Header`] or [`Refused::Shape`].
    pub fn header(block: &[u8; BLOCK_BYTES], blocks: u64) -> Result<Table, Refused> {
        if block.get(..8) != Some(b"EFI PART".as_slice()) {
            return Err(Refused::Header);
        }
        let size = u32_at(block, 12);
        if !(92..=BLOCK_BYTES as u32).contains(&size) {
            return Err(Refused::Header);
        }
        let mut crc = Crc::new();
        for (index, byte) in block.iter().take(size as usize).enumerate() {
            // The CRC is taken with its own field read as zeros.
            crc.update(&[if (16..20).contains(&index) { 0 } else { *byte }]);
        }
        if crc.value() != u32_at(block, 16) || u64_at(block, 24) != 1 {
            return Err(Refused::Header);
        }
        let first_usable = u64_at(block, 40);
        let last_usable = u64_at(block, 48);
        let entries_at = u64_at(block, 72);
        let entries = u32_at(block, 80);
        let entry_size = u32_at(block, 84);
        if entry_size < 128
            || !entry_size.is_power_of_two()
            || entry_size as usize > BLOCK_BYTES
            || entries == 0
            || entries > MOST_ENTRIES
        {
            return Err(Refused::Shape);
        }
        let array_blocks = u64::from(entries * entry_size).div_ceil(BLOCK_BYTES as u64);
        if entries_at < 2
            || entries_at.saturating_add(array_blocks) > first_usable
            || first_usable > last_usable
            || last_usable >= blocks
        {
            return Err(Refused::Header);
        }
        Ok(Table {
            first_usable,
            last_usable,
            entries_at,
            entries,
            entry_size,
            entries_crc: u32_at(block, 88),
            crc: Crc::new(),
            seen: 0,
            fed: 0,
            writable: Writable::NONE,
            all: [(0, 0); MOST_ENTRIES as usize],
            partitions: 0,
            refused: None,
        })
    }

    /// The first block of the entry array.
    #[must_use]
    pub const fn entries_at(&self) -> u64 {
        self.entries_at
    }

    /// How many blocks the entry array takes.
    #[must_use]
    pub fn entry_blocks(&self) -> u64 {
        u64::from(self.entries * self.entry_size).div_ceil(BLOCK_BYTES as u64)
    }

    /// Take the array's next block.
    pub fn feed(&mut self, block: &[u8; BLOCK_BYTES]) {
        self.fed += 1;
        if self.fed > self.entry_blocks() {
            let _ = self.refused.get_or_insert(Refused::Feed);
            return;
        }
        for entry in block.chunks_exact(self.entry_size as usize) {
            if self.seen >= self.entries {
                break;
            }
            self.seen += 1;
            self.crc.update(entry);
            if let Err(why) = self.entry(entry) {
                let _ = self.refused.get_or_insert(why);
            }
        }
    }

    /// One entry: an unused one (type GUID all zeros) is skipped.
    fn entry(&mut self, entry: &[u8]) -> Result<(), Refused> {
        if entry
            .get(..16)
            .is_none_or(|guid| guid.iter().all(|&b| b == 0))
        {
            return Ok(());
        }
        let first = u64_at(entry, 32);
        let last = u64_at(entry, 40);
        if first < self.first_usable || last > self.last_usable || first > last {
            return Err(Refused::Partition);
        }
        let slot = self.all.get_mut(self.partitions).ok_or(Refused::Shape)?;
        *slot = (first, last);
        self.partitions += 1;
        if named_writable(entry) {
            let slot = self
                .writable
                .ranges
                .get_mut(self.writable.count)
                .ok_or(Refused::TooMany)?;
            *slot = (first, last);
            self.writable.count += 1;
        }
        Ok(())
    }

    /// The writable ranges, once the whole array was fed and checked.
    ///
    /// # Errors
    ///
    /// The first thing wrong with the array.
    pub fn finish(self) -> Result<Writable, Refused> {
        if let Some(why) = self.refused {
            return Err(why);
        }
        if self.fed != self.entry_blocks() || self.seen != self.entries {
            return Err(Refused::Feed);
        }
        if self.crc.value() != self.entries_crc {
            return Err(Refused::EntriesCrc);
        }
        let all = self.all.get(..self.partitions).unwrap_or_default();
        for &(start, end) in self.writable.ranges() {
            let overlapping = all
                .iter()
                .filter(|&&(first, last)| first <= end && start <= last)
                .count();
            // Itself, and nothing else.
            if overlapping != 1 {
                return Err(Refused::Overlap);
            }
        }
        Ok(self.writable)
    }
}

/// Whether an entry's name, UTF-16LE at byte 56, begins with [`PREFIX`].
fn named_writable(entry: &[u8]) -> bool {
    PREFIX.bytes().enumerate().all(|(index, wanted)| {
        let at = 56 + index * 2;
        entry.get(at) == Some(&wanted) && entry.get(at + 1) == Some(&0)
    })
}

/// The little-endian `u32` at `at`, zero past the end.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0_u8; 4];
    if let Some(source) = bytes.get(at..at + 4) {
        word.copy_from_slice(source);
    }
    u32::from_le_bytes(word)
}

/// The little-endian `u64` at `at`, zero past the end.
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_at(bytes, at)) | (u64::from(u32_at(bytes, at + 4)) << 32)
}

/// CRC-32 as UEFI takes it (IEEE 802.3, reflected, polynomial 0xEDB88320),
/// a byte at a time.
#[derive(Clone, Copy, Debug)]
pub struct Crc(u32);

impl Crc {
    /// A CRC over nothing yet.
    #[must_use]
    pub const fn new() -> Crc {
        Crc(0xFFFF_FFFF)
    }

    /// Take `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u32::from(byte);
            for _ in 0..8 {
                let low = self.0 & 1;
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & 0_u32.wrapping_sub(low));
            }
        }
    }

    /// The CRC of everything taken.
    #[must_use]
    pub const fn value(&self) -> u32 {
        !self.0
    }
}

impl Default for Crc {
    fn default() -> Self {
        Crc::new()
    }
}
