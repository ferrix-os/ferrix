//! The chips' arithmetic, the status bytes, termios and the Blaster's
//! command lines, on their own. The chips on a bus, behind a model
//! controller, are `ferrix-usb-host`'s tests (`src/tests/serial.rs`).

use std::string::String;
use std::vec;
use std::vec::Vec;

use crate::blaster;
use crate::ftdi::{
    self, Chip, Flow, Line, PRODUCT_FT232, PRODUCT_FT232H, PRODUCT_USB_BLASTER, Parity, Role,
    Status, Termios, VENDOR_ALTERA, VENDOR_FTDI,
};

/// The MAX 10 10M08's IDCODE, as `OpenOCD` read it from the customer's
/// board.
const MAX10_IDCODE: u32 = 0x0318_20DD;

// ---------------------------------------------------------------------------
// The arithmetic
// ---------------------------------------------------------------------------

#[test]
fn the_b_family_divides_three_megahertz_by_eighths() {
    // 3 MHz / 9600 = 312.5: 312, and the half's code 1 in bits 16..14.
    assert_eq!(ftdi::divisor(Chip::B, 9600), Some(0x4138));
    // 3 MHz / 115200 = 26.04: 26, no fraction.
    assert_eq!(ftdi::divisor(Chip::B, 115_200), Some(0x001A));
    // The two the chips read specially: 3 Mbaud is divisor 1, sent as 0,
    // and 2 Mbaud is 1.5, sent as 1.
    assert_eq!(ftdi::divisor(Chip::B, 3_000_000), Some(0));
    assert_eq!(ftdi::divisor(Chip::B, 2_000_000), Some(1));
    assert_eq!(ftdi::divisor(Chip::B, 4_000_000), None);
    assert_eq!(ftdi::divisor(Chip::B, 100), None);
}

#[test]
fn the_h_family_divides_twelve_megahertz_with_its_clock_bit() {
    // 12 MHz / 115200 = 104.17: 104 and an eighth, whose code is 3, and
    // bit 17 set.
    assert_eq!(ftdi::divisor(Chip::H, 115_200), Some(0x2_C068));
    assert_eq!(ftdi::divisor(Chip::H, 12_000_000), Some(0x2_0000));
    assert_eq!(ftdi::divisor(Chip::H, 3_000_000), Some(0x2_0004));
    // Below 1200 the 3 MHz clock, without the bit.
    assert_eq!(ftdi::divisor(Chip::H, 300), Some(10_000));
    assert_eq!(ftdi::divisor(Chip::H, 12_000_001), None);
}

#[test]
fn every_rate_a_divisor_is_made_for_comes_back_within_three_percent() {
    for chip in [Chip::B, Chip::R, Chip::X, Chip::H] {
        for baud in [
            184, 300, 1200, 2400, 9600, 19200, 38400, 57600, 115_200, 230_400, 460_800, 921_600,
            1_000_000, 2_000_000, 3_000_000,
        ] {
            let value = ftdi::divisor(chip, baud).expect("a divisor");
            let got = ftdi::rate(chip, value).expect("a rate");
            assert!(
                u64::from(got.abs_diff(baud)) * 100 <= u64::from(baud) * 3,
                "{chip:?} {baud}: {got}"
            );
        }
    }
    for baud in [6_000_000, 8_000_000, 12_000_000] {
        let value = ftdi::divisor(Chip::H, baud).expect("a divisor");
        assert_eq!(ftdi::rate(Chip::H, value), Some(baud));
    }
}

#[test]
fn qemu_s_decoding_reads_the_b_family_s_divisor_as_its_rate() {
    // hw/usb/dev-serial.c: the eighths' code from wValue's top two bits and
    // wIndex's bit 0, then 24 MHz over eight times the divisor plus it.
    let subdivisors = [0, 4, 2, 1, 3, 5, 6, 7];
    for baud in [300, 9600, 115_200, 1_000_000] {
        let [setup, ..] = ftdi::configure(
            Chip::B,
            0,
            &Line {
                baud,
                ..Line::DEFAULT
            },
        )
        .expect("setups");
        let value = u32::from(setup.value);
        let index = u32::from(setup.index);
        let sub = subdivisors[(((value & 0xC000) >> 14) | ((index & 1) << 2)) as usize];
        let divisor = value & 0x3FFF;
        let (divisor, sub) = match (divisor, sub) {
            (1, 0) => (1, 4),
            (0, 0) => (1, 0),
            other => other,
        };
        let speed = 24_000_000 / (8 * divisor + sub);
        assert!(speed.abs_diff(baud) * 100 <= baud * 3, "{baud}: {speed}");
    }
}

#[test]
fn the_chips_are_told_apart_by_ids_and_release() {
    assert_eq!(
        ftdi::identify(VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        Some((Chip::H, Role::Uart))
    );
    assert_eq!(
        ftdi::identify(VENDOR_FTDI, PRODUCT_FT232, 0x400),
        Some((Chip::B, Role::Uart))
    );
    assert_eq!(
        ftdi::identify(VENDOR_ALTERA, PRODUCT_USB_BLASTER, 0x400),
        Some((Chip::B, Role::Blaster))
    );
    // An FT232AM's divisor is another, and nothing here computes it.
    assert_eq!(ftdi::identify(VENDOR_FTDI, PRODUCT_FT232, 0x200), None);
    assert_eq!(ftdi::identify(0x1234, PRODUCT_FT232H, 0x900), None);
    assert_eq!(Chip::H.port(0), 1);
    assert_eq!(Chip::B.port(0), 0);
}

#[test]
fn the_status_bytes_go_from_every_packet() {
    let mut transfer = vec![0x31, 0x60];
    transfer.extend(0..62_u8);
    transfer.extend_from_slice(&[0xF1, 0x64, 100, 101]);
    let mut data = Vec::new();
    let mut statuses = Vec::new();
    ftdi::payload(&transfer, 64, |status, bytes| {
        statuses.push(status);
        data.extend_from_slice(bytes);
    });
    let mut wanted: Vec<u8> = (0..62).collect();
    wanted.extend_from_slice(&[100, 101]);
    assert_eq!(data, wanted);
    assert_eq!(
        statuses,
        vec![
            Status {
                modem: 0x30,
                line: 0x60
            },
            Status {
                modem: 0xF0,
                line: 0x64
            }
        ]
    );
    // A status-only packet has no data; a stray byte is no packet.
    let mut none = Vec::new();
    ftdi::payload(&[0x01, 0x60], 512, |_, bytes| none.extend_from_slice(bytes));
    ftdi::payload(&[0x01], 512, |_, bytes| none.extend_from_slice(bytes));
    assert!(none.is_empty());
}

#[test]
fn a_termios_reads_as_the_line_it_asks_for() {
    // cs7, parenb, parodd, cstopb, crtscts, at 57600.
    let termios = Termios {
        iflag: 0,
        cflag: 0o40 | 0o400 | 0o1000 | 0o100 | 0o20_000_000_000,
        ospeed: 57600,
        start: 0x11,
        stop: 0x13,
    };
    assert_eq!(
        termios.line(9600),
        Line {
            baud: 57600,
            data_bits: 7,
            parity: Parity::Odd,
            two_stop_bits: true,
            flow: Flow::RtsCts,
        }
    );
    // cs8, ixon, B0: the speed kept.
    let termios = Termios {
        iflag: 0x400,
        cflag: 0o60,
        ospeed: 0,
        start: 0x11,
        stop: 0x13,
    };
    assert_eq!(
        termios.line(9600),
        Line {
            flow: Flow::XonXoff {
                start: 0x11,
                stop: 0x13
            },
            ..Line::DEFAULT
        }
    );
}

#[test]
fn a_terminal_s_lines_read_as_commands() {
    use blaster::{Command, Refused};
    assert_eq!(Command::parse(b"  reset \n"), Ok(Command::Reset));
    assert_eq!(Command::parse(b"idcode"), Ok(Command::Idcode));
    let Ok(Command::Scan { ir, bits, tdi }) = Command::parse(b"ir 10 0x3ff") else {
        panic!("a scan");
    };
    assert_eq!((ir, bits, &tdi[..3]), (true, 10, &[0xFF, 0x03, 0][..]));
    let Ok(Command::Scan { ir, bits, tdi }) = Command::parse(b"dr 32 31820DD") else {
        panic!("a scan");
    };
    assert_eq!((ir, bits), (false, 32));
    assert_eq!(
        u32::from_le_bytes(tdi[..4].try_into().expect("four")),
        MAX10_IDCODE
    );
    assert_eq!(Command::parse(b"ir 10 0x7ff"), Err(Refused::Value));
    assert_eq!(Command::parse(b"ir 10 0xg"), Err(Refused::Value));
    assert_eq!(Command::parse(b"dr 0 1"), Err(Refused::Bits));
    assert_eq!(Command::parse(b"dr 513 1"), Err(Refused::Bits));
    assert_eq!(Command::parse(b"dr 8"), Err(Refused::Value));
    assert_eq!(Command::parse(b"shift"), Err(Refused::Unknown));
    assert_eq!(Command::parse(b"reset now"), Err(Refused::Unknown));

    let mut text = String::new();
    blaster::write_hex(&mut text, &[0xFF, 0x03], 10).expect("written");
    assert_eq!(text, "0x3ff");
    text.clear();
    blaster::write_hex(&mut text, &MAX10_IDCODE.to_le_bytes(), 32).expect("written");
    assert_eq!(text, "0x031820dd");
}
