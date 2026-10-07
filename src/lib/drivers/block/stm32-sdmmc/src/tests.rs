//! The controller and card logic against `fake`'s model, and the partition
//! rule against tables built here.

extern crate std;

mod fake;

use std::vec;
use std::vec::Vec;

use fake::{Fake, FakeClock, KERNEL_HZ, NEGEDGE, RCA};

use crate::card::{Card, capacity};
use crate::gpt::{Crc, Refused, Table, Writable};
use crate::host::{Controller, Error, WRITTEN, clock_divider};
use crate::regs::{CLKDIV_MASK, HWFC_EN, IDMACTRLR, POWER, WIDBUS_4, WIDBUS_MASK};
use crate::{BLOCK_BYTES, BLOCK_WORDS, Words};

impl Words for Vec<u32> {
    fn len_bytes(&self) -> usize {
        self.len() * 4
    }

    fn read_word(&self, offset: usize) -> u32 {
        self[offset / 4]
    }

    fn write_word(&mut self, offset: usize, value: u32) {
        self[offset / 4] = value;
    }
}

type TestCard = Card<Fake, FakeClock>;

fn card_on(fake: &Fake) -> TestCard {
    let host = Controller::take_over(fake.clone(), FakeClock::default(), KERNEL_HZ)
        .expect("the controller is taken over");
    let card = Card::init(host)
        .map_err(|(_, error)| error)
        .expect("the card comes up");
    assert_eq!(
        fake.violations(),
        Vec::<std::string::String>::new(),
        "no violation"
    );
    card
}

#[test]
fn an_sdhc_card_comes_up_on_four_lines_at_default_speed() {
    let fake = Fake::sdhc(8192);
    let card = card_on(&fake);
    let identity = card.identity();
    assert_eq!(identity.rca, RCA, "its address");
    assert!(identity.high_capacity, "block addressed");
    assert_eq!(identity.blocks, 8192, "its size from the CSD");
    let clkcr = fake.state().clkcr;
    assert_eq!(clkcr & CLKDIV_MASK, 2, "99 MHz over 4: 24.75 MHz");
    assert_eq!(card.host().card_hz(), 24_750_000, "the rate set");
    assert_eq!(clkcr & WIDBUS_MASK, WIDBUS_4, "four lines");
    assert_ne!(clkcr & HWFC_EN, 0, "flow control on");
    assert_ne!(clkcr & NEGEDGE, 0, "firmware's clock edge kept");
}

#[test]
fn the_card_is_identified_at_no_more_than_400_khz() {
    // The model records a violation for any identification command above
    // 400 kHz; this proves it would, by starting from a host that never
    // slows the clock.
    let fake = Fake::sdhc(1024);
    let mut host = Controller::take_over(fake.clone(), FakeClock::default(), KERNEL_HZ).unwrap();
    let _ = host.set_clock(25_000_000, false);
    let _ = host.command(&crate::host::Command {
        index: 0,
        argument: 0,
        response: crate::host::Response::None,
        busy: false,
    });
    let _ = host.command(&crate::host::Command::r1(8, 0x1AA));
    assert!(
        fake.violations()
            .iter()
            .any(|v| v.contains("while identifying")),
        "the model sees a fast identification: {:?}",
        fake.violations()
    );
    // And the driver's own bring-up never does it.
    let fake = Fake::sdhc(1024);
    let _ = card_on(&fake);
}

#[test]
fn only_the_listed_registers_are_written_and_never_power_or_dma() {
    let fake = Fake::sdhc(4096);
    let mut card = card_on(&fake);
    let mut memory = fake::buffer(4);
    card.read(10, 4, &mut memory, 0).unwrap();
    card.write(20, 2, &mut memory, 0).unwrap();
    let state = fake.state();
    for &(offset, value) in &state.writes {
        assert!(WRITTEN.contains(&offset), "{offset:#x} is listed");
        assert_ne!(offset, POWER, "POWER is never written");
        if offset == IDMACTRLR {
            assert_eq!(value, 0, "the DMA is only ever turned off");
        }
    }
    assert_eq!(state.idmactrlr, 0, "firmware's DMA enable turned off");
}

#[test]
fn a_dma_left_off_is_not_written() {
    let fake = Fake::sdhc(1024);
    fake.state().idmactrlr = 0;
    let _ = card_on(&fake);
    assert!(
        !fake
            .state()
            .writes
            .iter()
            .any(|&(offset, _)| offset == IDMACTRLR),
        "IDMACTRLR untouched"
    );
}

#[test]
fn the_model_fails_a_dma_enable() {
    // The control for the rule above: one write of IDMAEN, which no code
    // path makes, is a violation.
    let fake = Fake::sdhc(1024);
    let mut regs = fake.clone();
    crate::Registers::write(&mut regs, IDMACTRLR, 1);
    assert!(
        fake.violations()
            .iter()
            .any(|v| v.contains("DMA register 0x50")),
        "an IDMAEN write fires: {:?}",
        fake.violations()
    );
}

#[test]
fn a_controller_firmware_left_off_is_refused_untouched() {
    let fake = Fake::sdhc(1024);
    fake.state().power = 0;
    let error = Controller::take_over(fake.clone(), FakeClock::default(), KERNEL_HZ).unwrap_err();
    assert_eq!(error, Error::NotPowered(0), "refused");
    assert!(fake.state().writes.is_empty(), "nothing written");
}

#[test]
fn blocks_read_back_as_the_card_holds_them() {
    let fake = Fake::sdhc(4096);
    let mut card = card_on(&fake);
    let mut memory = fake::buffer(9);
    card.read(100, 1, &mut memory, 0).unwrap();
    assert!(memory[..BLOCK_WORDS].iter().all(|&w| w == 100), "one block");
    card.read(200, 8, &mut memory, BLOCK_BYTES).unwrap();
    for block in 0..8 {
        let words = &memory[(block + 1) * BLOCK_WORDS..(block + 2) * BLOCK_WORDS];
        assert!(
            words.iter().all(|&w| w == 200 + block as u32),
            "block {block}"
        );
    }
    let commands = fake.state().commands.clone();
    assert!(commands.contains(&(17, 100)), "CMD17 at block 100");
    assert!(commands.contains(&(18, 200)), "CMD18 at block 200");
    assert!(
        commands.contains(&(12, 0)),
        "CMD12 after the multi-block read"
    );
    assert!(fake.violations().is_empty(), "{:?}", fake.violations());
}

#[test]
fn written_blocks_are_programmed_and_read_back() {
    let fake = Fake::sdhc(4096);
    let mut card = card_on(&fake);
    let mut memory: Vec<u32> = (0..3 * BLOCK_WORDS as u32).collect();
    card.write(300, 3, &mut memory, 0).unwrap();
    card.write(400, 1, &mut memory, 0).unwrap();
    let mut back = fake::buffer(3);
    card.read(300, 3, &mut back, 0).unwrap();
    assert_eq!(back, memory, "three blocks round trip");
    card.read(400, 1, &mut back, 0).unwrap();
    assert_eq!(back[..BLOCK_WORDS], memory[..BLOCK_WORDS], "one block");
    let state = fake.state();
    assert!(state.commands.contains(&(25, 300)), "CMD25");
    assert!(state.commands.contains(&(24, 400)), "CMD24");
    assert!(
        state.commands.iter().filter(|c| c.0 == 13).count() >= 4,
        "programming waited for"
    );
    assert!(state.violations.is_empty(), "{:?}", state.violations);
}

#[test]
fn a_late_reader_loses_nothing_with_flow_control() {
    let fake = Fake::sdhc(4096);
    let mut card = card_on(&fake);
    // The card fills the FIFO faster than eight words a look.
    fake.state().pace = 40;
    let mut memory = fake::buffer(16);
    card.read(1000, 16, &mut memory, 0).unwrap();
    for block in 0..16 {
        assert_eq!(
            memory[block * BLOCK_WORDS],
            1000 + block as u32,
            "block {block}"
        );
    }
}

#[test]
fn without_flow_control_a_late_reader_overruns() {
    // The control for the test above: the same read with HWFC_EN cleared
    // behind the driver's back overruns, so it is flow control that keeps
    // the read whole.
    let fake = Fake::sdhc(4096);
    let mut card = card_on(&fake);
    fake.state().pace = 40;
    fake.state().clkcr &= !HWFC_EN;
    let mut memory = fake::buffer(16);
    assert_eq!(
        card.read(1000, 16, &mut memory, 0),
        Err(Error::Fifo),
        "overrun"
    );
}

#[test]
fn a_crc_error_fails_the_read_and_the_card_reads_again() {
    let fake = Fake::sdhc(4096);
    let mut card = card_on(&fake);
    fake.state().bad_block = Some(503);
    let mut memory = fake::buffer(8);
    assert_eq!(
        card.read(500, 8, &mut memory, 0),
        Err(Error::DataCrc),
        "the CRC error"
    );
    card.read(500, 8, &mut memory, 0).unwrap();
    assert_eq!(memory[3 * BLOCK_WORDS], 503, "read again");
    assert!(fake.violations().is_empty(), "{:?}", fake.violations());
}

#[test]
fn reads_past_the_card_or_the_memory_are_refused_before_any_command() {
    let fake = Fake::sdhc(2048);
    let mut card = card_on(&fake);
    let before = fake.state().commands.len();
    let mut memory = fake::buffer(2);
    assert_eq!(
        card.read(2047, 2, &mut memory, 0),
        Err(Error::OutOfRange),
        "past the end"
    );
    assert_eq!(
        card.read(0, 0, &mut memory, 0),
        Err(Error::OutOfRange),
        "nothing"
    );
    assert_eq!(
        card.read(0, 2, &mut memory, 4),
        Err(Error::OutOfRange),
        "past memory"
    );
    assert_eq!(fake.state().commands.len(), before, "no command sent");
}

#[test]
fn a_version_1_card_is_not_taken() {
    let fake = Fake::sdhc(1024);
    fake.state().version1 = true;
    let host = Controller::take_over(fake, FakeClock::default(), KERNEL_HZ).unwrap();
    let error = Card::init(host).map(|_| ()).map_err(|(_, e)| e);
    assert_eq!(error, Err(Error::Unsupported), "refused");
}

#[test]
fn an_sdsc_card_is_byte_addressed_with_512_byte_blocks() {
    let fake = Fake::sdsc(2048);
    let mut card = card_on(&fake);
    assert!(!card.identity().high_capacity, "byte addressed");
    assert_eq!(card.identity().blocks, 2048, "size from a version 1 CSD");
    let mut memory = fake::buffer(2);
    card.read(7, 2, &mut memory, 0).unwrap();
    assert_eq!(memory[BLOCK_WORDS], 8, "second block");
    let state = fake.state();
    assert!(state.commands.contains(&(16, 512)), "CMD16");
    assert!(state.commands.contains(&(18, 7 * 512)), "a byte address");
}

#[test]
fn clock_dividers() {
    assert_eq!(clock_divider(KERNEL_HZ, 400_000), 124, "99 MHz to 399 kHz");
    assert_eq!(clock_divider(KERNEL_HZ, 25_000_000), 2, "to 24.75 MHz");
    assert_eq!(clock_divider(KERNEL_HZ, 200_000_000), 0, "bypass");
    assert_eq!(clock_divider(KERNEL_HZ, 1), CLKDIV_MASK, "the slowest");
}

#[test]
fn capacities() {
    // An SDHC card's CSD with C_SIZE 15159: 7.4 GiB.
    let whole: u128 = (1 << 126) | (15159 << 48) | (0xFF << 56 >> 56);
    let csd = [
        (whole >> 96) as u32,
        (whole >> 64) as u32,
        (whole >> 32) as u32,
        whole as u32,
    ];
    assert_eq!(capacity(csd), Some((15159 + 1) * 1024), "SDHC");
    assert_eq!(
        capacity([0xC000_0000, 0, 0, 0]),
        None,
        "a reserved structure"
    );
}

// -- The partition rule ----------------------------------------------------

/// A GPT entry: type, first, last, name.
type Entry = ([u8; 16], u64, u64, &'static str);

/// The DK1's card as step 1 of its guide lays it out, with a scratch
/// partition after `bootfs`.
fn dk1_entries() -> Vec<Entry> {
    let basic = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];
    vec![
        (basic, 34, 4129, "fsbl1"),
        (basic, 4130, 8225, "fsbl2"),
        (basic, 8226, 16417, "fip"),
        (basic, 16418, 278561, "bootfs"),
        (basic, 278562, 2375713, "ferrix-scratch"),
    ]
}

/// The table's blocks: block 1 (the header) and blocks 2 to 33 (128
/// entries of 128 bytes), for a card of `blocks` blocks.
fn table(entries: &[Entry], blocks: u64) -> (Vec<u8>, Vec<u8>) {
    let mut array = vec![0_u8; 128 * 128];
    for (slot, (kind, first, last, name)) in entries.iter().enumerate() {
        let entry = &mut array[slot * 128..(slot + 1) * 128];
        entry[..16].copy_from_slice(kind);
        entry[16] = slot as u8 + 1;
        entry[32..40].copy_from_slice(&first.to_le_bytes());
        entry[40..48].copy_from_slice(&last.to_le_bytes());
        for (unit, byte) in name.bytes().enumerate() {
            entry[56 + unit * 2] = byte;
        }
    }
    let mut crc = Crc::new();
    crc.update(&array);
    let mut header = vec![0_u8; BLOCK_BYTES];
    header[..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
    header[12..16].copy_from_slice(&92_u32.to_le_bytes());
    header[24..32].copy_from_slice(&1_u64.to_le_bytes());
    header[32..40].copy_from_slice(&(blocks - 1).to_le_bytes());
    header[40..48].copy_from_slice(&34_u64.to_le_bytes());
    header[48..56].copy_from_slice(&(blocks - 34).to_le_bytes());
    header[72..80].copy_from_slice(&2_u64.to_le_bytes());
    header[80..84].copy_from_slice(&128_u32.to_le_bytes());
    header[84..88].copy_from_slice(&128_u32.to_le_bytes());
    header[88..92].copy_from_slice(&crc.value().to_le_bytes());
    seal(&mut header);
    (header, array)
}

/// Write the header's CRC.
fn seal(header: &mut [u8]) {
    header[16..20].fill(0);
    let mut crc = Crc::new();
    crc.update(&header[..92]);
    header[16..20].copy_from_slice(&crc.value().to_le_bytes());
}

#[allow(
    clippy::unwrap_in_result,
    reason = "a fixture that is not a block is a broken test"
)]
fn read_table(header: &[u8], array: &[u8], blocks: u64) -> Result<Writable, Refused> {
    let header: &[u8; BLOCK_BYTES] = header.try_into().expect("a block");
    let blocks_of: Vec<[u8; BLOCK_BYTES]> = array
        .chunks(BLOCK_BYTES)
        .map(|block| block.try_into().expect("a block"))
        .collect();
    let mut table = Table::header(header, blocks)?;
    for block in blocks_of.iter().take(table.entry_blocks() as usize) {
        table.feed(block);
    }
    table.finish()
}

const CARD: u64 = 250_000_000;

#[test]
fn only_a_ferrix_partition_is_writable() {
    let (header, array) = table(&dk1_entries(), CARD);
    let writable = read_table(&header, &array, CARD).unwrap();
    assert_eq!(
        writable.ranges(),
        &[(278562, 2375713)],
        "the scratch partition alone"
    );
    assert!(writable.allows(278562, 8), "its first blocks");
    assert!(writable.allows(2375706, 8), "its last blocks");
    for (_, first, last, name) in &dk1_entries()[..4] {
        assert!(!writable.allows(*first, 1), "{name}'s first block");
        assert!(!writable.allows(*last, 1), "{name}'s last block");
    }
    assert!(!writable.allows(0, 1), "the protective MBR");
    assert!(!writable.allows(1, 1), "the header");
    assert!(!writable.allows(278561, 2), "across bootfs's end");
    assert!(!writable.allows(2375713, 2), "past the partition's end");
    assert!(!writable.allows(CARD - 1, 1), "the backup header");
    assert!(!writable.allows(278562, 0), "nothing");
    assert!(!writable.allows(u64::MAX, 2), "an overflow");
}

#[test]
fn a_card_as_it_is_today_has_nothing_writable() {
    let (header, array) = table(&dk1_entries()[..4], CARD);
    assert!(
        read_table(&header, &array, CARD).unwrap().is_empty(),
        "read-only"
    );
}

#[test]
fn a_damaged_table_is_refused() {
    let (header, array) = table(&dk1_entries(), CARD);
    let mut bad = header.clone();
    bad[40] ^= 1;
    assert_eq!(
        read_table(&bad, &array, CARD),
        Err(Refused::Header),
        "header CRC"
    );
    let mut bad = array.clone();
    bad[4 * 128 + 56] = b'g';
    assert_eq!(
        read_table(&header, &bad, CARD),
        Err(Refused::EntriesCrc),
        "array CRC"
    );
    assert_eq!(
        read_table(&header, &array, 1000),
        Err(Refused::Header),
        "a smaller card"
    );
    let table = Table::header(header.as_slice().try_into().unwrap(), CARD).unwrap();
    assert_eq!(table.finish(), Err(Refused::Feed), "an array never read");
}

#[test]
fn a_writable_partition_over_another_is_refused() {
    let mut entries = dk1_entries();
    // A scratch partition laid over bootfs's last blocks.
    entries[4].1 = 278000;
    let (header, array) = table(&entries, CARD);
    assert_eq!(
        read_table(&header, &array, CARD),
        Err(Refused::Overlap),
        "overlap"
    );
}

#[test]
fn a_partition_outside_the_usable_range_is_refused() {
    let mut entries = dk1_entries();
    entries[4].2 = CARD - 10;
    let (header, array) = table(&entries, CARD);
    assert_eq!(
        read_table(&header, &array, CARD),
        Err(Refused::Partition),
        "past last usable"
    );
}

#[test]
fn the_crc_is_uefis() {
    let mut crc = Crc::new();
    crc.update(b"123456789");
    assert_eq!(crc.value(), 0xCBF4_3926, "the check value");
}
