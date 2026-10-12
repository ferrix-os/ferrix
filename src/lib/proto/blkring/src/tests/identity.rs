//! Disk names, their indices and minors, against Linux's own naming.

use std::string::String;
use std::vec::Vec;

use crate::identity::{
    DiskName, Location, MAX_DISK_INDEX, MAX_NVME_CONTROLLER, NAME_BYTES, NVME_MAJOR,
    VIRTIO_BLK_MAJOR, disk_index, nvme_controller, whole_disk_minor,
};

/// Linux's `virtblk_name_format` for the `vd` prefix, transcribed: letters
/// from the least significant, `index = index / 26 - 1` until it goes
/// negative.
fn virtblk_name_format(index: u32) -> String {
    let mut index = i64::from(index);
    let mut letters = Vec::new();
    loop {
        letters.push(b'a' + (index % 26) as u8);
        index = index / 26 - 1;
        if index < 0 {
            break;
        }
    }
    letters.reverse();
    let mut name = String::from("vd");
    name.push_str(core::str::from_utf8(&letters).expect("ASCII"));
    name
}

/// A name as HELLO carries it, or `None` if it does not fit.
fn padded(name: &str) -> Option<[u8; NAME_BYTES]> {
    let mut bytes = [0; NAME_BYTES];
    bytes
        .get_mut(..name.len())?
        .copy_from_slice(name.as_bytes());
    Some(bytes)
}

#[test]
fn indices_follow_linux_at_every_boundary() {
    let cases = [
        ("vda", 0),
        ("vdb", 1),
        ("vdz", 25),
        ("vdaa", 26),
        ("vdaz", 51),
        ("vdba", 52),
        ("vdzz", 701),
        ("vdaaa", 702),
        ("vdzzz", 18277),
    ];
    for (name, index) in cases {
        assert_eq!(
            virtblk_name_format(index),
            name,
            "Linux names {index} {name}"
        );
        let bytes = padded(name).expect("fits");
        assert_eq!(disk_index(&bytes), Some(index), "{name}");
        let disk = DiskName::for_index(index).expect("in range");
        assert_eq!(disk.as_str(), name, "for_index({index})");
        assert_eq!(disk.as_bytes(), &bytes, "{name} padded");
        assert_eq!(disk.minor(), index * 16, "{name} minor");
        assert_eq!(
            whole_disk_minor(index),
            Some(index * 16),
            "{name} whole disk"
        );
    }
    assert_eq!(MAX_DISK_INDEX, 18277, "vdzzz");
}

#[test]
fn every_name_linux_would_write_maps_back_to_its_index() {
    let step = if cfg!(miri) { 97 } else { 1 };
    for index in (0..=MAX_DISK_INDEX).step_by(step) {
        let name = virtblk_name_format(index);
        let bytes = padded(&name).expect("fits in eight bytes");
        assert_eq!(disk_index(&bytes), Some(index), "{name}");
        assert_eq!(
            DiskName::for_index(index).map(|disk| disk.index()),
            Some(index),
            "{name}"
        );
    }
}

#[test]
fn past_vdzzz_there_is_no_name() {
    let next = virtblk_name_format(MAX_DISK_INDEX + 1);
    assert_eq!(next, "vdaaaa", "Linux's next name");
    let bytes = padded(&next).expect("six bytes still fit");
    assert_eq!(disk_index(&bytes), None, "four letters are refused");
    assert_eq!(DiskName::for_index(MAX_DISK_INDEX + 1), None, "no name");
    assert_eq!(whole_disk_minor(MAX_DISK_INDEX + 1), None, "no minor");
    assert_eq!(DiskName::for_index(u32::MAX), None, "no name");
}

#[test]
fn a_disk_name_is_checked_when_made() {
    assert!(DiskName::new(*b"vdq\0\0\0\0\0").is_some(), "vdq");
    assert!(
        DiskName::new(*b"vdq\0\0\0\0\x01").is_none(),
        "a byte after the padding"
    );
    assert!(
        DiskName::new(*b"vdq\0q\0\0\0").is_none(),
        "a letter after the padding"
    );
    let disk = DiskName::new(*b"vdbc\0\0\0\0").expect("valid");
    assert_eq!(std::format!("{disk}"), "vdbc", "display");
    assert_eq!(disk.index(), 2 * 26 + 3 - 1, "b = 2, c = 3");
}

#[test]
fn a_location_packs_segment_bus_and_devfn() {
    let location = Location::new(0x1234, 0x56, (0x1F << 3) | 5);
    assert_eq!(
        location.0, 0x1234_56FD,
        "segment 31:16, bus 15:8, devfn 7:0"
    );
    assert_eq!(
        (location.segment(), location.bus(), location.devfn()),
        (0x1234, 0x56, 0xFD),
        "fields"
    );
    assert_eq!((location.device(), location.function()), (0x1F, 5), "devfn");
    assert_eq!(std::format!("{location}"), "1234:56:1f.5", "display");
}

#[test]
fn an_nvme_namespace_is_named_as_linux_names_it() {
    for (name, controller) in [
        ("nvme0n1", 0),
        ("nvme1n1", 1),
        ("nvme9n1", 9),
        ("nvme10n1", 10),
        ("nvme99n1", 99),
    ] {
        let bytes = padded(name).expect("fits");
        assert_eq!(nvme_controller(&bytes), Some(controller), "{name}");
        assert_eq!(disk_index(&bytes), None, "{name} is no virtio name");
        let disk = DiskName::new(bytes).expect("valid");
        assert_eq!(DiskName::nvme(controller), Some(disk), "nvme({controller})");
        assert_eq!(disk.as_str(), name, "{name}");
        assert!(disk.is_nvme(), "{name}");
        assert_eq!(disk.index(), controller, "{name} index");
        assert_eq!(disk.minor(), controller * 16, "{name} minor");
        assert_eq!(disk.major(), NVME_MAJOR, "{name} major");
    }
    assert_eq!(
        DiskName::nvme(MAX_NVME_CONTROLLER + 1),
        None,
        "nvme100n1 does not fit"
    );
    let virtio = DiskName::for_index(3).expect("vdd");
    assert!(!virtio.is_nvme(), "vdd");
    assert_eq!(virtio.major(), VIRTIO_BLK_MAJOR, "vdd major");
}

#[test]
fn a_malformed_nvme_name_is_refused() {
    for name in [
        "nvme", "nvmen1", "nvme01n1", "nvme0n2", "nvme0n", "nvme0", "nvme0n10", "nvme0p1",
        "nvmeAn1", "Nvme0n1", "nvme0n1p", "vme0n1",
    ] {
        let bytes = padded(name).expect("fits");
        assert_eq!(nvme_controller(&bytes), None, "{name}");
        assert!(DiskName::new(bytes).is_none(), "{name}");
    }
    assert!(
        DiskName::new(*b"nvme0n1\x01").is_none(),
        "a byte after the padding"
    );
    assert!(DiskName::new(*b"nvme100n").is_none(), "three digits");
}
