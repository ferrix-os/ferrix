//! Host tests of the event log: the formats, the composition from a
//! pstore, the append's order, torn writes and the range check.

extern crate std;

use std::vec;
use std::vec::Vec;

use ferrix_bootinfo::pstore::{self, CAPACITY, HEADER_BYTES, WRITTEN_OFFSET};
use ferrix_slots::gpt::Part;
use ferrix_slots::record::{ControlDisk, DeviceError};

use super::*;

/// The N150 box's partition: LBA 40 to 2047.
const N150: Part = Part {
    index: 2,
    unique: [7; 16],
    first_lba: 40,
    last_lba: 2047,
};

/// A disk of 4096 blocks of 512 bytes that can lose power: after `budget`
/// block writes, every further write is dropped, as a reset or a power cut
/// mid-append drops them. Any write outside the partition `bounds` is
/// recorded, so a test can require none.
struct Disk {
    blocks: Vec<[u8; 512]>,
    budget: Option<usize>,
    writes: usize,
    outside: usize,
    bounds: (u64, u64),
}

impl Disk {
    fn new() -> Disk {
        Disk {
            blocks: vec![[0u8; 512]; 4096],
            budget: None,
            writes: 0,
            outside: 0,
            bounds: (N150.first_lba, N150.last_lba),
        }
    }
}

impl ControlDisk for Disk {
    fn block_size(&self) -> u32 {
        512
    }

    fn write_block(&mut self, lba: u64, bytes: &[u8]) -> Result<(), DeviceError> {
        if lba < self.bounds.0 || lba > self.bounds.1 {
            self.outside += 1;
        }
        self.writes += 1;
        if self.budget.is_some_and(|budget| self.writes > budget) {
            return Ok(());
        }
        let block = self.blocks.get_mut(lba as usize).ok_or(DeviceError)?;
        block.copy_from_slice(bytes.get(..512).ok_or(DeviceError)?);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }

    fn read_block(&mut self, lba: u64, bytes: &mut [u8]) -> Result<(), DeviceError> {
        let block = self.blocks.get(lba as usize).ok_or(DeviceError)?;
        bytes.copy_from_slice(block);
        Ok(())
    }
}

fn geometry() -> Geometry {
    Geometry::of(&N150, 512).unwrap_or_else(|why| panic!("{why:?}"))
}

/// A pstore region of boot `sequence` holding `text` from the ring's start.
fn region(sequence: u64, text: &[u8]) -> ([u8; HEADER_BYTES], Vec<u8>) {
    let mut header = [0u8; HEADER_BYTES];
    header.copy_from_slice(&pstore::encode(CAPACITY, sequence));
    header[WRITTEN_OFFSET..WRITTEN_OFFSET + 8].copy_from_slice(&(text.len() as u64).to_le_bytes());
    let mut ring = vec![0u8; CAPACITY as usize];
    ring[..text.len()].copy_from_slice(text);
    (header, ring)
}

/// A kernel log that ends in a panic report saying `why`.
fn panicked(why: &str) -> Vec<u8> {
    let mut text = Vec::new();
    text.extend_from_slice(b"\nFerrix 0.1.0 on x86_64\n  memory   16127 MiB\n");
    text.extend_from_slice(b"  init     started\n\n");
    text.extend_from_slice(b"FERRIX-PANIC ");
    text.extend_from_slice(why.as_bytes());
    text.extend_from_slice(b"\n  at        src/kernel/src/power.rs:180:24\n  code      FX-1501\n");
    text
}

/// Compose the event of `text` from boot `sequence` into a record buffer.
fn incoming(sequence: u64, text: &[u8]) -> (Incoming, Vec<u8>) {
    let (header, ring) = region(sequence, text);
    let mut record = vec![0u8; RECORD_LEN];
    let event = compose(&header, &ring, [0; 8], &mut record[HEAD_LEN..])
        .unwrap_or_else(|| panic!("no event composed"));
    (event, record)
}

/// Append the event of `text` from boot `sequence`.
fn append_text(disk: &mut Disk, sequence: u64, text: &[u8]) -> Result<Outcome, AppendError> {
    let (event, mut record) = incoming(sequence, text);
    let mut scratch = vec![0u8; RECORD_LEN];
    append(disk, &geometry(), &event, &mut record, &mut scratch)
}

/// The current header, if a copy is valid.
fn current(disk: &mut Disk) -> Option<Header> {
    match read_header(disk, &geometry()).unwrap_or_else(|why| panic!("{why:?}")) {
        Current::Header(header) => Some(header),
        Current::Fresh | Current::Damaged => None,
    }
}

/// Every counted, valid record, oldest first, as (index, count, first line).
fn listing(disk: &mut Disk) -> Vec<(u64, u32, Vec<u8>)> {
    let geometry = geometry();
    let Some(header) = current(disk) else {
        return Vec::new();
    };
    let mut buffer = vec![0u8; RECORD_LEN];
    let mut found = Vec::new();
    for slot in 0..geometry.records() {
        if let Ok(Some(meta)) = read_record(disk, &geometry, slot, &mut buffer)
            && counted(&header, &meta)
        {
            let text = decode_record(&buffer)
                .map(|(_, text)| text)
                .unwrap_or_default();
            found.push((meta.index, meta.count, first_line(text, 200).to_vec()));
        }
    }
    found.sort();
    found
}

#[test]
fn the_n150_partition_holds_thirty_one_records() {
    let geometry = geometry();
    assert_eq!(geometry.records(), 31);
    assert_eq!(geometry.lba(0), Some(40));
    assert_eq!(geometry.lba(2007 * 512), Some(2047), "the last block");
    assert_eq!(geometry.record_offset(30), Some(8192 + 30 * 32768));
    assert_eq!(geometry.record_offset(31), None);
    let small = Part {
        last_lba: 40 + 16 + 63,
        ..N150
    };
    assert_eq!(
        Geometry::of(&small, 512),
        Err(Refusal::Geometry),
        "one record"
    );
    assert_eq!(
        Geometry::of(&N150, 520),
        Err(Refusal::Geometry),
        "block size"
    );
    assert_eq!(
        Geometry::of(&N150, 4096).map(|g| g.records()),
        Ok(250),
        "2008 blocks of 4 KiB"
    );
}

/// Verifies the range check (E1).
#[test]
fn no_address_outside_the_partition() {
    let geometry = geometry();
    assert_eq!(geometry.lba(2008 * 512), None, "one block past the end");
    assert_eq!(geometry.lba(100), None, "not a whole block");
    assert_eq!(geometry.lba(u64::MAX - 511), None, "no overflow");
    let mut disk = Disk::new();
    let past = vec![0u8; 1024];
    assert_eq!(
        write_bytes(&mut disk, &geometry, 2007 * 512, &past),
        Err(AppendError::Range),
        "a write running past the end is refused before its first block"
    );
    assert_eq!(disk.writes, 0);
    for round in 0..40 {
        let why = std::format!("panic number {round}");
        assert!(append_text(&mut disk, round + 1, &panicked(&why)).is_ok());
    }
    assert_eq!(disk.outside, 0, "every write inside LBA 40-2047");
}

#[test]
fn a_header_round_trips_and_every_field_is_checked() {
    let header = Header {
        sequence: 7,
        next: 3,
        appended: 34,
        bad: BadFrames::empty(),
    };
    let bytes = header.encode(31);
    assert_eq!(Header::decode(&bytes, 31), Ok(header));
    assert_eq!(
        Header::decode(&bytes, 30),
        Err(Refusal::Field),
        "another size"
    );
    for at in 0..HEADER_CHECKED + 4 {
        let mut bad = bytes;
        bad[at] ^= 0x40;
        assert!(Header::decode(&bad, 31).is_err(), "byte {at}");
    }
    let next_out = Header { next: 31, ..header }.encode(31);
    assert_eq!(Header::decode(&next_out, 31), Err(Refusal::Field));
    let zero = Header {
        sequence: 0,
        ..header
    }
    .encode(31);
    assert_eq!(Header::decode(&zero, 31), Err(Refusal::Field));
    let many = Header {
        appended: 1 << 48,
        ..header
    }
    .encode(31);
    assert_eq!(Header::decode(&many, 31), Err(Refusal::Field));
}

#[test]
fn the_newer_valid_copy_is_current() {
    let old = Header::empty();
    let new = old.after_append(31);
    assert_eq!(select(Ok(old), Ok(new)), Some(new));
    assert_eq!(select(Ok(new), Ok(old)), Some(new));
    assert_eq!(select(Err(Refusal::Crc), Ok(old)), Some(old));
    assert_eq!(select(Err(Refusal::Crc), Err(Refusal::Magic)), None);
    let wrapped = Header {
        sequence: 9,
        next: 30,
        appended: 30,
        bad: BadFrames::empty(),
    }
    .after_append(31);
    assert_eq!((wrapped.next, wrapped.appended), (0, 31));
    assert_eq!(wrapped.newest_slot(31), Some(30));
}

#[test]
fn the_crc_continues_across_head_and_text() {
    let whole = pstore::crc32c(b"123456789");
    assert_eq!(whole, 0xE306_9283, "the CRC-32C check value");
    assert_eq!(crc32c_continue(pstore::crc32c(b"1234"), b"56789"), whole);
}

/// Verifies the record's CRC (E4's "never garbage").
#[test]
fn a_record_round_trips_and_a_changed_byte_is_refused() {
    let (event, mut record) = incoming(5, &panicked("one"));
    let text = record[HEAD_LEN..HEAD_LEN + event.text_len].to_vec();
    let meta = Meta {
        index: 4,
        kind: Kind::Panic,
        flags: event.flags,
        count: 1,
        first_boot: 5,
        last_boot: 5,
        identity: event.identity,
        key: event.key,
        first_time: [1; 8],
        last_time: [2; 8],
        text_len: event.text_len as u32,
    };
    assert_eq!(encode_record(&meta, &text, &mut record), Some(()));
    let (decoded, decoded_text) = decode_record(&record).unwrap_or_else(|why| panic!("{why:?}"));
    assert_eq!(decoded, meta);
    assert_eq!(decoded_text, &text[..]);
    for at in [
        0,
        12,
        16,
        24,
        28,
        40,
        56,
        64,
        92,
        HEAD_LEN,
        HEAD_LEN + text.len() - 1,
    ] {
        let mut bad = record.clone();
        bad[at] ^= 0x01;
        assert!(decode_record(&bad).is_err(), "byte {at}");
    }
    assert_eq!(
        encode_record(&meta, &vec![b'x'; TEXT_MAX + 1], &mut record),
        None
    );
    let mut zero = meta;
    zero.count = 0;
    assert_eq!(seal(&zero, &mut record), Some(()));
    assert_eq!(
        decode_record(&record).map(|_| ()),
        Err(Refusal::Field),
        "count 0"
    );
}

#[test]
fn the_text_is_the_report_then_the_log_before_it() {
    let (event, record) = incoming(3, &panicked("init exited"));
    let text = &record[HEAD_LEN..HEAD_LEN + event.text_len];
    assert!(
        text.starts_with(b"FERRIX_PANIC init exited\n"),
        "defused, first"
    );
    let separator = text
        .windows(SEPARATOR.len())
        .position(|window| window == SEPARATOR)
        .unwrap_or_else(|| panic!("no separator"));
    assert!(
        text[separator..].ends_with(b"  init     started\n\n"),
        "the log before"
    );
    assert!(text[separator..].windows(6).any(|w| w == b"Ferrix"));
    assert_eq!(event.flags, flags::UNCHECKED, "whole, unchecked");
    assert_eq!(event.boot, 3);
    assert_eq!(event.identity >> 32, 3);
    assert_eq!(first_line(text, 13), b"FERRIX_PANIC ");
}

#[test]
fn a_long_log_keeps_the_report_and_the_lines_before_it() {
    let mut text = Vec::new();
    for line in 0..5000 {
        text.extend_from_slice(std::format!("  line {line:05} of the boot\n").as_bytes());
    }
    text.extend_from_slice(&panicked("late")[1..]);
    let (event, record) = incoming(1, &text);
    let kept = &record[HEAD_LEN..HEAD_LEN + event.text_len];
    assert_eq!(event.flags, flags::UNCHECKED | flags::TRUNCATED);
    assert!(kept.starts_with(b"FERRIX_PANIC late\n"));
    let separator = kept
        .windows(SEPARATOR.len())
        .position(|window| window == SEPARATOR)
        .unwrap_or_else(|| panic!("no separator"));
    assert!(
        kept[separator + SEPARATOR.len()..].starts_with(b"  line "),
        "from a line's start"
    );
    assert!(event.text_len <= TEXT_MAX);
}

#[test]
fn no_panic_report_is_no_event() {
    let (header, ring) = region(2, b"\nFerrix 0.1.0\n  power    resetting\n");
    let mut text = vec![0u8; TEXT_MAX];
    assert_eq!(compose(&header, &ring, [0; 8], &mut text), None);
    let mut short = vec![0u8; 10];
    let (header, ring) = region(2, &panicked("x"));
    assert_eq!(
        compose(&header, &ring, [0; 8], &mut short),
        None,
        "a short buffer"
    );
}

/// RAM a panicked kernel or firmware left: whatever its bytes, compose
/// answers without panicking and within its buffer (E5).
#[test]
fn hostile_regions_are_answered() {
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut text = vec![0u8; TEXT_MAX];
    for round in 0..300 {
        let (mut header, mut ring) = region(round + 1, &panicked("hostile"));
        // A valid header with any count, or any header at all.
        let written = next() % (1 << 49);
        header[WRITTEN_OFFSET..WRITTEN_OFFSET + 8].copy_from_slice(&written.to_le_bytes());
        if round % 3 == 0 {
            for byte in header.iter_mut() {
                *byte = next() as u8;
            }
        }
        for _ in 0..64 {
            let at = (next() % CAPACITY) as usize;
            ring[at] = next() as u8;
        }
        if let Some(event) = compose(&header, &ring, [0; 8], &mut text) {
            assert!(event.text_len <= TEXT_MAX);
            assert!(
                text[..event.text_len]
                    .iter()
                    .all(|&b| b == b'\n' || b == b'\t' || (0x20..0x7f).contains(&b))
            );
        }
    }
}

#[test]
fn an_empty_partition_gets_a_header_and_the_first_record() {
    let mut disk = Disk::new();
    assert_eq!(read_header(&mut disk, &geometry()), Ok(Current::Fresh));
    assert_eq!(
        append_text(&mut disk, 1, &panicked("first")),
        Ok(Outcome::Appended { index: 0, slot: 0 })
    );
    let header = current(&mut disk);
    assert_eq!(header.map(|h| (h.next, h.appended)), Some((1, 1)));
    assert_eq!(
        listing(&mut disk),
        vec![(0, 1, b"FERRIX_PANIC first".to_vec())]
    );
}

/// Verifies idempotence (E3).
#[test]
fn the_same_pstore_record_is_appended_once() {
    let mut disk = Disk::new();
    let text = panicked("once");
    assert!(matches!(
        append_text(&mut disk, 4, &text),
        Ok(Outcome::Appended { .. })
    ));
    // The loader after a reset during the header write, or the inspection
    // Linux after the loader: the same record again.
    assert_eq!(
        append_text(&mut disk, 4, &text),
        Ok(Outcome::Skipped { index: 0 })
    );
    assert_eq!(
        append_text(&mut disk, 4, &text),
        Ok(Outcome::Skipped { index: 0 })
    );
    assert_eq!(listing(&mut disk).len(), 1);
}

/// Verifies the collapse of a reset loop (E8).
#[test]
fn a_reset_loop_is_one_record_with_a_count() {
    let mut disk = Disk::new();
    assert!(append_text(&mut disk, 1, &panicked("other")).is_ok());
    for boot in 1..=50 {
        // Each boot's log differs (a longer uptime), its panic does not.
        let mut text = panicked("the same fault");
        text.extend_from_slice(std::format!("  boot {boot}\n").as_bytes());
        let outcome = append_text(&mut disk, boot, &text);
        if boot == 1 {
            assert_eq!(outcome, Ok(Outcome::Appended { index: 1, slot: 1 }));
        } else {
            assert_eq!(
                outcome,
                Ok(Outcome::Collapsed {
                    index: 1,
                    count: boot as u32
                })
            );
        }
    }
    assert_eq!(
        listing(&mut disk),
        vec![
            (0, 1, b"FERRIX_PANIC other".to_vec()),
            (1, 50, b"FERRIX_PANIC the same fault".to_vec())
        ],
        "the first record of the run stays, its copies counted"
    );
}

#[test]
fn the_ring_overwrites_its_oldest() {
    let mut disk = Disk::new();
    for round in 0..40u64 {
        let why = std::format!("fault {round}");
        assert!(append_text(&mut disk, round + 1, &panicked(&why)).is_ok());
    }
    let listed = listing(&mut disk);
    assert_eq!(listed.len(), 31);
    assert_eq!(
        listed.first().map(|entry| entry.0),
        Some(9),
        "the oldest nine gone"
    );
    assert_eq!(listed.last().map(|entry| entry.0), Some(39));
}

/// Verifies torn writes (E4): power lost after every number of block writes
/// of an append leaves the newest valid record or the one before, never a
/// counted record that is not whole.
#[test]
fn a_torn_append_leaves_the_newest_or_the_previous_record() {
    // An append to a log that has one record: 64 record blocks, then two
    // header blocks; and a collapse: 64 blocks in place.
    for budget in 0..=70 {
        let mut disk = Disk::new();
        assert!(append_text(&mut disk, 1, &panicked("before")).is_ok());
        disk.writes = 0;
        disk.budget = Some(budget);
        let _ = append_text(&mut disk, 2, &panicked("new"));
        disk.budget = None;
        let listed = listing(&mut disk);
        let lines: Vec<&[u8]> = listed.iter().map(|entry| &entry.2[..]).collect();
        assert!(
            lines == [&b"FERRIX_PANIC before"[..]]
                || lines == [&b"FERRIX_PANIC before"[..], &b"FERRIX_PANIC new"[..]],
            "after {budget} writes: {lines:?}"
        );
        // And the log goes on: the next append lands whole.
        assert!(append_text(&mut disk, 3, &panicked("after")).is_ok());
        assert_eq!(
            listing(&mut disk).last().map(|e| e.2.clone()),
            Some(b"FERRIX_PANIC after".to_vec())
        );
    }
    for budget in 0..=64 {
        let mut disk = Disk::new();
        assert!(append_text(&mut disk, 1, &panicked("before")).is_ok());
        assert!(append_text(&mut disk, 2, &panicked("loop")).is_ok());
        disk.writes = 0;
        disk.budget = Some(budget);
        let _ = append_text(&mut disk, 3, &panicked("loop"));
        disk.budget = None;
        let listed = listing(&mut disk);
        assert!(
            !listed.is_empty() && listed[0].2 == b"FERRIX_PANIC before",
            "after {budget}"
        );
        assert!(listed.iter().all(|entry| entry.1 == 1 || entry.1 == 2));
    }
}

#[test]
fn a_header_copy_torn_falls_back_to_the_other() {
    let mut disk = Disk::new();
    assert!(append_text(&mut disk, 1, &panicked("a")).is_ok());
    assert!(append_text(&mut disk, 2, &panicked("b")).is_ok());
    // Copy A damaged: copy B, as new, is current.
    disk.blocks[40][5] ^= 1;
    let header = current(&mut disk);
    assert_eq!(header.map(|h| h.appended), Some(2));
    assert_eq!(listing(&mut disk).len(), 2);
}

/// Verifies the absent/unusable decision (the consultant's (2)).
#[test]
fn a_present_but_unusable_log_is_never_taken_for_absent() {
    use ferrix_slots::gpt::{GptError, Lookup};
    let unusable = |store: Store| matches!(store, Store::Unusable(_));
    // Absent: no GPT at all, or no partition of the type.
    assert_eq!(
        classify(Scan::Scanned(Err(Lookup::Missing)), 512),
        Store::Absent
    );
    assert_eq!(
        classify(
            Scan::Scanned(Err(Lookup::Table(GptError::NoSignature))),
            512
        ),
        Store::Absent
    );
    // Unusable: two logs, an overlapping one, a damaged table or array, a
    // too small one, a block size the log is not kept on, a read error.
    assert_eq!(
        classify(Scan::Scanned(Err(Lookup::Many)), 512),
        Store::Unusable(Unusable::Many)
    );
    for why in [
        GptError::Overlap,
        GptError::Range,
        GptError::Entries,
        GptError::Header,
    ] {
        assert_eq!(
            classify(Scan::Scanned(Err(Lookup::Table(why))), 512),
            Store::Unusable(Unusable::Table(why)),
            "{why:?}"
        );
    }
    let small = Part {
        last_lba: N150.first_lba + 16 + 63,
        ..N150
    };
    assert!(
        unusable(classify(Scan::Scanned(Ok(small)), 512)),
        "too small"
    );
    assert!(
        unusable(classify(Scan::Scanned(Ok(N150)), 520)),
        "block size"
    );
    assert_eq!(
        classify(Scan::Unreadable, 512),
        Store::Unusable(Unusable::Read)
    );
    // Usable: the box's.
    assert_eq!(
        classify(Scan::Scanned(Ok(N150)), 512),
        Store::Present(geometry())
    );
}

/// A version-1 region, as a kernel before the stop word left it, with
/// bytes where version 2's stop word is: still a panic's record, and kept
/// (`docs/POSTMORTEM.md` §5.3, the consultant's C10).
#[test]
fn a_version_1_region_with_a_panic_is_kept() {
    let (mut header, ring) = region(4, &panicked("before the stop word"));
    header[8..12].copy_from_slice(&pstore::VERSION_1.to_le_bytes());
    let crc = pstore::crc32c(&header[..32]);
    header[32..36].copy_from_slice(&crc.to_le_bytes());
    header[pstore::STOP_OFFSET..pstore::STOP_OFFSET + 8].copy_from_slice(&[0xA5; 8]);
    let mut record = vec![0u8; RECORD_LEN];
    let event = compose(&header, &ring, [0; 8], &mut record[HEAD_LEN..])
        .unwrap_or_else(|| panic!("a version-1 panic composed no event"));
    let mut disk = Disk::new();
    let mut scratch = vec![0u8; RECORD_LEN];
    assert_eq!(
        append(&mut disk, &geometry(), &event, &mut record, &mut scratch),
        Ok(Outcome::Appended { index: 0, slot: 0 })
    );
    assert_eq!(
        listing(&mut disk),
        vec![(0, 1, b"FERRIX_PANIC before the stop word".to_vec())]
    );
}

/// A set of these frames.
fn set_of(frames: &[u32]) -> BadFrames {
    let mut set = BadFrames::empty();
    for &frame in frames {
        assert_eq!(set.insert(frame), Insert::Added, "{frame}");
    }
    set
}

/// The extension's CRC computed again over `bytes`, so that a test reaches
/// the field checks behind it.
fn reseal(bytes: &mut [u8; HEADER_USED]) {
    let crc = extension_crc(bytes);
    bytes[EXTENSION_CHECKED..EXTENSION_CHECKED + 4].copy_from_slice(&crc.to_le_bytes());
}

/// Header copy A's and B's first blocks: LBA 40 and 48.
const COPY_A: usize = 40;
const COPY_B: usize = 48;

/// Verifies the set's order, dedupe and bound (`MACHINE-CHECK-RESTART.md`
/// §9.1).
#[test]
fn the_set_is_ascending_without_duplicates_and_holds_sixty_four() {
    let mut set = BadFrames::empty();
    assert_eq!(set.insert(9), Insert::Added);
    assert_eq!(set.insert(3), Insert::Added);
    assert_eq!(set.insert(5), Insert::Added);
    assert_eq!(set.insert(3), Insert::Already);
    assert_eq!(set.frames(), &[3, 5, 9]);
    let mut full = BadFrames::empty();
    for frame in 0..64 {
        assert_eq!(full.insert(1000 - frame), Insert::Added);
    }
    assert_eq!(full.insert(1), Insert::Full);
    assert_eq!(full.insert(1000), Insert::Already);
    assert_eq!(full.len(), BAD_FRAMES_MOST);
    assert!(full.frames().windows(2).all(|pair| pair[0] < pair[1]));
}

/// Verifies the extension's format: an empty set leaves a PM4 header byte
/// for byte, and every byte and field of a set is checked.
#[test]
fn the_extension_round_trips_and_every_field_is_checked() {
    let plain = Header {
        sequence: 7,
        next: 3,
        appended: 34,
        bad: BadFrames::empty(),
    };
    let bytes = plain.encode(31);
    assert!(bytes[44..].iter().all(|&byte| byte == 0), "a PM4 header");
    let header = Header {
        bad: set_of(&[3, 5, 0x1cd6c]),
        ..plain
    };
    let bytes = header.encode(31);
    assert_eq!(
        bytes[..44],
        plain.encode(31)[..44],
        "the header's bytes unchanged"
    );
    assert_eq!(Header::decode(&bytes, 31), Ok(header));
    for at in 44..HEADER_USED {
        let mut bad = bytes;
        bad[at] ^= 0x40;
        assert!(Header::decode(&bad, 31).is_err(), "byte {at}");
    }
    // Field checks behind a good CRC.
    let fields: [(usize, u32); 4] = [
        (EXTENSION_AT + 8, 2),   // another version: a newer format
        (EXTENSION_COUNT, 65),   // more than 64
        (EXTENSION_COUNT, 0),    // a set of none written as one
        (EXTENSION_RESERVED, 1), // reserved
    ];
    for (at, value) in fields {
        let mut bad = bytes;
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        reseal(&mut bad);
        assert!(Header::decode(&bad, 31).is_err(), "{at} = {value}");
    }
    let mut unordered = bytes;
    unordered[EXTENSION_FRAMES..EXTENSION_FRAMES + 4].copy_from_slice(&9u32.to_le_bytes());
    reseal(&mut unordered);
    assert_eq!(
        Header::decode(&unordered, 31),
        Err(Refusal::Field),
        "out of order"
    );
    let mut twice = bytes;
    twice[EXTENSION_FRAMES + 4..EXTENSION_FRAMES + 8].copy_from_slice(&3u32.to_le_bytes());
    reseal(&mut twice);
    assert_eq!(
        Header::decode(&twice, 31),
        Err(Refusal::Field),
        "a duplicate"
    );
    let mut tail = bytes;
    tail[EXTENSION_FRAMES + 12] = 1;
    reseal(&mut tail);
    assert_eq!(
        Header::decode(&tail, 31),
        Err(Refusal::Field),
        "a non-zero tail"
    );
    // No extension's magic, but a stray byte: invalid, not an empty set.
    let mut stray = plain.encode(31);
    stray[200] = 1;
    assert!(Header::decode(&stray, 31).is_err(), "a stray byte");
}

/// Verifies the extension's CRC binds it to its copy's header.
#[test]
fn an_extension_moved_to_the_other_copy_is_refused() {
    let bad = set_of(&[11, 12]);
    let a = Header {
        sequence: 4,
        next: 1,
        appended: 1,
        bad,
    }
    .encode(31);
    let b = Header {
        sequence: 5,
        next: 2,
        appended: 2,
        bad,
    }
    .encode(31);
    let mut spliced = b;
    spliced[EXTENSION_AT..].copy_from_slice(&a[EXTENSION_AT..]);
    assert_eq!(Header::decode(&spliced, 31), Err(Refusal::Crc));
}

/// Verifies `add_bad_frames`: merged in order, nothing written when none is
/// new, the generation one more when one is, the bound said.
#[test]
fn frames_are_added_once_and_an_unchanged_set_writes_nothing() {
    let mut disk = Disk::new();
    let geometry = geometry();
    let added = add_bad_frames(&mut disk, &geometry, &[7, 3, 7]);
    assert_eq!(
        added,
        Ok(Added {
            added: 2,
            already: 1,
            full: 0,
            held: 2
        })
    );
    let first = current(&mut disk).unwrap_or_else(|| panic!("a header"));
    assert_eq!(first.bad.frames(), &[3, 7]);
    let writes = disk.writes;
    assert_eq!(
        add_bad_frames(&mut disk, &geometry, &[3, 7]).map(|added| added.added),
        Ok(0)
    );
    assert_eq!(disk.writes, writes, "an unchanged set writes no block");
    assert!(add_bad_frames(&mut disk, &geometry, &[5]).is_ok());
    let second = current(&mut disk).unwrap_or_else(|| panic!("a header"));
    assert_eq!(second.bad.frames(), &[3, 5, 7]);
    assert_eq!(second.sequence, first.sequence + 1);
    assert_eq!(disk.writes, writes + 2, "copy A, then copy B");
    let many: Vec<u32> = (100..200).collect();
    let full = add_bad_frames(&mut disk, &geometry, &many).unwrap_or_else(|why| panic!("{why:?}"));
    assert_eq!((full.added, full.full, full.held), (61, 39, 64));
    assert_eq!(disk.outside, 0, "every write inside LBA 40-2047");
}

/// Verifies that a record never drops the set (§9.2: `append` carries it).
#[test]
fn an_append_keeps_the_bad_frames() {
    let mut disk = Disk::new();
    assert!(add_bad_frames(&mut disk, &geometry(), &[42]).is_ok());
    assert!(append_text(&mut disk, 1, &panicked("a")).is_ok());
    assert!(append_text(&mut disk, 2, &panicked("b")).is_ok());
    assert!(append_text(&mut disk, 3, &panicked("b")).is_ok());
    let header = current(&mut disk).unwrap_or_else(|| panic!("a header"));
    assert_eq!(header.bad.frames(), &[42]);
    assert_eq!(header.appended, 2);
}

/// Verifies that a torn `add_bad_frames` never loses a frame the set held:
/// stopped after every number of block writes, the set read back is the old
/// one or the new one.
#[test]
fn a_torn_add_leaves_the_old_set_or_the_new() {
    for budget in 0..=3 {
        let mut disk = Disk::new();
        assert!(add_bad_frames(&mut disk, &geometry(), &[1, 2]).is_ok());
        disk.writes = 0;
        disk.budget = Some(budget);
        let _ = add_bad_frames(&mut disk, &geometry(), &[3]);
        disk.budget = None;
        let header = current(&mut disk).unwrap_or_else(|| panic!("budget {budget}: a header"));
        let frames = header.bad.frames();
        assert!(
            frames == [1, 2] || frames == [1, 2, 3],
            "budget {budget}: {frames:?}"
        );
    }
    // A copy torn inside its block: the other copy holds.
    let mut disk = Disk::new();
    assert!(add_bad_frames(&mut disk, &geometry(), &[1, 2]).is_ok());
    assert!(add_bad_frames(&mut disk, &geometry(), &[3]).is_ok());
    disk.blocks[COPY_A][EXTENSION_FRAMES] ^= 1;
    let header = current(&mut disk).unwrap_or_else(|| panic!("a header"));
    assert_eq!(header.bad.frames(), &[1, 2, 3]);
}

/// Verifies no fresh header over damage (`MACHINE-CHECK-RESTART.md` §9.2,
/// ledger 5511 (3)): copies that carry the magic and both fail to decode,
/// or carry a newer extension, get no write.
#[test]
fn damaged_or_newer_copies_are_never_written_over() {
    let mut damaged = Disk::new();
    assert!(append_text(&mut damaged, 1, &panicked("a")).is_ok());
    damaged.blocks[COPY_A][41] ^= 1;
    damaged.blocks[COPY_B][41] ^= 1;
    let mut newer = Disk::new();
    assert!(add_bad_frames(&mut newer, &geometry(), &[5]).is_ok());
    for copy in [COPY_A, COPY_B] {
        let block = &mut newer.blocks[copy];
        block[EXTENSION_AT + 8..EXTENSION_AT + 12].copy_from_slice(&2u32.to_le_bytes());
        let mut bytes = [0u8; HEADER_USED];
        bytes.copy_from_slice(&block[..HEADER_USED]);
        reseal(&mut bytes);
        block[..HEADER_USED].copy_from_slice(&bytes);
    }
    for (name, disk) in [("damaged", &mut damaged), ("newer", &mut newer)] {
        assert_eq!(
            read_header(disk, &geometry()),
            Ok(Current::Damaged),
            "{name}"
        );
        let writes = disk.writes;
        assert_eq!(
            add_bad_frames(disk, &geometry(), &[9]),
            Err(AppendError::Damaged),
            "{name}"
        );
        assert_eq!(
            append_text(disk, 2, &panicked("b")),
            Err(AppendError::Damaged),
            "{name}"
        );
        assert_eq!(
            clear_bad_frames(disk, &geometry()),
            Err(AppendError::Damaged)
        );
        assert_eq!(disk.writes, writes, "{name}: no block written");
    }
}

/// Verifies `clear_bad_frames`: the set emptied, the generation one more,
/// the frames dropped returned; torn, the old set or none.
#[test]
fn the_clear_empties_the_set_and_a_torn_clear_leaves_old_or_none() {
    let mut disk = Disk::new();
    assert!(append_text(&mut disk, 1, &panicked("a")).is_ok());
    assert!(add_bad_frames(&mut disk, &geometry(), &[8, 4]).is_ok());
    let before = current(&mut disk).unwrap_or_else(|| panic!("a header"));
    let dropped = clear_bad_frames(&mut disk, &geometry()).unwrap_or_else(|why| panic!("{why:?}"));
    assert_eq!(dropped.frames(), &[4, 8]);
    let after = current(&mut disk).unwrap_or_else(|| panic!("a header"));
    assert!(after.bad.is_empty());
    assert_eq!(after.sequence, before.sequence + 1);
    assert_eq!(after.appended, 1, "the records stay");
    let writes = disk.writes;
    assert!(clear_bad_frames(&mut disk, &geometry()).is_ok());
    assert_eq!(disk.writes, writes, "an empty set: nothing written");
    for budget in 0..=2 {
        let mut disk = Disk::new();
        assert!(add_bad_frames(&mut disk, &geometry(), &[8, 4]).is_ok());
        disk.writes = 0;
        disk.budget = Some(budget);
        let _ = clear_bad_frames(&mut disk, &geometry());
        disk.budget = None;
        let header = current(&mut disk).unwrap_or_else(|| panic!("budget {budget}: a header"));
        let frames = header.bad.frames();
        assert!(
            frames == [4, 8] || frames.is_empty(),
            "budget {budget}: {frames:?}"
        );
    }
}

/// Verifies the region rule (`MACHINE-CHECK-RESTART.md` §9.2): the region is
/// renewed unless the log failed to take a record or a frame.
#[test]
fn the_region_is_renewed_only_when_nothing_failed() {
    use Durability::{Durable, Failed, NotNeeded};
    for record in [Durable, NotNeeded, Failed] {
        for frames in [Durable, NotNeeded, Failed] {
            assert_eq!(
                may_renew(record, frames),
                record != Failed && frames != Failed,
                "{record:?} {frames:?}"
            );
        }
    }
}
