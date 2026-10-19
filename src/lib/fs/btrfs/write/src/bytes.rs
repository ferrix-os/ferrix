//! Little-endian field access for the structures this crate builds.
//!
//! The same discipline as `ferrix-btrfs`: every access is checked, and a field
//! that does not fit is `None` rather than a panic. The writers exist because
//! this crate builds structures, and a builder that indexes would be the one
//! place a wrong length becomes a crash instead of an error.

use ferrix_btrfs::tree::{BtrfsKey, KEY_SIZE};

/// Copy `bytes` into `buf` at `at`, or `None` if they do not fit.
pub(crate) fn put(buf: &mut [u8], at: usize, bytes: &[u8]) -> Option<()> {
    buf.get_mut(at..at.checked_add(bytes.len())?)?
        .copy_from_slice(bytes);
    Some(())
}

/// Store a little-endian `u64` at `at`.
pub(crate) fn put_u64(buf: &mut [u8], at: usize, value: u64) -> Option<()> {
    put(buf, at, &value.to_le_bytes())
}

/// Store a little-endian `u32` at `at`.
pub(crate) fn put_u32(buf: &mut [u8], at: usize, value: u32) -> Option<()> {
    put(buf, at, &value.to_le_bytes())
}

/// Store a little-endian `u16` at `at`.
pub(crate) fn put_u16(buf: &mut [u8], at: usize, value: u16) -> Option<()> {
    put(buf, at, &value.to_le_bytes())
}

/// Store one byte at `at`.
pub(crate) fn put_u8(buf: &mut [u8], at: usize, value: u8) -> Option<()> {
    put(buf, at, &[value])
}

/// Store a key's 17 bytes at `at`.
pub(crate) fn put_key(buf: &mut [u8], at: usize, key: &BtrfsKey) -> Option<()> {
    put(buf, at, &key_bytes(key))
}

/// A key as it is laid out on disk.
pub(crate) fn key_bytes(key: &BtrfsKey) -> [u8; KEY_SIZE] {
    let mut out = [0u8; KEY_SIZE];
    let object = key.objectid.to_le_bytes();
    let offset = key.offset.to_le_bytes();
    for (to, from) in out.iter_mut().zip(object.iter()) {
        *to = *from;
    }
    if let Some(kind) = out.get_mut(8) {
        *kind = key.item_type;
    }
    for (to, from) in out.iter_mut().skip(9).zip(offset.iter()) {
        *to = *from;
    }
    out
}

/// Read a little-endian `u64` at `at`.
pub(crate) fn get_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Read a little-endian `u32` at `at`.
pub(crate) fn get_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Read one byte at `at`.
pub(crate) fn get_u8(bytes: &[u8], at: usize) -> Option<u8> {
    bytes.get(at).copied()
}
