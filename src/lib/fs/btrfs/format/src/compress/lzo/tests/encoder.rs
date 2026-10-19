//! A test-only LZO1X compressor, written from the format rather than from
//! either decoder.
//!
//! It is greedy and remembers one earlier position per hash slot, so its
//! output is larger than a real compressor's. That is not the point. The point
//! is that it reaches for every instruction the format has, including the two
//! short M1 forms and extended lengths, which a good compressor rarely emits
//! on small inputs. That way a round trip exercises all of the decoder, and
//! [`Stats`] lets a test prove that it did.
//!
//! It is not an independent oracle: it and the decoder share one reading of
//! the format. The real vectors from `mkfs.btrfs` are what pin that reading to
//! Linux's.

extern crate std;

use std::vec;
use std::vec::Vec;

/// How many of each instruction a compression emitted.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Stats {
    /// Literal runs, including the first-byte forms.
    pub literal_runs: usize,
    /// Lengths that continued into extension bytes.
    pub extended: usize,
    /// Two-byte M1 matches, after trailing literals.
    pub m1_short: usize,
    /// Three-byte M1 matches, after a literal run.
    pub m1_after_run: usize,
    /// M2 matches.
    pub m2: usize,
    /// M3 matches.
    pub m3: usize,
    /// M4 matches.
    pub m4: usize,
}

impl Stats {
    /// Add another compression's counts to these.
    pub(super) fn add(&mut self, other: Stats) {
        self.literal_runs += other.literal_runs;
        self.extended += other.extended;
        self.m1_short += other.m1_short;
        self.m1_after_run += other.m1_after_run;
        self.m2 += other.m2;
        self.m3 += other.m3;
        self.m4 += other.m4;
    }
}

const HASH_BITS: u32 = 14;
const MAX_DISTANCE: usize = 0xBFFF;

/// Compress `data` into one LZO1X stream.
pub(super) fn compress(data: &[u8]) -> (Vec<u8>, Stats) {
    let mut encoder = Encoder::default();
    let mut table = vec![usize::MAX; 1 << HASH_BITS];
    let mut literal_start = 0;
    let mut at = 0;
    while at + 3 <= data.len() {
        let slot = &mut table[hash(&data[at..at + 3])];
        let candidate = core::mem::replace(slot, at);
        if candidate == usize::MAX
            || at - candidate > MAX_DISTANCE
            || data[candidate..candidate + 3] != data[at..at + 3]
        {
            at += 1;
            continue;
        }
        let mut len = 3;
        while at + len < data.len() && data[candidate + len] == data[at + len] {
            len += 1;
        }
        encoder.literals(&data[literal_start..at]);
        let len = encoder.choose_length(at - candidate, len, at);
        encoder.matched(at - candidate, len);
        at += len;
        literal_start = at;
    }
    encoder.literals(&data[literal_start..]);
    encoder.out.extend_from_slice(&[0x11, 0, 0]);
    (encoder.out, encoder.stats)
}

fn hash(bytes: &[u8]) -> usize {
    let key = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
    (key.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

#[derive(Default)]
struct Encoder {
    out: Vec<u8>,
    /// The decoder's state after what has been emitted so far.
    state: usize,
    /// Whether anything has been emitted, which decides the first-byte forms.
    started: bool,
    /// The byte holding the last match's trailing-literal bits, while no
    /// literals have followed it.
    patch: Option<usize>,
    stats: Stats,
}

impl Encoder {
    /// Shorten a match now and then so the M1 forms get used.
    fn choose_length(&self, distance: usize, len: usize, at: usize) -> usize {
        if (1..=3).contains(&self.state) && distance <= 0x400 && at.is_multiple_of(5) {
            2
        } else if self.state == 4 && (0x801..=0xC00).contains(&distance) && at.is_multiple_of(3) {
            3
        } else {
            len
        }
    }

    fn literals(&mut self, literals: &[u8]) {
        let n = literals.len();
        if n == 0 {
            return;
        }
        if !self.started && n <= 238 {
            self.out.push(17 + n as u8);
            self.state = n.min(4);
            self.stats.literal_runs += 1;
        } else if n <= 3 {
            let at = self.patch.expect("a few literals only ever follow a match");
            self.out[at] |= n as u8;
            self.state = n;
        } else {
            self.length(0, 15, n - 3);
            self.state = 4;
            self.stats.literal_runs += 1;
        }
        self.started = true;
        self.patch = None;
        self.out.extend_from_slice(literals);
    }

    fn matched(&mut self, distance: usize, len: usize) {
        match (self.state, distance, len) {
            (1..=3, 1..=0x400, 2) => {
                self.short(distance - 1);
                self.stats.m1_short += 1;
            }
            (4, 0x801..=0xC00, 3) => {
                self.short(distance - 0x801);
                self.stats.m1_after_run += 1;
            }
            (_, 1..=0x800, 3..=8) => {
                let d = distance - 1;
                self.patched(((len - 1) << 5) as u8 | ((d & 7) << 2) as u8);
                self.out.push((d >> 3) as u8);
                self.stats.m2 += 1;
            }
            (_, 1..=0x4000, _) => {
                self.length(32, 31, len - 2);
                self.word((distance - 1) << 2);
                self.stats.m3 += 1;
            }
            _ => {
                let d = distance - 0x4000;
                self.length(16 | ((d >> 14) << 3) as u8, 7, len - 2);
                self.word((d & 0x3FFF) << 2);
                self.stats.m4 += 1;
            }
        }
        self.started = true;
        self.state = 0;
    }

    /// Both M1 forms: `0000DDSS` then the high bits of the distance.
    fn short(&mut self, d: usize) {
        self.patched(((d & 3) << 2) as u8);
        self.out.push((d >> 2) as u8);
    }

    /// A distance word, whose low byte carries the trailing-literal bits.
    fn word(&mut self, value: usize) {
        self.patched(value as u8);
        self.out.push((value >> 8) as u8);
    }

    fn patched(&mut self, byte: u8) {
        self.patch = Some(self.out.len());
        self.out.push(byte);
    }

    /// An instruction byte whose length field is `max` bits wide, extended if
    /// `value` does not fit it.
    fn length(&mut self, marker: u8, max: usize, value: usize) {
        if (1..=max).contains(&value) {
            self.out.push(marker | value as u8);
            return;
        }
        self.out.push(marker);
        let mut rest = value - max;
        while rest > 255 {
            self.out.push(0);
            rest -= 255;
        }
        self.out.push(rest as u8);
        self.stats.extended += 1;
    }
}
