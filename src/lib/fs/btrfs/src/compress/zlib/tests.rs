//! Tests for the zlib decoder.
//!
//! Three kinds of evidence, each catching what the others cannot:
//!
//! - **What mkfs wrote.** Extents lifted from an image made with
//!   `mkfs.btrfs --compress zlib`, so the framing — the inline payload, the
//!   sector padding after a regular extent — is the real thing rather than a
//!   belief about it.
//! - **An independent implementation.** `miniz_oxide` compresses seeded inputs
//!   of every entropy at every level, which between them produce stored, fixed
//!   and dynamic blocks, and the decoder must reproduce each input exactly.
//! - **Streams built bit by bit.** Every rule the decoder enforces has a stream
//!   here that breaks exactly that rule, written with a bit writer and a
//!   canonical-code builder that share no code with the decoder.
//!
//! On top of those, every truncation and a sweep of bit flips must come back as
//! an error or as a length that fits, which is the totality claim made
//! concrete.

extern crate std;

use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

use miniz_oxide::deflate::compress_to_vec_zlib;
use miniz_oxide::inflate::{decompress_to_vec_with_limit, decompress_to_vec_zlib};

use super::decompress;
use crate::BtrfsError;
use crate::compress::{self, MAX_UNCOMPRESSED, zstd};
use crate::items::COMPRESS_ZLIB;

const BAD: BtrfsError = BtrfsError::BadCompressedData {
    compression: COMPRESS_ZLIB,
};

/// Decode into a buffer of `capacity`, checking the length claim on success.
fn decode(stream: &[u8], capacity: usize) -> Result<Vec<u8>, BtrfsError> {
    let mut out = vec![0u8; capacity];
    let written = decompress(stream, &mut out)?;
    out.truncate(fits(written, capacity));
    Ok(out)
}

/// `written`, once the decoder's claim that it fits has been checked.
fn fits(written: usize, capacity: usize) -> usize {
    assert!(written <= capacity, "wrote {written} into {capacity} bytes");
    written
}

/// `line` repeated and cut to `len` bytes, as `yes LINE | head -c LEN` prints.
fn yes(line: &str, len: usize) -> Vec<u8> {
    let mut with_newline = String::from(line);
    with_newline.push('\n');
    with_newline.bytes().cycle().take(len).collect()
}

// ---------------------------------------------------------------------------
// Extents written by mkfs.btrfs
// ---------------------------------------------------------------------------

/// An inline extent's payload, exactly as it sits in the leaf.
const INLINE: &[u8] = include_bytes!("../testdata/zlib/inline.txt@0.inline.bin");

/// The last extent of a 500 KiB text file: a stream padded to a sector.
const TEXT_REGULAR: &[u8] = include_bytes!("../testdata/zlib/text.txt@393216.regular.bin");

/// The last extent of a file mixing text with incompressible bytes.
const MIXED_REGULAR: &[u8] = include_bytes!("../testdata/zlib/mixed.txt@262144.regular.bin");

/// `ram_bytes` of the mixed extent.
const MIXED_RAM_BYTES: usize = 52_616;

#[test]
fn decodes_an_inline_extent_mkfs_wrote() {
    let plain = yes("inline me", 1800);
    assert_eq!(decode(INLINE, plain.len()), Ok(plain), "inline extent");
}

#[test]
fn decodes_a_sector_padded_regular_extent_mkfs_wrote() {
    let file = yes("compressible line of text", 500_000);
    let plain = &file[393_216..];
    assert!(
        TEXT_REGULAR.ends_with(&[0; 64]),
        "the fixture should carry its sector padding"
    );
    assert!(
        decode(TEXT_REGULAR, plain.len()) == Ok(plain.to_vec()),
        "regular text extent"
    );
}

#[test]
fn decodes_a_mixed_extent_as_miniz_oxide_does() {
    let expected = decompress_to_vec_zlib(MIXED_REGULAR).expect("miniz_oxide decodes the fixture");
    assert_eq!(expected.len(), MIXED_RAM_BYTES, "oracle length");
    assert!(
        decode(MIXED_REGULAR, MIXED_RAM_BYTES) == Ok(expected),
        "mixed extent"
    );
}

#[test]
fn a_larger_buffer_is_a_short_read_not_an_error() {
    let plain = yes("inline me", 1800);
    let mut out = vec![0xAAu8; MAX_UNCOMPRESSED];
    assert_eq!(decompress(INLINE, &mut out), Ok(plain.len()), "length");
    assert_eq!(&out[..plain.len()], &plain[..], "contents");
}

#[test]
fn the_compress_entry_point_routes_zlib_here() {
    let plain = yes("inline me", 1800);
    let mut out = vec![0u8; plain.len()];
    let mut bytes = vec![0u8; zstd::Workspace::SIZE];
    let mut workspace = zstd::Workspace::new(&mut bytes).unwrap();
    let written = compress::decompress(COMPRESS_ZLIB, INLINE, &mut out, 4096, &mut workspace);
    assert_eq!(written, Ok(plain.len()), "length");
    assert_eq!(out, plain, "contents");
}

// ---------------------------------------------------------------------------
// Seeded inputs
// ---------------------------------------------------------------------------

/// `SplitMix64`: enough to vary inputs reproducibly, nothing more.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }
}

/// How many kinds of input [`sample`] makes.
const KINDS: usize = 5;

/// A seeded input of `len` bytes whose entropy depends on `kind`.
fn sample(rng: &mut Rng, kind: usize, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        match kind {
            // Incompressible: stored blocks, or Huffman blocks of literals.
            0 => out.push(rng.byte()),
            // A four-letter alphabet: short codes and short matches.
            1 => out.push(b"ACGT"[rng.below(4)]),
            // Runs: back-references that overlap their own output.
            2 => push_run(rng, &mut out),
            // Words: matches at every distance a sentence produces.
            3 => push_word(rng, &mut out),
            // Copies from far back with noise: long distances, long codes.
            _ => push_far_copy(rng, &mut out),
        }
    }
    out.truncate(len);
    out
}

fn push_run(rng: &mut Rng, out: &mut Vec<u8>) {
    let byte = rng.byte();
    let run = 1 + rng.below(300);
    out.extend(core::iter::repeat_n(byte, run));
}

fn push_word(rng: &mut Rng, out: &mut Vec<u8>) {
    const WORDS: [&str; 8] = [
        "btrfs", "extent", "inflate", "sector", "the", "of", "window", "kernel",
    ];
    out.extend_from_slice(WORDS[rng.below(WORDS.len())].as_bytes());
    out.push(if rng.below(10) == 0 { b'\n' } else { b' ' });
}

fn push_far_copy(rng: &mut Rng, out: &mut Vec<u8>) {
    if out.len() < 64 || rng.below(4) == 0 {
        out.extend((0..32).map(|_| rng.byte()));
        return;
    }
    let distance = 1 + rng.below(out.len().min(40_000));
    let start = out.len() - distance;
    let len = 3 + rng.below(300);
    for i in 0..len {
        let byte = out[start + i % distance];
        out.push(byte);
    }
}

/// Input lengths for the differential test: the edges of stored-block size,
/// of the window, and of btrfs's extent cap.
#[cfg(not(miri))]
const LENGTHS: &[usize] = &[
    0, 1, 2, 3, 258, 1000, 4096, 32_768, 65_535, 65_536, 65_537, 100_000, 131_072,
];
#[cfg(miri)]
const LENGTHS: &[usize] = &[0, 1, 300];

#[cfg(not(miri))]
const LEVELS: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
#[cfg(miri)]
const LEVELS: &[u8] = &[0, 1, 10];

/// The type of the first block of a zlib stream.
fn first_block_type(stream: &[u8]) -> usize {
    usize::from((stream[2] >> 1) & 3)
}

#[test]
fn reproduces_every_input_miniz_oxide_compresses() {
    let mut rng = Rng(0x5EED);
    let mut seen = [false; 3];
    for kind in 0..KINDS {
        for &len in LENGTHS {
            let plain = sample(&mut rng, kind, len);
            seen = roundtrip_all_levels(&plain, kind, seen);
        }
    }
    assert_eq!(
        seen, [true; 3],
        "stored, fixed and dynamic blocks all tested"
    );
}

/// Compress `plain` at every level and decode each, noting block types seen.
fn roundtrip_all_levels(plain: &[u8], kind: usize, mut seen: [bool; 3]) -> [bool; 3] {
    for &level in LEVELS {
        let stream = compress_to_vec_zlib(plain, level);
        if let Some(slot) = seen.get_mut(first_block_type(&stream)) {
            *slot = true;
        }
        let len = plain.len();
        assert!(
            decode(&stream, len).as_deref() == Ok(plain),
            "kind {kind}, {len} bytes, level {level}"
        );
    }
    seen
}

// ---------------------------------------------------------------------------
// btrfs framing and the trailer
// ---------------------------------------------------------------------------

/// A stream of text at level 6, and its plaintext.
fn text_stream() -> (Vec<u8>, Vec<u8>) {
    let plain = yes("what follows the final block is not read", 3000);
    (compress_to_vec_zlib(&plain, 6), plain)
}

#[test]
fn ignores_sector_padding_and_garbage_after_the_stream() {
    let (stream, plain) = text_stream();
    let mut padded = stream.clone();
    padded.resize(4096, 0);
    assert_eq!(decode(&padded, plain.len()), Ok(plain.clone()), "zeros");
    let mut garbage = stream;
    garbage.extend_from_slice(&[0xFF; 100]);
    assert_eq!(decode(&garbage, plain.len()), Ok(plain), "garbage");
}

#[test]
fn does_not_check_the_adler32_trailer_as_linux_does_not() {
    let (stream, plain) = text_stream();
    let mut wrong = stream.clone();
    let last = wrong.len() - 1;
    wrong[last] ^= 0x01;
    assert_eq!(
        decode(&wrong, plain.len()),
        Ok(plain.clone()),
        "bad trailer"
    );
    let missing = &stream[..stream.len() - 4];
    assert_eq!(decode(missing, plain.len()), Ok(plain), "no trailer");
}

#[test]
fn a_small_declared_window_does_not_limit_distances() {
    let (mut stream, plain) = text_stream();
    // CINFO 0 declares a 256-byte window; the text repeats every 41 bytes but
    // level 6 matches much further back. Linux passes this on to zlib, which
    // enforces it only across its page-sized output chunks.
    stream[..2].copy_from_slice(&header(0x08, 0));
    assert_eq!(decode(&stream, plain.len()), Ok(plain), "CINFO 0");
}

// ---------------------------------------------------------------------------
// The zlib header
// ---------------------------------------------------------------------------

/// `CMF` and `FLG` with the check bits filled in so the pair divides by 31.
fn header(cmf: u8, flags: u8) -> [u8; 2] {
    let base = u16::from(cmf) << 8 | u16::from(flags & 0xE0);
    let check = (31 - base % 31) % 31;
    [cmf, flags & 0xE0 | check as u8]
}

/// A header followed by one fixed block holding `hi`.
fn with_header(head: [u8; 2]) -> Vec<u8> {
    let mut w = BitWriter::new(&head);
    w.bits(1, 1);
    w.bits(1, 2);
    fixed_literal(&mut w, b'h');
    fixed_literal(&mut w, b'i');
    w.code(0, 7);
    w.finish()
}

#[test]
fn accepts_every_window_size_up_to_32k() {
    for cinfo in 0..=7u8 {
        let stream = with_header(header(cinfo << 4 | 8, 0));
        assert_eq!(decode(&stream, 16), Ok(b"hi".to_vec()), "CINFO {cinfo}");
    }
    let compression_level_bits = header(0x78, 0xC0);
    assert_eq!(
        decode(&with_header(compression_level_bits), 16),
        Ok(b"hi".to_vec()),
        "FLEVEL is informational"
    );
}

#[test]
fn refuses_a_header_linux_refuses() {
    let cases: [(&str, [u8; 2]); 5] = [
        ("method 7", header(0x77, 0)),
        ("method 15", header(0x7F, 0)),
        ("a 64 KiB window", header(0x88, 0)),
        ("a preset dictionary", header(0x78, 0x20)),
        ("a bad check", [0x78, 0x9D]),
    ];
    for (what, head) in cases {
        assert_eq!(decode(&with_header(head), 16), Err(BAD), "{what}");
    }
    for short in [&[][..], &[0x78], &[0x78, 0x9C]] {
        assert_eq!(decode(short, 16), Err(BAD), "{} bytes", short.len());
    }
}

// ---------------------------------------------------------------------------
// Building streams by hand
// ---------------------------------------------------------------------------

/// Writes DEFLATE's bit order: numbers low bit first, Huffman codes high bit
/// first.
struct BitWriter {
    bytes: Vec<u8>,
    used: u32,
}

impl BitWriter {
    fn new(prefix: &[u8]) -> Self {
        BitWriter {
            bytes: prefix.to_vec(),
            used: 8,
        }
    }

    fn zlib() -> Self {
        Self::new(&[0x78, 0x01])
    }

    fn bit(&mut self, bit: u32) {
        if self.used == 8 {
            self.bytes.push(0);
            self.used = 0;
        }
        *self.bytes.last_mut().unwrap() |= ((bit & 1) as u8) << self.used;
        self.used += 1;
    }

    fn bits(&mut self, value: u32, count: u32) {
        (0..count).for_each(|i| self.bit(value >> i));
    }

    fn code(&mut self, code: u32, len: u32) {
        (0..len).rev().for_each(|i| self.bit(code >> i));
    }

    fn align(&mut self) {
        self.used = 8;
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.align();
        self.bytes.extend_from_slice(bytes);
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// The canonical code for each symbol, from RFC 1951 §3.2.2 as written.
fn canonical(lengths: &[u8]) -> Vec<u32> {
    let mut bl_count = [0u32; 16];
    lengths
        .iter()
        .for_each(|&len| bl_count[usize::from(len)] += 1);
    bl_count[0] = 0;
    let mut next_code = [0u32; 16];
    let mut code = 0;
    for bits in 1..16 {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    lengths
        .iter()
        .map(|&len| {
            let code = next_code[usize::from(len)];
            next_code[usize::from(len)] += 1;
            code
        })
        .collect()
}

/// Write a literal with the fixed code.
fn fixed_literal(w: &mut BitWriter, byte: u8) {
    match byte {
        0..=143 => w.code(0x30 + u32::from(byte), 8),
        _ => w.code(0x190 + u32::from(byte) - 144, 9),
    }
}

/// Write a literal/length symbol of 256 or above with the fixed code.
fn fixed_symbol(w: &mut BitWriter, symbol: u32) {
    match symbol {
        256..=279 => w.code(symbol - 256, 7),
        _ => w.code(0xC0 + symbol - 280, 8),
    }
}

/// A final fixed block: `prefix` literals, then `symbol` and a five-bit
/// distance code, then end-of-block.
fn fixed_match(prefix: &[u8], symbol: u32, distance_code: u32) -> Vec<u8> {
    let mut w = BitWriter::zlib();
    w.bits(1, 1);
    w.bits(1, 2);
    prefix.iter().for_each(|&byte| fixed_literal(&mut w, byte));
    fixed_symbol(&mut w, symbol);
    w.code(distance_code, 5);
    fixed_symbol(&mut w, 256);
    w.finish()
}

/// A final stored block holding `data`, with `nlen` as its complement field.
fn stored(data: &[u8], nlen: u16) -> Vec<u8> {
    let mut w = BitWriter::zlib();
    w.bits(1, 1);
    w.bits(0, 2);
    let len = data.len() as u16;
    w.bytes(&len.to_le_bytes());
    w.bytes(&nlen.to_le_bytes());
    w.bytes(data);
    w.finish()
}

#[test]
fn fixed_blocks_decode_literals_and_matches() {
    assert_eq!(
        decode(&with_header([0x78, 0x01]), 2),
        Ok(b"hi".to_vec()),
        "literals"
    );
    // Length symbol 257 is 3 bytes; distance code 0 is distance 1.
    assert_eq!(
        decode(&fixed_match(b"a", 257, 0), 8),
        Ok(b"aaaa".to_vec()),
        "run"
    );
    // 264 is 10 bytes, distance code 1 is distance 2.
    let expected = b"ababababababab".to_vec();
    assert_eq!(
        decode(&fixed_match(b"abab", 264, 1), 16),
        Ok(expected),
        "overlap"
    );
}

/// Verifies: L.btrfs.5
#[test]
fn refuses_a_distance_before_the_start_of_the_output() {
    // Distance code 1 is 2 back, with one byte written.
    assert_eq!(decode(&fixed_match(b"a", 257, 1), 8), Err(BAD), "one past");
    assert_eq!(
        decode(&fixed_match(b"", 257, 0), 8),
        Err(BAD),
        "nothing written"
    );
    // Distance code 29 with no extra bits set is 24577 back.
    assert_eq!(
        decode(&fixed_match(b"abc", 257, 29), 8),
        Err(BAD),
        "far past"
    );
}

#[test]
fn refuses_symbols_the_fixed_code_assigns_but_deflate_does_not() {
    for symbol in [286, 287] {
        let stream = fixed_match(b"a", symbol, 0);
        assert_eq!(decode(&stream, 8), Err(BAD), "length symbol {symbol}");
    }
    for code in [30, 31] {
        let stream = fixed_match(b"a", 257, code);
        assert_eq!(decode(&stream, 8), Err(BAD), "distance symbol {code}");
    }
}

#[test]
fn refuses_the_reserved_block_type() {
    let mut w = BitWriter::zlib();
    w.bits(1, 1);
    w.bits(3, 2);
    w.bits(0, 16);
    assert_eq!(decode(&w.finish(), 8), Err(BAD), "BTYPE 3");
}

#[test]
fn stored_blocks_check_nlen_and_their_length() {
    assert_eq!(
        decode(&stored(b"hello", !5), 5),
        Ok(b"hello".to_vec()),
        "valid"
    );
    assert_eq!(decode(&stored(b"hello", !5 ^ 1), 5), Err(BAD), "bad NLEN");
    assert_eq!(
        decode(&stored(b"hello", 5), 5),
        Err(BAD),
        "NLEN not inverted"
    );
    let short = stored(b"hello", !5);
    assert_eq!(
        decode(&short[..short.len() - 1], 5),
        Err(BAD),
        "data cut short"
    );
    assert_eq!(decode(&stored(b"", !0), 5), Ok(Vec::new()), "empty");
}

#[test]
fn a_stored_block_after_a_partial_byte_starts_on_the_boundary() {
    let mut w = BitWriter::zlib();
    w.bits(0, 1);
    w.bits(1, 2);
    fixed_literal(&mut w, b'x');
    fixed_symbol(&mut w, 256);
    w.bits(1, 1);
    w.bits(0, 2);
    w.bytes(&[2, 0, !2, 0xFF, b'y', b'z']);
    assert_eq!(
        decode(&w.finish(), 8),
        Ok(b"xyz".to_vec()),
        "fixed then stored"
    );
}

/// Verifies: L.btrfs.5
#[test]
fn refuses_output_that_does_not_fit() {
    assert_eq!(decode(&stored(b"hello", !5), 4), Err(BAD), "stored");
    assert_eq!(
        decode(&stored(b"hello", !5), 6),
        Ok(b"hello".to_vec()),
        "room"
    );
    assert_eq!(decode(&with_header([0x78, 0x01]), 1), Err(BAD), "literal");
    assert_eq!(decode(&fixed_match(b"a", 257, 0), 3), Err(BAD), "match");
    assert_eq!(
        decode(&fixed_match(b"a", 257, 0), 0),
        Err(BAD),
        "no room at all"
    );
}

// ---------------------------------------------------------------------------
// Dynamic blocks
// ---------------------------------------------------------------------------

/// Code-length code lengths by symbol: 0–15 get five bits, 16 two, 17 and 18
/// three. That is 16/32 + 1/4 + 2/8, exactly complete.
const CODE_LENGTHS: [u8; 19] = [5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 2, 3, 3];

const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// A dynamic block header: counts, the code-length code `cl`, then `runs` as
/// code-length symbols with their extra-bit values.
fn dynamic_header(w: &mut BitWriter, nlen: u32, ndist: u32, cl: &[u8; 19], runs: &[(u32, u32)]) {
    w.bits(nlen - 257, 5);
    w.bits(ndist - 1, 5);
    w.bits(19 - 4, 4);
    CODE_LENGTH_ORDER
        .iter()
        .for_each(|&symbol| w.bits(u32::from(cl[symbol]), 3));
    let codes = canonical(cl);
    for &(symbol, extra) in runs {
        let symbol = symbol as usize;
        w.code(codes[symbol], u32::from(cl[symbol]));
        let extra_bits = [2, 3, 7].get(symbol.wrapping_sub(16)).copied().unwrap_or(0);
        w.bits(extra, extra_bits);
    }
}

/// Every length sent as itself, no repeats.
fn literally(lengths: &[u8]) -> Vec<(u32, u32)> {
    lengths.iter().map(|&len| (u32::from(len), 0)).collect()
}

/// Literal/length lengths for 257 symbols with the given symbols set.
fn litlen_lengths(set: &[(usize, u8)]) -> Vec<u8> {
    let mut lengths = vec![0u8; 257];
    set.iter().for_each(|&(symbol, len)| lengths[symbol] = len);
    lengths
}

/// A final dynamic block using `litlen` and `dist` lengths, whose body is
/// written by `body` given each code.
fn dynamic(litlen: &[u8], dist: &[u8], body: impl Fn(&mut BitWriter, &[u32], &[u32])) -> Vec<u8> {
    let mut w = BitWriter::zlib();
    w.bits(1, 1);
    w.bits(2, 2);
    let mut all = litlen.to_vec();
    all.extend_from_slice(dist);
    dynamic_header(
        &mut w,
        litlen.len() as u32,
        dist.len() as u32,
        &CODE_LENGTHS,
        &literally(&all),
    );
    body(&mut w, &canonical(litlen), &canonical(dist));
    w.finish()
}

/// `a` and `b` at one and two bits, end-of-block and length 3 at three.
fn ab_litlen() -> Vec<u8> {
    litlen_lengths(&[(97, 1), (98, 2), (256, 3)])
        .into_iter()
        .chain([3])
        .collect()
}

/// Write `a b <3 bytes back 1> EOB` with the given codes and distance code.
fn ab_run(w: &mut BitWriter, lit: &[u32], dist_code: (u32, u32)) {
    w.code(lit[97], 1);
    w.code(lit[98], 2);
    w.code(lit[257], 3);
    w.code(dist_code.0, dist_code.1);
    w.code(lit[256], 3);
}

#[test]
fn a_single_distance_code_of_one_bit_is_allowed_as_zlib_allows() {
    let stream = dynamic(&ab_litlen(), &[1], |w, lit, dist| {
        ab_run(w, lit, (dist[0], 1));
    });
    assert_eq!(decode(&stream, 16), Ok(b"abbbb".to_vec()), "used code");
    // The other one-bit pattern is the code the set leaves unused.
    let stream = dynamic(&ab_litlen(), &[1], |w, lit, _| ab_run(w, lit, (1, 1)));
    assert_eq!(decode(&stream, 16), Err(BAD), "unused code");
}

#[test]
fn a_block_of_literals_needs_no_distance_codes() {
    let litlen = litlen_lengths(&[(97, 1), (256, 1)]);
    let stream = dynamic(&litlen, &[0], |w, lit, _| {
        w.code(lit[97], 1);
        w.code(lit[256], 1);
    });
    assert_eq!(decode(&stream, 16), Ok(b"a".to_vec()), "literals only");
    let stream = dynamic(&ab_litlen(), &[0], |w, lit, _| ab_run(w, lit, (0, 1)));
    assert_eq!(
        decode(&stream, 16),
        Err(BAD),
        "a match with no distance code"
    );
}

#[test]
fn a_literal_code_of_only_end_of_block_is_allowed_as_zlib_allows() {
    let litlen = litlen_lengths(&[(256, 1)]);
    let stream = dynamic(&litlen, &[0], |w, lit, _| w.code(lit[256], 1));
    assert_eq!(decode(&stream, 16), Ok(Vec::new()), "empty block");
}

#[test]
fn refuses_over_subscribed_and_incomplete_codes() {
    let nothing = |_: &mut BitWriter, _: &[u32], _: &[u32]| {};
    let cases: [(&str, Vec<u8>, Vec<u8>); 5] = [
        (
            "over-subscribed literals",
            litlen_lengths(&[(97, 1), (98, 1), (256, 1)]),
            vec![0],
        ),
        (
            "incomplete literals",
            litlen_lengths(&[(97, 2), (256, 2)]),
            vec![0],
        ),
        (
            "over-subscribed distances",
            litlen_lengths(&[(97, 1), (256, 1)]),
            vec![1, 1, 1],
        ),
        (
            "incomplete distances",
            litlen_lengths(&[(97, 1), (256, 1)]),
            vec![2],
        ),
        (
            "no end-of-block",
            litlen_lengths(&[(97, 1), (98, 1)]),
            vec![0],
        ),
    ];
    for (what, litlen, dist) in cases {
        let mut stream = dynamic(&litlen, &dist, nothing);
        stream.extend_from_slice(&[0; 8]);
        assert_eq!(decode(&stream, 16), Err(BAD), "{what}");
    }
}

/// A final dynamic block header with 257 + 1 lengths described by `cl` and
/// `runs`, padded so only the header can be at fault.
fn header_only(nlen: u32, ndist: u32, cl: &[u8; 19], runs: &[(u32, u32)]) -> Vec<u8> {
    let mut w = BitWriter::zlib();
    w.bits(1, 1);
    w.bits(2, 2);
    dynamic_header(&mut w, nlen, ndist, cl, runs);
    let mut stream = w.finish();
    stream.extend_from_slice(&[0; 64]);
    stream
}

#[test]
fn refuses_a_bad_code_length_code() {
    let mut over = [1u8; 19];
    over[0] = 1;
    assert_eq!(
        decode(&header_only(257, 1, &over, &[]), 16),
        Err(BAD),
        "over-subscribed"
    );
    let mut incomplete = CODE_LENGTHS;
    incomplete[15] = 0;
    assert_eq!(
        decode(&header_only(257, 1, &incomplete, &[]), 16),
        Err(BAD),
        "incomplete"
    );
    assert_eq!(
        decode(&header_only(257, 1, &[0; 19], &[]), 16),
        Err(BAD),
        "empty"
    );
}

#[test]
fn refuses_more_symbols_than_deflate_has() {
    let runs = literally(&[0; 320]);
    assert_eq!(
        decode(&header_only(287, 1, &CODE_LENGTHS, &runs), 16),
        Err(BAD),
        "HLIT 287"
    );
    assert_eq!(
        decode(&header_only(288, 1, &CODE_LENGTHS, &runs), 16),
        Err(BAD),
        "HLIT 288"
    );
    assert_eq!(
        decode(&header_only(257, 31, &CODE_LENGTHS, &runs), 16),
        Err(BAD),
        "HDIST 31"
    );
    assert_eq!(
        decode(&header_only(257, 32, &CODE_LENGTHS, &runs), 16),
        Err(BAD),
        "HDIST 32"
    );
}

#[test]
fn refuses_bad_code_length_repeats() {
    let nothing_before = [(16, 0)];
    assert_eq!(
        decode(&header_only(257, 1, &CODE_LENGTHS, &nothing_before), 16),
        Err(BAD),
        "repeat with no previous length"
    );
    let overrun = [(18, 127), (18, 127)];
    assert_eq!(
        decode(&header_only(257, 1, &CODE_LENGTHS, &overrun), 16),
        Err(BAD),
        "repeat past the end"
    );
}

#[test]
fn a_zero_run_may_cross_from_literal_to_distance_lengths() {
    // 97 zeros, a:1, b:2, 157 zeros, EOB:2, then three zeros covering symbol
    // 257 and both distance codes.
    let runs = [
        (18, 86),
        (1, 0),
        (2, 0),
        (18, 127),
        (18, 8),
        (2, 0),
        (17, 0),
    ];
    let mut w = BitWriter::zlib();
    w.bits(1, 1);
    w.bits(2, 2);
    dynamic_header(&mut w, 258, 2, &CODE_LENGTHS, &runs);
    let mut lengths = litlen_lengths(&[(97, 1), (98, 2), (256, 2)]);
    lengths.push(0);
    let codes = canonical(&lengths);
    w.code(codes[97], 1);
    w.code(codes[98], 2);
    w.code(codes[256], 2);
    assert_eq!(
        decode(&w.finish(), 8),
        Ok(b"ab".to_vec()),
        "run across the boundary"
    );
}

// ---------------------------------------------------------------------------
// Totality
// ---------------------------------------------------------------------------

/// Valid streams to damage: every block type, small enough to damage whole.
fn victims() -> Vec<(Vec<u8>, Vec<u8>)> {
    #[cfg(not(miri))]
    const LEN: usize = 3000;
    #[cfg(miri)]
    const LEN: usize = 120;
    let mut rng = Rng(0xDA3A6E);
    let mut out = Vec::new();
    for (kind, level) in [(0, 0), (3, 1), (3, 6), (4, 9), (2, 6), (1, 10)] {
        let plain = sample(&mut rng, kind, LEN);
        out.push((compress_to_vec_zlib(&plain, level), plain));
    }
    out
}

/// Verifies: L.btrfs.5, H.STORE.1
#[test]
fn every_truncation_before_the_trailer_is_refused() {
    for (stream, plain) in victims() {
        let deflate_end = stream.len() - 4;
        for cut in 0..deflate_end {
            assert_eq!(
                decode(&stream[..cut], plain.len()),
                Err(BAD),
                "cut at {cut}"
            );
        }
        for cut in deflate_end..=stream.len() {
            let decoded = decode(&stream[..cut], plain.len());
            assert!(decoded == Ok(plain.clone()), "cut in the trailer at {cut}");
        }
    }
}

/// Verifies: L.btrfs.5, H.STORE.1
#[test]
fn bit_flips_give_an_error_or_what_miniz_oxide_decodes() {
    #[cfg(not(miri))]
    const STRIDE: usize = 1;
    #[cfg(miri)]
    const STRIDE: usize = 29;
    for (stream, plain) in victims() {
        for bit in (0..stream.len() * 8).step_by(STRIDE) {
            let mut damaged = stream.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            agrees_with_oracle(&damaged, plain.len(), bit);
        }
    }
}

/// Decode `stream` both ways and require the two to agree.
///
/// The oracle is run on the raw DEFLATE data after the header, with its output
/// capped at `capacity`, which is the same question this decoder answers: any
/// stream one accepts, the other must accept with the same bytes.
fn agrees_with_oracle(stream: &[u8], capacity: usize, context: usize) {
    let ours = decode(stream, capacity);
    let header_ok = super::check_header(stream).is_some();
    let theirs = if header_ok {
        decompress_to_vec_with_limit(&stream[2..], capacity).map_err(|_| BAD)
    } else {
        Err(BAD)
    };
    assert!(
        ours == theirs,
        "bit {context}: ours {:?}, miniz_oxide {:?}",
        ours.as_ref().map(Vec::len),
        theirs.as_ref().map(Vec::len)
    );
}

/// Verifies: L.btrfs.5
#[test]
fn random_bytes_behind_a_valid_header_are_total() {
    #[cfg(not(miri))]
    const ROUNDS: usize = 20_000;
    #[cfg(miri)]
    const ROUNDS: usize = 40;
    let mut rng = Rng(0xBAD_DA7A);
    for round in 0..ROUNDS {
        let mut stream = vec![0x78, 0x9C];
        stream.extend((0..rng.below(600)).map(|_| rng.byte()));
        if let Some(first) = stream.get_mut(2) {
            // Half the time, a dynamic block, whose header has the most rules.
            *first = if round % 2 == 0 {
                *first & !6 | 4
            } else {
                *first
            };
        }
        agrees_with_oracle(&stream, 1 + rng.below(4096), round);
    }
}

#[test]
fn format_helpers_are_consistent() {
    // The bit writer and canonical builder are what the hand-built tests
    // stand on, so check them against a known encoding: RFC 1951's own
    // example, lengths (3, 3, 3, 3, 3, 2, 4, 4) giving codes 010 .. 1111.
    let codes = canonical(&[3, 3, 3, 3, 3, 2, 4, 4]);
    assert_eq!(codes, [2, 3, 4, 5, 6, 0, 14, 15], "RFC 1951 §3.2.2 example");
    let mut w = BitWriter::new(&[]);
    w.bits(0b101, 3);
    w.code(0b11, 2);
    assert_eq!(
        w.finish(),
        [0b11101],
        "fields low bit first, codes high bit first"
    );
    assert_eq!(
        format!("{BAD}"),
        "corrupt stream for compression type 1",
        "display"
    );
}
