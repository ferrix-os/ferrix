//! What the HBA reads from memory for a command: the command header in the
//! command list, the command FIS in the command table, and the physical
//! region descriptor table after it (AHCI 1.3.1 sections 4.2.2 and 4.2.3;
//! the FIS is SATA's Register Host to Device FIS), and where each lives in
//! this crate's control memory.

/// IDENTIFY DEVICE.
pub const IDENTIFY: u8 = 0xEC;
/// READ DMA EXT.
pub const READ_DMA_EXT: u8 = 0x25;
/// WRITE DMA EXT.
pub const WRITE_DMA_EXT: u8 = 0x35;
/// FLUSH CACHE EXT.
pub const FLUSH_CACHE_EXT: u8 = 0xEA;

/// The Register Host to Device FIS's type.
pub const FIS_H2D: u8 = 0x27;
/// The FIS's `C` bit: this FIS carries a command.
pub const FIS_COMMAND: u8 = 0x80;
/// The device register's `LBA` bit.
pub const DEVICE_LBA: u8 = 0x40;
/// The command FIS's length in dwords, as the command header gives it.
pub const FIS_DWORDS: u32 = 5;

/// The command header's `W` bit: data goes to the device.
pub const HEADER_WRITE: u32 = 1 << 6;
/// The command header's `C` bit: clear `BSY` -- and the command's `PxCI`
/// bit -- as soon as the FIS is sent. Only for a FIS with no data and no
/// answer (a soft reset); on a data command the command looks done before
/// its data came, which real HBAs do and QEMU's, finishing at once, hides.
/// Never set by this crate.
pub const HEADER_CLEAR_BUSY: u32 = 1 << 10;

/// The most bytes one PRDT entry describes.
pub const PRD_MAX: u32 = 4 << 20;
/// The most sectors one READ/WRITE DMA EXT moves (a count of zero means
/// 65536; this crate never sends zero).
pub const MAX_SECTORS: u32 = 65535;

// Control memory: page 0 holds the command list at 0 (32 headers of 32
// bytes; only slot 0 is used), the received FIS area at 1 KiB (256 bytes)
// and the one command table at 2 KiB, its PRDT filling the page. Page 1
// is where IDENTIFY's 512 bytes land.

/// The command list's offset in control memory.
pub const COMMAND_LIST: usize = 0;
/// The received FIS area's offset in control memory.
pub const RECEIVED_FIS: usize = 1024;
/// The command table's offset in control memory.
pub const TABLE: usize = 2048;
/// The command FIS's offset in the command table.
pub const TABLE_FIS: usize = 0x00;
/// The PRDT's offset in the command table.
pub const TABLE_PRDT: usize = 0x80;
/// One PRDT entry's size.
pub const PRD_SIZE: usize = 16;
/// PRDT entries the command table holds.
pub const PRD_ENTRIES: usize = (crate::regs::PAGE - TABLE - TABLE_PRDT) / PRD_SIZE;
/// The offset of the page IDENTIFY's data lands in.
pub const IDENTIFY_PAGE: usize = crate::regs::PAGE;

/// Slot 0's command header.
#[must_use]
pub fn header(write: bool, prds: u16, table: u64) -> [u8; 32] {
    let mut flags = FIS_DWORDS | (u32::from(prds) << 16);
    if write {
        flags |= HEADER_WRITE;
    }
    let mut bytes = [0; 32];
    put(&mut bytes, 0, flags);
    // Byte 4: PRDBC, which the HBA writes.
    put(&mut bytes, 8, table as u32);
    put(&mut bytes, 12, (table >> 32) as u32);
    bytes
}

/// A Register Host to Device FIS carrying `command` for `lba` and `count`.
#[must_use]
pub fn fis(command: u8, lba: u64, count: u16) -> [u8; 20] {
    let lba = lba.to_le_bytes();
    let count = count.to_le_bytes();
    [
        FIS_H2D,
        FIS_COMMAND,
        command,
        0,
        lba[0],
        lba[1],
        lba[2],
        DEVICE_LBA,
        lba[3],
        lba[4],
        lba[5],
        0,
        count[0],
        count[1],
        0,
        0,
        0,
        0,
        0,
        0,
    ]
}

/// One PRDT entry: `len` bytes, even and at most [`PRD_MAX`], at `addr`.
#[must_use]
pub fn prd(addr: u64, len: u32) -> [u8; 16] {
    let mut bytes = [0; 16];
    put(&mut bytes, 0, addr as u32);
    put(&mut bytes, 4, (addr >> 32) as u32);
    put(&mut bytes, 12, len.wrapping_sub(1) & 0x003F_FFFF);
    bytes
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    if let Some(slot) = bytes.get_mut(at..at + 4) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}
