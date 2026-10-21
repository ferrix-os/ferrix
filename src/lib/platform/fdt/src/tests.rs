//! Tests for the device tree reader.
//!
//! Blobs are assembled by a builder rather than checked in as binaries, so that
//! a failure names the field that is wrong instead of pointing at an opaque
//! blob, and so that every malformed case can be produced by mutating one field
//! of a known-good tree.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// A synthetic device tree under construction.
struct Builder {
    structs: Vec<u8>,
    strings: Vec<u8>,
    reservations: Vec<(u64, u64)>,
    version: u32,
    last_comp_version: u32,
}

impl Builder {
    fn new() -> Self {
        Builder {
            structs: Vec::new(),
            strings: Vec::new(),
            reservations: Vec::new(),
            version: 17,
            last_comp_version: 16,
        }
    }

    fn token(&mut self, token: u32) {
        self.structs.extend_from_slice(&token.to_be_bytes());
    }

    /// Append raw bytes to the structure block, padding to four.
    fn padded(&mut self, bytes: &[u8]) {
        self.structs.extend_from_slice(bytes);
        while !self.structs.len().is_multiple_of(4) {
            self.structs.push(0);
        }
    }

    /// Add `name` to the strings block and return its offset.
    fn intern(&mut self, name: &str) -> u32 {
        let offset = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        offset
    }

    fn begin(&mut self, name: &str) {
        self.token(FDT_BEGIN_NODE);
        let mut bytes = Vec::from(name.as_bytes());
        bytes.push(0);
        self.padded(&bytes);
    }

    fn end(&mut self) {
        self.token(FDT_END_NODE);
    }

    fn prop(&mut self, name: &str, value: &[u8]) {
        let nameoff = self.intern(name);
        self.token(FDT_PROP);
        self.token(value.len() as u32);
        self.token(nameoff);
        self.padded(value);
    }

    fn prop_str(&mut self, name: &str, value: &str) {
        let mut bytes = Vec::from(value.as_bytes());
        bytes.push(0);
        self.prop(name, &bytes);
    }

    fn prop_strings(&mut self, name: &str, values: &[&str]) {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(0);
        }
        self.prop(name, &bytes);
    }

    fn prop_u32(&mut self, name: &str, value: u32) {
        self.prop(name, &value.to_be_bytes());
    }

    fn prop_cells(&mut self, name: &str, cells: &[u32]) {
        let mut bytes = Vec::new();
        for cell in cells {
            bytes.extend_from_slice(&cell.to_be_bytes());
        }
        self.prop(name, &bytes);
    }

    fn reserve(&mut self, address: u64, size: u64) {
        self.reservations.push((address, size));
    }

    fn build(&self) -> Vec<u8> {
        self.assemble(true)
    }

    fn build_without_end(&self) -> Vec<u8> {
        self.assemble(false)
    }

    fn assemble(&self, terminate: bool) -> Vec<u8> {
        let mut structs = self.structs.clone();
        if terminate {
            structs.extend_from_slice(&FDT_END.to_be_bytes());
        }

        let mut blob = vec![0u8; HEADER_SIZE];
        let off_mem_rsvmap = blob.len() as u32;
        for &(address, size) in &self.reservations {
            blob.extend_from_slice(&address.to_be_bytes());
            blob.extend_from_slice(&size.to_be_bytes());
        }
        blob.extend_from_slice(&[0u8; 16]);

        let off_dt_struct = blob.len() as u32;
        blob.extend_from_slice(&structs);
        let off_dt_strings = blob.len() as u32;
        blob.extend_from_slice(&self.strings);
        let totalsize = blob.len() as u32;

        let header = [
            FDT_MAGIC,
            totalsize,
            off_dt_struct,
            off_dt_strings,
            off_mem_rsvmap,
            self.version,
            self.last_comp_version,
            0,
            self.strings.len() as u32,
            structs.len() as u32,
        ];
        for (index, field) in header.iter().enumerate() {
            blob[index * 4..index * 4 + 4].copy_from_slice(&field.to_be_bytes());
        }
        blob
    }
}

/// Overwrite one big-endian header word.
fn patch(blob: &mut [u8], offset: usize, value: u32) {
    blob[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

/// Header field offsets, so a patch names what it is corrupting.
const OFF_TOTALSIZE: usize = 4;
const OFF_STRUCT: usize = 8;
const OFF_STRINGS: usize = 12;
const OFF_RSVMAP: usize = 16;
const OFF_VERSION: usize = 20;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A tree shaped like QEMU's `virt` machine, trimmed to what the kernel reads.
fn virt() -> Vec<u8> {
    let mut builder = Builder::new();
    builder.reserve(0x4000_0000, 0x1_0000);
    builder.reserve(0x4001_0000, 0x2000);

    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.prop_str("model", "linux,dummy-virt");

    builder.begin("memory@40000000");
    builder.prop_str("device_type", "memory");
    builder.prop_cells("reg", &[0, 0x4000_0000, 0, 0x2000_0000]);
    builder.end();

    builder.begin("pl011@9000000");
    builder.prop_strings("compatible", &["arm,pl011", "arm,primecell"]);
    builder.prop_cells("reg", &[0, 0x0900_0000, 0, 0x1000]);
    builder.prop_cells("interrupts", &[0, 1, 4]);
    builder.end();

    builder.begin("intc@8000000");
    builder.prop_strings("compatible", &["arm,gic-v3"]);
    builder.prop_cells(
        "reg",
        &[0, 0x0800_0000, 0, 0x1_0000, 0, 0x080a_0000, 0, 0x00f6_0000],
    );
    builder.end();

    builder.begin("timer");
    builder.prop_strings("compatible", &["arm,armv8-timer"]);
    builder.prop_cells(
        "interrupts",
        &[1, 13, 0x108, 1, 14, 0x108, 1, 11, 0x108, 1, 10, 0x108],
    );
    builder.end();

    builder.begin("chosen");
    builder.prop_str("stdout-path", "/pl011@9000000:115200n8");
    builder.prop_str("bootargs", "console=ttyAMA0 root=/dev/vda");
    builder.end();

    builder.begin("aliases");
    builder.prop_str("serial0", "/pl011@9000000");
    builder.end();

    builder.end();
    builder.build()
}

/// Parse a fixture that is expected to be well formed.
fn parse(blob: &[u8]) -> Fdt<'_> {
    match Fdt::parse(blob) {
        Ok(fdt) => fdt,
        Err(error) => panic!("fixture should parse, got {error}"),
    }
}

/// The error a blob is rejected with, or `None` if it parses.
///
/// `Fdt` is deliberately not `PartialEq` — comparing two borrowed trees means
/// nothing — so a rejection is asserted on the error alone.
fn rejection(blob: &[u8]) -> Option<FdtError> {
    Fdt::parse(blob).err()
}

// ---------------------------------------------------------------------------
// Header and blocks
// ---------------------------------------------------------------------------

#[test]
fn header_fields_survive_a_round_trip() {
    let blob = virt();
    let fdt = parse(&blob);
    let header = *fdt.header();
    assert_eq!(
        header.totalsize as usize,
        blob.len(),
        "totalsize should cover the whole assembled blob"
    );
    assert_eq!(
        header.version, SUPPORTED_VERSION,
        "the builder emits the version the parser supports"
    );
    assert!(
        header.off_dt_struct.is_multiple_of(4),
        "the structure block must stay four-byte aligned"
    );
    assert_eq!(
        fdt.boot_cpu(),
        0,
        "the fixture boots on CPU 0, as boot_cpuid_phys says"
    );
}

#[test]
fn a_blob_longer_than_totalsize_is_truncated_not_read() {
    let mut blob = virt();
    let declared = blob.len();
    blob.extend_from_slice(&[0xAA; 64]);
    let fdt = parse(&blob);
    assert_eq!(
        fdt.blob().len(),
        declared,
        "bytes past totalsize must not become part of the tree"
    );
}

#[test]
fn bad_magic_is_rejected() {
    let mut blob = virt();
    patch(&mut blob, 0, 0xDEAD_BEEF);
    assert_eq!(
        rejection(&blob),
        Some(FdtError::BadMagic(0xDEAD_BEEF)),
        "a blob that is not a device tree must be named as such"
    );
}

#[test]
fn a_truncated_header_is_rejected() {
    let blob = virt();
    for length in [0usize, 1, 3, 4, 20, 39] {
        assert_eq!(
            rejection(&blob[..length]),
            Some(FdtError::TooShort),
            "a {length}-byte blob is shorter than a header"
        );
    }
}

#[test]
fn a_totalsize_beyond_the_slice_is_rejected() {
    let mut blob = virt();
    let beyond = blob.len() as u32 + 4;
    patch(&mut blob, OFF_TOTALSIZE, beyond);
    assert_eq!(
        rejection(&blob),
        Some(FdtError::BadTotalSize),
        "totalsize must never let the parser read past the slice"
    );
}

#[test]
fn a_totalsize_smaller_than_a_header_is_rejected() {
    let mut blob = virt();
    patch(&mut blob, OFF_TOTALSIZE, 8);
    assert_eq!(
        rejection(&blob),
        Some(FdtError::BadTotalSize),
        "a totalsize that excludes its own header is malformed"
    );
}

#[test]
fn block_offsets_outside_the_blob_are_rejected() {
    let mut struct_out = virt();
    // Rounded up to the alignment the format demands, so that the blob is
    // rejected for the offset and not for a stray low bit.
    let total = (struct_out.len() as u32).next_multiple_of(8);
    patch(&mut struct_out, OFF_STRUCT, total);
    assert_eq!(
        rejection(&struct_out),
        Some(FdtError::BlockOutOfBounds),
        "a structure block starting at the end of the blob cannot hold it"
    );

    let mut strings_out = virt();
    patch(&mut strings_out, OFF_STRINGS, total);
    assert_eq!(
        rejection(&strings_out),
        Some(FdtError::BlockOutOfBounds),
        "a strings block starting at the end of the blob cannot hold it"
    );
}

#[test]
fn a_misaligned_structure_block_is_rejected() {
    let mut blob = virt();
    let offset = u32::from_be_bytes([blob[8], blob[9], blob[10], blob[11]]);
    patch(&mut blob, OFF_STRUCT, offset + 1);
    assert_eq!(
        rejection(&blob),
        Some(FdtError::Misaligned),
        "the token stream is words, so its block must be word aligned"
    );
}

#[test]
fn an_unsupported_version_is_rejected() {
    let mut blob = virt();
    patch(&mut blob, OFF_VERSION, 16);
    assert_eq!(
        rejection(&blob),
        Some(FdtError::UnsupportedVersion(16)),
        "version 16 does not carry size_dt_struct, so it is refused"
    );
}

#[test]
fn a_reservation_block_outside_the_blob_is_rejected() {
    let mut blob = virt();
    // The last eight-byte-aligned position in the blob: a reservation block
    // there has room for at most half of its terminating pair.
    let last = (blob.len() as u32) & !7;
    patch(&mut blob, OFF_RSVMAP, last);
    assert_eq!(
        rejection(&blob),
        Some(FdtError::ReservationOutOfBounds),
        "even an empty reservation list needs sixteen bytes inside the blob"
    );
}

// ---------------------------------------------------------------------------
// Structure block validation
// ---------------------------------------------------------------------------

#[test]
fn a_property_running_past_the_block_is_rejected() {
    let mut builder = Builder::new();
    builder.begin("");
    let nameoff = builder.intern("reg");
    builder.token(FDT_PROP);
    builder.token(0x1000);
    builder.token(nameoff);
    builder.end();
    assert_eq!(
        rejection(&builder.build()),
        Some(FdtError::PropertyOutOfBounds),
        "a length longer than the block must not be believed"
    );
}

#[test]
fn a_property_name_off_the_end_of_the_strings_block_is_rejected() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.end();
    let mut blob = builder.build();
    // Drop the NUL that terminates the last (and only) interned name.
    let total = blob.len();
    blob.truncate(total - 1);
    patch(&mut blob, OFF_TOTALSIZE, (total - 1) as u32);
    let size_of_strings = u32::from_be_bytes([blob[32], blob[33], blob[34], blob[35]]);
    patch(&mut blob, 32, size_of_strings - 1);
    assert_eq!(
        rejection(&blob),
        Some(FdtError::UnterminatedString),
        "a name with no terminator must not be read past its block"
    );
}

#[test]
fn an_unterminated_node_name_is_rejected() {
    let mut builder = Builder::new();
    builder.token(FDT_BEGIN_NODE);
    builder.structs.extend_from_slice(b"abcd");
    assert_eq!(
        rejection(&builder.build_without_end()),
        Some(FdtError::UnterminatedString),
        "a node name that runs to the end of the block is malformed"
    );
}

#[test]
fn an_unbalanced_end_token_is_rejected() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.end();
    builder.end();
    assert_eq!(
        rejection(&builder.build()),
        Some(FdtError::UnbalancedNode),
        "a node cannot end that never began"
    );
}

#[test]
fn a_node_left_open_is_rejected() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("soc");
    builder.end();
    assert_eq!(
        rejection(&builder.build()),
        Some(FdtError::UnbalancedNode),
        "the tree must not end with the root still open"
    );
}

#[test]
fn a_missing_end_token_is_rejected() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.end();
    assert_eq!(
        rejection(&builder.build_without_end()),
        Some(FdtError::MissingEnd),
        "the token stream must end with FDT_END, not simply stop"
    );
}

#[test]
fn an_unknown_token_is_rejected() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.token(7);
    builder.end();
    assert_eq!(
        rejection(&builder.build()),
        Some(FdtError::BadToken(7)),
        "only the five defined tokens may appear"
    );
}

#[test]
fn nesting_deeper_than_the_limit_is_refused_not_recursed_into() {
    let mut builder = Builder::new();
    let levels = MAX_DEPTH + 1;
    for level in 0..levels {
        builder.begin(if level == 0 { "" } else { "child" });
    }
    for _ in 0..levels {
        builder.end();
    }
    assert_eq!(
        rejection(&builder.build()),
        Some(FdtError::DepthOverflow),
        "a tree that nests past MAX_DEPTH must be rejected, never recursed into"
    );
}

#[test]
fn nesting_exactly_at_the_limit_is_accepted() {
    let mut builder = Builder::new();
    for level in 0..MAX_DEPTH {
        builder.begin(if level == 0 { "" } else { "child" });
    }
    for _ in 0..MAX_DEPTH {
        builder.end();
    }
    let blob = builder.build();
    let fdt = parse(&blob);
    assert_eq!(
        fdt.nodes().count(),
        MAX_DEPTH,
        "every level of a tree at the depth limit should still be walked"
    );
}

#[test]
fn nop_tokens_are_skipped_everywhere() {
    let mut builder = Builder::new();
    builder.token(FDT_NOP);
    builder.begin("");
    builder.token(FDT_NOP);
    builder.prop_u32("#address-cells", 1);
    builder.token(FDT_NOP);
    builder.prop_u32("#size-cells", 1);
    builder.begin("uart@1000");
    builder.prop_cells("reg", &[0x1000, 0x100]);
    builder.end();
    builder.token(FDT_NOP);
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let uart = fdt.find_node("/uart@1000").expect("uart should be found");
    assert_eq!(
        uart.reg().next(),
        Some(Region {
            address: 0x1000,
            size: 0x100
        }),
        "padding between properties must not disturb the reg decoding"
    );
}

// ---------------------------------------------------------------------------
// Walking
// ---------------------------------------------------------------------------

#[test]
fn every_node_is_walked_in_tree_order() {
    let blob = virt();
    let fdt = parse(&blob);
    let names: Vec<&str> = fdt.nodes().map(|node| node.name).collect();
    assert_eq!(
        names,
        vec![
            "",
            "memory@40000000",
            "pl011@9000000",
            "intc@8000000",
            "timer",
            "chosen",
            "aliases",
        ],
        "the walk should yield the root and then each child in blob order"
    );
    let depths: Vec<usize> = fdt.nodes().map(|node| node.depth).collect();
    assert_eq!(
        depths,
        vec![0, 1, 1, 1, 1, 1, 1],
        "the root is depth zero and its children depth one"
    );
}

#[test]
fn properties_of_a_node_are_read_back_whole() {
    let blob = virt();
    let fdt = parse(&blob);
    let uart = fdt.find_node("/pl011@9000000").expect("uart should exist");
    let names: Vec<&str> = uart.properties().map(|property| property.name).collect();
    assert_eq!(
        names,
        vec!["compatible", "reg", "interrupts"],
        "a node's property list must stop at its first subnode or its end"
    );
    let compatible: Vec<&str> = uart
        .property("compatible")
        .expect("compatible should exist")
        .strings()
        .collect();
    assert_eq!(
        compatible,
        vec!["arm,pl011", "arm,primecell"],
        "a compatible list is several NUL-terminated strings, not one"
    );
    assert!(
        uart.is_compatible("arm,primecell"),
        "a later entry of the compatible list still matches"
    );
    assert!(
        !uart.is_compatible("arm,pl0"),
        "matching a compatible entry must not be a prefix match"
    );
}

#[test]
fn a_path_component_without_a_unit_address_matches() {
    let blob = virt();
    let fdt = parse(&blob);
    let by_full = fdt.find_node("/memory@40000000");
    let by_base = fdt.find_node("/memory");
    assert!(by_full.is_some(), "the fully written path should be found");
    assert_eq!(
        by_base.map(|node| node.name),
        by_full.map(|node| node.name),
        "/memory should resolve to /memory@40000000"
    );
    assert!(
        fdt.find_node("/memory@1").is_none(),
        "a path with the wrong unit address must not match"
    );
    assert!(
        fdt.find_node("/nothing").is_none(),
        "a path that is not in the tree yields nothing"
    );
    assert_eq!(
        fdt.find_node("/").map(|node| node.depth),
        Some(0),
        "the bare root path names the root"
    );
}

#[test]
fn a_nested_path_matches_only_at_its_own_depth() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    builder.begin("soc");
    builder.begin("uart@9000000");
    builder.prop_cells("reg", &[0x900_0000, 0x1000]);
    builder.end();
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    assert!(
        fdt.find_node("/soc/uart@9000000").is_some(),
        "a two-component path should reach a grandchild"
    );
    assert!(
        fdt.find_node("/uart@9000000").is_none(),
        "a child of /soc must not answer to a root-level path"
    );
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

#[test]
fn memory_is_decoded_with_two_address_and_two_size_cells() {
    let blob = virt();
    let fdt = parse(&blob);
    let regions: Vec<Region> = fdt.memory().collect();
    assert_eq!(
        regions,
        vec![Region {
            address: 0x4000_0000,
            size: 0x2000_0000
        }],
        "the fixture has 512 MiB of RAM at 1 GiB"
    );
}

#[test]
fn memory_is_decoded_with_one_address_and_one_size_cell() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    builder.begin("memory@80000000");
    builder.prop_str("device_type", "memory");
    builder.prop_cells("reg", &[0x8000_0000, 0x1000_0000, 0x9000_0000, 0x1000]);
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let regions: Vec<Region> = fdt.memory().collect();
    assert_eq!(
        regions,
        vec![
            Region {
                address: 0x8000_0000,
                size: 0x1000_0000
            },
            Region {
                address: 0x9000_0000,
                size: 0x1000
            }
        ],
        "one-cell values must be read as single words, not halves of a pair"
    );
}

#[test]
fn several_memory_nodes_are_all_reported() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.begin("memory@40000000");
    builder.prop_str("device_type", "memory");
    builder.prop_cells("reg", &[0, 0x4000_0000, 0, 0x1000_0000]);
    builder.end();
    builder.begin("memory@100000000");
    builder.prop_str("device_type", "memory");
    builder.prop_cells("reg", &[1, 0, 0, 0x2000_0000]);
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let addresses: Vec<u64> = fdt.memory().map(|region| region.address).collect();
    assert_eq!(
        addresses,
        vec![0x4000_0000, 0x1_0000_0000],
        "both memory nodes contribute, and the high word is the upper half"
    );
}

#[test]
fn a_child_overrides_the_cell_counts_of_its_parent() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.begin("soc");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    builder.prop_cells("reg", &[0, 0x1000_0000, 0, 0x1000]);
    builder.begin("uart@9000000");
    builder.prop_cells("reg", &[0x900_0000, 0x1000]);
    builder.end();
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);

    let soc = fdt.find_node("/soc").expect("soc should exist");
    assert_eq!(
        (soc.address_cells, soc.size_cells),
        (2, 2),
        "a node's own reg is decoded with the counts its parent declared"
    );
    assert_eq!(
        soc.reg().next(),
        Some(Region {
            address: 0x1000_0000,
            size: 0x1000
        }),
        "the soc node's reg uses the root's two-cell counts"
    );

    let uart = fdt
        .find_node("/soc/uart@9000000")
        .expect("uart should exist");
    assert_eq!(
        (uart.address_cells, uart.size_cells),
        (1, 1),
        "a child sees the counts its parent declared, not the root's"
    );
    assert_eq!(
        uart.reg().next(),
        Some(Region {
            address: 0x900_0000,
            size: 0x1000
        }),
        "the overridden one-cell counts must decode the child's reg"
    );
}

#[test]
fn cell_counts_are_inherited_by_a_node_that_declares_none() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    builder.begin("bus");
    builder.begin("uart@2000");
    builder.prop_cells("reg", &[0x2000, 0x40]);
    builder.end();
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let uart = fdt.find_node("/bus/uart@2000").expect("uart should exist");
    assert_eq!(
        uart.reg().next(),
        Some(Region {
            address: 0x2000,
            size: 0x40
        }),
        "a bus that declares no counts passes its parent's down unchanged"
    );
}

#[test]
fn a_value_too_wide_for_sixty_four_bits_is_refused() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 3);
    builder.prop_u32("#size-cells", 1);
    builder.begin("wide@0");
    builder.prop_cells("reg", &[1, 0, 0x1000, 0x10]);
    builder.end();
    builder.begin("narrow@0");
    builder.prop_cells("reg", &[0, 0, 0x1000, 0x10]);
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    assert_eq!(
        fdt.find_node("/wide@0")
            .expect("wide should exist")
            .reg()
            .next(),
        None,
        "an address that does not fit a u64 must be refused, not truncated"
    );
    assert_eq!(
        fdt.find_node("/narrow@0")
            .expect("narrow should exist")
            .reg()
            .next(),
        Some(Region {
            address: 0x1000,
            size: 0x10
        }),
        "three cells with a zero high word still describe a 64-bit address"
    );
}

#[test]
fn zero_width_cells_yield_no_regions_instead_of_looping() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 0);
    builder.prop_u32("#size-cells", 0);
    builder.begin("odd");
    builder.prop_cells("reg", &[0, 0, 0, 0]);
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let node = fdt.find_node("/odd").expect("odd should exist");
    assert_eq!(
        node.reg().count(),
        0,
        "a pair of zero-width fields consumes nothing, so it yields nothing"
    );
}

#[test]
fn an_absurd_cell_count_is_ignored_in_favour_of_the_inherited_one() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 1);
    builder.begin("bus");
    builder.prop_u32("#address-cells", 0x4000_0000);
    builder.begin("uart@3000");
    builder.prop_cells("reg", &[0x3000, 0x20]);
    builder.end();
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let uart = fdt.find_node("/bus/uart@3000").expect("uart should exist");
    assert_eq!(
        uart.address_cells, 1,
        "a count wider than MAX_CELLS is ignored rather than honoured"
    );
    assert_eq!(
        uart.reg().next(),
        Some(Region {
            address: 0x3000,
            size: 0x20
        }),
        "the inherited counts still decode the reg"
    );
}

// ---------------------------------------------------------------------------
// The devices the kernel looks for
// ---------------------------------------------------------------------------

#[test]
fn the_console_is_resolved_through_an_absolute_stdout_path() {
    let blob = virt();
    let fdt = parse(&blob);
    assert_eq!(
        fdt.stdout_path(),
        Some("/pl011@9000000:115200n8"),
        "stdout-path is reported raw, options suffix and all"
    );
    let console = fdt.console().expect("the console should resolve");
    assert_eq!(
        console.compatible(),
        Some("arm,pl011"),
        "the console driver is chosen by the first compatible entry"
    );
    assert_eq!(
        console.reg().next(),
        Some(Region {
            address: 0x900_0000,
            size: 0x1000
        }),
        "the console's first reg range is the MMIO window to map"
    );
}

#[test]
fn the_console_is_resolved_through_an_alias() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.begin("pl011@9000000");
    builder.prop_strings("compatible", &["arm,pl011"]);
    builder.prop_cells("reg", &[0, 0x900_0000, 0, 0x1000]);
    builder.end();
    builder.begin("chosen");
    builder.prop_str("stdout-path", "serial0:115200n8");
    builder.end();
    builder.begin("aliases");
    builder.prop_str("serial0", "/pl011@9000000");
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    assert_eq!(
        fdt.alias("serial0"),
        Some("/pl011@9000000"),
        "an alias maps a short name onto a path"
    );
    assert_eq!(
        fdt.console().map(|node| node.name),
        Some("pl011@9000000"),
        "a stdout-path that is an alias must be resolved through /aliases"
    );
    assert!(
        fdt.alias("serial9").is_none(),
        "an alias that does not exist resolves to nothing"
    );
}

#[test]
fn a_stdout_path_without_a_suffix_still_resolves() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("uart");
    builder.prop_strings("compatible", &["ns16550a"]);
    builder.end();
    builder.begin("chosen");
    builder.prop_str("stdout-path", "/uart");
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    assert_eq!(
        fdt.console().and_then(|node| node.compatible()),
        Some("ns16550a"),
        "the colon suffix is optional, not required"
    );
}

#[test]
fn a_dangling_stdout_path_resolves_to_nothing() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("chosen");
    builder.prop_str("stdout-path", "/nowhere@0:115200n8");
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    assert!(
        fdt.console().is_none(),
        "a stdout-path naming a node that is not there yields no console"
    );
}

#[test]
fn a_gicv3_reports_its_distributor_and_redistributor() {
    let blob = virt();
    let fdt = parse(&blob);
    let gic = fdt
        .interrupt_controller()
        .expect("the fixture describes a GIC");
    assert_eq!(
        gic.version,
        GicVersion::V3,
        "arm,gic-v3 must be recognised as a GICv3"
    );
    assert_eq!(
        gic.distributor(),
        Some(Region {
            address: 0x800_0000,
            size: 0x1_0000
        }),
        "the first reg range of a GIC is its distributor"
    );
    assert_eq!(
        gic.redistributor(),
        Some(Region {
            address: 0x80a_0000,
            size: 0xf6_0000
        }),
        "the second reg range of a GICv3 is the redistributor region"
    );
    assert!(
        gic.cpu_interface().is_none(),
        "a GICv3 has no memory-mapped CPU interface"
    );
}

#[test]
fn a_gicv2_reports_its_distributor_and_cpu_interface() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.begin("intc@8000000");
    builder.prop_strings("compatible", &["arm,cortex-a15-gic"]);
    builder.prop_cells(
        "reg",
        &[0, 0x800_0000, 0, 0x1_0000, 0, 0x801_0000, 0, 0x1_0000],
    );
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let gic = fdt
        .interrupt_controller()
        .expect("the tree describes a GICv2");
    assert_eq!(
        gic.version,
        GicVersion::V2,
        "arm,cortex-a15-gic must be recognised as a GICv2"
    );
    assert_eq!(
        gic.cpu_interface(),
        Some(Region {
            address: 0x801_0000,
            size: 0x1_0000
        }),
        "the second reg range of a GICv2 is the CPU interface"
    );
    assert!(
        gic.redistributor().is_none(),
        "a GICv2 has no redistributor"
    );
}

#[test]
fn a_tree_without_an_interrupt_controller_reports_none() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("chosen");
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    assert!(
        fdt.interrupt_controller().is_none(),
        "no GIC node means no interrupt controller, not a wrong one"
    );
}

#[test]
fn the_timer_reports_its_four_interrupts() {
    let blob = virt();
    let fdt = parse(&blob);
    let timer = fdt.timer().expect("the fixture describes a timer");
    assert_eq!(
        timer.name, "timer",
        "the architected timer is found by compatible, not by name"
    );
    let cells: Vec<u32> = timer
        .interrupts()
        .expect("the timer has interrupts")
        .collect();
    assert_eq!(
        cells,
        vec![1, 13, 0x108, 1, 14, 0x108, 1, 11, 0x108, 1, 10, 0x108],
        "the timer's four PPIs are handed over as raw cells"
    );
}

#[test]
fn reservations_are_read_until_the_zero_pair() {
    let blob = virt();
    let fdt = parse(&blob);
    let entries: Vec<MemoryReservation> = fdt.reservations().collect();
    assert_eq!(
        entries,
        vec![
            MemoryReservation {
                address: 0x4000_0000,
                size: 0x1_0000
            },
            MemoryReservation {
                address: 0x4001_0000,
                size: 0x2000
            }
        ],
        "both reserved ranges must be reported, and the terminator must not be"
    );
}

#[test]
fn an_empty_reservation_block_yields_nothing() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    assert_eq!(
        fdt.reservations().count(),
        0,
        "a reservation block that is only its terminator reserves nothing"
    );
}

#[test]
fn root_properties_and_bootargs_are_readable() {
    let blob = virt();
    let fdt = parse(&blob);
    assert_eq!(
        fdt.model(),
        Some("linux,dummy-virt"),
        "the machine's model comes from the root node"
    );
    assert_eq!(
        fdt.bootargs(),
        Some("console=ttyAMA0 root=/dev/vda"),
        "the command line comes from /chosen"
    );
    let root = fdt.root().expect("the root should exist");
    assert_eq!(root.name, "", "the root node has an empty name");
    assert_eq!(
        root.property("#address-cells").and_then(|p| p.as_u32()),
        Some(2),
        "a one-word property reads back as a single big-endian value"
    );
    assert!(
        root.property("nonexistent").is_none(),
        "a property that is not there is absent, not empty"
    );
}

#[test]
fn node_names_split_into_base_and_unit_address() {
    let blob = virt();
    let fdt = parse(&blob);
    let uart = fdt.find_node("/pl011@9000000").expect("uart should exist");
    assert_eq!(
        uart.base_name(),
        "pl011",
        "the base name is everything before the unit address"
    );
    assert_eq!(
        uart.unit_address(),
        Some("9000000"),
        "the unit address is what follows the at sign"
    );
    let timer = fdt.timer().expect("timer should exist");
    assert!(
        timer.unit_address().is_none(),
        "a node without an at sign has no unit address"
    );
}

#[test]
fn property_accessors_refuse_values_of_the_wrong_width() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_cells("pair", &[1, 2]);
    builder.prop("empty", &[]);
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);
    let root = fdt.root().expect("root should exist");
    let pair = root.property("pair").expect("pair should exist");
    assert_eq!(
        pair.as_u32(),
        None,
        "an eight-byte value is not a single word"
    );
    assert_eq!(
        pair.as_u64(),
        Some(0x0000_0001_0000_0002),
        "two cells read back as one big-endian doubleword"
    );
    let empty = root.property("empty").expect("empty should exist");
    assert!(
        empty.is_empty(),
        "a zero-length value is how a boolean property is spelled"
    );
    assert_eq!(empty.len(), 0, "an empty value has no bytes");
    assert_eq!(
        empty.as_str(),
        None,
        "an empty value is not a string, since it has no terminator"
    );
}

#[test]
fn errors_describe_themselves() {
    use std::string::ToString;
    let rendered = FdtError::BadMagic(0xDEAD_BEEF).to_string();
    assert!(
        rendered.contains("0xdeadbeef"),
        "the magic error should name what was found, got {rendered}"
    );
    assert!(
        !FdtError::DepthOverflow.to_string().is_empty(),
        "every error variant must render to something readable"
    );
}

// ---------------------------------------------------------------------------
// Totality
// ---------------------------------------------------------------------------

/// Call every accessor on one blob, requiring only that none of them panics.
fn poke_at_everything(blob: &[u8]) {
    let Ok(fdt) = Fdt::parse(blob) else {
        return;
    };
    let _ = fdt.header();
    let _ = fdt.boot_cpu();
    let _ = fdt.model();
    let _ = fdt.bootargs();
    let _ = fdt.stdout_path();
    let _ = fdt.alias("serial0");
    let _ = fdt.find_node("/chosen");
    let _ = fdt.find_node("/memory");
    let _ = fdt.find_compatible("arm,pl011");
    let _ = fdt.timer();
    let _ = fdt.psci_conduit();
    for which in [
        TimerInterrupt::SecurePhysical,
        TimerInterrupt::NonSecurePhysical,
        TimerInterrupt::Virtual,
        TimerInterrupt::Hypervisor,
    ] {
        let _ = fdt.timer_interrupt(which);
    }

    for region in fdt.memory().take(64) {
        let _ = region;
    }
    for reservation in fdt.reservations().take(64) {
        let _ = reservation;
    }
    for cpu in fdt.cpus().take(64) {
        let _ = (cpu.id, cpu.enable_method(), cpu.status());
    }
    if let Some(console) = fdt.console() {
        let _ = console.compatible();
        let _ = console.reg().take(8).count();
    }
    if let Some(gic) = fdt.interrupt_controller() {
        let _ = gic.distributor();
        let _ = gic.redistributor();
        let _ = gic.cpu_interface();
    }
    for smmu in fdt.smmu_v3s().take(64) {
        let _ = smmu.phandle.map(|phandle| fdt.node_by_phandle(phandle));
    }
    for host in fdt.ecam_hosts().take(64) {
        if let Some(map) = fdt.ecam_iommu_map(&host) {
            let _ = map.entries().take(64).count();
            let _ = (map.translate(0), map.translate(u32::MAX));
        }
    }
    for node in fdt.nodes().take(4096) {
        let _ = node.phandle();
        let _ = node.base_name();
        let _ = node.unit_address();
        let _ = node.device_type();
        let _ = node.reg().take(16).count();
        let _ = node.interrupts().map(|cells| cells.take(16).count());
        for property in node.properties().take(64) {
            let _ = property.as_u32();
            let _ = property.as_u64();
            let _ = property.as_str();
            let _ = property.strings().take(16).count();
            let _ = property.cells().take(16).count();
        }
    }
}

#[test]
fn parsing_never_panics_on_corrupted_blobs() {
    // The cheap deterministic version of a fuzz run: walk a known-good blob
    // byte by byte, corrupting each in turn, and require that every accessor
    // either answers or errors. Nothing about the answers is asserted; the
    // property under test is that ring 0 survives whatever firmware hands over.
    for good in [virt(), virt_armv7()] {
        for index in 0..good.len().min(512) {
            for patch_value in [0x00u8, 0x01, 0x7F, 0xFF] {
                let mut blob = good.clone();
                blob[index] = patch_value;
                poke_at_everything(&blob);
            }
        }
    }
}

#[test]
fn parsing_never_panics_on_truncation() {
    let good = virt();
    for length in 0..good.len() {
        poke_at_everything(&good[..length]);
    }
}

#[test]
fn parsing_never_panics_on_arbitrary_bytes() {
    // A header-shaped blob whose every field is hostile, plus a few degenerate
    // slices that have tripped byte parsers before.
    poke_at_everything(&[]);
    poke_at_everything(&[0xFF; 40]);
    let mut hostile = vec![0u8; 128];
    hostile[0..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
    for offset in (4..40).step_by(4) {
        let mut blob = hostile.clone();
        patch(&mut blob, offset, u32::MAX);
        poke_at_everything(&blob);
        patch(&mut blob, offset, 0);
        poke_at_everything(&blob);
    }
}

// ---------------------------------------------------------------------------
// What a 32-bit Arm machine is described with
// ---------------------------------------------------------------------------

/// A tree shaped like QEMU's `virt` machine with a Cortex-A7, as QEMU 8.2
/// generates it: four processors started through PSCI, the 32-bit timer
/// binding, a GICv2, and PSCI over `hvc`.
fn virt_armv7() -> Vec<u8> {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.prop_str("compatible", "linux,dummy-virt");

    builder.begin("cpus");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 0);
    for id in 0..4u32 {
        builder.begin(&std::format!("cpu@{id}"));
        builder.prop_str("device_type", "cpu");
        builder.prop_str("compatible", "arm,cortex-a7");
        builder.prop_u32("reg", id);
        builder.prop_str("enable-method", "psci");
        builder.end();
    }
    builder.end();

    builder.begin("psci");
    builder.prop_strings("compatible", &["arm,psci-1.0", "arm,psci-0.2", "arm,psci"]);
    builder.prop_str("method", "hvc");
    builder.end();

    builder.begin("intc@8000000");
    builder.prop_strings("compatible", &["arm,cortex-a15-gic"]);
    builder.prop_cells(
        "reg",
        &[0, 0x800_0000, 0, 0x1_0000, 0, 0x801_0000, 0, 0x1_0000],
    );
    builder.end();

    builder.begin("timer");
    builder.prop_strings("compatible", &["arm,armv7-timer"]);
    builder.prop_cells(
        "interrupts",
        &[1, 13, 0x104, 1, 14, 0x104, 1, 11, 0x104, 1, 10, 0x104],
    );
    builder.end();

    builder.end();
    builder.build()
}

#[test]
fn an_armv7_timer_is_found_by_its_own_binding() {
    let blob = virt_armv7();
    let fdt = parse(&blob);
    let timer = fdt
        .timer()
        .expect("arm,armv7-timer is the architected timer on a 32-bit CPU");
    assert_eq!(timer.name, "timer");
}

#[test]
fn the_timer_interrupts_decode_to_gic_identifiers() {
    for blob in [virt(), virt_armv7()] {
        let fdt = parse(&blob);
        assert_eq!(
            fdt.timer_interrupt(TimerInterrupt::SecurePhysical),
            Some(29)
        );
        assert_eq!(
            fdt.timer_interrupt(TimerInterrupt::NonSecurePhysical),
            Some(30)
        );
        assert_eq!(
            fdt.timer_interrupt(TimerInterrupt::Virtual),
            Some(27),
            "the virtual timer is PPI 11, identifier 27"
        );
        assert_eq!(fdt.timer_interrupt(TimerInterrupt::Hypervisor), Some(26));
    }
}

#[test]
fn a_timer_specifier_cut_short_names_no_interrupt() {
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("timer");
    builder.prop_strings("compatible", &["arm,armv7-timer"]);
    builder.prop_cells("interrupts", &[1, 13, 0x104, 1, 14]);
    builder.end();
    builder.end();
    let blob = builder.build();
    let fdt = parse(&blob);

    assert_eq!(
        fdt.timer_interrupt(TimerInterrupt::SecurePhysical),
        Some(29)
    );
    assert_eq!(
        fdt.timer_interrupt(TimerInterrupt::NonSecurePhysical),
        None,
        "a specifier missing its flags cell is not trusted"
    );
    assert_eq!(fdt.timer_interrupt(TimerInterrupt::Virtual), None);
}

#[test]
fn gic_specifiers_follow_the_binding() {
    assert_eq!(gic_interrupt_id(1, 11), Some(27));
    assert_eq!(gic_interrupt_id(1, 15), Some(31));
    assert_eq!(
        gic_interrupt_id(1, 16),
        None,
        "there are sixteen private peripherals"
    );
    assert_eq!(gic_interrupt_id(0, 1), Some(33), "the PL011 on virt");
    assert_eq!(gic_interrupt_id(0, 987), Some(1019));
    assert_eq!(
        gic_interrupt_id(0, 988),
        None,
        "identifiers from 1020 up are special, not interrupts"
    );
    assert_eq!(gic_interrupt_id(2, 0), None, "no third kind of peripheral");
}

#[test]
fn every_gicv2_binding_is_recognised() {
    for binding in GICV2_COMPATIBLES {
        let mut builder = Builder::new();
        builder.begin("");
        builder.prop_u32("#address-cells", 1);
        builder.prop_u32("#size-cells", 1);
        builder.begin("interrupt-controller@a0021000");
        builder.prop_strings("compatible", &[binding]);
        builder.prop_cells("reg", &[0xA002_1000, 0x1000, 0xA002_2000, 0x2000]);
        builder.end();
        builder.end();
        let blob = builder.build();
        let fdt = parse(&blob);

        let gic = fdt
            .interrupt_controller()
            .unwrap_or_else(|| panic!("{binding} is a GICv2"));
        assert_eq!(gic.version, GicVersion::V2, "{binding}");
        assert_eq!(
            gic.cpu_interface(),
            Some(Region {
                address: 0xA002_2000,
                size: 0x2000
            }),
            "{binding}: one address cell, as the STM32MP1 writes it"
        );
    }
}

/// The conduit a tree with one PSCI node of this shape reports.
fn psci_conduit_of(compatible: &[&str], method: Option<&str>) -> Option<PsciConduit> {
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("psci");
    builder.prop_strings("compatible", compatible);
    if let Some(method) = method {
        builder.prop_str("method", method);
    }
    builder.end();
    builder.end();
    let blob = builder.build();
    parse(&blob).psci_conduit()
}

#[test]
fn the_psci_conduit_is_read_from_its_method() {
    assert_eq!(
        parse(&virt_armv7()).psci_conduit(),
        Some(PsciConduit::Hvc),
        "QEMU emulates PSCI as a hypervisor when the machine has no EL2 or EL3"
    );
    assert_eq!(
        psci_conduit_of(&["arm,psci-0.2"], Some("smc")),
        Some(PsciConduit::Smc)
    );
    assert_eq!(
        psci_conduit_of(&["arm,psci"], Some("hvc")),
        Some(PsciConduit::Hvc)
    );
    assert_eq!(
        psci_conduit_of(&["arm,psci-1.0"], Some("mmio")),
        None,
        "an unknown conduit is not guessed at"
    );
    assert_eq!(psci_conduit_of(&["arm,psci-1.0"], None), None);
    assert_eq!(psci_conduit_of(&["vendor,not-psci"], Some("smc")), None);
    assert_eq!(
        parse(&virt()).psci_conduit(),
        None,
        "the AArch64 fixture has no PSCI node"
    );
}

// ---------------------------------------------------------------------------
// Processors
// ---------------------------------------------------------------------------

/// The hardware identifiers of the processors a tree describes, in order.
fn cpu_ids(blob: &[u8]) -> Vec<u64> {
    parse(blob).cpus().map(|cpu| cpu.id).collect()
}

/// A tree whose `/cpus` holds whatever `children` writes, with one address
/// cell and no size cells, as every Arm tree has.
fn with_cpus(children: impl FnOnce(&mut Builder)) -> Vec<u8> {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.begin("cpus");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 0);
    children(&mut builder);
    builder.end();
    builder.end();
    builder.build()
}

/// One `cpu` node, with `extra` properties written after its `reg`.
fn cpu_node(builder: &mut Builder, name: &str, reg: Option<u32>, extra: &[(&str, &str)]) {
    builder.begin(name);
    builder.prop_str("device_type", "cpu");
    if let Some(reg) = reg {
        builder.prop_u32("reg", reg);
    }
    for (property, value) in extra {
        builder.prop_str(property, value);
    }
    builder.end();
}

#[test]
fn qemu_armv7_virt_describes_four_processors_started_by_psci() {
    let blob = virt_armv7();
    let fdt = parse(&blob);
    assert_eq!(cpu_ids(&blob), [0, 1, 2, 3]);
    for cpu in fdt.cpus() {
        assert_eq!(cpu.enable_method(), Some("psci"), "cpu {}", cpu.id);
        assert_eq!(
            cpu.status(),
            None,
            "cpu {} has no status, so is okay",
            cpu.id
        );
        assert!(cpu.node.is_compatible("arm,cortex-a7"));
    }
}

#[test]
fn a_two_cell_identifier_carries_aff3() {
    // AArch64 trees use two address cells under `/cpus`, with `Aff3` in the
    // upper one: the identifier is the whole 64-bit value, not its low word.
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("cpus");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 0);
    builder.begin("cpu@100000000");
    builder.prop_str("device_type", "cpu");
    builder.prop_cells("reg", &[0x1, 0x0]);
    builder.end();
    builder.begin("cpu@101");
    builder.prop_str("device_type", "cpu");
    builder.prop_cells("reg", &[0x0, 0x101]);
    builder.end();
    builder.end();
    builder.end();

    assert_eq!(cpu_ids(&builder.build()), [0x1_0000_0000, 0x101]);
}

#[test]
fn only_direct_children_of_cpus_that_are_processors_count() {
    let blob = with_cpus(|builder| {
        // The topology map and idle states live beside the processors and
        // are not processors, however `cpu`-like their children look.
        builder.begin("cpu-map");
        builder.begin("cluster0");
        builder.begin("core0");
        builder.prop_u32("cpu", 1);
        builder.end();
        builder.end();
        builder.end();
        builder.begin("idle-states");
        builder.end();

        // A processor with a cache inside it: the cache is not a second one.
        builder.begin("cpu@0");
        builder.prop_str("device_type", "cpu");
        builder.prop_u32("reg", 0);
        builder.begin("l2-cache");
        builder.prop_str("device_type", "cache");
        builder.prop_u32("reg", 0x77);
        builder.end();
        builder.end();

        // Typed but not named `cpu`: still a processor.
        builder.begin("processor@5");
        builder.prop_str("device_type", "cpu");
        builder.prop_u32("reg", 5);
        builder.end();

        // Named `cpu` but untyped: also still one, the other spelling.
        builder.begin("cpu@6");
        builder.prop_u32("reg", 6);
        builder.end();
    });
    assert_eq!(cpu_ids(&blob), [0, 5, 6]);

    // And a `cpu` node anywhere but directly under `/cpus` is nobody's.
    let mut builder = Builder::new();
    builder.begin("");
    builder.begin("cpus");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 0);
    cpu_node(&mut builder, "cpu@0", Some(0), &[]);
    builder.end();
    builder.begin("soc");
    builder.prop_u32("#address-cells", 1);
    builder.prop_u32("#size-cells", 0);
    cpu_node(&mut builder, "cpu@9", Some(9), &[]);
    builder.end();
    builder.end();
    assert_eq!(
        cpu_ids(&builder.build()),
        [0],
        "the walk must stop at the end of /cpus"
    );
}

#[test]
fn a_failed_processor_is_skipped_and_a_stopped_one_is_not() {
    let blob = with_cpus(|builder| {
        cpu_node(builder, "cpu@0", Some(0), &[("status", "okay")]);
        cpu_node(builder, "cpu@1", Some(1), &[("status", "disabled")]);
        cpu_node(builder, "cpu@2", Some(2), &[("status", "fail")]);
        cpu_node(builder, "cpu@3", Some(3), &[("status", "fail-sss")]);
    });
    let fdt = parse(&blob);
    let found: Vec<(u64, Option<&str>)> = fdt.cpus().map(|cpu| (cpu.id, cpu.status())).collect();
    assert_eq!(
        found,
        [(0, Some("okay")), (1, Some("disabled"))],
        "a disabled processor is quiescent, not absent; a failed one is broken"
    );
}

#[test]
fn a_processor_with_no_reg_cannot_be_addressed_and_is_skipped() {
    let blob = with_cpus(|builder| {
        cpu_node(builder, "cpu@0", None, &[("enable-method", "psci")]);
        cpu_node(
            builder,
            "cpu@1",
            Some(1),
            &[("enable-method", "spin-table")],
        );
    });
    let fdt = parse(&blob);
    let found: Vec<(u64, Option<&str>)> = fdt
        .cpus()
        .map(|cpu| (cpu.id, cpu.enable_method()))
        .collect();
    assert_eq!(found, [(1, Some("spin-table"))]);
}

#[test]
fn a_tree_without_cpus_describes_no_processors() {
    assert!(
        cpu_ids(&virt()).is_empty(),
        "the AArch64 fixture has no /cpus node"
    );
    let blob = with_cpus(|_| {});
    assert!(cpu_ids(&blob).is_empty(), "an empty /cpus holds none");
}

// ---------------------------------------------------------------------------
// PCI host bridges
// ---------------------------------------------------------------------------

/// A root with two address and two size cells, holding whatever `body` adds.
fn tree(body: impl FnOnce(&mut Builder)) -> Vec<u8> {
    let mut builder = Builder::new();
    builder.begin("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    body(&mut builder);
    builder.end();
    builder.build()
}

/// An ECAM host node with a window of `size` bytes at `address`.
fn ecam_node(builder: &mut Builder, address: u64, size: u64, extra: impl FnOnce(&mut Builder)) {
    builder.begin("pcie@10000000");
    builder.prop_str("compatible", PCI_HOST_ECAM_COMPATIBLE);
    builder.prop_cells(
        "reg",
        &[
            (address >> 32) as u32,
            address as u32,
            (size >> 32) as u32,
            size as u32,
        ],
    );
    // A host's own children are addressed with three cells, which must not
    // change how its own `reg` is read.
    builder.prop_u32("#address-cells", 3);
    builder.prop_u32("#size-cells", 2);
    extra(builder);
    builder.end();
}

fn hosts(blob: &[u8]) -> Vec<EcamHost> {
    parse(blob).ecam_hosts().collect()
}

#[test]
fn qemu_virt_describes_one_ecam_host_above_four_gibibytes() {
    let blob = tree(|b| {
        ecam_node(b, 0x40_1000_0000, 0x1000_0000, |b| {
            b.prop_cells("bus-range", &[0, 0xff]);
            b.prop_str("device_type", "pci");
        });
    });
    assert_eq!(
        hosts(&blob),
        vec![EcamHost {
            window: Region {
                address: 0x40_1000_0000,
                size: 0x1000_0000
            },
            segment: None,
            start_bus: 0,
            end_bus: 0xff,
            cam: false,
        }],
        "the window QEMU's virt machine puts in highmem"
    );
}

#[test]
fn an_ecam_host_without_bus_range_or_domain_takes_the_defaults() {
    let blob = tree(|b| {
        ecam_node(b, 0x3f00_0000, 0x1000_0000, |_| {});
        ecam_node(b, 0x5000_0000, 0x0100_0000, |b| {
            b.prop_u32("linux,pci-domain", 2);
            b.prop_cells("bus-range", &[0x10, 0x1f]);
        });
    });
    let found = hosts(&blob);
    assert_eq!(found.len(), 2, "both hosts, in order");
    assert_eq!(
        (found[0].segment, found[0].start_bus, found[0].end_bus),
        (None, 0, 255),
        "defaults"
    );
    assert_eq!(
        (found[1].segment, found[1].start_bus, found[1].end_bus),
        (Some(2), 0x10, 0x1f),
        "declared"
    );
}

#[test]
fn an_ecam_window_smaller_than_its_bus_range_reaches_only_the_buses_it_holds() {
    let blob = tree(|b| {
        ecam_node(b, 0x3f00_0000, 4 << 20, |b| {
            b.prop_cells("bus-range", &[0x10, 0x20]);
        });
    });
    assert_eq!(hosts(&blob)[0].end_bus, 0x13, "four buses from 0x10");

    let blob = tree(|b| {
        ecam_node(b, 0x3f00_0000, 0x8_0000, |_| {});
    });
    assert!(hosts(&blob).is_empty(), "half a megabyte is not one bus");

    let blob = tree(|b| {
        ecam_node(b, 0x3f00_0000, 1 << 40, |b| {
            b.prop_cells("bus-range", &[0xf0, 0xff]);
        });
    });
    assert_eq!(
        hosts(&blob)[0].end_bus,
        0xff,
        "a huge window holds every bus it names"
    );
}

#[test]
fn an_ecam_host_that_cannot_be_used_is_skipped() {
    let blob = tree(|b| {
        ecam_node(b, 0x1000_0000, 1 << 28, |b| {
            b.prop_str("status", "disabled");
        });
        ecam_node(b, 0x2000_0000, 1 << 28, |b| {
            b.prop_cells("bus-range", &[2, 1]);
        });
        ecam_node(b, 0x3000_0000, 1 << 28, |b| {
            b.prop_cells("bus-range", &[0, 300]);
        });
        ecam_node(b, 0x4000_0000, 1 << 28, |b| {
            b.prop_cells("bus-range", &[0, 1, 2]);
        });
        ecam_node(b, 0x5000_0000, 1 << 28, |b| {
            b.prop_u32("linux,pci-domain", 0x1_0000);
        });
        // CAM, but smaller than its one bus of 64 KiB.
        b.begin("pcie@60000000");
        b.prop_str("compatible", PCI_HOST_CAM_COMPATIBLE);
        b.prop_cells("reg", &[0, 0x6000_0000, 0, 0x8000]);
        b.end();
        b.begin("pcie@70000000");
        b.prop_str("compatible", PCI_HOST_ECAM_COMPATIBLE);
        b.end();
        ecam_node(b, 0x8000_0000, 1 << 28, |b| {
            b.prop_str("status", "okay");
        });
    });
    let found = hosts(&blob);
    assert_eq!(found.len(), 1, "only the last: {found:?}");
    assert_eq!(found[0].window.address, 0x8000_0000, "explicitly okay");
}

// ---------------------------------------------------------------------------
// Devices by binding, and their interrupts
// ---------------------------------------------------------------------------

/// A `virtio,mmio` node at `base` with `interrupts` and extra properties.
fn virtio_node(builder: &mut Builder, base: u32, interrupts: &[u32], status: Option<&str>) {
    builder.begin(&std::format!("virtio_mmio@{base:x}"));
    builder.prop_cells("interrupts", interrupts);
    builder.prop_cells("reg", &[0, base, 0, 0x200]);
    builder.prop_str("compatible", VIRTIO_MMIO_COMPATIBLE);
    if let Some(status) = status {
        builder.prop_str("status", status);
    }
    builder.end();
}

#[test]
fn qemu_virt_lists_its_virtio_mmio_transports_with_their_interrupts() {
    let blob = tree(|b| {
        b.begin("intc@8000000");
        b.prop_strings("compatible", &["arm,cortex-a15-gic"]);
        b.prop_u32("#interrupt-cells", 3);
        b.end();
        virtio_node(b, 0x0a00_0000, &[0, 0x10, 1], None);
        virtio_node(b, 0x0a00_0200, &[0, 0x11, 1], Some("okay"));
        virtio_node(b, 0x0a00_0400, &[0, 0x12, 1], Some("disabled"));
    });
    let fdt = parse(&blob);
    let found: Vec<_> = fdt
        .compatible_nodes(VIRTIO_MMIO_COMPATIBLE)
        .map(|node| {
            let base = node.reg().next().map(|region| region.address);
            let interrupts: Vec<_> = node.gic_interrupts().collect();
            (base, interrupts)
        })
        .collect();
    let edge = |id| GicInterrupt {
        id,
        trigger: Some(Trigger::EdgeRising),
    };
    assert_eq!(
        found,
        vec![
            (Some(0x0a00_0000), vec![edge(48)]),
            (Some(0x0a00_0200), vec![edge(49)])
        ],
        "SPI 16 and 17, the disabled third skipped"
    );
    assert_eq!(
        fdt.interrupt_controller()
            .and_then(|gic| gic.interrupt_cells()),
        Some(3),
        "three-cell specifiers"
    );
}

#[test]
fn a_gic_specifier_carries_its_sense_when_it_names_exactly_one() {
    let sense = |flags| gic_interrupt(0, 5, flags).map(|interrupt| interrupt.trigger);
    assert_eq!(sense(1), Some(Some(Trigger::EdgeRising)), "1");
    assert_eq!(sense(2), Some(Some(Trigger::EdgeFalling)), "2");
    assert_eq!(sense(4), Some(Some(Trigger::LevelHigh)), "4");
    assert_eq!(sense(8), Some(Some(Trigger::LevelLow)), "8");
    assert_eq!(
        sense(0x104),
        Some(Some(Trigger::LevelHigh)),
        "CPU mask ignored"
    );
    assert_eq!(sense(0), Some(None), "controller default");
    assert_eq!(sense(3), Some(None), "two senses");
    assert_eq!(gic_interrupt(1, 13, 4).map(|i| i.id), Some(29), "a PPI");
    assert_eq!(gic_interrupt(2, 0, 4), None, "no such kind");
}

#[test]
fn gic_interrupts_stop_at_the_first_bad_or_partial_specifier() {
    let blob = tree(|b| {
        b.begin("dev@0");
        b.prop_cells("interrupts", &[0, 1, 4, 0, 2, 4, 7, 3, 4, 0, 4, 4, 0]);
        b.end();
        b.begin("tail@0");
        b.prop_cells("interrupts", &[0, 1, 4, 0, 2]);
        b.end();
        b.begin("none@0");
        b.end();
    });
    let fdt = parse(&blob);
    let ids = |path| -> Vec<u32> {
        fdt.find_node(path)
            .unwrap()
            .gic_interrupts()
            .map(|interrupt| interrupt.id)
            .collect()
    };
    assert_eq!(
        ids("/dev@0"),
        vec![33, 34],
        "kind 7 ends it, the good one after is not renumbered"
    );
    assert_eq!(ids("/tail@0"), vec![33], "a partial specifier is not one");
    assert!(ids("/none@0").is_empty(), "no property");
}

#[test]
fn a_node_is_enabled_unless_its_status_says_otherwise() {
    let blob = tree(|b| {
        for (name, status) in [
            ("a@0", None),
            ("b@0", Some("okay")),
            ("c@0", Some("ok")),
            ("d@0", Some("disabled")),
            ("e@0", Some("fail")),
        ] {
            b.begin(name);
            if let Some(status) = status {
                b.prop_str("status", status);
            }
            b.end();
        }
    });
    let fdt = parse(&blob);
    let enabled: Vec<_> = fdt
        .nodes()
        .filter(|node| node.depth == 1 && node.is_enabled())
        .map(|node| node.name)
        .collect();
    assert_eq!(enabled, vec!["a@0", "b@0", "c@0"], "absent, okay and ok");
}

// ---------------------------------------------------------------------------
// GICv2m frames
// ---------------------------------------------------------------------------

#[test]
fn qemu_virt_describes_a_gicv2m_frame_inside_its_gic() {
    let blob = tree(|b| {
        b.begin("intc@8000000");
        b.prop_strings("compatible", &["arm,cortex-a15-gic"]);
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin("v2m@8020000");
        b.prop_str("compatible", GICV2M_FRAME_COMPATIBLE);
        b.prop_cells("reg", &[0, 0x0802_0000, 0, 0x1000]);
        b.end();
        b.begin("v2m@8030000");
        b.prop_str("compatible", GICV2M_FRAME_COMPATIBLE);
        b.prop_cells("reg", &[0, 0x0803_0000, 0, 0x1000]);
        b.prop_u32("arm,msi-base-spi", 160);
        b.prop_u32("arm,msi-num-spis", 32);
        b.end();
        b.begin("v2m@8040000");
        b.prop_str("compatible", GICV2M_FRAME_COMPATIBLE);
        b.prop_str("status", "disabled");
        b.prop_cells("reg", &[0, 0x0804_0000, 0, 0x1000]);
        b.end();
        b.end();
    });
    let frames: Vec<MsiFrame> = parse(&blob).gicv2m_frames().collect();
    assert_eq!(
        frames,
        vec![
            MsiFrame {
                region: Region {
                    address: 0x0802_0000,
                    size: 0x1000
                },
                spi_base: None,
                spi_count: None
            },
            MsiFrame {
                region: Region {
                    address: 0x0803_0000,
                    size: 0x1000
                },
                spi_base: Some(160),
                spi_count: Some(32)
            },
        ],
        "QEMU's, one with overrides, and the disabled one skipped"
    );
}

// ---------------------------------------------------------------------------
// GICv3 ITSes
// ---------------------------------------------------------------------------

#[test]
fn qemu_virt_describes_its_its_inside_its_gic() {
    let blob = tree(|b| {
        b.begin("intc@8000000");
        b.prop_str("compatible", GICV3_COMPATIBLE);
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.prop_cells(
            "reg",
            &[0, 0x0800_0000, 0, 0x1_0000, 0, 0x080A_0000, 0, 0xF6_0000],
        );
        b.begin("its@8070000");
        b.prop_str("compatible", GICV3_ITS_COMPATIBLE);
        b.prop_str("status", "disabled");
        b.prop_cells("reg", &[0, 0x0807_0000, 0, 0x2_0000]);
        b.end();
        b.begin("its@8080000");
        b.prop_str("compatible", GICV3_ITS_COMPATIBLE);
        b.prop("msi-controller", &[]);
        b.prop_cells("reg", &[0, 0x0808_0000, 0, 0x2_0000]);
        b.end();
        b.end();
    });
    let fdt = parse(&blob);
    assert_eq!(
        fdt.gicv3_its(),
        Some(Region {
            address: 0x0808_0000,
            size: 0x2_0000
        }),
        "the enabled one, its reg read with the GIC's two-cell counts"
    );
    assert_eq!(
        fdt.interrupt_controller().map(|gic| gic.version),
        Some(GicVersion::V3),
        "the ITS's own compatible does not make it the GIC"
    );
}

#[test]
fn a_gicv3_without_an_its_has_none() {
    let blob = virt();
    assert_eq!(
        parse(&blob).gicv3_its(),
        None,
        "the fixture's GIC has no ITS"
    );
}

// ---------------------------------------------------------------------------
// IOMMUs, and what a PCI requester ID arrives at them as
// ---------------------------------------------------------------------------

/// An `SMMUv3` node as QEMU's `virt` machine writes it with `iommu=smmuv3`.
fn smmu_node(builder: &mut Builder, phandle: u32) {
    builder.begin("smmuv3@9050000");
    builder.prop_u32("phandle", phandle);
    builder.prop_u32("#iommu-cells", 1);
    builder.prop_cells("reg", &[0, 0x0905_0000, 0, 0x2_0000]);
    builder.prop_str("compatible", SMMU_V3_COMPATIBLE);
    builder.end();
}

#[test]
fn qemu_virt_sends_every_requester_id_to_its_smmu_as_the_same_stream_id() {
    let blob = tree(|b| {
        smmu_node(b, 0x8005);
        ecam_node(b, 0x40_1000_0000, 0x1000_0000, |b| {
            b.prop_cells("iommu-map", &[0, 0x8005, 0, 0x1_0000]);
        });
    });
    let fdt = parse(&blob);
    let smmus: Vec<SmmuV3Node> = fdt.smmu_v3s().collect();
    assert_eq!(
        smmus,
        vec![SmmuV3Node {
            region: Region {
                address: 0x0905_0000,
                size: 0x2_0000
            },
            phandle: Some(0x8005),
            iommu_cells: Some(1),
        }],
        "the SMMU QEMU describes"
    );
    assert_eq!(
        fdt.node_by_phandle(0x8005).map(|node| node.name),
        Some("smmuv3@9050000"),
        "the map's phandle names it"
    );
    let host = hosts(&blob)[0];
    let map = fdt.ecam_iommu_map(&host).expect("the host's iommu-map");
    assert_eq!(map.translate(0x0010), Some((0x8005, 0x0010)), "00:02.0");
    assert_eq!(map.translate(0xffff), Some((0x8005, 0xffff)), "the last");
    assert_eq!(map.translate(0x1_0000), None, "past the entry");
}

#[test]
fn an_iommu_map_is_masked_then_matched_first_entry_first() {
    let blob = tree(|b| {
        ecam_node(b, 0x3f00_0000, 0x1000_0000, |b| {
            b.prop_cells("iommu-map", &[0x100, 1, 0x40, 0x10, 0x100, 2, 0, 0x100]);
            b.prop_u32("iommu-map-mask", 0xfff8);
        });
    });
    let fdt = parse(&blob);
    let map = fdt.ecam_iommu_map(&hosts(&blob)[0]).expect("a map");
    assert_eq!(map.entries().count(), 2, "two entries");
    assert_eq!(
        map.translate(0x10b),
        Some((1, 0x48)),
        "function bits masked off, and the first entry wins"
    );
    assert_eq!(
        map.translate(0x1f8),
        Some((2, 0xf8)),
        "only the second covers it"
    );
    assert_eq!(map.translate(0xff), None, "below every entry");
    assert_eq!(
        (IdMapping {
            rid_base: 0,
            phandle: 1,
            id_base: u32::MAX,
            length: 2,
        })
        .translate(1),
        None,
        "an ID past the top of the space is not wrapped"
    );
}

#[test]
fn a_ragged_map_or_a_malformed_mask_translates_nothing() {
    let blob = tree(|b| {
        ecam_node(b, 0x1000_0000, 0x1000_0000, |b| {
            b.prop_cells("iommu-map", &[0, 1, 0]);
        });
        ecam_node(b, 0x2000_0000, 0x1000_0000, |b| {
            b.prop_cells("iommu-map", &[0, 1, 0, 0x100]);
            b.prop_cells("iommu-map-mask", &[0xff, 0xff]);
        });
        ecam_node(b, 0x3000_0000, 0x1000_0000, |_| {});
    });
    let fdt = parse(&blob);
    let found = hosts(&blob);
    let ragged = fdt.ecam_iommu_map(&found[0]).expect("present, if ragged");
    assert_eq!(ragged.entries().count(), 0, "no partial entry");
    assert_eq!(ragged.translate(0), None, "nothing translated");
    assert!(
        fdt.ecam_iommu_map(&found[1]).is_none(),
        "a two-cell mask makes the map unusable"
    );
    assert!(
        fdt.ecam_iommu_map(&found[2]).is_none(),
        "a host with no map is not behind an IOMMU"
    );
}

// ---------------------------------------------------------------------------
// Following a device's interrupt to the GIC
// ---------------------------------------------------------------------------

/// The GIC specifiers of an STM32MP15's EXTI lines 0 to 30, as
/// `stm32mp151.dtsi` lists them: `Some(spi)` for a line with a GIC interrupt
/// behind it, `None` for the `<0>` placeholder of a line without one.
const STM32MP15_EXTI_SPIS: [Option<u32>; 31] = [
    Some(6),
    Some(7),
    Some(8),
    Some(9),
    Some(10),
    Some(23),
    Some(64),
    Some(65),
    Some(66),
    Some(67),
    Some(40),
    Some(42),
    Some(76),
    Some(77),
    Some(121),
    Some(127),
    Some(1),
    None,
    None,
    Some(3),
    None,
    Some(31),
    Some(33),
    Some(72),
    Some(95),
    Some(107),
    Some(37),
    Some(38),
    Some(39),
    Some(71),
    Some(52),
];

/// The GIC's phandle in the STM32MP15 trees below.
const INTC: u32 = 5;
/// EXTI's phandle in the same trees.
const EXTI: u32 = 8;
/// A GIC specifier's level-high flags.
const LEVEL_HIGH: u32 = 4;

/// An STM32MP15's GIC, and its EXTI with lines 0 to 30 routed as the chip's
/// tree routes them.
fn stm32mp15_controllers(b: &mut Builder) {
    b.begin("interrupt-controller@a0021000");
    b.prop_strings("compatible", &["arm,cortex-a7-gic"]);
    b.prop("interrupt-controller", &[]);
    b.prop_u32("#interrupt-cells", 3);
    b.prop_u32("phandle", INTC);
    b.end();

    let mut routes = Vec::new();
    for spi in STM32MP15_EXTI_SPIS {
        match spi {
            Some(spi) => routes.extend([INTC, 0, spi, LEVEL_HIGH]),
            None => routes.push(0),
        }
    }
    b.begin("interrupt-controller@5000d000");
    b.prop_strings("compatible", &[STM32MP1_EXTI_COMPATIBLE, "syscon"]);
    b.prop("interrupt-controller", &[]);
    b.prop_u32("#interrupt-cells", 2);
    b.prop_u32("phandle", EXTI);
    b.prop_cells("interrupts-extended", &routes);
    b.end();
}

/// A UART whose interrupt is `interrupts-extended` with `cells`.
fn extended_uart(b: &mut Builder, cells: &[u32]) {
    b.begin("serial@40010000");
    b.prop_str("compatible", "st,stm32h7-uart");
    b.prop_cells("interrupts-extended", cells);
    b.end();
}

#[test]
fn a_plain_gic_interrupt_is_followed_as_qemu_virt_writes_it() {
    let blob = tree(|b| {
        b.begin("intc@8000000");
        b.prop_strings("compatible", &["arm,cortex-a15-gic"]);
        b.prop_u32("#interrupt-cells", 3);
        b.end();
        b.begin("pl011@9000000");
        b.prop_str("compatible", "arm,pl011");
        b.prop_cells("interrupts", &[0, 1, LEVEL_HIGH]);
        b.end();
    });
    let fdt = parse(&blob);
    let uart = fdt.find_compatible("arm,pl011").unwrap();
    assert_eq!(
        fdt.gic_interrupt_of(&uart, 0),
        Some(GicInterrupt {
            id: 33,
            trigger: Some(Trigger::LevelHigh)
        })
    );
    assert_eq!(fdt.gic_interrupt_of(&uart, 1), None, "it has only one");
}

#[test]
fn an_extended_entry_naming_the_gic_is_its_interrupt() {
    let blob = tree(|b| {
        stm32mp15_controllers(b);
        extended_uart(b, &[INTC, 0, 52, LEVEL_HIGH]);
    });
    let fdt = parse(&blob);
    let uart = fdt.find_compatible("st,stm32h7-uart").unwrap();
    assert_eq!(
        fdt.gic_interrupt_of(&uart, 0),
        Some(GicInterrupt {
            id: 84,
            trigger: Some(Trigger::LevelHigh)
        })
    );
}

#[test]
fn a_dk1_uart_reaches_gic_spi_52_through_exti_line_30() {
    // UART4 as the DK1's live tree carries it: `<&exti 30 IRQ_TYPE_LEVEL_HIGH>`.
    let blob = tree(|b| {
        stm32mp15_controllers(b);
        extended_uart(b, &[EXTI, 30, LEVEL_HIGH]);
    });
    let fdt = parse(&blob);
    let uart = fdt.find_compatible("st,stm32h7-uart").unwrap();
    assert_eq!(
        fdt.gic_interrupt_of(&uart, 0),
        Some(GicInterrupt {
            id: 84,
            trigger: Some(Trigger::LevelHigh)
        }),
        "the entries before line 30 mix four-cell routes with one-cell placeholders"
    );
}

#[test]
fn every_routed_exti_line_reaches_the_spi_its_entry_names() {
    for (line, spi) in STM32MP15_EXTI_SPIS.iter().enumerate() {
        let line = u32::try_from(line).unwrap();
        let blob = tree(|b| {
            stm32mp15_controllers(b);
            extended_uart(b, &[EXTI, line, LEVEL_HIGH]);
        });
        let fdt = parse(&blob);
        let uart = fdt.find_compatible("st,stm32h7-uart").unwrap();
        let found = fdt.gic_interrupt_of(&uart, 0).map(|interrupt| interrupt.id);
        assert_eq!(found, spi.map(|spi| 32 + spi), "EXTI line {line}");
    }
}

#[test]
fn an_exti_line_past_its_table_or_through_a_second_exti_is_not_followed() {
    let blob = tree(|b| {
        stm32mp15_controllers(b);
        extended_uart(b, &[EXTI, 31, LEVEL_HIGH]);
    });
    let fdt = parse(&blob);
    let uart = fdt.find_compatible("st,stm32h7-uart").unwrap();
    assert_eq!(
        fdt.gic_interrupt_of(&uart, 0),
        None,
        "line 31 is past the table"
    );

    // An EXTI whose line is routed to another EXTI: one level is followed, not two.
    let blob = tree(|b| {
        b.begin("interrupt-controller@1");
        b.prop_strings("compatible", &[STM32MP1_EXTI_COMPATIBLE]);
        b.prop_u32("#interrupt-cells", 2);
        b.prop_u32("phandle", 9);
        b.prop_cells("interrupts-extended", &[EXTI, 30, LEVEL_HIGH]);
        b.end();
        stm32mp15_controllers(b);
        extended_uart(b, &[9, 0, LEVEL_HIGH]);
    });
    let fdt = parse(&blob);
    let uart = fdt.find_compatible("st,stm32h7-uart").unwrap();
    assert_eq!(fdt.gic_interrupt_of(&uart, 0), None);
}

#[test]
fn a_dangling_phandle_or_a_short_entry_is_none_not_a_guess() {
    for cells in [
        &[77, 30, LEVEL_HIGH][..],
        &[EXTI, 30][..],
        &[INTC, 0, 52][..],
        &[][..],
    ] {
        let blob = tree(|b| {
            stm32mp15_controllers(b);
            extended_uart(b, cells);
        });
        let fdt = parse(&blob);
        let uart = fdt.find_compatible("st,stm32h7-uart").unwrap();
        assert_eq!(fdt.gic_interrupt_of(&uart, 0), None, "cells {cells:?}");
    }
}

#[test]
fn a_plain_interrupt_whose_parent_is_not_the_gic_is_not_followed() {
    let blob = tree(|b| {
        stm32mp15_controllers(b);
        b.begin("serial@40010000");
        b.prop_str("compatible", "st,stm32h7-uart");
        b.prop_u32("interrupt-parent", EXTI);
        b.prop_cells("interrupts", &[30, LEVEL_HIGH]);
        b.end();
    });
    let fdt = parse(&blob);
    let uart = fdt.find_compatible("st,stm32h7-uart").unwrap();
    assert_eq!(fdt.gic_interrupt_of(&uart, 0), None);
}

// ---------------------------------------------------------------------------
// A host bridge's windows: what a BAR is allowed to be inside
// ---------------------------------------------------------------------------

/// QEMU `virt`'s own three `ranges` entries, as the machine writes them.
const QEMU_VIRT_RANGES: &[u32] = &[
    // I/O space at bus 0, forwarded to 0x3eff_0000, 64 KiB.
    0x0100_0000,
    0x0000_0000,
    0x0000_0000,
    0x0000_0000,
    0x3eff_0000,
    0x0000_0000,
    0x0001_0000,
    // 32-bit memory, translated by zero.
    0x0200_0000,
    0x0000_0000,
    0x1000_0000,
    0x0000_0000,
    0x1000_0000,
    0x0000_0000,
    0x2eff_0000,
    // 64-bit memory, high.
    0x0300_0000,
    0x0000_0080,
    0x0000_0000,
    0x0000_0080,
    0x0000_0000,
    0x0000_0080,
    0x0000_0000,
];

fn windows(blob: &[u8]) -> Vec<PciWindow> {
    parse(blob)
        .ecam_hosts_with_windows()
        .next()
        .expect("one host")
        .windows
        .take(16)
        .collect()
}

#[test]
fn qemu_virt_forwards_an_io_and_two_memory_windows() {
    let blob = tree(|b| {
        ecam_node(b, 0x4010_0000, 0x1000_0000, |b| {
            b.prop_cells("ranges", QEMU_VIRT_RANGES);
        });
    });
    assert_eq!(
        windows(&blob),
        vec![
            PciWindow {
                space: PciSpace::Io,
                prefetchable: false,
                bus: 0,
                cpu: 0x3eff_0000,
                size: 0x1_0000,
            },
            PciWindow {
                space: PciSpace::Memory32,
                prefetchable: false,
                bus: 0x1000_0000,
                cpu: 0x1000_0000,
                size: 0x2eff_0000,
            },
            PciWindow {
                space: PciSpace::Memory64,
                prefetchable: false,
                bus: 0x80_0000_0000,
                cpu: 0x80_0000_0000,
                size: 0x80_0000_0000,
            },
        ],
        "the windows QEMU's virt machine forwards"
    );
}

#[test]
fn a_bridge_that_translates_gives_back_the_processor_address() {
    // The case QEMU cannot show, because it translates by zero: a BAR holds
    // the bus address, and the aperture must be built from the other one.
    let window = PciWindow {
        space: PciSpace::Memory32,
        prefetchable: false,
        bus: 0x1000_0000,
        cpu: 0xC000_0000,
        size: 0x1000_0000,
    };
    assert_eq!(
        window.translate(0x1000_2000, 0x1000),
        Some(0xC000_2000),
        "a BAR at bus 0x10002000 is reached at 0xC0002000"
    );
    assert_eq!(
        window.translate(0x0FFF_F000, 0x1000),
        None,
        "a bus address below the window is not in it"
    );
}

#[test]
fn a_bar_running_off_the_end_of_a_window_is_not_held_by_it() {
    let window = PciWindow {
        space: PciSpace::Memory32,
        prefetchable: false,
        bus: 0x1000_0000,
        cpu: 0x1000_0000,
        size: 0x1_0000,
    };
    assert!(window.holds(0x1000_F000, 0x1000), "the last page is inside");
    assert!(
        !window.holds(0x1000_F000, 0x2000),
        "a BAR the window forwards only half of is not forwarded"
    );
    assert!(
        !window.holds(0x1000_0000, 0),
        "an empty range is no BAR at all"
    );
    assert!(
        !window.holds(u64::MAX, 0x1000),
        "a length that wraps is refused rather than wrapped"
    );
}

#[test]
fn configuration_space_is_not_a_window_a_bar_may_sit_in() {
    let blob = tree(|b| {
        ecam_node(b, 0x4010_0000, 0x1000_0000, |b| {
            b.prop_cells(
                "ranges",
                &[
                    // Space code 00: configuration space.
                    0x0000_0000,
                    0x0000_0000,
                    0x0000_0000,
                    0x0000_0000,
                    0x4010_0000,
                    0x0000_0000,
                    0x1000_0000, // Then a real memory window, which must still be found.
                    0x0200_0000,
                    0x0000_0000,
                    0x1000_0000,
                    0x0000_0000,
                    0x1000_0000,
                    0x0000_0000,
                    0x1000_0000,
                ],
            );
        });
    });
    assert_eq!(
        windows(&blob),
        vec![PciWindow {
            space: PciSpace::Memory32,
            prefetchable: false,
            bus: 0x1000_0000,
            cpu: 0x1000_0000,
            size: 0x1000_0000,
        }],
        "the configuration entry is skipped and the walk goes on"
    );
}

#[test]
fn a_prefetchable_window_says_so() {
    let blob = tree(|b| {
        ecam_node(b, 0x4010_0000, 0x1000_0000, |b| {
            b.prop_cells(
                "ranges",
                &[
                    // Bit 30 set, on a 64-bit memory window.
                    0x4300_0000,
                    0x0000_0080,
                    0x0000_0000,
                    0x0000_0080,
                    0x0000_0000,
                    0x0000_0000,
                    0x1000_0000,
                ],
            );
        });
    });
    let found = windows(&blob);
    assert_eq!(found.len(), 1, "one window");
    assert!(found[0].prefetchable, "phys.hi bit 30 is prefetchable");
    assert_eq!(found[0].space, PciSpace::Memory64);
}

#[test]
fn ranges_is_not_read_when_the_node_declares_other_cell_counts() {
    // The dangerous case: decoded with the wrong widths, `ranges` yields
    // windows that look plausible and are not there, so a BAR could be
    // admitted against a window nothing forwards.
    let blob = tree(|b| {
        b.begin("pcie@10000000");
        b.prop_str("compatible", PCI_HOST_ECAM_COMPATIBLE);
        b.prop_cells("reg", &[0, 0x4010_0000, 0, 0x1000_0000]);
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.prop_cells("ranges", QEMU_VIRT_RANGES);
        b.end();
    });
    let found: Vec<PciWindow> = parse(&blob)
        .ecam_hosts_with_windows()
        .next()
        .expect("one host")
        .windows
        .take(16)
        .collect();
    assert!(
        found.is_empty(),
        "two address cells is not PCI's three, so ranges is not decoded at all"
    );
}

#[test]
fn a_host_bridge_without_ranges_forwards_nothing() {
    let blob = tree(|b| {
        ecam_node(b, 0x4010_0000, 0x1000_0000, |b| {
            b.prop_cells("bus-range", &[0, 0xff]);
        });
    });
    assert!(
        windows(&blob).is_empty(),
        "no ranges is no window, not an open one"
    );
}

/// crosvm's AArch64 PCI host, as its `--dump-device-tree-blob` wrote it on a
/// Pixel 7 (2026-09-26): CAM at 0x10000, one bus, and an `interrupt-map`
/// giving slots 1 to 5 SPIs 4 to 8, level high, through a GICv3 whose
/// `#address-cells` is 2.
fn crosvm_pci(b: &mut Builder) {
    b.begin("intc");
    b.prop_str("compatible", GICV3_COMPATIBLE);
    b.prop("interrupt-controller", &[]);
    b.prop_u32("#interrupt-cells", 3);
    b.prop_u32("#address-cells", 2);
    b.prop_u32("#size-cells", 2);
    b.prop_u32("phandle", 1);
    b.end();

    b.begin("pci");
    b.prop_str("compatible", PCI_HOST_CAM_COMPATIBLE);
    b.prop_str("device_type", "pci");
    b.prop_cells("bus-range", &[0, 0]);
    b.prop_u32("#address-cells", 3);
    b.prop_u32("#size-cells", 2);
    b.prop_cells("reg", &[0, 0x10000, 0, 0x100_0000]);
    b.prop_u32("#interrupt-cells", 1);
    let mut map = Vec::new();
    for slot in 1..=5_u32 {
        map.extend([slot << 11, 0, 0, 1, 1, 0, 0, 0, slot + 3, 4]);
    }
    b.prop_cells("interrupt-map", &map);
    b.prop_cells("interrupt-map-mask", &[0xf800, 0, 0, 7]);
    b.end();
}

#[test]
fn crosvm_describes_one_cam_host_of_one_bus() {
    let blob = tree(crosvm_pci);
    assert_eq!(
        hosts(&blob),
        vec![EcamHost {
            window: Region {
                address: 0x10000,
                size: 0x100_0000
            },
            segment: None,
            start_bus: 0,
            end_bus: 0,
            cam: true,
        }],
        "crosvm's CAM window, cut to its bus-range"
    );
}

#[test]
fn a_cam_host_window_holds_a_bus_every_64_kib() {
    let blob = tree(|b| {
        b.begin("pci");
        b.prop_str("compatible", PCI_HOST_CAM_COMPATIBLE);
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        b.prop_cells("reg", &[0, 0x10000, 0, 0x3_0000]);
        b.end();
    });
    let found = hosts(&blob);
    assert_eq!(found.len(), 1, "one host");
    assert_eq!(found[0].end_bus, 2, "three buses of 64 KiB, not a megabyte");
    assert!(found[0].cam, "laid out as CAM");
}

#[test]
fn crosvm_routes_each_slots_inta_to_its_own_spi() {
    let blob = tree(crosvm_pci);
    let fdt = parse(&blob);
    let host = fdt.ecam_hosts().next().unwrap();
    for slot in 1..=5_u8 {
        assert_eq!(
            fdt.ecam_intx(&host, 0, slot, 0, 1),
            Some(GicInterrupt {
                id: 32 + 3 + u32::from(slot),
                trigger: Some(Trigger::LevelHigh),
            }),
            "slot {slot}"
        );
    }
    assert_eq!(
        fdt.ecam_intx(&host, 0, 2, 3, 1),
        Some(GicInterrupt {
            id: 37,
            trigger: Some(Trigger::LevelHigh),
        }),
        "the mask ignores the function"
    );
    assert_eq!(fdt.ecam_intx(&host, 0, 6, 0, 1), None, "an unmapped slot");
    assert_eq!(fdt.ecam_intx(&host, 0, 1, 0, 2), None, "INTB, unmapped");
    assert_eq!(fdt.ecam_intx(&host, 0, 1, 0, 0), None, "no pin");
}

#[test]
fn an_interrupt_map_to_anything_but_a_gic_is_not_followed() {
    let blob = tree(|b| {
        b.begin("other");
        b.prop_str("compatible", "vendor,not-a-gic");
        b.prop("interrupt-controller", &[]);
        b.prop_u32("#interrupt-cells", 3);
        b.prop_u32("phandle", 2);
        b.end();
        b.begin("pci");
        b.prop_str("compatible", PCI_HOST_CAM_COMPATIBLE);
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        b.prop_cells("reg", &[0, 0x10000, 0, 0x1_0000]);
        b.prop_u32("#interrupt-cells", 1);
        b.prop_cells("interrupt-map", &[0x800, 0, 0, 1, 2, 0, 4, 4]);
        b.prop_cells("interrupt-map-mask", &[0xf800, 0, 0, 7]);
        b.end();
    });
    let fdt = parse(&blob);
    let host = fdt.ecam_hosts().next().unwrap();
    assert_eq!(fdt.ecam_intx(&host, 0, 1, 0, 1), None, "not a GIC");
}

// ---------------------------------------------------------------------------
// cpu-map
// ---------------------------------------------------------------------------

/// A `cpu-map` of one cluster with `cores`, each listing `threads` threads
/// (none: the core names its processor itself).
fn cpu_map_blob(cores: u32, threads: u32) -> Vec<u8> {
    with_cpus(|builder| {
        builder.begin("cpu-map");
        builder.begin("cluster0");
        for core in 0..cores {
            builder.begin(&std::format!("core{core}"));
            if threads == 0 {
                builder.prop_u32("cpu", core + 1);
            }
            for thread in 0..threads {
                builder.begin(&std::format!("thread{thread}"));
                builder.prop_u32("cpu", core * threads + thread + 1);
                builder.end();
            }
            builder.end();
        }
        builder.end();
        builder.end();
        cpu_node(builder, "cpu@0", Some(0), &[]);
    })
}

/// The map of `blob`, `None` where the test's tree does not parse.
fn cpu_map_of(blob: &[u8]) -> Option<Result<Option<CpuMap>, CpuMapError>> {
    Fdt::parse(blob).ok().map(|tree| tree.cpu_map())
}

#[test]
fn a_cpu_map_with_two_threads_a_core_says_so() {
    assert_eq!(
        cpu_map_of(&cpu_map_blob(2, 2)),
        Some(Ok(Some(CpuMap {
            cores: 2,
            threads: 4,
            most_a_core: 2
        })))
    );
}

#[test]
fn a_cpu_map_of_cores_alone_names_no_thread() {
    assert_eq!(
        cpu_map_of(&cpu_map_blob(4, 0)),
        Some(Ok(Some(CpuMap {
            cores: 4,
            threads: 0,
            most_a_core: 0
        })))
    );
}

#[test]
fn a_tree_without_a_cpu_map_has_none() {
    let blob = with_cpus(|builder| cpu_node(builder, "cpu@0", Some(0), &[]));
    assert_eq!(cpu_map_of(&blob), Some(Ok(None)));
}

#[test]
fn a_thread_outside_a_core_is_refused() {
    let blob = with_cpus(|builder| {
        builder.begin("cpu-map");
        builder.begin("cluster0");
        builder.begin("thread0");
        builder.prop_u32("cpu", 1);
        builder.end();
        builder.end();
        builder.end();
    });
    assert_eq!(cpu_map_of(&blob), Some(Err(CpuMapError::ThreadOutsideCore)));
}

#[test]
fn a_thread_below_a_cores_child_is_refused() {
    let blob = with_cpus(|builder| {
        builder.begin("cpu-map");
        builder.begin("core0");
        builder.begin("cluster0");
        builder.begin("thread0");
        builder.prop_u32("cpu", 1);
        builder.end();
        builder.end();
        builder.end();
        builder.end();
    });
    assert_eq!(cpu_map_of(&blob), Some(Err(CpuMapError::ThreadOutsideCore)));
}

#[test]
fn a_thread_naming_no_processor_is_refused() {
    let blob = with_cpus(|builder| {
        builder.begin("cpu-map");
        builder.begin("cluster0");
        builder.begin("core0");
        builder.begin("thread0");
        builder.end();
        builder.end();
        builder.end();
        builder.end();
    });
    assert_eq!(cpu_map_of(&blob), Some(Err(CpuMapError::ThreadWithoutCpu)));
}

#[test]
fn threads_of_the_next_core_are_counted_apart() {
    let blob = with_cpus(|builder| {
        builder.begin("cpu-map");
        builder.begin("cluster0");
        for core in 0..2 {
            builder.begin(&std::format!("core{core}"));
            builder.begin("thread0");
            builder.prop_u32("cpu", core + 1);
            builder.end();
            builder.end();
        }
        builder.end();
        builder.end();
    });
    assert_eq!(
        cpu_map_of(&blob),
        Some(Ok(Some(CpuMap {
            cores: 2,
            threads: 2,
            most_a_core: 1
        })))
    );
}
