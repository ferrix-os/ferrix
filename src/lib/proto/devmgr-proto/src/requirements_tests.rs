//! Host tests of the requirement table and devmgr's check of it against a
//! generic node's identity record (`docs/DRIVER-MODEL.md` §8, D13's
//! missing-provider control).

#![allow(
    unused_results,
    reason = "the tree writer's methods chain, and a test builds a tree statement by statement"
)]

extern crate std;

use std::string::ToString;
use std::vec;
use std::vec::Vec;

use ferrix_fdt::Fdt;
use ferrix_fdt::identity::{self, RECORD_BYTES, Record};
use ferrix_fdt::write::Writer;

use crate::requirements::{Offered, Requirement, Table, What, check, encode, find_section, size};

/// A node like the test driver's: registers, one interrupt, a clock named
/// `apb_pclk`, a pin state `default`, and `with_clock` deciding whether the
/// clock's provider exists.
fn record_bytes(with_clock: bool) -> Vec<u8> {
    let mut structs = vec![0u8; 4096];
    let mut strings = vec![0u8; 512];
    let mut out = vec![0u8; 8192];
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("")
        .cells("#address-cells", &[1])
        .cells("#size-cells", &[1]);
    if with_clock {
        w.begin("clock")
            .strings("compatible", &["fixed-clock"])
            .cells("#clock-cells", &[0])
            .cells("phandle", &[1])
            .end();
    }
    w.begin("pins").strings("compatible", &["vendor,pins"]);
    w.begin("default-pins").cells("phandle", &[2]).end();
    w.end();
    w.begin("seconds@9010000")
        .strings("compatible", &["ferrix,test-seconds"])
        .cells("reg", &[0x901_0000, 0x1000])
        .cells("clocks", &[1])
        .strings("clock-names", &["apb_pclk"])
        .cells("pinctrl-0", &[2])
        .strings("pinctrl-names", &["default"])
        .end();
    w.end();
    let length = w.finish(&mut out).expect("fits");
    out.truncate(length);
    let fdt = Fdt::parse(&out).expect("parses");
    let mut found = None;
    fdt.for_each_lineage(|node, ancestors| {
        if node.name == "seconds@9010000" {
            let mut bytes = vec![0u8; RECORD_BYTES];
            let encoded = identity::encode(&fdt, &node, ancestors, 0, &mut bytes).expect("fits");
            bytes.truncate(encoded.length);
            found = Some(bytes);
        }
        core::ops::ControlFlow::Continue(())
    });
    found.expect("the node")
}

const NEEDS: [Requirement<'static>; 5] = [
    Requirement::at(What::Registers, 0),
    Requirement::at(What::Interrupt, 0),
    Requirement::named(What::Clock, "apb_pclk"),
    Requirement::named(What::PinControl, "default"),
    Requirement::named(What::PowerSupply, "vs").optional(),
];
const TABLE: [u8; size(&NEEDS)] = encode(&NEEDS);

/// Verifies: H.DEV.7
#[test]
fn a_table_reads_back_what_was_encoded() {
    let table = Table::parse(&TABLE).expect("a table");
    let read: Vec<Requirement<'_>> = table.iter().collect();
    assert_eq!(read, NEEDS);
    assert!(Table::parse(&TABLE[..TABLE.len() - 4]).is_err());
    let mut bad = TABLE;
    bad[0] = 0;
    assert!(Table::parse(&bad).is_err());
    let mut unknown = TABLE;
    unknown[8] = 99;
    assert!(Table::parse(&unknown).is_err());
}

/// Verifies: H.DEV.7
#[test]
fn a_node_that_gives_everything_is_admitted() {
    let bytes = record_bytes(true);
    let record = Record::parse(&bytes).expect("a record");
    let table = Table::parse(&TABLE).expect("a table");
    let offered = Offered {
        record,
        apertures: 1,
        vectors: 1,
    };
    assert_eq!(check(&table, &offered), Ok(()));
}

/// Verifies: H.DEV.7
#[test]
fn a_missing_provider_is_refused_and_named() {
    let bytes = record_bytes(false);
    let record = Record::parse(&bytes).expect("a record");
    let table = Table::parse(&TABLE).expect("a table");
    let offered = Offered {
        record,
        apertures: 1,
        vectors: 1,
    };
    let unmet = check(&table, &offered).expect_err("no clock provider");
    assert_eq!(unmet.requirement.what, What::Clock);
    assert!(unmet.unresolved);
    assert_eq!(
        unmet.to_string(),
        "clock apb_pclk (clocks), which names a node that is missing or disabled"
    );
}

/// Verifies: H.DEV.7
#[test]
fn registers_and_interrupts_are_counted_and_absent_ones_named() {
    let bytes = record_bytes(true);
    let record = Record::parse(&bytes).expect("a record");
    let table = Table::parse(&TABLE).expect("a table");
    let none = Offered {
        record,
        apertures: 1,
        vectors: 0,
    };
    let unmet = check(&table, &none).expect_err("no vector");
    assert_eq!(unmet.requirement.what, What::Interrupt);
    assert_eq!(
        unmet.to_string(),
        "interrupt 0 (interrupts), which the node does not give"
    );
    const RESET: [Requirement<'static>; 1] = [Requirement::named(What::Reset, "bus")];
    const RESET_TABLE: [u8; size(&RESET)] = encode(&RESET);
    let table = Table::parse(&RESET_TABLE).expect("a table");
    let all = Offered {
        record,
        apertures: 1,
        vectors: 1,
    };
    assert!(check(&table, &all).is_err());
    const BUS: [Requirement<'static>; 1] = [Requirement::at(What::ParentBus, 0)];
    const BUS_TABLE: [u8; size(&BUS)] = encode(&BUS);
    let table = Table::parse(&BUS_TABLE).expect("a table");
    assert!(
        check(&table, &all).is_err(),
        "only a child device has a parent bus"
    );
}

/// A minimal ELF image with one named section holding `payload`.
fn elf(wide: bool, payload: &[u8]) -> Vec<u8> {
    let names = b"\0.shstrtab\0.ferrix.requirements\0";
    let header = if wide { 64 } else { 52 };
    let entry = if wide { 64 } else { 40 };
    let names_at = header;
    let payload_at = names_at + names.len();
    let sections_at = (payload_at + payload.len()).next_multiple_of(8);
    let mut image = vec![0u8; sections_at + 3 * entry];
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = if wide { 2 } else { 1 };
    image[5] = 1;
    let put16 = |image: &mut Vec<u8>, at: usize, value: u16| {
        image[at..at + 2].copy_from_slice(&value.to_le_bytes());
    };
    if wide {
        image[0x28..0x30].copy_from_slice(&(sections_at as u64).to_le_bytes());
        put16(&mut image, 0x3A, entry as u16);
        put16(&mut image, 0x3C, 3);
        put16(&mut image, 0x3E, 1);
    } else {
        image[0x20..0x24].copy_from_slice(&(sections_at as u32).to_le_bytes());
        put16(&mut image, 0x2E, entry as u16);
        put16(&mut image, 0x30, 3);
        put16(&mut image, 0x32, 1);
    }
    image[names_at..names_at + names.len()].copy_from_slice(names);
    image[payload_at..payload_at + payload.len()].copy_from_slice(payload);
    let mut section = |index: usize, name: u32, offset: usize, size: usize| {
        let at = sections_at + index * entry;
        image[at..at + 4].copy_from_slice(&name.to_le_bytes());
        if wide {
            image[at + 0x18..at + 0x20].copy_from_slice(&(offset as u64).to_le_bytes());
            image[at + 0x20..at + 0x28].copy_from_slice(&(size as u64).to_le_bytes());
        } else {
            image[at + 0x10..at + 0x14].copy_from_slice(&(offset as u32).to_le_bytes());
            image[at + 0x14..at + 0x18].copy_from_slice(&(size as u32).to_le_bytes());
        }
    };
    section(1, 1, names_at, names.len());
    section(2, 11, payload_at, payload.len());
    image
}

/// Verifies: H.DEV.7
#[test]
fn the_table_is_found_in_32_and_64_bit_images() {
    for wide in [false, true] {
        let image = elf(wide, &TABLE);
        let read = |at: u64, buffer: &mut [u8]| {
            let at = at as usize;
            match image.get(at..at + buffer.len()) {
                Some(bytes) => {
                    buffer.copy_from_slice(bytes);
                    true
                }
                None => false,
            }
        };
        let (at, length) = find_section(read, crate::requirements::SECTION).expect("found");
        assert_eq!(&image[at as usize..(at + length) as usize], &TABLE[..]);
        assert_eq!(find_section(read, ".absent"), None);
    }
    let read = |_: u64, _: &mut [u8]| false;
    assert_eq!(find_section(read, crate::requirements::SECTION), None);
}
