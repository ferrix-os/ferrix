//! FTDI serial adapters and the USB-Blaster: the chip's arithmetic on its
//! own, then the bus behind the xHCI model, with the model's chip on the
//! far side of the bulk pipes.

use std::vec;
use std::vec::Vec;

use super::model::ftdi::{Cable, Serial, Tap, chip};
use super::model::xhci::{Memory, Regs, Shared, Time, Xhci};
use crate::blaster::{self, Jtag, MAX_TAPS};
use crate::bus::{Bus, SerialChange};
use crate::ftdi::{
    self, Chip, Flow, LINE_PARITY, Line, PRODUCT_FT232, PRODUCT_FT232H, PRODUCT_USB_BLASTER,
    Parity, Role, Status, Termios, VENDOR_ALTERA, VENDOR_FTDI,
};
use crate::usb::Speed;
use crate::xhci::Controller;
use crate::{Clock, MILLISECOND, Parts};

type TestBus = Bus<Controller<Regs, Memory, Time>>;

/// A MAX 10 10M08: a ten-bit IR, IDCODE at 0x006, and the IDCODE `OpenOCD`
/// read from the customer's board.
const MAX10_IDCODE: u32 = 0x0318_20DD;
const MAX10_IR: usize = 10;
const MAX10_IDCODE_INSTRUCTION: u64 = 0x006;

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

// ---------------------------------------------------------------------------
// On the bus
// ---------------------------------------------------------------------------

fn started(model: &Shared) -> TestBus {
    Bus::with(
        Controller::start(Parts {
            registers: Regs(model.clone()),
            memory: Memory(model.clone()),
            clock: Time(model.clone()),
        })
        .map_err(|(error, _)| error)
        .expect("the controller starts"),
    )
}

/// The model with one chip on root port `port` (0 and 1 are USB 2), its
/// far side `serial`, and the bus once it found it.
fn board(
    device: super::model::Device,
    port: usize,
    cable: Option<Cable>,
) -> (Shared, TestBus, usize) {
    let model = Xhci::new();
    let index = {
        let mut m = model.borrow_mut();
        let packet = if device.speed == Speed::High { 512 } else { 64 };
        let index = m.add(device);
        let mut serial = Serial::new(index, packet);
        serial.cable = cable;
        m.serials.push(serial);
        m.plug_root(port, index);
        index
    };
    let mut bus = started(&model);
    bus.poll();
    (model, bus, index)
}

fn no_violations(model: &Shared) {
    let model = model.borrow();
    assert!(model.violations.is_empty(), "{:#?}", model.violations);
}

/// Let `ms` milliseconds pass, serving the controller each one.
fn pass(model: &Shared, bus: &mut TestBus, ms: u64) {
    for _ in 0..ms {
        Time(model.clone()).sleep_nanos(MILLISECOND);
        bus.on_interrupt().expect("the controller runs");
    }
}

/// The vendor requests the device received, as `(request, value, index)`.
fn vendor_requests(model: &Shared, device: usize) -> Vec<(u8, u16, u16)> {
    model.borrow().world.devices[device]
        .setups
        .iter()
        .filter(|setup| setup[0] == 0x40)
        .map(|setup| {
            (
                setup[1],
                u16::from_le_bytes([setup[2], setup[3]]),
                u16::from_le_bytes([setup[4], setup[5]]),
            )
        })
        .collect()
}

/// Everything serial `serial` has, read until the controller has nothing
/// more for `ms` milliseconds.
fn drain(model: &Shared, bus: &mut TestBus, serial: usize, into: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buffer = vec![0_u8; into];
    let mut quiet = 0;
    while quiet < 5 {
        pass(model, bus, 1);
        let n = bus.serial_read(serial, &mut buffer);
        if n == 0 {
            quiet += 1;
        } else {
            quiet = 0;
            got.extend_from_slice(&buffer[..n]);
        }
    }
    got
}

#[test]
fn an_ft232h_is_set_up_as_a_uart_at_9600_8n1() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    assert_eq!(bus.next_serial_change(), Some(SerialChange::Attached(0)));
    let serial = *bus.serial(0).expect("the port");
    assert_eq!(
        (serial.chip, serial.role, serial.max_packet),
        (Chip::H, Role::Uart, 512)
    );
    assert_eq!(serial.line, Line::DEFAULT);
    // Every request names port A, interface 0's, as 1; the H divisor for
    // 9600 has bit 17, so 2 in wIndex's high byte.
    let divisor = ftdi::divisor(Chip::H, 9600).expect("a divisor");
    assert_eq!(
        vendor_requests(&model, device),
        vec![
            (0, 0, 1),
            (0, 1, 1),
            (0, 2, 1),
            (9, 16, 1),
            (1, 0x0303, 1),
            (3, divisor as u16, (((divisor >> 16) as u16) << 8) | 1),
            (4, 8, 1),
            (2, 0, 1),
        ]
    );
    no_violations(&model);
}

#[test]
fn bytes_go_out_and_come_in_without_the_status_bytes() {
    for (speed, product, release, port) in [
        (Speed::High, PRODUCT_FT232H, 0x900, 0),
        // QEMU's usb-serial: an FT232BM at full speed.
        (Speed::Full, PRODUCT_FT232, 0x400, 1),
    ] {
        let (model, mut bus, _) = board(chip(speed, VENDOR_FTDI, product, release), port, None);
        let _ = bus.next_serial_change();
        let out: Vec<u8> = (0..3000).map(|at| (at * 7 % 251) as u8).collect();
        assert_eq!(bus.serial_write(0, &out), Ok(out.len()));
        assert_eq!(model.borrow().serials[0].from_host, out);

        // More than a request holds, and more than the stream's queue, and
        // three status-only packets before any of it.
        let incoming: Vec<u8> = (0..5000).map(|at| (at * 13 % 253) as u8).collect();
        {
            let mut m = model.borrow_mut();
            m.serials[0].status_only = 3;
            m.serials[0].to_host.extend(incoming.iter().copied());
        }
        assert_eq!(drain(&model, &mut bus, 0, 4096), incoming, "{speed:?}");
        assert_eq!(bus.serial(0).expect("the port").modem, 0xB0);
        no_violations(&model);
    }
}

#[test]
fn a_reader_with_little_room_loses_nothing() {
    let (model, mut bus, _) = board(
        chip(Speed::Full, VENDOR_FTDI, PRODUCT_FT232, 0x400),
        0,
        None,
    );
    let _ = bus.next_serial_change();
    let incoming: Vec<u8> = (0..4000).map(|at| (at % 241) as u8).collect();
    model.borrow_mut().serials[0]
        .to_host
        .extend(incoming.iter().copied());
    // A buffer of one request's size takes a chunk at a time.
    assert_eq!(drain(&model, &mut bus, 0, 512), incoming);
    // One too small takes nothing, rather than part of a chunk.
    model.borrow_mut().serials[0].to_host.extend([1, 2, 3]);
    pass(&model, &mut bus, 2);
    assert_eq!(bus.serial_read(0, &mut [0; 100]), 0);
    assert_eq!(drain(&model, &mut bus, 0, 512), vec![1, 2, 3]);
}

#[test]
fn a_line_error_is_kept_until_it_is_asked_for() {
    let (model, mut bus, _) = board(
        chip(Speed::Full, VENDOR_FTDI, PRODUCT_FT232, 0x400),
        0,
        None,
    );
    let _ = bus.next_serial_change();
    {
        let mut m = model.borrow_mut();
        m.serials[0].line_error = LINE_PARITY;
        m.serials[0].to_host.extend(b"x");
    }
    assert_eq!(drain(&model, &mut bus, 0, 512), b"x");
    assert_eq!(bus.take_serial_errors(0), LINE_PARITY);
    assert_eq!(bus.take_serial_errors(0), 0);
}

#[test]
fn a_new_line_is_sent_as_the_chip_s_three_requests() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let _ = bus.next_serial_change();
    let before = vendor_requests(&model, device).len();
    let line = Line {
        baud: 3_000_000,
        data_bits: 7,
        parity: Parity::Even,
        two_stop_bits: true,
        flow: Flow::RtsCts,
    };
    bus.serial_configure(0, &line).expect("configured");
    assert_eq!(
        vendor_requests(&model, device)[before..],
        [
            (3, 0x0004, 0x0201),
            (4, 7 | (2 << 8) | (2 << 11), 1),
            (2, 0, 0x0101)
        ]
    );
    assert_eq!(bus.serial(0).expect("the port").line, line);
    // A rate the chip cannot have leaves the line as it was.
    assert!(bus.serial_configure(0, &Line { baud: 50, ..line }).is_err());
    assert_eq!(bus.serial(0).expect("the port").line, line);
    bus.serial_modem(0, false, true).expect("the modem lines");
    assert_eq!(
        vendor_requests(&model, device).last(),
        Some(&(1, 0x0302, 1))
    );
}

#[test]
fn an_unplugged_adapter_goes_and_gives_its_slot_back() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let _ = bus.next_serial_change();
    model.borrow_mut().unplug_root(0);
    bus.poll();
    assert_eq!(bus.next_serial_change(), Some(SerialChange::Detached(0)));
    assert!(bus.serial(0).is_none());
    assert!(bus.serial_write(0, b"gone").is_err());
    assert_eq!(model.borrow().slots_used(), 0);
    // Plugged in again, it is found again, at the same index.
    model.borrow_mut().plug_root(0, device);
    bus.poll();
    assert_eq!(bus.next_serial_change(), Some(SerialChange::Attached(0)));
    no_violations(&model);
}

// ---------------------------------------------------------------------------
// The USB-Blaster
// ---------------------------------------------------------------------------

fn max10() -> Tap {
    Tap::new(MAX10_IR, MAX10_IDCODE_INSTRUCTION, Some(MAX10_IDCODE))
}

fn blaster_board(taps: Vec<Tap>) -> (Shared, TestBus) {
    let (model, mut bus, _) = board(
        chip(Speed::Full, VENDOR_ALTERA, PRODUCT_USB_BLASTER, 0x400),
        0,
        Some(Cable::new(taps)),
    );
    assert_eq!(bus.next_serial_change(), Some(SerialChange::Attached(0)));
    assert_eq!(bus.serial(0).expect("the cable").role, Role::Blaster);
    (model, bus)
}

#[test]
fn a_blaster_reads_the_max10_s_idcode() {
    let (model, mut bus) = blaster_board(vec![max10()]);
    let mut cable = bus.blaster(0).expect("a cable");
    let mut jtag = Jtag::new(&mut cable);
    let mut taps = [blaster::Tap::Bypass; MAX_TAPS];
    assert_eq!(jtag.idcodes(&mut taps), Ok(1));
    assert_eq!(taps[0], blaster::Tap::Idcode(MAX10_IDCODE));
    // Version 0, part 0x3182, Altera's JEDEC code 0x6E.
    assert_eq!(blaster::fields(MAX10_IDCODE), (0, 0x3182, 0x6E));
    let m = model.borrow();
    let cable = m.serials[0].cable.as_ref().expect("the cable");
    assert!(cable.faults.is_empty(), "{:?}", cable.faults);
    no_violations(&model);
}

#[test]
fn ir_and_dr_scans_read_what_the_tap_captured() {
    let (model, mut bus) = blaster_board(vec![max10()]);
    let mut cable = bus.blaster(0).expect("a cable");
    let mut jtag = Jtag::new(&mut cable);
    jtag.reset().expect("reset");
    // IR captures 0b01 in its lowest bits.
    let mut tdo = [0_u8; 2];
    jtag.scan(true, MAX10_IR, &[0x06, 0x00], &mut tdo)
        .expect("the IR scan");
    assert_eq!(u16::from_le_bytes(tdo) & 0x3FF, 0b01);
    // IDCODE selected, DR is it.
    let mut idcode = [0_u8; 4];
    jtag.scan(false, 32, &[0; 4], &mut idcode)
        .expect("the DR scan");
    assert_eq!(u32::from_le_bytes(idcode), MAX10_IDCODE);
    // BYPASS selected, DR is one zero, then what goes in comes out a bit
    // later.
    jtag.scan(true, MAX10_IR, &[0xFF, 0x03], &mut tdo)
        .expect("BYPASS");
    let mut out = [0_u8; 2];
    jtag.scan(false, 16, &[0xA5, 0x5A], &mut out)
        .expect("through BYPASS");
    assert_eq!(u16::from_le_bytes(out), 0x5AA5 << 1);
    let m = model.borrow();
    let cable = m.serials[0].cable.as_ref().expect("the cable");
    assert_eq!(cable.taps[0].updates, vec![0x006, 0x3FF]);
    assert!(cable.faults.is_empty(), "{:?}", cable.faults);
}

#[test]
fn a_chain_is_read_tap_by_tap_bypassed_ones_too() {
    let other = 0x020F_30DD;
    let (_, mut bus) = blaster_board(vec![
        Tap::new(8, 0x01, Some(other)),
        Tap::new(4, 0x01, None),
        max10(),
    ]);
    let mut cable = bus.blaster(0).expect("a cable");
    let mut jtag = Jtag::new(&mut cable);
    let mut taps = [blaster::Tap::Bypass; MAX_TAPS];
    assert_eq!(jtag.idcodes(&mut taps), Ok(3));
    // The tap nearest TDO first.
    assert_eq!(
        taps[..3],
        [
            blaster::Tap::Idcode(MAX10_IDCODE),
            blaster::Tap::Bypass,
            blaster::Tap::Idcode(other)
        ]
    );
}

#[test]
fn no_chain_reads_as_no_taps_and_a_tdo_held_low_as_a_fault() {
    // Nothing behind the cable: TDO is TDI, all ones.
    let (_, mut bus) = blaster_board(Vec::new());
    let mut cable = bus.blaster(0).expect("a cable");
    let mut taps = [blaster::Tap::Bypass; MAX_TAPS];
    assert_eq!(Jtag::new(&mut cable).idcodes(&mut taps), Ok(0));

    // Every tap bypassed, and more of them than are read: zeros all the
    // way, which no chain of real taps ends in.
    let bypassed = (0..40).map(|_| Tap::new(2, 0x01, None)).collect();
    let (_, mut bus) = blaster_board(bypassed);
    let mut cable = bus.blaster(0).expect("a cable");
    assert_eq!(
        Jtag::new(&mut cable).idcodes(&mut taps),
        Err(blaster::Error::Chain)
    );
}

#[test]
fn a_uart_is_no_jtag_cable() {
    let (_, mut bus, _) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let _ = bus.next_serial_change();
    assert!(bus.blaster(0).is_none());
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

    let mut text = std::string::String::new();
    blaster::write_hex(&mut text, &[0xFF, 0x03], 10).expect("written");
    assert_eq!(text, "0x3ff");
    text.clear();
    blaster::write_hex(&mut text, &MAX10_IDCODE.to_le_bytes(), 32).expect("written");
    assert_eq!(text, "0x031820dd");
}
