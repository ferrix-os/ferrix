//! Sealed segments, compressed (`docs/LOGGING.md` §8.2): a segment's
//! entries in zstd frames of about 64 KiB each, with the segment's header
//! kept whole in front so a reader can tell what it is without unpacking.
//!
//! ```text
//!  0  magic "FXJZSTD1"           8  u32 version (1)
//! 12  u32 header length H       16  u64 the segment's length unpacked
//! 24  u32 CRC-32C of bytes 0..24 and the header
//! 28  u32 zero                  32  the segment's header, H bytes
//! then frames, each:
//!     u32 unpacked length       u32 packed length
//!     u32 CRC-32C of the packed bytes       u32 zero
//!     the packed bytes, one zstd frame of whole entries
//! ```
//!
//! Unpacked, a compressed segment is byte for byte the segment it was made
//! from, cut at its last good entry: the same header, the same entries at
//! the same offsets, so a reader -- a follower that kept an offset
//! included -- reads it exactly as it read the uncompressed one. A frame
//! whose checksum or length does not match ends the unpacking there, as a
//! torn entry ends a scan.
//!
//! [`Compressor`] packs a frame a call, so the service can seal a 16 MiB
//! segment between reads of the kernel log rather than in one stall.

use alloc::vec::Vec;

use ruzstd::decoding::FrameDecoder;
use ruzstd::encoding::{CompressionLevel, compress_to_vec};

use crate::crc::crc32c;
use crate::segment::{self, Header};

/// A compressed segment's first bytes.
pub const MAGIC: [u8; 8] = *b"FXJZSTD1";

/// The format's version.
pub const VERSION: u32 = 1;

/// The fixed part before the header.
const PREFIX: usize = 32;

/// A frame's own bytes before its packed ones.
const FRAME_HEAD: usize = 16;

/// The unpacked bytes a frame aims at; one entry larger is a frame alone.
pub const FRAME_RAW: usize = 64 * 1024;

/// The most a frame may say it unpacks to: the largest entry and a frame.
const FRAME_RAW_MAX: usize = segment::ENTRY_MAX + FRAME_RAW;

/// Whether `bytes` are a compressed segment's.
#[must_use]
pub fn is_compressed(bytes: &[u8]) -> bool {
    bytes.get(..8) == Some(&MAGIC[..])
}

/// The segment header in `bytes`, a segment's first bytes, compressed or not.
#[must_use]
pub fn header_bytes(bytes: &[u8]) -> &[u8] {
    if !is_compressed(bytes) {
        return bytes;
    }
    let length = u32_at(bytes, 12).map_or(0, |length| length as usize);
    bytes
        .get(PREFIX..PREFIX.saturating_add(length))
        .unwrap_or_default()
}

/// The segment header of `bytes`, compressed or not.
#[must_use]
pub fn header(bytes: &[u8]) -> Option<Header> {
    Header::decode(header_bytes(bytes)).map(|(header, _)| header)
}

/// A segment's bytes as a reader wants them: unpacked if compressed (to
/// the last good frame), else as they are.
#[must_use]
pub fn expand(bytes: Vec<u8>) -> Vec<u8> {
    if is_compressed(&bytes) {
        unpack(&bytes).unwrap_or_default()
    } else {
        bytes
    }
}

/// A compressed segment unpacked, to the last frame that reads; `None` for
/// one whose header does not.
#[must_use]
pub fn unpack(bytes: &[u8]) -> Option<Vec<u8>> {
    if !is_compressed(bytes) || u32_at(bytes, 8)? != VERSION {
        return None;
    }
    let header_len = usize::try_from(u32_at(bytes, 12)?).ok()?;
    let total = usize::try_from(u64_at(bytes, 16)?).ok()?;
    let header = bytes.get(PREFIX..PREFIX.checked_add(header_len)?)?;
    if prefix_crc(bytes.get(..24)?, header) != u32_at(bytes, 24)? {
        return None;
    }
    let _ = Header::decode(header)?;
    let mut out = Vec::with_capacity(total.min(64 * 1024 * 1024));
    out.extend_from_slice(header);
    let mut at = PREFIX + header_len;
    let mut decoder = FrameDecoder::new();
    while at < bytes.len() && out.len() < total {
        let (Some(raw), Some(packed), Some(crc)) = (
            u32_at(bytes, at).and_then(|n| usize::try_from(n).ok()),
            u32_at(bytes, at + 4).and_then(|n| usize::try_from(n).ok()),
            u32_at(bytes, at + 8),
        ) else {
            break;
        };
        let Some(data) = at
            .checked_add(FRAME_HEAD)
            .and_then(|start| bytes.get(start..start.checked_add(packed)?))
        else {
            break;
        };
        if raw == 0 || raw > FRAME_RAW_MAX || crc32c(data) != crc {
            break;
        }
        let mut frame = Vec::with_capacity(raw);
        if decoder.decode_all_to_vec(data, &mut frame).is_err() || frame.len() != raw {
            break;
        }
        out.extend_from_slice(&frame);
        at += FRAME_HEAD + packed;
    }
    Some(out)
}

/// A segment being compressed, a frame at a time.
#[derive(Debug)]
pub struct Compressor {
    /// The segment's bytes, cut at the last good entry.
    raw: Vec<u8>,
    /// Where each entry ends, in order.
    ends: Vec<usize>,
    /// The next entry to pack.
    next: usize,
    /// Where it begins.
    at: usize,
    /// What is packed so far, the header first.
    out: Vec<u8>,
}

impl Compressor {
    /// Begin on a segment's bytes; `None` for bytes that are not an
    /// uncompressed segment.
    #[must_use]
    pub fn new(mut raw: Vec<u8>) -> Option<Compressor> {
        if is_compressed(&raw) {
            return None;
        }
        let scanned = segment::scan(&raw, 0)?;
        let (_, header_len) = Header::decode(&raw)?;
        raw.truncate(scanned.good);
        let ends = scanned
            .records
            .iter()
            .map(|found| found.offset + found.length)
            .collect();
        let header = raw.get(..header_len)?;
        let mut out = Vec::with_capacity(raw.len() / 4);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&u32::try_from(header_len).ok()?.to_le_bytes());
        out.extend_from_slice(&u64::try_from(raw.len()).ok()?.to_le_bytes());
        let crc = prefix_crc(&out, header);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&0_u32.to_le_bytes());
        out.extend_from_slice(header);
        Some(Compressor {
            at: header_len,
            raw,
            ends,
            next: 0,
            out,
        })
    }

    /// Pack the next frame. Answers whether every entry is packed.
    pub fn step(&mut self) -> bool {
        let start = self.at;
        while let Some(&end) = self.ends.get(self.next) {
            if end - start > FRAME_RAW && self.at > start {
                break;
            }
            self.at = end;
            self.next += 1;
        }
        if self.at > start
            && let Some(raw) = self.raw.get(start..self.at)
        {
            let packed = compress_to_vec(raw, CompressionLevel::Fastest);
            for number in [
                u32::try_from(raw.len()).unwrap_or(0),
                u32::try_from(packed.len()).unwrap_or(0),
                crc32c(&packed),
                0,
            ] {
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            self.out.extend_from_slice(&packed);
        }
        self.next >= self.ends.len()
    }

    /// The unpacked length: what the compressed segment will unpack to.
    #[must_use]
    pub fn raw_len(&self) -> usize {
        self.raw.len()
    }

    /// The compressed segment.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// A segment compressed whole; `None` for bytes that are not an
/// uncompressed segment.
#[must_use]
pub fn compress(raw: Vec<u8>) -> Option<Vec<u8>> {
    let mut compressor = Compressor::new(raw)?;
    while !compressor.step() {}
    Some(compressor.finish())
}

/// The CRC the prefix carries: of its first 24 bytes and the header.
fn prefix_crc(first: &[u8], header: &[u8]) -> u32 {
    let mut both = Vec::with_capacity(first.len() + header.len());
    both.extend_from_slice(first);
    both.extend_from_slice(header);
    crc32c(&both)
}

/// The little-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// The little-endian `u64` at `at`.
fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}
