//! Tests for the LZO decoder.
//!
//! Three kinds of evidence, each covering a gap in the others. The real extents
//! from `mkfs.btrfs --compress lzo` pin the framing and the stream format to
//! what Linux writes. Round trips through [`encoder`] reach every instruction,
//! every length form and all four sector sizes, which a handful of real files
//! cannot. Hand-built extents pin the rules that no well-formed writer ever
//! breaks, such as a header that must skip to a sector boundary, a
//! back-reference into the previous segment and a segment that grows past its
//! sector. Sweeps of truncations and bit flips then check the one property
//! that matters most in ring 0: whatever the bytes, the decoder answers.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::{LEN_SIZE, MAX_COMPRESSED, decompress, lzo1x, segment_at, worst_compress};
use crate::BtrfsError;
use crate::crc32c::crc32c;
use crate::items::COMPRESS_LZO;

mod encoder;
mod oracle;

use encoder::Stats;

const SECTOR: u32 = 4096;
const CORRUPT: BtrfsError = BtrfsError::BadCompressedData {
    compression: COMPRESS_LZO,
};

// ---------------------------------------------------------------------------
// Real extents
//
// Written by `mkfs.btrfs --compress lzo --rootdir` with 4 KiB sectors, then
// read back off the image: the inline item's payload, or the regular extent's
// sectors including their padding.
// ---------------------------------------------------------------------------

/// `yes 'inline me' | head -c 1800`, stored inline.
const INLINE: &[u8] = include_bytes!("../testdata/lzo/inline.bin");

/// The extent at file offset 393216 of
/// `yes 'compressible line of text' | head -c 500000`: 27 segments in one
/// sector.
const TEXT: &[u8] = include_bytes!("../testdata/lzo/text-393216.bin");

/// The extent at file offset 262144 of a file mixing text and random bytes: 13
/// segments of up to 1.5 KiB spread over five sectors.
const MIXED: &[u8] = include_bytes!("../testdata/lzo/mixed-262144.bin");

/// The plaintext of [`MIXED`], which cannot be regenerated, so it is checked
/// by length and checksum. Both were taken from the file that went into the
/// image, and the same extent decodes to it through `lzokay-native` too.
const MIXED_LEN: usize = 52_616;
const MIXED_CRC32C: u32 = 0x0745_6A32;

fn repeated(line: &[u8], len: usize) -> Vec<u8> {
    line.iter().copied().cycle().take(len).collect()
}

fn inline_plain() -> Vec<u8> {
    repeated(b"inline me\n", 1800)
}

fn text_plain() -> Vec<u8> {
    repeated(b"compressible line of text\n", 500_000).split_off(393_216)
}

/// Decode `input` into a buffer of `len` bytes and return what was written.
fn decode(input: &[u8], len: usize, sectorsize: u32) -> Result<Vec<u8>, BtrfsError> {
    let mut out = vec![0; len];
    let written = decompress(input, &mut out, sectorsize)?;
    out.truncate(written);
    Ok(out)
}

/// Walk an extent's framing the way the writer laid it out, independently of
/// the decoder, and return each segment's offset and length.
fn segments(extent: &[u8], sector: usize) -> Vec<(usize, usize)> {
    let total = u32::from_le_bytes(extent[..4].try_into().unwrap()) as usize;
    let mut at = LEN_SIZE;
    let mut found = Vec::new();
    while at < total {
        if sector - at % sector < LEN_SIZE {
            at = at.next_multiple_of(sector);
            continue;
        }
        let len = u32::from_le_bytes(extent[at..at + 4].try_into().unwrap()) as usize;
        found.push((at + LEN_SIZE, len));
        at += LEN_SIZE + len;
    }
    found
}

#[test]
fn an_inline_extent_from_mkfs_decodes() {
    assert_eq!(
        u32::from_le_bytes(INLINE[..4].try_into().unwrap()) as usize,
        INLINE.len(),
        "an inline extent's total length is exactly its payload"
    );
    assert_eq!(decode(INLINE, 1800, SECTOR), Ok(inline_plain()));
}

#[test]
fn a_regular_extent_from_mkfs_decodes() {
    assert_eq!(segments(TEXT, 4096).len(), 27, "one segment per sector");
    assert_eq!(decode(TEXT, 106_784, SECTOR), Ok(text_plain()));
}

#[test]
fn a_many_sector_extent_from_mkfs_decodes() {
    let found = segments(MIXED, 4096);
    assert_eq!(found.len(), 13);
    assert!(
        found.iter().any(|&(at, _)| at > 4096),
        "segments continue past the first sector"
    );
    let plain = decode(MIXED, MIXED_LEN, SECTOR).unwrap();
    assert_eq!(plain.len(), MIXED_LEN);
    assert_eq!(crc32c(&plain), MIXED_CRC32C);
}

#[test]
fn padding_after_the_total_length_is_ignored() {
    // The rest of the last sector is not part of the extent, so whatever it
    // holds, the decoder must not read it.
    let mut extent = TEXT.to_vec();
    let total = u32::from_le_bytes(extent[..4].try_into().unwrap()) as usize;
    extent[total..].fill(0xFF);
    assert_eq!(decode(&extent, 106_784, SECTOR), Ok(text_plain()));
}

// ---------------------------------------------------------------------------
// Hand-built framing
// ---------------------------------------------------------------------------

/// An LZO1X stream of nothing but `literals`, followed by the end marker.
fn literal_stream(literals: &[u8]) -> Vec<u8> {
    let n = literals.len();
    let mut out = if (1..=238).contains(&n) {
        vec![17 + n as u8]
    } else {
        assert!(n > 18, "a literal run instruction carries at least 19");
        let mut head = vec![0];
        let mut rest = n - 18;
        while rest > 255 {
            head.push(0);
            rest -= 255;
        }
        head.push(rest as u8);
        head
    };
    out.extend_from_slice(literals);
    out.extend_from_slice(&[0x11, 0, 0]);
    out
}

/// Frame `segments` as btrfs does: a total length, then each segment, padding
/// to the sector boundary whenever fewer than four bytes are left in it, then
/// zeroes to the end of the last sector. Returns the extent and how many times
/// the padding rule applied.
fn frame(segments: &[Vec<u8>], sector: usize) -> (Vec<u8>, usize) {
    let mut out = vec![0; LEN_SIZE];
    let mut skips = 0;
    for segment in segments {
        out.extend_from_slice(&(segment.len() as u32).to_le_bytes());
        out.extend_from_slice(segment);
        let left = sector - out.len() % sector;
        if left < LEN_SIZE {
            out.resize(out.len() + left, 0);
            skips += 1;
        }
    }
    let total = out.len() as u32;
    out[..4].copy_from_slice(&total.to_le_bytes());
    out.resize(out.len().next_multiple_of(sector), 0);
    (out, skips)
}

/// A literal-only stream exactly `len` bytes long.
fn stream_of_length(len: usize, fill: u8) -> Vec<u8> {
    (19..len)
        .map(|n| literal_stream(&vec![fill; n]))
        .find(|stream| stream.len() == len)
        .unwrap()
}

#[test]
fn a_header_that_would_straddle_a_sector_starts_on_the_boundary() {
    // With 512-byte sectors the first segment's data starts at 8. A stream of
    // 500 bytes leaves exactly four before the boundary, so the next header
    // fits; 501 to 503 leave three, two and one, so the next header moves to
    // 512; 504 ends on the boundary itself.
    for (len, skips) in [(500, 0), (501, 1), (502, 1), (503, 1), (504, 0)] {
        let first = stream_of_length(len, b'a');
        let first_plain = first.len() - 6;
        let second = literal_stream(b"the second segment");
        let (extent, found) = frame(&[first, second.clone()], 512);
        assert_eq!(found, skips, "stream of {len}");
        let header = if skips == 1 { 512 } else { 8 + len };
        assert_eq!(
            u32::from_le_bytes(extent[header..header + 4].try_into().unwrap()) as usize,
            second.len()
        );

        let mut expected = vec![b'a'; first_plain];
        expected.extend_from_slice(b"the second segment");
        assert_eq!(decode(&extent, 4096, 512), Ok(expected), "stream of {len}");
    }
}

#[test]
fn a_header_in_the_padding_is_not_found() {
    // The same two segments packed without the alignment rule: the second
    // header straddles the boundary at 512. A reader that does not skip would
    // decode this; the rule says the header is at 512, which is garbage.
    let first = stream_of_length(502, b'a');
    let second = literal_stream(b"the second segment");
    let mut extent = vec![0; LEN_SIZE];
    for segment in [&first, &second] {
        extent.extend_from_slice(&(segment.len() as u32).to_le_bytes());
        extent.extend_from_slice(segment);
    }
    let total = extent.len() as u32;
    extent[..4].copy_from_slice(&total.to_le_bytes());
    extent.resize(1024, 0);
    assert_eq!(decode(&extent, 4096, 512), Err(CORRUPT));
}

#[test]
fn a_back_reference_cannot_reach_the_previous_segment() {
    let first = literal_stream(b"abcdefgh");
    // Four literals, then an M2 match of three bytes at distance `d + 1`, then
    // the end marker.
    let second = |d: u8| vec![21, b'w', b'x', b'y', b'z', 64 | (d << 2), 0, 0x11, 0, 0];

    let (inside, _) = frame(&[first.clone(), second(3)], 4096);
    assert_eq!(
        decode(&inside, 4096, SECTOR),
        Ok(b"abcdefghwxyzwxy".to_vec())
    );

    // Distance 5 from the fifth byte of the segment is the last byte of the
    // first segment. It is right there in the output buffer, and Linux would
    // still refuse it.
    let (across, _) = frame(&[first, second(4)], 4096);
    assert_eq!(decode(&across, 4096, SECTOR), Err(CORRUPT));
}

#[test]
fn a_segment_expands_to_at_most_one_sector() {
    let (fits, _) = frame(&[literal_stream(&[7; 512])], 512);
    assert_eq!(decode(&fits, 4096, 512), Ok(vec![7; 512]));
    let (spills, _) = frame(&[literal_stream(&[7; 513])], 1024);
    assert_eq!(decode(&spills, 4096, 512), Err(CORRUPT));
}

/// Verifies: L.btrfs.5
#[test]
fn the_output_bounds_the_decode() {
    let plain = text_plain();
    assert_eq!(decode(TEXT, plain.len() - 1, SECTOR), Err(CORRUPT));
    // Full exactly at a segment boundary is the caller's range satisfied.
    assert_eq!(
        decode(TEXT, 3 * 4096, SECTOR),
        Ok(plain[..3 * 4096].to_vec())
    );
    assert_eq!(decode(TEXT, 0, SECTOR), Ok(Vec::new()));
    // A larger buffer is fine; the caller zero-fills the rest.
    assert_eq!(decode(TEXT, 128 * 1024, SECTOR), Ok(plain));
}

#[test]
fn the_total_length_is_checked_against_the_input() {
    let with_total = |total: u32, input: &[u8]| {
        let mut extent = input.to_vec();
        extent[..4].copy_from_slice(&total.to_le_bytes());
        extent
    };
    let inline_total = INLINE.len() as u32;
    assert_eq!(
        decode(&with_total(inline_total + 1, INLINE), 1800, SECTOR),
        Err(CORRUPT),
        "longer than the input"
    );
    assert_eq!(
        decode(&with_total(inline_total - 1, INLINE), 1800, SECTOR),
        Err(CORRUPT),
        "cuts the only segment short"
    );
    for total in 0..4 {
        assert_eq!(
            decode(&with_total(total, INLINE), 1800, SECTOR),
            Err(CORRUPT),
            "does not cover its own header"
        );
    }

    let mut unused_sector = TEXT.to_vec();
    unused_sector.resize(2 * 4096, 0);
    assert_eq!(
        decode(&unused_sector, 106_784, SECTOR),
        Err(CORRUPT),
        "leaves a whole sector unused"
    );

    let mut huge = vec![0; MAX_COMPRESSED + 1];
    huge[..4].copy_from_slice(&(MAX_COMPRESSED as u32 + 1).to_le_bytes());
    assert_eq!(decode(&huge, 4096, SECTOR), Err(CORRUPT), "past 128 KiB");

    assert_eq!(decode(&[], 4096, SECTOR), Err(CORRUPT), "no header at all");
    assert_eq!(decode(&[4, 0, 0, 0], 4096, SECTOR), Ok(Vec::new()));
}

#[test]
fn a_segment_length_is_bounded_by_the_worst_case() {
    assert_eq!(
        worst_compress(4096),
        4421,
        "Linux's lzo1x_worst_compress(4096)"
    );
    let worst = worst_compress(4096);
    let mut extent = vec![0; 3 * 4096];
    let total = extent.len();
    extent[4..8].copy_from_slice(&(worst as u32).to_le_bytes());
    assert!(segment_at(&extent, 4, total, 4096).is_some());
    extent[4..8].copy_from_slice(&(worst as u32 + 1).to_le_bytes());
    assert!(segment_at(&extent, 4, total, 4096).is_none());

    extent[..4].copy_from_slice(&(total as u32).to_le_bytes());
    assert_eq!(decode(&extent, 128 * 1024, SECTOR), Err(CORRUPT));
    extent[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decode(&extent, 128 * 1024, SECTOR), Err(CORRUPT));
}

#[test]
fn a_segment_must_end_within_the_total_length() {
    let (mut extent, _) = frame(&[literal_stream(b"abcdefgh")], 4096);
    let total = u32::from_le_bytes(extent[..4].try_into().unwrap());
    extent[..4].copy_from_slice(&(total - 1).to_le_bytes());
    assert_eq!(decode(&extent, 4096, SECTOR), Err(CORRUPT));
}

#[test]
fn sector_sizes_outside_the_format_are_refused() {
    for size in [0, 256, 511, 1000, 4097, 131_072, u32::MAX] {
        assert_eq!(
            decode(INLINE, 1800, size),
            Err(BtrfsError::BadSectorSize(size))
        );
    }
    // The inline extent's one segment holds 1800 bytes, which is more than a
    // sector of 512 or 1024 can: under those sizes the same bytes are corrupt.
    for size in [512, 1024] {
        assert_eq!(decode(INLINE, 1800, size), Err(CORRUPT), "{size}");
    }
    for size in [2048, 4096, 8192, 16384, 32768, 65536] {
        assert_eq!(decode(INLINE, 1800, size), Ok(inline_plain()), "{size}");
    }
}

// ---------------------------------------------------------------------------
// Hand-built streams
// ---------------------------------------------------------------------------

fn raw(stream: &[u8], len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0; len];
    let written = lzo1x::decompress(stream, &mut out)?;
    out.truncate(written);
    Some(out)
}

#[test]
fn a_stream_ends_exactly_at_its_end_marker() {
    assert_eq!(raw(&[0x11, 0, 0], 16), Some(Vec::new()));
    assert_eq!(raw(&literal_stream(b"abcd"), 16), Some(b"abcd".to_vec()));

    let mut trailing = literal_stream(b"abcd");
    trailing.push(0);
    assert_eq!(raw(&trailing, 16), None, "input left after the marker");

    let missing = &literal_stream(b"abcd")[..5];
    assert_eq!(raw(missing, 16), None, "no marker at all");

    // An M4 with a zero distance but a length of four is not the marker.
    assert_eq!(raw(&[21, b'a', b'b', b'c', b'd', 0x12, 0, 0], 16), None);

    for short in [&[][..], &[0x11], &[0x11, 0]] {
        assert_eq!(raw(short, 16), None);
    }
}

#[test]
fn every_first_byte_form_decodes() {
    // 18 to 20 copy one to three literals and leave that many as the state, so
    // the next instruction below 16 is a two-byte M1 match.
    let m1 = [19, b'a', b'b', 0x04, 0, 0x11, 0, 0];
    assert_eq!(raw(&m1, 16), Some(b"abab".to_vec()));
    // A first byte of 16 or 17 in a short stream is a match into nothing.
    assert_eq!(raw(&[16, 0, 0], 16), None);
    // 0 to 15 is an ordinary literal run.
    assert_eq!(
        raw(&[1, b'a', b'b', b'c', b'd', 0x11, 0, 0], 16),
        Some(b"abcd".to_vec())
    );
}

#[test]
fn lzo_rle_zero_runs_decode_only_under_version_one() {
    // Version prefix, four literals, a run of 100 zeroes (96 = 12 << 3 | 0),
    // end marker.
    let stream = |version: u8| {
        vec![
            17, version, 21, b'a', b'b', b'c', b'd', 0x18, 0xFC, 0xFF, 12, 0x11, 0, 0,
        ]
    };
    let mut expected = b"abcd".to_vec();
    expected.extend_from_slice(&[0; 100]);
    assert_eq!(raw(&stream(1), 256), Some(expected));
    // Under version zero the same bytes are an M4 match far out of reach.
    assert_eq!(raw(&stream(0), 256), None);
    // And the run is bounded by the output like everything else.
    assert_eq!(raw(&stream(1), 103), None);
}

#[test]
fn long_extension_runs_terminate() {
    let mut stream = vec![0; 100_000];
    assert_eq!(raw(&stream, 1 << 20), None);
    stream[0] = 32;
    assert_eq!(raw(&stream, 1 << 20), None);
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

/// xorshift64*: small, seedable, and good enough to vary test inputs.
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

/// `len` bytes that switch between runs of zeroes, a four-letter alphabet,
/// words, copies from up to 48 KiB back, random bytes and repeated bytes. Each
/// input leans towards one of these, so the corpus spans entropies from a run
/// of zeroes to noise.
fn sample(rng: &mut Rng, len: usize) -> Vec<u8> {
    const WORDS: [&[u8]; 6] = [b"the ", b"btrfs ", b"sector ", b"segment ", b"lzo ", b"\n"];
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
            0 => out.resize(out.len() + piece, 0),
            1 => (0..piece).for_each(|_| out.push(b"ACGT"[rng.below(4)])),
            2 => (0..piece / 4).for_each(|_| out.extend_from_slice(WORDS[rng.below(6)])),
            3 if !out.is_empty() => {
                let start = out.len().saturating_sub(1 + rng.below(0xC000));
                for i in 0..piece {
                    out.push(out[start + i]);
                }
            }
            4 => (0..piece).for_each(|_| out.push(rng.next() as u8)),
            _ => {
                let byte = rng.next() as u8;
                out.resize(out.len() + piece, byte);
            }
        }
    }
    out.truncate(len);
    out
}

/// Input lengths: every edge of the first-byte and sector forms, then random
/// lengths up to 128 KiB.
fn lengths(rng: &mut Rng, random: usize, max: usize) -> Vec<usize> {
    let mut lengths = vec![
        1, 2, 3, 4, 5, 17, 18, 238, 239, 511, 512, 513, 4095, 4096, 4097,
    ];
    lengths.retain(|&len| len <= max);
    lengths.extend((0..random).map(|_| 1 + rng.below(max)));
    lengths
}

const RANDOM_CASES: usize = if cfg!(miri) { 2 } else { 120 };
const MAX_LEN: usize = if cfg!(miri) { 4096 } else { 128 * 1024 };

#[test]
fn raw_streams_round_trip_through_the_test_encoder() {
    let mut rng = Rng(0x5EED_0000_0000_0001);
    let mut stats = Stats::default();
    for len in lengths(&mut rng, RANDOM_CASES, MAX_LEN) {
        let plain = sample(&mut rng, len);
        let (stream, used) = encoder::compress(&plain);
        stats.add(used);
        assert_eq!(
            raw(&stream, len).as_deref(),
            Some(&plain[..]),
            "length {len}"
        );
        assert_eq!(raw(&stream, len - 1), None, "one byte short, length {len}");
    }
    if !cfg!(miri) {
        let Stats {
            literal_runs,
            extended,
            m1_short,
            m1_after_run,
            m2,
            m3,
            m4,
        } = stats;
        for (name, count) in [
            ("literal runs", literal_runs),
            ("extended lengths", extended),
            ("short M1", m1_short),
            ("M1 after a run", m1_after_run),
            ("M2", m2),
            ("M3", m3),
            ("M4", m4),
        ] {
            assert!(count > 0, "the corpus never produced {name}: {stats:?}");
        }
    }
}

#[test]
fn extents_round_trip_through_the_test_encoder() {
    let mut rng = Rng(0x5EED_0000_0000_0002);
    let mut skips = 0;
    let mut decoded = 0;
    for len in lengths(&mut rng, RANDOM_CASES, MAX_LEN) {
        let plain = sample(&mut rng, len);
        let sector = [512, 1024, 4096, 65536][rng.below(4)];
        let segments: Vec<Vec<u8>> = plain
            .chunks(sector)
            .map(|chunk| encoder::compress(chunk).0)
            .collect();
        let (extent, skipped) = frame(&segments, sector);
        let total = u32::from_le_bytes(extent[..4].try_into().unwrap()) as usize;
        let result = decode(&extent, len, sector as u32);
        if total > MAX_COMPRESSED {
            // Linux would have stored this uncompressed; as an LZO extent it
            // is corrupt.
            assert_eq!(result, Err(CORRUPT));
            continue;
        }
        assert_eq!(
            result.as_deref(),
            Ok(&plain[..]),
            "length {len} sector {sector}"
        );
        skips += skipped;
        decoded += 1;
    }
    assert!(decoded > 0);
    if !cfg!(miri) {
        assert!(skips > 0, "no extent exercised the alignment rule");
    }
}

// ---------------------------------------------------------------------------
// Corruption
// ---------------------------------------------------------------------------

/// Whatever the bytes, the decoder answers: a length within the buffer, or
/// the one error it reports for bad data.
fn answers(input: &[u8], len: usize) {
    let mut out = vec![0; len];
    match decompress(input, &mut out, SECTOR) {
        Ok(written) => assert!(written <= len),
        Err(error) => assert_eq!(error, CORRUPT),
    }
}

const VECTORS: [(&[u8], usize); 3] = [(INLINE, 1800), (TEXT, 106_784), (MIXED, MIXED_LEN)];

/// Verifies: L.btrfs.5, H.STORE.1
#[test]
fn every_truncation_answers() {
    for (extent, len) in VECTORS {
        // Under Miri a 53 KiB decode is slow, so the sweep samples instead.
        let stride = match (cfg!(miri), extent.len()) {
            (false, _) => 1,
            (true, n) if n > 4096 => 2039,
            (true, _) => 257,
        };
        for cut in (0..=extent.len()).step_by(stride) {
            answers(&extent[..cut], len);
            // Also with the header agreeing, so the cut lands inside the
            // segments rather than failing at the total length.
            if cut >= LEN_SIZE {
                let mut agreeing = extent[..cut].to_vec();
                agreeing[..4].copy_from_slice(&(cut as u32).to_le_bytes());
                answers(&agreeing, len);
            }
        }
    }
}

/// Verifies: L.btrfs.5, H.STORE.1
#[test]
fn single_bit_flips_answer() {
    for (extent, len) in VECTORS {
        let total = u32::from_le_bytes(extent[..4].try_into().unwrap()) as usize;
        let (stride, bits) = match (cfg!(miri), extent.len()) {
            (true, n) if n > 4096 => (1999, 0..1),
            (true, _) => (61, 0..8),
            (false, n) if n > 4096 => (7, 0..8),
            (false, _) => (1, 0..8),
        };
        let mut flipped = extent.to_vec();
        for at in (0..total).step_by(stride) {
            for bit in bits.clone() {
                flipped[at] ^= 1 << bit;
                answers(&flipped, len);
                if !cfg!(miri) {
                    // A buffer with room to spare, so a segment that grows
                    // past its sector is caught by the sector rule alone.
                    answers(&flipped, 128 * 1024);
                }
                flipped[at] ^= 1 << bit;
            }
        }
    }
}

/// Verifies: L.btrfs.5
#[test]
fn random_garbage_answers() {
    let mut rng = Rng(0x5EED_0000_0000_0003);
    let cases = if cfg!(miri) { 16 } else { 4000 };
    for _ in 0..cases {
        let len = rng.below(2 * 4096);
        let mut input: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if len >= 8 {
            // A plausible header, so the garbage reaches the stream decoder.
            let total = len - rng.below(len.min(4096));
            input[..4].copy_from_slice(&(total.max(8) as u32).to_le_bytes());
            let segment = rng.below(total.max(8) - 7) as u32;
            input[4..8].copy_from_slice(&segment.to_le_bytes());
        }
        answers(&input, rng.below(2 * 4096));
    }
}
