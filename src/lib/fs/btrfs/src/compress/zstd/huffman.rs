//! Huffman-coded literals: the tree description and the streams it decodes.
//!
//! zstd never sends code lengths directly. It sends a *weight* per symbol, up
//! to 255 of them, and leaves the last symbol's weight implied: weights `w`
//! contribute `2^(w-1)` each, the total must be a power of two, and whatever is
//! missing from the next power of two is the last symbol's share. A symbol of
//! weight `w` gets a code of `max_bits + 1 - w` bits (RFC 8878 §4.2.1).
//!
//! The weights are either four bits each, or FSE-compressed with two states
//! sharing one table of accuracy log at most 6. Codes are assigned
//! canonically — lowest weight first, then symbol order — so the decoding table
//! is built by filling consecutive runs of `2^(w-1)` cells, and decoding a
//! symbol is one lookup of `max_bits` peeked bits.
//!
//! # Limits
//!
//! RFC 8878 caps `max_bits` at 11. The reference decoder accepts 12, and Linux
//! reads btrfs through it, so 12 is accepted here too: refusing a stream that
//! every Linux kernel reads would be the wrong failure. The table is sized for
//! 12. A weight above 12, a total of zero, a remainder that is not a power of
//! two, or an odd number of the longest codes all describe no prefix code and
//! are rejected, as the reference decoder rejects them.

use super::bits::ReverseBits;
use super::fse::{self, Counts, ENTRY_BYTES, MAX_SYMBOLS};
use crate::u16_at;

/// The longest code this decoder accepts, in bits.
pub(super) const MAX_BITS: u8 = 12;

/// Bytes per decoding-table cell: symbol, code length.
const CELL_BYTES: usize = 2;

/// Bytes the decoding table needs at [`MAX_BITS`]: 8 KiB.
pub(super) const TABLE_BYTES: usize = CELL_BYTES << MAX_BITS;

/// Accuracy log of the FSE table that compresses weights.
const WEIGHTS_MAX_LOG: u8 = 6;

/// Weights a description can list explicitly; the 256th is implied.
const MAX_EXPLICIT_WEIGHTS: usize = 255;

/// A built decoding table and its code length.
#[derive(Debug, Clone, Copy)]
pub(super) struct Tree<'t> {
    cells: &'t [u8],
    bits: u8,
}

impl<'t> Tree<'t> {
    /// View storage that [`read_tree`] filled for codes of up to `bits`.
    pub(super) fn new(storage: &'t [u8], bits: u8) -> Option<Self> {
        (bits <= MAX_BITS).then_some(())?;
        let cells = storage.get(..CELL_BYTES << bits)?;
        Some(Tree { cells, bits })
    }
}

/// Read a tree description at the front of `input` and build its table into
/// `storage`. Returns the code length and the bytes the description occupied.
pub(super) fn read_tree(input: &[u8], storage: &mut [u8]) -> Option<(u8, usize)> {
    let mut weights = [0u8; MAX_SYMBOLS];
    let header = usize::from(*input.first()?);
    let explicit_weights = weights.get_mut(..MAX_EXPLICIT_WEIGHTS)?;
    let (explicit, used) = if header < 128 {
        let body = input.get(1..header.checked_add(1)?)?;
        (fse_weights(body, explicit_weights)?, header.checked_add(1)?)
    } else {
        let explicit = header.checked_sub(127)?;
        let bytes = explicit.div_ceil(2);
        let body = input.get(1..bytes.checked_add(1)?)?;
        direct_weights(body, explicit_weights.get_mut(..explicit)?)?;
        (explicit, bytes.checked_add(1)?)
    };
    let bits = build_table(&mut weights, explicit, storage)?;
    Some((bits, used))
}

/// Four bits per weight, high nibble first.
fn direct_weights(body: &[u8], weights: &mut [u8]) -> Option<()> {
    for (index, weight) in weights.iter_mut().enumerate() {
        let byte = *body.get(index / 2)?;
        *weight = if index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0F
        };
    }
    Some(())
}

/// FSE-compressed weights: a table description, then a backward stream
/// decoded by two states taking turns.
///
/// The stream has no count. It ends when a state update reads past the start
/// of the stream; the other state then emits one last symbol. Every turn emits
/// into `weights`, which holds 255, so a stream that never overflows fails
/// there rather than looping.
fn fse_weights(body: &[u8], weights: &mut [u8]) -> Option<usize> {
    let mut counts: Counts = [0; MAX_SYMBOLS];
    let (log, used) = fse::read_description(body, WEIGHTS_MAX_LOG, u8::MAX, &mut counts)?;
    let mut storage = [0u8; ENTRY_BYTES << WEIGHTS_MAX_LOG];
    fse::build(&counts, log, &mut storage)?;
    let table = fse::Table::new(&storage, log)?;

    let mut bits = ReverseBits::new(body.get(used..)?)?;
    let mut states = [0usize; 2];
    for state in &mut states {
        *state = usize::try_from(bits.read(u32::from(log))).ok()?;
    }
    let mut emitted = 0usize;
    let mut turn = 0usize;
    loop {
        // NOALLOC: a decoding table's `entry` lookup; this crate has no `alloc`.
        let entry = table.entry(*states.get(turn)?)?;
        *weights.get_mut(emitted)? = entry.symbol;
        emitted = emitted.checked_add(1)?;
        let step = usize::try_from(bits.read(u32::from(entry.bits))).ok()?;
        *states.get_mut(turn)? = usize::from(entry.baseline).checked_add(step)?;
        turn ^= 1;
        if bits.overflowed() {
            // NOALLOC: a decoding table's `entry` lookup; this crate has no `alloc`.
            let last = table.entry(*states.get(turn)?)?;
            *weights.get_mut(emitted)? = last.symbol;
            return emitted.checked_add(1);
        }
    }
}

/// Complete the weights with the implied last one and fill the decoding
/// table. Returns the code length.
fn build_table(weights: &mut [u8; MAX_SYMBOLS], explicit: usize, storage: &mut [u8]) -> Option<u8> {
    let mut ranks = [0u32; MAX_BITS as usize + 1];
    let mut total = 0u32;
    for &weight in weights.get(..explicit)? {
        (weight <= MAX_BITS).then_some(())?;
        if weight > 0 {
            let rank = ranks.get_mut(usize::from(weight))?;
            *rank = rank.checked_add(1)?;
            total = total.checked_add(1 << (weight - 1))?;
        }
    }
    (total != 0).then_some(())?;
    let bits = total.checked_ilog2()?.checked_add(1)?;
    (bits <= u32::from(MAX_BITS)).then_some(())?;
    let left = (1u32 << bits).checked_sub(total)?;
    (left.is_power_of_two()).then_some(())?;
    let last = u8::try_from(left.trailing_zeros().checked_add(1)?).ok()?;
    *weights.get_mut(explicit)? = last;
    let rank = ranks.get_mut(usize::from(last))?;
    *rank = rank.checked_add(1)?;
    // A complete prefix code has an even number, at least two, of longest
    // codes.
    let longest = *ranks.get(1)?;
    (longest >= 2 && longest % 2 == 0).then_some(())?;

    let bits = u8::try_from(bits).ok()?;
    fill_cells(weights.get(..=explicit)?, &ranks, bits, storage)?;
    Some(bits)
}

/// Assign each symbol its run of cells, lowest weight first.
fn fill_cells(weights: &[u8], ranks: &[u32], bits: u8, storage: &mut [u8]) -> Option<()> {
    let mut starts = [0usize; MAX_BITS as usize + 1];
    let mut at = 0usize;
    for (weight, start) in starts.iter_mut().enumerate().skip(1) {
        *start = at;
        let run = usize::try_from(*ranks.get(weight)?).ok()?;
        at = at.checked_add(run.checked_shl(u32::try_from(weight).ok()? - 1)?)?;
    }
    let cells = storage.get_mut(..CELL_BYTES << bits)?;
    (at == cells.len() / CELL_BYTES).then_some(())?;

    for (symbol, &weight) in weights.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let start = starts.get_mut(usize::from(weight))?;
        let first = *start;
        *start = first.checked_add(1 << (weight - 1))?;
        let run = cells.get_mut(first.checked_mul(CELL_BYTES)?..start.checked_mul(CELL_BYTES)?)?;
        let cell = [
            u8::try_from(symbol).ok()?,
            bits.checked_add(1)?.checked_sub(weight)?,
        ];
        for to in run.chunks_exact_mut(CELL_BYTES) {
            to.copy_from_slice(&cell);
        }
    }
    Some(())
}

/// Decode `streams` — one stream, or four behind a jump table — filling all
/// of `out`.
pub(super) fn decode(tree: Tree<'_>, streams: &[u8], four: bool, out: &mut [u8]) -> Option<()> {
    if !four {
        return decode_stream(tree, streams, out);
    }
    // The jump table gives the sizes of the first three streams; the fourth is
    // whatever is left. Each of the first three regenerates a quarter, rounded
    // up, and the fourth the rest.
    let sizes = [
        u16_at(streams, 0)?,
        u16_at(streams, 2)?,
        u16_at(streams, 4)?,
    ];
    let mut data = streams.get(6..)?;
    let segment = out.len().div_ceil(4);
    let mut out = out;
    for size in sizes {
        let (stream, rest) = data.split_at_checked(usize::from(size))?;
        let (target, others) = out.split_at_mut_checked(segment)?;
        decode_stream(tree, stream, target)?;
        data = rest;
        out = others;
    }
    decode_stream(tree, data, out)
}

/// Decode one backward stream into exactly `out.len()` symbols, which must
/// consume the stream to its last bit.
fn decode_stream(tree: Tree<'_>, stream: &[u8], out: &mut [u8]) -> Option<()> {
    let mut bits = ReverseBits::new(stream)?;
    let peek = u32::from(tree.bits);
    for byte in out.iter_mut() {
        let code = usize::try_from(bits.peek(peek)).ok()?;
        let at = code.checked_mul(CELL_BYTES)?;
        let [symbol, length] = crate::array_at::<CELL_BYTES>(tree.cells, at)?;
        *byte = symbol;
        bits.consume(u32::from(length));
    }
    bits.finished_exactly().then_some(())
}
