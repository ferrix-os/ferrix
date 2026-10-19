//! zlib (RFC 1950) around DEFLATE (RFC 1951), as btrfs stores it.
//!
//! # What is on the disk
//!
//! Linux compresses each extent — at most [`super::MAX_UNCOMPRESSED`] of file
//! data — into one zlib stream: a two-byte header, a run of DEFLATE blocks, and
//! a four-byte Adler-32 of the plaintext. An inline extent's payload is exactly
//! that stream. A regular extent is rounded up to a whole sector, so its stream
//! is followed by zeros up to the sector boundary, and nothing outside the
//! stream records where it ends. The decoder therefore stops at the end of the
//! block marked final, and whatever follows — trailer, padding, anything — is
//! never read.
//!
//! # Reading what Linux reads, and no less
//!
//! `fs/btrfs/zlib.c` does not hand the stream to zlib whole. It checks the
//! header itself (method 8, no preset dictionary, `(CMF << 8 | FLG) % 31 ==
//! 0`), skips the two bytes, and inflates what follows as *raw* DEFLATE with a
//! negative `windowBits`. A raw inflate has no trailer, so Linux never looks at
//! the Adler-32, and neither does this decoder. That is deliberate, not an
//! omission, for two reasons:
//!
//! - The bytes are already guarded. A regular extent is covered by btrfs's data
//!   checksum over the compressed bytes as stored, and an inline one by the
//!   checksum of the leaf that holds it; both are verified before anything is
//!   decompressed and both catch strictly more than Adler-32 would.
//! - A reader stricter than Linux refuses a file that the kernel which wrote it,
//!   and every tool built on that kernel, reads without complaint. A trailer
//!   mismatch Linux has never checked may well exist on real volumes, and a
//!   mount that fails over it is a bug report nobody can act on.
//!
//! A header Linux's check refuses falls through, in Linux, to a full zlib
//! inflate, which refuses the same header again — or asks for a dictionary
//! btrfs never supplies — so every such header is an I/O error there and
//! [`BtrfsError::BadCompressedData`] here. The window size in the header's
//! `CINFO` must be at most 32 KiB, because Linux passes it to
//! `zlib_inflateInit2`, which rejects anything larger.
//!
//! Past the header, the window is the output buffer itself, as for every
//! decoder in [`super`]: a distance is refused only when it reaches before the
//! first byte written. For `CINFO = 7`, which every encoder writes, that is
//! Linux's rule exactly. A smaller declared window Linux enforces only where a
//! match crosses one of its own page-sized output chunks, a limit that depends
//! on the page size rather than the format, so it is not reproduced.
//!
//! # DEFLATE
//!
//! A stream is a sequence of blocks, each opened by a final-block bit and a
//! two-bit type: stored (a length, its complement, and raw bytes), fixed
//! Huffman (codes from the RFC's table) or dynamic Huffman (codes described by
//! the block's own header). Type 3 is reserved and is an error. Huffman-coded
//! blocks are a stream of literal bytes and (length, distance) back-references
//! into what has already been produced, ended by symbol 256. The pieces live in
//! submodules: the bit order in `bits`, code tables in `huffman`, the block
//! formats in `block`, and the output window in `output`.
//!
//! Every check zlib makes on a stream is made here, because anything zlib
//! refuses Linux refuses, and a check skipped is a code path that runs on
//! garbage: over-subscribed and incomplete code sets, literal/length symbols
//! 286 and 287, distance symbols 30 and 31, more than 286 or 30 of them
//! declared, a code-length repeat with nothing to repeat or running past the
//! end, a stored block whose `LEN` is not `!NLEN`, a dynamic block with no
//! end-of-block code, and input that ends before the final block does.
//!
//! # Stack
//!
//! Kernel stacks are small, and the decoding tables live on them. The largest
//! frame set is a dynamic block: 316 code lengths, a literal/length table of
//! 1632 bytes, a distance table of 608 and a code-length table of 326, a little
//! under 3 KiB of data before frame overhead. Each table is a canonical-code
//! description (a count per length and the symbols in code order) plus a
//! direct-lookup array for the short codes that dominate real data; a longer
//! code is resolved by walking the counts, which needs no further storage.
//! Tables are built in place, never returned by value, so no build step holds
//! a second copy, and the fixed and dynamic block decoders are kept out of line
//! so an optimiser cannot merge their frames into one that reserves both sets.
//! Measured by stack painting over the mkfs vectors, the whole decode peaks
//! near 2.8 KiB at the kernel's optimisation levels and 5.3 KiB at none.

mod bits;
mod block;
mod huffman;
mod output;

#[cfg(test)]
mod tests;

use crate::BtrfsError;
use crate::items::COMPRESS_ZLIB;
use bits::Bits;
use output::Output;

/// `CM`, the low nibble of the first header byte: 8 is DEFLATE, the only
/// method RFC 1950 defines.
const METHOD_DEFLATE: u8 = 8;

/// The largest `CINFO`, the high nibble of the first header byte. The window is
/// `1 << (CINFO + 8)` bytes, so 7 is DEFLATE's 32 KiB.
const MAX_WINDOW_LOG: u8 = 7;

/// `FDICT` in the second header byte: a preset dictionary identifier follows.
/// btrfs never writes one and Linux cannot supply one.
const PRESET_DICTIONARY: u8 = 0x20;

/// Bytes in the zlib header that precedes the DEFLATE data.
const HEADER_SIZE: usize = 2;

/// Expand a zlib stream into `output`, returning the bytes written.
///
/// `input` is the extent as stored: the stream, and for a regular extent the
/// zeros that pad it to a sector. Fewer bytes than `output.len()` is a success
/// the caller zero-fills; a stream that would write more than fits, a malformed
/// header, and any malformed DEFLATE data are all
/// [`BtrfsError::BadCompressedData`]. The Adler-32 trailer is not checked; see
/// the module documentation for why.
pub fn decompress(input: &[u8], output: &mut [u8]) -> Result<usize, BtrfsError> {
    inflate(input, output).ok_or(BtrfsError::BadCompressedData {
        compression: COMPRESS_ZLIB,
    })
}

/// Check the header, then decode blocks until the final one ends.
///
/// Terminates on any input: every pass consumes at least the three bits of a
/// block header, and the input is finite.
fn inflate(input: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut bits = Bits::new(input.get(check_header(input)?..)?);
    let mut out = Output::new(output);
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => block::stored(&mut bits, &mut out)?,
            1 => block::fixed(&mut bits, &mut out)?,
            2 => block::dynamic(&mut bits, &mut out)?,
            _ => return None,
        }
        if last {
            return Some(out.written());
        }
    }
}

/// Validate the two-byte zlib header, returning where the DEFLATE data starts.
///
/// These are the conditions under which Linux skips the header and inflates
/// raw, plus the window bound its `zlib_inflateInit2` imposes.
fn check_header(input: &[u8]) -> Option<usize> {
    let cmf = *input.first()?;
    let flg = *input.get(1)?;
    let check = (u16::from(cmf) << 8) | u16::from(flg);
    let valid = cmf & 0x0F == METHOD_DEFLATE
        && cmf >> 4 <= MAX_WINDOW_LOG
        && flg & PRESET_DICTIONARY == 0
        && check % 31 == 0;
    valid.then_some(HEADER_SIZE)
}
