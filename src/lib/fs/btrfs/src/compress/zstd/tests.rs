//! Tests for the zstd decoder.
//!
//! The expected output never comes from this decoder. There are four sources:
//!
//! - extents `mkfs.btrfs --compress zstd:15` wrote, padding and all, whose
//!   plaintext is regenerated here or decoded by `ruzstd`;
//! - frames the reference `zstd` CLI wrote at several levels, chosen for the
//!   modes they exercise, checked against `ruzstd` and, where the frame carries
//!   one, their own content checksum;
//! - `ruzstd`'s encoder, fed seeded inputs whose plaintext is the oracle;
//! - frames built by hand, for the modes neither encoder chose — RLE literals,
//!   RLE sequence tables — and for the repeat-offset rules, where a wrong
//!   reading still produces plausible bytes.
//!
//! Then the hostile half: every truncation, single-bit flips, random bodies
//! behind valid headers, and buffers too small for the content, all of which
//! must come back as a length that fits or an error, never a panic.

extern crate std;

use std::vec;
use std::vec::Vec;

use ruzstd::decoding::FrameDecoder;
use ruzstd::encoding::{CompressionLevel, compress_to_vec};

use super::xxh64::xxh64;
use super::*;

const BAD: Result<usize, BtrfsError> = Err(BtrfsError::BadCompressedData {
    compression: COMPRESS_ZSTD,
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decode `input` into a fresh buffer of `out_len` bytes.
fn decode(input: &[u8], out_len: usize) -> (Result<usize, BtrfsError>, Vec<u8>) {
    let mut buffer = vec![0u8; Workspace::SIZE];
    let mut workspace = Workspace::new(&mut buffer).unwrap();
    let mut out = vec![0u8; out_len];
    let result = decompress(input, &mut out, &mut workspace);
    (result, out)
}

/// Decode `input`, which must succeed and fill exactly `expected`.
fn assert_decodes_to(input: &[u8], expected: &[u8], what: &str) {
    let (result, out) = decode(input, expected.len());
    assert_eq!(result, Ok(expected.len()), "{what}: result");
    assert!(out == expected, "{what}: output differs");
}

/// What `ruzstd` makes of a frame, which must fit in `capacity`.
fn ruzstd_decode(frame: &[u8], capacity: usize) -> Vec<u8> {
    let mut out = vec![0u8; capacity];
    let written = FrameDecoder::new().decode_all(frame, &mut out).unwrap();
    out.truncate(written);
    out
}

/// `yes LINE | head -c LEN`, with `line` including its newline.
fn yes(line: &[u8], len: usize) -> Vec<u8> {
    line.iter().copied().cycle().take(len).collect()
}

/// What a sweep decode came to, once checked to be total.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Wrote(usize),
    Corrupt,
}

/// A decoder that keeps one workspace and output buffer across many calls,
/// asserting only that every result is total.
struct Sweeper {
    buffer: Vec<u8>,
    out: Vec<u8>,
}

impl Sweeper {
    fn new(out_len: usize) -> Self {
        Sweeper {
            buffer: vec![0u8; Workspace::SIZE],
            out: vec![0u8; out_len],
        }
    }

    fn run(&mut self, input: &[u8]) -> Outcome {
        let mut workspace = Workspace::new(&mut self.buffer).unwrap();
        match decompress(input, &mut self.out, &mut workspace) {
            Ok(written) => {
                assert!(written <= self.out.len(), "wrote more than the buffer");
                Outcome::Wrote(written)
            }
            result => {
                assert_eq!(result, BAD, "the only error is corrupt data");
                Outcome::Corrupt
            }
        }
    }
}

/// splitmix64: small, seeded, and the same on every host.
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
        (self.next() % bound.max(1) as u64) as usize
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

// ---------------------------------------------------------------------------
// Extents written by mkfs.btrfs
// ---------------------------------------------------------------------------

const INLINE: &[u8] = include_bytes!("../testdata/zstd/inline.txt@0.inline.bin");
const TEXT: &[u8] = include_bytes!("../testdata/zstd/text.txt@393216.regular.bin");
const MIXED: &[u8] = include_bytes!("../testdata/zstd/mixed.txt@0.regular.bin");

/// Where the frame in [`TEXT`] ends and its sector padding begins.
const TEXT_FRAME_LEN: usize = 46;
/// Where the frame in [`MIXED`] ends and its sector padding begins.
const MIXED_FRAME_LEN: usize = 17_790;

fn inline_plaintext() -> Vec<u8> {
    yes(b"inline me\n", 1800)
}

fn text_plaintext() -> Vec<u8> {
    yes(b"compressible line of text\n", 500_000).split_off(393_216)
}

#[test]
fn decodes_an_inline_extent_from_mkfs() {
    assert_decodes_to(INLINE, &inline_plaintext(), "inline.txt");
}

#[test]
fn decodes_a_regular_extent_and_ignores_its_padding() {
    assert!(
        TEXT[TEXT_FRAME_LEN..].iter().all(|&byte| byte == 0),
        "the rest of the extent is padding"
    );
    assert_decodes_to(TEXT, &text_plaintext(), "text.txt@393216");
}

#[test]
fn decodes_a_mixed_extent_as_ruzstd_does() {
    // Four compressed blocks: Huffman literals with one stream and with four,
    // treeless literals, and FSE tables for all three sequence codes.
    assert!(
        MIXED[MIXED_FRAME_LEN..].iter().all(|&byte| byte == 0),
        "the rest of the extent is padding"
    );
    let expected = ruzstd_decode(&MIXED[..MIXED_FRAME_LEN], 128 * 1024);
    assert_eq!(expected.len(), 128 * 1024, "the extent is a full 128 KiB");
    assert_decodes_to(MIXED, &expected, "mixed.txt@0");
}

#[test]
fn the_compress_entry_point_routes_zstd_here() {
    let mut buffer = vec![0u8; Workspace::SIZE];
    let mut workspace = Workspace::new(&mut buffer).unwrap();
    let mut out = vec![0u8; 4096];
    let result = crate::compress::decompress(COMPRESS_ZSTD, INLINE, &mut out, 4096, &mut workspace);
    assert_eq!(
        result,
        Ok(1800),
        "a short result is the caller's to zero-fill"
    );
    assert!(out[..1800] == inline_plaintext()[..], "output differs");
}

// ---------------------------------------------------------------------------
// Frames written by the zstd CLI
// ---------------------------------------------------------------------------

/// A committed CLI frame: name, bytes, content size, whether it is checksummed.
type CliFrame = (&'static str, &'static [u8], usize, bool);

/// Chosen for what each exercises, not for its contents.
const CLI_FRAMES: [CliFrame; 7] = [
    // Few symbols: Huffman weights sent directly as nibbles, four streams.
    (
        "nibbles4k.l3",
        include_bytes!("../testdata/zstd/nibbles4k.l3.zst"),
        4000,
        true,
    ),
    // Piped: no content size, a window descriptor; raw literals, FSE tables.
    (
        "periodic20k.l19.stdin",
        include_bytes!("../testdata/zstd/periodic20k.l19.stdin.zst"),
        20_000,
        false,
    ),
    // RLE blocks, and a frame larger than one block.
    (
        "runs200k.l19",
        include_bytes!("../testdata/zstd/runs200k.l19.zst"),
        200_000,
        true,
    ),
    // Small target block size: dozens of blocks of treeless literals and
    // repeated sequence tables.
    (
        "text24k.blocks.stdin",
        include_bytes!("../testdata/zstd/text24k.blocks.stdin.zst"),
        24_000,
        false,
    ),
    // A single Huffman stream.
    (
        "small300.l19",
        include_bytes!("../testdata/zstd/small300.l19.zst"),
        300,
        true,
    ),
    // Incompressible: a raw block.
    (
        "random1500.l3",
        include_bytes!("../testdata/zstd/random1500.l3.zst"),
        1500,
        true,
    ),
    // A four-byte literals header, predefined and FSE tables in one block.
    (
        "acgt16k.l19",
        include_bytes!("../testdata/zstd/acgt16k.l19.zst"),
        16_000,
        true,
    ),
];

#[test]
fn decodes_cli_frames_as_ruzstd_does() {
    for (name, frame, len, _) in CLI_FRAMES {
        let expected = ruzstd_decode(frame, len);
        assert_eq!(expected.len(), len, "{name}: ruzstd's length");
        assert_decodes_to(frame, &expected, name);
    }
}

#[test]
fn a_wrong_content_checksum_is_corrupt() {
    // The last byte of a checksummed frame is the checksum's top byte, so the
    // content decodes cleanly and only the checksum can notice.
    for (name, frame, len, _) in CLI_FRAMES.into_iter().filter(|cli| cli.3) {
        let mut damaged = frame.to_vec();
        *damaged.last_mut().unwrap() ^= 0x40;
        assert_eq!(decode(&damaged, len).0, BAD, "{name}");
    }
}

#[test]
fn xxh64_matches_published_values() {
    assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999, "empty input");
    assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B, "one byte");
    assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999, "three bytes");
    assert_eq!(
        xxh64(b"Nobody inspects the spammish repetition", 0),
        0xFBCE_A83C_8A37_8BF1,
        "longer than one stripe"
    );
}

// ---------------------------------------------------------------------------
// Frames built by hand
// ---------------------------------------------------------------------------

/// A backward bitstream in which a reader meets `fields` — `(value, width)` —
/// in order: they are written last-read-first, then the end marker.
fn backward_stream(fields: &[(u64, u32)]) -> Vec<u8> {
    let mut bits = Vec::new();
    for &(value, width) in fields.iter().rev() {
        bits.extend((0..width).map(|bit| (value >> bit) & 1 == 1));
    }
    bits.push(true);
    bits.chunks(8)
        .map(|byte| {
            byte.iter()
                .enumerate()
                .fold(0u8, |acc, (at, &set)| acc | (u8::from(set) << at))
        })
        .collect()
}

/// A frame header: magic, descriptor, then whatever fields follow it.
fn header(descriptor: u8, fields: &[u8]) -> Vec<u8> {
    let mut frame = MAGIC.to_le_bytes().to_vec();
    frame.push(descriptor);
    frame.extend_from_slice(fields);
    frame
}

/// A single-segment header with a one-byte content size.
fn single_segment(content_size: u8) -> Vec<u8> {
    header(0x20, &[content_size])
}

/// Append a block of `kind` whose header size field is `size`.
fn push_block(frame: &mut Vec<u8>, last: bool, kind: u32, size: usize, body: &[u8]) {
    let field = (u32::try_from(size).unwrap() << 3) | (kind << 1) | u32::from(last);
    frame.extend_from_slice(&field.to_le_bytes()[..3]);
    frame.extend_from_slice(body);
}

fn push_compressed(frame: &mut Vec<u8>, last: bool, body: &[u8]) {
    push_block(frame, last, 2, body.len(), body);
}

/// A raw literals section of fewer than 32 bytes.
fn raw_literals(bytes: &[u8]) -> Vec<u8> {
    let mut section = vec![u8::try_from(bytes.len() << 3).unwrap()];
    section.extend_from_slice(bytes);
    section
}

/// A sequences section with all three codes in RLE mode, so the bitstream is
/// nothing but extra bits.
fn rle_sequences(count: u8, ll: u8, of: u8, ml: u8, fields: &[(u64, u32)]) -> Vec<u8> {
    let mut section = vec![count, 0x54, ll, of, ml];
    section.extend(backward_stream(fields));
    section
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Five blocks walking every repeat-offset case, with and without literals,
/// and relying on the repeats surviving from one block to the next.
fn repeat_offsets_frame() -> Vec<u8> {
    let mut frame = single_segment(52);
    // Literal length 16 (code 16, one extra bit), offset value 8 (code 3, three
    // extra bits: a distance of 5), match length 8 (code 5). Repeats become
    // [5, 1, 4].
    let block = concat(&[
        &raw_literals(b"abcdefghijklmnop"),
        &rle_sequences(1, 16, 3, 5, &[(0, 3), (0, 1)]),
    ]);
    push_compressed(&mut frame, false, &block);
    // No literals, offset code 1, match length 3. Value 3 means the first
    // repeat minus one (4); value 2 means the third (1).
    let block = concat(&[
        &raw_literals(b""),
        &rle_sequences(2, 0, 1, 0, &[(1, 1), (0, 1)]),
    ]);
    push_compressed(&mut frame, false, &block);
    // One literal each. Value 2 is the second repeat (4), value 3 the third (5).
    let block = concat(&[
        &raw_literals(b"XY"),
        &rle_sequences(2, 1, 1, 0, &[(0, 1), (1, 1)]),
    ]);
    push_compressed(&mut frame, false, &block);
    // One literal each, offset code 0: value 1 is the first repeat, unchanged.
    let block = concat(&[&raw_literals(b"ZW"), &rle_sequences(2, 1, 0, 0, &[])]);
    push_compressed(&mut frame, false, &block);
    // No literals, value 1: the second repeat, swapped to the front each time.
    let block = concat(&[&raw_literals(b""), &rle_sequences(2, 0, 0, 0, &[])]);
    push_compressed(&mut frame, true, &block);
    frame
}

#[test]
fn repeat_offsets_follow_the_rfc() {
    // Worked by hand from RFC 8878 §3.1.2.5:
    //   block 1  "abcdefghijklmnop" + 8 from 5      repeats [5, 1, 4]
    //   block 2  3 from 4, 3 from 1                 repeats [1, 4, 5]
    //   block 3  "X" + 3 from 4, "Y" + 3 from 5     repeats [5, 4, 1]
    //   block 4  "Z" + 3 from 5, "W" + 3 from 5     repeats [5, 4, 1]
    //   block 5  3 from 4, 3 from 5                 repeats [5, 4, 1]
    let expected = b"abcdefghijklmnoplmnoplmnplmmmmXmmmYXmmZYXmWZYXWZYYXW";
    assert_decodes_to(&repeat_offsets_frame(), expected, "repeat offsets");
}

#[test]
fn rle_literals_with_no_sequences() {
    let mut frame = single_segment(23);
    // RLE literals, 20 of 'z', then a sequence count of zero.
    push_compressed(&mut frame, false, &[(20 << 3) | 1, b'z', 0]);
    // And a raw block and an RLE block after it.
    push_block(&mut frame, false, 0, 2, b"ab");
    push_block(&mut frame, true, 1, 1, b"c");
    let mut expected = vec![b'z'; 20];
    expected.extend_from_slice(b"abc");
    assert_decodes_to(&frame, &expected, "RLE literals");
}

/// A one-block frame of `literals` then `sequences`, content size `size`.
fn one_block(size: u8, literals: &[u8], sequences: &[u8]) -> Vec<u8> {
    let mut frame = single_segment(size);
    push_compressed(&mut frame, true, &concat(&[literals, sequences]));
    frame
}

/// Verifies: L.btrfs.5
#[test]
fn a_match_before_the_output_start_is_corrupt() {
    // Two literals, then a match of 3 at a distance of 2 (value 5): fine.
    let good = one_block(
        5,
        &raw_literals(b"ab"),
        &rle_sequences(1, 2, 2, 0, &[(1, 2)]),
    );
    assert_decodes_to(&good, b"ababa", "distance 2 after 2 bytes");
    // The same at a distance of 4 (value 7) reaches before the buffer.
    let bad = one_block(
        5,
        &raw_literals(b"ab"),
        &rle_sequences(1, 2, 2, 0, &[(3, 2)]),
    );
    assert_eq!(decode(&bad, 5).0, BAD, "distance 4 after 2 bytes");
}

#[test]
fn a_leading_repeat_or_treeless_section_is_corrupt() {
    let repeat = one_block(3, &raw_literals(b"abc"), &[1, 0xFC, 0x01]);
    assert_eq!(
        decode(&repeat, 3).0,
        BAD,
        "repeat tables with no previous block"
    );
    // Treeless literals, one stream, 3 regenerated from 2 compressed bytes:
    // type 3, format 0, then sizes 3 and 2 in ten bits each.
    let treeless = one_block(3, &[0x33, 0x80, 0x00, 0x80, 0x01], &[0]);
    assert_eq!(
        decode(&treeless, 3).0,
        BAD,
        "treeless with no previous tree"
    );
}

#[test]
fn malformed_sequence_headers_are_corrupt() {
    // Each case is a valid block with one thing wrong, so that the header rule
    // is the only reason it can fail.
    let literals = raw_literals(b"ab");
    assert_decodes_to(&one_block(2, &literals, &[0]), b"ab", "no sequences");
    let valid = rle_sequences(1, 2, 2, 0, &[(1, 2)]);
    assert_decodes_to(&one_block(5, &literals, &valid), b"ababa", "one sequence");
    let mut reserved = valid;
    reserved[1] |= 1;
    let cases: [(&str, u8, Vec<u8>); 4] = [
        ("trailing bytes after zero sequences", 2, vec![0, 0]),
        ("reserved mode bits", 5, reserved),
        ("offset code 32", 5, rle_sequences(1, 2, 32, 0, &[(1, 32)])),
        (
            "literal length code 36",
            5,
            rle_sequences(1, 36, 2, 0, &[(1, 2)]),
        ),
    ];
    for (what, size, sequences) in cases {
        assert_eq!(
            decode(&one_block(size, &literals, &sequences), 64).0,
            BAD,
            "{what}"
        );
    }
}

#[test]
fn frame_header_rules() {
    let block = |mut frame: Vec<u8>| {
        push_block(&mut frame, true, 0, 3, b"abc");
        frame
    };
    // Dictionary ID field present but zero: allowed.
    assert_decodes_to(&block(header(0x21, &[0, 3])), b"abc", "zero dictionary ID");
    assert_eq!(
        decode(&block(header(0x21, &[7, 3])), 3).0,
        BAD,
        "dictionary ID 7"
    );
    assert_eq!(decode(&block(header(0x28, &[3])), 3).0, BAD, "reserved bit");
    assert_eq!(
        decode(&block(single_segment(2)), 3).0,
        BAD,
        "content size 2"
    );
    assert_eq!(
        decode(&block(single_segment(4)), 9).0,
        BAD,
        "content size 4"
    );
    // Window descriptor, no content size, two-byte form of the size field.
    assert_decodes_to(&block(header(0x00, &[0x50])), b"abc", "window descriptor");
    let mut frame = header(0x60, &[0, 0]);
    push_block(&mut frame, true, 1, 256, b"q");
    assert_decodes_to(
        &frame,
        &[b'q'; 256],
        "two-byte content size is offset by 256",
    );
    let mut reserved = single_segment(3);
    push_block(&mut reserved, true, 3, 3, b"abc");
    assert_eq!(decode(&reserved, 3).0, BAD, "reserved block type");
}

#[test]
fn skippable_frames_before_and_anything_after() {
    let mut frame = 0x184D_2A5Au32.to_le_bytes().to_vec();
    frame.extend_from_slice(&4u32.to_le_bytes());
    frame.extend_from_slice(b"skip");
    frame.extend_from_slice(INLINE);
    frame.extend_from_slice(b"trailing garbage is not read");
    assert_decodes_to(&frame, &inline_plaintext(), "skippable, frame, garbage");
    assert_eq!(decode(&[], 16).0, BAD, "empty input");
    assert_eq!(decode(&[0; 4096], 16).0, BAD, "padding alone");
}

#[test]
fn a_block_over_128_kib_is_corrupt() {
    let mut frame = header(0x00, &[0x90]);
    push_block(&mut frame, true, 1, BLOCK_SIZE_MAX + 1, b"x");
    assert_eq!(
        decode(&frame, 256 * 1024).0,
        BAD,
        "RLE block of 128 KiB + 1"
    );
    let mut frame = header(0x00, &[0x90]);
    push_block(&mut frame, true, 1, BLOCK_SIZE_MAX, b"x");
    assert_decodes_to(&frame, &vec![b'x'; BLOCK_SIZE_MAX], "RLE block of 128 KiB");
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

#[test]
fn workspace_size_is_as_documented() {
    assert_eq!(Workspace::SIZE, 144_384, "the documented size");
    let mut short = vec![0u8; Workspace::SIZE - 1];
    assert_eq!(
        Workspace::new(&mut short).unwrap_err(),
        BtrfsError::Truncated {
            needed: Workspace::SIZE,
            found: Workspace::SIZE - 1
        },
        "a short buffer is refused"
    );
}

#[test]
// Two full extents through both decoders; the extent itself is already
// checked under Miri by `decodes_a_mixed_extent_as_ruzstd_does`.
#[cfg_attr(miri, ignore)]
fn stale_workspace_contents_do_not_matter() {
    let mut buffer = vec![0xA5u8; Workspace::SIZE + 100];
    let mut out = vec![0u8; 128 * 1024];
    for (name, frame, expected) in [
        (
            "mixed",
            MIXED,
            ruzstd_decode(&MIXED[..MIXED_FRAME_LEN], 128 * 1024),
        ),
        ("inline", INLINE, inline_plaintext()),
        (
            "mixed again",
            MIXED,
            ruzstd_decode(&MIXED[..MIXED_FRAME_LEN], 128 * 1024),
        ),
    ] {
        let mut workspace = Workspace::new(&mut buffer).unwrap();
        let result = decompress(frame, &mut out, &mut workspace);
        assert_eq!(result, Ok(expected.len()), "{name}");
        assert!(
            out[..expected.len()] == expected[..],
            "{name}: output differs"
        );
    }
}

// ---------------------------------------------------------------------------
// Differential: ruzstd's encoder
// ---------------------------------------------------------------------------

/// Seeded input of one of several shapes, from no entropy to full.
fn sample(rng: &mut Rng, shape: usize, len: usize) -> Vec<u8> {
    const WORDS: [&[u8]; 12] = [
        b"the ",
        b"extent ",
        b"inode ",
        b"of ",
        b"btrfs ",
        b"zstd ",
        b"frame ",
        b"block ",
        b"literal ",
        b"sequence ",
        b"offset ",
        b".\n",
    ];
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        match shape {
            0 => data.push(0),
            1 => data.push(rng.byte()),
            2 => data.push(b"ACGT"[rng.below(4)]),
            3 => data.extend_from_slice(WORDS[rng.below(WORDS.len())]),
            4 => data.extend(std::iter::repeat_n(rng.byte(), rng.below(300) + 1)),
            _ => push_mixed_segment(rng, &mut data),
        }
    }
    data.truncate(len);
    data
}

/// A segment of random, skewed or copied bytes, for inputs whose statistics
/// change from block to block.
fn push_mixed_segment(rng: &mut Rng, data: &mut Vec<u8>) {
    let len = rng.below(3000) + 1;
    match rng.below(3) {
        0 => data.extend((0..len).map(|_| rng.byte())),
        1 => data.extend((0..len).map(|_| (rng.below(16) * rng.below(16)) as u8)),
        _ if !data.is_empty() => {
            let from = rng.below(data.len());
            let copy: Vec<u8> = data[from..].iter().copied().take(len).collect();
            data.extend_from_slice(&copy);
        }
        _ => data.push(1),
    }
}

/// Compress `data` with `ruzstd` at both of its levels and check the decode,
/// into an exact buffer and a roomier one.
fn round_trip(data: &[u8], what: &str) {
    for level in [CompressionLevel::Uncompressed, CompressionLevel::Fastest] {
        let frame = compress_to_vec(data, level);
        assert_decodes_to(&frame, data, what);
        let (result, out) = decode(&frame, data.len() + 17);
        assert_eq!(result, Ok(data.len()), "{what}: in a larger buffer");
        assert!(out[..data.len()] == *data, "{what}: output differs");
    }
}

#[test]
fn ruzstd_frames_of_every_shape_round_trip() {
    let lengths: &[usize] = if cfg!(miri) {
        &[0, 1, 300, 2000]
    } else {
        &[
            0,
            1,
            2,
            3,
            7,
            31,
            100,
            1000,
            4096,
            10_000,
            65_536,
            100_000,
            128 * 1024,
        ]
    };
    let mut rng = Rng(0x2026_0913);
    for shape in 0..6 {
        for &len in lengths {
            round_trip(
                &sample(&mut rng, shape, len),
                &std::format!("shape {shape}, {len} bytes"),
            );
        }
    }
}

#[test]
#[cfg_attr(miri, ignore)]
fn ruzstd_frames_of_random_shape_and_length_round_trip() {
    let mut rng = Rng(8878);
    for round in 0..60 {
        let shape = rng.below(6);
        let len = rng.below(128 * 1024 + 1);
        round_trip(
            &sample(&mut rng, shape, len),
            &std::format!("round {round}"),
        );
    }
}

// ---------------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------------

/// Frames that are whole (no padding) with their content size.
fn whole_frames() -> Vec<(&'static str, Vec<u8>, usize)> {
    let mut frames = vec![
        ("inline", INLINE.to_vec(), 1800),
        ("text", TEXT[..TEXT_FRAME_LEN].to_vec(), 106_784),
        ("repeat offsets", repeat_offsets_frame(), 52),
    ];
    for (name, frame, len, _) in CLI_FRAMES {
        if !cfg!(miri) || frame.len() < 200 {
            frames.push((name, frame.to_vec(), len));
        }
    }
    frames
}

/// Verifies: L.btrfs.5, H.STORE.1
#[test]
fn every_truncation_is_corrupt_and_total() {
    for (name, frame, len) in whole_frames() {
        let mut sweeper = Sweeper::new(len);
        let stride = if cfg!(miri) { 7 } else { 1 };
        for cut in (0..frame.len()).step_by(stride) {
            assert_eq!(
                sweeper.run(&frame[..cut]),
                Outcome::Corrupt,
                "{name} cut at {cut}"
            );
        }
        assert_eq!(sweeper.run(&frame), Outcome::Wrote(len), "{name} whole");
    }
}

/// Verifies: L.btrfs.5, H.STORE.1
#[test]
fn single_bit_flips_are_total() {
    for (name, frame, len) in whole_frames() {
        let mut sweeper = Sweeper::new(len);
        let mut damaged = frame.clone();
        let stride = if cfg!(miri) { 61 } else { 1 };
        for bit in (0..frame.len() * 8).step_by(stride) {
            damaged[bit / 8] ^= 1 << (bit % 8);
            let _ = sweeper.run(&damaged);
            damaged[bit / 8] ^= 1 << (bit % 8);
        }
        assert_eq!(
            sweeper.run(&damaged),
            Outcome::Wrote(len),
            "{name} restored"
        );
    }
}

/// Verifies: L.btrfs.5
#[test]
#[cfg_attr(miri, ignore)]
fn bit_flips_in_a_full_extent_are_total() {
    // Every bit of the headers and tables at the front of the first block,
    // then a stride through the rest.
    let mut sweeper = Sweeper::new(128 * 1024);
    let mut damaged = MIXED.to_vec();
    let bits = (0..2048 * 8).chain((2048 * 8..MIXED_FRAME_LEN * 8).step_by(13));
    for bit in bits {
        damaged[bit / 8] ^= 1 << (bit % 8);
        let _ = sweeper.run(&damaged);
        damaged[bit / 8] ^= 1 << (bit % 8);
    }
}

/// Verifies: L.btrfs.5
#[test]
fn random_bodies_behind_valid_headers_are_total() {
    let rounds = if cfg!(miri) { 40 } else { 20_000 };
    let mut rng = Rng(0xBAD_F00D);
    let mut sweeper = Sweeper::new(4096);
    for _ in 0..rounds {
        let mut frame = single_segment(rng.byte());
        let body_len = rng.below(200) + 1;
        let body: Vec<u8> = (0..body_len).map(|_| rng.byte()).collect();
        // Mostly compressed blocks, where the tables are.
        let kind = if rng.below(8) == 0 {
            rng.below(4) as u32
        } else {
            2
        };
        push_block(&mut frame, rng.below(4) != 0, kind, body_len, &body);
        if rng.below(2) == 0 {
            frame.extend((0..rng.below(64)).map(|_| rng.byte()));
        }
        let _ = sweeper.run(&frame);
    }
}

/// Verifies: L.btrfs.5
#[test]
fn random_rewrites_of_real_frames_are_total() {
    let rounds = if cfg!(miri) { 20 } else { 3000 };
    let mut rng = Rng(0x5EED);
    let frames = whole_frames();
    for _ in 0..rounds {
        let (_, frame, len) = &frames[rng.below(frames.len())];
        let mut damaged = frame.clone();
        for _ in 0..rng.below(8) + 1 {
            let at = rng.below(damaged.len());
            damaged[at] = rng.byte();
        }
        let _ = Sweeper::new(*len).run(&damaged);
    }
}

/// Verifies: L.btrfs.5
#[test]
fn an_output_buffer_smaller_than_the_content_is_corrupt() {
    for (name, frame, len) in whole_frames() {
        assert_eq!(decode(&frame, len - 1).0, BAD, "{name} one byte short");
        assert_eq!(decode(&frame, 0).0, BAD, "{name} into nothing");
    }
    assert_eq!(decode(MIXED, 128 * 1024 - 1).0, BAD, "mixed one byte short");
}

// ---------------------------------------------------------------------------
// Stack
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn decodes_on_a_32_kib_stack() {
    // A kernel stack is small, and the workspace exists so that a decode does
    // not need a large one. This is a tripwire for a table creeping back onto
    // the stack, with room for an unoptimised build and the thread's own
    // start-up frames; the frames themselves are far smaller (see the module
    // documentation).
    let mut buffer = vec![0u8; Workspace::SIZE];
    let mut out = vec![0u8; 128 * 1024];
    let handle = std::thread::Builder::new()
        .stack_size(32 * 1024)
        .spawn(move || {
            let mut workspace = Workspace::new(&mut buffer).unwrap();
            [MIXED, CLI_FRAMES[0].1, CLI_FRAMES[3].1]
                .map(|frame| decompress(frame, &mut out, &mut workspace))
        })
        .unwrap();
    let results = handle.join().unwrap();
    assert_eq!(
        results,
        [Ok(128 * 1024), Ok(4000), Ok(24_000)],
        "full extent, direct weights, treeless blocks"
    );
}
