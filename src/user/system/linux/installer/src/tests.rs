//! The record's and the slot's file I/O against files standing in for
//! partitions.

use std::fs::{File, OpenOptions};

use ferrix_slots::Slot;
use ferrix_slots::record::{COPY_OFFSETS, Record};

use crate::{disk, record, slotio};

fn scratch(name: &str, bytes: u64) -> File {
    let path = std::env::temp_dir().join(format!("ab-sys-{}-{name}", std::process::id()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    file.set_len(bytes).unwrap();
    file
}

#[test]
fn records_alternate_copies_and_a_torn_newest_falls_back() {
    let mut file = scratch("control", 2 << 20);
    // A control partition from sector 2048 to the end.
    let part = ferrix_slots::gpt::Part {
        index: 2,
        unique: [0; 16],
        first_lba: 2048,
        last_lba: 4095,
    };
    let mut blocks = record::Blocks {
        file: &mut file,
        block: 512,
    };
    let first = record::read(&mut blocks, &part).unwrap();
    assert_eq!(record::current(&first), None);
    let init = record::write(&mut blocks, &part, &first, Some(Record::initial(Slot::A))).unwrap();
    let read = record::read(&mut blocks, &part).unwrap();
    assert_eq!(record::current(&read), Some(init));
    let armed = record::write(&mut blocks, &part, &read, init.armed(Slot::B, 1)).unwrap();
    assert_eq!(
        record::current(&record::read(&mut blocks, &part).unwrap()),
        Some(armed)
    );
    assert_eq!(record::copies_valid(&mut blocks, &part), [true, true]);
    // A write that goes nowhere: a sequence not above the current one.
    let current = record::read(&mut blocks, &part).unwrap();
    assert!(record::write(&mut blocks, &part, &current, Some(init)).is_err());
    // Tear the newest copy (copy 1): the record is the one before it.
    disk::write_at(&mut file, 2048 * 512 + COPY_OFFSETS[1] + 20, &[0xAA]).unwrap();
    let mut blocks = record::Blocks {
        file: &mut file,
        block: 512,
    };
    assert_eq!(
        record::current(&record::read(&mut blocks, &part).unwrap()),
        Some(init)
    );
}

fn image() -> slotio::Image {
    slotio::Image {
        kernel: vec![0x7F; 70_000],
        initrd: (0..300_000_u32).map(|i| i as u8).collect(),
        cmdline: b"ferrix.root=btrfs".to_vec(),
        from: "a test".into(),
    }
}

#[test]
fn a_written_slot_verifies_and_a_flipped_byte_does_not() {
    let bytes = 4 << 20;
    let mut slot = scratch("slot", bytes);
    assert!(
        slotio::verify(&mut slot, 0, bytes).is_err(),
        "an empty slot verifies"
    );
    let written = slotio::write(&mut slot, 0, bytes, &image(), 3, "ferrix test").unwrap();
    assert_eq!(slotio::verify(&mut slot, 0, bytes), Ok(written));
    let at = written.initrd.offset + 1234;
    disk::write_at(&mut slot, at, &[0x55]).unwrap();
    let why = slotio::verify(&mut slot, 0, bytes).unwrap_err();
    assert!(why.contains("initramfs"), "{why}");
}

#[test]
fn an_image_larger_than_the_slot_is_refused() {
    let bytes = 256 << 10;
    let mut slot = scratch("small", bytes);
    assert!(slotio::write(&mut slot, 0, bytes, &image(), 1, "x").is_err());
    assert!(slotio::verify(&mut slot, 0, bytes).is_err());
}
