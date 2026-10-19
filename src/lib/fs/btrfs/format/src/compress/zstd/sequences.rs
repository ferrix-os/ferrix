//! The sequences section of a compressed block (RFC 8878 §3.1.1.3.2), decoded
//! and executed in one pass.
//!
//! A sequence is three numbers: copy `literal_length` bytes from the literal
//! buffer, then copy `match_length` bytes from `offset` bytes back in the
//! output. Each number is sent as an FSE-coded *code* plus raw extra bits, and
//! the three FSE states share one backward bitstream. Whatever literals are
//! left after the last sequence are appended.
//!
//! # Table modes, and the state that outlives a block
//!
//! Each of the three codes picks a mode per block: the RFC's predefined
//! distribution, a single repeated symbol (RLE), a table description sent in
//! the block, or *repeat*, which reuses whatever table the previous
//! sequences section of this frame used. A slot's table is rebuilt in place
//! whenever a block does not say repeat, so "the previous table" is simply the
//! one in the slot; its `log` is `None` until a table has been built in this
//! frame, which is what makes a leading repeat corrupt.
//!
//! Offsets carry a second piece of cross-block state, the three *repeat
//! offsets*. Offset values 1 to 3 name one of them rather than a distance, and
//! which one depends on whether the sequence has literals; see
//! [`resolve_offset`].

use super::bits::ReverseBits;
use super::ensure;
use super::fse::{self, Counts, ENTRY_BYTES, Entry, MAX_SYMBOLS, Table};
use super::window::Window;

/// An FSE table slot that lives across the blocks of one frame.
#[derive(Debug)]
pub(super) struct FseSlot<'w> {
    /// Table storage, sized for the code's largest accuracy log.
    pub(super) storage: &'w mut [u8],
    /// Accuracy log of the table in `storage`, once one has been built.
    pub(super) log: Option<u8>,
}

/// The three table slots, in the order the section lists their modes.
#[derive(Debug)]
pub(super) struct Slots<'w> {
    /// Literal length codes.
    pub(super) literal_lengths: FseSlot<'w>,
    /// Offset codes.
    pub(super) offsets: FseSlot<'w>,
    /// Match length codes.
    pub(super) match_lengths: FseSlot<'w>,
}

/// The limits and predefined distribution of one of the three codes.
struct Code {
    max_log: u8,
    max_symbol: u8,
    predefined_log: u8,
    predefined: &'static [i16],
}

/// Largest accuracy log of a literal length or match length table.
const LENGTH_MAX_LOG: u8 = 9;
/// Largest accuracy log of an offset table.
const OFFSET_MAX_LOG: u8 = 8;

/// Bytes a literal length or match length table needs: 2 KiB.
pub(super) const LENGTH_TABLE_BYTES: usize = ENTRY_BYTES << LENGTH_MAX_LOG;
/// Bytes an offset table needs: 1 KiB.
pub(super) const OFFSET_TABLE_BYTES: usize = ENTRY_BYTES << OFFSET_MAX_LOG;

const LITERAL_LENGTHS: Code = Code {
    max_log: LENGTH_MAX_LOG,
    max_symbol: 35,
    predefined_log: 6,
    predefined: &[
        4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1,
        1, 1, -1, -1, -1, -1,
    ],
};

/// Offset codes above 31 would name distances no 64-bit frame can have, and
/// the reference decoder refuses them in every mode.
const OFFSETS: Code = Code {
    max_log: OFFSET_MAX_LOG,
    max_symbol: 31,
    predefined_log: 5,
    predefined: &[
        1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
    ],
};

const MATCH_LENGTHS: Code = Code {
    max_log: LENGTH_MAX_LOG,
    max_symbol: 52,
    predefined_log: 6,
    predefined: &[
        1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
    ],
};

/// Baseline and extra bits of each literal length code.
const LITERAL_LENGTH_CODES: [(u32, u8); 36] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 1),
    (18, 1),
    (20, 1),
    (22, 1),
    (24, 2),
    (28, 2),
    (32, 3),
    (40, 3),
    (48, 4),
    (64, 6),
    (128, 7),
    (256, 8),
    (512, 9),
    (1024, 10),
    (2048, 11),
    (4096, 12),
    (8192, 13),
    (16384, 14),
    (32768, 15),
    (65536, 16),
];

/// Baseline and extra bits of each match length code.
const MATCH_LENGTH_CODES: [(u32, u8); 53] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 0),
    (17, 0),
    (18, 0),
    (19, 0),
    (20, 0),
    (21, 0),
    (22, 0),
    (23, 0),
    (24, 0),
    (25, 0),
    (26, 0),
    (27, 0),
    (28, 0),
    (29, 0),
    (30, 0),
    (31, 0),
    (32, 0),
    (33, 0),
    (34, 0),
    (35, 1),
    (37, 1),
    (39, 1),
    (41, 1),
    (43, 2),
    (47, 2),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 5),
    (131, 7),
    (259, 8),
    (515, 9),
    (1027, 10),
    (2051, 11),
    (4099, 12),
    (8195, 13),
    (16387, 14),
    (32771, 15),
    (65539, 16),
];

/// The repeat offsets every frame starts with.
pub(super) const INITIAL_REPEATS: [usize; 3] = [1, 4, 8];

/// Decode the sequences section `section` and execute it into `window`,
/// drawing literals from `literals`.
pub(super) fn execute(
    section: &[u8],
    slots: &mut Slots<'_>,
    literals: &[u8],
    window: &mut Window<'_>,
    repeats: &mut [usize; 3],
) -> Option<()> {
    let (count, header) = read_count(section)?;
    let rest = section.get(header..)?;
    if count == 0 {
        // No modes byte and no bitstream: anything more is not this format.
        ensure(rest.is_empty())?;
        // NOALLOC: `Window::push` writes into the caller's buffer; this crate has no `alloc`.
        return window.push(literals);
    }
    let (&modes, rest) = rest.split_first()?;
    ensure(modes & 3 == 0)?;
    let rest = prepare(
        &mut slots.literal_lengths,
        &LITERAL_LENGTHS,
        modes >> 6,
        rest,
    )?;
    let rest = prepare(&mut slots.offsets, &OFFSETS, (modes >> 4) & 3, rest)?;
    let rest = prepare(
        &mut slots.match_lengths,
        &MATCH_LENGTHS,
        (modes >> 2) & 3,
        rest,
    )?;

    let mut decoder = Decoder::new(rest, slots)?;
    let mut unused = literals;
    for index in 1..=count {
        let sequence = decoder.next(index == count)?;
        let offset = resolve_offset(sequence.offset_value, sequence.literals, repeats)?;
        let (now, later) = unused.split_at_checked(sequence.literals)?;
        // NOALLOC: `Window::push` writes into the caller's buffer; this crate has no `alloc`.
        window.push(now)?;
        window.copy_match(offset, sequence.match_length)?;
        unused = later;
    }
    ensure(decoder.bits.finished_exactly())?;
    // NOALLOC: `Window::push` writes into the caller's buffer; this crate has no `alloc`.
    window.push(unused)
}

/// The sequence count: one, two or three bytes.
fn read_count(section: &[u8]) -> Option<(usize, usize)> {
    let first = usize::from(*section.first()?);
    let byte = |at: usize| section.get(at).copied().map(usize::from);
    match first {
        0..128 => Some((first, 1)),
        128..255 => Some((((first - 128) << 8) + byte(1)?, 2)),
        _ => Some((byte(1)? + (byte(2)? << 8) + 0x7F00, 3)),
    }
}

/// Make `slot` hold the table `mode` asks for, consuming its description or
/// RLE symbol from `input`. Returns the input after it.
fn prepare<'i>(slot: &mut FseSlot<'_>, code: &Code, mode: u8, input: &'i [u8]) -> Option<&'i [u8]> {
    match mode {
        0 => {
            slot.log = None;
            fse::build(code.predefined, code.predefined_log, slot.storage)?;
            slot.log = Some(code.predefined_log);
            Some(input)
        }
        1 => {
            let (&symbol, rest) = input.split_first()?;
            ensure(symbol <= code.max_symbol)?;
            slot.log = None;
            fse::build_rle(symbol, slot.storage)?;
            slot.log = Some(0);
            Some(rest)
        }
        2 => {
            let mut counts: Counts = [0; MAX_SYMBOLS];
            let (log, used) =
                fse::read_description(input, code.max_log, code.max_symbol, &mut counts)?;
            slot.log = None;
            fse::build(&counts, log, slot.storage)?;
            slot.log = Some(log);
            input.get(used..)
        }
        _ => {
            ensure(slot.log.is_some())?;
            Some(input)
        }
    }
}

/// One decoded sequence, before its offset is resolved.
struct Sequence {
    literals: usize,
    offset_value: u64,
    match_length: usize,
}

/// One FSE state and the table it walks.
struct State<'t> {
    table: Table<'t>,
    state: usize,
}

impl<'t> State<'t> {
    /// Read the initial state for `slot`'s table.
    fn new(slot: &'t FseSlot<'_>, bits: &mut ReverseBits<'_>) -> Option<Self> {
        let table = Table::new(slot.storage, slot.log?)?;
        let state = usize::try_from(bits.read(u32::from(table.log()))).ok()?;
        Some(State { table, state })
    }

    fn entry(&self) -> Option<Entry> {
        // NOALLOC: a decoding table's `entry` lookup; this crate has no `alloc`.
        self.table.entry(self.state)
    }

    /// Move to the next state from `entry`, the current state's own entry.
    fn update(&mut self, entry: Entry, bits: &mut ReverseBits<'_>) -> Option<()> {
        let step = usize::try_from(bits.read(u32::from(entry.bits))).ok()?;
        self.state = usize::from(entry.baseline).checked_add(step)?;
        Some(())
    }
}

/// The three states and the bitstream they share.
struct Decoder<'a> {
    bits: ReverseBits<'a>,
    literal_lengths: State<'a>,
    offsets: State<'a>,
    match_lengths: State<'a>,
}

impl<'a> Decoder<'a> {
    /// Initial states are read literal lengths, offsets, match lengths.
    fn new(stream: &'a [u8], slots: &'a Slots<'_>) -> Option<Self> {
        let mut bits = ReverseBits::new(stream)?;
        let literal_lengths = State::new(&slots.literal_lengths, &mut bits)?;
        let offsets = State::new(&slots.offsets, &mut bits)?;
        let match_lengths = State::new(&slots.match_lengths, &mut bits)?;
        Some(Decoder {
            bits,
            literal_lengths,
            offsets,
            match_lengths,
        })
    }

    /// Decode one sequence. Extra bits are read offset, match length, literal
    /// length; states update literal length, match length, offset; and the
    /// last sequence updates none.
    fn next(&mut self, last: bool) -> Option<Sequence> {
        // NOALLOC: each FSE state's `entry` lookup; this crate has no `alloc`.
        let ll = self.literal_lengths.entry()?;
        let of = self.offsets.entry()?; // NOALLOC: as above.
        let ml = self.match_lengths.entry()?; // NOALLOC: as above.

        let offset_code = u32::from(of.symbol);
        ensure(offset_code <= u32::from(OFFSETS.max_symbol))?;
        let offset_value = (1u64 << offset_code).checked_add(self.bits.read(offset_code))?;
        let match_length = extra(&MATCH_LENGTH_CODES, ml.symbol, &mut self.bits)?;
        let literals = extra(&LITERAL_LENGTH_CODES, ll.symbol, &mut self.bits)?;

        if !last {
            self.literal_lengths.update(ll, &mut self.bits)?;
            self.match_lengths.update(ml, &mut self.bits)?;
            self.offsets.update(of, &mut self.bits)?;
        }
        Some(Sequence {
            literals,
            offset_value,
            match_length,
        })
    }
}

/// A length from its code: the code's baseline plus its extra bits.
fn extra(codes: &[(u32, u8)], symbol: u8, bits: &mut ReverseBits<'_>) -> Option<usize> {
    let &(baseline, width) = codes.get(usize::from(symbol))?;
    let value = u64::from(baseline).checked_add(bits.read(u32::from(width)))?;
    usize::try_from(value).ok()
}

/// Turn an offset value into a distance, updating the repeat offsets.
///
/// Values above 3 are a distance plus 3, and push the repeats down. Values 1
/// to 3 select a repeat offset — the first, second or third when the sequence
/// has literals, and shifted by one when it has none, so that "repeat the
/// previous match immediately" is not wasted on a code: with no literals, 1
/// means the second, 2 the third, and 3 the first minus one. A selected
/// repeat moves to the front. A distance of zero is corrupt.
fn resolve_offset(value: u64, literals: usize, repeats: &mut [usize; 3]) -> Option<usize> {
    let [first, second, third] = *repeats;
    if value > 3 {
        let offset = usize::try_from(value - 3).ok()?;
        *repeats = [offset, first, second];
        return Some(offset);
    }
    let index = usize::try_from(value)
        .ok()?
        .checked_sub(1)?
        .checked_add(usize::from(literals == 0))?;
    let offset = match index {
        0 => return Some(first),
        1 => second,
        2 => third,
        _ => first.checked_sub(1)?,
    };
    ensure(offset != 0)?;
    *repeats = if index == 1 {
        [second, first, third]
    } else {
        [offset, first, second]
    };
    Some(offset)
}
