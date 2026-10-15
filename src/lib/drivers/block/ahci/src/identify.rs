//! IDENTIFY DEVICE's fields (ATA8-ACS section 7.16, table 22): the
//! capacity, the logical sector size, and the model and serial strings.

/// Word 83: command sets supported; bit 10 is the 48-bit address feature.
pub const WORD_COMMAND_SETS: usize = 83;
/// Words 60-61: sectors reachable with 28-bit addresses.
pub const WORD_LBA28: usize = 60;
/// Words 100-103: sectors reachable with 48-bit addresses.
pub const WORD_LBA48: usize = 100;
/// Word 106: physical and logical sector size.
pub const WORD_SECTOR_SIZE: usize = 106;
/// Words 117-118: words per logical sector, when word 106 bit 12 says so.
pub const WORD_LOGICAL_SIZE: usize = 117;
/// Words 10-19: the serial number.
pub const WORD_SERIAL: usize = 10;
/// Words 27-46: the model number.
pub const WORD_MODEL: usize = 27;

/// What IDENTIFY DEVICE said that this crate uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Parsed {
    /// Logical sectors, by 48-bit address.
    pub sectors: u64,
    /// Bytes a logical sector.
    pub sector_size: u32,
    /// The model number, ASCII padded with spaces.
    pub model: [u8; 40],
    /// The serial number, ASCII padded with spaces.
    pub serial: [u8; 20],
}

/// Why IDENTIFY DEVICE's data cannot be used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The device has no 48-bit addressing.
    NoLba48,
    /// The logical sector size is not a power of two from 512 bytes up.
    SectorSize(u32),
}

/// Take the 256 words apart; `word(i)` is word `i`.
pub fn parse(word: impl Fn(usize) -> u16) -> Result<Parsed, Refused> {
    if word(WORD_COMMAND_SETS) & (1 << 10) == 0 {
        return Err(Refused::NoLba48);
    }
    let sectors = (0..4).fold(0u64, |acc, i| {
        acc | (u64::from(word(WORD_LBA48 + i)) << (16 * i))
    });
    let sectors = if sectors == 0 {
        u64::from(word(WORD_LBA28)) | (u64::from(word(WORD_LBA28 + 1)) << 16)
    } else {
        sectors
    };
    let size_word = word(WORD_SECTOR_SIZE);
    // Bits 15:14 = 01b mark the word valid; bit 12 says words 117-118 hold
    // the logical sector's size in words.
    let sector_size = if size_word & 0xC000 == 0x4000 && size_word & (1 << 12) != 0 {
        let words =
            u32::from(word(WORD_LOGICAL_SIZE)) | (u32::from(word(WORD_LOGICAL_SIZE + 1)) << 16);
        words.saturating_mul(2)
    } else {
        512
    };
    if sector_size < 512 || !sector_size.is_power_of_two() {
        return Err(Refused::SectorSize(sector_size));
    }
    let mut model = [0; 40];
    swapped(&word, WORD_MODEL, &mut model);
    let mut serial = [0; 20];
    swapped(&word, WORD_SERIAL, &mut serial);
    Ok(Parsed {
        sectors,
        sector_size,
        model,
        serial,
    })
}

/// An ATA string: each word holds two characters, the first in its high
/// byte.
fn swapped(word: &impl Fn(usize) -> u16, first: usize, out: &mut [u8]) {
    for (i, pair) in out.chunks_exact_mut(2).enumerate() {
        pair.copy_from_slice(&word(first + i).to_be_bytes());
    }
}
