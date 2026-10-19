//! `lzokay-native` as an independent oracle.
//!
//! The round trips in the parent module go through [`super::encoder`], which
//! was written from the same reading of the format as the decoder, so a
//! misreading shared by both would pass them. `lzokay-native` is a separate
//! implementation of LZO1X. Whatever it compresses must decode here to exactly
//! what went in, and each segment of a real `mkfs.btrfs` extent must decode
//! through it to exactly what this decoder produced.
//!
//! The btrfs framing around the segments is walked here by a few lines that
//! share nothing with `lzo.rs` either.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::{MIXED, MIXED_LEN, SECTOR};
use crate::compress::lzo::decompress;

/// The segments of a framed extent, found without the decoder under test.
fn walk(extent: &[u8], sector: usize) -> Vec<&[u8]> {
    let total = u32::from_le_bytes(extent[..4].try_into().unwrap()) as usize;
    let mut at = 4;
    let mut out = Vec::new();
    while at < total {
        let left = sector - at % sector;
        if left < 4 {
            at += left;
            continue;
        }
        let len = u32::from_le_bytes(extent[at..at + 4].try_into().unwrap()) as usize;
        out.push(&extent[at + 4..at + 4 + len]);
        at += 4 + len;
    }
    out
}

/// Frame `segments` the way btrfs does, padding to a whole sector.
fn frame(segments: &[Vec<u8>], sector: usize) -> Vec<u8> {
    let mut out = vec![0; 4];
    for segment in segments {
        out.extend_from_slice(&u32::try_from(segment.len()).unwrap().to_le_bytes());
        out.extend_from_slice(segment);
        let left = sector - out.len() % sector;
        if left < 4 {
            out.resize(out.len() + left, 0);
        }
    }
    let total = u32::try_from(out.len()).unwrap();
    out[..4].copy_from_slice(&total.to_le_bytes());
    out.resize(out.len().next_multiple_of(sector), 0);
    out
}

/// xorshift64*, so the inputs do not depend on a `rand` version.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A buffer mixing runs, text, near and far repeats and noise, so every
/// instruction form `lzokay` emits turns up.
fn sample(rng: &mut Rng, len: usize) -> Vec<u8> {
    let words: [&[u8]; 6] = [b"the ", b"btrfs ", b"sector ", b"segment ", b"lzo ", b"\n"];
    let mut out: Vec<u8> = Vec::with_capacity(len);
    let bias = rng.below(6);
    while out.len() < len {
        let kind = if rng.below(2) == 0 {
            bias
        } else {
            rng.below(6)
        };
        let piece = 1 + rng.below(2048);
        match kind {
            0 => out.extend(std::iter::repeat_n(0, piece)),
            1 => (0..piece).for_each(|_| out.push(b"ACGT"[rng.below(4)])),
            2 => (0..piece / 4).for_each(|_| out.extend_from_slice(words[rng.below(6)])),
            3 if !out.is_empty() => {
                let start = out.len().saturating_sub(1 + rng.below(0xC000));
                for i in 0..piece {
                    out.push(out[start + i]);
                }
            }
            4 => (0..piece).for_each(|_| out.push(rng.next() as u8)),
            _ => {
                let byte = rng.next() as u8;
                out.extend(std::iter::repeat_n(byte, piece));
            }
        }
    }
    out.truncate(len);
    out
}

#[test]
fn a_real_extent_decodes_the_same_through_lzokay() {
    let mut ours = vec![0u8; MIXED_LEN];
    assert_eq!(
        decompress(MIXED, &mut ours, SECTOR),
        Ok(MIXED_LEN),
        "the mkfs extent decodes here"
    );
    let mut theirs = Vec::new();
    for segment in walk(MIXED, SECTOR as usize) {
        theirs.extend(lzokay_native::decompress_all(segment, None).unwrap());
    }
    assert!(
        theirs == ours,
        "lzokay reads the same bytes out of every segment"
    );
}

#[test]
fn whatever_lzokay_compresses_decodes_to_its_input() {
    // Miri interprets every byte of both implementations, so it gets a sample
    // that still reaches all four sector sizes rather than the full sweep.
    let (cases, max_len) = if cfg!(miri) {
        (6, 9000)
    } else {
        (400, 128 * 1024)
    };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for case in 0..cases {
        let len = if case < 6 {
            1 + case * 1531
        } else {
            1 + rng.below(max_len)
        };
        let plain = sample(&mut rng, len);
        for sector in [512usize, 1024, 4096, 65536] {
            let segments: Vec<Vec<u8>> = plain
                .chunks(sector)
                .map(|chunk| lzokay_native::compress(chunk).unwrap())
                .collect();
            let extent = frame(&segments, sector);
            let mut out = vec![0u8; plain.len()];
            let got = decompress(&extent, &mut out, u32::try_from(sector).unwrap());
            if u32::from_le_bytes(extent[..4].try_into().unwrap()) > 128 * 1024 {
                assert!(got.is_err(), "case {case}: btrfs caps an extent at 128 KiB");
                continue;
            }
            assert_eq!(got, Ok(plain.len()), "case {case}, {sector}-byte sectors");
            assert!(
                out == plain,
                "case {case}, {sector}-byte sectors: same bytes"
            );
        }
    }
}
