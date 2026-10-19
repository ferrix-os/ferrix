//! LZO, as btrfs frames it: one independent LZO1X stream per sector.
//!
//! LZO has no container format of its own, so btrfs wraps one. The layout is
//! the same for an inline extent and a regular one: a little-endian `u32`
//! holding the length of the whole compressed extent *including those four
//! bytes*, then one or more segments. A segment is a `u32` length and that
//! many bytes of LZO1X, and each is the compression of at most one sector of
//! the file. The stream itself is decoded by `lzo1x`; this module is the
//! framing around it.
//!
//! ```text
//! 0       4       8                   +len0          sector boundary
//! +-------+-------+-------------------+-------+------------- ~ ---+----+
//! | total | len0  | LZO1X of sector 0 | len1  | LZO1X of sector 1 | 00 |
//! +-------+-------+-------------------+-------+------------- ~ ---+----+
//!                                                 next header here ^
//! ```
//!
//! # Headers are aligned; segments are not
//!
//! Segments are packed end to end with no padding, whatever their length,
//! with one exception, and it is the reason this decoder needs `sectorsize`.
//! A segment *header* never straddles a sector boundary. When fewer than four
//! bytes are left before the next boundary (counting from the start of the
//! extent, not of the disk), the writer pads them with zeroes and starts the
//! header on the boundary. Linux reads each header through a mapping of one
//! sector-sized page, which is where the rule comes from. A reader that does
//! not apply it reads its next length out of the padding, and everything after
//! that is garbage that may still happen to parse.
//!
//! # Segments do not share a window
//!
//! Linux decompresses every segment into a scratch buffer of its own, so a
//! back-reference in one segment can never name a byte that an earlier segment
//! produced. No writer emits one, and Linux refuses one. That is narrower than
//! [`super`]'s rule that the whole output is the window, and it is enforced
//! structurally: the LZO1X decoder is handed only this segment's share of the
//! output, a subslice that starts where the segment's first byte goes and is
//! at most one sector long. A distance that reaches further back is then out
//! of bounds for the slice it has, rather than caught by a check that someone
//! has to remember. Accepting it would decode bytes Linux calls corruption,
//! and one file would read differently on the two kernels.
//!
//! # What is checked, and where Linux's checks come from
//!
//! - The total length has to cover its own header, fit the input and fit the
//!   128 KiB `BTRFS_MAX_COMPRESSED`, and it must not leave a whole sector of the
//!   input unused. Linux makes the last two checks in `lzo_decompress_bio`,
//!   because an extent whose header undercounts its own sectors is corrupt, not
//!   merely generous.
//! - A segment length may not exceed `lzo1x_worst_compress(sectorsize)`, the
//!   size of Linux's scratch buffer and the most one sector can grow to.
//! - A segment must end within the total length. Linux bounds it only by the
//!   input, which also holds the padding at the end of the last sector. The
//!   writer never puts a segment there, so reading past the total means
//!   reading bytes the header says are not part of the extent.
//! - A segment must decode to at most one sector, and into what is left of the
//!   output. Its stream must end in the end-of-stream marker, exactly at the
//!   segment's last byte. Linux's `lzo1x_decompress_safe` returns an error for
//!   input left over after the marker, and btrfs treats that error as EIO.
//!
//! Decoding stops when the total length is consumed or the output is full.
//! Output that is full at a segment boundary is the caller's range satisfied,
//! and Linux stops there too. A segment that overflows what is left is
//! corruption.

mod lzo1x;

#[cfg(test)]
mod tests;

use crate::items::COMPRESS_LZO;
use crate::{BtrfsError, is_valid_block_size, u32_at};

/// Bytes in the total-length header and in each segment header.
const LEN_SIZE: usize = 4;

/// The largest compressed extent btrfs writes, `BTRFS_MAX_COMPRESSED`.
const MAX_COMPRESSED: usize = 128 * 1024;

/// Smallest sector size accepted.
const MIN_SECTOR_SIZE: u32 = 512;

/// Largest sector size accepted.
const MAX_SECTOR_SIZE: u32 = 65536;

/// Expand a btrfs LZO extent into `output`, returning the bytes written.
///
/// `input` is the extent as it lies on disk: an inline item's payload, or a
/// regular extent's sectors, padding included. `sectorsize` is the
/// filesystem's; the framing is aligned to it and each segment expands to at
/// most one sector of it. A `sectorsize` that is not a power of two in
/// `512..=65536` is refused as [`BtrfsError::BadSectorSize`], and every
/// problem with the bytes is [`BtrfsError::BadCompressedData`].
pub fn decompress(input: &[u8], output: &mut [u8], sectorsize: u32) -> Result<usize, BtrfsError> {
    if !is_valid_block_size(sectorsize, MIN_SECTOR_SIZE, MAX_SECTOR_SIZE) {
        return Err(BtrfsError::BadSectorSize(sectorsize));
    }
    let sector = usize::try_from(sectorsize).map_err(|_| BtrfsError::BadSectorSize(sectorsize))?;
    let total = total_length(input, sector).ok_or_else(corrupt)?;

    let mut at = LEN_SIZE;
    let mut written = 0;
    // Each pass either stops or moves `at` forward by at least a header, and
    // `at` never passes `total`, so the loop runs at most `total / 4` times.
    while at < total && written < output.len() {
        at = header_position(at, sector).ok_or_else(corrupt)?;
        if at >= total {
            break;
        }
        let (segment, after) = segment_at(input, at, total, sector).ok_or_else(corrupt)?;
        written = decode_segment(segment, output, written, sector).ok_or_else(corrupt)?;
        at = after;
    }
    Ok(written)
}

/// The error every malformed stream reports.
fn corrupt() -> BtrfsError {
    BtrfsError::BadCompressedData {
        compression: COMPRESS_LZO,
    }
}

/// `lzo1x_worst_compress`: the most a block of `len` bytes can grow to under
/// LZO1X, which is a literal run's length prefix every 255 bytes or so, plus
/// the first byte and the end marker.
const fn worst_compress(len: usize) -> usize {
    len + len / 16 + 64 + 3 + 2
}

/// Read and check the total-length header at the front of `input`.
fn total_length(input: &[u8], sector: usize) -> Option<usize> {
    let total = usize::try_from(u32_at(input, 0)?).ok()?;
    let in_bounds = (LEN_SIZE..=input.len().min(MAX_COMPRESSED)).contains(&total);
    let uses_every_sector = total.checked_next_multiple_of(sector)? >= input.len();
    (in_bounds && uses_every_sector).then_some(total)
}

/// Where the segment header at or after `at` starts: `at` itself, or the next
/// sector boundary if fewer than [`LEN_SIZE`] bytes are left before it.
fn header_position(at: usize, sector: usize) -> Option<usize> {
    let left = sector.checked_sub(at.checked_rem(sector)?)?;
    if left < LEN_SIZE {
        at.checked_add(left)
    } else {
        Some(at)
    }
}

/// The segment whose header starts at `at`, and the offset just past it.
fn segment_at(input: &[u8], at: usize, total: usize, sector: usize) -> Option<(&[u8], usize)> {
    let len = usize::try_from(u32_at(input, at)?).ok()?;
    if len > worst_compress(sector) {
        return None;
    }
    let start = at.checked_add(LEN_SIZE)?;
    let end = start.checked_add(len)?;
    if end > total {
        return None;
    }
    Some((input.get(start..end)?, end))
}

/// Decode `segment` into `output` at `written`, returning the new total.
///
/// The window handed to the stream decoder is this segment's share of the
/// output and nothing else: it begins at `written`, so no back-reference can
/// reach an earlier segment's bytes, and it ends a sector later or at the end
/// of `output`, so a segment can overflow neither.
fn decode_segment(
    segment: &[u8],
    output: &mut [u8],
    written: usize,
    sector: usize,
) -> Option<usize> {
    let end = written.checked_add(sector)?.min(output.len());
    let window = output.get_mut(written..end)?;
    let produced = lzo1x::decompress(segment, window)?;
    written.checked_add(produced)
}
