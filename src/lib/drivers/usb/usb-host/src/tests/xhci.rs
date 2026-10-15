//! The bus behind an xHCI controller, against the model of one: the same
//! devices as the EHCI tests', found by slots and rings instead of queue
//! heads, and a disk.

use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use ferrix_inputctl::message::RawEvent;
use ferrix_linux_abi::input::{EV_KEY, EV_SYN, SYN_REPORT};

use super::model::xhci::{Memory, Regs, Shared, Time, Xhci, stick, super_mouse};
use super::model::{g502, sem_keyboard, usb2514b};
use crate::bus::{Bus, DiskChange, Error as BusError, Note, Output, Parent};
use crate::host::{self, Host};
use crate::storage;
use crate::usb::Speed;
use crate::xhci::{Controller, area_bytes};
use crate::{MILLISECOND, Parts, Registers};

type TestBus = Bus<Controller<Regs, Memory, Time>>;

const KEY_A: u16 = 30;

fn parts(model: &Shared) -> Parts<Regs, Memory, Time> {
    Parts {
        registers: Regs(model.clone()),
        memory: Memory(model.clone()),
        clock: Time(model.clone()),
    }
}

fn started(model: &Shared) -> TestBus {
    Bus::with(
        Controller::start(parts(model))
            .map_err(|(error, _)| error)
            .expect("the controller starts"),
    )
}

/// Everything the bus has to hand on: attachments, detachments, and each
/// report's events.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Got {
    Attached(usize),
    Events(usize, Vec<RawEvent>),
    Detached(usize),
    Disk(usize),
    DiskGone(usize),
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
    while let Some(change) = bus.next_disk_change() {
        got.push(match change {
            DiskChange::Attached(disk) => Got::Disk(disk),
            DiskChange::Detached(disk) => Got::DiskGone(disk),
        });
    }
    got
}

fn notes(bus: &mut TestBus) -> Vec<Note> {
    core::iter::from_fn(|| bus.next_note()).collect()
}

fn attached(outputs: &[Got]) -> Vec<usize> {
    outputs
        .iter()
        .filter_map(|got| match got {
            Got::Attached(function) => Some(*function),
            _ => None,
        })
        .collect()
}

/// Let `ms` milliseconds pass, serving the controller each one.
fn pass(model: &Shared, bus: &mut TestBus, ms: u64) {
    for _ in 0..ms {
        Time(model.clone()).sleep(MILLISECOND);
        bus.on_interrupt().expect("the controller runs");
    }
}

impl Time {
    fn sleep(&mut self, nanos: u64) {
        crate::Clock::sleep_nanos(self, nanos);
    }
}

fn no_violations(model: &Shared) {
    let model = model.borrow();
    assert!(model.violations.is_empty(), "{:#?}", model.violations);
}

fn key(code: u16, down: bool) -> RawEvent {
    RawEvent::new(EV_KEY, code, i32::from(down))
}

fn syn() -> RawEvent {
    RawEvent::new(EV_SYN, SYN_REPORT, 0)
}

/// The board the EHCI tests have, behind xHCI: the hub on the second USB 2
/// root port, the mouse and the keyboard behind it.
fn hub_board(wide_contexts: bool) -> (Shared, TestBus, usize, usize, usize) {
    let model = Xhci::new();
    let (hub, mouse, keyboard) = {
        let mut m = model.borrow_mut();
        m.wide_contexts = wide_contexts;
        let hub = m.add(usb2514b());
        let mouse = m.add(g502());
        let keyboard = m.add(sem_keyboard());
        m.plug_root(1, hub);
        m.plug(hub, 1, mouse);
        m.plug(hub, 3, keyboard);
        (hub, mouse, keyboard)
    };
    let mut bus = started(&model);
    bus.poll();
    (model, bus, hub, mouse, keyboard)
}

#[test]
fn the_controller_is_taken_from_firmware_and_started() {
    let model = Xhci::new();
    let _bus = started(&model);
    no_violations(&model);
    let m = model.borrow();
    assert!(!m.firmware_owned, "firmware let go");
    assert_eq!(
        m.legacy_control & 0xE000_E011,
        0,
        "SMIs off, changes cleared"
    );
    assert_eq!(
        area_bytes(&Regs(model.clone())),
        Some(m.memory.len()),
        "the area is sized from the scratchpad count"
    );
}

#[test]
fn firmware_that_never_lets_go_loses_the_controller_anyway() {
    let model = Xhci::new();
    model.borrow_mut().firmware_lets_go = false;
    let _bus = started(&model);
    no_violations(&model);
    assert!(!model.borrow().firmware_owned);
}

#[test]
fn a_low_speed_keyboard_on_a_root_port_types() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    let found = attached(&outputs(&mut bus));
    assert_eq!(found.len(), 2, "the boot keyboard and its media keys");
    let said = notes(&mut bus);
    assert!(
        said.iter().any(|note| matches!(
            note,
            Note::Device {
                speed: Speed::Low,
                vendor: 0x1A2C,
                functions: 2,
                address: 41,
                ..
            }
        )),
        "the address is the controller's own: {said:?}"
    );

    model
        .borrow_mut()
        .report(keyboard, 1, &[0, 0, 4, 0, 0, 0, 0, 0]);
    pass(&model, &mut bus, 2);
    model.borrow_mut().report(keyboard, 1, &[0; 8]);
    pass(&model, &mut bus, 2);
    let events: Vec<Vec<RawEvent>> = outputs(&mut bus)
        .into_iter()
        .filter_map(|got| match got {
            Got::Events(_, events) => Some(events),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        vec![
            vec![key(KEY_A, true), syn()],
            vec![key(KEY_A, false), syn()]
        ]
    );
    no_violations(&model);
}

#[test]
fn a_hub_and_what_is_behind_it_are_found_by_route_and_translator() {
    for wide_contexts in [false, true] {
        let (model, mut bus, _, mouse, _) = hub_board(wide_contexts);
        let found = attached(&outputs(&mut bus));
        let said = notes(&mut bus);
        assert_eq!(
            found.len(),
            4,
            "two functions each, contexts {wide_contexts}: {said:?} {:?}",
            model.borrow().violations
        );
        assert!(said.iter().any(|note| matches!(
            note,
            Note::Device {
                hub_ports: Some(4),
                speed: Speed::High,
                ..
            }
        )));
        // The mouse moves: a report on its first interface.
        model
            .borrow_mut()
            .report(mouse, 1, &[1, 0, 5, 0, 0, 0, 0, 0]);
        pass(&model, &mut bus, 2);
        assert!(
            outputs(&mut bus)
                .iter()
                .any(|got| matches!(got, Got::Events(..))),
            "the mouse's report made events"
        );
        no_violations(&model);
        assert_eq!(model.borrow().slots_used(), 3);
    }
}

#[test]
fn a_superspeed_device_on_a_usb3_port_needs_no_reset_and_takes_512_byte_packets() {
    let model = Xhci::new();
    {
        let mut m = model.borrow_mut();
        let mouse = m.add(super_mouse());
        m.plug_root(2, mouse);
    }
    let mut bus = started(&model);
    bus.poll();
    assert_eq!(attached(&outputs(&mut bus)).len(), 2);
    assert!(notes(&mut bus).iter().any(|note| matches!(
        note,
        Note::Device {
            speed: Speed::Super,
            ..
        }
    )));
    no_violations(&model);
}

#[test]
fn unplugging_and_plugging_again_gives_every_slot_and_page_back() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    // More rounds than the pool has pages for, were any kept.
    for round in 0..40 {
        bus.poll();
        let got = outputs(&mut bus);
        assert_eq!(attached(&got).len(), 2, "round {round}: {got:?}");
        assert_eq!(model.borrow().slots_used(), 1);
        model.borrow_mut().unplug_root(0);
        bus.poll();
        let got = outputs(&mut bus);
        assert_eq!(
            got.iter()
                .filter(|got| matches!(got, Got::Detached(_)))
                .count(),
            2,
            "round {round}: {got:?}"
        );
        assert_eq!(model.borrow().slots_used(), 0, "the slot was disabled");
        model.borrow_mut().plug_root(0, keyboard);
    }
    no_violations(&model);
}

#[test]
fn key_cycles_with_an_led_report_each_cross_both_rings_many_times() {
    use ferrix_linux_abi::input::{EV_LED, LED_CAPSL};
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    let _ = outputs(&mut bus);
    let _ = notes(&mut bus);
    // Each cycle: a press, a release, and the LED report a console sends
    // back -- three TRBs on the control ring, two on the interrupt ring.
    // 300 cycles lap the control ring three times and the interrupt ring
    // twice, past every count of ten reports on the way.
    for cycle in 0..300_u32 {
        model
            .borrow_mut()
            .report(keyboard, 1, &[0, 0, 4, 0, 0, 0, 0, 0]);
        pass(&model, &mut bus, 1);
        model.borrow_mut().report(keyboard, 1, &[0; 8]);
        pass(&model, &mut bus, 1);
        let got = outputs(&mut bus);
        assert_eq!(
            got,
            vec![
                Got::Events(0, vec![key(KEY_A, true), syn()]),
                Got::Events(0, vec![key(KEY_A, false), syn()])
            ],
            "cycle {cycle}"
        );
        let caps = [RawEvent::new(EV_LED, LED_CAPSL, i32::from(cycle % 2 == 0))];
        assert_eq!(bus.set_leds(0, &caps), Ok(true), "cycle {cycle}");
        bus.poll();
        let said = notes(&mut bus);
        assert!(
            !said.iter().any(|note| matches!(note, Note::Gone { .. })),
            "cycle {cycle}: {said:?}"
        );
    }
    no_violations(&model);
}

#[test]
fn a_root_device_let_go_is_said_with_its_portsc_and_the_port_event() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let _ = keyboard;
    let mut bus = started(&model);
    bus.poll();
    let _ = outputs(&mut bus);
    let _ = notes(&mut bus);
    model.borrow_mut().unplug_root(0);
    pass(&model, &mut bus, 1);
    bus.poll();
    let said = notes(&mut bus);
    assert!(
        said.iter()
            .any(|note| matches!(note, Note::PortEvent { port: 0, .. })),
        "{said:?}"
    );
    assert!(
        said.iter().any(|note| matches!(
            note,
            Note::RootGone { port: 0, status, changed: true } if status & 1 == 0
        )),
        "{said:?}"
    );
}

#[test]
fn a_device_behind_the_hub_comes_and_goes_while_the_other_reports() {
    let (model, mut bus, hub, mouse, keyboard) = hub_board(false);
    let _ = outputs(&mut bus);
    model.borrow_mut().unplug(hub, 3);
    bus.poll();
    assert_eq!(
        outputs(&mut bus)
            .iter()
            .filter(|got| matches!(got, Got::Detached(_)))
            .count(),
        2
    );
    assert_eq!(model.borrow().slots_used(), 2);
    model
        .borrow_mut()
        .report(mouse, 1, &[0, 0, 1, 0, 0, 0, 0, 0]);
    pass(&model, &mut bus, 2);
    assert!(
        outputs(&mut bus)
            .iter()
            .any(|got| matches!(got, Got::Events(..)))
    );
    model.borrow_mut().plug(hub, 3, keyboard);
    bus.poll();
    assert_eq!(attached(&outputs(&mut bus)).len(), 2);
    no_violations(&model);
}

#[test]
fn a_port_change_event_brings_the_next_poll_forward() {
    let model = Xhci::new();
    let mut bus = started(&model);
    bus.poll();
    let later = bus.next_poll();
    let keyboard = model.borrow_mut().add(sem_keyboard());
    model.borrow_mut().plug_root(0, keyboard);
    assert!(
        model.borrow().interrupting(),
        "the plug raised an interrupt"
    );
    bus.on_interrupt().expect("the controller runs");
    assert!(!model.borrow().interrupting(), "and it was acknowledged");
    assert!(bus.next_poll() < later, "the poll is due at once");
}

#[test]
fn a_stalled_request_leaves_the_control_endpoint_working() {
    let model = Xhci::new();
    {
        let mut m = model.borrow_mut();
        let mut keyboard = sem_keyboard();
        keyboard.stall_report_descriptor = true;
        let keyboard = m.add(keyboard);
        m.plug_root(0, keyboard);
    }
    let mut bus = started(&model);
    bus.poll();
    // Its report descriptors stall, so only the boot keyboard is a function,
    // by the boot protocol: every request after each stall still worked.
    assert_eq!(attached(&outputs(&mut bus)).len(), 1);
    no_violations(&model);
    // Reset Endpoint (14) then Set TR Dequeue Pointer (16), for each stall.
    let commands = model.borrow().commands.clone();
    let resets = commands.iter().filter(|&&kind| kind == 14).count();
    assert!(resets >= 2, "{commands:?}");
    assert_eq!(resets, commands.iter().filter(|&&kind| kind == 16).count());
}

#[test]
fn a_pipe_that_keeps_stalling_is_recovered_then_stopped_and_said_so() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    let _ = outputs(&mut bus);
    let _ = notes(&mut bus);
    model.borrow_mut().world.devices[keyboard].stall_interrupts = true;
    pass(&model, &mut bus, 20);
    let said = notes(&mut bus);
    assert_eq!(
        said.iter()
            .filter(|note| matches!(note, Note::Stopped { .. }))
            .count(),
        2,
        "{said:?}"
    );
    // Each recovery cleared the halt on the device too.
    let cleared = model.borrow().cleared_halts.clone();
    assert!(cleared.contains(&(keyboard, 0x81)) && cleared.contains(&(keyboard, 0x82)));
    assert_eq!(cleared.len(), 8, "four tries a pipe: {cleared:?}");
    no_violations(&model);
}

#[test]
fn a_stall_on_an_interrupt_pipe_is_said_with_its_code() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    let _ = outputs(&mut bus);
    let _ = notes(&mut bus);
    model.borrow_mut().world.devices[keyboard].stall_interrupts = true;
    pass(&model, &mut bus, 2);
    let said = notes(&mut bus);
    assert!(
        said.iter().any(|note| matches!(
            note,
            Note::Transfer(host::Trouble {
                code: 6,
                known: true,
                ..
            })
        )),
        "{said:?}"
    );
}

#[test]
fn an_event_for_a_trb_it_did_not_queue_is_said_and_the_pipe_kept_armed() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    let _ = outputs(&mut bus);
    let _ = notes(&mut bus);
    // More strays than requests queued: a pipe that was not re-armed for
    // each would run dry, and the reports after them would never come.
    model.borrow_mut().stray_pointers = 12;
    for _ in 0..12 {
        model
            .borrow_mut()
            .report(keyboard, 1, &[0, 0, 4, 0, 0, 0, 0, 0]);
        pass(&model, &mut bus, 2);
    }
    let said = notes(&mut bus);
    assert!(
        said.iter()
            .any(|note| matches!(note, Note::Transfer(host::Trouble { known: false, .. }))),
        "{said:?}"
    );
    let _ = outputs(&mut bus);
    model
        .borrow_mut()
        .report(keyboard, 1, &[0, 0, 5, 0, 0, 0, 0, 0]);
    pass(&model, &mut bus, 2);
    assert!(
        outputs(&mut bus).contains(&Got::Events(0, vec![key(48, true), syn()])),
        "the pipe still carries reports"
    );
    no_violations(&model);
}

#[test]
fn a_pipe_recovers_when_the_stall_was_once() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    let _ = outputs(&mut bus);
    model.borrow_mut().world.devices[keyboard].stall_interrupts = true;
    pass(&model, &mut bus, 2);
    model.borrow_mut().world.devices[keyboard].stall_interrupts = false;
    model
        .borrow_mut()
        .report(keyboard, 1, &[0, 0, 4, 0, 0, 0, 0, 0]);
    pass(&model, &mut bus, 3);
    assert!(outputs(&mut bus).contains(&Got::Events(0, vec![key(KEY_A, true), syn()])));
    no_violations(&model);
}

#[test]
fn many_reports_cross_the_ring_s_link_in_order() {
    let model = Xhci::new();
    let keyboard = {
        let mut m = model.borrow_mut();
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, keyboard);
        keyboard
    };
    let mut bus = started(&model);
    bus.poll();
    let _ = outputs(&mut bus);
    // Three laps of a 255-request ring, and of the event ring.
    let mut down = 0;
    for round in 0..800_u32 {
        let pressed = round % 2 == 0;
        let report = [0, 0, if pressed { 4 } else { 0 }, 0, 0, 0, 0, 0];
        model.borrow_mut().report(keyboard, 1, &report);
        pass(&model, &mut bus, 1);
        for got in outputs(&mut bus) {
            assert_eq!(got, Got::Events(0, vec![key(KEY_A, pressed), syn()]));
            down += 1;
        }
    }
    assert_eq!(down, 800);
    no_violations(&model);
}

#[test]
fn reports_that_arrive_during_a_transfer_are_kept_for_the_next_service() {
    let (model, mut bus, _, mouse, keyboard) = hub_board(false);
    let _ = outputs(&mut bus);
    {
        let mut m = model.borrow_mut();
        m.report(keyboard, 1, &[0, 0, 4, 0, 0, 0, 0, 0]);
        m.report(mouse, 1, &[1, 0, 0, 0, 0, 0, 0, 0]);
    }
    // A poll is control transfers to the hub, each of which reads the event
    // ring while it waits.
    Time(Rc::clone(&model)).sleep(MILLISECOND);
    bus.poll();
    assert!(
        outputs(&mut bus).is_empty(),
        "nothing is handed on mid-poll"
    );
    bus.on_interrupt().expect("the controller runs");
    let got = outputs(&mut bus);
    assert_eq!(
        got.iter()
            .filter(|got| matches!(got, Got::Events(..)))
            .count(),
        2,
        "{got:?}"
    );
}

#[test]
fn a_controller_that_stops_answering_is_given_up() {
    let model = Xhci::new();
    let mut bus = started(&model);
    bus.poll();
    let keyboard = model.borrow_mut().add(sem_keyboard());
    model.borrow_mut().mute = true;
    model.borrow_mut().plug_root(0, keyboard);
    bus.poll();
    assert!(
        notes(&mut bus).iter().any(|note| matches!(
            note,
            Note::Failed {
                parent: Parent::Root(0),
                error: BusError::Transfer(host::Error::Timeout)
            }
        )),
        "the Enable Slot command timed out"
    );
    assert_eq!(bus.on_interrupt(), Err(host::Error::Unresponsive));
}

#[test]
fn pointers_past_4_gib_are_refused_by_a_32_bit_controller() {
    let model = Xhci::new();
    model.borrow_mut().wide_pointers = false;
    let refused = Controller::start(parts(&model))
        .map(drop)
        .map_err(|(error, _)| error);
    assert_eq!(refused, Err(host::Error::Memory));
}

#[test]
fn the_controller_stops_and_gives_its_memory_back_unless_it_will_not_halt() {
    let model = Xhci::new();
    let bus = started(&model);
    assert!(bus.shutdown().is_ok());
    let model = Xhci::new();
    let bus = started(&model);
    model.borrow_mut().wedged = true;
    assert!(bus.shutdown().is_err());
}

#[test]
fn the_model_notices_a_port_write_that_would_disable_the_port() {
    let model = Xhci::new();
    let _bus = started(&model);
    // PORTSC of port 0, written back as read while enabled: PED is bit 1.
    Regs(model.clone()).write32(0x20 + 0x400, (1 << 9) | (1 << 25) | (1 << 1));
    assert_eq!(model.borrow().violations.len(), 1);
}

// ---------------------------------------------------------------------------
// Disks
// ---------------------------------------------------------------------------

fn disk_board(speed: Speed, port: usize) -> (Shared, TestBus, usize) {
    let model = Xhci::new();
    let device = {
        let mut m = model.borrow_mut();
        let device = m.add(stick(speed));
        m.add_disk(device, 300);
        m.plug_root(port, device);
        device
    };
    let mut bus = started(&model);
    bus.poll();
    (model, bus, device)
}

#[test]
fn a_stick_is_a_disk_that_reads_writes_and_flushes() {
    for (speed, port) in [(Speed::High, 0), (Speed::Super, 3)] {
        let (model, mut bus, _) = disk_board(speed, port);
        assert_eq!(outputs(&mut bus), vec![Got::Disk(0)]);
        let disk = *bus.disk(0).expect("the disk");
        assert_eq!((disk.blocks, disk.block_bytes), (300, 512));
        assert_eq!(&disk.vendor, b"FERRIX  ");

        // 200 blocks: two commands, and a page at a time within each.
        let mut read = vec![0_u8; 200 * 512];
        bus.disk_read(0, 7, &mut read).expect("the read");
        assert_eq!(read, model.borrow().disks[0].blocks[7 * 512..207 * 512]);

        let written: Vec<u8> = (0..9 * 512).map(|at| (at * 31 % 253) as u8).collect();
        bus.disk_write(0, 290, &written).expect("the write");
        assert_eq!(
            model.borrow().disks[0].blocks[290 * 512..299 * 512],
            written
        );
        let mut back = vec![0_u8; 9 * 512];
        bus.disk_read(0, 290, &mut back).expect("the read back");
        assert_eq!(back, written);

        bus.disk_flush(0).expect("the flush");
        assert_eq!(model.borrow().disks[0].flushes, 1);
        assert_eq!(
            bus.disk_read(0, 299, &mut [0; 1024]),
            Err(storage::Error::Range)
        );
        assert_eq!(
            bus.disk_read(0, 0, &mut [0; 100]),
            Err(storage::Error::Range)
        );
        no_violations(&model);
    }
}

#[test]
fn a_failed_command_says_its_sense_and_the_next_one_works() {
    let (model, mut bus, _) = disk_board(Speed::High, 0);
    model.borrow_mut().disks[0].failing = true;
    let mut block = [0_u8; 512];
    assert_eq!(
        bus.disk_read(0, 0, &mut block),
        Err(storage::Error::Failed(6))
    );
    model.borrow_mut().disks[0].failing = false;
    bus.disk_read(0, 1, &mut block).expect("the next read");
    assert_eq!(block[..], model.borrow().disks[0].blocks[512..1024]);
    no_violations(&model);
}

#[test]
fn a_stalled_bulk_endpoint_is_reset_on_both_sides() {
    let (model, mut bus, device) = disk_board(Speed::High, 0);
    model.borrow_mut().disks[0].stall_in = true;
    let mut block = [0_u8; 512];
    // The data stage stalls; the status that follows is one transfer late,
    // so the two sides are out of step and the transport is reset.
    let first = bus.disk_read(0, 0, &mut block);
    assert!(first.is_err(), "{first:?}");
    assert!(model.borrow().cleared_halts.contains(&(device, 0x81)));
    no_violations(&model);
}

#[test]
fn a_disk_goes_with_its_stick() {
    let (model, mut bus, _) = disk_board(Speed::Super, 2);
    let _ = outputs(&mut bus);
    model.borrow_mut().unplug_root(2);
    bus.poll();
    assert_eq!(outputs(&mut bus), vec![Got::DiskGone(0)]);
    assert!(bus.disk(0).is_none());
    assert_eq!(
        bus.disk_read(0, 0, &mut [0; 512]),
        Err(storage::Error::Unsupported)
    );
    assert_eq!(model.borrow().slots_used(), 0);
}

#[test]
fn an_ehci_bus_has_no_disks_and_says_nothing_of_it() {
    // The trait's defaults: a controller with no bulk pipes.
    let model = super::model::Model::new();
    let parts = Parts {
        registers: super::model::Regs(model.clone()),
        memory: super::model::Memory(model.clone()),
        clock: super::model::Time(model),
    };
    let mut hc = crate::ehci::Controller::start(parts)
        .map_err(|(error, _)| error)
        .expect("the controller starts");
    assert_eq!(hc.bulk(0, host::Data::None), Err(host::Error::Unsupported));
}
