//! Exported interfaces, and FTDI serial adapters and the USB-Blaster as
//! their class drivers drive them: `ferrix-usb-serial`'s logic reaching a
//! model chip through the bus's export calls, as `usbserial` reaches a
//! real one through `usbhid`'s.

use std::vec;
use std::vec::Vec;

use ferrix_usb_serial::blaster::{self, Jtag, MAX_TAPS};
use ferrix_usb_serial::ftdi::{
    self, Chip, Flow, LINE_BREAK, LINE_OVERRUN, LINE_PARITY, Line, PRODUCT_FT232, PRODUCT_FT232H,
    PRODUCT_USB_BLASTER, Parity, Role, VENDOR_ALTERA, VENDOR_FTDI,
};
use ferrix_usb_serial::port::{
    self, Cable as Link, FLAG_BREAK, FLAG_NORMAL, FLAG_OVERRUN, FLAG_PARITY, Identity, Serial,
};
use ferrix_usb_serial::{CHUNK_BYTES, Setup, Usb};

use super::model::ftdi::{Cable, Serial as Far, Tap, chip};
use super::model::xhci::{Memory, Regs, Shared, Time, Xhci};
use crate::bus::{Bus, ExportChange, ExportError, Output};
use crate::usb::Speed;
use crate::xhci::Controller;
use crate::{Clock, MILLISECOND, Parts};

type TestBus = Bus<Controller<Regs, Memory, Time>>;

const MAX10_IDCODE: u32 = 0x0318_20DD;
const MAX10_IR: usize = 10;
const MAX10_IDCODE_INSTRUCTION: u64 = 0x006;

const IN: u8 = 0x81;
const OUT: u8 = 0x02;

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
/// far side behind it, and the bus once it found and exported it.
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
        let mut far = Far::new(index, packet);
        far.cable = cable;
        m.serials.push(far);
        m.plug_root(port, index);
        index
    };
    let mut bus = started(&model);
    bus.poll();
    assert_eq!(bus.next_export_change(), Some(ExportChange::Attached(0)));
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

/// A class driver's view of exported interface 0, through the bus's
/// export calls: what `usbserial` has through `usbhid`'s channel.
struct Driver<'a> {
    bus: &'a mut TestBus,
    model: Shared,
}

impl Usb for Driver<'_> {
    type Error = ExportError;

    fn control(&mut self, setup: &Setup, data: &mut [u8]) -> Result<usize, ExportError> {
        self.bus.export_control(0, setup.bytes(), data)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<usize, ExportError> {
        self.bus.export_send(0, OUT, bytes)
    }

    fn in_flight(&self) -> usize {
        self.bus.export_in_flight(0, OUT)
    }

    fn take_chunk(&mut self, into: &mut [u8; CHUNK_BYTES]) -> Option<usize> {
        let chunk = self.bus.export_take_chunk(0, IN)?;
        into[..chunk.len].copy_from_slice(chunk.as_slice());
        Some(chunk.len)
    }

    fn pause(&mut self) -> Result<(), ExportError> {
        Time(self.model.clone()).sleep_nanos(MILLISECOND / 2);
        self.bus.on_interrupt()?;
        Ok(())
    }

    fn now(&self) -> u64 {
        Time(self.model.clone()).now_nanos()
    }
}

fn driver<'a>(model: &Shared, bus: &'a mut TestBus) -> Driver<'a> {
    Driver {
        bus,
        model: model.clone(),
    }
}

/// Bind `ferrix-usb-serial` to exported interface 0, as `usbserial` does
/// at its start: both streams opened, the chip set up.
fn bind(model: &Shared, bus: &mut TestBus) -> Serial {
    bus.export_open(0, IN).expect("IN opens");
    bus.export_open(0, OUT).expect("OUT opens");
    let export = *bus.export(0).expect("exported");
    let identity = Identity {
        vendor: export.identity.vendor,
        product: export.identity.product,
        release: export.identity.release,
        interface: export.identity.interface,
        max_packet: export.endpoints()[0].max_packet,
    };
    Serial::open(&mut driver(model, bus), &identity).expect("the chip is set up")
}

/// Everything the chip received, read `room` bytes at a time until the
/// controller has had nothing more for 5 ms.
fn drain(
    model: &Shared,
    bus: &mut TestBus,
    serial: &mut Serial,
    room: usize,
) -> (Vec<u8>, Vec<u8>) {
    let (mut got, mut marks) = (Vec::new(), Vec::new());
    let (mut data, mut flags) = (vec![0_u8; room], vec![0_u8; room]);
    let mut quiet = 0;
    while quiet < 5 {
        pass(model, bus, 1);
        let n = serial.read(&mut driver(model, bus), &mut data, &mut flags);
        if n == 0 {
            quiet += 1;
        } else {
            quiet = 0;
            got.extend_from_slice(&data[..n]);
            marks.extend_from_slice(&flags[..n]);
        }
    }
    (got, marks)
}

// ---------------------------------------------------------------------------
// The export
// ---------------------------------------------------------------------------

#[test]
fn a_vendor_interface_is_exported_with_its_identity_and_endpoints() {
    let (model, bus, _) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let export = *bus.export(0).expect("exported");
    let identity = export.identity;
    assert_eq!(
        (identity.vendor, identity.product, identity.release),
        (VENDOR_FTDI, PRODUCT_FT232H, 0x900)
    );
    assert_eq!(
        (identity.class, identity.interface, identity.speed),
        (0xFF, 0, 2)
    );
    assert_eq!(identity.path, 1 << 24, "root port 1, from one, no hub");
    assert_eq!(identity.manufacturer.as_bytes(), b"FTDI");
    assert_eq!(identity.product_name.as_bytes(), b"Model Serial");
    let addresses: Vec<(u8, u16)> = export
        .endpoints()
        .iter()
        .map(|endpoint| (endpoint.address, endpoint.max_packet))
        .collect();
    assert_eq!(addresses, vec![(IN, 512), (OUT, 512)]);
    // Nothing was sent the chip: setting it up is its class driver's.
    assert!(vendor_requests(&model, 0).is_empty());
    no_violations(&model);
}

#[test]
fn a_class_driver_asks_only_what_its_interface_may() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut none = [0_u8; 0];
    let set_address = [0x00, 5, 9, 0, 0, 0, 0, 0];
    let set_configuration = [0x00, 9, 0, 0, 0, 0, 0, 0];
    let another_interface = [0x41, 1, 0, 0, 1, 0, 0, 0];
    for refused in [set_address, set_configuration, another_interface] {
        assert_eq!(
            bus.export_control(0, refused, &mut none),
            Err(ExportError::Refused),
            "{refused:?}"
        );
    }
    // A data stage longer than the buffer the driver gave is refused too.
    assert_eq!(
        bus.export_control(0, [0xC0, 5, 0, 0, 1, 0, 2, 0], &mut none),
        Err(ExportError::Refused)
    );
    assert_eq!(model.borrow().world.devices[device].setups.len(), {
        // Enumeration's own requests, and none of the refused.
        let all = &model.borrow().world.devices[device].setups;
        all.iter()
            .filter(|setup| setup[0] != 0x40 && setup[0] != 0x41)
            .count()
    });
    // A vendor request to the device goes.
    assert_eq!(
        bus.export_control(0, [0x40, 0, 0, 0, 1, 0, 0, 0], &mut none),
        Ok(0)
    );
    // Streams only on the interface's own bulk endpoints, once each.
    assert_eq!(bus.export_open(0, 0x83), Err(ExportError::Refused));
    assert_eq!(bus.export_send(0, OUT, b"x"), Err(ExportError::Busy));
    bus.export_open(0, OUT).expect("opens");
    assert_eq!(bus.export_open(0, OUT), Err(ExportError::Busy));
    no_violations(&model);
}

#[test]
fn a_driver_that_goes_leaves_nothing_open_and_the_next_binds_as_the_first() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    model.borrow_mut().serials[0].to_host.extend(b"before");
    assert_eq!(drain(&model, &mut bus, &mut serial, 4096).0, b"before");
    // The driver dies: the host closes what it opened.
    bus.export_release(0);
    assert_eq!(bus.export_send(0, OUT, b"x"), Err(ExportError::Busy));
    let requests = vendor_requests(&model, device).len();
    // The next driver starts afresh, and the chip is set up again.
    let mut again = bind(&model, &mut bus);
    assert!(vendor_requests(&model, device).len() > requests);
    model.borrow_mut().serials[0].to_host.extend(b"after");
    assert_eq!(drain(&model, &mut bus, &mut again, 4096).0, b"after");
    assert_eq!(again.write(&mut driver(&model, &mut bus), b"out"), Ok(3));
    pass(&model, &mut bus, 1);
    assert_eq!(model.borrow().serials[0].from_host, b"out");
    no_violations(&model);
}

#[test]
fn an_unplugged_interface_goes_and_comes_back_under_a_new_key() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let _ = bind(&model, &mut bus);
    let key = bus.export(0).expect("exported").key;
    model.borrow_mut().unplug_root(0);
    bus.poll();
    assert_eq!(bus.next_export_change(), Some(ExportChange::Detached(0)));
    assert!(bus.export(0).is_none());
    assert_eq!(bus.export_send(0, OUT, b"gone"), Err(ExportError::Gone));
    assert_eq!(model.borrow().slots_used(), 0);
    model.borrow_mut().plug_root(0, device);
    bus.poll();
    assert_eq!(bus.next_export_change(), Some(ExportChange::Attached(0)));
    assert_ne!(bus.export(0).expect("exported").key, key);
    no_violations(&model);
}

// ---------------------------------------------------------------------------
// FTDI's UARTs
// ---------------------------------------------------------------------------

#[test]
fn an_ft232h_is_set_up_as_a_uart_at_9600_8n1() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let serial = bind(&model, &mut bus);
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
        let mut serial = bind(&model, &mut bus);
        let out: Vec<u8> = (0..3000).map(|at| (at * 7 % 251) as u8).collect();
        assert_eq!(
            serial.write(&mut driver(&model, &mut bus), &out),
            Ok(out.len())
        );
        pass(&model, &mut bus, 1);
        assert_eq!(model.borrow().serials[0].from_host, out);

        let incoming: Vec<u8> = (0..5000).map(|at| (at * 13 % 253) as u8).collect();
        {
            let mut m = model.borrow_mut();
            m.serials[0].status_only = 3;
            m.serials[0].to_host.extend(incoming.iter().copied());
        }
        let (got, flags) = drain(&model, &mut bus, &mut serial, 4096);
        assert_eq!(got, incoming, "{speed:?}");
        assert!(flags.iter().all(|&flag| flag == FLAG_NORMAL));
        assert_eq!(serial.modem, 0xB0);
        assert_eq!(serial.counters.rx, 5000);
        assert_eq!(serial.counters.tx, 3000);
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
    let mut serial = bind(&model, &mut bus);
    let incoming: Vec<u8> = (0..4000).map(|at| (at % 241) as u8).collect();
    model.borrow_mut().serials[0]
        .to_host
        .extend(incoming.iter().copied());
    assert_eq!(
        drain(&model, &mut bus, &mut serial, CHUNK_BYTES).0,
        incoming
    );
    // A buffer too small for a chunk takes nothing, rather than part of one.
    model.borrow_mut().serials[0].to_host.extend([1, 2, 3]);
    pass(&model, &mut bus, 2);
    assert_eq!(
        serial.read(&mut driver(&model, &mut bus), &mut [0; 100], &mut [0; 100]),
        0
    );
    assert_eq!(
        drain(&model, &mut bus, &mut serial, CHUNK_BYTES).0,
        vec![1, 2, 3]
    );
}

#[test]
fn line_errors_mark_their_bytes_as_linux_s_ftdi_sio_does() {
    let (model, mut bus, _) = board(
        chip(Speed::Full, VENDOR_FTDI, PRODUCT_FT232, 0x400),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    // A parity error marks every byte of its packet.
    {
        let mut m = model.borrow_mut();
        m.serials[0].line_error = LINE_PARITY;
        m.serials[0].to_host.extend(b"xyz");
    }
    let (got, flags) = drain(&model, &mut bus, &mut serial, 4096);
    assert_eq!(
        (got.as_slice(), flags.as_slice()),
        (&b"xyz"[..], &[FLAG_PARITY; 3][..])
    );
    // A break marks only the NUL it arrived as.
    {
        let mut m = model.borrow_mut();
        m.serials[0].line_error = LINE_BREAK;
        m.serials[0].to_host.extend(b"a\0");
    }
    let (got, flags) = drain(&model, &mut bus, &mut serial, 4096);
    assert_eq!(
        (got.as_slice(), flags.as_slice()),
        (&b"a\0"[..], &[FLAG_NORMAL, FLAG_BREAK][..])
    );
    // An overrun is a NUL of its own before the packet's bytes.
    {
        let mut m = model.borrow_mut();
        m.serials[0].line_error = LINE_OVERRUN;
        m.serials[0].to_host.extend(b"q");
    }
    let (got, flags) = drain(&model, &mut bus, &mut serial, 4096);
    assert_eq!(
        (got.as_slice(), flags.as_slice()),
        (&b"\0q"[..], &[FLAG_OVERRUN, FLAG_NORMAL][..])
    );
    assert_eq!(
        (
            serial.counters.parity,
            serial.counters.brk,
            serial.counters.overrun
        ),
        (1, 1, 1)
    );
}

#[test]
fn modem_line_changes_are_counted_as_tiocgicount_counts_them() {
    let (model, mut bus, _) = board(
        chip(Speed::Full, VENDOR_FTDI, PRODUCT_FT232, 0x400),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    model.borrow_mut().serials[0].status_only = 1;
    let _ = drain(&model, &mut bus, &mut serial, 4096);
    assert_eq!(serial.modem, 0xB0, "CTS, DSR and carrier up");
    // CTS and carrier drop, the ring indicator rises.
    {
        let mut m = model.borrow_mut();
        m.serials[0].modem = 0x61;
        m.serials[0].status_only = 1;
    }
    let _ = drain(&model, &mut bus, &mut serial, 4096);
    assert_eq!(serial.modem, 0x60);
    let counters = serial.counters;
    assert_eq!(
        (counters.cts, counters.dsr, counters.rng, counters.dcd),
        (1, 0, 1, 1)
    );
}

#[test]
fn a_new_line_and_a_break_are_the_chip_s_own_requests() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    let before = vendor_requests(&model, device).len();
    let line = Line {
        baud: 12_000_000,
        data_bits: 7,
        parity: Parity::Even,
        two_stop_bits: true,
        flow: Flow::RtsCts,
    };
    serial
        .configure(&mut driver(&model, &mut bus), &line)
        .expect("configured");
    assert_eq!(
        vendor_requests(&model, device)[before..],
        [
            (3, 0x0000, 0x0201),
            (4, 7 | (2 << 8) | (2 << 11), 1),
            (2, 0, 0x0101)
        ]
    );
    assert_eq!(serial.line, line);
    assert_eq!(serial.actual_baud(), 12_000_000);
    // A rate the chip cannot have leaves the line as it was.
    assert_eq!(
        serial.configure(&mut driver(&model, &mut bus), &Line { baud: 50, ..line }),
        Err(port::Error::Rate)
    );
    assert_eq!(serial.line, line);
    // An odd rate it can: 250000, DMX's, exactly.
    let dmx = Line {
        baud: 250_000,
        ..Line::DEFAULT
    };
    serial
        .configure(&mut driver(&model, &mut bus), &dmx)
        .expect("250000");
    assert_eq!(serial.actual_baud(), 250_000);
    serial
        .modem(&mut driver(&model, &mut bus), false, true)
        .expect("the modem lines");
    assert_eq!(
        vendor_requests(&model, device).last(),
        Some(&(1, 0x0302, 1))
    );
    // A break is the framing request with bit 14, its end without.
    serial
        .set_break(&mut driver(&model, &mut bus), true)
        .expect("break on");
    assert_eq!(
        vendor_requests(&model, device).last(),
        Some(&(4, 8 | (1 << 14), 1))
    );
    serial
        .set_break(&mut driver(&model, &mut bus), false)
        .expect("break off");
    assert_eq!(vendor_requests(&model, device).last(), Some(&(4, 8, 1)));
    serial
        .set_latency(&mut driver(&model, &mut bus), 1)
        .expect("latency");
    assert_eq!(vendor_requests(&model, device).last(), Some(&(9, 1, 1)));
}

#[test]
fn a_megabyte_through_a_slow_reader_loses_nothing() {
    let (model, mut bus, _) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    let incoming: Vec<u8> = (0..1_048_576_u32)
        .map(|at| (at.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    model.borrow_mut().serials[0]
        .to_host
        .extend(incoming.iter().copied());
    let mut got = Vec::new();
    let (mut data, mut flags) = (vec![0_u8; CHUNK_BYTES], vec![0_u8; CHUNK_BYTES]);
    // The reader takes one chunk every 3 ms: far slower than the chip
    // could send, which the streams' queue holds back.
    for _ in 0..20_000 {
        pass(&model, &mut bus, 3);
        let n = serial.read(&mut driver(&model, &mut bus), &mut data, &mut flags);
        got.extend_from_slice(&data[..n]);
        if got.len() == incoming.len() {
            break;
        }
    }
    assert_eq!(got.len(), incoming.len());
    assert!(got == incoming, "the bytes came in order and whole");
    no_violations(&model);
}

// ---------------------------------------------------------------------------
// A chip that holds its OUT endpoint off
// ---------------------------------------------------------------------------

/// Every report the bus made events of, counted.
fn events_seen(bus: &mut TestBus) -> usize {
    let mut seen = 0;
    while let Some(output) = bus.next_output() {
        if matches!(output, Output::Events(_)) {
            seen += 1;
        }
    }
    seen
}

#[test]
fn a_chip_holding_its_out_endpoint_off_holds_back_only_its_own_bytes() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(super::model::sem_keyboard());
        let ftdi = m.add(chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900));
        m.serials.push(Far::new(ftdi, 512));
        m.plug_root(0, keyboard);
        m.plug_root(1, ftdi);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    assert_eq!(bus.next_export_change(), Some(ExportChange::Attached(0)));
    let mut serial = bind(&model, &mut bus);
    let _ = events_seen(&mut bus);

    // CTS low: the chip NAKs every OUT packet.
    model.borrow_mut().serials[0].hold_out = true;
    let out: Vec<u8> = (0..10_000).map(|at| (at * 11 % 251) as u8).collect();
    let clock = Time(model.clone());
    let before = clock.now_nanos();
    let queued = serial
        .write(&mut driver(&model, &mut bus), &out)
        .expect("queued");
    // Queued at once, not waited for: no model time passed.
    assert_eq!(clock.now_nanos(), before, "the write waited for the chip");
    assert_eq!(queued, 6 * 512);
    assert_eq!(
        serial.write(&mut driver(&model, &mut bus), &out[queued..]),
        Ok(0),
        "a full stream takes nothing"
    );
    assert_eq!(bus.export_in_flight(0, OUT), queued);

    // The keyboard on the same controller types all the while.
    for _ in 0..20 {
        model
            .borrow_mut()
            .report(keyboard, 1, &[0, 0, 4, 0, 0, 0, 0, 0]);
        pass(&model, &mut bus, 2);
        model.borrow_mut().report(keyboard, 1, &[0; 8]);
        pass(&model, &mut bus, 2);
    }
    assert_eq!(events_seen(&mut bus), 40, "every key went through");
    assert!(model.borrow().serials[0].from_host.is_empty());
    assert_eq!(bus.export_in_flight(0, OUT), queued);

    // CTS up again: what was held goes, then the rest, in order.
    model.borrow_mut().serials[0].hold_out = false;
    let mut sent = queued;
    for _ in 0..1000 {
        pass(&model, &mut bus, 1);
        sent += serial
            .write(&mut driver(&model, &mut bus), &out[sent..])
            .expect("queued");
        if sent == out.len() && bus.export_in_flight(0, OUT) == 0 {
            break;
        }
    }
    assert_eq!(model.borrow().serials[0].from_host, out);
    assert_eq!(bus.export_take_lost(0, OUT), 0);
    no_violations(&model);
}

#[test]
fn a_stalled_out_endpoint_says_what_it_lost_and_sends_again() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    model.borrow_mut().serials[0].stall_out = true;
    assert_eq!(
        serial.write(&mut driver(&model, &mut bus), &[7; 1000]),
        Ok(1000)
    );
    pass(&model, &mut bus, 3);
    assert_eq!(bus.export_take_lost(0, OUT), 1000);
    assert!(model.borrow().cleared_halts.contains(&(device, OUT)));
    model.borrow_mut().serials[0].stall_out = false;
    assert_eq!(serial.write(&mut driver(&model, &mut bus), b"again"), Ok(5));
    pass(&model, &mut bus, 2);
    assert_eq!(model.borrow().serials[0].from_host, b"again");
    no_violations(&model);
}

// ---------------------------------------------------------------------------
// The USB-Blaster
// ---------------------------------------------------------------------------

fn max10() -> Tap {
    Tap::new(MAX10_IR, MAX10_IDCODE_INSTRUCTION, Some(MAX10_IDCODE))
}

fn blaster_board(taps: Vec<Tap>) -> (Shared, TestBus, Serial) {
    let (model, mut bus, _) = board(
        chip(Speed::Full, VENDOR_ALTERA, PRODUCT_USB_BLASTER, 0x400),
        0,
        Some(Cable::new(taps)),
    );
    let serial = bind(&model, &mut bus);
    assert_eq!(serial.role, Role::Blaster);
    (model, bus, serial)
}

#[test]
fn a_blaster_reads_the_max10_s_idcode() {
    let (model, mut bus, serial) = blaster_board(vec![max10()]);
    let mut usb = driver(&model, &mut bus);
    let mut cable = Link::new(&mut usb, &serial).expect("a cable");
    let mut jtag = Jtag::new(&mut cable);
    let mut taps = [blaster::Tap::Bypass; MAX_TAPS];
    assert_eq!(jtag.idcodes(&mut taps), Ok(1));
    assert_eq!(taps[0], blaster::Tap::Idcode(MAX10_IDCODE));
    assert_eq!(blaster::fields(MAX10_IDCODE), (0, 0x3182, 0x6E));
    let m = model.borrow();
    let far = m.serials[0].cable.as_ref().expect("the cable");
    assert!(far.faults.is_empty(), "{:?}", far.faults);
    no_violations(&model);
}

#[test]
fn ir_and_dr_scans_read_what_the_tap_captured() {
    let (model, mut bus, serial) = blaster_board(vec![max10()]);
    {
        let mut usb = driver(&model, &mut bus);
        let mut cable = Link::new(&mut usb, &serial).expect("a cable");
        let mut jtag = Jtag::new(&mut cable);
        jtag.reset().expect("reset");
        let mut tdo = [0_u8; 2];
        jtag.scan(true, MAX10_IR, &[0x06, 0x00], &mut tdo)
            .expect("the IR scan");
        assert_eq!(u16::from_le_bytes(tdo) & 0x3FF, 0b01);
        let mut idcode = [0_u8; 4];
        jtag.scan(false, 32, &[0; 4], &mut idcode)
            .expect("the DR scan");
        assert_eq!(u32::from_le_bytes(idcode), MAX10_IDCODE);
        jtag.scan(true, MAX10_IR, &[0xFF, 0x03], &mut tdo)
            .expect("BYPASS");
        let mut out = [0_u8; 2];
        jtag.scan(false, 16, &[0xA5, 0x5A], &mut out)
            .expect("through BYPASS");
        assert_eq!(u16::from_le_bytes(out), 0x5AA5 << 1);
    }
    let m = model.borrow();
    let far = m.serials[0].cable.as_ref().expect("the cable");
    assert_eq!(far.taps[0].updates, vec![0x006, 0x3FF]);
    assert!(far.faults.is_empty(), "{:?}", far.faults);
}

#[test]
fn a_chain_is_read_tap_by_tap_bypassed_ones_too() {
    let other = 0x020F_30DD;
    let (model, mut bus, serial) = blaster_board(vec![
        Tap::new(8, 0x01, Some(other)),
        Tap::new(4, 0x01, None),
        max10(),
    ]);
    let mut usb = driver(&model, &mut bus);
    let mut cable = Link::new(&mut usb, &serial).expect("a cable");
    let mut taps = [blaster::Tap::Bypass; MAX_TAPS];
    assert_eq!(Jtag::new(&mut cable).idcodes(&mut taps), Ok(3));
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
    let (model, mut bus, serial) = blaster_board(Vec::new());
    let mut taps = [blaster::Tap::Bypass; MAX_TAPS];
    {
        let mut usb = driver(&model, &mut bus);
        let mut cable = Link::new(&mut usb, &serial).expect("a cable");
        assert_eq!(Jtag::new(&mut cable).idcodes(&mut taps), Ok(0));
    }
    let bypassed = (0..40).map(|_| Tap::new(2, 0x01, None)).collect();
    let (model, mut bus, serial) = blaster_board(bypassed);
    let mut usb = driver(&model, &mut bus);
    let mut cable = Link::new(&mut usb, &serial).expect("a cable");
    assert_eq!(
        Jtag::new(&mut cable).idcodes(&mut taps),
        Err(blaster::Error::Chain)
    );
}

#[test]
fn a_uart_is_no_jtag_cable() {
    let (model, mut bus, _) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let serial = bind(&model, &mut bus);
    assert!(Link::new(&mut driver(&model, &mut bus), &serial).is_none());
}

// ---------------------------------------------------------------------------
// The FT232H's MPSSE loopback
// ---------------------------------------------------------------------------

#[test]
fn an_ft232h_s_mpsse_loopback_brings_the_pattern_back_and_the_uart_too() {
    let (model, mut bus, device) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    let line = Line {
        baud: 115_200,
        ..Line::DEFAULT
    };
    serial
        .configure(&mut driver(&model, &mut bus), &line)
        .expect("configured");
    model.borrow_mut().serials[0].mpsse = Some(super::model::ftdi::Mpsse::default());
    let before = vendor_requests(&model, device).len();
    let pattern: Vec<u8> = (0..4096_u32).map(|at| (at * 37 % 251) as u8).collect();
    let outcome =
        ferrix_usb_serial::mpsse::loopback(&mut driver(&model, &mut bus), &mut serial, &pattern);
    assert_eq!(outcome, Ok(ferrix_usb_serial::mpsse::Outcome::Equal));
    let requests = &vendor_requests(&model, device)[before..];
    // MPSSE in, and the chip's own mode again after, and the line it had.
    assert!(requests.contains(&(0x0B, 0x0200, 1)), "{requests:?}");
    assert_eq!(
        requests.iter().filter(|r| **r == (0x0B, 0x0000, 1)).count(),
        2
    );
    let divisor = ftdi::divisor(Chip::H, 115_200).expect("a divisor");
    assert!(requests.contains(&(3, divisor as u16, (((divisor >> 16) as u16) << 8) | 1)));
    assert_eq!(serial.line, line);
    no_violations(&model);
}

#[test]
fn a_loopback_that_does_not_loop_says_where_it_differs() {
    let (model, mut bus, _) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    let mut broken = super::model::ftdi::Mpsse::default();
    broken.broken = true;
    model.borrow_mut().serials[0].mpsse = Some(broken);
    let pattern = [0x55_u8; 64];
    assert_eq!(
        ferrix_usb_serial::mpsse::loopback(&mut driver(&model, &mut bus), &mut serial, &pattern),
        Ok(ferrix_usb_serial::mpsse::Outcome::Differs(0))
    );
    // A chip with no engine answers no synchronisation.
    let (model, mut bus, _) = board(
        chip(Speed::High, VENDOR_FTDI, PRODUCT_FT232H, 0x900),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    assert_eq!(
        ferrix_usb_serial::mpsse::loopback(&mut driver(&model, &mut bus), &mut serial, &pattern),
        Err(port::Error::Timeout)
    );
    // And a B-family chip has no engine to run it on.
    let (model, mut bus, _) = board(
        chip(Speed::Full, VENDOR_FTDI, PRODUCT_FT232, 0x400),
        0,
        None,
    );
    let mut serial = bind(&model, &mut bus);
    assert_eq!(
        ferrix_usb_serial::mpsse::loopback(&mut driver(&model, &mut bus), &mut serial, &pattern),
        Err(port::Error::Unknown)
    );
}
