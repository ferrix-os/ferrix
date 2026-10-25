//! A system slot's header and blobs (`docs/AB-BOOT.md` §2).
//!
//! A slot is a raw partition: a 4096-byte header, then the kernel, the
//! initramfs and the command line, each on a 4096-byte boundary. The header
//! names each blob's place, length and SHA-256, and seals itself with a
//! SHA-256 of its own bytes.
//!
//! Header version 2 (`docs/SIGNING.md` §1.3) adds a signed manifest block
//! at bytes 4096..8192, puts every blob at or after 8192, and holds
//! `generation` at zero, so that the seal the manifest signs is the same on
//! every device the release is written to (§1.2).

use crate::sha256::digest;
use crate::{le16, le32, le64, put};

/// What every slot header begins with.
pub const MAGIC: [u8; 8] = *b"FXSLOTHD";
/// The unsigned format: the header, then the blobs.
pub const VERSION_1: u16 = 1;
/// The signed format: the header, the manifest block, then the blobs.
pub const VERSION_2: u16 = 2;
/// Where a version-2 slot's manifest block starts.
pub const MANIFEST_AT: u64 = 4096;
/// The manifest block's length.
pub const MANIFEST_LEN: usize = 4096;
/// Where a version-2 slot's first blob may start.
const BLOBS_FROM_2: u64 = 8192;
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
    /// [`VERSION_1`] or [`VERSION_2`].
    pub version: u16,
    /// The record sequence `bootctl` armed the slot under; informational,
    /// and zero in version 2.
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
    /// A version-2 header whose `generation` is not zero.
    Generation,
}

impl SlotHeader {
    /// The header for these three blobs laid out one after the other, each
    /// on a 4096-byte boundary after the header (version 1) or after the
    /// manifest block (version 2, whose `generation` is then zero).
    #[must_use]
    pub fn lay_out(
        version: u16,
        generation: u64,
        label: &str,
        kernel: &[u8],
        initrd: &[u8],
        cmdline: &[u8],
    ) -> SlotHeader {
        let next = |after: u64| after.div_ceil(ALIGN).saturating_mul(ALIGN);
        let signed = version == VERSION_2;
        let first = if signed { BLOBS_FROM_2 } else { ALIGN };
        let kernel = Blob::of(first, kernel);
        let initrd = Blob::of(next(kernel.end().unwrap_or(u64::MAX)), initrd);
        let cmdline = Blob::of(next(initrd.end().unwrap_or(u64::MAX)), cmdline);
        let mut name = [0u8; 32];
        for (to, from) in name.iter_mut().zip(label.bytes().take(31)) {
            *to = from;
        }
        SlotHeader {
            version: if signed { VERSION_2 } else { VERSION_1 },
            generation: if signed { 0 } else { generation },
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
        put(&mut out, 8, &self.version.to_le_bytes());
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
        let version = le16(bytes, 8).ok_or(SlotError::Short)?;
        if !(version == VERSION_1 || version == VERSION_2)
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
        let generation = le64(bytes, 16).ok_or(SlotError::Short)?;
        if version == VERSION_2 && generation != 0 {
            return Err(SlotError::Generation);
        }
        let header = SlotHeader {
            version,
            generation,
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

    /// The seal: the SHA-256 of header bytes 0..4064, which a version-2
    /// slot's manifest signs as its payload digest (`docs/SIGNING.md` §1.2).
    #[must_use]
    pub fn seal(&self) -> [u8; 32] {
        let bytes = self.encode();
        let mut seal = [0u8; 32];
        for (to, from) in seal.iter_mut().zip(bytes.iter().skip(SEAL_AT)) {
            *to = *from;
        }
        seal
    }

    /// Every blob aligned, after the header (and the manifest block in
    /// version 2), inside the slot, and apart.
    fn check_placement(&self, slot_bytes: u64) -> Result<(), SlotError> {
        let blobs = self.blobs();
        let first = if self.version == VERSION_2 {
            BLOBS_FROM_2
        } else {
            ALIGN
        };
        for (index, blob) in blobs.iter().enumerate() {
            let end = blob.end().ok_or(SlotError::Placement)?;
            if blob.offset < first || blob.offset % ALIGN != 0 || end > slot_bytes {
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
