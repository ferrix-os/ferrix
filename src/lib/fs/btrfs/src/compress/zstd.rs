//! Zstandard (RFC 8878), as btrfs frames it.
//!
//! A zstd *frame* is a header, a run of *blocks* and an optional checksum. A
//! block is raw bytes, one byte repeated, or compressed; a compressed block is
//! a *literals section* — the bytes no match could express, often Huffman
//! coded — followed by a *sequences section*, whose sequences say "copy this
//! many literals, then copy this many bytes from that far back". The lengths
//! and offsets are FSE-coded, and both the Huffman and FSE tables can be
//! carried over from the previous block of the same frame. The submodules
//! follow that structure: `literals` and `huffman`, `sequences` and
//! `fse`, with `bits` underneath and `window` as the destination.
//!
//! # One frame, then stop
//!
//! btrfs compresses each extent (at most 128 KiB of file data) as one frame
//! and stores it as is: inline in the `EXTENT_DATA` item, or in a regular
//! extent rounded up to a sector with zeros. Linux decodes by streaming until
//! `zstd_decompress_stream` reports the frame finished, or the output is
//! full, and never looks at what follows. So does this decoder: it decodes
//! exactly one frame and ignores everything after it. Treating the padding as
//! the start of a second frame would make every regular extent corrupt, and
//! trying to tell padding from a real second frame would be guessing about
//! bytes Linux never reads. Leading *skippable* frames are stepped over, since
//! that costs nothing and is what the format says a decoder does with them.
//!
//! A frame naming a dictionary is refused: btrfs never uses one, so a non-zero
//! dictionary ID is a corrupt header, not a missing feature.
//!
//! # The output is the window
//!
//! The frame decompresses straight into the caller's buffer, and every match
//! is resolved against that buffer; a distance reaching before its start is
//! corruption. The header's window size is therefore read and ignored. It
//! neither needs to be enforced — every reference is bounds-checked against
//! what is really there — nor can it be trusted to size anything. A frame whose
//! declared content size exceeds the buffer, or whose blocks would write past
//! it, is refused rather than truncated: a short extent is a normal result, an
//! extent that does not fit is a corrupt one.
//!
//! # Totality
//!
//! Every field this decoder reads from the frame is bounded before it becomes
//! a size or an index: accuracy logs, symbol values, Huffman weights, stream
//! and section sizes, sequence counts and lengths. The internal functions
//! return `Option`, so the path from a hostile field to the single
//! [`BtrfsError::BadCompressedData`] at [`decompress`] is a `?` on every
//! fallible step, with no panicking accessor anywhere between. Every loop
//! either consumes input, produces output into a bounded buffer, or walks a
//! table of bounded size, so every loop ends.
//!
//! # Workspace
//!
//! The tables and literal buffer need 141 KiB ([`Workspace::SIZE`]), nearly
//! all of it the literal buffer, which must hold a whole block's literals
//! because they are decoded before the sequences that interleave them. That is
//! far more than a kernel stack, so the caller supplies it as a plain byte
//! buffer: bytes rather than typed tables so that constructing a [`Workspace`]
//! builds nothing on the stack and any page allocator can provide it. Its
//! contents on entry are irrelevant — nothing read from it is trusted until
//! this decode has written it — so it needs no zeroing and can be reused for
//! every extent. What decoding itself keeps on the stack is small arrays
//! bounded by the format — an FSE count array and its companion (512 bytes
//! each), 256 Huffman weights and the 256-byte table that decodes them. With
//! `-Z emit-stack-sizes` on an optimised x86-64 build the largest single frame
//! is `fse::build`, at 1 096 bytes, and a whole 128 KiB extent decodes on a
//! 16 KiB thread; a test holds the decode to 32 KiB so a table creeping back
//! onto the stack is caught.

mod bits;
mod fse;
mod huffman;
mod literals;
mod sequences;
mod window;
mod xxh64;

use core::fmt;

use crate::items::COMPRESS_ZSTD;
use crate::{BtrfsError, slice_at, u8_at, u32_at};
use literals::HuffmanSlot;
use sequences::{FseSlot, Slots};
use window::Window;

/// The magic number opening every zstd frame.
const MAGIC: u32 = 0xFD2F_B528;

/// Skippable frames use magics `0x184D2A50..=0x184D2A5F`.
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;

/// The most a block may decompress to, and the most it may occupy on disk.
///
/// This is zstd's `ZSTD_BLOCKSIZE_MAX`. That it equals btrfs's 128 KiB extent
/// limit is a coincidence of the two formats, not a dependency between them.
const BLOCK_SIZE_MAX: usize = 128 * 1024;

/// Bytes of literal buffer: one block's worth.
const LITERALS_BYTES: usize = BLOCK_SIZE_MAX;

/// Scratch memory for one zstd decode: the literal buffer and every table that
/// lives across the blocks of a frame.
///
/// It borrows [`Workspace::SIZE`] bytes from the caller. The layout inside is
/// the decoder's business:
///
/// | region                   | bytes   |
/// |--------------------------|---------|
/// | literal buffer           | 131 072 |
/// | Huffman table (12 bits)  |   8 192 |
/// | literal length FSE table |   2 048 |
/// | offset FSE table         |   1 024 |
/// | match length FSE table   |   2 048 |
///
/// A kernel caller allocates 36 pages (or any [`Workspace::SIZE`] bytes, at
/// any alignment, uninitialised contents being fine once they are a valid
/// `&mut [u8]`) once — per CPU, or per mount behind the lock that serialises
/// reads — and wraps it with [`Workspace::new`] for each decode, which is
/// free. Nothing from one decode is read by the next.
pub struct Workspace<'a> {
    buffer: &'a mut [u8],
}

impl<'a> Workspace<'a> {
    /// Bytes a workspace buffer must have: 144 384, just over 141 KiB.
    pub const SIZE: usize = LITERALS_BYTES
        + huffman::TABLE_BYTES
        + sequences::LENGTH_TABLE_BYTES
        + sequences::OFFSET_TABLE_BYTES
        + sequences::LENGTH_TABLE_BYTES;

    /// Wrap `buffer` as a workspace. Bytes past [`Workspace::SIZE`] are unused.
    ///
    /// Fails with [`BtrfsError::Truncated`] if the buffer is too small, which is
    /// a bug in the caller rather than anything about the disk.
    pub fn new(buffer: &'a mut [u8]) -> Result<Self, BtrfsError> {
        let found = buffer.len();
        let buffer = buffer
            .get_mut(..Self::SIZE)
            .ok_or_else(|| crate::truncated(Self::SIZE, found))?;
        Ok(Workspace { buffer })
    }

    /// Carve the buffer into the regions one frame uses, with no table valid.
    fn frame_state(&mut self) -> Option<FrameState<'_>> {
        let (literals, rest) = self.buffer.split_at_mut_checked(LITERALS_BYTES)?;
        let (huffman, rest) = rest.split_at_mut_checked(huffman::TABLE_BYTES)?;
        let (literal_lengths, rest) = rest.split_at_mut_checked(sequences::LENGTH_TABLE_BYTES)?;
        let (offsets, match_lengths) = rest.split_at_mut_checked(sequences::OFFSET_TABLE_BYTES)?;
        let slot = |storage| FseSlot { storage, log: None };
        Some(FrameState {
            literals,
            huffman: HuffmanSlot {
                storage: huffman,
                bits: None,
            },
            sequences: Slots {
                literal_lengths: slot(literal_lengths),
                offsets: slot(offsets),
                match_lengths: slot(match_lengths),
            },
            repeats: sequences::INITIAL_REPEATS,
        })
    }
}

impl fmt::Debug for Workspace<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspace")
            .field("bytes", &self.buffer.len())
            .finish()
    }
}

/// Everything that carries from one block of a frame to the next.
struct FrameState<'w> {
    literals: &'w mut [u8],
    huffman: HuffmanSlot<'w>,
    sequences: Slots<'w>,
    repeats: [usize; 3],
}

/// Expand the zstd frame at the front of `input` into `output`, returning the
/// bytes written.
///
/// Anything after the frame — the zero padding of a regular extent — is
/// ignored. Any malformation, a frame that would not fit in `output`, a
/// dictionary, or a checksum mismatch is [`BtrfsError::BadCompressedData`].
pub fn decompress(
    input: &[u8],
    output: &mut [u8],
    workspace: &mut Workspace<'_>,
) -> Result<usize, BtrfsError> {
    decode_frame(input, output, workspace).ok_or(BtrfsError::BadCompressedData {
        compression: COMPRESS_ZSTD,
    })
}

/// `Some(())` if `condition` holds: the `?`-able form of a validity check.
fn ensure(condition: bool) -> Option<()> {
    condition.then_some(())
}

/// The first `len` (at most 8) bytes of `bytes` as a little-endian number.
fn little_endian(bytes: &[u8], len: usize) -> Option<u64> {
    let field = bytes.get(..len.min(8))?;
    Some(
        field
            .iter()
            .rev()
            .fold(0u64, |value, &byte| (value << 8) | u64::from(byte)),
    )
}

/// The whole decode, with every failure collapsed to `None`.
fn decode_frame(input: &[u8], output: &mut [u8], workspace: &mut Workspace<'_>) -> Option<usize> {
    let input = skip_skippable_frames(input)?;
    let header = FrameHeader::parse(input)?;
    if let Some(size) = header.content_size {
        ensure(size <= u64::try_from(output.len()).ok()?)?;
    }
    let mut state = workspace.frame_state()?;
    let mut window = Window::new(output);
    let trailer = decode_blocks(input.get(header.length..)?, &mut window, &mut state)?;
    let written = window.position();

    if let Some(size) = header.content_size {
        ensure(u64::try_from(written).ok()? == size)?;
    }
    if header.checksum {
        let stored = u32_at(trailer, 0)?;
        let hash = xxh64::xxh64(output.get(..written)?, 0);
        ensure(u64::from(stored) == hash & 0xFFFF_FFFF)?;
    }
    Some(written)
}

/// Step over any skippable frames. Each consumes at least its 8-byte header.
fn skip_skippable_frames(mut input: &[u8]) -> Option<&[u8]> {
    while u32_at(input, 0)? & !0xF == SKIPPABLE_MAGIC {
        let size = usize::try_from(u32_at(input, 4)?).ok()?;
        input = input.get(size.checked_add(8)?..)?;
    }
    Some(input)
}

/// What the frame header says that this decoder acts on.
struct FrameHeader {
    /// Bytes the header occupies.
    length: usize,
    /// The declared decompressed size, when present.
    content_size: Option<u64>,
    /// Whether a content checksum follows the last block.
    checksum: bool,
}

impl FrameHeader {
    /// Parse the magic, descriptor, window descriptor, dictionary ID and
    /// content size (RFC 8878 §3.1.1.1).
    fn parse(input: &[u8]) -> Option<Self> {
        ensure(u32_at(input, 0)? == MAGIC)?;
        let descriptor = u8_at(input, 4)?;
        // Bit 3 is reserved and must be zero.
        ensure(descriptor & 0x08 == 0)?;
        let single_segment = descriptor & 0x20 != 0;
        // The window descriptor is present unless the frame is one segment;
        // its value is not needed (see the module documentation).
        let mut at = if single_segment { 5 } else { 6 };
        let id_length = match descriptor & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        ensure(little_endian(slice_at(input, at, id_length)?, id_length)? == 0)?;
        at += id_length;
        let size_length = match descriptor >> 6 {
            0 => usize::from(single_segment),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let raw_size = little_endian(slice_at(input, at, size_length)?, size_length)?;
        let content_size = match size_length {
            0 => None,
            // The two-byte form is offset by 256; the one-byte form covers
            // below that.
            2 => Some(raw_size.checked_add(256)?),
            _ => Some(raw_size),
        };
        Some(FrameHeader {
            length: at.checked_add(size_length)?,
            content_size,
            checksum: descriptor & 0x04 != 0,
        })
    }
}

/// Block type: bytes stored verbatim.
const BLOCK_RAW: u64 = 0;
/// Block type: one byte, repeated.
const BLOCK_RLE: u64 = 1;
/// Block type: literals and sequences.
const BLOCK_COMPRESSED: u64 = 2;

/// Decode blocks until the one marked last, returning the input after it.
///
/// Every block consumes at least its three-byte header, so the loop ends.
fn decode_blocks<'i>(
    mut input: &'i [u8],
    window: &mut Window<'_>,
    state: &mut FrameState<'_>,
) -> Option<&'i [u8]> {
    loop {
        let header = little_endian(input, 3)?;
        ensure(input.len() >= 3)?;
        let last = header & 1 != 0;
        let size = usize::try_from(header >> 3).ok()?;
        ensure(size <= BLOCK_SIZE_MAX)?;
        let body = input.get(3..)?;
        window.begin_block();
        let used = match (header >> 1) & 3 {
            BLOCK_RAW => {
                // NOALLOC: `Window::push` writes into the caller's buffer; this crate has no `alloc`.
                window.push(body.get(..size)?)?;
                size
            }
            BLOCK_RLE => {
                window.fill(*body.first()?, size)?;
                1
            }
            BLOCK_COMPRESSED => {
                decode_compressed(body.get(..size)?, window, state)?;
                size
            }
            _ => return None,
        };
        input = body.get(used..)?;
        if last {
            return Some(input);
        }
    }
}

/// A compressed block: literals into the buffer, then sequences into the
/// window.
fn decode_compressed(
    block: &[u8],
    window: &mut Window<'_>,
    state: &mut FrameState<'_>,
) -> Option<()> {
    let (count, used) = literals::read(block, &mut state.huffman, state.literals)?;
    let literals = state.literals.get(..count)?;
    sequences::execute(
        block.get(used..)?,
        &mut state.sequences,
        literals,
        window,
        &mut state.repeats,
    )
}

#[cfg(test)]
mod tests;
