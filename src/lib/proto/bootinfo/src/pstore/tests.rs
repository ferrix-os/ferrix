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
            capacity: CAPACITY,
            sequence: 7,
            written: 0
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
    assert_eq!(refused(8, 1), Err(Refusal::Version(0)), "version");
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
fn the_choice_is_the_lowest_aligned_place_sixty_four_mebibytes_up() {
    // A PC: low memory, then a big range from 1 MiB.
    let map = [(0x1000, 0x9_F000), (MIB, 2047 * MIB)];
    assert_eq!(choose(map, 4096 * MIB, LEN), Some(64 * MIB), "a PC");
    // The same map in another order gives the same address.
    let turned = [(MIB, 2047 * MIB), (0x1000, 0x9_F000)];
    assert_eq!(
        choose(turned, 4096 * MIB, LEN),
        Some(64 * MIB),
        "order does not matter"
    );
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
        capacity: capacity as u64,
        sequence: 1,
        written: text.len() as u64,
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
