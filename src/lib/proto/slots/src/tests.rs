//! Host tests of the formats, the selection, the GPT scan and the plan.

extern crate std;

use std::string::ToString;
use std::vec;
use std::vec::Vec;

use crate::Slot;
use crate::crc::{crc32, crc32c};
use crate::gpt::{self, CONTROL_TYPE, GptError, SLOT_TYPE};
use crate::plan::{Attempt, Kind, LoaderWords, plan, record_block, write_words};
use crate::record::{
    COPY_OFFSETS, Current, MAX_TRIES, RECORD_LEN, Record, RecordError, select, write_copy,
};
use crate::sha256::{Sha256, digest};
use crate::slot::{HEADER_LEN, SlotError, SlotHeader};

fn hex(bytes: &[u8]) -> std::string::String {
    bytes
        .iter()
        .map(|byte| std::format!("{byte:02x}"))
        .collect()
}

#[test]
fn checksums_match_their_check_values() {
    // The standard check value of each CRC over "123456789".
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn sha256_matches_fips_180_4() {
    assert_eq!(
        hex(&digest(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&digest(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&digest(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    let million = vec![b'a'; 1_000_000];
    assert_eq!(
        hex(&digest(&million)),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
    // Fed in uneven pieces, the same digest.
    let mut hash = Sha256::new();
    for piece in million.chunks(997) {
        hash.update(piece);
    }
    assert_eq!(hash.finish(), digest(&million));
}

fn record(sequence: u64, good: Slot, trial: Option<Slot>, tries: u8) -> Record {
    Record {
        sequence,
        good,
        trial,
        tries,
    }
}

#[test]
fn a_record_round_trips() {
    for good in Slot::BOTH {
        for (trial, tries) in [(None, 0), (Some(good.other()), 1), (Some(good.other()), 7)] {
            let value = record(42, good, trial, tries);
            assert_eq!(Record::decode(&value.encode()), Ok(value));
        }
    }
}

#[test]
fn a_record_refuses_every_bad_field() {
    let good = record(5, Slot::A, Some(Slot::B), 1).encode();
    assert_eq!(Record::decode(&good[..511]), Err(RecordError::Short));
    assert_eq!(Record::decode(&[0u8; 512]), Err(RecordError::Magic));
    // Change one byte, fix the CRC, and see the field's own refusal.
    let refix = |at: usize, value: u8| {
        let mut bytes = good;
        bytes[at] = value;
        let crc = crc32c(&bytes[..508]);
        bytes[508..].copy_from_slice(&crc.to_le_bytes());
        Record::decode(&bytes)
    };
    assert_eq!(refix(8, 2), Err(RecordError::Version));
    assert_eq!(refix(11, 0), Err(RecordError::Version));
    assert_eq!(refix(12, 1), Err(RecordError::Reserved));
    assert_eq!(refix(27, 1), Err(RecordError::Reserved));
    assert_eq!(refix(507, 1), Err(RecordError::Reserved));
    assert_eq!(refix(24, 2), Err(RecordError::Field));
    assert_eq!(refix(25, 0), Err(RecordError::Field)); // trial == good
    assert_eq!(refix(25, 2), Err(RecordError::Field));
    assert_eq!(refix(26, MAX_TRIES + 1), Err(RecordError::Field));
    let none_with_tries = {
        let mut bytes = record(5, Slot::A, None, 0).encode();
        bytes[26] = 1;
        let crc = crc32c(&bytes[..508]);
        bytes[508..].copy_from_slice(&crc.to_le_bytes());
        Record::decode(&bytes)
    };
    assert_eq!(none_with_tries, Err(RecordError::Field));
    // Any single flipped bit is caught.
    for bit in 0..RECORD_LEN * 8 {
        let mut bytes = good;
        bytes[bit / 8] ^= 1 << (bit % 8);
        assert!(Record::decode(&bytes).is_err(), "bit {bit}");
    }
}

#[test]
fn the_newer_valid_copy_is_current() {
    let old = record(7, Slot::A, Some(Slot::B), 1).encode();
    let new = record(8, Slot::A, Some(Slot::B), 0).encode();
    let zero = [0u8; 512];
    let mut torn = new;
    torn[300..].fill(0xA5);
    let valid = |copy, sequence| match select_pair(copy) {
        Current::Valid { copy: at, record } => (at, record.sequence) == sequence,
        Current::Uninitialised => false,
    };
    fn select_pair((first, second): (&[u8], &[u8])) -> Current {
        select(first, second)
    }
    assert!(valid((&old, &new), (1, 8)));
    assert!(valid((&new, &old), (0, 8)));
    assert!(valid((&old, &torn), (0, 7)));
    assert!(valid((&torn, &old), (1, 7)));
    assert!(valid((&zero, &old), (1, 7)));
    assert!(valid((&old, &old), (0, 7)));
    assert_eq!(select(&zero, &zero), Current::Uninitialised);
    assert_eq!(select(&torn, &zero), Current::Uninitialised);
}

#[test]
fn a_write_torn_anywhere_leaves_the_old_record_or_the_new() {
    // Copy 1 holds the current record, so the write goes to copy 0, which
    // holds the one before it. A cut after any byte leaves either the old
    // current (never the older one, never garbage) or the new one.
    let before = record(6, Slot::A, None, 0);
    let current = record(7, Slot::A, Some(Slot::B), 1);
    let copies = [before.encode(), current.encode()];
    let now = select(&copies[0], &copies[1]);
    assert_eq!(write_copy(&now), 0);
    let Some(next) = current.after_try() else {
        panic!("a try to take");
    };
    let written = next.encode();
    for cut in 0..=RECORD_LEN {
        let mut copy0 = copies[0];
        copy0[..cut].copy_from_slice(&written[..cut]);
        let result = select(&copy0, &copies[1]);
        let Current::Valid { record, .. } = result else {
            panic!("cut at {cut}: no record");
        };
        assert!(
            record == current || record == next,
            "cut at {cut}: {record:?}"
        );
        assert_eq!(record == next, cut == RECORD_LEN, "cut at {cut}");
    }
}

#[test]
fn the_record_transitions() {
    let armed = record(3, Slot::A, Some(Slot::B), 2);
    assert_eq!(
        armed.after_try(),
        Some(record(4, Slot::A, Some(Slot::B), 1))
    );
    assert_eq!(record(3, Slot::A, Some(Slot::B), 0).after_try(), None);
    assert_eq!(record(3, Slot::A, None, 0).after_try(), None);
    assert_eq!(armed.marked_good(), Some(record(4, Slot::B, None, 0)));
    assert_eq!(armed.disarmed(), Some(record(4, Slot::A, None, 0)));
    let quiet = record(3, Slot::A, None, 0);
    assert_eq!(
        quiet.armed(Slot::B, 1),
        Some(record(4, Slot::A, Some(Slot::B), 1))
    );
    assert_eq!(
        quiet.armed(Slot::A, 1),
        None,
        "the good slot is never a trial"
    );
    assert_eq!(quiet.armed(Slot::B, 0), None);
    assert_eq!(quiet.armed(Slot::B, MAX_TRIES + 1), None);
    assert_eq!(
        record(u64::MAX, Slot::A, Some(Slot::B), 1).after_try(),
        None
    );
}

#[test]
fn the_plan_follows_the_record() {
    let attempts = |current: Current| plan(&current).attempts().collect::<Vec<_>>();
    let at = |slot, kind| Attempt { slot, kind };
    assert_eq!(
        attempts(Current::Uninitialised),
        [at(Slot::A, Kind::Good), at(Slot::B, Kind::Good)]
    );
    for good in Slot::BOTH {
        let other = good.other();
        let valid = |trial, tries| Current::Valid {
            copy: 0,
            record: record(9, good, trial, tries),
        };
        assert_eq!(
            attempts(valid(None, 0)),
            [at(good, Kind::Good), at(other, Kind::Fallback)]
        );
        // A failed trial is never a fallback (the consultant's A2): the
        // ESP's rescue files come before it.
        assert_eq!(attempts(valid(Some(other), 0)), [at(good, Kind::Good)]);
        for tries in 1..=MAX_TRIES {
            assert_eq!(
                attempts(valid(Some(other), tries)),
                [at(other, Kind::Trial), at(good, Kind::Good)]
            );
        }
    }
}

#[test]
fn record_blocks_stay_in_the_control_partition() {
    let control = gpt::Part {
        index: 2,
        unique: [0; 16],
        first_lba: 1000,
        last_lba: 1000 + 2047,
    };
    assert_eq!(record_block(&control, 0, 512), Some(1000));
    assert_eq!(record_block(&control, 1, 512), Some(1128));
    assert_eq!(record_block(&control, 1, 4096), Some(1016));
    assert_eq!(record_block(&control, 2, 512), None);
    assert_eq!(record_block(&control, 0, 256), None);
    let tiny = gpt::Part {
        last_lba: 1000 + 100,
        ..control
    };
    assert_eq!(record_block(&tiny, 1, 512), None);
    assert_eq!(COPY_OFFSETS, [0, 65536]);
}

#[test]
fn loader_words_are_written_or_refused() {
    let words = LoaderWords {
        slot: Slot::B,
        kind: Kind::Trial,
        sequence: 12,
        disk: DISK,
    };
    let mut out = [0u8; 128];
    let Some(len) = write_words(&words, &mut out) else {
        panic!("room enough");
    };
    assert_eq!(
        &out[..len],
        b"ferrix.slot=b ferrix.slot.boot=trial ferrix.slot.seq=12 \
          ferrix.slot.disk=12345678-9ABC-DEF0-1122-334455667788"
    );
    assert_eq!(write_words(&words, &mut [0u8; 20]), None);
}

fn slot_image(kernel: &[u8], initrd: &[u8], cmdline: &[u8]) -> (SlotHeader, Vec<u8>) {
    let header = SlotHeader::lay_out(3, "ferrix test", kernel, initrd, cmdline);
    let mut image = vec![0u8; header.needed() as usize];
    image[..HEADER_LEN].copy_from_slice(&header.encode());
    for (blob, bytes) in header.blobs().iter().zip([kernel, initrd, cmdline]) {
        let at = blob.offset as usize;
        image[at..at + bytes.len()].copy_from_slice(bytes);
    }
    (header, image)
}

#[test]
fn a_slot_round_trips_and_verifies() {
    let kernel = vec![0x7F; 10_000];
    let initrd = vec![0x30; 70_000];
    let (header, image) = slot_image(&kernel, &initrd, b"ferrix.root=tmpfs");
    assert_eq!(header.kernel.offset, 4096);
    assert_eq!(header.initrd.offset, 4096 * 4);
    assert_eq!(header.cmdline.offset, 4096 * 22);
    let read = SlotHeader::decode(&image, 1 << 20);
    assert_eq!(read, Ok(header));
    assert_eq!(header.label_text(), "ferrix test");
    for (blob, bytes) in header
        .blobs()
        .iter()
        .zip([&kernel[..], &initrd, b"ferrix.root=tmpfs"])
    {
        let at = blob.offset as usize;
        assert!(blob.matches(&image[at..at + bytes.len()]));
    }
    let mut flipped = initrd.clone();
    flipped[69_999] ^= 1;
    assert!(!header.initrd.matches(&flipped));
    assert!(!header.initrd.matches(&initrd[..69_999]));
    // No initramfs is a zero-length blob, and still verifies.
    let (bare, image) = slot_image(&kernel, &[], b"");
    assert_eq!(SlotHeader::decode(&image, 1 << 20), Ok(bare));
}

#[test]
fn a_slot_header_refuses_what_it_must() {
    let (header, image) = slot_image(&[1; 5000], &[2; 5000], b"x");
    assert_eq!(
        SlotHeader::decode(&image[..4095], 1 << 20),
        Err(SlotError::Short)
    );
    assert_eq!(
        SlotHeader::decode(&[0u8; 4096], 1 << 20),
        Err(SlotError::Magic)
    );
    // Too small a slot for its blobs.
    assert_eq!(
        SlotHeader::decode(&image, header.needed() - 1),
        Err(SlotError::Placement)
    );
    // Every bit of the header is sealed.
    for bit in (0..HEADER_LEN * 8).step_by(7) {
        let mut bytes = image.clone();
        bytes[bit / 8] ^= 1 << (bit % 8);
        assert!(SlotHeader::decode(&bytes, 1 << 20).is_err(), "bit {bit}");
    }
    let encode = |edit: &dyn Fn(&mut SlotHeader)| {
        let mut changed = header;
        edit(&mut changed);
        SlotHeader::decode(&changed.encode(), 1 << 20)
    };
    assert_eq!(encode(&|h| h.kernel.offset = 0), Err(SlotError::Placement));
    assert_eq!(
        encode(&|h| h.kernel.offset = 4097),
        Err(SlotError::Placement)
    );
    assert_eq!(
        encode(&|h| h.initrd.offset = 4096),
        Err(SlotError::Placement)
    );
    assert_eq!(
        encode(&|h| h.cmdline.length = u64::MAX),
        Err(SlotError::Placement)
    );
    assert_eq!(encode(&|h| h.kernel.length = 0), Err(SlotError::Length));
    assert_eq!(encode(&|h| h.cmdline.length = 2049), Err(SlotError::Length));
}

const DISK_BLOCKS: u64 = 1 << 16;

/// A disk GUID whose text is easy to read.
const DISK: gpt::Guid = gpt::guid(0x1234_5678, 0x9abc, 0xdef0, 0x1122, 0x3344_5566_7788);

fn table(parts: &[(ferrix_partition::Guid, u64, u64)]) -> (Vec<u8>, Vec<u8>) {
    let partitions: Vec<_> = parts
        .iter()
        .enumerate()
        .map(|(index, &(kind, first, last))| {
            ferrix_partition::Partition::new(
                kind,
                ferrix_partition::Guid::random([index as u8 + 1; 16]),
                first,
                last,
                "p",
            )
        })
        .collect();
    let writes = ferrix_partition::write_table(
        DISK_BLOCKS,
        512,
        ferrix_partition::Guid::random([9; 16]),
        &partitions,
    )
    .unwrap_or_else(|error| panic!("{error:?}"));
    let mut disk = vec![0u8; 34 * 512];
    for (sector, bytes) in writes {
        if sector < 34 {
            let at = sector as usize * 512;
            disk[at..at + bytes.len()].copy_from_slice(&bytes);
        }
    }
    (disk[512..1024].to_vec(), disk[1024..34 * 512].to_vec())
}

fn find(parts: &[(ferrix_partition::Guid, u64, u64)]) -> Result<gpt::Layout, GptError> {
    let (header, array) = table(parts);
    let header = gpt::parse_header(&header, DISK_BLOCKS, 512)?;
    gpt::find(&header, &array, DISK_BLOCKS, 512)
}

#[test]
fn the_gpt_scan_finds_the_layout_by_type() {
    let swap =
        ferrix_partition::Guid::from_fields(0x0657_FD6D, 0xA4AB, 0x43C4, 0x84E5, 0x0933_C84B_4F4F);
    let slot = ferrix_partition::Guid(SLOT_TYPE);
    let control = ferrix_partition::Guid(CONTROL_TYPE);
    let layout = find(&[
        (swap, 2048, 4095),
        (ferrix_partition::ESP, 4096, 8191),
        (control, 8192, 8192 + 2047),
        (slot, 12288, 20479),
        (slot, 20480, 28671),
        (ferrix_partition::LINUX_FILESYSTEM, 28672, 60000),
    ]);
    let Ok(layout) = layout else {
        panic!("{layout:?}");
    };
    assert_eq!((layout.control.index, layout.control.first_lba), (2, 8192));
    assert_eq!((layout.a.index, layout.a.first_lba), (3, 12288));
    assert_eq!((layout.b.index, layout.b.first_lba), (4, 20480));
    assert_eq!(layout.slot(Slot::B), layout.b);

    assert_eq!(
        find(&[(slot, 2048, 9000), (slot, 9001, 20000)]),
        Err(GptError::NoControl)
    );
    assert_eq!(
        find(&[(control, 2048, 4095), (slot, 4096, 9000)]),
        Err(GptError::SlotCount)
    );
    assert_eq!(
        find(&[
            (control, 2048, 4095),
            (slot, 4096, 9000),
            (slot, 9001, 20000),
            (slot, 20001, 30000)
        ]),
        Err(GptError::SlotCount)
    );
    assert_eq!(
        find(&[
            (control, 2048, 4095),
            (control, 4096, 8191),
            (slot, 9001, 20000)
        ]),
        Err(GptError::ManyControls)
    );
    assert_eq!(
        find(&[
            (control, 2048, 2100),
            (slot, 4096, 9000),
            (slot, 9001, 20000)
        ]),
        Err(GptError::TooSmall)
    );
}

#[test]
fn the_gpt_scan_trusts_nothing() {
    let slot = ferrix_partition::Guid(SLOT_TYPE);
    let control = ferrix_partition::Guid(CONTROL_TYPE);
    let (header, array) = table(&[
        (control, 2048, 4095),
        (slot, 4096, 9000),
        (slot, 9001, 20000),
    ]);
    let mut bad = header.clone();
    bad[0] = b'X';
    assert_eq!(
        gpt::parse_header(&bad, DISK_BLOCKS, 512),
        Err(GptError::Header)
    );
    let mut bad = header.clone();
    bad[40] ^= 1;
    assert_eq!(
        gpt::parse_header(&bad, DISK_BLOCKS, 512),
        Err(GptError::Header)
    );
    // A disk smaller than the table says.
    assert_eq!(gpt::parse_header(&header, 1000, 512), Err(GptError::Header));
    let Ok(parsed) = gpt::parse_header(&header, DISK_BLOCKS, 512) else {
        panic!("header");
    };
    let mut bad = array.clone();
    bad[128 + 40] ^= 1;
    assert_eq!(
        gpt::find(&parsed, &bad, DISK_BLOCKS, 512),
        Err(GptError::Entries)
    );
    // A slot past the end of the disk the firmware reports.
    assert_eq!(gpt::find(&parsed, &array, 15000, 512), Err(GptError::Range));
}

/// A table edited after `ferrix-partition` wrote it (which refuses such
/// tables itself), with both CRCs made right again, so only the edit is
/// wrong.
fn edited(
    parts: &[(ferrix_partition::Guid, u64, u64)],
    edit: &dyn Fn(&mut Vec<u8>, &mut Vec<u8>),
) -> Result<gpt::Layout, GptError> {
    let (mut header, mut array) = table(parts);
    edit(&mut header, &mut array);
    let count = u32::from_le_bytes(header[80..84].try_into().unwrap_or_default()) as usize;
    let size = u32::from_le_bytes(header[84..88].try_into().unwrap_or_default()) as usize;
    let crc = crc32(&array[..(count * size).min(array.len())]);
    header[88..92].copy_from_slice(&crc.to_le_bytes());
    header[16..20].fill(0);
    let crc = crc32(&header[..92]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
    let header = gpt::parse_header(&header, DISK_BLOCKS, 512)?;
    gpt::find(&header, &array, DISK_BLOCKS, 512)
}

fn set_lba(array: &mut [u8], entry: usize, field: usize, value: u64) {
    let at = entry * 128 + field;
    array[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn the_gpt_scan_refuses_hostile_tables() {
    let slot = ferrix_partition::Guid(SLOT_TYPE);
    let control = ferrix_partition::Guid(CONTROL_TYPE);
    let swap = ferrix_partition::LINUX_FILESYSTEM;
    let parts = [
        (swap, 2048, 4095),
        (control, 4096, 8191),
        (slot, 8192, 12287),
        (slot, 12288, 16383),
    ];
    assert!(edited(&parts, &|_, _| {}).is_ok());
    // The control partition reaching into the swap before it.
    assert_eq!(
        edited(&parts, &|_, array| set_lba(array, 1, 32, 4000)),
        Err(GptError::Overlap)
    );
    // The swap reaching into the control partition after it.
    assert_eq!(
        edited(&parts, &|_, array| set_lba(array, 0, 40, 4096)),
        Err(GptError::Overlap)
    );
    // Slot B over slot A.
    assert_eq!(
        edited(&parts, &|_, array| set_lba(array, 3, 32, 12000)),
        Err(GptError::Overlap)
    );
    // Backwards, on a Ferrix entry and on another one.
    assert_eq!(
        edited(&parts, &|_, array| set_lba(array, 2, 40, 8000)),
        Err(GptError::Range)
    );
    assert_eq!(
        edited(&parts, &|_, array| set_lba(array, 0, 40, 100)),
        Err(GptError::Range)
    );
    // Entry size and count outside their bounds.
    for (size, count) in [
        (64u32, 128u32),
        (96, 128),
        (2048, 16),
        (384, 16),
        (128, 0),
        (128, 513),
    ] {
        assert_eq!(
            edited(&parts, &|header, _| {
                header[80..84].copy_from_slice(&count.to_le_bytes());
                header[84..88].copy_from_slice(&size.to_le_bytes());
            }),
            Err(GptError::Header),
            "size {size} count {count}"
        );
    }
}

/// A disk of `blocks` blocks in memory; `drop_writes` acknowledges writes
/// and keeps nothing, as a device that lies would.
struct MemDisk {
    size: u32,
    bytes: Vec<u8>,
    drop_writes: bool,
    writes: Vec<u64>,
}

impl MemDisk {
    fn new(size: u32, blocks: u64) -> MemDisk {
        MemDisk {
            size,
            bytes: vec![0; (u64::from(size) * blocks) as usize],
            drop_writes: false,
            writes: Vec::new(),
        }
    }
    fn range(&self, lba: u64) -> core::ops::Range<usize> {
        let at = (lba * u64::from(self.size)) as usize;
        at..at + self.size as usize
    }
}

impl crate::record::ControlDisk for MemDisk {
    fn block_size(&self) -> u32 {
        self.size
    }
    fn write_block(&mut self, lba: u64, bytes: &[u8]) -> Result<(), crate::record::DeviceError> {
        self.writes.push(lba);
        if !self.drop_writes {
            let range = self.range(lba);
            self.bytes
                .get_mut(range)
                .ok_or(crate::record::DeviceError)?
                .copy_from_slice(bytes);
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), crate::record::DeviceError> {
        Ok(())
    }
    fn read_block(&mut self, lba: u64, bytes: &mut [u8]) -> Result<(), crate::record::DeviceError> {
        let range = self.range(lba);
        bytes.copy_from_slice(self.bytes.get(range).ok_or(crate::record::DeviceError)?);
        Ok(())
    }
}

#[test]
fn a_record_write_counts_only_when_read_back() {
    use crate::record::{WriteError, read_current, write_record};
    for size in [512u32, 4096] {
        let blocks_per_mib = (1 << 20) / u64::from(size);
        let control = gpt::Part {
            index: 2,
            unique: [0; 16],
            first_lba: 10,
            last_lba: 10 + blocks_per_mib - 1,
        };
        let mut disk = MemDisk::new(size, 20 + blocks_per_mib);
        assert_eq!(
            read_current(&mut disk, &control),
            Ok(Current::Uninitialised)
        );
        let first = Record::initial(Slot::A);
        assert_eq!(
            write_record(&mut disk, &control, &Current::Uninitialised, &first),
            Ok(())
        );
        let now = read_current(&mut disk, &control);
        assert_eq!(
            now,
            Ok(Current::Valid {
                copy: 0,
                record: first
            })
        );
        let Ok(now) = now else { panic!() };
        let Some(armed) = first.armed(Slot::B, 1) else {
            panic!()
        };
        assert_eq!(write_record(&mut disk, &control, &now, &armed), Ok(()));
        let Ok(now) = read_current(&mut disk, &control) else {
            panic!()
        };
        assert_eq!(
            now,
            Current::Valid {
                copy: 1,
                record: armed
            }
        );
        // Every write landed inside the control partition, at a copy.
        let copy1 = 10 + 65536 / u64::from(size);
        assert_eq!(disk.writes, [10, copy1]);
        // A stale or inconsistent record is refused before any write.
        assert_eq!(
            write_record(&mut disk, &control, &now, &first),
            Err(WriteError::Record)
        );
        // A device that drops writes: the read-back says so (the no_readback
        // control removes that comparison and this assertion fails).
        disk.drop_writes = true;
        let Some(tried) = armed.after_try() else {
            panic!()
        };
        assert_eq!(
            write_record(&mut disk, &control, &now, &tried),
            Err(WriteError::ReadBack)
        );
        assert_eq!(read_current(&mut disk, &control), Ok(now));
    }
    // Block sizes a record cannot be written in.
    let control = gpt::Part {
        index: 0,
        unique: [0; 16],
        first_lba: 1,
        last_lba: 4000,
    };
    let mut disk = MemDisk::new(8192, 5000);
    assert_eq!(
        write_record(
            &mut disk,
            &control,
            &Current::Uninitialised,
            &Record::initial(Slot::A)
        ),
        Err(WriteError::Range)
    );
    assert!(disk.writes.is_empty());
}

#[test]
fn a_slot_command_line_is_composed_whole_or_not_at_all() {
    use crate::plan::compose;
    let words = LoaderWords {
        slot: Slot::A,
        kind: Kind::Good,
        sequence: 3,
        disk: DISK,
    };
    let full: &[u8] = b"ferrix.slot=a ferrix.slot.boot=good ferrix.slot.seq=3 \
        ferrix.slot.disk=12345678-9ABC-DEF0-1122-334455667788 ferrix.root=tmpfs";
    let mut out = [0u8; 200];
    let len = compose(&words, b"  ferrix.root=tmpfs\n", &mut out);
    assert_eq!(len.map(|len| &out[..len]), Some(full));
    assert_eq!(compose(&words, b"", &mut out), Some(full.len() - 18));
    // Exactly enough room, and one byte short: refused, never cut.
    let mut exact = vec![0u8; full.len()];
    assert_eq!(
        compose(&words, b"ferrix.root=tmpfs", &mut exact),
        Some(full.len())
    );
    let mut short = vec![0u8; full.len() - 1];
    assert_eq!(compose(&words, b"ferrix.root=tmpfs", &mut short), None);
    assert_eq!(compose(&words, &[0xFF, 0xFE], &mut out), None);
}

#[test]
fn the_longest_slot_command_line_fits_the_boot_info() {
    use crate::plan::compose;
    use crate::slot::MAX_CMDLINE;
    // The loader's command-line area: the page of the boot info after the
    // `BootInfo` (src/boot/common/uefi/src/main.rs, CMDLINE_OFFSET to
    // REGIONS_OFFSET).
    let capacity = ferrix_bootinfo::PAGE_SIZE as usize - size_of::<ferrix_bootinfo::BootInfo>();
    let words = LoaderWords {
        slot: Slot::B,
        kind: Kind::Fallback,
        sequence: u64::MAX,
        disk: [0xFF; 16],
    };
    let line = vec![b'x'; MAX_CMDLINE as usize];
    let mut out = vec![0u8; capacity];
    assert!(
        compose(&words, &line, &mut out).is_some(),
        "capacity {capacity}"
    );
}

#[test]
fn a_ferrix_partition_outside_the_usable_blocks_is_refused() {
    let slot = ferrix_partition::Guid(SLOT_TYPE);
    let control = ferrix_partition::Guid(CONTROL_TYPE);
    let parts = [
        (control, 4096, 8191),
        (slot, 8192, 12287),
        (slot, 12288, 16383),
    ];
    // The control partition moved onto the primary header at LBA 1.
    assert_eq!(
        edited(&parts, &|_, array| set_lba(array, 0, 32, 1)),
        Err(GptError::Range)
    );
    // A slot running into the backup table past the last usable block.
    assert_eq!(
        edited(&parts, &|_, array| set_lba(array, 2, 40, DISK_BLOCKS - 2)),
        Err(GptError::Range)
    );
    // The disk GUID is carried to the layout.
    let Ok(layout) = edited(&parts, &|_, _| {}) else {
        panic!("layout");
    };
    assert_eq!(gpt::GuidText(layout.disk).to_string().len(), 36);
    assert_eq!(
        gpt::GuidText(DISK).to_string(),
        "12345678-9ABC-DEF0-1122-334455667788"
    );
}

#[test]
fn the_loader_words_parse_back_first_match() {
    use crate::plan::parse_words;
    for slot in Slot::BOTH {
        for kind in [Kind::Trial, Kind::Good, Kind::Fallback] {
            for sequence in [0, 3, u64::MAX] {
                let words = LoaderWords {
                    slot,
                    kind,
                    sequence,
                    disk: DISK,
                };
                let line = std::format!("{words} ferrix.root=tmpfs");
                assert_eq!(parse_words(&line), Some(words), "{line}");
            }
        }
    }
    let words = LoaderWords {
        slot: Slot::A,
        kind: Kind::Good,
        sequence: 3,
        disk: DISK,
    };
    // A slot's own line cannot override the loader's words, which come first.
    let forged = std::format!("{words} ferrix.slot=b ferrix.slot.boot=trial ferrix.slot.seq=9");
    assert_eq!(parse_words(&forged), Some(words));
    // Lower case is read too; a word missing or malformed is no words at all.
    let lower = std::format!("{words}").to_lowercase();
    assert_eq!(parse_words(&lower), Some(words));
    assert_eq!(
        parse_words("ferrix.slot=a ferrix.slot.boot=good ferrix.slot.seq=3"),
        None
    );
    assert_eq!(
        parse_words(
            "ferrix.slot=c ferrix.slot.boot=good ferrix.slot.seq=3 \
             ferrix.slot.disk=12345678-9ABC-DEF0-1122-334455667788"
        ),
        None
    );
    assert_eq!(gpt::parse_guid("12345678-9ABC-DEF0-1122-33445566778"), None);
    assert_eq!(
        gpt::parse_guid("12345678x9ABC-DEF0-1122-334455667788"),
        None
    );
    assert_eq!(
        gpt::parse_guid("12345678-9ABC-DEF0-1122-3344556677GG"),
        None
    );
    let text = gpt::GuidText(SLOT_TYPE).to_string();
    assert_eq!(text, "5475B681-82F5-47DA-A5A1-55DB0E7E2BD3");
    assert_eq!(gpt::parse_guid(&text), Some(SLOT_TYPE));
    assert_eq!(text, ferrix_partition::Guid(SLOT_TYPE).to_string());
}
