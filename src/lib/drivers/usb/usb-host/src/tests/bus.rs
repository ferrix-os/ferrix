//! The bus against the model of a DK board: the hub, a full-speed mouse and
//! a low-speed keyboard behind it, each with a second HID interface.

use std::vec;
use std::vec::Vec;

use ferrix_inputctl::message::{Events, Hello, Message, PORT_RIGHTS, RawEvent, bit};
use ferrix_inputctl::session::Session;
use ferrix_linux_abi::input::{
    BTN_LEFT, BTN_RIGHT, BUS_USB, EV_KEY, EV_LED, EV_REL, EV_SYN, LED_CAPSL, LED_NUML, REL_HWHEEL,
    REL_WHEEL, REL_X, REL_Y, SYN_REPORT,
};
use ferrix_native_abi::types::DEVICE_NOT_PCI;

use super::model::{Memory, Regs, Shared, Time, dk_board};
use crate::bus::{Bus, Note, Output, Parent};
use crate::hid::Kind;
use crate::usb::Speed;
use crate::{MILLISECOND, Parts};

type TestBus = Bus<crate::ehci::Controller<Regs, Memory, Time>>;

const KEY_A: u16 = 30;
const KEY_B: u16 = 48;
const KEY_F1: u16 = 59;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTCTRL: u16 = 29;
const KEY_VOLUMEUP: u16 = 115;
const KEY_SLEEP: u16 = 142;
/// The sixth mouse button, `BTN_FORWARD`, and the sixteenth.
const BTN_FORWARD: u16 = 0x115;
const BTN_SIXTEENTH: u16 = 0x11F;

const KEYBOARD: &[u8] = b"SEM USB Keyboard";
const MEDIA: &[u8] = b"SEM USB Keyboard Consumer Control";
const MOUSE: &[u8] = b"Logitech G502 HERO Gaming Mouse";
const MACROS: &[u8] = b"Logitech G502 HERO Gaming Mouse Keyboard";

/// What the bus handed on, with an [`Output::Events`]'s events.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Got {
    Attached(usize),
    Events(usize, Vec<RawEvent>),
    Detached(usize),
}

fn parts(model: &Shared) -> Parts<Regs, Memory, Time> {
    Parts {
        registers: Regs(model.clone()),
        memory: Memory(model.clone()),
        clock: Time(model.clone()),
    }
}

fn started(model: &Shared) -> TestBus {
    Bus::start(parts(model))
        .map_err(|(error, _)| error)
        .expect("the controller starts")
}

fn outputs(bus: &mut TestBus) -> Vec<Got> {
    let mut got = Vec::new();
    while let Some(output) = bus.next_output() {
        got.push(match output {
            Output::Attached(function) => Got::Attached(function),
            Output::Events(function) => Got::Events(function, bus.events().to_vec()),
            Output::Detached(function) => Got::Detached(function),
        });
    }
    got
}

fn attached(outputs: &[Got]) -> Vec<usize> {
    outputs
        .iter()
        .filter_map(|output| match output {
            Got::Attached(function) => Some(*function),
            _ => None,
        })
        .collect()
}

fn events(outputs: &[Got], wanted: usize) -> Vec<Vec<RawEvent>> {
    outputs
        .iter()
        .filter_map(|output| match output {
            Got::Events(function, events) if *function == wanted => Some(events.clone()),
            _ => None,
        })
        .collect()
}

/// Let `ms` frames run, then take the interrupt.
fn frames(model: &Shared, bus: &mut TestBus, ms: u64) {
    for _ in 0..ms {
        crate::Clock::sleep_nanos(&mut Time(model.clone()), MILLISECOND);
    }
    bus.on_interrupt().expect("the controller runs");
}

fn key(code: u16, down: bool) -> RawEvent {
    RawEvent::new(EV_KEY, code, i32::from(down))
}

fn rel(code: u16, value: i32) -> RawEvent {
    RawEvent::new(EV_REL, code, value)
}

fn syn() -> RawEvent {
    RawEvent::new(EV_SYN, SYN_REPORT, 0)
}

fn hello(bus: &TestBus, function: usize) -> Hello {
    bus.hello(function, DEVICE_NOT_PCI).expect("a HELLO")
}

/// The function whose HELLO carries `name`, among `found`.
fn named(bus: &TestBus, found: &[usize], name: &[u8]) -> usize {
    *found
        .iter()
        .find(|&&function| hello(bus, function).name.as_bytes() == name)
        .unwrap_or_else(|| panic!("no function {}", std::string::String::from_utf8_lossy(name)))
}

/// Feed `batches` to a core session made from `function`'s HELLO, which
/// must take every one.
fn judge(bus: &TestBus, function: usize, batches: &[Vec<RawEvent>]) {
    let mut session = Session::accept(&hello(bus, function), &[PORT_RIGHTS]).expect("the HELLO");
    for batch in batches {
        let message = Message::Events(Events::new(batch).expect("few enough"));
        let _ = session
            .receive(&message, 0, |_| {})
            .expect("the core takes the report");
    }
}

/// The board, started and polled once.
struct Board {
    model: Shared,
    bus: TestBus,
    /// The model's devices.
    keyboard: usize,
    mouse: usize,
    /// The functions, by name.
    keys: usize,
    media: usize,
    pointer: usize,
    macros: usize,
}

fn board() -> Board {
    let (model, _, mouse, keyboard) = dk_board();
    let mut bus = started(&model);
    bus.poll();
    let found = attached(&outputs(&mut bus));
    assert_eq!(found.len(), 4, "two functions each");
    Board {
        keys: named(&bus, &found, KEYBOARD),
        media: named(&bus, &found, MEDIA),
        pointer: named(&bus, &found, MOUSE),
        macros: named(&bus, &found, MACROS),
        model,
        bus,
        keyboard,
        mouse,
    }
}

impl Board {
    /// Queue `report` on `device`'s `endpoint` and let two frames take it.
    fn send(&mut self, device: usize, endpoint: u8, report: &[u8]) {
        self.model.borrow_mut().report(device, endpoint, report);
        frames(&self.model, &mut self.bus, 2);
    }

    fn no_violations(&self) {
        let model = self.model.borrow();
        assert!(model.violations.is_empty(), "{:?}", model.violations);
    }
}

#[test]
fn the_dk_boards_bus_gives_each_device_two_functions() {
    let mut b = board();
    b.no_violations();
    {
        let m = b.model.borrow();
        assert_eq!(m.devices[0].address, 1);
        assert_eq!(m.devices[b.mouse].address, 2);
        assert_eq!(m.devices[b.keyboard].address, 3);
        for device in [0, b.mouse, b.keyboard] {
            assert_eq!(
                m.devices[device].configured, 1,
                "device {device} configured"
            );
        }
        // Both boot interfaces were put in the report protocol.
        for device in [b.mouse, b.keyboard] {
            assert!(
                m.devices[device]
                    .setups
                    .contains(&[0x21, 0x0B, 1, 0, 0, 0, 0, 0]),
                "device {device}"
            );
            assert!(!m.devices[device].boot_protocol[0]);
        }
    }

    let keys = hello(&b.bus, b.keys);
    assert_eq!(
        (
            keys.id.bustype,
            keys.id.vendor,
            keys.id.product,
            keys.id.version
        ),
        (BUS_USB, 0x1A2C, 0x2124, 0x0116)
    );
    assert_eq!(keys.location, DEVICE_NOT_PCI);
    assert!(keys.bits.has_type(EV_LED) && bit(&keys.bits.leds, LED_CAPSL));
    for function in [b.keys, b.media, b.pointer, b.macros] {
        let _ = Session::accept(&hello(&b.bus, function), &[PORT_RIGHTS]).expect("taken");
    }
    assert_eq!(b.bus.kind(b.media), Some(Kind::Consumer));
    assert_eq!(b.bus.kind(b.macros), Some(Kind::Keyboard));

    let notes: Vec<Note> = std::iter::from_fn(|| b.bus.next_note()).collect();
    assert!(notes.contains(&Note::Device {
        address: 1,
        speed: Speed::High,
        vendor: 0x0424,
        product: 0x2514,
        hub_ports: Some(4),
        functions: 0,
    }));
    assert!(notes.contains(&Note::Device {
        address: 3,
        speed: Speed::Low,
        vendor: 0x1A2C,
        product: 0x2124,
        hub_ports: None,
        functions: 2,
    }));
}

#[test]
fn a_second_poll_finds_nothing_new() {
    let mut b = board();
    let transfers = b.model.borrow().async_runs;
    b.bus.poll();
    assert!(outputs(&mut b.bus).is_empty());
    // Four GET_STATUS, one per hub port, and nothing else.
    assert_eq!(b.model.borrow().async_runs - transfers, 4);
}

#[test]
fn a_key_is_pressed_and_let_go() {
    let mut b = board();
    b.send(b.keyboard, 1, &[0, 0, 0x04, 0, 0, 0, 0, 0]);
    b.send(b.keyboard, 1, &[0; 8]);
    let got = events(&outputs(&mut b.bus), b.keys);
    assert_eq!(
        got,
        [
            vec![key(KEY_A, true), syn()],
            vec![key(KEY_A, false), syn()]
        ]
    );
    judge(&b.bus, b.keys, &got);
    b.no_violations();
}

#[test]
fn modifiers_come_first_and_a_roll_over_changes_nothing() {
    let mut b = board();
    for report in [
        [0x02, 0, 0x04, 0, 0, 0, 0, 0],
        [0x02, 0, 0x04, 0x05, 0, 0, 0, 0],
        [0x02, 0, 1, 1, 1, 1, 1, 1],
        [0x01, 0, 0x05, 0, 0, 0, 0, 0],
    ] {
        b.send(b.keyboard, 1, &report);
    }
    let got = events(&outputs(&mut b.bus), b.keys);
    assert_eq!(
        got,
        [
            vec![key(KEY_LEFTSHIFT, true), key(KEY_A, true), syn()],
            vec![key(KEY_B, true), syn()],
            vec![
                key(KEY_LEFTCTRL, true),
                key(KEY_LEFTSHIFT, false),
                key(KEY_A, false),
                syn()
            ],
        ]
    );
}

#[test]
fn the_mouse_moves_clicks_and_scrolls() {
    let mut b = board();
    for report in [
        [0x01, 0, 5, 0, 0xFD, 0xFF, 0, 0],
        [0x03, 0, 0, 0, 0, 0, 1, 0],
        [0; 8],
    ] {
        b.send(b.mouse, 1, &report);
    }
    let got = events(&outputs(&mut b.bus), b.pointer);
    assert_eq!(
        got,
        [
            vec![key(BTN_LEFT, true), rel(REL_X, 5), rel(REL_Y, -3), syn()],
            vec![key(BTN_RIGHT, true), rel(REL_WHEEL, 1), syn()],
            vec![key(BTN_LEFT, false), key(BTN_RIGHT, false), syn()],
        ]
    );
    judge(&b.bus, b.pointer, &got);
}

/// The buttons past the boot protocol's three, and the wheel's tilt: what
/// reading the mouse's own report descriptor is for.
#[test]
fn the_mouses_side_buttons_and_tilt_arrive() {
    let mut b = board();
    // Button 6 (byte 0 bit 5) and button 16 (byte 1 bit 7); tilt left, and a
    // sixteen-bit move.
    b.send(b.mouse, 1, &[0x20, 0x80, 0x2C, 0x01, 0, 0, 0, 0xFF]);
    b.send(b.mouse, 1, &[0; 8]);
    let got = events(&outputs(&mut b.bus), b.pointer);
    assert_eq!(
        got,
        [
            vec![
                key(BTN_FORWARD, true),
                key(BTN_SIXTEENTH, true),
                rel(REL_X, 300),
                rel(REL_HWHEEL, -1),
                syn()
            ],
            vec![key(BTN_FORWARD, false), key(BTN_SIXTEENTH, false), syn()],
        ]
    );
    judge(&b.bus, b.pointer, &got);
}

#[test]
fn media_and_sleep_keys_arrive_from_the_keyboards_second_interface() {
    let mut b = board();
    b.send(b.keyboard, 2, &[1, 0xE9, 0x00]);
    b.send(b.keyboard, 2, &[1, 0, 0]);
    b.send(b.keyboard, 2, &[2, 0x02]);
    b.send(b.keyboard, 2, &[2, 0x00]);
    let got = events(&outputs(&mut b.bus), b.media);
    assert_eq!(
        got,
        [
            vec![key(KEY_VOLUMEUP, true), syn()],
            vec![key(KEY_VOLUMEUP, false), syn()],
            vec![key(KEY_SLEEP, true), syn()],
            vec![key(KEY_SLEEP, false), syn()],
        ]
    );
    judge(&b.bus, b.media, &got);
    b.no_violations();
}

#[test]
fn the_mouses_macro_keys_are_a_keyboard_and_its_vendor_report_is_nothing() {
    let mut b = board();
    b.send(b.mouse, 2, &[1, 0, 0x3A, 0, 0, 0, 0, 0]);
    b.send(b.mouse, 2, &[0x10, 1, 2, 3, 4, 5, 6]);
    b.send(b.mouse, 2, &[1, 0, 0, 0, 0, 0, 0, 0]);
    let got = events(&outputs(&mut b.bus), b.macros);
    assert_eq!(
        got,
        [
            vec![key(KEY_F1, true), syn()],
            vec![key(KEY_F1, false), syn()]
        ]
    );
}

#[test]
fn caps_lock_lights_the_keyboard() {
    let mut b = board();
    let caps = [RawEvent::new(EV_LED, LED_CAPSL, 1)];
    assert_eq!(b.bus.set_leds(b.keys, &caps), Ok(true));
    // The same state again sends nothing.
    assert_eq!(b.bus.set_leds(b.keys, &caps), Ok(false));
    let num = [RawEvent::new(EV_LED, LED_NUML, 1)];
    assert_eq!(b.bus.set_leds(b.keys, &num), Ok(true));
    let off = [RawEvent::new(EV_LED, LED_CAPSL, 0)];
    assert_eq!(b.bus.set_leds(b.keys, &off), Ok(true));
    // A function with no LEDs sends nothing.
    assert_eq!(b.bus.set_leds(b.pointer, &caps), Ok(false));
    let outputs = b.model.borrow().devices[b.keyboard].outputs.clone();
    assert_eq!(
        outputs,
        [
            (0, 0x0200, vec![0x02]),
            (0, 0x0200, vec![0x03]),
            (0, 0x0200, vec![0x01]),
        ]
    );
    assert!(b.model.borrow().devices[b.mouse].outputs.is_empty());
    b.no_violations();
}

#[test]
fn a_keyboard_whose_report_descriptor_cannot_be_read_uses_the_boot_protocol() {
    let (model, _, _, keyboard) = dk_board();
    model.borrow_mut().devices[keyboard].stall_report_descriptor = true;
    let mut bus = started(&model);
    bus.poll();
    let found = attached(&outputs(&mut bus));
    // The mouse's two, and the keyboard's boot interface alone: its second
    // interface has no layout without its descriptor.
    assert_eq!(found.len(), 3);
    let keys = named(&bus, &found, KEYBOARD);
    assert!(model.borrow().devices[keyboard].boot_protocol[0]);

    model
        .borrow_mut()
        .report(keyboard, 1, &[0, 0, 0x04, 0, 0, 0, 0, 0]);
    frames(&model, &mut bus, 2);
    assert_eq!(
        events(&outputs(&mut bus), keys),
        [vec![key(KEY_A, true), syn()]]
    );

    let caps = [RawEvent::new(EV_LED, LED_CAPSL, 1)];
    assert_eq!(bus.set_leds(keys, &caps), Ok(true));
    assert_eq!(
        model.borrow().devices[keyboard].outputs,
        [(0, 0x0200, vec![0x02])]
    );
}

#[test]
fn a_keyboard_that_refuses_both_gives_no_function() {
    let (model, _, _, keyboard) = dk_board();
    model.borrow_mut().devices[keyboard].stall_report_descriptor = true;
    model.borrow_mut().devices[keyboard].stall_protocol = true;
    let mut bus = started(&model);
    bus.poll();
    let found = attached(&outputs(&mut bus));
    assert_eq!(found.len(), 2, "only the mouse's");
}

/// A pipe that halts packet after packet is started again four times, then
/// left stopped, and a note says so once; the other pipes carry on.
#[test]
fn a_pipe_that_keeps_halting_is_stopped_and_said_so() {
    let mut b = board();
    let _ = std::iter::from_fn(|| b.bus.next_note()).count();
    b.model.borrow_mut().devices[b.keyboard].stall_interrupts = true;
    for _ in 0..20 {
        frames(&b.model, &mut b.bus, 1);
    }
    let notes: Vec<Note> = std::iter::from_fn(|| b.bus.next_note()).collect();
    let stopped: Vec<&Note> = notes
        .iter()
        .filter(|note| matches!(note, Note::Stopped { .. }))
        .collect();
    assert!(
        stopped.contains(&&Note::Stopped { function: b.keys }),
        "{notes:?}"
    );
    assert!(
        stopped.contains(&&Note::Stopped { function: b.media }),
        "{notes:?}"
    );
    assert_eq!(stopped.len(), 2, "once each");
    b.send(b.mouse, 1, &[0, 0, 1, 0, 1, 0, 0, 0]);
    assert_eq!(events(&outputs(&mut b.bus), b.pointer).len(), 1);
}

#[test]
fn many_reports_between_interrupts_keep_their_order() {
    let mut b = board();
    for usage in 0x04..0x0A_u8 {
        b.model
            .borrow_mut()
            .report(b.keyboard, 1, &[0, 0, usage, 0, 0, 0, 0, 0]);
    }
    // Two qTDs per pipe: one interrupt, at most two reports taken; the
    // rest wait in the device, as they would on the wire.
    for _ in 0..8 {
        frames(&b.model, &mut b.bus, 1);
    }
    let got = events(&outputs(&mut b.bus), b.keys);
    let pressed: Vec<u16> = got
        .iter()
        .flatten()
        .filter(|event| event.kind == EV_KEY && event.value == 1)
        .map(|event| event.code)
        .collect();
    assert_eq!(pressed, [30, 48, 46, 32, 18, 33]);
}

#[test]
fn unplugging_the_keyboard_detaches_both_its_functions_and_plugging_it_back_attaches_them() {
    let mut b = board();
    b.model.borrow_mut().unplug(0, 2);
    b.bus.poll();
    let mut gone = outputs(&mut b.bus);
    gone.sort_by_key(|got| match got {
        Got::Detached(function) => *function,
        _ => usize::MAX,
    });
    let mut expected = vec![Got::Detached(b.keys), Got::Detached(b.media)];
    expected.sort_by_key(|got| match got {
        Got::Detached(function) => *function,
        _ => usize::MAX,
    });
    assert_eq!(gone, expected);
    assert!(std::iter::from_fn(|| b.bus.next_note()).any(|note| note == Note::Gone { address: 3 }));

    b.model.borrow_mut().plug(0, 2, b.keyboard);
    b.bus.poll();
    let again = attached(&outputs(&mut b.bus));
    assert_eq!(again.len(), 2);
    let keys = named(&b.bus, &again, KEYBOARD);
    // Its old address was free again.
    assert_eq!(b.model.borrow().devices[b.keyboard].address, 3);

    b.send(b.keyboard, 1, &[0, 0, 0x04, 0, 0, 0, 0, 0]);
    assert_eq!(
        events(&outputs(&mut b.bus), keys),
        [vec![key(KEY_A, true), syn()]]
    );
    b.no_violations();
}

#[test]
fn the_mouse_still_reports_while_the_keyboard_comes_and_goes() {
    let mut b = board();
    b.model.borrow_mut().unplug(0, 2);
    b.bus.poll();
    b.model.borrow_mut().plug(0, 2, b.keyboard);
    b.bus.poll();
    let _ = outputs(&mut b.bus);
    b.send(b.mouse, 1, &[0, 0, 1, 0, 1, 0, 0, 0]);
    assert_eq!(events(&outputs(&mut b.bus), b.pointer).len(), 1);
}

#[test]
fn a_device_that_cannot_be_set_up_is_left_until_it_is_unplugged() {
    let (model, _, _, keyboard) = dk_board();
    model.borrow_mut().devices[keyboard].device_descriptor[7] = 7;
    let mut bus = started(&model);
    bus.poll();
    let notes: Vec<Note> = std::iter::from_fn(|| bus.next_note()).collect();
    assert!(notes.iter().any(|note| matches!(
        note,
        Note::Failed {
            parent: Parent::Hub { port: 2, .. },
            ..
        }
    )));
    let transfers = model.borrow().async_runs;
    bus.poll();
    assert_eq!(
        model.borrow().async_runs - transfers,
        4,
        "no retry, only the status polls"
    );

    model.borrow_mut().devices[keyboard].device_descriptor[7] = 8;
    model.borrow_mut().unplug(0, 2);
    bus.poll();
    model.borrow_mut().plug(0, 2, keyboard);
    bus.poll();
    assert_eq!(
        attached(&outputs(&mut bus)).len(),
        4,
        "the mouse's two, then the keyboard's at last"
    );
}

#[test]
fn the_controller_stops_and_gives_its_memory_back() {
    let b = board();
    assert!(b.bus.shutdown().is_ok());
    assert_ne!(b.model.borrow().read_status() & (1 << 12), 0, "halted");
}

#[test]
fn a_controller_that_will_not_halt_keeps_its_memory() {
    let b = board();
    b.model.borrow_mut().wedged = true;
    assert!(b.bus.shutdown().is_err());
}

#[test]
fn memory_whose_pages_the_controller_cannot_reach_is_refused() {
    #[derive(Debug)]
    struct High(Memory);
    impl crate::Dma for High {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn read32(&self, offset: usize) -> u32 {
            self.0.read32(offset)
        }
        fn write32(&mut self, offset: usize, value: u32) {
            self.0.write32(offset, value);
        }
        fn read8(&self, offset: usize) -> u8 {
            self.0.read8(offset)
        }
        fn write8(&mut self, offset: usize, value: u8) {
            self.0.write8(offset, value);
        }
        fn device_address(&self, offset: usize) -> Option<u32> {
            // The last page is above 4 GiB.
            (offset < 3 * 4096)
                .then(|| self.0.device_address(offset))
                .flatten()
        }
        fn barrier(&self) {}
    }
    let (model, ..) = dk_board();
    let parts = Parts {
        registers: Regs(model.clone()),
        memory: High(Memory(model.clone())),
        clock: Time(model),
    };
    let refused = Bus::start(parts).map(drop).map_err(|(error, _)| error);
    assert_eq!(refused, Err(crate::ehci::Error::Memory));
}
