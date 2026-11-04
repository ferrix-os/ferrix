use super::*;

extern crate std;
use std::vec;
use std::vec::Vec;

fn file(partitions: u16) -> Vec<u8> {
    let mut bytes = vec![0_u8; HEADER_BYTES + usize::from(partitions) * PARTITION_BYTES];
    bytes[..4].copy_from_slice(&MAGIC);
    bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
    bytes[6..8].copy_from_slice(&partitions.to_le_bytes());
    for index in 0..usize::from(partitions) {
        let at = HEADER_BYTES + index * PARTITION_BYTES;
        bytes[at..at + 5].copy_from_slice(b"hello");
    }
    bytes
}

#[test]
fn a_well_formed_file_reads_back() {
    let mut bytes = file(1);
    let at = HEADER_BYTES;
    bytes[at + 32] = 0xAB;
    bytes[at + 64..at + 70].copy_from_slice(b"sbin/x");
    bytes[at + 128] = 7;
    bytes[at + 152] = 2;
    bytes[at + 156] = 1;
    bytes[at + 168] = 1;
    bytes[at + 176..at + 180].copy_from_slice(&0x0000_0310_u32.to_le_bytes());
    let description = Description::parse(&bytes).expect("parses");
    assert_eq!(description.count(), 1);
    let partition = description.partition(0).expect("there");
    assert_eq!(partition.name(), b"hello");
    assert_eq!(partition.digest()[0], 0xAB);
    assert_eq!(partition.image(), b"sbin/x");
    assert_eq!(partition.reserve_pages(), 7);
    assert_eq!(partition.restarts(), 2);
    let device = partition.devices().next().expect("one");
    assert_eq!(device.kind(), KIND_PCI);
    assert_eq!(device.bdf(), 0x310);
    assert!(description.partition(1).is_none());
}

#[test]
fn an_empty_description_is_one() {
    assert_eq!(Description::parse(&file(0)).expect("parses").count(), 0);
}

#[test]
fn what_is_wrong_is_refused() {
    assert_eq!(Description::parse(&[]).err(), Some(Malformed::Length));
    let mut bad = file(1);
    bad[0] = b'x';
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Magic));
    let mut bad = file(1);
    bad[4] = 9;
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Version));
    let mut bad = file(1);
    bad[6..8].copy_from_slice(&17_u16.to_le_bytes());
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::TooMany));
    let mut bad = file(1);
    bad[6..8].copy_from_slice(&2_u16.to_le_bytes());
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Length));
    let mut bad = file(1);
    bad.truncate(bad.len() - 1);
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Length));
    let mut bad = file(1);
    bad[12] = 200;
    assert_eq!(
        Description::parse(&bad)
            .map(|good| good.ready_millis())
            .ok(),
        Some(200)
    );
    let mut bad = file(1);
    bad[HEADER_BYTES + 156] = 9;
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Devices));
    let mut bad = file(1);
    bad[HEADER_BYTES + 156] = 1;
    bad[HEADER_BYTES + 168] = 3;
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Device));
    let mut bad = file(1);
    bad[HEADER_BYTES + 156] = 1;
    bad[HEADER_BYTES + 168] = 1;
    bad[HEADER_BYTES + 172] = 2;
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Device));
    let mut bad = file(1);
    bad[HEADER_BYTES + 164] = 2;
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Reserved));
}

#[test]
fn a_file_server_stands_beside_no_safety_device() {
    let mut bytes = file(2);
    bytes[HEADER_BYTES + 160] = 1;
    assert!(Description::parse(&bytes).is_ok());
    let second = HEADER_BYTES + PARTITION_BYTES;
    bytes[second + 156] = 1;
    bytes[second + 168] = 1;
    assert_eq!(Description::parse(&bytes).err(), Some(Malformed::Role));
}

#[test]
fn what_the_first_message_cannot_hold_is_refused_here() {
    let count = 16_usize;
    let mut bytes = file(16);
    for index in 0..count {
        let at = HEADER_BYTES + index * PARTITION_BYTES;
        bytes[at] = b'a' + u8::try_from(index).expect("few");
        bytes[at + 156] = u8::try_from(MAX_DEVICES).expect("few");
        for slot in 0..MAX_DEVICES {
            let device = at + 168 + slot * DEVICE_BYTES;
            bytes[device] = 1;
            bytes[device + 8] = u8::try_from(index * MAX_DEVICES + slot).expect("few");
        }
    }
    assert_eq!(Description::parse(&bytes).err(), Some(Malformed::TooMany));
}

#[test]
fn text_must_end_in_padding() {
    let mut bad = file(1);
    bad[HEADER_BYTES + 6] = b'y';
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Text));
    let mut bad = file(1);
    for byte in &mut bad[HEADER_BYTES..HEADER_BYTES + 32] {
        *byte = b'n';
    }
    assert_eq!(Description::parse(&bad).err(), Some(Malformed::Text));
}

#[test]
fn every_length_short_of_a_whole_file_is_refused_without_a_panic() {
    let whole = file(2);
    for length in 0..whole.len() {
        assert!(Description::parse(&whole[..length]).is_err(), "{length}");
    }
}
