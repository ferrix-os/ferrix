//! The messages byte for byte, strict decoding, a session driven through
//! everything a device sends and each way a driver can lie, and a queue
//! driven through evdev's drop, flush, clock and read rules.

extern crate std;

use std::vec::Vec;

use ferrix_linux_abi::input::{
    ABS_MT_SLOT, ABS_X, ABS_Y, BTN_LEFT, BUS_VIRTUAL, EV_ABS, EV_FF, EV_KEY, EV_LED, EV_MAX,
    EV_MSC, EV_REL, EV_REP, EV_SND, EV_SW, EV_SYN, Event, KEY_A, KEY_MAX, KEY_RESERVED, LED_CAPSL,
    MSC_SCAN, REL_WHEEL, REL_X, REP_DELAY, REP_PERIOD, SW_MAX, SYN_DROPPED, SYN_MT_REPORT,
    SYN_REPORT,
};
use ferrix_linux_abi::socket::Width;
use ferrix_native_abi::rights::Rights;

use crate::message::*;
use crate::queue::*;
use crate::session::*;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes"))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn set(bits: &mut [u8], index: u16) {
    bits[usize::from(index / 8)] |= 1 << (index % 8);
}

const KEY_B: u16 = 48;

/// A device with a little of everything: keys and a button, a wheel, two
/// axes and a multi-touch one, a scan code, a switch, a LED, repeat, and
/// force feedback the core leaves out.
fn hello() -> Hello {
    let mut bits = Bitmaps::EMPTY;
    for kind in [EV_KEY, EV_REL, EV_ABS, EV_MSC, EV_SW, EV_LED, EV_REP, EV_FF] {
        set(&mut bits.types, kind);
    }
    for key in [KEY_RESERVED, KEY_A, BTN_LEFT] {
        set(&mut bits.keys, key);
    }
    set(&mut bits.rels, REL_X);
    set(&mut bits.rels, REL_WHEEL);
    for axis in [ABS_X, ABS_Y, ABS_MT_SLOT] {
        set(&mut bits.abs, axis);
    }
    set(&mut bits.msc, MSC_SCAN);
    set(&mut bits.sw, 0);
    set(&mut bits.leds, LED_CAPSL);
    let mut axes = [AxisRange::default(); AXES];
    axes[usize::from(ABS_X)] = AxisRange {
        minimum: 0,
        maximum: 32767,
        ..AxisRange::default()
    };
    axes[usize::from(ABS_Y)] = AxisRange {
        minimum: 0,
        maximum: 32767,
        fuzz: 10,
        ..AxisRange::default()
    };
    axes[usize::from(ABS_MT_SLOT)] = AxisRange {
        minimum: 0,
        maximum: 9,
        ..AxisRange::default()
    };
    Hello {
        version: VERSION,
        location: 0x0000_1800,
        id: DeviceId {
            bustype: BUS_VIRTUAL,
            vendor: 0x0627,
            product: 1,
            version: 1,
        },
        name: Text::new(b"QEMU Virtio Keyboard").expect("fits"),
        serial: Text::NONE,
        bits,
        axes,
    }
}

fn ev(kind: u16, code: u16, value: i32) -> RawEvent {
    RawEvent::new(kind, code, value)
}

fn syn() -> RawEvent {
    ev(EV_SYN, SYN_REPORT, 0)
}

fn events(list: &[RawEvent]) -> Message {
    Message::Events(Events::new(list).expect("at most 64"))
}

fn session() -> Session {
    Session::accept(&hello(), &Hello::HANDLE_RIGHTS).expect("a good HELLO")
}

/// Send `list` and collect the reports it delivers. A report count that
/// disagrees with what was delivered comes back as a refusal no test expects.
fn send(session: &mut Session, list: &[RawEvent], now: u64) -> Result<Vec<Report>, Refusal> {
    let mut reports = Vec::new();
    let received = session.receive(&events(list), now, |report| reports.push(*report))?;
    if received
        == (Received::Events {
            reports: reports.len(),
        })
    {
        Ok(reports)
    } else {
        Err(Refusal::Malformed)
    }
}

// -- Bytes ----------------------------------------------------------------------

fn every_message() -> Vec<Message> {
    std::vec![
        Message::Hello(hello()),
        Message::Ready(Ready { node: 3 }),
        Message::Refused(Refusal::Undeclared),
        events(&[ev(EV_KEY, KEY_A, 1), ev(EV_REL, REL_X, -5), syn()]),
        events(&[]),
        Message::Stop,
        Message::Stopped,
        Message::Status(Events::new(&[ev(EV_LED, LED_CAPSL, 1)]).unwrap()),
    ]
}

#[test]
fn every_message_round_trips_at_its_fixed_length() {
    for message in every_message() {
        let encoded = message.encode();
        let bytes = encoded.as_bytes();
        assert_eq!(u32_at(bytes, 0), message.kind());
        assert_eq!(u32_at(bytes, 4) as usize, bytes.len());
        assert_eq!(Message::length_of(message.kind()), Some(bytes.len()));
        assert_eq!(Message::decode(bytes), Ok(message), "{message:?}");
    }
    let full: Vec<RawEvent> = (0..MAX_EVENTS).map(|_| syn()).collect();
    assert!(Events::new(&full).is_some());
    let over: Vec<RawEvent> = (0..=MAX_EVENTS).map(|_| syn()).collect();
    assert_eq!(Events::new(&over), None);
}

#[test]
fn fields_lie_where_the_specification_puts_them() {
    let hello = Message::Hello(hello()).encode();
    let bytes = hello.as_bytes();
    assert_eq!(bytes.len(), 1688);
    assert_eq!((u16_at(bytes, 8), u32_at(bytes, 12)), (1, 0x1800));
    assert_eq!(
        [16, 18, 20, 22].map(|at| u16_at(bytes, at)),
        [BUS_VIRTUAL, 0x0627, 1, 1]
    );
    assert_eq!((u16_at(bytes, 24), u16_at(bytes, 26)), (20, 0));
    assert_eq!(&bytes[32..52], b"QEMU Virtio Keyboard");
    assert!(bytes[52..288].iter().all(|&byte| byte == 0));
    // Types at 292: KEY, REL, ABS, MSC, SW (bits 1 to 5), LED (17), REP (20),
    // FF (21).
    assert_eq!(u32_at(bytes, 292), 0b11_0010_0000_0000_0011_1110);
    assert_eq!(bytes[296], 0b0000_0001, "KEY_RESERVED");
    assert_eq!(bytes[296 + 3], 0b0100_0000, "KEY_A, 30");
    assert_eq!(bytes[296 + 34], 0b0000_0001, "BTN_LEFT, 0x110");
    assert_eq!(u16_at(bytes, 392), 0b1_0000_0001, "REL_X and REL_WHEEL");
    assert_eq!(bytes[394], 0b11, "ABS_X and ABS_Y");
    assert_eq!(bytes[394 + 5], 0b1000_0000, "ABS_MT_SLOT, 0x2f");
    assert_eq!(bytes[402], 0b1_0000, "MSC_SCAN");
    assert_eq!(bytes[403], 1, "switch 0");
    assert_eq!(u16_at(bytes, 406), 0b10, "LED_CAPSL");
    let axis_y = 408 + 20;
    assert_eq!(
        [0, 4, 8, 12, 16].map(|offset| u32_at(bytes, axis_y + offset)),
        [0, 32767, 10, 0, 0]
    );

    let message = events(&[ev(EV_REL, REL_X, -5), syn()]).encode();
    let bytes = message.as_bytes();
    assert_eq!(bytes.len(), 528);
    assert_eq!(u32_at(bytes, 8), 2);
    assert_eq!(
        (u16_at(bytes, 16), u16_at(bytes, 18), u32_at(bytes, 20)),
        (EV_REL, REL_X, (-5i32).cast_unsigned())
    );
    assert!(bytes[32..].iter().all(|&byte| byte == 0));

    let ready = Message::Ready(Ready { node: 7 }).encode();
    assert_eq!(ready.as_bytes().len(), 16);
    assert_eq!(u32_at(ready.as_bytes(), 8), 7);
}

#[test]
fn decoding_is_strict() {
    let stop = Message::Stop.encode();
    assert_eq!(
        Message::decode(&stop.as_bytes()[..7]),
        Err(MessageError::Short)
    );
    let mut unknown = stop.as_bytes().to_vec();
    unknown[0] = 8;
    assert_eq!(Message::decode(&unknown), Err(MessageError::Type(8)));
    let mut lying = stop.as_bytes().to_vec();
    lying[4] = 9;
    assert_eq!(Message::decode(&lying), Err(MessageError::Length));
    let mut longer = stop.as_bytes().to_vec();
    longer.push(0);
    assert_eq!(Message::decode(&longer), Err(MessageError::Length));

    let hello = Message::Hello(hello()).encode().as_bytes().to_vec();
    for reserved in [10, 11, 28, 31] {
        let mut wrong = hello.clone();
        wrong[reserved] = 1;
        assert_eq!(
            Message::decode(&wrong),
            Err(MessageError::Field),
            "byte {reserved}"
        );
    }

    let mut ready = Message::Ready(Ready { node: 1 })
        .encode()
        .as_bytes()
        .to_vec();
    ready[12] = 1;
    assert_eq!(Message::decode(&ready), Err(MessageError::Field));

    let two = events(&[syn(), syn()]).encode().as_bytes().to_vec();
    let mut count = two.clone();
    count[8] = 65;
    assert_eq!(
        Message::decode(&count),
        Err(MessageError::Field),
        "a count over MAX_EVENTS"
    );
    let mut past = two.clone();
    past[16 + 2 * 8] = 1;
    assert_eq!(
        Message::decode(&past),
        Err(MessageError::Field),
        "an event past the count"
    );
    let mut reserved = two;
    reserved[13] = 1;
    assert_eq!(Message::decode(&reserved), Err(MessageError::Field));

    let mut refusal = Message::Refused(Refusal::Version)
        .encode()
        .as_bytes()
        .to_vec();
    for raw in [0, 12] {
        refusal[8] = raw;
        assert_eq!(Message::decode(&refusal), Err(MessageError::Field));
    }
}

// -- HELLO and what the core publishes -------------------------------------------

#[test]
fn hello_is_validated_in_the_order_its_fields_are_read() {
    let rights = Hello::HANDLE_RIGHTS;
    assert_eq!(hello().validate(&rights), Ok(()));
    let with = |change: fn(&mut Hello)| {
        let mut hello = hello();
        change(&mut hello);
        hello.validate(&rights)
    };

    assert_eq!(with(|h| h.version = 2), Err(Refusal::Version));
    assert_eq!(
        with(|h| {
            h.version = 0;
            h.name.len = 200;
        }),
        Err(Refusal::Version),
        "the version is read first"
    );

    assert_eq!(with(|h| h.name.len = 129), Err(Refusal::Text));
    assert_eq!(with(|h| h.serial.bytes[0] = b'x'), Err(Refusal::Text));
    assert_eq!(with(|h| h.name.bytes[4] = 0), Err(Refusal::Text));
    assert_eq!(
        with(|h| h.name = Text::new(&[b'n'; TEXT_BYTES]).expect("fits")),
        Ok(())
    );
    assert_eq!(Text::new(b"a\0b"), None);
    assert_eq!(Text::new(&[b'n'; TEXT_BYTES + 1]), None);
    assert_eq!(
        with(|h| {
            h.name.len = 200;
            h.bits.sw[2] = 0xff;
        }),
        Err(Refusal::Text),
        "the name before the bitmaps"
    );

    // SW_CNT is 18: bit 17 is a switch, bit 18 is past the last.
    assert_eq!(with(|h| h.bits.sw[2] = 0b10), Ok(()));
    assert_eq!(with(|h| h.bits.sw[2] = 0b100), Err(Refusal::Bits));
    assert_eq!(
        with(|h| h.bits.types[0] &= !(1 << EV_REL)),
        Err(Refusal::Bits),
        "codes of a type not declared"
    );
    assert_eq!(
        with(|h| {
            h.bits.types[0] &= !(1 << EV_REL);
            h.bits.rels = [0; REL_BYTES];
        }),
        Ok(())
    );

    assert_eq!(
        with(|h| h.axes[usize::from(ABS_X)].minimum = 32768),
        Err(Refusal::Axis)
    );
    assert_eq!(with(|h| h.axes[usize::from(ABS_X)].minimum = 32767), Ok(()));
    assert_eq!(
        with(|h| h.axes[2].fuzz = 1),
        Err(Refusal::Axis),
        "a range for an axis not declared"
    );
    assert_eq!(
        with(|h| {
            h.axes[2].fuzz = 1;
            h.bits.types[0] &= !(1 << EV_REL);
        }),
        Err(Refusal::Bits),
        "the bitmaps before the axes"
    );

    for handles in [
        &[][..],
        &[Rights::WRITE],
        &[PORT_RIGHTS, PORT_RIGHTS],
        &[Rights(PORT_RIGHTS.0 | Rights::DUPLICATE.0)],
    ] {
        assert_eq!(hello().validate(handles), Err(Refusal::Rights));
    }
    assert_eq!(
        Session::accept(&hello(), &[]).map(|_| ()),
        Err(Refusal::Rights)
    );
}

#[test]
fn the_core_publishes_what_it_supports_and_says_what_it_left_out() {
    let mut declared = hello();
    set(&mut declared.bits.types, EV_SND);
    let caps = Capabilities::from_hello(&declared);
    let bits = &caps.bits;
    for kind in [
        EV_SYN, EV_KEY, EV_REL, EV_ABS, EV_MSC, EV_SW, EV_LED, EV_REP,
    ] {
        assert!(bits.has_type(kind), "type {kind}");
    }
    assert!(!bits.has_type(EV_FF) && !bits.has_type(EV_SND));
    assert!(!bits.has_code(EV_KEY, KEY_RESERVED));
    assert!(bits.has_code(EV_KEY, KEY_A) && bits.has_code(EV_KEY, BTN_LEFT));
    assert!(bits.has_code(EV_ABS, ABS_Y));
    assert!(!bits.has_code(EV_ABS, ABS_MT_SLOT));
    assert_eq!(caps.axes[usize::from(ABS_MT_SLOT)], AxisRange::default());
    assert_eq!(caps.axes[usize::from(ABS_Y)].fuzz, 10);

    let mut types = [0; TYPE_BYTES];
    set(&mut types, EV_FF);
    set(&mut types, EV_SND);
    assert_eq!(
        caps.left_out,
        LeftOut {
            types,
            mt_axes: true
        }
    );

    let mut plain = hello();
    plain.bits.types[2] &= !(1 << (EV_FF - 16));
    plain.bits.abs[5] = 0;
    plain.axes[usize::from(ABS_MT_SLOT)] = AxisRange::default();
    assert!(Capabilities::from_hello(&plain).left_out.is_empty());
}

#[test]
fn queues_are_as_long_as_evdev_makes_them() {
    let mut keyboard = hello();
    keyboard.bits.types = [0; TYPE_BYTES];
    set(&mut keyboard.bits.types, EV_KEY);
    keyboard.bits.rels = [0; REL_BYTES];
    keyboard.bits.abs = [0; ABS_BYTES];
    keyboard.bits.msc = [0; MSC_BYTES];
    keyboard.bits.sw = [0; SW_BYTES];
    keyboard.bits.leds = [0; LED_BYTES];
    keyboard.axes = [AxisRange::default(); AXES];
    // Linux's rule with 256 packets where Linux has 8 (`BUFFER_PACKETS` says
    // why). One SYN_REPORT and seven for keys: 8 × 256 = 2048.
    assert_eq!(Capabilities::from_hello(&keyboard).queue_size(), 2048);
    // Plus ABS_X, ABS_Y, REL_X and REL_WHEEL: 12 × 256 = 3072, rounded up.
    assert_eq!(Capabilities::from_hello(&hello()).queue_size(), 4096);
    // Every axis short of multi-touch, 47 of them: 55 × 256 = 14080.
    let mut axes = hello();
    axes.bits.abs = [0xff; ABS_BYTES];
    assert_eq!(Capabilities::from_hello(&axes).queue_size(), 16384);
}

// -- The session -----------------------------------------------------------------

#[test]
fn reports_are_delivered_whole_across_messages() {
    let mut session = session();
    assert_eq!(session.name().as_bytes(), b"QEMU Virtio Keyboard");
    assert_eq!(session.id().vendor, 0x0627);

    let first = send(&mut session, &[ev(EV_KEY, KEY_A, 1)], 10).expect("accepted");
    assert!(first.is_empty(), "no SYN_REPORT yet");
    let reports = send(
        &mut session,
        &[syn(), ev(EV_KEY, KEY_A, 0), syn(), ev(EV_REL, REL_X, 3)],
        20,
    )
    .expect("accepted");
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].events(), &[ev(EV_KEY, KEY_A, 1), syn()]);
    assert_eq!(reports[1].events(), &[ev(EV_KEY, KEY_A, 0), syn()]);
    assert!(
        reports.iter().all(|report| report.time() == 20),
        "stamped when the SYN_REPORT arrives"
    );
    assert_eq!(reports[0].recipient(), None);
    let last = send(&mut session, &[syn()], 30).expect("accepted");
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].events(), &[ev(EV_REL, REL_X, 3), syn()]);
    assert_eq!(last[0].time(), 30);
}

#[test]
fn events_pass_and_change_the_state_as_input_get_disposition_decides() {
    let mut session = session();
    let reports = send(
        &mut session,
        &[
            ev(EV_KEY, KEY_A, 1),
            syn(),
            ev(EV_KEY, KEY_A, 1),
            syn(),
            ev(EV_KEY, KEY_A, 2),
            syn(),
            ev(EV_REL, REL_X, 0),
            ev(EV_REL, REL_WHEEL, -1),
            ev(EV_MSC, MSC_SCAN, 30),
            syn(),
            ev(EV_ABS, ABS_Y, 100),
            ev(EV_ABS, ABS_MT_SLOT, 1),
            ev(EV_KEY, KEY_RESERVED, 1),
            syn(),
            ev(EV_ABS, ABS_Y, 104),
            syn(),
            ev(EV_ABS, ABS_Y, 108),
            syn(),
            ev(EV_LED, LED_CAPSL, 1),
            ev(EV_SW, 0, 1),
            ev(EV_REP, REP_DELAY, 500),
            ev(EV_REP, REP_PERIOD, -1),
            syn(),
        ],
        5,
    )
    .expect("accepted");
    let seen: Vec<&[RawEvent]> = reports.iter().map(Report::events).collect();
    assert_eq!(
        seen,
        [
            &[ev(EV_KEY, KEY_A, 1), syn()][..],
            // The second press changed nothing, so its report was empty.
            &[ev(EV_KEY, KEY_A, 2), syn()],
            &[ev(EV_REL, REL_WHEEL, -1), ev(EV_MSC, MSC_SCAN, 30), syn()],
            // Multi-touch and KEY_RESERVED are not published.
            &[ev(EV_ABS, ABS_Y, 100), syn()],
            // 104 is within half the fuzz of 100; 108 is within the fuzz, so
            // a quarter of the way: (3 × 100 + 108) / 4.
            &[ev(EV_ABS, ABS_Y, 102), syn()],
            &[
                ev(EV_LED, LED_CAPSL, 1),
                ev(EV_SW, 0, 1),
                ev(EV_REP, REP_DELAY, 500),
                syn()
            ],
        ]
    );

    let state = session.state();
    assert_eq!(state.keys[3], 1 << 6, "KEY_A held");
    assert_eq!(state.keys[0], 0);
    assert_eq!(state.leds[0], 1 << LED_CAPSL);
    assert_eq!(state.sw[0], 1);
    assert_eq!(state.repeat, [500, DEFAULT_REPEAT[1]]);
    let y = session.abs_info(ABS_Y).expect("an axis");
    assert_eq!((y.value, y.maximum, y.fuzz), (102, 32767, 10));
    assert_eq!(
        session.abs_info(ABS_MT_SLOT).map(|info| info.maximum),
        Some(0)
    );
    assert_eq!(session.abs_info(64), None);

    session.set_repeat(250, 20);
    assert_eq!(session.state().repeat, [250, 20]);

    let released = send(
        &mut session,
        &[ev(EV_KEY, KEY_A, 0), ev(EV_LED, LED_CAPSL, 0), syn()],
        6,
    )
    .expect("accepted");
    assert_eq!(released.len(), 1);
    assert_eq!(session.state().keys[3], 0);
    assert_eq!(session.state().leds[0], 0);
}

#[test]
fn defuzzing_is_input_c_s() {
    assert_eq!(defuzz(104, 100, 0), 104);
    assert_eq!(defuzz(104, 100, 10), 100);
    assert_eq!(defuzz(96, 100, 10), 100);
    assert_eq!(defuzz(105, 100, 10), 101, "(300 + 105) / 4");
    assert_eq!(defuzz(108, 100, 10), 102);
    assert_eq!(defuzz(115, 100, 10), 107, "(100 + 115) / 2");
    assert_eq!(defuzz(120, 100, 10), 120);
    assert_eq!(defuzz(-115, -100, 10), -107, "truncating toward zero");
    assert_eq!(defuzz(i32::MAX - 1, i32::MAX, i32::MAX), i32::MAX);
    assert_eq!(defuzz(i32::MIN, i32::MAX, i32::MAX), i32::MIN);
}

#[test]
fn an_event_the_driver_did_not_declare_is_refused_before_anything_happens() {
    for bad in [
        ev(EV_KEY, KEY_B, 1),
        ev(EV_KEY, KEY_MAX + 1, 1),
        ev(EV_SYN, SYN_MT_REPORT, 0),
        ev(EV_SYN, SYN_DROPPED, 0),
        ev(EV_FF, 0, 1),
        ev(EV_SND, 0, 1),
        ev(EV_REP, 2, 1),
        ev(EV_MAX + 1, 0, 0),
    ] {
        let mut session = session();
        let mut delivered = 0;
        let result = session.receive(&events(&[ev(EV_KEY, KEY_A, 1), syn(), bad]), 1, |_| {
            delivered += 1;
        });
        assert_eq!(result, Err(Refusal::Undeclared), "{bad:?}");
        assert_eq!(delivered, 0, "nothing of a refused message is delivered");
        assert_eq!(session.state().keys[3], 0, "nor changes the state");
        assert!(session.is_broken());
        assert_eq!(
            session.receive(&events(&[syn()]), 1, |_| {}),
            Err(Refusal::Protocol),
            "a broken session stays broken"
        );
        assert_eq!(session.stop(), Err(RequestError::Closed));
    }
    // What the core does not publish, the driver may still send.
    let mut session = session();
    assert!(send(&mut session, &[ev(EV_KEY, KEY_RESERVED, 1), syn()], 1).is_ok());
}

#[test]
fn a_report_holds_at_most_max_report_events() {
    let scan = ev(EV_MSC, MSC_SCAN, 1);
    let mut session = session();
    for _ in 0..3 {
        assert_eq!(send(&mut session, &[scan; MAX_EVENTS], 1), Ok(Vec::new()));
    }
    let mut last = std::vec![scan; MAX_EVENTS - 1];
    last.push(syn());
    let reports = send(&mut session, &last, 2).expect("255 events and a SYN_REPORT");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].events().len(), MAX_REPORT);

    let mut session = self::session();
    for _ in 0..3 {
        assert!(send(&mut session, &[scan; MAX_EVENTS], 1).is_ok());
    }
    assert_eq!(
        send(&mut session, &[scan; MAX_EVENTS], 1),
        Err(Refusal::ReportTooLong)
    );
    assert!(session.is_broken());
    assert_eq!(Report::new(0, None, &[scan; MAX_REPORT + 1]), None);
}

#[test]
fn a_driver_that_sends_what_nobody_waits_for_breaks_the_session() {
    for message in [
        Message::Hello(hello()),
        Message::Ready(Ready { node: 0 }),
        Message::Refused(Refusal::Protocol),
        Message::Stop,
        Message::Stopped,
    ] {
        let mut session = session();
        assert_eq!(
            session.receive(&message, 0, |_| {}),
            Err(Refusal::Protocol),
            "{message:?}"
        );
        assert!(session.is_broken());
    }
}

#[test]
fn stop_waits_for_stopped_and_accepts_nothing_after_it() {
    let mut session = session();
    assert_eq!(session.stop(), Ok(Message::Stop));
    assert_eq!(session.stop(), Err(RequestError::Closed));
    assert_eq!(
        send(&mut session, &[ev(EV_KEY, KEY_A, 1)], 1),
        Ok(Vec::new()),
        "events the driver sent before it read STOP"
    );
    assert_eq!(
        session.receive(&Message::Stopped, 2, |_| {}),
        Ok(Received::Stopped)
    );
    assert!(session.is_stopped());
    assert_eq!(
        session.receive(&events(&[syn()]), 3, |_| {}),
        Err(Refusal::Protocol)
    );
    assert!(session.is_broken());
}

#[test]
fn grabs_follow_evdev_grab_and_ungrab() {
    let (one, two) = (OpenId(1), OpenId(2));
    let mut session = session();
    assert_eq!(session.ungrab(one), Err(GrabError::NotHolder));
    assert_eq!(session.grab(one), Ok(()));
    assert_eq!(session.grab(one), Err(GrabError::Busy), "even the holder");
    assert_eq!(session.grab(two), Err(GrabError::Busy));
    assert_eq!(session.ungrab(two), Err(GrabError::NotHolder));
    assert_eq!(session.grabbed(), Some(one));

    let reports = send(&mut session, &[ev(EV_KEY, KEY_A, 1), syn()], 1).expect("accepted");
    assert_eq!(reports[0].recipient(), Some(one));
    assert!(reports[0].is_for(one) && !reports[0].is_for(two));

    session.release(two);
    assert_eq!(
        session.grabbed(),
        Some(one),
        "only the holder's close releases"
    );
    session.release(one);
    assert_eq!(session.grabbed(), None);
    let reports = send(&mut session, &[ev(EV_KEY, KEY_A, 0), syn()], 2).expect("accepted");
    assert!(reports[0].is_for(one) && reports[0].is_for(two));

    assert_eq!(session.grab(two), Ok(()));
    assert_eq!(session.ungrab(two), Ok(()));
    assert_eq!(session.grabbed(), None);
}

// -- The queue -------------------------------------------------------------------

const W64: ReadFlags = ReadFlags {
    width: Width::Bits64,
    nonblocking: true,
    gone: false,
};

fn queue(len: usize) -> Queue<Vec<Stamped>> {
    Queue::new(std::vec![Stamped::default(); len]).expect("a power of two")
}

/// A report stamped `time` microseconds.
fn report(time: u64, list: &[RawEvent]) -> Report {
    Report::new(time * 1000, None, list).expect("short")
}

/// Everything readable, as (microseconds in the open's clock, event) at 64
/// bits.
fn drain(queue: &mut Queue<Vec<Stamped>>) -> Vec<(u64, RawEvent)> {
    let mut out = [0u8; 24 * 64];
    let Ok(len) = queue.read(&mut out, W64, &Clocks::default()) else {
        return Vec::new();
    };
    out[..len]
        .as_chunks::<24>()
        .0
        .iter()
        .map(|bytes| {
            let event = Event::read(Width::Bits64, bytes).expect("24 bytes");
            (
                event.sec * 1_000_000 + event.usec,
                ev(event.r#type, event.code, event.value),
            )
        })
        .collect()
}

#[test]
fn a_queue_is_a_power_of_two_of_at_least_four() {
    for len in [0, 1, 2, 3, 6, 12] {
        assert_eq!(
            Queue::new(std::vec![Stamped::default(); len]).map(|_| ()),
            Err(SizeError),
            "{len}"
        );
    }
    for len in [4, 8, 64] {
        assert!(Queue::new(std::vec![Stamped::default(); len]).is_ok());
    }
    let mut array = [Stamped::default(); 8];
    assert!(Queue::new(&mut array[..]).is_ok());
}

#[test]
fn reports_are_read_in_order_and_a_read_may_split_one() {
    let mut queue = queue(16);
    queue.deliver(&report(1_000_000, &[ev(EV_KEY, KEY_A, 1), syn()]));
    queue.deliver(&report(
        2_000_000,
        &[ev(EV_REL, REL_X, 4), ev(EV_REL, REL_WHEEL, 1), syn()],
    ));
    assert_eq!(queue.len(), 5);

    let mut small = [0u8; 50];
    assert_eq!(queue.read(&mut small, W64, &Clocks::default()), Ok(48));
    assert_eq!(
        Event::read(Width::Bits64, &small[24..]).map(|e| (e.r#type, e.code)),
        Some((EV_SYN, SYN_REPORT))
    );
    assert_eq!(
        queue.read(&mut small[..24], W64, &Clocks::default()),
        Ok(24)
    );
    assert_eq!(
        drain(&mut queue),
        [(2_000_000, ev(EV_REL, REL_WHEEL, 1)), (2_000_000, syn())]
    );
    assert!(queue.is_empty());
}

#[test]
fn a_full_queue_keeps_syn_dropped_and_the_newest_event_as_pass_event_does() {
    // Eight slots hold seven events. The fourth report's first event fills
    // the ring: everything unread goes, SYN_DROPPED and that event stay, and
    // nothing is readable until the report's SYN_REPORT.
    let mut queue = queue(8);
    queue.deliver(&report(1, &[ev(EV_KEY, KEY_A, 1), syn()]));
    queue.deliver(&report(2, &[ev(EV_KEY, KEY_A, 0), syn()]));
    queue.deliver(&report(
        3,
        &[ev(EV_REL, REL_X, 5), ev(EV_REL, REL_X, 6), syn()],
    ));
    assert_eq!(queue.len(), 7);
    queue.deliver(&report(4, &[ev(EV_KEY, KEY_B, 1), syn()]));
    assert_eq!(
        drain(&mut queue),
        [
            (4, ev(EV_SYN, SYN_DROPPED, 0)),
            (4, ev(EV_KEY, KEY_B, 1)),
            (4, syn())
        ]
    );

    // A report longer than the ring overflows as it goes; the reader sees
    // SYN_DROPPED, the newest event and the SYN_REPORT.
    let mut queue = self::queue(4);
    let scans: Vec<RawEvent> = (1..=4).map(|value| ev(EV_MSC, MSC_SCAN, value)).collect();
    let mut long = scans;
    long.push(syn());
    queue.deliver(&report(9, &long));
    assert_eq!(
        drain(&mut queue),
        [
            (9, ev(EV_SYN, SYN_DROPPED, 0)),
            (9, ev(EV_MSC, MSC_SCAN, 4)),
            (9, syn())
        ]
    );
}

#[test]
fn a_syn_report_after_nothing_is_not_queued() {
    let mut queue = queue(8);
    queue.deliver(&report(1, &[syn()]));
    assert!(queue.is_empty());
    queue.deliver(&report(1, &[ev(EV_KEY, KEY_A, 1), syn(), syn()]));
    assert_eq!(queue.len(), 2);
}

#[test]
fn reading_the_state_flushes_its_type_as_evdev_flush_queue_does() {
    let mut queue = queue(16);
    queue.deliver(&report(
        1,
        &[ev(EV_KEY, KEY_A, 1), ev(EV_REL, REL_X, 1), syn()],
    ));
    queue.deliver(&report(2, &[ev(EV_KEY, KEY_B, 1), syn()]));
    queue.deliver(&report(3, &[ev(EV_REL, REL_X, 2), syn()]));
    queue.flush_type(EV_KEY);
    assert_eq!(
        drain(&mut queue),
        [
            (1, ev(EV_REL, REL_X, 1)),
            (1, syn()),
            // The second report had only a key: its SYN_REPORT went with it.
            (3, ev(EV_REL, REL_X, 2)),
            (3, syn())
        ]
    );

    // A SYN_REPORT left at the front by a split read is kept.
    let mut queue = self::queue(16);
    queue.deliver(&report(1, &[ev(EV_KEY, KEY_A, 1), syn()]));
    queue.deliver(&report(2, &[ev(EV_KEY, KEY_A, 0), syn()]));
    let mut one = [0u8; 24];
    assert_eq!(queue.read(&mut one, W64, &Clocks::default()), Ok(24));
    queue.flush_type(EV_KEY);
    assert_eq!(drain(&mut queue), [(1, syn())]);

    let mut queue = self::queue(8);
    queue.deliver(&report(1, &[ev(EV_KEY, KEY_A, 1), syn()]));
    queue.flush_type(EV_SYN);
    assert_eq!(queue.len(), 2, "EV_SYN is not flushed");

    let mut queue = self::queue(8);
    queue.deliver(&report(1, &[ev(EV_KEY, KEY_A, 1), syn()]));
    queue.queue_syn_dropped(5);
    assert!(queue.has_packet());
    assert_eq!(queue.len(), 3);
}

#[test]
fn changing_the_clock_drops_a_non_empty_queue() {
    let mut queue = queue(8);
    assert_eq!(queue.clock(), Clock::Realtime);
    queue.set_clock(Clock::Monotonic, 50_000);
    assert!(queue.is_empty(), "an empty queue gets no SYN_DROPPED");

    queue.deliver(&report(60, &[ev(EV_KEY, KEY_A, 1), syn()]));
    queue.set_clock(Clock::Monotonic, 70_000);
    assert_eq!(queue.len(), 2, "the same clock changes nothing");

    queue.set_clock(Clock::Boottime, 80_000);
    assert_eq!(queue.len(), 1);
    assert!(!queue.has_packet(), "SYN_DROPPED waits for a SYN_REPORT");
    queue.deliver(&report(90, &[ev(EV_KEY, KEY_A, 0), syn()]));
    assert_eq!(
        drain(&mut queue),
        [
            (80, ev(EV_SYN, SYN_DROPPED, 0)),
            (90, ev(EV_KEY, KEY_A, 0)),
            (90, syn())
        ]
    );

    for (id, clock) in [
        (0, Some(Clock::Realtime)),
        (1, Some(Clock::Monotonic)),
        (7, Some(Clock::Boottime)),
        (4, None),
        (-1, None),
    ] {
        assert_eq!(Clock::from_id(id), clock, "{id}");
    }
}

#[test]
fn a_read_answers_in_evdev_read_s_order() {
    let mut queue = queue(8);
    let clocks = Clocks::default();
    let blocking = ReadFlags {
        nonblocking: false,
        ..W64
    };
    let mut buffer = [0u8; 48];
    assert_eq!(
        queue.read(&mut buffer, W64, &clocks),
        Err(ReadError::WouldBlock)
    );
    assert_eq!(
        queue.read(&mut buffer, blocking, &clocks),
        Err(ReadError::Empty)
    );
    assert_eq!(queue.read(&mut [], blocking, &clocks), Ok(0));
    assert_eq!(
        queue.read(&mut [], W64, &clocks),
        Err(ReadError::WouldBlock),
        "O_NONBLOCK is checked before a zero count"
    );

    queue.deliver(&report(1, &[ev(EV_KEY, KEY_A, 1), syn()]));
    assert_eq!(
        queue.poll(false),
        Poll {
            readable: true,
            hangup: false
        }
    );
    assert_eq!(
        queue.read(&mut buffer[..23], W64, &clocks),
        Err(ReadError::TooSmall)
    );
    assert_eq!(
        queue.read(&mut buffer[..20], ReadFlags { gone: true, ..W64 }, &clocks),
        Err(ReadError::TooSmall),
        "the size is checked first"
    );
    assert_eq!(
        queue.read(&mut buffer, ReadFlags { gone: true, ..W64 }, &clocks),
        Err(ReadError::Gone),
        "a gone device reads ENODEV with events queued"
    );
    assert_eq!(
        queue.poll(true),
        Poll {
            readable: true,
            hangup: true
        }
    );
    assert_eq!(queue.read(&mut [], W64, &clocks), Ok(0));
    assert_eq!(queue.read(&mut buffer, W64, &clocks), Ok(48));

    queue.deliver(&report(2, &[ev(EV_KEY, KEY_A, 0), syn()]));
    queue.revoke();
    assert!(queue.is_revoked());
    assert_eq!(queue.read(&mut buffer, W64, &clocks), Err(ReadError::Gone));
    assert!(queue.poll(false).hangup);
    let before = queue.len();
    queue.deliver(&report(3, &[ev(EV_KEY, KEY_A, 1), syn()]));
    assert_eq!(queue.len(), before, "a revoked open receives nothing");
}

#[test]
fn events_are_read_at_both_widths_in_the_open_s_clock() {
    let stamped = Stamped {
        time: 5_123_456_789,
        event: ev(EV_ABS, ABS_X, -7),
    };
    let clocks = Clocks {
        realtime_offset: 1_700_000_000_000_000_000,
        boottime_offset: 2_000_000_000,
    };
    let mut out = [0u8; 24];
    stamped
        .write(Clock::Monotonic, &clocks, Width::Bits64, &mut out)
        .expect("fits");
    assert_eq!(
        Event::read(Width::Bits64, &out),
        Some(Event {
            sec: 5,
            usec: 123_456,
            r#type: EV_ABS,
            code: ABS_X,
            value: -7
        })
    );
    stamped
        .write(Clock::Realtime, &clocks, Width::Bits64, &mut out)
        .expect("fits");
    assert_eq!(
        Event::read(Width::Bits64, &out).map(|e| (e.sec, e.usec)),
        Some((1_700_000_005, 123_456))
    );

    let mut short = [0u8; 16];
    stamped
        .write(Clock::Boottime, &clocks, Width::Bits32, &mut short)
        .expect("fits");
    assert_eq!(
        [u32_at(&short, 0), u32_at(&short, 4)],
        [7, 123_456],
        "16 bytes on ARMv7-A"
    );
    assert_eq!(
        (u16_at(&short, 8), u16_at(&short, 10), u32_at(&short, 12)),
        (EV_ABS, ABS_X, (-7i32).cast_unsigned())
    );
    assert_eq!(
        stamped.write(Clock::Monotonic, &clocks, Width::Bits32, &mut short[..15]),
        None
    );

    let late = Clocks {
        realtime_offset: (1 << 32) * 1_000_000_000,
        boottime_offset: -10_000_000_000,
    };
    stamped
        .write(Clock::Realtime, &late, Width::Bits32, &mut short)
        .expect("cut, not refused");
    assert_eq!(u32_at(&short, 0), 5, "seconds cut to an unsigned long");
    assert_eq!(late.convert(Clock::Boottime, 5), 0, "clamped at zero");
    assert_eq!(late.convert(Clock::Realtime, u64::MAX), u64::MAX);

    let mut queue = queue(8);
    queue.deliver(&report(1_500, &[ev(EV_KEY, KEY_A, 1), syn()]));
    let mut buffer = [0u8; 32];
    let flags = ReadFlags {
        width: Width::Bits32,
        ..W64
    };
    assert_eq!(
        queue.read(&mut buffer[..15], flags, &Clocks::default()),
        Err(ReadError::TooSmall)
    );
    assert_eq!(queue.read(&mut buffer, flags, &Clocks::default()), Ok(32));
    assert_eq!(
        [u32_at(&buffer, 0), u32_at(&buffer, 4), u32_at(&buffer, 28)],
        [0, 1_500, 0]
    );
}

#[test]
fn bitmaps_and_strings_are_copied_as_evdev_copies_them() {
    let mut keys = [0u8; KEY_BYTES];
    keys[95] = 0x80;
    let mut out = [0xaau8; 128];
    for width in [Width::Bits64, Width::Bits32] {
        assert_eq!(copy_bits(&keys, KEY_MAX, width, 128, &mut out), Some(96));
        assert_eq!(out[95], 0x80);
    }
    assert_eq!(
        copy_bits(&keys, KEY_MAX, Width::Bits64, 3, &mut out),
        Some(3)
    );
    assert_eq!(
        copy_bits(&[1], EV_MAX, Width::Bits64, 64, &mut out),
        Some(8)
    );
    assert_eq!(out[..8], [1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        copy_bits(&[1], EV_MAX, Width::Bits32, 64, &mut out),
        Some(4)
    );
    assert_eq!(
        copy_bits(&[0, 0, 2], SW_MAX, Width::Bits64, 64, &mut out),
        Some(8)
    );
    assert_eq!(
        copy_bits(&keys, KEY_MAX, Width::Bits64, 128, &mut out[..95]),
        None
    );

    let name = Text::new(b"QEMU").expect("fits");
    assert_eq!(copy_text(&name, 64, &mut out), Ok(Some(5)));
    assert_eq!(&out[..5], b"QEMU\0");
    assert_eq!(copy_text(&name, 2, &mut out), Ok(Some(2)));
    assert_eq!(copy_text(&name, 0, &mut out), Ok(Some(0)));
    assert_eq!(copy_text(&name, 64, &mut out[..4]), Ok(None));
    assert_eq!(copy_text(&Text::NONE, 64, &mut out), Err(NoEntry));
}

/// `Hello::decode_into` exists so the kernel's input core can decode a hello
/// without three of them on one stack frame; it must judge a message exactly
/// as `Message::decode` does, or the core would accept what the protocol
/// refuses.
#[test]
fn decoding_into_a_place_judges_what_decode_judges() {
    let whole = Message::Hello(hello()).encode().as_bytes().to_vec();
    let mut place = Hello::EMPTY;
    assert_eq!(Hello::decode_into(&whole, &mut place), Ok(()));
    assert_eq!(Message::decode(&whole), Ok(Message::Hello(place)));

    // A message of another type, a short one, one whose length field lies,
    // and one with a byte set in the padding the format reserves.
    let ready = Message::Ready(Ready { node: 0 })
        .encode()
        .as_bytes()
        .to_vec();
    assert_eq!(
        Hello::decode_into(&ready, &mut place),
        Err(MessageError::Type(READY))
    );
    assert_eq!(
        Hello::decode_into(&whole[..4], &mut place),
        Err(MessageError::Short)
    );
    let mut lying = whole.clone();
    lying[4] = 0;
    assert_eq!(
        Hello::decode_into(&lying, &mut place),
        Err(MessageError::Length)
    );
    for at in [10, 28] {
        let mut dirty = whole.clone();
        dirty[at] = 1;
        assert_eq!(
            Hello::decode_into(&dirty, &mut place),
            Err(MessageError::Field)
        );
        assert_eq!(Message::decode(&dirty), Err(MessageError::Field));
    }
}

/// The place a hello is decoded into keeps nothing of what it held: a core
/// that reused one would otherwise take the last device's bitmaps for this
/// one's.
#[test]
fn decoding_into_a_place_overwrites_all_of_it() {
    let whole = Message::Hello(hello()).encode().as_bytes().to_vec();
    let mut place = hello();
    place.name = Text::new(b"something else").expect("a name");
    set(&mut place.bits.keys, KEY_MAX);
    place.axes[usize::from(ABS_Y)].maximum = 9999;

    assert_eq!(Hello::decode_into(&whole, &mut place), Ok(()));
    assert_eq!(place, hello());
}

/// A program's LED write changes the state once, is handed back for the
/// driver while it changes something, and takes nothing but a declared LED.
#[test]
fn a_written_led_changes_the_state_once_and_only_a_declared_one() {
    let mut session = session();
    let caps = ev(EV_LED, LED_CAPSL, 1);
    assert_eq!(session.write_led(caps), Some(caps));
    assert!(bit(&session.state().leds, LED_CAPSL));
    assert_eq!(session.write_led(caps), None, "already lit");
    assert_eq!(
        session.write_led(ev(EV_LED, 0, 1)),
        None,
        "NUML is not declared"
    );
    assert_eq!(session.write_led(ev(EV_KEY, KEY_A, 1)), None, "not an LED");
    assert!(!bit(&session.state().keys, KEY_A));
    let off = ev(EV_LED, LED_CAPSL, 0);
    assert_eq!(session.write_led(off), Some(off));
    assert!(!bit(&session.state().leds, LED_CAPSL));
}
