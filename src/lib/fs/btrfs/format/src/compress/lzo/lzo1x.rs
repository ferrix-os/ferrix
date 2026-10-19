//! One LZO1X stream, decoded into a slice that is also its whole window.
//!
//! An LZO1X stream is a sequence of instructions. Each one either copies a run
//! of literal bytes out of the stream, or copies `len` bytes starting
//! `distance` bytes back in the output (a match). There are no Huffman codes
//! and no bit I/O. What makes it fiddly is that the meaning of an instruction
//! byte depends on the byte *and* on the instruction before it. That context
//! is the `state`: the number of literals the previous match appended (0 to
//! 3), or 4 straight after a literal run.
//!
//! Every match carries two low bits `SS` saying how many literals (0 to 3)
//! follow it without an instruction of their own. For M3 and M4 they are the
//! low bits of the little-endian distance word. Those bits become the next
//! `state`.
//!
//! | Instruction byte     | State  | Meaning     | Length          | Distance                 |
//! |----------------------|--------|-------------|-----------------|--------------------------|
//! | `0000LLLL`           | 0      | literal run | `L + 3`         |                          |
//! | `0000DDSS` `H`       | 1 to 3 | M1 match    | 2               | `(H << 2) + D + 1`       |
//! | `0000DDSS` `H`       | 4      | M1 match    | 3               | `(H << 2) + D + 2049`    |
//! | `LLLDDDSS` `H`       | any    | M2 match    | `L + 1` (3..=8) | `(H << 3) + D + 1`       |
//! | `001LLLLL` `W`       | any    | M3 match    | `L + 2`         | `(W >> 2) + 1`           |
//! | `0001HLLL` `W`       | any    | M4 match    | `L + 2`         | `(H << 14) + (W >> 2) + 16384` |
//!
//! A length field of zero means the length continues in extension bytes after
//! the instruction byte: some zero bytes worth 255 each, then one non-zero
//! byte added as it is. The base is then 18 for a literal run, 33 for M3 and 9
//! for M4. An M4 with `H` and `W >> 2` both zero is not a match but the end of
//! the stream. Its only valid form is `11 00 00`, and nothing may follow it.
//!
//! # The first byte
//!
//! A stream starts with nothing behind it, so the byte values that would
//! otherwise be an M4 into nothing are reused. A first byte of 18 to 20 copies
//! 1 to 3 literals (state = that count). A first byte of 21 to 255 copies 4 to
//! 238 literals (state 4). A first byte of 17, when at least five bytes are
//! present, is not an instruction at all: it and the byte after it announce a
//! bitstream version. Linux's LZO-RLE writes version 1, which adds one
//! instruction, a zero run hidden in the M4 space: `00011RRR`, then `W` with
//! `W >> 2 == 0x3FFF`, then a byte `R'`, writes `(R' << 3 | RRR) + 4` zeroes.
//! btrfs never writes that version, but Linux's decoder, and so Linux's btrfs,
//! reads it, so this decoder reads it too rather than refusing a stream Linux
//! would accept.
//!
//! # Totality
//!
//! Every read goes through [`u8_at`] or [`u16_at`] and every write through
//! [`slice::get_mut`], so a length or distance off the end of either slice is
//! `None`, never an index out of bounds. Every instruction consumes at least
//! one input byte or fails, so the main loop ends. A match whose distance
//! reaches before the start of the slice is refused, which is what keeps one
//! btrfs segment from reading another's output (see [`super`]).
//!
//! Like Linux, the decoder also insists that at least three input bytes
//! remain after every literal copy and after every match, because some
//! instruction must follow, and the shortest one is the end marker. A
//! well-formed stream always satisfies this, so the check only ever refuses a
//! stream that would have run out of input anyway.

use crate::{u8_at, u16_at};

/// The longest distance an M2 match can express, and the base of the long
/// form of M1.
const M2_MAX_DISTANCE: usize = 0x800;

/// The longest distance an M3 match can express, and the base of M4.
const M3_MAX_DISTANCE: usize = 0x4000;

/// Input bytes that must remain after a literal copy or a match: the length of
/// the end marker, the shortest instruction there is.
const LOOKAHEAD: usize = 3;

/// The shortest zero run LZO-RLE encodes.
const MIN_ZERO_RUN: usize = 4;

/// The state after a literal run.
const AFTER_RUN: usize = 4;

/// Decode one complete LZO1X stream into `output`.
///
/// Returns the number of bytes written, or `None` if the stream is malformed,
/// does not fit `output`, or has input left over after its end marker.
pub(super) fn decompress(input: &[u8], output: &mut [u8]) -> Option<usize> {
    if input.len() < LOOKAHEAD {
        return None;
    }
    let mut stream = Stream {
        input,
        ip: 0,
        output,
        op: 0,
        zero_runs: false,
    };
    let mut state = stream.start()?;
    loop {
        match stream.instruction(state)? {
            Next::State(next) => state = next,
            Next::End => return Some(stream.op),
        }
    }
}

/// What follows a decoded instruction.
enum Next {
    /// Another instruction, read in this state.
    State(usize),
    /// Nothing: the end marker was read and the input is exhausted.
    End,
}

/// A stream being decoded: both slices and a cursor into each.
struct Stream<'a> {
    /// The compressed stream.
    input: &'a [u8],
    /// The next byte of `input` to read.
    ip: usize,
    /// Where the stream expands to, which is also every match's window.
    output: &'a mut [u8],
    /// The next byte of `output` to write.
    op: usize,
    /// The stream declared a non-zero bitstream version, so LZO-RLE's zero run
    /// is an instruction rather than an M4 match.
    zero_runs: bool,
}

impl Stream<'_> {
    /// Read the optional version prefix and the first-byte literal forms, and
    /// return the state the first ordinary instruction is read in.
    fn start(&mut self) -> Option<usize> {
        if self.input.len() >= 5 && u8_at(self.input, 0)? == 17 {
            self.zero_runs = u8_at(self.input, 1)? != 0;
            self.ip = 2;
        }
        let first = usize::from(u8_at(self.input, self.ip)?);
        let Some(count) = first.checked_sub(17).filter(|&count| count > 0) else {
            return Some(0);
        };
        self.ip = self.ip.checked_add(1)?;
        self.literals(count)?;
        Some(count.min(AFTER_RUN))
    }

    /// Decode one instruction read in `state`.
    fn instruction(&mut self, state: usize) -> Option<Next> {
        let t = self.byte()?;
        match t {
            0..=15 if state == 0 => self.literal_run(t),
            0..=15 => self.m1(t, state),
            16..=31 => self.m4(t),
            32..=63 => self.m3(t),
            _ => self.m2(t),
        }
    }

    /// A literal run: `t + 3` literals, or an extended count when `t` is zero.
    fn literal_run(&mut self, t: usize) -> Option<Next> {
        let len = if t == 0 { self.extended(15)? } else { t };
        self.literals(len.checked_add(3)?)?;
        Some(Next::State(AFTER_RUN))
    }

    /// M1: a two-byte match after trailing literals, or a three-byte match
    /// just beyond M2's reach after a literal run.
    fn m1(&mut self, t: usize, state: usize) -> Option<Next> {
        let high = self.byte()?;
        let (base, len) = if state == AFTER_RUN {
            (M2_MAX_DISTANCE + 1, 3)
        } else {
            (1, 2)
        };
        self.matched(base + (t >> 2) + (high << 2), len, t & 3)
    }

    /// M2: a match of 3 to 8 bytes within 2 KiB.
    fn m2(&mut self, t: usize) -> Option<Next> {
        let high = self.byte()?;
        self.matched(1 + ((t >> 2) & 7) + (high << 3), (t >> 5) + 1, t & 3)
    }

    /// M3: a match of any length within 16 KiB.
    fn m3(&mut self, t: usize) -> Option<Next> {
        let len = match t & 31 {
            0 => self.extended(31)?,
            short => short,
        };
        let word = self.le16()?;
        self.matched((word >> 2) + 1, len.checked_add(2)?, word & 3)
    }

    /// M4: a match of any length from 16 KiB to 48 KiB back. Also the end
    /// marker, and in LZO-RLE the zero run.
    fn m4(&mut self, t: usize) -> Option<Next> {
        if self.zero_runs && t & 0xF8 == 0x18 {
            let word = usize::from(u16_at(self.input, self.ip)?);
            if word & 0xFFFC == 0xFFFC {
                return self.zero_run(t, word);
            }
        }
        let len = match t & 7 {
            0 => self.extended(7)?,
            short => short,
        };
        let len = len.checked_add(2)?;
        let word = self.le16()?;
        let distance = ((t & 8) << 11) + (word >> 2);
        if distance == 0 {
            return self.end(len);
        }
        self.matched(distance + M3_MAX_DISTANCE, len, word & 3)
    }

    /// LZO-RLE's zero run. `word` has been peeked, not consumed.
    fn zero_run(&mut self, t: usize, word: usize) -> Option<Next> {
        let high = usize::from(u8_at(self.input, self.ip.checked_add(2)?)?);
        self.ip = self.ip.checked_add(3)?;
        let len = ((high << 3) | (t & 7)) + MIN_ZERO_RUN;
        let end = self.op.checked_add(len)?;
        self.output.get_mut(self.op..end)?.fill(0);
        self.op = end;
        self.trailing(word & 3)
    }

    /// The end marker: valid only in its three-byte form and only as the last
    /// thing in the input.
    fn end(&self, len: usize) -> Option<Next> {
        (len == 3 && self.ip == self.input.len()).then_some(Next::End)
    }

    /// Copy a match, then the literals its low bits promise.
    fn matched(&mut self, distance: usize, len: usize, trailing: usize) -> Option<Next> {
        self.copy_match(distance, len)?;
        self.trailing(trailing)
    }

    /// Copy the `count` literals that follow a match without an instruction
    /// of their own; `count` is the next state.
    fn trailing(&mut self, count: usize) -> Option<Next> {
        self.literals(count)?;
        Some(Next::State(count))
    }

    /// Read a length continued in extension bytes: `base`, plus 255 for each
    /// zero byte, plus the non-zero byte that ends them.
    fn extended(&mut self, base: usize) -> Option<usize> {
        let start = self.ip;
        while u8_at(self.input, self.ip)? == 0 {
            self.ip = self.ip.checked_add(1)?;
        }
        let zeros = self.ip.checked_sub(start)?;
        let last = self.byte()?;
        zeros.checked_mul(255)?.checked_add(base)?.checked_add(last)
    }

    /// Copy `len` literals from the input, leaving [`LOOKAHEAD`] bytes behind.
    fn literals(&mut self, len: usize) -> Option<()> {
        let end = self.ip.checked_add(len)?;
        if end.checked_add(LOOKAHEAD)? > self.input.len() {
            return None;
        }
        let out_end = self.op.checked_add(len)?;
        let from = self.input.get(self.ip..end)?;
        self.output.get_mut(self.op..out_end)?.copy_from_slice(from);
        self.ip = end;
        self.op = out_end;
        Some(())
    }

    /// Copy `len` bytes from `distance` back in the output.
    ///
    /// A match may overlap what it writes: distance 1 and length 100 is a run
    /// of one byte. The copy goes in chunks from a fixed source start, and each
    /// chunk is as long as the gap between that start and the write position.
    /// That gap is always a multiple of `distance`, so each chunk continues the
    /// period exactly, and it doubles each time, so a long run costs a
    /// logarithmic number of copies rather than one per byte.
    fn copy_match(&mut self, distance: usize, len: usize) -> Option<()> {
        if distance == 0 {
            return None;
        }
        let from = self.op.checked_sub(distance)?;
        let end = self.op.checked_add(len)?;
        if end > self.output.len() {
            return None;
        }
        while self.op < end {
            let op = self.op;
            let chunk = op.checked_sub(from)?.min(end.checked_sub(op)?);
            let (behind, ahead) = self.output.split_at_mut_checked(op)?;
            let source = behind.get(from..from.checked_add(chunk)?)?;
            ahead.get_mut(..chunk)?.copy_from_slice(source);
            self.op = op.checked_add(chunk)?;
        }
        Some(())
    }

    /// Read one byte.
    fn byte(&mut self) -> Option<usize> {
        let byte = u8_at(self.input, self.ip)?;
        self.ip = self.ip.checked_add(1)?;
        Some(usize::from(byte))
    }

    /// Read a little-endian 16-bit word.
    fn le16(&mut self) -> Option<usize> {
        let word = u16_at(self.input, self.ip)?;
        self.ip = self.ip.checked_add(2)?;
        Some(usize::from(word))
    }
}
