//! CRC-32C (Castagnoli), the checksum every segment entry carries.
//!
//! The polynomial btrfs and iSCSI use, reflected (0x82F63B78), initial value
//! and final XOR all ones: `crc32c(b"123456789") == 0xE306_9283`.

/// The reflected polynomial.
const POLY: u32 = 0x82F6_3B78;

/// The byte-at-a-time table, made at compile time.
const TABLE: [u32; 256] = table();

/// Build [`TABLE`].
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: `byte` stays below 256, the table's length; `get_mut` is not const here"
)]
const fn table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// CRC-32C of `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        let index = usize::from((crc as u8) ^ byte);
        crc = (crc >> 8) ^ TABLE.get(index).copied().unwrap_or(0);
    }
    !crc
}
