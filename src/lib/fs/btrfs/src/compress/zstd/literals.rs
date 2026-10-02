//! The literals section of a compressed block (RFC 8878 §3.1.1.3.1).
//!
//! Literals are the bytes a block could not express as matches. They are
//! decoded in full, into the workspace's literal buffer, before any sequence
//! runs: sequences then interleave slices of that buffer with back-references
//! into the output. Four encodings share one header:
//!
//! - **raw** — the bytes follow verbatim;
//! - **RLE** — one byte, repeated;
//! - **compressed** — a Huffman tree description, then one or four streams;
//! - **treeless** — streams only, decoded with the tree of the most recent
//!   compressed literals section in this frame.
//!
//! The header's sizes are bounded before anything is written from them: the
//! regenerated size by the block limit and the literal buffer, and the
//! compressed size by the block the section sits in.

use super::huffman::{self, Tree};
use super::{BLOCK_SIZE_MAX, ensure, little_endian};

/// Huffman state that lives across the blocks of one frame.
#[derive(Debug)]
pub(super) struct HuffmanSlot<'w> {
    /// Table storage, [`huffman::TABLE_BYTES`] long.
    pub(super) storage: &'w mut [u8],
    /// Code length of the table in `storage`, once one has been built in this
    /// frame. `None` makes a treeless section corrupt.
    pub(super) bits: Option<u8>,
}

/// Section type: the literals follow verbatim.
const RAW: u8 = 0;
/// Section type: one byte, repeated.
const RLE: u8 = 1;
/// Section type: a Huffman tree, then its streams.
const COMPRESSED: u8 = 2;

/// Decode the literals section at the front of `block` into `literals`.
///
/// Returns how many literals there are and how many bytes of `block` the
/// section occupied.
pub(super) fn read(
    block: &[u8],
    huffman: &mut HuffmanSlot<'_>,
    literals: &mut [u8],
) -> Option<(usize, usize)> {
    let first = *block.first()?;
    let kind = first & 3;
    let format = (first >> 2) & 3;
    if kind == RAW || kind == RLE {
        return read_plain(block, kind, format, literals);
    }

    let sizes = coded_header(block, format)?;
    let out = literal_target(literals, sizes.regenerated)?;
    let end = sizes.header.checked_add(sizes.compressed)?;
    let section = block.get(sizes.header..end)?;
    let (bits, tree_bytes) = if kind == COMPRESSED {
        huffman.bits = None;
        let (bits, used) = huffman::read_tree(section, huffman.storage)?;
        huffman.bits = Some(bits);
        (bits, used)
    } else {
        (huffman.bits?, 0)
    };
    let tree = Tree::new(huffman.storage, bits)?;
    huffman::decode(tree, section.get(tree_bytes..)?, sizes.four_streams, out)?;
    Some((sizes.regenerated, end))
}

/// Raw and RLE sections: 5, 12 or 20 bits of size in a 1, 2 or 3 byte header.
fn read_plain(block: &[u8], kind: u8, format: u8, literals: &mut [u8]) -> Option<(usize, usize)> {
    let header = match format {
        1 => 2,
        3 => 3,
        _ => 1,
    };
    let shift = if header == 1 { 3 } else { 4 };
    let count = usize::try_from(little_endian(block, header)? >> shift).ok()?;
    let out = literal_target(literals, count)?;
    let body = block.get(header..)?;
    let used = if kind == RAW {
        out.copy_from_slice(body.get(..count)?);
        count
    } else {
        out.fill(*body.first()?);
        1
    };
    Some((count, header.checked_add(used)?))
}

/// The first `count` bytes of the literal buffer, if `count` is a size a block
/// may have.
fn literal_target(literals: &mut [u8], count: usize) -> Option<&mut [u8]> {
    ensure(count <= BLOCK_SIZE_MAX)?;
    literals.get_mut(..count)
}

/// What a compressed or treeless header says.
struct CodedSizes {
    header: usize,
    regenerated: usize,
    compressed: usize,
    four_streams: bool,
}

/// Compressed and treeless headers: two sizes of 10, 14 or 18 bits each, after
/// the four type and format bits. Format 0 alone means a single stream.
fn coded_header(block: &[u8], format: u8) -> Option<CodedSizes> {
    let (header, width) = match format {
        0 | 1 => (3, 10),
        2 => (4, 14),
        _ => (5, 18),
    };
    let value = little_endian(block, header)?;
    let mask = (1u64 << width) - 1;
    let size = |shift: u32| usize::try_from((value >> shift) & mask).ok();
    Some(CodedSizes {
        header,
        regenerated: size(4)?,
        compressed: size(4 + width)?,
        // NOALLOC: `format != 0` compares the header's format bits; no macro.
        four_streams: format != 0,
    })
}
