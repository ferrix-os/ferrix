//! Decompression of extent data: zlib, LZO and zstd, as btrfs frames them.
//!
//! An `EXTENT_DATA` item names its algorithm in `compression`, and the bytes it
//! points at — inline in the item or in an extent elsewhere — are one
//! compressed stream that expands to at most `ram_bytes`. btrfs caps a
//! compressed extent at 128 KiB of uncompressed data, so the caller always
//! knows an upper bound on the output before it starts, and every decoder here
//! writes into a caller-supplied buffer rather than allocating.
//!
//! # The same rules as the rest of the crate
//!
//! The input is whatever is on the disk. Each decoder is total — any input at
//! all produces a length or a [`BtrfsError`], never a panic, an out-of-bounds
//! write or a loop that does not terminate — and is written without `unsafe`.
//! Back-references are resolved against the output buffer itself, which is
//! also the window: an extent is decompressed whole, so every distance a
//! well-formed stream can name lies inside what has already been written, and
//! one that does not is corrupt.
//!
//! # A short result is not an error
//!
//! A decoder returns how many bytes it produced. That can be fewer than
//! `output.len()`: Linux zero-fills the rest of the range, and so must the
//! caller. Producing *more* than fits is corruption, reported as
//! [`BtrfsError::BadCompressedData`].

pub mod lzo;
pub mod zlib;
pub mod zstd;

use crate::BtrfsError;
use crate::items::{COMPRESS_LZO, COMPRESS_NONE, COMPRESS_ZLIB, COMPRESS_ZSTD};

/// The largest uncompressed size of one compressed extent, in bytes.
///
/// This is `BTRFS_MAX_UNCOMPRESSED` in Linux. A caller that sizes its output
/// buffer to this never needs a second one.
pub const MAX_UNCOMPRESSED: usize = 128 * 1024;

/// Expand `input`, compressed with algorithm `compression`, into `output`.
///
/// Returns the number of bytes written. `sectorsize` is the filesystem's, which
/// LZO's framing is aligned to; `workspace` holds zstd's tables and literal
/// buffer, which are too large to put on a kernel stack.
///
/// [`COMPRESS_NONE`] is accepted and copies, so a caller can route every
/// extent through here without a special case.
pub fn decompress(
    compression: u8,
    input: &[u8],
    output: &mut [u8],
    sectorsize: u32,
    workspace: &mut zstd::Workspace<'_>,
) -> Result<usize, BtrfsError> {
    match compression {
        COMPRESS_NONE => {
            let len = input.len().min(output.len());
            if let (Some(to), Some(from)) = (output.get_mut(..len), input.get(..len)) {
                to.copy_from_slice(from);
            }
            Ok(len)
        }
        COMPRESS_ZLIB => zlib::decompress(input, output),
        COMPRESS_LZO => lzo::decompress(input, output, sectorsize),
        COMPRESS_ZSTD => zstd::decompress(input, output, workspace),
        other => Err(BtrfsError::UnsupportedCompression(other)),
    }
}
