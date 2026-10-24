//! The IVRS parser, against nazuna's table as its firmware wrote it, QEMU
//! 10.2's as `build_amd_iommu` writes it, and every way a table can fail
//! to be read (`docs/AMD-VI.md` §1.1, §5.2).

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;
use crate::Table;

/// nazuna's IVRS (AMD Ryzen, Granite Ridge, on a B650 board), as its Linux
/// read it from `/sys/firmware/acpi/tables/IVRS` on 2026-10-24 -- copied
/// by the customer at nazuna's screen with one `pkexec cp`, the request
/// relayed by the PO, the file being root-only: 200 bytes,
/// SHA-256 9fb713bc9d2ed402f15c388a5a70d213be4b637350c5b87ebe04976c09720e44.
/// One unit, the function 00:00.2, at 0xF7500000, described twice -- an
/// IVHD 10h and an 11h -- with the same entries: devids 0x0003-0xFFFE, an
/// alias range 0xFF00-0xFFFF arriving as 00:14.5, the HPET at 00:14.0, and
/// I/O APICs 0x20 (00:14.0) and 0x21 (00:00.1). No IVMD.
const NAZUNA: [u8; 200] = [
    0x49, 0x56, 0x52, 0x53, 0xc8, 0x00, 0x00, 0x00, 0x02, 0x6d, 0x41, 0x4d, 0x44, 0x20, 0x20, 0x00,
    0x41, 0x6d, 0x64, 0x54, 0x61, 0x62, 0x6c, 0x65, 0x01, 0x00, 0x00, 0x00, 0x41, 0x4d, 0x44, 0x20,
    0x01, 0x00, 0x00, 0x00, 0x41, 0x30, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x10, 0xb0, 0x44, 0x00, 0x02, 0x00, 0x40, 0x00, 0x00, 0x00, 0x50, 0xf7, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x6e, 0x8f, 0x04, 0x80, 0x03, 0x03, 0x00, 0x00, 0x04, 0xfe, 0xff, 0x00,
    0x43, 0x00, 0xff, 0x00, 0x00, 0xa5, 0x00, 0x00, 0x04, 0xff, 0xff, 0x00, 0x48, 0x00, 0x00, 0x00,
    0x00, 0xa0, 0x00, 0x02, 0x48, 0x00, 0x00, 0xd7, 0x20, 0xa0, 0x00, 0x01, 0x48, 0x00, 0x00, 0x00,
    0x21, 0x01, 0x00, 0x01, 0x11, 0x30, 0x54, 0x00, 0x02, 0x00, 0x40, 0x00, 0x00, 0x00, 0x50, 0xf7,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x04, 0x00, 0xfa, 0x4a, 0x25, 0xa2,
    0xef, 0x77, 0x65, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x03, 0x00, 0x00,
    0x04, 0xfe, 0xff, 0x00, 0x43, 0x00, 0xff, 0x00, 0x00, 0xa5, 0x00, 0x00, 0x04, 0xff, 0xff, 0x00,
    0x48, 0x00, 0x00, 0x00, 0x00, 0xa0, 0x00, 0x02, 0x48, 0x00, 0x00, 0xd7, 0x20, 0xa0, 0x00, 0x01,
    0x48, 0x00, 0x00, 0x00, 0x21, 0x01, 0x00, 0x01,
];

/// The devid of `bus:device.function`.
const fn devid(bus: u8, device: u8, function: u8) -> u16 {
    (bus as u16) << 8 | (device as u16) << 3 | function as u16
}

/// A table of `body` after the IVRS header, its length and checksum
/// filled in.
fn table(info: u32, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; IVRS_HEADER_LEN];
    bytes[0..4].copy_from_slice(&IVRS_SIGNATURE);
    bytes[8] = 2;
    bytes[36..40].copy_from_slice(&info.to_le_bytes());
    bytes.extend_from_slice(body);
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    let sum = bytes.iter().copied().fold(0u8, u8::wrapping_add);
    bytes[9] = 0u8.wrapping_sub(sum);
    bytes
}

/// An IVHD of `kind` for the unit at `base` whose function is `unit`, with
/// `entries`, as `build_amd_iommu` lays it out.
fn ivhd(kind: u8, unit: u16, base: u64, attributes: u32, efr: u64, entries: &[u8]) -> Vec<u8> {
    let header = if kind == IVHD_10 { 24 } else { 40 };
    let mut block = vec![0u8; header];
    block[0] = kind;
    block[1] = 0x11;
    block[2..4].copy_from_slice(&((header + entries.len()) as u16).to_le_bytes());
    block[4..6].copy_from_slice(&unit.to_le_bytes());
    block[6..8].copy_from_slice(&0x40u16.to_le_bytes());
    block[8..16].copy_from_slice(&base.to_le_bytes());
    if kind == IVHD_10 {
        block[20..24].copy_from_slice(&((48u32 << 30) | (1 << 2) | (1 << 6)).to_le_bytes());
    } else {
        block[20..24].copy_from_slice(&attributes.to_le_bytes());
        block[24..32].copy_from_slice(&efr.to_le_bytes());
    }
    block.extend_from_slice(entries);
    block
}

/// An IVMD of `kind`.
fn ivmd(kind: u8, flags: u8, devid: u16, aux: u16, start: u64, length: u64) -> Vec<u8> {
    let mut block = vec![0u8; 32];
    block[0] = kind;
    block[1] = flags;
    block[2..4].copy_from_slice(&32u16.to_le_bytes());
    block[4..6].copy_from_slice(&devid.to_le_bytes());
    block[6..8].copy_from_slice(&aux.to_le_bytes());
    block[16..24].copy_from_slice(&start.to_le_bytes());
    block[24..32].copy_from_slice(&length.to_le_bytes());
    block
}

/// A four-byte entry.
fn short(kind: u8, devid: u16) -> [u8; 4] {
    let [low, high] = devid.to_le_bytes();
    [kind, low, high, 0]
}

/// An eight-byte entry, with `ext`'s bytes 4 to 7.
fn long(kind: u8, devid: u16, ext: [u8; 4]) -> [u8; 8] {
    let [low, high] = devid.to_le_bytes();
    [kind, low, high, 0, ext[0], ext[1], ext[2], ext[3]]
}

/// QEMU 10.2's entries on the AMD-Vi row: a select for each function on
/// bus 0 (00:00.0 to 00:05.0, the `amd-iommu` function among them), and the
/// I/O APIC special entry, handle 0, devid 00:14.0.
fn qemu_entries() -> Vec<u8> {
    let mut entries = Vec::new();
    for device in 0..6 {
        entries.extend_from_slice(&short(0x02, devid(0, device, 0)));
    }
    let [low, high] = devid(0, 0x14, 0).to_le_bytes();
    entries.extend_from_slice(&long(0x48, 0, [0, low, high, SPECIAL_IOAPIC]));
    entries
}

/// QEMU 10.2's IVRS for `-device amd-iommu,intremap=on` (`build_amd_iommu`):
/// `IVinfo` `EFRSup` and a 40-bit `PASize`, then a 10h and an 11h for the unit
/// at 0xFED80000, function 00:03.0, with the same entries.
fn qemu(attributes: u32, efr: u64) -> Vec<u8> {
    let entries = qemu_entries();
    let unit = devid(0, 3, 0);
    let mut body = ivhd(IVHD_10, unit, 0xFED8_0000, 0, 0, &entries);
    body.extend(ivhd(IVHD_11, unit, 0xFED8_0000, attributes, efr, &entries));
    table(1 | 40 << 8, &body)
}

fn parse(bytes: &[u8]) -> Ivrs<'_> {
    Ivrs::parse(Table::parse(bytes).unwrap()).unwrap()
}

/// Verifies: L.iommu.79, L.iommu.81
#[test]
fn nazunas_ivrs_is_one_unit_read_at_type_11h() {
    let bytes = NAZUNA;
    let table = Table::parse(&bytes).unwrap();
    assert!(table.checksum_valid(), "as firmware wrote it");
    let ivrs = parse(&bytes);
    assert_eq!(ivrs.info(), 0x0020_3041);
    assert_eq!(ivrs.chosen_type(), Some(IVHD_11), "the 11h over the 10h");
    assert!(ivrs.whole());
    let units: Vec<_> = ivrs.units().collect();
    assert_eq!(units.len(), 1, "the 10h is not read");
    let unit = units[0];
    assert_eq!(unit.devid, devid(0, 0, 2), "00:00.2");
    assert_eq!(unit.base, 0xF750_0000);
    assert_eq!(unit.segment, 0);
    assert_eq!(unit.efr, Some(0x2465_77ef_a225_4afa), "the EFR Linux used");
    assert!(!unit.translation_disabled());
    assert_eq!(ivrs.memory().count(), 0, "no IVMD: nothing holds the unit");
}

/// The functions this work is for, and the ones the IVHD leaves out.
///
/// Verifies: L.iommu.79, L.iommu.97
#[test]
fn nazunas_functions_are_named_as_their_own() {
    let bytes = NAZUNA;
    let ivrs = parse(&bytes);
    let unit = ivrs.units().next().unwrap();
    for (what, named) in [
        ("the RTX 3060", devid(1, 0, 0)),
        ("its audio", devid(1, 0, 1)),
        ("the RTX 3090", devid(3, 0, 0)),
        ("the SN720", devid(6, 0, 0)),
        ("the RTL8111H", devid(8, 0, 0)),
        ("a dummy host bridge", devid(0, 1, 0)),
        ("the chipset switch's port", devid(5, 9, 0)),
    ] {
        assert_eq!(unit.names(named), Named::Own, "{what}");
    }
    assert_eq!(
        unit.names(devid(0xFF, 0, 0)),
        Named::Aliased {
            source: devid(0, 0x14, 5)
        },
        "bus 0xFF arrives as 00:14.5"
    );
    for left_out in [devid(0, 0, 0), devid(0, 0, 1), devid(0, 0, 2)] {
        assert_eq!(unit.names(left_out), Named::No, "{left_out:#x}");
    }
    assert!(!unit.includes_all());
    assert_eq!(unit.special(SPECIAL_IOAPIC, 0x20), Some(devid(0, 0x14, 0)));
    assert_eq!(unit.special(SPECIAL_IOAPIC, 0x21), Some(devid(0, 0, 1)));
    assert_eq!(unit.special(SPECIAL_HPET, 0), Some(devid(0, 0x14, 0)));
    assert_eq!(unit.special(SPECIAL_IOAPIC, 0), None);
}

/// Verifies: L.iommu.79, L.iommu.81
#[test]
fn qemus_ivrs_is_its_unit_once() {
    let bytes = qemu(0, 0x29d);
    let ivrs = parse(&bytes);
    assert_eq!(ivrs.chosen_type(), Some(IVHD_11));
    assert!(ivrs.whole());
    let units: Vec<_> = ivrs.units().collect();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].base, 0xFED8_0000);
    assert_eq!(units[0].devid, devid(0, 3, 0));
    assert_eq!(units[0].efr, Some(0x29d));
    for device in 0..6 {
        assert_eq!(units[0].names(devid(0, device, 0)), Named::Own);
    }
    assert_eq!(units[0].names(devid(0, 6, 0)), Named::No);
    assert_eq!(
        units[0].special(SPECIAL_IOAPIC, 0),
        Some(devid(0, 0x14, 0)),
        "QEMU's IOAPIC_SB_DEVID"
    );
}

/// A table with only a 10h reads it, and its EFR comes from the unit.
///
/// Verifies: L.iommu.81
#[test]
fn a_lone_10h_is_read() {
    let entries = qemu_entries();
    let bytes = table(1, &ivhd(IVHD_10, 0x18, 0xFED8_0000, 0, 0, &entries));
    let ivrs = parse(&bytes);
    assert_eq!(ivrs.chosen_type(), Some(IVHD_10));
    let unit = ivrs.units().next().unwrap();
    assert_eq!(unit.efr, None);
    assert_ne!(unit.feature_reporting, 0);
    assert!(ivrs.whole());
}

/// The 40h is the highest type, and its ACPI-HID entries are stepped over
/// by their length.
///
/// Verifies: L.iommu.79, L.iommu.81
#[test]
fn a_40h_with_an_acpi_hid_entry_is_read_through_it() {
    let mut entries = Vec::new();
    entries.extend_from_slice(&short(0x02, 0x08));
    // F0h: devid, flags, HID (8), CID (8), UID format, UID length 4, UID.
    let mut hid = vec![0xF0, 0xA0, 0x00, 0x00];
    hid.extend_from_slice(b"AMDI0040");
    hid.extend_from_slice(&[0; 8]);
    hid.extend_from_slice(&[2, 4]);
    hid.extend_from_slice(b"ABCD");
    entries.extend_from_slice(&hid);
    entries.extend_from_slice(&short(0x02, 0x10));
    let mut body = ivhd(IVHD_11, 0x02, 0xF750_0000, 0, 1, &entries);
    body.extend(ivhd(IVHD_40, 0x02, 0xF750_0000, 0, 1, &entries));
    let bytes = table(1, &body);
    let ivrs = parse(&bytes);
    assert_eq!(ivrs.chosen_type(), Some(IVHD_40));
    assert!(ivrs.whole());
    let unit = ivrs.units().next().unwrap();
    assert_eq!(
        unit.names(0x10),
        Named::Own,
        "read past the 26-byte HID entry"
    );
    assert!(
        unit.entries()
            .any(|entry| entry == Entry::AcpiHid { devid: 0xA0 })
    );
}

/// Verifies: L.iommu.97
#[test]
fn alias_select_and_extended_entries() {
    let mut entries = Vec::new();
    entries.extend_from_slice(&long(0x42, 0x0500, [0, 0x08, 0x04, 0]));
    entries.extend_from_slice(&long(0x46, 0x0600, [0, 0, 0, 0]));
    entries.extend_from_slice(&long(0x47, 0x0700, [0, 0, 0, 0]));
    entries.extend_from_slice(&short(0x04, 0x07FF));
    entries.extend_from_slice(&short(0x00, 0));
    entries.extend_from_slice(&short(0x01, 0));
    let bytes = table(1, &ivhd(IVHD_11, 0x02, 0xF750_0000, 0, 1, &entries));
    let ivrs = parse(&bytes);
    assert!(ivrs.whole());
    let unit = ivrs.units().next().unwrap();
    assert_eq!(unit.names(0x0500), Named::Aliased { source: 0x0408 });
    assert_eq!(unit.names(0x0600), Named::Own);
    assert_eq!(unit.names(0x07FF), Named::Own);
    assert_eq!(unit.names(0x0800), Named::No, "named by ALL only");
    assert!(unit.includes_all());
}

/// Each way a unit's entries cannot be read to their end makes the table
/// not whole.
///
/// Verifies: L.iommu.80
#[test]
fn unreadable_entries_make_the_table_not_whole() {
    let cases: [(&str, Vec<u8>); 6] = [
        ("a lone end of range", short(0x04, 0x10).to_vec()),
        ("a range ending below its start", {
            let mut entries = short(0x03, 0x20).to_vec();
            entries.extend_from_slice(&short(0x04, 0x10));
            entries
        }),
        ("a range never ended", short(0x03, 0x20).to_vec()),
        ("a start followed by another start", {
            let mut entries = short(0x03, 0x20).to_vec();
            entries.extend_from_slice(&short(0x03, 0x30));
            entries.extend_from_slice(&short(0x04, 0x40));
            entries
        }),
        ("a variable type with no known length", vec![0x80, 0, 0, 0]),
        ("a reserved fixed-size type", short(0x05, 0x10).to_vec()),
    ];
    for (what, entries) in cases {
        let bytes = table(1, &ivhd(IVHD_11, 0x02, 0xF750_0000, 0, 1, &entries));
        let ivrs = parse(&bytes);
        assert!(!ivrs.whole(), "{what}");
        let unit = ivrs.units().next().unwrap();
        assert!(!unit.entries_whole(), "{what}");
    }
}

/// An entry cut off by the block's end is unreadable too.
///
/// Verifies: L.iommu.80
#[test]
fn an_entry_cut_off_by_the_block_is_unreadable() {
    let mut entries = qemu_entries();
    entries.truncate(entries.len() - 3);
    let bytes = table(1, &ivhd(IVHD_11, 0x18, 0xFED8_0000, 0, 1, &entries));
    assert!(!parse(&bytes).whole());
}

/// Verifies: L.iommu.80
#[test]
fn a_block_past_the_table_or_too_short_ends_the_walk() {
    let mut bytes = qemu(0, 1);
    // The 11h's length one past the table.
    let at = IVRS_HEADER_LEN + 24 + qemu_entries().len() + 2;
    let length = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    bytes[at..at + 2].copy_from_slice(&(length + 1).to_le_bytes());
    let ivrs = parse(&bytes);
    assert!(!ivrs.whole(), "the 11h runs past the table");
    assert!(ivrs.blocks().any(|block| block == Block::Broken));

    let bytes = table(1, &[0x10, 0, 2, 0]);
    assert!(!parse(&bytes).whole(), "a block shorter than its header");

    let bytes = table(1, &ivmd(IVMD_ONE, 1, 0x10, 0, 0x1000, 0x1000)[..24]);
    assert!(!parse(&bytes).whole(), "an IVMD cut short");
}

/// A unit a 10h names that the chosen 11h does not.
///
/// Verifies: L.iommu.80, L.iommu.81
#[test]
fn a_unit_missing_at_the_chosen_type_makes_the_table_not_whole() {
    let entries = qemu_entries();
    let mut body = ivhd(IVHD_10, 0x02, 0xF750_0000, 0, 0, &entries);
    body.extend(ivhd(IVHD_11, 0x02, 0xF750_0000, 0, 1, &entries));
    body.extend(ivhd(IVHD_10, 0x42, 0xF760_0000, 0, 0, &entries));
    let bytes = table(1, &body);
    let ivrs = parse(&bytes);
    assert_eq!(ivrs.chosen_type(), Some(IVHD_11));
    assert_eq!(ivrs.units().count(), 1);
    assert!(!ivrs.whole(), "the second unit is described at 10h only");
}

/// QEMU's `dma-translation=off` sets `HATDis`.
///
/// Verifies: L.iommu.82
#[test]
fn hat_disabled_is_read_from_the_attributes() {
    let bytes = qemu(ATTRIBUTE_HAT_DISABLED, 0x29d | 3 << 10);
    let unit = parse(&bytes).units().next().unwrap();
    assert!(unit.translation_disabled());
}

/// Verifies: L.iommu.79, L.iommu.94
#[test]
fn ivmds_name_their_devids() {
    let mut body = ivhd(IVHD_11, 0x02, 0xF750_0000, 0, 1, &qemu_entries());
    body.extend(ivmd(
        IVMD_ALL,
        IVMD_UNITY | IVMD_READ,
        0,
        0,
        0x7C00_0000,
        0x10_0000,
    ));
    body.extend(ivmd(
        IVMD_ONE,
        IVMD_UNITY | IVMD_READ | IVMD_WRITE,
        0x10,
        0,
        0x8000,
        0x2000,
    ));
    body.extend(ivmd(
        IVMD_RANGE,
        IVMD_EXCLUSION,
        0x20,
        0x2F,
        0x10_0000,
        0x1000,
    ));
    body.extend(ivmd(IVMD_ONE, 0, 0x30, 0, 0x9000, 0x1000));
    let bytes = table(1, &body);
    let ivrs = parse(&bytes);
    assert!(ivrs.whole());
    let regions: Vec<_> = ivrs.memory().collect();
    assert_eq!(regions.len(), 4);
    assert!(regions[0].names(0x1234), "20h names every devid");
    assert!(regions[1].names(0x10) && !regions[1].names(0x11));
    assert!(regions[2].names(0x20) && regions[2].names(0x2F) && !regions[2].names(0x30));
    assert_eq!(regions[1].limit(), Some(0x9FFF));
    assert!(regions[0].kept() && regions[1].kept() && regions[2].kept());
    assert!(
        !regions[3].kept(),
        "neither a unity map nor an exclusion range"
    );
    assert_eq!(
        Ivmd {
            length: 0,
            ..regions[1]
        }
        .limit(),
        None
    );
}

/// D10 on nazuna: the unit's own function and the root complex, and every
/// case that is neither.
///
/// Verifies: L.iommu.99, L.iommu.100
#[test]
fn d10_on_nazuna_and_beside_it() {
    let bytes = NAZUNA;
    let ivrs = parse(&bytes);
    let units: Vec<(u16, u16, Vec<Entry>)> = ivrs
        .units()
        .map(|unit| (unit.segment, unit.devid, unit.entries().collect()))
        .collect();
    let view = || {
        units
            .iter()
            .map(|(segment, devid, entries)| (*segment, *devid, entries.as_slice()))
    };
    let iommu = 0x08_0600;
    let host_bridge = 0x06_0000;
    assert_eq!(
        exempt(view(), 0, devid(0, 0, 2), iommu, false),
        Some(Exempt::Unit(0)),
        "00:00.2, the unit"
    );
    assert_eq!(
        exempt(view(), 0, devid(0, 0, 0), host_bridge, false),
        Some(Exempt::RootComplex),
        "00:00.0, the root complex"
    );
    assert_eq!(
        exempt(view(), 0, devid(0, 0, 0), host_bridge, true),
        None,
        "bus mastering on: bypassing"
    );
    assert_eq!(
        exempt(view(), 0, devid(0, 0, 0), 0x02_0000, false),
        None,
        "another class"
    );
    assert_eq!(
        exempt(view(), 0, devid(0, 1, 0), host_bridge, false),
        None,
        "a host bridge an entry names (00:01.0, a dummy host bridge)"
    );
    assert_eq!(
        exempt(view(), 0, devid(1, 0, 0), host_bridge, false),
        None,
        "off bus 0: 01:00.0 is named anyway, and a host bridge there is no root complex"
    );
    assert_eq!(
        exempt(view(), 0, devid(0, 0, 2), host_bridge, false),
        Some(Exempt::RootComplex),
        "the unit's devid with a host bridge's class is not the unit: an unnamed host bridge"
    );
    assert_eq!(
        exempt(view(), 1, devid(0, 0, 0), host_bridge, false),
        Some(Exempt::RootComplex),
        "another segment names nothing"
    );
}
