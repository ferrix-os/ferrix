//! Tests for the SMBIOS reader.
//!
//! Tables are assembled here rather than checked in, so a failure names the
//! byte that is wrong, and every hostile case is a known-good table with one
//! thing changed. The good tables are laid out as QEMU's `hw/smbios/smbios.c`
//! lays out its types 0, 1, 3 and 127 under `-smbios type=1,...`.

#![allow(
    clippy::arithmetic_side_effects,
    reason = "fixtures are built with small known lengths"
)]

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

/// One structure: its type, the formatted area after the 4-byte header, and
/// its strings.
fn structure(kind: u8, formatted: &[u8], strings: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![kind, u8::try_from(formatted.len() + 4).unwrap(), 0, 0];
    out.extend_from_slice(formatted);
    for string in strings {
        out.extend_from_slice(string);
        out.push(0);
    }
    if strings.is_empty() {
        out.push(0);
    }
    out.push(0);
    out
}

/// A type 0: vendor 1, version 2, date 3, release 2.0 at 20-21, EC release
/// unknown at 22-23.
fn bios() -> Vec<u8> {
    let mut formatted = vec![0u8; 20];
    formatted[0] = 1; // offset 4: vendor
    formatted[1] = 2; // offset 5: version
    formatted[4] = 3; // offset 8: date
    formatted[16] = 2; // offset 20: major
    formatted[17] = 0; // offset 21: minor
    formatted[18] = 0xFF; // offset 22
    formatted[19] = 0xFF; // offset 23
    structure(
        0,
        &formatted,
        &[b"EFI Development Kit II / OVMF", b"0.0.0", b"02/06/2015"],
    )
}

/// The UUID the type 1 carries.
const UUID: [u8; 16] = [
    0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

/// A type 1: manufacturer 1, product 2, version 3, serial 4, the UUID,
/// SKU 5, family 6.
fn system() -> Vec<u8> {
    let mut formatted = vec![0u8; 23];
    formatted[0] = 1;
    formatted[1] = 2;
    formatted[2] = 3;
    formatted[3] = 4;
    formatted[4..20].copy_from_slice(&UUID);
    formatted[21] = 5; // offset 25: SKU
    formatted[22] = 6; // offset 26: family
    structure(
        1,
        &formatted,
        &[
            b"Ferrix test",
            b"dmi-d1",
            b"pc-q35",
            b"SER-1",
            b"SKU-1",
            b"Fam",
        ],
    )
}

/// A type 3: manufacturer 1, type 0x81 (lock bit and "other"), version 0,
/// serial 2, asset tag 0.
fn chassis() -> Vec<u8> {
    let mut formatted = vec![0u8; 5];
    formatted[0] = 1;
    formatted[1] = 0x81;
    formatted[2] = 0;
    formatted[3] = 2;
    formatted[4] = 0;
    structure(3, &formatted, &[b"QEMU", b"CH-SER"])
}

fn end() -> Vec<u8> {
    structure(127, &[], &[])
}

/// The good table: types 0, 1, 3, 127.
fn table() -> Vec<u8> {
    [bios(), system(), chassis(), end()].concat()
}

/// A 3.x entry point for `table`, with a correct checksum.
fn entry_v3(table_len: u32, version: [u8; 3]) -> Vec<u8> {
    let mut entry = vec![0u8; 24];
    entry[..5].copy_from_slice(b"_SM3_");
    entry[6] = 24;
    entry[7..10].copy_from_slice(&version);
    entry[10] = 1; // entry point revision
    entry[12..16].copy_from_slice(&table_len.to_le_bytes());
    entry[16..24].copy_from_slice(&0x7fb4_0000u64.to_le_bytes());
    fix_checksum(&mut entry, 5, 24);
    entry
}

/// A 2.x entry point for `table`, with both checksums correct.
fn entry_v2(table_len: u16, count: u16, version: [u8; 2]) -> Vec<u8> {
    let mut entry = vec![0u8; 31];
    entry[..4].copy_from_slice(b"_SM_");
    entry[5] = 31;
    entry[6..8].copy_from_slice(&version);
    entry[16..21].copy_from_slice(b"_DMI_");
    entry[22..24].copy_from_slice(&table_len.to_le_bytes());
    entry[24..28].copy_from_slice(&0x000f_0000u32.to_le_bytes());
    entry[28..30].copy_from_slice(&count.to_le_bytes());
    entry[30] = 0x28;
    fix_checksum(&mut entry[16..31], 5, 15);
    fix_checksum(&mut entry, 4, 31);
    entry
}

/// Set byte `at` so that the first `len` bytes sum to zero.
fn fix_checksum(bytes: &mut [u8], at: usize, len: usize) {
    bytes[at] = 0;
    let sum = bytes[..len].iter().fold(0u8, |sum, &b| sum.wrapping_add(b));
    bytes[at] = 0u8.wrapping_sub(sum);
}

fn len32(table: &[u8]) -> u32 {
    u32::try_from(table.len()).unwrap()
}

fn text(smbios: &Smbios<'_>, field: Field) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    smbios
        .field_text(field, &mut |piece| out.extend_from_slice(piece))
        .map(|()| out)
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Verifies: L.discovery.2
#[test]
fn a_v3_entry_point_parses() {
    let table = table();
    let point = EntryPoint::parse(&entry_v3(len32(&table), [3, 0, 0])).unwrap();
    assert_eq!(point.kind, Kind::V3);
    assert_eq!(point.len, 24);
    assert_eq!(point.version, 0x03_00_00);
    assert_eq!(point.table_len, len32(&table));
    assert_eq!(point.table_address, 0x7fb4_0000);
    assert_eq!(point.count, 0);
}

/// Verifies: L.discovery.2
#[test]
fn a_v2_entry_point_parses_and_its_version_is_corrected_as_linux_does() {
    let point = EntryPoint::parse(&entry_v2(100, 4, [2, 8])).unwrap();
    assert_eq!(point.kind, Kind::V2);
    assert_eq!(point.len, 31);
    assert_eq!(point.version, 0x02_08_00);
    assert_eq!((point.table_len, point.count), (100, 4));
    assert_eq!(point.table_address, 0xf_0000);
    let point = EntryPoint::parse(&entry_v2(100, 4, [2, 0x33])).unwrap();
    assert_eq!(point.version, 0x02_06_00, "2.51 is read as 2.6");
    let point = EntryPoint::parse(&entry_v2(100, 4, [2, 0x1F])).unwrap();
    assert_eq!(point.version, 0x02_03_00, "2.31 is read as 2.3");
    let point = EntryPoint::parse(&entry_v2(100, 4, [2, 0x21])).unwrap();
    assert_eq!(point.version, 0x02_03_00, "2.33 is read as 2.3");
}

/// Verifies: L.discovery.2
#[test]
fn a_bad_checksum_is_refused() {
    let mut entry = entry_v3(10, [3, 0, 0]);
    entry[20] ^= 1;
    assert_eq!(EntryPoint::parse(&entry), Err(Error::Checksum));
    let mut entry = entry_v2(10, 1, [2, 8]);
    entry[8] ^= 1;
    assert_eq!(EntryPoint::parse(&entry), Err(Error::Checksum));
}

/// Verifies: L.discovery.2
#[test]
fn a_v2_whose_dmi_header_is_bad_is_refused_even_when_its_own_checksum_holds() {
    let mut entry = entry_v2(10, 1, [2, 8]);
    entry[23] ^= 1;
    // Mend the outer checksum so only the inner one is wrong.
    fix_checksum(&mut entry, 4, 31);
    assert_eq!(EntryPoint::parse(&entry), Err(Error::DmiHeader));
    let mut entry = entry_v2(10, 1, [2, 8]);
    entry[16] = b'X';
    fix_checksum(&mut entry[16..31], 5, 15);
    fix_checksum(&mut entry, 4, 31);
    assert_eq!(EntryPoint::parse(&entry), Err(Error::DmiHeader));
}

/// Verifies: L.discovery.2
#[test]
fn a_v2_whose_own_checksum_fails_is_refused_though_its_dmi_header_holds() {
    // Linux would take this as legacy DMI; Ferrix refuses (docs/DMI.md §2).
    let mut entry = entry_v2(10, 1, [2, 8]);
    entry[4] ^= 0x10;
    assert_eq!(EntryPoint::parse(&entry), Err(Error::Checksum));
}

/// Verifies: L.discovery.2
#[test]
fn entry_point_lengths_out_of_range_are_refused() {
    for len in [0u8, 23, 33, 255] {
        let mut entry = entry_v3(10, [3, 0, 0]);
        entry.resize(40, 0);
        entry[6] = len;
        fix_checksum(&mut entry, 5, usize::from(len).clamp(6, 32));
        assert_eq!(
            EntryPoint::parse(&entry),
            Err(Error::Length),
            "3.x length {len}"
        );
    }
    for len in [0u8, 29, 33] {
        let mut entry = entry_v2(10, 1, [2, 8]);
        entry.resize(40, 0);
        entry[5] = len;
        assert_eq!(
            EntryPoint::parse(&entry),
            Err(Error::Length),
            "2.x length {len}"
        );
    }
}

/// Verifies: L.discovery.2
#[test]
fn an_entry_point_cut_short_is_refused() {
    let entry = entry_v3(10, [3, 0, 0]);
    for cut in 0..entry.len() {
        assert!(
            EntryPoint::parse(&entry[..cut]).is_err(),
            "3.x cut at {cut}"
        );
    }
    let entry = entry_v2(10, 1, [2, 8]);
    for cut in 0..entry.len() {
        assert!(
            EntryPoint::parse(&entry[..cut]).is_err(),
            "2.x cut at {cut}"
        );
    }
}

/// Verifies: L.discovery.2
#[test]
fn no_anchor_is_refused() {
    assert_eq!(
        EntryPoint::parse(b"_SM2_ and the rest of it......."),
        Err(Error::Anchor)
    );
    assert_eq!(EntryPoint::parse(&[0; 32]), Err(Error::Anchor));
}

/// Verifies: L.discovery.2
#[test]
fn an_empty_or_overlong_table_is_refused() {
    assert_eq!(
        EntryPoint::parse(&entry_v3(0, [3, 0, 0])),
        Err(Error::EmptyTable)
    );
    assert_eq!(
        EntryPoint::parse(&entry_v3(TABLE_MAX + 1, [3, 0, 0])),
        Err(Error::TableTooLong)
    );
    assert!(EntryPoint::parse(&entry_v3(TABLE_MAX, [3, 0, 0])).is_ok());
    assert_eq!(
        EntryPoint::parse(&entry_v2(0, 0, [2, 8])),
        Err(Error::EmptyTable)
    );
}

/// Verifies: L.discovery.3
#[test]
fn a_table_not_the_length_the_entry_point_says_is_refused() {
    let table = table();
    let entry = entry_v3(len32(&table) + 1, [3, 0, 0]);
    assert_eq!(Smbios::new(&entry, &table).err(), Some(Error::TableLength));
}

// ---------------------------------------------------------------------------
// The identity fields
// ---------------------------------------------------------------------------

/// Verifies: L.discovery.4
#[test]
fn qemus_table_reads_as_linux_shows_it() {
    let table = table();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(smbios.structures(), 4);
    assert_eq!(smbios.table(), &table[..]);
    assert_eq!(smbios.entry_point(), &entry[..24]);
    let read = |field| text(&smbios, field);
    assert_eq!(
        read(Field::BiosVendor).unwrap(),
        b"EFI Development Kit II / OVMF\n"
    );
    assert_eq!(read(Field::BiosVersion).unwrap(), b"0.0.0\n");
    assert_eq!(read(Field::BiosDate).unwrap(), b"02/06/2015\n");
    assert_eq!(read(Field::BiosRelease).unwrap(), b"2.0\n");
    assert_eq!(
        read(Field::EcFirmwareRelease),
        None,
        "0xFF 0xFF is no release"
    );
    assert_eq!(read(Field::SysVendor).unwrap(), b"Ferrix test\n");
    assert_eq!(read(Field::ProductName).unwrap(), b"dmi-d1\n");
    assert_eq!(read(Field::ProductVersion).unwrap(), b"pc-q35\n");
    assert_eq!(read(Field::ProductSerial).unwrap(), b"SER-1\n");
    assert_eq!(
        read(Field::ProductUuid).unwrap(),
        b"00112233-4455-6677-8899-aabbccddeeff\n",
        "SMBIOS 3.0 is past 2.6: the first three fields are little-endian"
    );
    assert_eq!(read(Field::ProductSku).unwrap(), b"SKU-1\n");
    assert_eq!(read(Field::ProductFamily).unwrap(), b"Fam\n");
    assert_eq!(read(Field::BoardVendor), None, "no type 2");
    assert_eq!(read(Field::ChassisVendor).unwrap(), b"QEMU\n");
    assert_eq!(
        read(Field::ChassisType).unwrap(),
        b"1\n",
        "the top bit is dropped"
    );
    assert_eq!(
        read(Field::ChassisVersion).unwrap(),
        b"\n",
        "index 0 is empty"
    );
    assert_eq!(read(Field::ChassisSerial).unwrap(), b"CH-SER\n");
    assert_eq!(read(Field::ChassisAssetTag).unwrap(), b"\n");
}

/// Verifies: L.discovery.4
#[test]
fn before_2_6_the_uuid_is_big_endian() {
    let table = [system(), end()].concat();
    let entry = entry_v2(u16::try_from(table.len()).unwrap(), 2, [2, 5]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(
        text(&smbios, Field::ProductUuid).unwrap(),
        b"33221100-5544-7766-8899-aabbccddeeff\n"
    );
}

/// Verifies: L.discovery.4
#[test]
fn an_all_zero_or_all_ff_uuid_is_absent() {
    for fill in [0u8, 0xFF] {
        let mut system = system();
        system[8..24].fill(fill);
        let table = [system, end()].concat();
        let entry = entry_v3(len32(&table), [3, 0, 0]);
        let smbios = Smbios::new(&entry, &table).unwrap();
        assert_eq!(text(&smbios, Field::ProductUuid), None, "fill {fill:#x}");
    }
}

/// Verifies: L.discovery.4
#[test]
fn the_modes_are_linuxs() {
    for field in Field::ALL {
        let expected = match field {
            Field::ProductSerial
            | Field::ProductUuid
            | Field::BoardSerial
            | Field::ChassisSerial => 0o400,
            _ => 0o444,
        };
        assert_eq!(field.mode(), expected, "{field:?}");
    }
    assert_eq!(Field::ALL.len(), FIELDS);
    for (at, field) in Field::ALL.into_iter().enumerate() {
        assert_eq!(field.slot(), at);
    }
}

/// Verifies: L.discovery.4
#[test]
fn the_first_structure_of_a_type_wins() {
    let mut second = system();
    // A second type 1 naming another vendor: its first string's first byte.
    second[27] = b'X';
    let table = [system(), second, end()].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(text(&smbios, Field::SysVendor).unwrap(), b"Ferrix test\n");
}

/// Verifies: L.discovery.4
#[test]
fn strings_of_spaces_and_indices_past_the_set_read_empty() {
    let mut formatted = vec![0u8; 23];
    formatted[0] = 1; // "   ", spaces only
    formatted[1] = 2; // "  x", kept whole
    formatted[2] = 9; // past the set
    let table = [structure(1, &formatted, &[b"   ", b"  x"]), end()].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(text(&smbios, Field::SysVendor).unwrap(), b"\n");
    assert_eq!(text(&smbios, Field::ProductName).unwrap(), b"  x\n");
    assert_eq!(text(&smbios, Field::ProductVersion).unwrap(), b"\n");
}

/// Verifies: L.discovery.4
#[test]
fn a_field_past_the_formatted_area_is_absent() {
    // A type 1 of SMBIOS 2.0's length, 8: up to the serial, no UUID, SKU or
    // family.
    let table = [structure(1, &[1, 2, 0, 3], &[b"V", b"P", b"S"]), end()].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(text(&smbios, Field::ProductSerial).unwrap(), b"S\n");
    assert_eq!(text(&smbios, Field::ProductVersion).unwrap(), b"\n");
    assert_eq!(text(&smbios, Field::ProductUuid), None);
    assert_eq!(text(&smbios, Field::ProductSku), None);
    assert_eq!(text(&smbios, Field::ProductFamily), None);
}

/// Verifies: L.discovery.4
#[test]
fn bytes_pass_through_unaltered() {
    let mut formatted = vec![0u8; 23];
    formatted[0] = 1;
    let odd: &[u8] = b"\xff\x01caf\xc3\xa9:\t";
    let table = [structure(1, &formatted, &[odd]), end()].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(
        text(&smbios, Field::SysVendor).unwrap(),
        [odd, b"\n"].concat()
    );
    let mut alias = Vec::new();
    smbios.modalias(&mut |piece| alias.extend_from_slice(piece));
    assert_eq!(alias, b"dmi:svncaf:pn:pvr:sku:\n");
}

/// Verifies: L.discovery.4
#[test]
fn the_modalias_is_linuxs() {
    let table = table();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    let mut alias = Vec::new();
    smbios.modalias(&mut |piece| alias.extend_from_slice(piece));
    assert_eq!(
        std::str::from_utf8(&alias).unwrap(),
        "dmi:bvnEFIDevelopmentKitII/OVMF:bvr0.0.0:bd02/06/2015:br2.0:svnFerrixtest:pndmi-d1:pvrpc-q35:cvnQEMU:ct1:cvr:skuSKU-1:\n"
    );
}

/// Verifies: L.discovery.4
#[test]
fn a_long_string_is_cut_to_a_page() {
    let long = vec![b'a'; 5000];
    let mut formatted = vec![0u8; 23];
    formatted[0] = 1;
    let table = [structure(1, &formatted, &[&long]), end()].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    let shown = text(&smbios, Field::SysVendor).unwrap();
    assert_eq!(shown.len(), 4095);
    assert_eq!(shown.last(), Some(&b'\n'));
    let mut alias = Vec::new();
    smbios.modalias(&mut |piece| alias.extend_from_slice(piece));
    assert_eq!(
        alias.len(),
        3 + 4090 + 2,
        "Linux's room, then `:` and a newline"
    );
}

// ---------------------------------------------------------------------------
// The walk, against hostile tables
// ---------------------------------------------------------------------------

/// Verifies: L.discovery.3
#[test]
fn the_walk_stops_at_type_127_without_a_count_and_cuts_the_table() {
    let mut table = table();
    let good = table.len();
    table.extend_from_slice(&bios());
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(smbios.table().len(), good);
    assert_eq!(smbios.structures(), 4);
}

/// Verifies: L.discovery.3
#[test]
fn with_a_count_the_walk_goes_past_type_127_as_linux_does() {
    let table = [bios(), end(), system()].concat();
    let entry = entry_v2(u16::try_from(table.len()).unwrap(), 3, [2, 8]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(smbios.structures(), 3);
    assert_eq!(text(&smbios, Field::SysVendor).unwrap(), b"Ferrix test\n");
}

/// Verifies: L.discovery.3
#[test]
fn a_2x_table_with_count_0_is_walked_as_linux_walks_it_and_stops_at_type_127() {
    // `!dmi_num`: no count, so the end-of-table structure ends the walk.
    let table = [bios(), end(), system()].concat();
    let entry = entry_v2(u16::try_from(table.len()).unwrap(), 0, [2, 8]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(smbios.structures(), 2);
    assert_eq!(smbios.table().len(), bios().len() + end().len());
    assert_eq!(text(&smbios, Field::SysVendor), None);
}

/// Verifies: L.discovery.3
#[test]
fn a_count_larger_than_the_table_stops_at_its_end() {
    let table = table();
    let entry = entry_v2(u16::try_from(table.len()).unwrap(), 500, [2, 8]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(smbios.structures(), 4);
    assert_eq!(smbios.table(), &table[..]);
}

/// Verifies: L.discovery.3
#[test]
fn a_structure_shorter_than_its_header_stops_the_walk() {
    let mut short = system();
    short[1] = 3;
    let table = [bios(), short, chassis(), end()].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(smbios.structures(), 1);
    assert_eq!(smbios.table().len(), bios().len());
    assert_eq!(text(&smbios, Field::SysVendor), None);
    assert_eq!(text(&smbios, Field::ChassisVendor), None);
}

/// Verifies: L.discovery.3
#[test]
fn a_length_past_the_end_is_not_decoded_and_not_read_past() {
    let mut system = system();
    system[1] = 0xFF;
    let table = [bios(), system].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(text(&smbios, Field::SysVendor), None);
    assert!(smbios.table().len() <= table.len());
    assert_eq!(
        text(&smbios, Field::BiosVendor).unwrap(),
        b"EFI Development Kit II / OVMF\n"
    );
}

/// Verifies: L.discovery.3
#[test]
fn unterminated_strings_are_not_decoded() {
    let mut system = system();
    // Drop the closing double NUL and every NUL between the strings: the
    // set never ends inside the table.
    system.truncate(system.len() - 2);
    for byte in system.iter_mut().skip(27) {
        if *byte == 0 {
            *byte = b'x';
        }
    }
    let table = [bios(), system].concat();
    let entry = entry_v3(len32(&table), [3, 0, 0]);
    let smbios = Smbios::new(&entry, &table).unwrap();
    assert_eq!(text(&smbios, Field::SysVendor), None);
    assert_eq!(text(&smbios, Field::ProductSerial), None);
    assert_eq!(smbios.table().len(), table.len());
}

/// Verifies: L.discovery.3
#[test]
fn every_truncation_and_every_single_byte_change_is_total() {
    let table = table();
    for cut in 0..=table.len() {
        let part = &table[..cut];
        let (used, _) = walk(part, 0, |structure| assert!(structure.len() <= cut));
        assert!(used <= cut);
    }
    for at in 0..table.len() {
        for value in [0u8, 1, 3, 4, 0x7F, 0xFF] {
            let mut changed = table.clone();
            changed[at] = value;
            let entry = entry_v3(len32(&changed), [3, 0, 0]);
            let smbios = Smbios::new(&entry, &changed).unwrap();
            assert!(smbios.table().len() <= changed.len());
            for field in Field::ALL {
                let _ = text(&smbios, field);
            }
            smbios.modalias(&mut |_| {});
        }
    }
}

// ---------------------------------------------------------------------------
// QEMU's own tables (`fixtures.rs` says how they were captured)
// ---------------------------------------------------------------------------

/// What QEMU's tables must read as, from xtask's `-smbios` arguments and QEMU's
/// defaults (QEMU 0b836b518c): `smbios_set_defaults("QEMU", mc->desc,
/// mc->name)` in `hw/i386/fw_cfg.c:76` gives type 1's version and type 2's and
/// 3's manufacturer and version (`hw/smbios/smbios.c:1021-1028`), q35's
/// `mc->desc` is "Standard PC (Q35 + ICH9, 2009)" (`hw/i386/pc_q35.c:342`) and
/// its name `pc-q35-10.2`, type 3's chassis type is 0x01, Other
/// (`smbios.c:650`). Type 0 is OVMF's own, firmware's words, not QEMU's.
fn reads_as_qemu_built_it(entry: &[u8], table: &[u8], kind: Kind, version: u32) {
    let smbios = Smbios::new(entry, table).unwrap();
    assert_eq!(smbios.point().kind, kind);
    assert_eq!(smbios.point().version, version);
    assert_eq!(smbios.table(), table, "QEMU's table is walked whole");
    let read = |field| text(&smbios, field);
    let expected: [(Field, &[u8]); 15] = [
        (Field::SysVendor, "Ferrix QEMU \u{e9}\n".as_bytes()),
        (Field::ProductName, b"Ferrix test machine\n"),
        (Field::ProductVersion, b"pc-q35-10.2\n"),
        (Field::ProductSerial, b"FXSER-SYS-71c3e9\n"),
        (
            Field::ProductUuid,
            b"5f0d9a7e-3b21-4c8e-9d4a-0b6e2f8c1d33\n",
        ),
        (Field::BoardVendor, b"QEMU\n"),
        (Field::BoardName, b"Standard PC (Q35 + ICH9, 2009)\n"),
        (Field::BoardVersion, b"pc-q35-10.2\n"),
        (Field::BoardSerial, b"FXSER-BRD-2a90d4\n"),
        (Field::ChassisVendor, b"QEMU\n"),
        (Field::ChassisType, b"1\n"),
        (Field::ChassisVersion, b"pc-q35-10.2\n"),
        (Field::ChassisSerial, b"FXSER-CHS-e5b017\n"),
        (Field::BiosVendor, b"Ubuntu distribution of EDK II\n"),
        (Field::BiosDate, b"03/31/2026\n"),
    ];
    for (field, shown) in expected {
        assert_eq!(read(field).as_deref(), Some(shown), "{field:?}");
    }
}

/// Verifies: L.discovery.2, L.discovery.3, L.discovery.4
#[test]
fn qemus_3x_table_as_ovmf_installed_it_reads_as_qemu_built_it() {
    use crate::fixtures::{ENTRY_V3, TABLE_V3};
    reads_as_qemu_built_it(&ENTRY_V3, &TABLE_V3, Kind::V3, 0x03_00_00);
}

/// Verifies: L.discovery.2, L.discovery.3, L.discovery.4
#[test]
fn qemus_2x_table_as_ovmf_installed_it_reads_as_qemu_built_it() {
    use crate::fixtures::{ENTRY_V2, TABLE_V2};
    reads_as_qemu_built_it(&ENTRY_V2, &TABLE_V2, Kind::V2, 0x02_08_00);
}

/// A type 16 with Use `usage` and Memory Error Correction `correction`.
fn array(usage: u8, correction: u8) -> Vec<u8> {
    // Location 03h (system board), Use, Correction, then capacity, error
    // handle and device count (SMBIOS 2.1's 15-byte form).
    let mut formatted = vec![0u8; 11];
    formatted[0] = 0x03;
    formatted[1] = usage;
    formatted[2] = correction;
    structure(16, &formatted, &[])
}

/// The scrubber's reading of type 16 (`docs/RAM-SCRUBBER.md` §3.3): only
/// system memory arrays count; every one saying None runs; Parity, the
/// ECCs and CRC stand down; Other and Unknown are unclear; none at all is
/// absent; a short structure or an unknown value is malformed.
///
/// Verifies: L.mm.106
#[test]
fn the_memory_arrays_say_whether_memory_corrects() {
    let read = |arrays: &[Vec<u8>]| {
        let mut table: Vec<u8> = arrays.concat();
        table.extend(end());
        correction_of(&table, 0)
    };
    assert_eq!(read(&[array(3, 3)]), Correction::None);
    assert_eq!(read(&[array(3, 3), array(3, 3)]), Correction::None);
    for kind in 4..=7 {
        assert_eq!(
            read(&[array(3, 3), array(3, kind)]),
            Correction::Corrects(kind)
        );
    }
    assert_eq!(read(&[array(3, 2)]), Correction::Unclear);
    assert_eq!(read(&[array(3, 1)]), Correction::Unclear);
    assert_eq!(read(&[array(3, 2), array(3, 6)]), Correction::Corrects(6));
    // A video memory array with ECC is not system memory.
    assert_eq!(read(&[array(4, 6), array(3, 3)]), Correction::None);
    assert_eq!(read(&[array(4, 6)]), Correction::Absent);
    assert_eq!(read(&[bios(), system()]), Correction::Absent);
    assert_eq!(read(&[array(3, 0)]), Correction::Malformed);
    assert_eq!(read(&[array(3, 9)]), Correction::Malformed);
    // A type 16 three bytes long: no Use, no correction.
    assert_eq!(
        read(&[structure(16, &[0x03, 0x03], &[])]),
        Correction::Malformed
    );
    assert_eq!(read(&[array(3, 6), array(3, 9)]), Correction::Malformed);
}
