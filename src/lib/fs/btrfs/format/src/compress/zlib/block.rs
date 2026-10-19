//! The three DEFLATE block types, and the symbol stream two of them share.
//!
//! A stored block is raw bytes behind a length and its one's complement. The
//! other two are the same thing with different codes: a stream of literal
//! bytes (symbols 0–255), back-references (symbols 257–285, each followed by
//! extra bits and a distance code with extra bits of its own) and an
//! end-of-block symbol (256). A fixed block uses the code tabulated in RFC 1951
//! §3.2.6. A dynamic block sends its code first, as a list of code lengths
//! which is itself Huffman-coded, with run-length symbols for repeats.
//!
//! The code tables for a block live in that block's stack frame and are gone
//! when it ends, so a stream of many dynamic blocks never holds two sets.

use super::bits::Bits;
use super::huffman::{CodeLen, Dist, LitLen, Rule};
use super::output::Output;
use crate::u16_at;

/// The literal/length symbol that ends a block.
const END_OF_BLOCK: u16 = 256;

/// The first length symbol; `LENGTH_BASE[symbol - FIRST_LENGTH]`.
const FIRST_LENGTH: usize = 257;

/// Match length for each of symbols 257–285, before extra bits. 286 and 287
/// are not here, so looking one up fails, which is what makes them invalid.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

/// Extra bits following each of length symbols 257–285.
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Distance for each of distance symbols 0–29, before extra bits. 30 and 31
/// are absent for the same reason as length symbols 286 and 287.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

/// Extra bits following each of distance symbols 0–29.
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order a dynamic block sends code-length code lengths in, most likely
/// used first so the tail can be left off.
const CODE_LENGTH_ORDER: [u8; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Literal/length codes a dynamic block may declare. `HLIT` could say 288,
/// but symbols 286 and 287 never occur, and zlib refuses a header claiming
/// them.
const MAX_LITLEN_CODES: usize = 286;

/// Distance codes a dynamic block may declare; `HDIST` could say 32.
const MAX_DIST_CODES: usize = 30;

/// Symbols in the fixed literal/length code, which does assign 286 and 287.
const FIXED_LITLEN_CODES: usize = 288;

/// Symbols in the fixed distance code, which assigns 30 and 31.
const FIXED_DIST_CODES: usize = 32;

/// Copy a stored block: `LEN`, `NLEN`, then `LEN` bytes, byte-aligned.
pub(super) fn stored(bits: &mut Bits<'_>, out: &mut Output<'_>) -> Option<()> {
    let header = bits.bytes(4)?;
    let len = u16_at(header, 0)?;
    let nlen = u16_at(header, 2)?;
    if len != !nlen {
        return None;
    }
    // NOALLOC: `Output::extend` writes into the caller's buffer; this crate has no `alloc`.
    out.extend(bits.bytes(usize::from(len))?)
}

/// Decode a block coded with the fixed code.
///
/// Never inlined, and nor is [`dynamic`]: each holds a full set of tables, and
/// inlined into the block loop the two frames would be merged into one that
/// reserves both sets at once, doubling the stack for no block that needs it.
#[inline(never)]
pub(super) fn fixed(bits: &mut Bits<'_>, out: &mut Output<'_>) -> Option<()> {
    let mut lengths = [0u8; FIXED_LITLEN_CODES + FIXED_DIST_CODES];
    for (range, len) in [(0..144, 8), (144..256, 9), (256..280, 7), (280..320, 8)] {
        lengths.get_mut(range)?.fill(len);
    }
    // The distance code is 32 five-bit codes, overwriting the tail above.
    lengths.get_mut(FIXED_LITLEN_CODES..)?.fill(5);
    let (litlen_lengths, dist_lengths) = lengths.split_at_checked(FIXED_LITLEN_CODES)?;
    let mut litlen = LitLen::EMPTY;
    litlen.build(litlen_lengths, Rule::Complete)?;
    let mut dist = Dist::EMPTY;
    dist.build(dist_lengths, Rule::Complete)?;
    symbols(bits, out, &litlen, &dist)
}

/// Read a dynamic block's code description, then decode the block with it.
#[inline(never)]
pub(super) fn dynamic(bits: &mut Bits<'_>, out: &mut Output<'_>) -> Option<()> {
    let nlen = (bits.take(5)? as usize).checked_add(FIRST_LENGTH)?;
    let ndist = (bits.take(5)? as usize).checked_add(1)?;
    let ncode = (bits.take(4)? as usize).checked_add(4)?;
    if nlen > MAX_LITLEN_CODES || ndist > MAX_DIST_CODES {
        return None;
    }
    let mut lengths = [0u8; MAX_LITLEN_CODES + MAX_DIST_CODES];
    read_lengths(bits, lengths.get_mut(..nlen.checked_add(ndist)?)?, ncode)?;
    let (litlen_lengths, rest) = lengths.split_at_checked(nlen)?;
    // A code with no end-of-block symbol describes a block that cannot end.
    if *litlen_lengths.get(usize::from(END_OF_BLOCK))? == 0 {
        return None;
    }
    let mut litlen = LitLen::EMPTY;
    litlen.build(litlen_lengths, Rule::AllowSingle)?;
    let mut dist = Dist::EMPTY;
    dist.build(rest.get(..ndist)?, Rule::AllowSingle)?;
    symbols(bits, out, &litlen, &dist)
}

/// Fill `lengths` from a dynamic block header: `ncode` three-bit code-length
/// code lengths, then the literal/length and distance lengths coded with them.
///
/// The two lists are sent as one sequence, and a repeat may cross from one to
/// the other. A repeat that would overrun the sequence is an error, as is
/// repeating the previous length when there is none.
fn read_lengths(bits: &mut Bits<'_>, lengths: &mut [u8], ncode: usize) -> Option<()> {
    let mut code_lengths = [0u8; CODE_LENGTH_ORDER.len()];
    for &symbol in CODE_LENGTH_ORDER.get(..ncode)? {
        *code_lengths.get_mut(usize::from(symbol))? = u8::try_from(bits.take(3)?).ok()?;
    }
    let mut code = CodeLen::EMPTY;
    code.build(&code_lengths, Rule::Complete)?;

    // Each pass fills at least one length, so this ends.
    let mut filled = 0;
    while filled < lengths.len() {
        let (len, repeat) = length_run(bits, &code, lengths, filled)?;
        let end = filled.checked_add(repeat)?;
        lengths.get_mut(filled..end)?.fill(len);
        filled = end;
    }
    Some(())
}

/// Decode one code-length symbol into a length and how many times it repeats.
fn length_run(
    bits: &mut Bits<'_>,
    code: &CodeLen,
    lengths: &[u8],
    filled: usize,
) -> Option<(u8, usize)> {
    match code.decode(bits)? {
        len @ 0..=15 => Some((u8::try_from(len).ok()?, 1)),
        16 => {
            let previous = *lengths.get(filled.checked_sub(1)?)?;
            Some((previous, 3 + bits.take(2)? as usize))
        }
        17 => Some((0, 3 + bits.take(3)? as usize)),
        18 => Some((0, 11 + bits.take(7)? as usize)),
        _ => None,
    }
}

/// Decode literals and back-references until the end-of-block symbol.
///
/// Each pass consumes at least one bit, since no code is shorter, so a stream
/// that never sends end-of-block runs out of input and fails.
fn symbols(bits: &mut Bits<'_>, out: &mut Output<'_>, litlen: &LitLen, dist: &Dist) -> Option<()> {
    loop {
        match litlen.decode(bits)? {
            END_OF_BLOCK => return Some(()),
            // NOALLOC: `Output::push` writes into the caller's buffer; this crate has no `alloc`.
            literal @ 0..=255 => out.push(u8::try_from(literal).ok()?)?,
            symbol => {
                let length = length(bits, symbol)?;
                let distance = distance(bits, dist)?;
                out.copy_back(distance, length)?;
            }
        }
    }
}

/// The match length a length symbol and its extra bits name.
fn length(bits: &mut Bits<'_>, symbol: u16) -> Option<usize> {
    let index = usize::from(symbol).checked_sub(FIRST_LENGTH)?;
    let base = *LENGTH_BASE.get(index)?;
    let extra = bits.take(u32::from(*LENGTH_EXTRA.get(index)?))?;
    usize::from(base).checked_add(extra as usize)
}

/// Decode a distance symbol and its extra bits into a distance.
fn distance(bits: &mut Bits<'_>, dist: &Dist) -> Option<usize> {
    let index = usize::from(dist.decode(bits)?);
    let base = *DIST_BASE.get(index)?;
    let extra = bits.take(u32::from(*DIST_EXTRA.get(index)?))?;
    usize::from(base).checked_add(extra as usize)
}
