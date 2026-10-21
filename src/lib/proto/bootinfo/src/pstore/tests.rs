//! The region's format, its refusals, the address choice and the text.

extern crate std;

use std::{format, vec, vec::Vec};

use super::*;

/// A header for `capacity` and `sequence` with `written` set.
fn header(capacity: u64, sequence: u64, written: u64) -> [u8; HEADER_BYTES] {
    let mut bytes = encode(capacity, sequence);
    put(&mut bytes, WRITTEN_OFFSET, &written.to_le_bytes());
    bytes
}

#[test]
fn crc32c_is_castagnoli() {
    assert_eq!(crc32c(b"123456789"), 0xE306_9283, "the CRC-32C check value");
    assert_eq!(crc32c(b""), 0, "the empty input");
}

#[test]
fn a_fresh_header_validates_as_written() {
    let found = validate(&header(CAPACITY, 7, 0), LEN).unwrap();
    assert_eq!(
        found,
        Found {
            version: VERSION,
            capacity: CAPACITY,
            sequence: 7,
            written: 0,
            stop: 0
        },
        "what encode wrote"
    );
    assert_eq!(&encode(CAPACITY, 7)[..8], b"FXPSTORE", "the magic first");
}

/// Verifies: `L.console.43`
#[test]
fn every_field_is_checked() {
    let good = header(CAPACITY, 3, 10);
    let refused = |at: usize, value: u8| {
        let mut bytes = good;
        bytes[at] ^= value;
        validate(&bytes, LEN)
    };
    assert_eq!(refused(0, 1), Err(Refusal::NothingThere), "magic");
    assert_eq!(refused(8, 1), Err(Refusal::Version(3)), "version");
    assert_eq!(refused(13, 1), Err(Refusal::Layout), "header length");
    assert_eq!(
        refused(18, 1),
        Err(Refusal::Layout),
        "capacity, not a power of two"
    );
    assert_eq!(refused(24, 1), Err(Refusal::Crc), "sequence, under the CRC");
    assert_eq!(refused(33, 1), Err(Refusal::Crc), "the CRC itself");
    assert_eq!(
        refused(36, 1),
        Ok(validate(&good, LEN).unwrap()),
        "reserved bytes are not checked"
    );
    let mut far = good;
    put(&mut far, WRITTEN_OFFSET, &WRITTEN_LIMIT.to_le_bytes());
    assert_eq!(validate(&far, LEN), Err(Refusal::Written), "count bound");
    assert_eq!(
        validate(&good, LEN + PAGE_SIZE),
        Err(Refusal::Layout),
        "region length"
    );
    assert_eq!(
        validate(&[0; HEADER_BYTES], LEN),
        Err(Refusal::NothingThere),
        "zeroed RAM"
    );
}

/// Verifies: `L.console.43`
#[test]
fn capacities_outside_the_bounds_are_refused() {
    for capacity in [MIN_CAPACITY / 2, MAX_CAPACITY * 2, CAPACITY + 1, 0] {
        assert_eq!(
            validate(&header(capacity, 1, 0), HEADER + capacity),
            Err(Refusal::Layout),
            "capacity {capacity}"
        );
    }
    assert!(
        validate(&header(MIN_CAPACITY, 1, 0), HEADER + MIN_CAPACITY).is_ok(),
        "smallest"
    );
    assert!(
        validate(&header(MAX_CAPACITY, 1, 0), HEADER + MAX_CAPACITY).is_ok(),
        "largest"
    );
}

const MIB: u64 = 1 << 20;

#[test]
fn the_choice_is_one_gibibyte_up_where_it_fits() {
    // A PC: low memory, then a big range from 1 MiB.
    let map = [(0x1000, 0x9_F000), (MIB, 2047 * MIB)];
    assert_eq!(choose(map, 4096 * MIB, LEN), Some(1024 * MIB), "a PC");
    // The same map in another order gives the same address.
    let turned = [(MIB, 2047 * MIB), (0x1000, 0x9_F000)];
    assert_eq!(
        choose(turned, 4096 * MIB, LEN),
        Some(1024 * MIB),
        "order does not matter"
    );
    // A range ending just short of 1 GiB plus the region: sixty-four
    // mebibytes up instead.
    let short = [(0x1000, 0x9_F000), (MIB, 1024 * MIB)];
    assert_eq!(
        choose(short, 4096 * MIB, LEN),
        Some(64 * MIB),
        "no room at 1 GiB"
    );
}

#[test]
fn the_n150_box() {
    // The SOYO M4 Pro's map below 4 GiB as Linux's e820 shows it
    // (2026-10-16), and its RAM above: the region at 1 GiB, where the
    // inspection Linux reserves it.
    let map = [
        (0, 0x9_E000),
        (0x9_F000, 0x1000),
        (MIB, 0x3e55_c000 - MIB),
        (0x3e55_d000, 0x3e59_3000 - 0x3e55_d000),
        (0x3e59_4000, 0x7023_0000 - 0x3e59_4000),
        (0x1_0000_0000, 0x3_8000_0000),
    ];
    assert_eq!(choose(map, u64::MAX, LEN), Some(0x4000_0000), "the N150");
}

#[test]
fn ovmf_shaped_map() {
    // OVMF at 512 MiB, as test-postmortem boots it: conventional memory below
    // 8 MiB, its MEMFD and the decompressed firmware volumes up to 27 MiB
    // (which a warm reset rewrites, measured 2026-10-15), conventional again
    // above, and the top taken by DXE's allocations.
    let map = [(0, 0xA_0000), (MIB, 7 * MIB), (27 * MIB, 450 * MIB)];
    assert_eq!(
        choose(map, 512 * MIB, LEN),
        Some(64 * MIB),
        "past the firmware's low work area"
    );
}

#[test]
fn an_occupied_place_moves_the_choice_up() {
    // 64..68 MiB taken by firmware: the next range that fits.
    let map = [(0, 64 * MIB), (68 * MIB + 4096, 100 * MIB)];
    assert_eq!(
        choose(map, u64::MAX, LEN),
        Some(70 * MIB),
        "aligned up past the gap"
    );
    // A range too short for the region is passed over.
    let short = [(0, MIB), (64 * MIB, MIB), (90 * MIB, 8 * MIB)];
    assert_eq!(
        choose(short, u64::MAX, LEN),
        Some(90 * MIB),
        "the short range skipped"
    );
}

#[test]
fn arm_ram_at_one_gibibyte() {
    let map = [(0x4000_0000 + 0x10_0000, 512 * MIB)];
    assert_eq!(
        choose(map, u64::MAX, LEN),
        Some(0x4000_0000 + 64 * MIB),
        "64 MiB above RAM's start, rounded down"
    );
}

#[test]
fn nothing_fits_below_the_direct_map() {
    let map = [(0, 128 * MIB)];
    assert_eq!(
        choose(map, 65 * MIB, LEN),
        None,
        "ends above the direct map"
    );
    assert_eq!(
        choose(map, 65 * MIB + 4096, LEN),
        Some(64 * MIB),
        "just fits"
    );
    assert_eq!(
        choose([] as [(u64, u64); 0], u64::MAX, LEN),
        None,
        "no memory"
    );
    assert_eq!(
        choose([(u64::MAX - MIB, MIB)], u64::MAX, LEN),
        None,
        "no overflow"
    );
}

/// A ring of `capacity` bytes that `text` was recorded into from the start.
fn recorded(capacity: usize, text: &[u8]) -> (Vec<u8>, Found) {
    let mut ring = vec![0u8; capacity];
    for (place, &byte) in text.iter().enumerate() {
        ring[place % capacity] = byte;
    }
    let found = Found {
        version: VERSION,
        capacity: capacity as u64,
        sequence: 1,
        written: text.len() as u64,
        stop: 0,
    };
    (ring, found)
}

fn collect(text: &Text<'_>) -> Vec<u8> {
    (0..text.len()).map(|at| text.at(at).unwrap()).collect()
}

#[test]
fn text_before_the_ring_goes_round_is_all_of_it() {
    let (ring, found) = recorded(4096, b"one\ntwo\n");
    let text = Text::of(&ring, &found);
    assert_eq!(collect(&text), b"one\ntwo\n", "everything");
    assert_eq!(found.kept(), 8, "kept");
}

#[test]
fn text_after_the_ring_went_round_is_the_newest_capacity_less_one() {
    let mut all = Vec::new();
    for line in 0..1000 {
        all.extend_from_slice(format!("line {line}\n").as_bytes());
    }
    let (ring, found) = recorded(4096, &all);
    let text = Text::of(&ring, &found);
    assert_eq!(text.len(), 4095, "capacity less the slot in flight");
    assert_eq!(
        collect(&text),
        all[all.len() - 4095..],
        "the newest bytes, in order"
    );
    let (first, second) = text.parts();
    assert_eq!(first.len() + second.len(), 4095, "two parts");
    // The first line is cut by the wrap and is never a match.
    let last = text.last_line().unwrap();
    assert_eq!(
        &collect(&text)[last.0..last.1],
        b"line 999",
        "the last line"
    );
}

#[test]
fn the_last_panic_line_is_found_across_the_wrap() {
    let mut all = Vec::new();
    all.extend_from_slice(b"FERRIX-PANIC an old one\n");
    for line in 0..300 {
        all.extend_from_slice(format!("filler {line}\n").as_bytes());
    }
    all.extend_from_slice(b"\nFERRIX-PANIC init exited\n  at        src/x.rs:1:1\n");
    let (ring, found) = recorded(4096, &all);
    let text = Text::of(&ring, &found);
    let (start, end) = text.last_line_after(b"FERRIX-PANIC ").unwrap();
    let mut out = [0u8; 160];
    let count = text.shown_into((start, end), 160, &mut out);
    assert_eq!(
        &out[..count],
        b"init exited",
        "the message, marker left off"
    );
    let none = recorded(4096, b"FERRIX-BOOT-OK\n");
    assert_eq!(
        Text::of(&none.0, &none.1).last_line_after(b"FERRIX-PANIC "),
        None,
        "no panic"
    );
}

#[test]
fn a_ring_of_the_wrong_length_has_no_text() {
    let (ring, found) = recorded(4096, b"text\n");
    assert!(
        Text::of(&ring[..2048], &found).is_empty(),
        "shorter than the header says"
    );
}

#[test]
fn unprintable_bytes_are_shown_as_question_marks() {
    let (ring, found) = recorded(4096, b"a\x00b\xffc\td\n");
    let text = Text::of(&ring, &found);
    let mut out = [0u8; 16];
    let count = text.shown_into((0, text.len()), 16, &mut out);
    assert_eq!(&out[..count], b"a?b?c\td\n", "filtered");
    let count = text.shown_into((0, text.len()), 3, &mut out);
    assert_eq!(&out[..count], b"a?b", "cut at most");
}

#[test]
fn quoted_markers_are_defused() {
    let (ring, found) = recorded(4096, b"FERRIX-PANIC x said FERRIX-BOOT-OK and FERRIX-\n");
    let text = Text::of(&ring, &found);
    let line = text.last_line_after(b"FERRIX-PANIC ").unwrap();
    let mut out = [0u8; 64];
    let count = text.shown_into(line, 64, &mut out);
    assert_eq!(
        &out[..count],
        b"x said FERRIX_BOOT-OK and FERRIX_",
        "no marker survives"
    );
    let mut short = *b"FERRIX";
    defuse(&mut short);
    assert_eq!(&short, b"FERRIX", "a part of the marker is left alone");
}

#[test]
fn the_switch_fails_closed() {
    assert_eq!(switch(""), Switch::On, "no option is on");
    assert_eq!(switch("quiet ferrix.pstore=on"), Switch::On, "on");
    assert_eq!(switch("ferrix.pstore=off"), Switch::Off, "off");
    assert_eq!(
        switch("ferrix.pstore=of"),
        Switch::NotUnderstood("of"),
        "a misspelling"
    );
    assert_eq!(switch("ferrix.pstore="), Switch::NotUnderstood(""), "empty");
    assert_eq!(
        switch("ferrix.pstore"),
        Switch::On,
        "a bare word is not the option"
    );
}

/// A header for `capacity` and `sequence` with `written` and the stop word
/// set.
fn stopped(written: u64, stop: u64) -> [u8; HEADER_BYTES] {
    let mut bytes = header(CAPACITY, 5, written);
    put(&mut bytes, STOP_OFFSET, &stop.to_le_bytes());
    bytes
}

/// The same header as version 1 would have written it: CRC and all.
fn version_1(written: u64, stop: u64) -> [u8; HEADER_BYTES] {
    let mut bytes = stopped(written, stop);
    put(&mut bytes, 8, &VERSION_1.to_le_bytes());
    let crc = crc32c(&bytes[..CHECKED]);
    put(&mut bytes, CHECKED, &crc.to_le_bytes());
    bytes
}

/// Verifies: `L.console.48`
#[test]
fn every_stop_round_trips_and_none_is_zero() {
    assert_eq!(ended(0), Ended::NoneRecorded, "a word of zeros is no stop");
    for (index, stop) in Stop::ALL.into_iter().enumerate() {
        assert_eq!(stop.code() as usize, index + 1, "{stop:?}'s code");
        assert_eq!(Stop::of(stop.code()), Some(stop), "{stop:?} by its code");
        assert_eq!(ended(stop.word()), Ended::Stopped(stop), "{stop:?}'s word");
        let found = validate(&stopped(9, stop.word()), LEN).unwrap();
        assert_eq!(found.ended(), Ended::Stopped(stop), "{stop:?} in a header");
    }
    assert_eq!(
        validate(&encode(CAPACITY, 2), LEN).unwrap().ended(),
        Ended::NoneRecorded,
        "the fresh header's word"
    );
}

/// Verifies: `L.console.48`
#[test]
fn a_changed_bit_is_damaged() {
    for stop in Stop::ALL {
        for bit in 0..64 {
            let word = stop.word() ^ (1 << bit);
            assert_eq!(ended(word), Ended::Damaged(word), "{stop:?}, bit {bit}");
        }
    }
    assert_eq!(
        ended(0xFFFF_FFFF_0000_0000),
        Ended::Damaged(0xFFFF_FFFF_0000_0000),
        "code 0 with its complement is no stop either"
    );
}

/// Verifies: `L.console.48`
#[test]
fn an_unassigned_code_is_a_newer_kernels() {
    for code in [7u32, 255, 0x1234_5678, u32::MAX - 1] {
        let word = u64::from(code) | (u64::from(!code) << 32);
        assert_eq!(ended(word), Ended::Unknown(code), "code {code}");
    }
}

/// Verifies: `L.console.48`
#[test]
fn version_1_reads_with_no_stop_word_and_version_3_is_refused() {
    let old = version_1(40, Stop::Restart.word());
    let found = validate(&old, LEN).unwrap();
    assert_eq!(found.version, VERSION_1, "read as version 1");
    assert_eq!(found.written, 40, "its count");
    assert_eq!(
        found.ended(),
        Ended::NotKept,
        "whatever its bytes 72..80 are"
    );
    let mut newer = stopped(0, 0);
    put(&mut newer, 8, &3u32.to_le_bytes());
    let crc = crc32c(&newer[..CHECKED]);
    put(&mut newer, CHECKED, &crc.to_le_bytes());
    assert_eq!(validate(&newer, LEN), Err(Refusal::Version(3)), "version 3");
}

/// Verifies: `L.console.43`
#[test]
fn the_kernel_takes_only_a_fresh_version_2_header() {
    assert!(
        fresh(&encode(CAPACITY, 4), LEN).is_ok(),
        "the loader's fresh header"
    );
    assert_eq!(
        fresh(&version_1(0, 0), LEN),
        Err(Refusal::Version(VERSION_1)),
        "a version-1 header"
    );
    assert_eq!(
        fresh(&stopped(1, 0), LEN),
        Err(Refusal::Recorded),
        "a count"
    );
    for word in Stop::ALL
        .map(Stop::word)
        .into_iter()
        .chain([1, 1 << 40, u64::MAX])
    {
        assert_eq!(
            fresh(&stopped(0, word), LEN),
            Err(Refusal::Stopped),
            "a stop word {word:#x}, whole or damaged"
        );
    }
    assert_eq!(
        fresh(&[0; HEADER_BYTES], LEN),
        Err(Refusal::NothingThere),
        "zeroed RAM"
    );
}
