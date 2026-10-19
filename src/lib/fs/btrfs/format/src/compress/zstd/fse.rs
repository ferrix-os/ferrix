//! Finite State Entropy: table descriptions, and the decoding tables built from
//! them.
//!
//! An FSE table of accuracy log `L` has `2^L` states. Each state names a symbol,
//! a number of bits to read, and a baseline; the next state is the baseline
//! plus those bits. How many states a symbol owns is its *normalised count*,
//! and a table description is nothing but those counts, sent compactly: the
//! decoder rebuilds the identical table by spreading each symbol over the
//! states in a fixed pseudo-random order (RFC 8878 §4.1.1).
//!
//! # Hostile descriptions
//!
//! Every quantity a description controls is bounded before it is used as a
//! size. The accuracy log is checked against the caller's maximum (6 for
//! Huffman weights, 8 or 9 for sequences) before `2^L` is computed, the symbol
//! index against the alphabet, and the counts must add up to exactly `2^L` —
//! the property that makes the spread visit every state and terminate, and
//! which [`build`] re-checks rather than trusts. Each table entry is four
//! bytes in caller-supplied storage, read back with bounds checks, so a table
//! that was never built for some state is an error and not a stale read.

use super::bits::ForwardBits;
use crate::array_at;

/// Bytes per decoding-table entry: symbol, bits to read, baseline (`u16` LE).
pub(super) const ENTRY_BYTES: usize = 4;

/// Symbols an FSE alphabet can have at most; the counts array is this long.
pub(super) const MAX_SYMBOLS: usize = 256;

/// The smallest accuracy log a description can encode: four bits, plus five.
const MIN_ACCURACY_LOG: u8 = 5;

/// Normalised counts per symbol. `-1` marks a "less than one" probability,
/// which owns exactly one state at the top of the table.
pub(super) type Counts = [i16; MAX_SYMBOLS];

/// One decoded state.
#[derive(Debug, Clone, Copy)]
pub(super) struct Entry {
    /// The symbol this state emits.
    pub(super) symbol: u8,
    /// Bits to read for the next state.
    pub(super) bits: u8,
    /// Added to those bits to form the next state.
    pub(super) baseline: u16,
}

/// A built decoding table, borrowed from wherever it was built.
#[derive(Debug, Clone, Copy)]
pub(super) struct Table<'t> {
    entries: &'t [u8],
    log: u8,
}

impl<'t> Table<'t> {
    /// View storage that [`build`] or [`build_rle`] filled for accuracy `log`.
    pub(super) fn new(storage: &'t [u8], log: u8) -> Option<Self> {
        let entries = storage.get(..table_bytes(log)?)?;
        Some(Table { entries, log })
    }

    /// Bits the initial state is read with.
    pub(super) fn log(&self) -> u8 {
        self.log
    }

    /// The entry for `state`, or `None` if the state is out of range.
    pub(super) fn entry(&self, state: usize) -> Option<Entry> {
        let [symbol, bits, low, high] =
            array_at::<ENTRY_BYTES>(self.entries, state.checked_mul(ENTRY_BYTES)?)?;
        Some(Entry {
            symbol,
            bits,
            baseline: u16::from_le_bytes([low, high]),
        })
    }
}

/// Bytes a table of accuracy `log` occupies, for any `log` up to 15.
fn table_bytes(log: u8) -> Option<usize> {
    (log <= 15).then_some(())?;
    (1usize << log).checked_mul(ENTRY_BYTES)
}

/// Parse a table description at the front of `input` into `counts`.
///
/// Returns the accuracy log and the number of bytes the description occupied.
/// `max_log` and `max_symbol` are the limits of the alphabet being described.
pub(super) fn read_description(
    input: &[u8],
    max_log: u8,
    max_symbol: u8,
    counts: &mut Counts,
) -> Option<(u8, usize)> {
    let mut bits = ForwardBits::new(input);
    let log = u8::try_from(bits.read(4)?)
        .ok()?
        .checked_add(MIN_ACCURACY_LOG)?;
    (log <= max_log && log <= 15).then_some(())?;
    counts.fill(0);

    let mut cursor = CountCursor::new(log);
    let mut symbol = 0usize;
    while cursor.remaining > 1 {
        (symbol <= usize::from(max_symbol)).then_some(())?;
        let count = cursor.next(&mut bits)?;
        *counts.get_mut(symbol)? = count;
        symbol = symbol.checked_add(1)?;
        if count == 0 {
            symbol = skip_zero_run(&mut bits, symbol, max_symbol)?;
        }
    }
    (cursor.remaining == 1).then_some((log, bits.consumed_bytes()))
}

/// The state of the variable-width count decoder.
///
/// `remaining` is the probability mass still to hand out, plus one; the width
/// of the next field shrinks as it does, which is what makes the encoding
/// compact.
struct CountCursor {
    remaining: i32,
    threshold: i32,
    width: u32,
}

impl CountCursor {
    fn new(log: u8) -> Self {
        let threshold = 1i32 << log;
        CountCursor {
            remaining: threshold + 1,
            threshold,
            width: u32::from(log) + 1,
        }
    }

    /// Decode one normalised count.
    ///
    /// Values below `max` fit in one bit fewer than the rest; the invariant
    /// `threshold <= remaining < 2 * threshold` keeps `max` within
    /// `0..threshold` and every count within `-1..remaining`.
    fn next(&mut self, bits: &mut ForwardBits<'_>) -> Option<i16> {
        let max = self
            .threshold
            .checked_mul(2)?
            .checked_sub(1)?
            .checked_sub(self.remaining)?;
        let short = i32::try_from(bits.peek(self.width.checked_sub(1)?)).ok()?;
        let value = if short < max {
            bits.skip(self.width.checked_sub(1)?)?;
            short
        } else {
            let long = i32::try_from(bits.peek(self.width)).ok()?;
            bits.skip(self.width)?;
            if long >= self.threshold {
                long.checked_sub(max)?
            } else {
                long
            }
        };
        let count = value.checked_sub(1)?;
        (count.abs() < self.remaining).then_some(())?;
        self.remaining = self.remaining.checked_sub(count.abs())?;
        while self.remaining < self.threshold && self.width > 1 {
            self.threshold >>= 1;
            self.width = self.width.checked_sub(1)?;
        }
        i16::try_from(count).ok()
    }
}

/// After a zero count, read the two-bit repeat flags that say how many more
/// symbols are zero, and return the next symbol to decode.
///
/// A flag of 3 means "three more, and another flag follows". Each flag
/// consumes input, so the loop ends with the input at the latest; running
/// past the alphabet ends it sooner.
fn skip_zero_run(bits: &mut ForwardBits<'_>, mut symbol: usize, max_symbol: u8) -> Option<usize> {
    loop {
        let flag = bits.read(2)?;
        symbol = symbol.checked_add(usize::try_from(flag).ok()?)?;
        (symbol <= usize::from(max_symbol).saturating_add(1)).then_some(())?;
        if flag != 3 {
            return Some(symbol);
        }
    }
}

/// Build the decoding table for `counts` at accuracy `log` into `storage`.
pub(super) fn build(counts: &[i16], log: u8, storage: &mut [u8]) -> Option<()> {
    let size = 1usize << log.min(15);
    let table = storage.get_mut(..table_bytes(log)?)?;
    let total = counts.iter().try_fold(0usize, |sum, &count| {
        sum.checked_add(usize::from(count.unsigned_abs()))
    })?;
    (total == size).then_some(())?;

    let mut next = [0u16; MAX_SYMBOLS];
    let free = place_low_probability(counts, table, &mut next)?;
    spread(counts, table, free)?;
    assign_states(table, &next, log)
}

/// Put every `-1` symbol in its own state at the top of the table.
///
/// Returns how many states at the bottom remain for the spread.
fn place_low_probability(
    counts: &[i16],
    table: &mut [u8],
    next: &mut [u16; MAX_SYMBOLS],
) -> Option<usize> {
    let mut free = table.len() / ENTRY_BYTES;
    for (symbol, (&count, slot)) in counts.iter().zip(next.iter_mut()).enumerate() {
        if count == -1 {
            free = free.checked_sub(1)?;
            *table.get_mut(free.checked_mul(ENTRY_BYTES)?)? = u8::try_from(symbol).ok()?;
            *slot = 1;
        } else {
            *slot = u16::try_from(count).ok()?;
        }
    }
    Some(free)
}

/// Spread the remaining symbols over states `0..free` in the RFC's order.
///
/// The step is odd for every table size of 32 or more, so stepping modulo the
/// size is a full cycle, and the counts summing to the size means the last
/// placement lands back on state 0. Both are checked, not assumed.
fn spread(counts: &[i16], table: &mut [u8], free: usize) -> Option<()> {
    let size = table.len() / ENTRY_BYTES;
    let step = (size >> 1) + (size >> 3) + 3;
    let mask = size.checked_sub(1)?;
    let mut position = 0usize;
    for (symbol, &count) in counts.iter().enumerate() {
        let symbol = u8::try_from(symbol).ok()?;
        for _ in 0..count.max(0) {
            *table.get_mut(position.checked_mul(ENTRY_BYTES)?)? = symbol;
            position = advance(position, step, mask, free)?;
        }
    }
    (position == 0).then_some(())
}

/// The next spread position below `free`, trying at most one full cycle.
fn advance(mut position: usize, step: usize, mask: usize, free: usize) -> Option<usize> {
    for _ in 0..=mask {
        position = position.wrapping_add(step) & mask;
        if position < free {
            return Some(position);
        }
    }
    None
}

/// Fill in each state's bit count and baseline from the order the symbols
/// were spread in.
fn assign_states(table: &mut [u8], next: &[u16; MAX_SYMBOLS], log: u8) -> Option<()> {
    let mut next = *next;
    let size = 1u32 << log.min(15);
    for entry in table.chunks_exact_mut(ENTRY_BYTES) {
        let [symbol, bits, low, high] = entry else {
            return None;
        };
        let counter = next.get_mut(usize::from(*symbol))?;
        let state = u32::from(*counter);
        *counter = counter.checked_add(1)?;
        let top = state.checked_ilog2()?;
        let width = u32::from(log).checked_sub(top)?;
        let baseline = state.checked_shl(width)?.checked_sub(size)?;
        *bits = u8::try_from(width).ok()?;
        [*low, *high] = u16::try_from(baseline).ok()?.to_le_bytes();
    }
    Some(())
}

/// Build the one-state table of RLE mode: every read emits `symbol` and reads
/// no bits.
pub(super) fn build_rle(symbol: u8, storage: &mut [u8]) -> Option<()> {
    let entry = storage.get_mut(..ENTRY_BYTES)?;
    entry.copy_from_slice(&[symbol, 0, 0, 0]);
    Some(())
}
