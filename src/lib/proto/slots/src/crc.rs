//! CRC-32 (the GPT's, IEEE 802.3) and CRC-32C (Castagnoli, the record's).
//!
//! Bitwise, with no table: the record is 508 bytes and a GPT's entry array
//! 16 KiB, read once a boot.

/// The reflected IEEE polynomial.
const IEEE: u32 = 0xEDB8_8320;

/// The reflected Castagnoli polynomial.
const CASTAGNOLI: u32 = 0x82F6_3B78;

/// A reflected CRC-32 with `polynomial`, initial and final value all ones.
fn reflected(polynomial: u32, bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (polynomial & mask);
        }
    }
    !crc
}

/// CRC-32 as the GUID partition table uses it.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    reflected(IEEE, bytes)
}

/// CRC-32C, the control record's checksum.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    reflected(CASTAGNOLI, bytes)
}
