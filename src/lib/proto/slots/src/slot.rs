//! A system slot's header and blobs (`docs/AB-BOOT.md` §2).
//!
//! A slot is a raw partition: a 4096-byte header, then the kernel, the
//! initramfs and the command line, each on a 4096-byte boundary. The header
//! names each blob's place, length and SHA-256, and seals itself with a
//! SHA-256 of its own bytes.

use crate::sha256::digest;
use crate::{le16, le32, le64, put};

/// What every slot header begins with.
pub const MAGIC: [u8; 8] = *b"FXSLOTHD";
/// The format this crate writes and reads.
pub const VERSION: u16 = 1;
/// Bytes in the header, and the alignment of every blob.
pub const HEADER_LEN: usize = 4096;
/// The longest command line a slot may carry.
pub const MAX_CMDLINE: u64 = 2048;
/// Blobs a version 1 header names.
const BLOBS: u32 = 3;
/// Where the blob descriptors start; each is 48 bytes.
const BLOB_AT: usize = 56;
/// Bytes per blob descriptor.
const BLOB_LEN: usize = 48;
/// The first byte after the descriptors; zero from here to the seal.
const FIELDS_END: usize = BLOB_AT + 3 * BLOB_LEN;
/// Where the seal is: the SHA-256 of every byte before it.
const SEAL_AT: usize = HEADER_LEN - 32;
/// The alignment of a blob, as a `u64`.
const ALIGN: u64 = HEADER_LEN as u64;

/// Where one blob is, and what it hashes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Blob {
    /// Bytes from the start of the slot.
    pub offset: u64,
    /// Bytes long; zero for an initramfs the slot does not carry.
    pub length: u64,
    /// SHA-256 of its bytes.
    pub sha256: [u8; 32],
}

impl Blob {
    /// A blob of `bytes` at `offset`.
    #[must_use]
    pub fn of(offset: u64, bytes: &[u8]) -> Blob {
        Blob {
            offset,
            length: bytes.len() as u64,
            sha256: digest(bytes),
        }
    }

    /// Whether `bytes` are this blob's.
    #[must_use]
    pub fn matches(&self, bytes: &[u8]) -> bool {
        bytes.len() as u64 == self.length && digest(bytes) == self.sha256
    }

    /// One past its last byte.
    fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.length)
    }
}

/// A slot's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotHeader {
    /// The record sequence `bootctl` armed the slot under; informational.
    pub generation: u64,
    /// What is in it, NUL padded; informational.
    pub label: [u8; 32],
    /// The kernel ELF.
    pub kernel: Blob,
    /// The initramfs, or a zero-length blob.
    pub initrd: Blob,
    /// The command line, UTF-8.
    pub cmdline: Blob,
}

/// Why a slot header does not verify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotError {
    /// Fewer than [`HEADER_LEN`] bytes.
    Short,
    /// Not [`MAGIC`]; an empty slot is this.
    Magic,
    /// Another version, header length or blob count.
    Version,
    /// The seal does not match.
    Seal,
    /// A reserved byte is not zero.
    Reserved,
    /// A blob is misaligned, overlaps the header or another blob, or ends
    /// past the slot.
    Placement,
    /// No kernel, or a command line longer than [`MAX_CMDLINE`].
    Length,
}

impl SlotHeader {
    /// The header for these three blobs laid out one after the other, each
    /// on a 4096-byte boundary after the header.
    #[must_use]
    pub fn lay_out(
        generation: u64,
        label: &str,
        kernel: &[u8],
        initrd: &[u8],
        cmdline: &[u8],
    ) -> SlotHeader {
        let next = |after: u64| after.div_ceil(ALIGN).saturating_mul(ALIGN);
        let kernel = Blob::of(ALIGN, kernel);
        let initrd = Blob::of(next(kernel.end().unwrap_or(u64::MAX)), initrd);
        let cmdline = Blob::of(next(initrd.end().unwrap_or(u64::MAX)), cmdline);
        let mut name = [0u8; 32];
        for (to, from) in name.iter_mut().zip(label.bytes().take(31)) {
            *to = from;
        }
        SlotHeader {
            generation,
            label: name,
            kernel,
            initrd,
            cmdline,
        }
    }

    /// The blobs in their order: kernel, initramfs, command line.
    #[must_use]
    pub const fn blobs(&self) -> [Blob; 3] {
        [self.kernel, self.initrd, self.cmdline]
    }

    /// Bytes the slot needs: one past the end of its last blob.
    #[must_use]
    pub fn needed(&self) -> u64 {
        self.blobs()
            .iter()
            .filter_map(Blob::end)
            .max()
            .unwrap_or(ALIGN)
    }

    /// The header's bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        put(&mut out, 0, &MAGIC);
        put(&mut out, 8, &VERSION.to_le_bytes());
        put(&mut out, 10, &(HEADER_LEN as u16).to_le_bytes());
        put(&mut out, 12, &BLOBS.to_le_bytes());
        put(&mut out, 16, &self.generation.to_le_bytes());
        put(&mut out, 24, &self.label);
        for (index, blob) in self.blobs().iter().enumerate() {
            let at = BLOB_AT + index * BLOB_LEN;
            put(&mut out, at, &blob.offset.to_le_bytes());
            put(&mut out, at + 8, &blob.length.to_le_bytes());
            put(&mut out, at + 16, &blob.sha256);
        }
        let seal = digest(out.get(..SEAL_AT).unwrap_or_default());
        put(&mut out, SEAL_AT, &seal);
        out
    }

    /// Read and check a header from the first [`HEADER_LEN`] bytes of
    /// `bytes`, for a slot of `slot_bytes` bytes.
    ///
    /// A header that decodes says where the blobs are and what they hash
    /// to; the slot verifies when each blob's bytes then match
    /// ([`Blob::matches`]).
    pub fn decode(bytes: &[u8], slot_bytes: u64) -> Result<SlotHeader, SlotError> {
        let bytes = bytes.get(..HEADER_LEN).ok_or(SlotError::Short)?;
        if bytes.get(..8) != Some(&MAGIC[..]) {
            return Err(SlotError::Magic);
        }
        if le16(bytes, 8) != Some(VERSION)
            || le16(bytes, 10) != Some(HEADER_LEN as u16)
            || le32(bytes, 12) != Some(BLOBS)
        {
            return Err(SlotError::Version);
        }
        let covered = bytes.get(..SEAL_AT).ok_or(SlotError::Short)?;
        if bytes.get(SEAL_AT..) != Some(&digest(covered)[..]) {
            return Err(SlotError::Seal);
        }
        if !bytes
            .get(FIELDS_END..SEAL_AT)
            .is_some_and(|rest| rest.iter().all(|&byte| byte == 0))
        {
            return Err(SlotError::Reserved);
        }
        let blob = |index: usize| -> Result<Blob, SlotError> {
            let at = BLOB_AT + index * BLOB_LEN;
            let sha256 = bytes
                .get(at + 16..at + 48)
                .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
                .ok_or(SlotError::Short)?;
            Ok(Blob {
                offset: le64(bytes, at).ok_or(SlotError::Short)?,
                length: le64(bytes, at + 8).ok_or(SlotError::Short)?,
                sha256,
            })
        };
        let label = bytes
            .get(24..56)
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
            .ok_or(SlotError::Short)?;
        let header = SlotHeader {
            generation: le64(bytes, 16).ok_or(SlotError::Short)?,
            label,
            kernel: blob(0)?,
            initrd: blob(1)?,
            cmdline: blob(2)?,
        };
        header.check_placement(slot_bytes)?;
        if header.kernel.length == 0 || header.cmdline.length > MAX_CMDLINE {
            return Err(SlotError::Length);
        }
        Ok(header)
    }

    /// Every blob aligned, after the header, inside the slot, and apart.
    fn check_placement(&self, slot_bytes: u64) -> Result<(), SlotError> {
        let blobs = self.blobs();
        for (index, blob) in blobs.iter().enumerate() {
            let end = blob.end().ok_or(SlotError::Placement)?;
            if blob.offset < ALIGN || blob.offset % ALIGN != 0 || end > slot_bytes {
                return Err(SlotError::Placement);
            }
            let overlaps = blobs.iter().skip(index + 1).any(|other| {
                other.end().is_none_or(|other_end| {
                    blob.length != 0
                        && other.length != 0
                        && blob.offset < other_end
                        && other.offset < end
                })
            });
            if overlaps {
                return Err(SlotError::Placement);
            }
        }
        Ok(())
    }

    /// The label as text, up to its first NUL; empty when it is not UTF-8.
    #[must_use]
    pub fn label_text(&self) -> &str {
        let end = self
            .label
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(self.label.len());
        self.label
            .get(..end)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }
}
