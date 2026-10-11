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
    let mut control = scratch("control", 1 << 20);
    let first = record::read(&mut control, 0).unwrap();
    assert_eq!(first.record(), None);
    let init = record::write(&mut control, 0, &first, Record::initial(Slot::A), 512).unwrap();
    let read = record::read(&mut control, 0).unwrap();
    assert_eq!(read.record(), Some(init));
    let armed =
        record::write(&mut control, 0, &read, init.armed(Slot::B, 1).unwrap(), 512).unwrap();
    let read = record::read(&mut control, 0).unwrap();
    assert_eq!(read.record(), Some(armed));
    assert!(read.valid(0) && read.valid(1));
    // Tear the newest copy (copy 1): the record is the one before it.
    disk::write_at(&mut control, COPY_OFFSETS[1] + 20, &[0xAA]).unwrap();
    assert_eq!(record::read(&mut control, 0).unwrap().record(), Some(init));
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
