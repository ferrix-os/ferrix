//! Tests for the ACPI table parser.
//!
//! Tables are built field by field rather than checked in as blobs, so a
//! failure names the field that is wrong, and so the malformed cases can be
//! produced by mutating one byte of a known-good table. The two fixtures are
//! shaped like the machines this kernel is actually brought up on: QEMU's `q35`
//! for x86-64 and QEMU's `virt` for AArch64.

extern crate std;

use std::collections::BTreeMap;
use std::vec;
use std::vec::Vec;

use super::*;

// ---------------------------------------------------------------------------
// Physical memory, as a map
// ---------------------------------------------------------------------------

/// Tables at the physical addresses a machine put them at.
#[derive(Clone, Debug, Default)]
struct Memory {
    tables: BTreeMap<u64, Vec<u8>>,
}

impl Memory {
    fn new() -> Self {
        Memory::default()
    }

    fn put(&mut self, physical_address: u64, bytes: Vec<u8>) {
        let _ = self.tables.insert(physical_address, bytes);
    }

    fn get_mut(&mut self, physical_address: u64) -> &mut Vec<u8> {
        self.tables.get_mut(&physical_address).unwrap()
    }
}

impl Tables for Memory {
    fn table(&self, physical_address: u64, len: usize) -> Option<&[u8]> {
        self.tables.get(&physical_address)?.get(..len)
    }
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// A system description table under construction. `build` fills in the length
/// and the checksum, so every table a test produces is well formed unless the
/// test deliberately breaks it afterwards.
struct TableBuilder {
    signature: [u8; 4],
    revision: u8,
    body: Vec<u8>,
}

impl TableBuilder {
    fn new(signature: [u8; 4]) -> Self {
        TableBuilder {
            signature,
            revision: 1,
            body: Vec::new(),
        }
    }

    fn u32(mut self, value: u32) -> Self {
        self.body.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn u64(mut self, value: u64) -> Self {
        self.body.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn raw(mut self, bytes: &[u8]) -> Self {
        self.body.extend_from_slice(bytes);
        self
    }

    fn build(self) -> Vec<u8> {
        let mut table = vec![0u8; SDT_HEADER_LEN];
        table[0..4].copy_from_slice(&self.signature);
        table[8] = self.revision;
        table[10..16].copy_from_slice(b"FERRIX");
        table[16..24].copy_from_slice(b"FERRIXOS");
        table[24..28].copy_from_slice(&1u32.to_le_bytes());
        table[28..32].copy_from_slice(b"FRRX");
        table[32..36].copy_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&self.body);

        let length = table.len() as u32;
        table[4..8].copy_from_slice(&length.to_le_bytes());
        table[9] = 0;
        let sum = table.iter().copied().fold(0u8, u8::wrapping_add);
        table[9] = 0u8.wrapping_sub(sum);
        table
    }
}

/// An RSDP with correct checksums for its revision.
fn build_rsdp(revision: u8, rsdt_address: u32, xsdt_address: u64) -> Vec<u8> {
    let len = if revision >= 2 {
        RSDP_V2_LEN
    } else {
        RSDP_V1_LEN
    };
    let mut rsdp = vec![0u8; len];
    rsdp[0..8].copy_from_slice(&RSDP_SIGNATURE);
    rsdp[9..15].copy_from_slice(b"FERRIX");
    rsdp[15] = revision;
    rsdp[16..20].copy_from_slice(&rsdt_address.to_le_bytes());
    if revision >= 2 {
        rsdp[20..24].copy_from_slice(&(RSDP_V2_LEN as u32).to_le_bytes());
        rsdp[24..32].copy_from_slice(&xsdt_address.to_le_bytes());
    }

    let sum = rsdp[..RSDP_V1_LEN]
        .iter()
        .copied()
        .fold(0u8, u8::wrapping_add);
    rsdp[8] = 0u8.wrapping_sub(sum);
    if revision >= 2 {
        let sum = rsdp.iter().copied().fold(0u8, u8::wrapping_add);
        rsdp[32] = 0u8.wrapping_sub(sum);
    }
    rsdp
}

/// A MADT entry: the two header bytes plus `payload`.
fn entry(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![kind, (payload.len() + 2) as u8];
    bytes.extend_from_slice(payload);
    bytes
}

fn local_apic_entry(processor_uid: u8, apic_id: u8, flags: u32) -> Vec<u8> {
    let mut payload = vec![processor_uid, apic_id];
    payload.extend_from_slice(&flags.to_le_bytes());
    entry(0, &payload)
}

fn io_apic_entry(id: u8, address: u32, gsi_base: u32) -> Vec<u8> {
    let mut payload = vec![id, 0];
    payload.extend_from_slice(&address.to_le_bytes());
    payload.extend_from_slice(&gsi_base.to_le_bytes());
    entry(1, &payload)
}

fn source_override_entry(source: u8, gsi: u32, flags: u16) -> Vec<u8> {
    let mut payload = vec![0, source];
    payload.extend_from_slice(&gsi.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    entry(2, &payload)
}

fn local_apic_nmi_entry(processor_uid: u8, flags: u16, lint: u8) -> Vec<u8> {
    let mut payload = vec![processor_uid];
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.push(lint);
    entry(4, &payload)
}

fn address_override_entry(address: u64) -> Vec<u8> {
    let mut payload = vec![0, 0];
    payload.extend_from_slice(&address.to_le_bytes());
    entry(5, &payload)
}

fn x2apic_entry(x2apic_id: u32, flags: u32, processor_uid: u32) -> Vec<u8> {
    let mut payload = vec![0, 0];
    payload.extend_from_slice(&x2apic_id.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(&processor_uid.to_le_bytes());
    entry(9, &payload)
}

fn gicc_entry(cpu_interface: u32, uid: u32, mpidr: u64, gicr_base: u64, flags: u32) -> Vec<u8> {
    let mut payload = vec![0, 0];
    payload.extend_from_slice(&cpu_interface.to_le_bytes());
    payload.extend_from_slice(&uid.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // parking protocol version
    payload.extend_from_slice(&23u32.to_le_bytes()); // performance interrupt
    payload.extend_from_slice(&0u64.to_le_bytes()); // parked address
    payload.extend_from_slice(&0u64.to_le_bytes()); // GICv2 CPU interface
    payload.extend_from_slice(&0u64.to_le_bytes()); // GICV
    payload.extend_from_slice(&0u64.to_le_bytes()); // GICH
    payload.extend_from_slice(&25u32.to_le_bytes()); // VGIC maintenance
    payload.extend_from_slice(&gicr_base.to_le_bytes());
    payload.extend_from_slice(&mpidr.to_le_bytes());
    payload.push(0); // power efficiency class
    payload.push(0); // reserved
    payload.extend_from_slice(&0u16.to_le_bytes()); // SPE overflow interrupt
    entry(11, &payload)
}

fn gicd_entry(gic_id: u32, base: u64, version: u8) -> Vec<u8> {
    let mut payload = vec![0, 0];
    payload.extend_from_slice(&gic_id.to_le_bytes());
    payload.extend_from_slice(&base.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // system vector base
    payload.push(version);
    payload.extend_from_slice(&[0, 0, 0]); // reserved
    entry(12, &payload)
}

fn gicr_entry(base: u64, length: u32) -> Vec<u8> {
    let mut payload = vec![0, 0];
    payload.extend_from_slice(&base.to_le_bytes());
    payload.extend_from_slice(&length.to_le_bytes());
    entry(14, &payload)
}

fn madt(local_address: u32, flags: u32, entries: &[Vec<u8>]) -> Vec<u8> {
    let mut builder = TableBuilder::new(MADT_SIGNATURE)
        .u32(local_address)
        .u32(flags);
    for bytes in entries {
        builder = builder.raw(bytes);
    }
    builder.build()
}

fn xsdt(addresses: &[u64]) -> Vec<u8> {
    let mut builder = TableBuilder::new(XSDT_SIGNATURE);
    for address in addresses {
        builder = builder.u64(*address);
    }
    builder.build()
}

fn rsdt(addresses: &[u32]) -> Vec<u8> {
    let mut builder = TableBuilder::new(RSDT_SIGNATURE);
    for address in addresses {
        builder = builder.u32(*address);
    }
    builder.build()
}

/// Bytes of a complete ACPI 6.x FADT.
const FADT_LEN: usize = 276;

fn put_u8(body: &mut [u8], absolute_offset: usize, value: u8) {
    body[absolute_offset - SDT_HEADER_LEN] = value;
}

fn put_u16(body: &mut [u8], absolute_offset: usize, value: u16) {
    let at = absolute_offset - SDT_HEADER_LEN;
    body[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(body: &mut [u8], absolute_offset: usize, value: u32) {
    let at = absolute_offset - SDT_HEADER_LEN;
    body[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(body: &mut [u8], absolute_offset: usize, value: u64) {
    let at = absolute_offset - SDT_HEADER_LEN;
    body[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A q35-shaped FADT: PM timer on port 0x608, century byte at CMOS offset 0x32.
fn fadt() -> Vec<u8> {
    let mut body = vec![0u8; FADT_LEN - SDT_HEADER_LEN];
    put_u32(&mut body, 36, 0x7FFF_4000); // FIRMWARE_CTRL
    put_u32(&mut body, 40, 0x7FFF_5000); // DSDT
    put_u32(&mut body, 76, 0x0000_0608); // PM_TMR_BLK
    put_u8(&mut body, 91, 4); // PM_TMR_LEN
    put_u8(&mut body, 108, 0x32); // CENTURY
    put_u16(&mut body, 109, 0b11); // IAPC_BOOT_ARCH
    put_u32(&mut body, 112, 1 << 8); // Flags: TMR_VAL_EXT
    put_u64(&mut body, 140, 0x0000_0000_7FFF_5000); // X_DSDT
    put_u8(&mut body, 208, GenericAddress::SYSTEM_IO); // X_PM_TMR_BLK
    put_u8(&mut body, 209, 32);
    put_u8(&mut body, 211, 3);
    put_u64(&mut body, 212, 0x608);
    TableBuilder::new(FADT_SIGNATURE).raw(&body).build()
}

/// A `virt`-shaped GTDT: the non-secure EL1 timer on PPI 30, the virtual timer
/// on PPI 27, both level triggered.
fn gtdt() -> Vec<u8> {
    TableBuilder::new(GTDT_SIGNATURE)
        .u64(GTDT_NO_COUNTER_BASE)
        .u32(0) // reserved
        .u32(29) // secure EL1 GSIV
        .u32(GTDT_FLAG_ACTIVE_LOW)
        .u32(30) // non-secure EL1 GSIV
        .u32(GTDT_FLAG_ACTIVE_LOW)
        .u32(27) // virtual timer GSIV
        .u32(GTDT_FLAG_ACTIVE_LOW)
        .u32(26) // non-secure EL2 GSIV
        .u32(GTDT_FLAG_ACTIVE_LOW)
        .u64(GTDT_NO_COUNTER_BASE)
        .u32(0) // platform timer count
        .u32(0) // platform timer offset
        .build()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const RSDP_ADDR: u64 = 0x000F_0000;
const XSDT_ADDR: u64 = 0x7FFF_0000;
const MADT_ADDR: u64 = 0x7FFF_1000;
const FADT_ADDR: u64 = 0x7FFF_2000;
const GTDT_ADDR: u64 = 0x7FFF_3000;
const HPET_ADDR: u64 = 0x7FFF_6000;

/// The x86-64 machine: revision-2 RSDP, XSDT, a four-processor MADT with one
/// I/O APIC and the two overrides QEMU emits.
fn q35() -> Memory {
    let entries = vec![
        local_apic_entry(0, 0, LOCAL_APIC_ENABLED),
        local_apic_entry(1, 1, LOCAL_APIC_ENABLED),
        local_apic_entry(2, 2, LOCAL_APIC_ENABLED),
        local_apic_entry(3, 3, LOCAL_APIC_ENABLED),
        io_apic_entry(0, 0xFEC0_0000, 0),
        // The one every PC has: the PIT arrives as GSI 2, not GSI 0.
        source_override_entry(0, 2, 0),
        source_override_entry(5, 5, 0b1101),
        local_apic_nmi_entry(0xFF, 0, 1),
    ];

    let mut memory = Memory::new();
    memory.put(RSDP_ADDR, build_rsdp(2, 0, XSDT_ADDR));
    memory.put(XSDT_ADDR, xsdt(&[MADT_ADDR, FADT_ADDR]));
    memory.put(
        MADT_ADDR,
        madt(0xFEE0_0000, MADT_FLAG_PCAT_COMPAT, &entries),
    );
    memory.put(FADT_ADDR, fadt());
    memory
}

/// The AArch64 machine: a GICv3 distributor, four CPU interfaces, one
/// redistributor discovery range, and a GTDT.
fn virt() -> Memory {
    let mut entries = vec![gicd_entry(0, 0x0800_0000, 3)];
    for cpu in 0..4u32 {
        entries.push(gicc_entry(
            cpu,
            cpu,
            u64::from(cpu),
            0x080A_0000 + u64::from(cpu) * 0x2_0000,
            GICC_FLAG_ENABLED,
        ));
    }
    entries.push(gicr_entry(0x080A_0000, 0x00F6_0000));

    let mut memory = Memory::new();
    memory.put(RSDP_ADDR, build_rsdp(2, 0, XSDT_ADDR));
    memory.put(XSDT_ADDR, xsdt(&[MADT_ADDR, GTDT_ADDR]));
    memory.put(MADT_ADDR, madt(0, 0, &entries));
    memory.put(GTDT_ADDR, gtdt());
    memory
}

// ---------------------------------------------------------------------------
// RSDP
// ---------------------------------------------------------------------------

#[test]
fn parses_a_revision_two_rsdp() {
    let bytes = build_rsdp(2, 0x000E_0000, XSDT_ADDR);
    let rsdp = Rsdp::parse(&bytes).unwrap();

    assert_eq!(rsdp.revision, 2, "the builder wrote revision 2");
    assert_eq!(
        rsdp.length, RSDP_V2_LEN as u32,
        "a revision-2 RSDP is 36 bytes"
    );
    assert_eq!(
        rsdp.xsdt_address, XSDT_ADDR,
        "the XSDT address must survive"
    );
    assert_eq!(
        rsdp.rsdt_address, 0x000E_0000,
        "the RSDT address must survive"
    );
    assert_eq!(&rsdp.oem_id, b"FERRIX", "the OEM id must survive");
    assert!(rsdp.is_valid(), "a freshly built RSDP must checksum");
    assert_eq!(
        rsdp.root_table().unwrap(),
        (XSDT_ADDR, RootKind::Xsdt),
        "with both present the XSDT wins"
    );
}

#[test]
fn a_revision_zero_rsdp_has_only_an_rsdt() {
    let bytes = build_rsdp(0, 0x000E_0000, 0);
    let rsdp = Rsdp::parse(&bytes).unwrap();

    assert_eq!(
        rsdp.length, RSDP_V1_LEN as u32,
        "revision 0 has no length field"
    );
    assert_eq!(rsdp.xsdt_address, 0, "revision 0 has no XSDT field to read");
    assert!(
        rsdp.extended_checksum_valid,
        "revision 0 has no extended checksum to fail"
    );
    assert_eq!(
        rsdp.root_table().unwrap(),
        (0x000E_0000, RootKind::Rsdt),
        "without an XSDT the RSDT is the root"
    );
}

#[test]
fn a_bad_rsdp_checksum_is_reported_not_refused() {
    let mut bytes = build_rsdp(2, 0, XSDT_ADDR);
    bytes[8] = bytes[8].wrapping_add(1);
    let rsdp = Rsdp::parse(&bytes).unwrap();

    assert!(
        !rsdp.checksum_valid,
        "the version-1 checksum must be seen to fail"
    );
    assert!(!rsdp.is_valid(), "an invalid part makes the whole invalid");
    assert_eq!(
        rsdp.xsdt_address, XSDT_ADDR,
        "the fields are still reported so the caller can proceed anyway"
    );
}

#[test]
fn a_bad_extended_rsdp_checksum_is_reported_on_its_own() {
    let mut bytes = build_rsdp(2, 0, XSDT_ADDR);
    bytes[32] = bytes[32].wrapping_add(1);
    let rsdp = Rsdp::parse(&bytes).unwrap();

    assert!(
        rsdp.checksum_valid,
        "the first twenty bytes are untouched and must still pass"
    );
    assert!(
        !rsdp.extended_checksum_valid,
        "the checksum over all 36 bytes must fail"
    );
}

#[test]
fn rejects_bytes_that_are_not_an_rsdp() {
    let mut bytes = build_rsdp(2, 0, XSDT_ADDR);
    bytes[4] = b'X';
    assert_eq!(
        Rsdp::parse(&bytes),
        Err(AcpiError::BadRsdpSignature),
        "a wrong signature is a hard error, not a checksum note"
    );
}

#[test]
fn rejects_an_rsdp_that_is_too_short() {
    let bytes = build_rsdp(2, 0, XSDT_ADDR);
    for length in [0usize, 8, 19] {
        assert!(
            Rsdp::parse(&bytes[..length]).is_err(),
            "{length} bytes cannot hold even an ACPI 1.0 RSDP"
        );
    }
    assert!(
        Rsdp::parse(&bytes[..RSDP_V1_LEN]).is_err(),
        "a revision-2 RSDP truncated to 20 bytes has no XSDT to read"
    );
}

#[test]
fn an_rsdp_naming_no_table_at_all_is_an_error() {
    let bytes = build_rsdp(2, 0, 0);
    let rsdp = Rsdp::parse(&bytes).unwrap();
    assert_eq!(
        rsdp.root_table(),
        Err(AcpiError::NoRootTable),
        "an RSDP with both addresses zero leads nowhere"
    );
}

#[test]
fn reads_the_rsdp_through_the_tables_trait() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    assert_eq!(
        rsdp.xsdt_address, XSDT_ADDR,
        "reading must agree with parsing"
    );
    assert_eq!(
        Rsdp::read(&memory, 0xDEAD_0000),
        Err(AcpiError::Unreadable(0xDEAD_0000)),
        "an address the caller cannot map must be reported as such"
    );
}

// ---------------------------------------------------------------------------
// Headers, checksums and extents
// ---------------------------------------------------------------------------

#[test]
fn parses_an_sdt_header() {
    let bytes = madt(0xFEE0_0000, 0, &[]);
    let header = SdtHeader::parse(&bytes).unwrap();

    assert_eq!(
        header.signature, MADT_SIGNATURE,
        "the signature must survive"
    );
    assert_eq!(
        header.length as usize,
        bytes.len(),
        "the builder's length must cover the whole table"
    );
    assert_eq!(&header.oem_id, b"FERRIX", "the OEM id must survive");
    assert_eq!(
        &header.oem_table_id, b"FERRIXOS",
        "the OEM table id must survive"
    );
    assert_eq!(header.oem_revision, 1, "the OEM revision must survive");
    assert_eq!(
        header.creator_revision, 1,
        "the creator revision must survive"
    );
}

#[test]
fn a_bad_sdt_checksum_is_reported_not_refused() {
    let mut bytes = madt(0xFEE0_0000, 0, &[local_apic_entry(0, 0, 1)]);
    bytes[9] = bytes[9].wrapping_add(1);
    let table = Table::parse(&bytes).unwrap();

    assert!(
        !table.checksum_valid(),
        "the corrupted checksum must be seen to fail"
    );
    let parsed = Madt::parse(table).unwrap();
    assert_eq!(
        parsed.entries().count(),
        1,
        "a bad checksum must not stop the table being read"
    );
}

#[test]
fn rejects_a_table_shorter_than_its_header() {
    let bytes = madt(0, 0, &[]);
    for length in [0usize, 1, 35] {
        assert_eq!(
            Table::parse(&bytes[..length]),
            Err(AcpiError::TooShort {
                got: length,
                need: SDT_HEADER_LEN
            }),
            "{length} bytes cannot hold a 36-byte SDT header"
        );
    }
}

#[test]
fn rejects_a_length_field_below_the_header() {
    let mut bytes = madt(0, 0, &[]);
    bytes[4..8].copy_from_slice(&12u32.to_le_bytes());
    assert_eq!(
        Table::parse(&bytes),
        Err(AcpiError::LengthTooSmall {
            declared: 12,
            need: SDT_HEADER_LEN
        }),
        "a table cannot be shorter than the header that declares it"
    );
}

#[test]
fn rejects_a_length_field_past_the_bytes_supplied() {
    let mut bytes = madt(0, 0, &[]);
    let available = bytes.len();
    bytes[4..8].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
    assert_eq!(
        Table::parse(&bytes),
        Err(AcpiError::LengthTooLarge {
            declared: 0xFFFF_0000,
            available
        }),
        "a length past the end of memory supplied must not be trusted"
    );
}

#[test]
fn a_table_is_cut_to_its_declared_length() {
    let mut bytes = madt(0xFEE0_0000, 0, &[local_apic_entry(0, 0, 1)]);
    let declared = bytes.len();
    bytes.extend_from_slice(&[0xCC; 64]);
    let table = Table::parse(&bytes).unwrap();

    assert_eq!(
        table.bytes().len(),
        declared,
        "bytes beyond the declared length belong to whatever follows the table"
    );
    assert_eq!(
        table.body().len(),
        declared - SDT_HEADER_LEN,
        "the body is everything after the header and nothing else"
    );
}

#[test]
fn a_wrong_signature_is_rejected_by_the_typed_parsers() {
    let bytes = madt(0, 0, &[]);
    let table = Table::parse(&bytes).unwrap();
    assert_eq!(
        Fadt::parse(table),
        Err(AcpiError::BadSignature(MADT_SIGNATURE)),
        "a MADT must not be read as an FADT"
    );
    assert_eq!(
        RootTable::parse(table, RootKind::Xsdt),
        Err(AcpiError::BadSignature(MADT_SIGNATURE)),
        "following 64-bit entries out of a MADT would produce nonsense addresses"
    );
}

#[test]
fn reading_an_unmapped_address_is_an_error() {
    let memory = q35();
    assert_eq!(
        read_table(&memory, 0x1234_0000),
        Err(AcpiError::Unreadable(0x1234_0000)),
        "a table nobody supplied cannot be read"
    );
}

// ---------------------------------------------------------------------------
// Root tables
// ---------------------------------------------------------------------------

#[test]
fn walks_a_q35_machine_from_the_rsdp_to_the_madt() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let root = acpi.root().unwrap();

    assert_eq!(
        root.kind(),
        RootKind::Xsdt,
        "a revision-2 RSDP names an XSDT"
    );
    assert_eq!(root.len(), 2, "the fixture lists the MADT and the FADT");
    assert!(
        !root.is_empty(),
        "a root table with two entries is not empty"
    );
    assert_eq!(root.entry(0), Some(MADT_ADDR), "the MADT is listed first");
    assert_eq!(root.entry(1), Some(FADT_ADDR), "the FADT is listed second");
    assert_eq!(root.entry(2), None, "there is no third entry");
    assert!(
        root.table().checksum_valid(),
        "the built XSDT must checksum"
    );

    let table = acpi.find(MADT_SIGNATURE).unwrap();
    assert_eq!(
        table.signature(),
        MADT_SIGNATURE,
        "find must return the table it was asked for"
    );
}

#[test]
fn walks_a_thirty_two_bit_rsdt() {
    let mut memory = Memory::new();
    memory.put(RSDP_ADDR, build_rsdp(0, XSDT_ADDR as u32, 0));
    memory.put(XSDT_ADDR, rsdt(&[MADT_ADDR as u32]));
    memory.put(
        MADT_ADDR,
        madt(0xFEE0_0000, 0, &[local_apic_entry(0, 0, 1)]),
    );

    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let root = acpi.root().unwrap();

    assert_eq!(root.kind(), RootKind::Rsdt, "revision 0 means an RSDT");
    assert_eq!(root.len(), 1, "the RSDT lists one table");
    assert_eq!(
        root.entry(0),
        Some(MADT_ADDR),
        "a 32-bit entry must widen to the same address"
    );
    assert!(
        acpi.madt().is_ok(),
        "the MADT must be reachable through an RSDT"
    );
}

#[test]
fn a_missing_signature_is_reported_as_not_found() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();

    assert_eq!(
        acpi.find(HPET_SIGNATURE).err(),
        Some(AcpiError::NotFound(HPET_SIGNATURE)),
        "the q35 fixture has no HPET table"
    );
    assert_eq!(
        acpi.gtdt().err(),
        Some(AcpiError::NotFound(GTDT_SIGNATURE)),
        "an x86-64 machine has no generic timer table"
    );
}

#[test]
fn an_entry_the_caller_cannot_supply_is_skipped() {
    let mut memory = q35();
    // An OEM table listed before the MADT that is outside the direct map: the
    // MADT after it must still be found.
    memory.put(
        XSDT_ADDR,
        xsdt(&[0xFFFF_FFFF_0000_0000, MADT_ADDR, FADT_ADDR]),
    );

    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();

    assert_eq!(
        acpi.root().unwrap().len(),
        3,
        "the unreadable entry is still listed"
    );
    assert!(
        acpi.madt().is_ok(),
        "one unreadable table must not hide the ones after it"
    );
    assert_eq!(
        acpi.find(HPET_SIGNATURE).err(),
        Some(AcpiError::NotFound(HPET_SIGNATURE)),
        "an unreadable entry is not a table with the wanted signature either"
    );
}

#[test]
fn a_trailing_partial_entry_in_a_root_table_is_ignored() {
    let mut memory = q35();
    let mut bytes = xsdt(&[MADT_ADDR]);
    bytes.extend_from_slice(&[0, 0, 0]); // three bytes of a fourth address
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    memory.put(XSDT_ADDR, bytes);

    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let root = acpi.root().unwrap();

    assert_eq!(root.len(), 1, "a partial address is not an address");
    assert_eq!(
        root.entries().count(),
        1,
        "the iterator must agree with len"
    );
}

// ---------------------------------------------------------------------------
// MADT
// ---------------------------------------------------------------------------

#[test]
fn decodes_the_q35_madt() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let madt = acpi.madt().unwrap();

    assert_eq!(
        madt.local_controller_address(),
        0xFEE0_0000,
        "the local APIC sits where the fixture put it"
    );
    assert!(madt.has_legacy_pics(), "q35 has a pair of 8259s to mask");
    madt.check_entries().unwrap();

    let entries: Vec<MadtEntry> = madt.entries().collect();
    assert_eq!(
        entries.len(),
        8,
        "the fixture has eight controller structures"
    );

    let cpus: Vec<LocalApic> = entries
        .iter()
        .filter_map(|entry| match entry {
            MadtEntry::LocalApic(cpu) => Some(*cpu),
            _ => None,
        })
        .collect();
    assert_eq!(cpus.len(), 4, "the fixture is a four-processor machine");
    assert_eq!(cpus[3].apic_id, 3, "the fourth processor has APIC id 3");
    assert_eq!(cpus[3].processor_uid, 3, "its AML identifier matches");
    assert!(cpus[3].is_enabled(), "all four processors are usable now");
    assert!(
        !cpus[3].is_online_capable(),
        "none of them are hot-plug placeholders"
    );
}

#[test]
fn decodes_the_io_apic_and_its_overrides() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let madt = acpi.madt().unwrap();

    let io_apics: Vec<IoApic> = madt
        .entries()
        .filter_map(|entry| match entry {
            MadtEntry::IoApic(io_apic) => Some(io_apic),
            _ => None,
        })
        .collect();
    assert_eq!(io_apics.len(), 1, "q35 has one I/O APIC");
    assert_eq!(io_apics[0].address, 0xFEC0_0000, "at the usual address");
    assert_eq!(io_apics[0].gsi_base, 0, "covering GSIs from zero");

    let overrides: Vec<InterruptSourceOverride> = madt
        .entries()
        .filter_map(|entry| match entry {
            MadtEntry::InterruptSourceOverride(iso) => Some(iso),
            _ => None,
        })
        .collect();
    assert_eq!(overrides.len(), 2, "the fixture has two source overrides");
    assert_eq!(overrides[0].source, 0, "ISA IRQ 0 is the timer");
    assert_eq!(overrides[0].gsi, 2, "which arrives as GSI 2");
    assert_eq!(
        overrides[0].polarity(),
        Polarity::BusDefault,
        "zero flags mean the bus decides"
    );
    assert_eq!(
        overrides[0].trigger_mode(),
        TriggerMode::BusDefault,
        "zero flags mean the bus decides the trigger mode too"
    );
    assert_eq!(
        overrides[1].polarity(),
        Polarity::ActiveHigh,
        "flag bits 0b01 in the low pair are active high"
    );
    assert_eq!(
        overrides[1].trigger_mode(),
        TriggerMode::Level,
        "flag bits 0b11 in the high pair are level triggered"
    );
}

#[test]
fn decodes_a_local_apic_nmi() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let nmi = acpi
        .madt()
        .unwrap()
        .entries()
        .find_map(|entry| match entry {
            MadtEntry::LocalApicNmi(nmi) => Some(nmi),
            _ => None,
        })
        .unwrap();

    assert_eq!(nmi.processor_uid, 0xFF, "0xFF means every processor");
    assert_eq!(nmi.lint, 1, "the NMI is wired to LINT1, as on every PC");
}

#[test]
fn decodes_an_x2apic_entry() {
    let bytes = madt(
        0xFEE0_0000,
        0,
        &[x2apic_entry(1024, LOCAL_APIC_ONLINE_CAPABLE, 7)],
    );
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();
    let entry = parsed.entries().next().unwrap();

    match entry {
        MadtEntry::LocalX2Apic(x2apic) => {
            assert_eq!(
                x2apic.x2apic_id, 1024,
                "an id above 255 needs the 32-bit field"
            );
            assert_eq!(x2apic.processor_uid, 7, "the AML identifier must survive");
            assert!(!x2apic.is_enabled(), "this processor is not usable yet");
            assert!(
                x2apic.is_online_capable(),
                "but it is declared as hot-pluggable"
            );
        }
        other => panic!("expected a local x2APIC entry, got {other:?}"),
    }
}

#[test]
fn a_local_apic_address_override_supersedes_the_header() {
    let bytes = madt(
        0xFEE0_0000,
        0,
        &[
            local_apic_entry(0, 0, LOCAL_APIC_ENABLED),
            address_override_entry(0x0000_0010_FEE0_0000),
        ],
    );
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(
        parsed.local_controller_address(),
        0xFEE0_0000,
        "the 32-bit header field is unchanged"
    );
    assert_eq!(
        parsed.local_apic_address(),
        0x0000_0010_FEE0_0000,
        "a type 5 entry moves the local APIC above 4 GiB"
    );
}

#[test]
fn without_an_override_the_header_address_stands() {
    let bytes = madt(0xFEE0_0000, 0, &[local_apic_entry(0, 0, 1)]);
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();
    assert_eq!(
        parsed.local_apic_address(),
        0xFEE0_0000,
        "the 32-bit field is the answer when nothing overrides it"
    );
}

#[test]
fn an_unknown_entry_type_is_skipped_by_its_length() {
    // Type 0x7F is in the OEM-reserved range; a parser that guessed at its
    // size would lose the local APIC that follows it.
    let bytes = madt(
        0xFEE0_0000,
        0,
        &[
            entry(0x7F, &[0xAA; 30]),
            local_apic_entry(9, 9, LOCAL_APIC_ENABLED),
        ],
    );
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();
    let entries: Vec<MadtEntry> = parsed.entries().collect();

    assert_eq!(
        entries.len(),
        2,
        "iteration must continue past an unknown type"
    );
    assert_eq!(
        entries[0],
        MadtEntry::Unknown {
            kind: 0x7F,
            length: 32
        },
        "an undecoded entry still reports its type and length"
    );
    assert!(
        matches!(entries[1], MadtEntry::LocalApic(cpu) if cpu.apic_id == 9),
        "the entry after the unknown one must be found intact"
    );
    parsed.check_entries().unwrap();
}

#[test]
fn a_zero_length_entry_terminates_instead_of_looping() {
    // The classic MADT parser bug. If this test hangs rather than fails, the
    // cursor is being advanced by the entry's own length without a floor.
    let bytes = madt(
        0xFEE0_0000,
        0,
        &[
            local_apic_entry(0, 0, LOCAL_APIC_ENABLED),
            vec![0u8, 0u8], // type 0, length 0
            local_apic_entry(1, 1, LOCAL_APIC_ENABLED),
        ],
    );
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();
    let entries: Vec<MadtEntry> = parsed.entries().collect();

    assert_eq!(entries.len(), 1, "the walk stops at the zero-length entry");
    assert_eq!(
        parsed.check_entries(),
        Err(AcpiError::ZeroLengthEntry { offset: 8 }),
        "and the reason is reported at the offset it happened"
    );
}

#[test]
fn an_entry_of_length_one_also_terminates() {
    let bytes = madt(0xFEE0_0000, 0, &[vec![0u8, 1u8, 0u8]]);
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(
        parsed.entries().count(),
        0,
        "length 1 is below the two header bytes and cannot advance the cursor"
    );
    assert_eq!(
        parsed.check_entries(),
        Err(AcpiError::ZeroLengthEntry { offset: 0 }),
        "a length of one is as unusable as a length of zero"
    );
}

#[test]
fn an_entry_running_past_the_table_stops_the_walk() {
    let mut bytes = madt(
        0xFEE0_0000,
        0,
        &[
            local_apic_entry(0, 0, LOCAL_APIC_ENABLED),
            local_apic_entry(1, 1, LOCAL_APIC_ENABLED),
        ],
    );
    // Cut the table short in the middle of the second entry, the way a wrong
    // `length` field in the SDT header does.
    let truncated = bytes.len() - 4;
    bytes[4..8].copy_from_slice(&(truncated as u32).to_le_bytes());
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(
        parsed.entries().count(),
        1,
        "only the entry wholly inside the table may be reported"
    );
    assert_eq!(
        parsed.check_entries(),
        Err(AcpiError::EntryOutOfBounds { offset: 8 }),
        "the truncation is reported at the entry that runs off the end"
    );
}

#[test]
fn a_single_trailing_byte_is_a_truncated_entry() {
    let mut bytes = madt(0xFEE0_0000, 0, &[local_apic_entry(0, 0, 1)]);
    bytes.push(0);
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(parsed.entries().count(), 1, "one byte is not an entry");
    assert_eq!(
        parsed.check_entries(),
        Err(AcpiError::EntryOutOfBounds { offset: 8 }),
        "a stray byte after the last entry is a malformed table, not padding"
    );
}

#[test]
fn a_known_entry_type_that_is_too_short_is_reported_as_malformed() {
    // A type 0 entry needs eight bytes; this one declares four, so its flags
    // would come from the entry after it.
    let bytes = madt(
        0xFEE0_0000,
        0,
        &[vec![0u8, 4u8, 1u8, 1u8], local_apic_entry(2, 2, 1)],
    );
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();
    let entries: Vec<MadtEntry> = parsed.entries().collect();

    assert_eq!(
        entries[0],
        MadtEntry::Malformed { kind: 0, length: 4 },
        "a short entry of a known type must not be decoded from its neighbour"
    );
    assert_eq!(entries.len(), 2, "the walk carries on past it");
}

#[test]
fn the_entry_walk_is_bounded() {
    let filler = vec![entry(0x7F, &[]); MAX_MADT_ENTRIES + 100];
    let bytes = madt(0, 0, &filler);
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(
        parsed.entries().count(),
        MAX_MADT_ENTRIES,
        "the iterator must stop at the budget rather than run for an unbounded time"
    );
    assert_eq!(
        parsed.check_entries(),
        Err(AcpiError::TooManyEntries),
        "and the caller must be able to learn that the table was cut short"
    );
}

#[test]
fn a_madt_too_short_for_its_own_fixed_part_is_rejected() {
    let mut bytes = madt(0xFEE0_0000, 0, &[]);
    bytes.truncate(SDT_HEADER_LEN + 4);
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());

    assert_eq!(
        Madt::parse(Table::parse(&bytes).unwrap()),
        Err(AcpiError::TooShort {
            got: SDT_HEADER_LEN + 4,
            need: MADT_HEADER_LEN
        }),
        "a MADT without its flags word cannot be read"
    );
}

#[test]
fn an_empty_madt_has_no_entries() {
    let bytes = madt(0xFEE0_0000, 0, &[]);
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(
        parsed.entries().count(),
        0,
        "there is nothing after the header"
    );
    parsed.check_entries().unwrap();
}

// ---------------------------------------------------------------------------
// AArch64
// ---------------------------------------------------------------------------

#[test]
fn decodes_the_virt_madt() {
    let memory = virt();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let madt = acpi.madt().unwrap();
    madt.check_entries().unwrap();

    let entries: Vec<MadtEntry> = madt.entries().collect();
    assert_eq!(
        entries.len(),
        6,
        "a distributor, four CPUs and a redistributor"
    );

    let gicd = entries
        .iter()
        .find_map(|entry| match entry {
            MadtEntry::Gicd(gicd) => Some(*gicd),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        gicd.physical_base_address, 0x0800_0000,
        "the distributor sits where the fixture put it"
    );
    assert_eq!(gicd.gic_version, 3, "the fixture is a GICv3 machine");
}

#[test]
fn decodes_the_virt_cpu_interfaces() {
    let memory = virt();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let cpus: Vec<Gicc> = acpi
        .madt()
        .unwrap()
        .entries()
        .filter_map(|entry| match entry {
            MadtEntry::Gicc(cpu) => Some(cpu),
            _ => None,
        })
        .collect();

    assert_eq!(cpus.len(), 4, "the fixture is a four-processor machine");
    assert_eq!(
        cpus[2].acpi_processor_uid, 2,
        "the third CPU's AML identifier"
    );
    assert_eq!(cpus[2].cpu_interface_number, 2, "and its interface number");
    assert_eq!(cpus[2].mpidr, 2, "PSCI needs the MPIDR to start it");
    assert_eq!(
        cpus[2].gicr_base_address,
        0x080A_0000 + 2 * 0x2_0000,
        "each CPU has its own redistributor frame"
    );
    assert!(cpus[2].is_enabled(), "all four processors are usable now");
    assert_eq!(
        cpus[2].performance_interrupt_gsiv, 23,
        "the PMU interrupt is PPI 23 on this machine"
    );
}

#[test]
fn decodes_a_redistributor_discovery_range() {
    let memory = virt();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let gicr = acpi
        .madt()
        .unwrap()
        .entries()
        .find_map(|entry| match entry {
            MadtEntry::Gicr(gicr) => Some(gicr),
            _ => None,
        })
        .unwrap();

    assert_eq!(
        gicr.discovery_range_base, 0x080A_0000,
        "the range starts at the first redistributor"
    );
    assert_eq!(
        gicr.discovery_range_length, 0x00F6_0000,
        "and spans every frame the machine has"
    );
}

#[test]
fn decodes_a_gicc_from_firmware_predating_the_efficiency_class() {
    // ACPI 5.1 GICC entries are 76 bytes; 6.0 appended the power efficiency
    // class. Both have to parse, because both ship.
    let mut short = gicc_entry(0, 0, 0x8000_0000, 0, GICC_FLAG_ENABLED);
    short.truncate(76);
    short[1] = 76;
    let bytes = madt(0, 0, &[short]);
    let parsed = Madt::parse(Table::parse(&bytes).unwrap()).unwrap();

    match parsed.entries().next().unwrap() {
        MadtEntry::Gicc(cpu) => {
            assert_eq!(
                cpu.mpidr, 0x8000_0000,
                "the MPIDR is the last required field"
            );
            assert_eq!(
                cpu.power_efficiency_class, 0,
                "an absent efficiency class reads as the homogeneous case"
            );
        }
        other => panic!("expected a GICC entry, got {other:?}"),
    }
}

#[test]
fn decodes_the_generic_timers() {
    let memory = virt();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let gtdt = acpi.gtdt().unwrap();

    assert_eq!(
        gtdt.non_secure_el1_timer().gsiv,
        30,
        "the physical timer is PPI 30 on this machine"
    );
    assert_eq!(
        gtdt.virtual_el1_timer().gsiv,
        27,
        "and the virtual timer is PPI 27"
    );
    assert_eq!(
        gtdt.secure_el1_timer().gsiv,
        29,
        "the secure timer is PPI 29"
    );
    assert_eq!(
        gtdt.non_secure_el2_timer().gsiv,
        26,
        "the EL2 timer is PPI 26"
    );
    assert!(
        gtdt.virtual_el1_timer().is_active_low(),
        "the fixture declares all four active low"
    );
    assert!(
        !gtdt.virtual_el1_timer().is_edge_triggered(),
        "and level triggered, as the architecture requires"
    );
    assert!(
        gtdt.virtual_el1_timer().is_present(),
        "a non-zero vector means the timer is described"
    );
    assert_eq!(
        gtdt.counter_control_base(),
        None,
        "all ones means the counter block is not implemented"
    );
    assert_eq!(gtdt.counter_read_base(), None, "nor is the read block");
    assert_eq!(
        gtdt.platform_timer_count(),
        Some(0),
        "the fixture has no SBSA timers"
    );
    assert_eq!(
        gtdt.virtual_el2_timer(),
        None,
        "a 96-byte GTDT predates the virtual EL2 timer field"
    );
}

#[test]
fn a_gtdt_too_short_to_describe_its_timers_is_rejected() {
    let mut bytes = gtdt();
    bytes.truncate(SDT_HEADER_LEN + 20);
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());

    assert_eq!(
        Gtdt::parse(Table::parse(&bytes).unwrap()),
        Err(AcpiError::TooShort {
            got: SDT_HEADER_LEN + 20,
            need: 80
        }),
        "a GTDT that stops before the timer vectors has described nothing"
    );
}

// ---------------------------------------------------------------------------
// FADT
// ---------------------------------------------------------------------------

#[test]
fn decodes_the_fadt_timer_and_century_byte() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    let fadt = acpi.fadt().unwrap();

    assert_eq!(fadt.century(), Some(0x32), "the CMOS century byte offset");
    assert_eq!(fadt.pm_timer_block(), Some(0x608), "the PM timer I/O port");
    assert_eq!(
        fadt.pm_timer_length(),
        Some(4),
        "the block is four bytes wide"
    );
    assert_eq!(fadt.iapc_boot_arch(), Some(0b11), "the legacy device flags");
    assert_eq!(
        fadt.firmware_ctrl(),
        Some(0x7FFF_4000),
        "the FACS address must survive"
    );

    let timer = fadt.pm_timer().unwrap();
    assert_eq!(timer.port, 0x608, "the timer is where the fixed field says");
    assert!(
        timer.is_32_bit,
        "TMR_VAL_EXT is set, so the counter is 32 bits"
    );
    let extended = timer.extended.unwrap();
    assert_eq!(
        extended.address_space_id,
        GenericAddress::SYSTEM_IO,
        "the extended description agrees that this is an I/O port"
    );
    assert_eq!(extended.address, 0x608, "and names the same port");
}

#[test]
fn the_fadt_prefers_the_sixty_four_bit_dsdt_pointer() {
    let memory = q35();
    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();
    assert_eq!(
        acpi.fadt().unwrap().dsdt_address(),
        Some(0x7FFF_5000),
        "X_DSDT is filled in, so it wins over the 32-bit field"
    );

    let mut body = vec![0u8; FADT_LEN - SDT_HEADER_LEN];
    put_u32(&mut body, 40, 0x1234_5000);
    let bytes = TableBuilder::new(FADT_SIGNATURE).raw(&body).build();
    let fadt = Fadt::parse(Table::parse(&bytes).unwrap()).unwrap();
    assert_eq!(
        fadt.dsdt_address(),
        Some(0x1234_5000),
        "a zero X_DSDT falls back to the 32-bit field"
    );
}

#[test]
fn a_short_fadt_answers_none_rather_than_guessing() {
    // ACPI 1.0 FADTs stop at 116 bytes, before the century byte's neighbours
    // and long before the extended block descriptions.
    let bytes = TableBuilder::new(FADT_SIGNATURE)
        .raw(&[0u8; 100 - SDT_HEADER_LEN])
        .build();
    let fadt = Fadt::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(fadt.century(), None, "the century byte is past the end");
    assert_eq!(fadt.flags(), None, "so is the flags word");
    assert_eq!(
        fadt.x_pm_timer_block(),
        None,
        "and the extended timer block"
    );
    assert_eq!(
        fadt.pm_timer_block(),
        Some(0),
        "the fixed timer field is present but zero"
    );
    assert_eq!(
        fadt.pm_timer(),
        None,
        "a zero port means there is no PM timer"
    );
    assert_eq!(
        fadt.arm_boot_arch(),
        None,
        "the ARM flags are past the end too"
    );
}

#[test]
fn a_generic_address_past_the_end_is_none() {
    let bytes = [0u8; 8];
    assert_eq!(
        GenericAddress::parse(&bytes, 0),
        None,
        "twelve bytes cannot be read out of eight"
    );
    assert_eq!(
        GenericAddress::parse(&bytes, usize::MAX),
        None,
        "an offset that would overflow must not wrap into a valid range"
    );
}

// ---------------------------------------------------------------------------
// Totality
// ---------------------------------------------------------------------------

/// Call everything this crate exposes, discarding the answers. Any panic,
/// overflow or out-of-bounds read shows up as a failing test rather than as a
/// kernel that dies on somebody's laptop.
fn exercise(memory: &Memory) {
    let Ok(rsdp) = Rsdp::read(memory, RSDP_ADDR) else {
        return;
    };
    let _ = rsdp.is_valid();
    let Ok(acpi) = Acpi::from_rsdp(memory, &rsdp) else {
        return;
    };
    let _ = acpi.root_address();
    if let Ok(root) = acpi.root() {
        let _ = (root.len(), root.is_empty(), root.kind());
        for index in 0..root.len().saturating_add(2) {
            let _ = root.entry(index);
        }
        for address in root.entries() {
            let _ = address;
        }
    }
    if let Ok(madt) = acpi.madt() {
        let _ = madt.local_controller_address();
        let _ = madt.local_apic_address();
        let _ = madt.has_legacy_pics();
        let _ = madt.check_entries();
        for entry in madt.entries() {
            let _ = entry;
        }
    }
    if let Ok(fadt) = acpi.fadt() {
        let _ = fadt.century();
        let _ = fadt.flags();
        let _ = fadt.dsdt_address();
        let _ = fadt.firmware_ctrl();
        let _ = fadt.iapc_boot_arch();
        let _ = fadt.arm_boot_arch();
        let _ = fadt.pm_timer_block();
        let _ = fadt.pm_timer_length();
        let _ = fadt.x_pm_timer_block();
        let _ = fadt.pm_timer();
    }
    if let Ok(gtdt) = acpi.gtdt() {
        let _ = gtdt.counter_control_base();
        let _ = gtdt.counter_read_base();
        let _ = gtdt.secure_el1_timer();
        let _ = gtdt.non_secure_el1_timer();
        let _ = gtdt.virtual_el1_timer();
        let _ = gtdt.non_secure_el2_timer();
        let _ = gtdt.virtual_el2_timer();
        let _ = gtdt.platform_timer_count();
    }
    if let Ok(hpet) = acpi.hpet() {
        let _ = hpet.revision();
        let _ = hpet.comparators();
        let _ = hpet.counter_is_64_bit();
        let _ = hpet.supports_legacy_replacement();
        let _ = hpet.vendor();
        let _ = hpet.base_address();
        let _ = hpet.block_number();
        let _ = hpet.minimum_tick();
        let _ = hpet.page_protection();
        let _ = hpet.table();
    }
}

/// Corrupt one byte of one table at a time and read the whole set again.
fn corrupt_every_byte(machine: &Memory, addresses: &[u64]) {
    for address in addresses {
        let length = machine.tables.get(address).unwrap().len();
        for index in 0..length {
            for value in [0x00u8, 0x01, 0x7F, 0xFF] {
                let mut memory = machine.clone();
                memory.get_mut(*address)[index] = value;
                exercise(&memory);
            }
        }
    }
}

#[test]
fn never_panics_on_a_corrupted_x86_table_set() {
    let machine = q35();
    corrupt_every_byte(&machine, &[RSDP_ADDR, XSDT_ADDR, MADT_ADDR, FADT_ADDR]);
}

#[test]
fn never_panics_on_a_corrupted_aarch64_table_set() {
    let machine = virt();
    corrupt_every_byte(&machine, &[RSDP_ADDR, XSDT_ADDR, MADT_ADDR, GTDT_ADDR]);
}

#[test]
fn never_panics_on_a_truncated_table_set() {
    let machine = q35();
    for address in [RSDP_ADDR, XSDT_ADDR, MADT_ADDR, FADT_ADDR] {
        let length = machine.tables.get(&address).unwrap().len();
        for cut in 0..length {
            let mut memory = machine.clone();
            memory.get_mut(address).truncate(cut);
            exercise(&memory);
        }
    }
}

#[test]
fn never_panics_on_arbitrary_bytes() {
    // Not a table at any address, just numbers, including the lengths and
    // signatures that would send a trusting parser anywhere.
    for seed in 0..64u8 {
        let mut memory = Memory::new();
        let pattern: Vec<u8> = (0..512u32)
            .map(|index| (index as u8).wrapping_mul(seed).wrapping_add(seed))
            .collect();
        for address in [RSDP_ADDR, XSDT_ADDR, MADT_ADDR, FADT_ADDR, GTDT_ADDR] {
            memory.put(address, pattern.clone());
        }
        exercise(&memory);

        // And the same bytes behind a well-formed RSDP, so the walk actually
        // starts.
        memory.put(RSDP_ADDR, build_rsdp(2, 0, XSDT_ADDR));
        exercise(&memory);
    }
}

#[test]
fn errors_all_have_a_message() {
    use std::string::ToString;

    let errors = [
        AcpiError::TooShort { got: 1, need: 36 },
        AcpiError::BadRsdpSignature,
        AcpiError::BadSignature(*b"AP\0C"),
        AcpiError::LengthTooSmall {
            declared: 4,
            need: 36,
        },
        AcpiError::LengthTooLarge {
            declared: 4096,
            available: 36,
        },
        AcpiError::Unreadable(0x1000),
        AcpiError::NotFound(MADT_SIGNATURE),
        AcpiError::NoRootTable,
        AcpiError::ZeroLengthEntry { offset: 8 },
        AcpiError::EntryOutOfBounds { offset: 8 },
        AcpiError::TooManyEntries,
    ];
    for error in errors {
        assert!(
            !error.to_string().is_empty(),
            "{error:?} must render to something a log can carry"
        );
    }
    assert!(
        AcpiError::BadSignature(*b"AP\0C")
            .to_string()
            .contains("\\x00"),
        "an unprintable byte in a signature must show as hex, not vanish"
    );
}

// ---------------------------------------------------------------------------
// HPET
// ---------------------------------------------------------------------------

/// The hardware id QEMU's q35 reports: revision 1, three comparators, a 64-bit
/// main counter, legacy replacement available, Intel's vendor id.
const HPET_ID: u32 = 0x8086_0000 | (1 << 15) | (1 << 13) | (2 << 8) | 1;

/// Where QEMU puts the timer block.
const HPET_BLOCK: u64 = 0xFED0_0000;

/// A q35-shaped HPET description table.
fn hpet() -> Vec<u8> {
    let mut body = vec![0u8; HPET_MIN_LEN - SDT_HEADER_LEN];
    put_u32(&mut body, 36, HPET_ID);
    put_u8(&mut body, 40, GenericAddress::SYSTEM_MEMORY);
    put_u8(&mut body, 41, 64); // register_bit_width
    put_u8(&mut body, 43, 4); // access_size: a 64-bit access
    put_u64(&mut body, 44, HPET_BLOCK);
    put_u8(&mut body, 52, 0); // block number
    put_u16(&mut body, 53, 0x0080); // minimum tick
    put_u8(&mut body, 55, 0); // page protection
    TableBuilder::new(HPET_SIGNATURE).raw(&body).build()
}

#[test]
fn an_hpet_describes_its_timer_block() {
    let bytes = hpet();
    let table = Table::parse(&bytes).unwrap();
    let hpet = Hpet::parse(table).unwrap();

    assert_eq!(hpet.revision(), 1, "the revision is the low byte");
    assert_eq!(
        hpet.comparators(),
        3,
        "the count is stored less one, so 2 encodes three comparators"
    );
    assert!(
        hpet.counter_is_64_bit(),
        "bit 13 says the main counter is 64 bits wide"
    );
    assert!(
        hpet.supports_legacy_replacement(),
        "bit 15 says the block can take over the PIT and RTC interrupts"
    );
    assert_eq!(hpet.vendor(), 0x8086, "the vendor is the top sixteen bits");

    let base = hpet.base_address().unwrap();
    assert_eq!(
        base.address, HPET_BLOCK,
        "the registers are where firmware said"
    );
    assert_eq!(
        base.address_space_id,
        GenericAddress::SYSTEM_MEMORY,
        "an MMIO block, so a caller must map it rather than use `in`/`out`"
    );

    assert_eq!(hpet.block_number(), Some(0));
    assert_eq!(hpet.minimum_tick(), Some(0x80));
    assert_eq!(hpet.page_protection(), Some(0));
}

#[test]
fn an_hpet_with_one_comparator_reports_one() {
    // The count is stored less one, so zero is the smallest encodable value
    // and it means *one* comparator, not none. Getting this backwards would
    // have the kernel conclude a working block has nothing to program.
    let mut body = vec![0u8; HPET_MIN_LEN - SDT_HEADER_LEN];
    put_u32(&mut body, 36, 0);
    let bytes = TableBuilder::new(HPET_SIGNATURE).raw(&body).build();
    let hpet = Hpet::parse(Table::parse(&bytes).unwrap()).unwrap();

    assert_eq!(hpet.comparators(), 1, "zero encodes one comparator");
    assert!(
        !hpet.counter_is_64_bit(),
        "a 32-bit counter wraps and must be noticed"
    );
}

#[test]
fn a_short_hpet_is_rejected() {
    // One byte short of the fixed fields. Accepting it would make every
    // accessor read off the end of the table.
    let mut bytes = hpet();
    let _ = bytes.pop();
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());

    assert_eq!(
        Hpet::parse(Table::parse(&bytes).unwrap()).err(),
        Some(AcpiError::TooShort {
            got: HPET_MIN_LEN - 1,
            need: HPET_MIN_LEN,
        }),
        "a table too short to hold the fields it promises is not an HPET"
    );
}

#[test]
fn a_table_that_is_not_an_hpet_is_rejected() {
    let bytes = gtdt();
    assert_eq!(
        Hpet::parse(Table::parse(&bytes).unwrap()).err(),
        Some(AcpiError::BadSignature(GTDT_SIGNATURE)),
        "the signature is the only thing distinguishing one table from another"
    );
}

#[test]
fn an_hpet_is_found_through_the_root_table() {
    let mut memory = Memory::new();
    memory.put(RSDP_ADDR, build_rsdp(2, 0, XSDT_ADDR));
    memory.put(XSDT_ADDR, xsdt(&[HPET_ADDR]));
    memory.put(HPET_ADDR, hpet());

    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();

    assert_eq!(
        acpi.hpet().unwrap().base_address().unwrap().address,
        HPET_BLOCK,
        "the timer block is reached by signature like any other table"
    );
}

#[test]
fn a_machine_with_no_hpet_says_so() {
    // AArch64 has an architected timer in the CPU and no HPET at all, so
    // "not found" is an ordinary answer rather than a broken machine.
    let mut memory = Memory::new();
    memory.put(RSDP_ADDR, build_rsdp(2, 0, XSDT_ADDR));
    memory.put(XSDT_ADDR, xsdt(&[GTDT_ADDR]));
    memory.put(GTDT_ADDR, gtdt());

    let rsdp = Rsdp::read(&memory, RSDP_ADDR).unwrap();
    let acpi = Acpi::from_rsdp(&memory, &rsdp).unwrap();

    assert_eq!(
        acpi.hpet().err(),
        Some(AcpiError::NotFound(HPET_SIGNATURE)),
        "a missing HPET is reported, not invented"
    );
}

/// A q35 that also lists an HPET, for the paths that need one. Separate from
/// [`q35`] on purpose: the tests built on that fixture assert on exactly what
/// it lists, and a shared fixture that grows under them stops being one.
fn q35_with_hpet() -> Memory {
    let mut memory = q35();
    memory.put(XSDT_ADDR, xsdt(&[MADT_ADDR, FADT_ADDR, HPET_ADDR]));
    memory.put(HPET_ADDR, hpet());
    memory
}

#[test]
fn never_panics_on_a_corrupted_hpet() {
    // Same argument as the other corruption sweeps: the timer block's address
    // comes from firmware, and a kernel that maps whatever a flipped bit says
    // has written to an arbitrary physical page.
    corrupt_every_byte(&q35_with_hpet(), &[HPET_ADDR]);
}
