//! Canonical Huffman codes: checking a set of code lengths, and decoding.
//!
//! DEFLATE never transmits codes, only a bit length per symbol. The codes
//! follow from the lengths by the canonical rule: shorter codes numerically
//! before longer ones, and within a length, symbols in increasing order. So a
//! table needs only how many codes each length has and the symbols sorted by
//! (length, symbol), and a code of length `len` is found by walking lengths
//! from 1, keeping track of the first code of each length (RFC 1951 §3.2.2;
//! this is also how Mark Adler's `puff` decodes).
//!
//! # Which length sets are codes at all
//!
//! Lengths describe a usable prefix code only if the Kraft sum
//! `Σ 2^-len` is at most 1. Above 1 the set is *over-subscribed*: two symbols
//! would need the same code, and a decoder that did not check would pick one.
//! Below 1 it is *incomplete*: some bit patterns decode to nothing. zlib's
//! `inflate_table` refuses both, with one exception it spells out and this
//! module copies: a literal/length or distance code whose longest length is 1
//! may be incomplete. RFC 1951 needs that for a block that uses a single
//! distance code, which is sent with one bit, leaving the other pattern
//! unused; it also covers a set with no codes at all, which is how a block of
//! only literals says it has no distances. Decoding an unused pattern is then
//! an error, found when it happens. The code-length code gets no exception.
//!
//! # The fast path
//!
//! Walking lengths costs a loop per bit. Most symbols in real data have short
//! codes, so each table also has a direct-lookup array indexed by the next few
//! input bits, holding the symbol and its length for every code short enough to
//! fit. Only a miss — a longer code, or an unused pattern — takes the walk.

use super::bits::Bits;

/// The longest code DEFLATE allows.
pub(super) const MAX_BITS: usize = 15;

/// Where a lookup entry's length begins; the symbol takes the bits below. The
/// largest symbol is 287, which needs nine.
const SYMBOL_BITS: u32 = 9;

/// Selects the symbol from a lookup entry.
const SYMBOL_MASK: u16 = (1 << SYMBOL_BITS) - 1;

/// Literal/length table: 288 symbols, nine bits of direct lookup.
pub(super) type LitLen = Huffman<288, 512>;

/// Distance table: 32 symbols (the fixed code assigns all of them), eight bits
/// of direct lookup.
pub(super) type Dist = Huffman<32, 256>;

/// Code-length table: 19 symbols whose codes are at most 7 bits, all of which
/// fit the direct lookup.
pub(super) type CodeLen = Huffman<19, 128>;

/// Whether a set of lengths may leave bit patterns unused.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Rule {
    /// The code must be complete. For the code-length code.
    Complete,
    /// Complete, or no code longer than one bit. For literal/length and
    /// distance codes, as zlib permits.
    AllowSingle,
}

/// A decoding table for up to `SYMBOLS` symbols with a `FAST`-entry lookup.
///
/// `FAST` must be a power of two no larger than `1 << MAX_BITS`; the aliases
/// above are the only instances.
pub(super) struct Huffman<const SYMBOLS: usize, const FAST: usize> {
    /// Number of codes of each length. Index 0 is kept at zero.
    counts: [u16; MAX_BITS + 1],
    /// Symbols in canonical code order.
    symbols: [u16; SYMBOLS],
    /// Indexed by the next `log2(FAST)` bits of input: `len << SYMBOL_BITS |
    /// symbol` for a code that short, or 0 when the walk has to decide.
    fast: [u16; FAST],
}

impl<const SYMBOLS: usize, const FAST: usize> Huffman<SYMBOLS, FAST> {
    /// A table with no codes, to be [`build`](Self::build)-ed in place.
    pub(super) const EMPTY: Self = Huffman {
        counts: [0; MAX_BITS + 1],
        symbols: [0; SYMBOLS],
        fast: [0; FAST],
    };

    /// Replace the table with the code `lengths` describe, `lengths[symbol]`
    /// being that symbol's code length and 0 meaning unused.
    ///
    /// `None` for more lengths than the table has symbols, a length over 15,
    /// or a set `rule` does not accept.
    pub(super) fn build(&mut self, lengths: &[u8], rule: Rule) -> Option<()> {
        if lengths.len() > SYMBOLS {
            return None;
        }
        self.count(lengths)?;
        self.check_kraft(rule)?;
        self.sort_symbols(lengths)?;
        self.fill_fast(lengths)
    }

    /// Count the codes of each length.
    fn count(&mut self, lengths: &[u8]) -> Option<()> {
        self.counts = [0; MAX_BITS + 1];
        for &len in lengths {
            // At most 288 lengths, so no count can overflow a `u16`.
            *self.counts.get_mut(usize::from(len))? += 1;
        }
        *self.counts.get_mut(0)? = 0;
        Some(())
    }

    /// Refuse an over-subscribed set, and an incomplete one `rule` forbids.
    ///
    /// `left` is the number of unassigned codes of the current length; it
    /// doubles with each extra bit and cannot exceed `1 << 15`.
    fn check_kraft(&self, rule: Rule) -> Option<()> {
        let mut left: i32 = 1;
        let mut longest = 0;
        for len in 1..=MAX_BITS {
            let count = *self.counts.get(len)?;
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return None;
            }
            if count != 0 {
                longest = len;
            }
        }
        let accepted = left == 0 || (rule == Rule::AllowSingle && longest <= 1);
        accepted.then_some(())
    }

    /// Lay the symbols out in canonical order: by length, then by value.
    fn sort_symbols(&mut self, lengths: &[u8]) -> Option<()> {
        let mut offsets = [0u16; MAX_BITS + 1];
        for len in 1..MAX_BITS {
            let end = offsets.get(len)?.checked_add(*self.counts.get(len)?)?;
            *offsets.get_mut(len + 1)? = end;
        }
        for (symbol, &len) in lengths.iter().enumerate().filter(|(_, len)| **len != 0) {
            let offset = offsets.get_mut(usize::from(len))?;
            *self.symbols.get_mut(usize::from(*offset))? = u16::try_from(symbol).ok()?;
            *offset = offset.checked_add(1)?;
        }
        Some(())
    }

    /// Enter every code of at most `log2(FAST)` bits in the lookup array.
    ///
    /// The canonical code of each symbol is assigned in order, bit-reversed to
    /// match the input order, and written at every index whose low `len` bits
    /// are that code — the higher bits belong to whatever follows it.
    fn fill_fast(&mut self, lengths: &[u8]) -> Option<()> {
        self.fast.fill(0);
        let mut next = self.first_codes()?;
        let fast_bits = FAST.trailing_zeros();
        for (symbol, &len) in lengths.iter().enumerate().filter(|(_, len)| **len != 0) {
            let slot = next.get_mut(usize::from(len))?;
            let code = *slot;
            *slot = slot.checked_add(1)?;
            if u32::from(len) <= fast_bits {
                let entry = (u16::from(len) << SYMBOL_BITS) | u16::try_from(symbol).ok()?;
                self.place(reverse(code, u32::from(len)), len, entry)?;
            }
        }
        Some(())
    }

    /// The first canonical code of each length.
    fn first_codes(&self) -> Option<[u32; MAX_BITS + 1]> {
        let mut first = [0u32; MAX_BITS + 1];
        let mut code = 0u32;
        for len in 1..=MAX_BITS {
            code = code.checked_add(u32::from(*self.counts.get(len - 1)?))? << 1;
            *first.get_mut(len)? = code;
        }
        Some(first)
    }

    /// Write `entry` at `reversed` and every index above it that agrees in the
    /// low `len` bits.
    fn place(&mut self, reversed: usize, len: u8, entry: u16) -> Option<()> {
        let stride = 1usize << len;
        let mut index = reversed;
        while index < FAST {
            *self.fast.get_mut(index)? = entry;
            index = index.checked_add(stride)?;
        }
        Some(())
    }

    /// Decode one symbol, consuming its code.
    ///
    /// `None` if the input ends inside the code or the bits are a pattern the
    /// code leaves unused.
    pub(super) fn decode(&self, bits: &mut Bits<'_>) -> Option<u16> {
        let window = bits.peek(MAX_BITS as u32);
        let entry = self
            .fast
            .get(window as usize & (FAST - 1))
            .copied()
            .unwrap_or(0);
        let (symbol, len) = if entry == 0 {
            self.walk(window)?
        } else {
            (entry & SYMBOL_MASK, u32::from(entry >> SYMBOL_BITS))
        };
        bits.consume(len)?;
        Some(symbol)
    }

    /// Find the code that prefixes `window` by walking lengths, returning its
    /// symbol and length.
    ///
    /// At each length, `code` is the bits read so far in code order and
    /// `first` the first code of that length; the codes of that length are
    /// `first..first + count`, and `index` is where their symbols begin.
    fn walk(&self, window: u32) -> Option<(u16, u32)> {
        let mut code = 0u32;
        let mut first = 0u32;
        let mut index = 0usize;
        for len in 1..=MAX_BITS {
            code |= (window >> (len - 1)) & 1;
            let count = *self.counts.get(len)?;
            let offset = code.checked_sub(first)?;
            if offset < u32::from(count) {
                let symbol = *self.symbols.get(index.checked_add(offset as usize)?)?;
                return Some((symbol, len as u32));
            }
            index = index.checked_add(usize::from(count))?;
            first = first.checked_add(u32::from(count))? << 1;
            code <<= 1;
        }
        None
    }
}

/// `code`'s low `len` bits in reverse order, `1 <= len <= 15`.
fn reverse(code: u32, len: u32) -> usize {
    code.reverse_bits()
        .checked_shr(32 - len.clamp(1, 32))
        .unwrap_or(0) as usize
}
