//! Absolute pointers read from their report descriptors, and the switch
//! that watches a function's raw reports.

use std::vec::Vec;

use ferrix_inputctl::message::{AxisRange, Hello, RawEvent};
use ferrix_linux_abi::input::{
    ABS_WHEEL, ABS_X, ABS_Y, BTN_LEFT, BTN_MISC, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, EV_SYN,
    REL_HWHEEL, REL_WHEEL, SYN_REPORT,
};

use super::model::{JETKVM_ABSOLUTE, QEMU_TABLET};
use crate::hid::{self, EventBuf, Identity, Interpreter, Kind};
use crate::report::Descriptor;
use crate::watch::Watch;

fn reader(bytes: &[u8]) -> Interpreter {
    Interpreter::new(Descriptor::parse(bytes).expect("the descriptor parses"))
}

fn run(reader: &mut Interpreter, report: &[u8]) -> Vec<RawEvent> {
    let mut out = EventBuf::default();
    reader.report(report, &mut out);
    out.as_slice().to_vec()
}

fn abs(code: u16, value: i32) -> RawEvent {
    RawEvent::new(EV_ABS, code, value)
}

fn key(code: u16, down: bool) -> RawEvent {
    RawEvent::new(EV_KEY, code, i32::from(down))
}

fn syn() -> RawEvent {
    RawEvent::new(EV_SYN, SYN_REPORT, 0)
}

fn hello_of(reader: &Interpreter) -> Hello {
    let identity = Identity {
        vendor: 0x0627,
        product: 0x0001,
        release: 0,
        name: ferrix_inputctl::message::Text::new(b"QEMU USB Tablet").expect("a name"),
    };
    hid::hello(reader, &identity, 0)
}

const FULL: AxisRange = AxisRange {
    minimum: 0,
    maximum: 32767,
    fuzz: 0,
    flat: 0,
    resolution: 0,
};

/// QEMU's tablet declares X and Y as absolute axes over its logical range,
/// and the core takes the HELLO that says so.
#[test]
fn qemus_tablet_declares_absolute_x_and_y_with_its_logical_range() {
    let tablet = reader(&QEMU_TABLET);
    let bits = tablet.bits();
    assert!(bits.has_type(EV_ABS) && bits.has_type(EV_KEY) && bits.has_type(EV_REL));
    assert!(bits.has_code(EV_ABS, ABS_X) && bits.has_code(EV_ABS, ABS_Y));
    assert!(!bits.has_code(EV_ABS, ABS_WHEEL), "its wheel is relative");
    assert!(bits.has_code(EV_REL, REL_WHEEL));
    assert!(bits.has_code(EV_KEY, BTN_LEFT) && bits.has_code(EV_KEY, BTN_RIGHT));
    let axes = tablet.axes();
    assert_eq!(axes[usize::from(ABS_X)], FULL);
    assert_eq!(axes[usize::from(ABS_Y)], FULL);
    assert!(
        axes.iter()
            .skip(2)
            .all(|range| *range == AxisRange::default())
    );
    assert_eq!(tablet.kind(), Kind::Mouse);
    let hello = hello_of(&tablet);
    assert_eq!(hello.axes, axes);
    assert_eq!(hello.validate(&Hello::HANDLE_RIGHTS), Ok(()));
}

/// A position is sent once, then again only for the axis that moved; a
/// click alone carries no axis.
#[test]
fn a_tablet_says_where_it_is_and_only_what_moved() {
    let mut tablet = reader(&QEMU_TABLET);
    assert_eq!(
        run(&mut tablet, &[0x00, 0x00, 0x40, 0x00, 0x20, 0x00]),
        [abs(ABS_X, 0x4000), abs(ABS_Y, 0x2000), syn()]
    );
    assert_eq!(
        run(&mut tablet, &[0x01, 0x00, 0x40, 0x00, 0x20, 0x00]),
        [key(BTN_LEFT, true), syn()]
    );
    assert_eq!(
        run(&mut tablet, &[0x01, 0xFF, 0x7F, 0x00, 0x20, 0x01]),
        [
            abs(ABS_X, 32767),
            RawEvent::new(EV_REL, REL_WHEEL, 1),
            syn()
        ]
    );
    assert!(run(&mut tablet, &[0x01, 0xFF, 0x7F, 0x00, 0x20, 0x00]).is_empty());
}

/// The `JetKVM` numbers its reports: the position is report 1, the wheel and
/// pan report 2, and neither disturbs the other.
#[test]
fn the_jetkvms_numbered_reports_give_position_buttons_and_wheel() {
    let mut kvm = reader(&JETKVM_ABSOLUTE);
    let bits = kvm.bits();
    assert!(bits.has_code(EV_ABS, ABS_X) && bits.has_code(EV_ABS, ABS_Y));
    assert!(bits.has_code(EV_REL, REL_WHEEL) && bits.has_code(EV_REL, REL_HWHEEL));
    assert_eq!(kvm.axes()[usize::from(ABS_Y)], FULL);
    assert_eq!(
        run(&mut kvm, &[1, 0x02, 0x10, 0x27, 0x88, 0x13]),
        [
            key(BTN_RIGHT, true),
            abs(ABS_X, 10000),
            abs(ABS_Y, 5000),
            syn()
        ]
    );
    assert_eq!(
        run(&mut kvm, &[2, 0xFF, 0x01]),
        [
            RawEvent::new(EV_REL, REL_WHEEL, -1),
            RawEvent::new(EV_REL, REL_HWHEEL, 1),
            syn()
        ]
    );
    assert_eq!(
        run(&mut kvm, &[1, 0x00, 0x10, 0x27, 0x88, 0x13]),
        [key(BTN_RIGHT, false), syn()]
    );
}

/// An absolute wheel is `ABS_WHEEL`, over its own range.
#[test]
fn an_absolute_wheel_is_abs_wheel() {
    let descriptor = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x01, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x81, 0x01, //
        0x05, 0x01, 0x09, 0x38, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, //
        0xC0,
    ];
    let mut wheel = reader(&descriptor);
    assert!(wheel.bits().has_code(EV_ABS, ABS_WHEEL));
    assert_eq!(wheel.axes()[usize::from(ABS_WHEEL)].maximum, 100);
    assert_eq!(run(&mut wheel, &[0, 42]), [abs(ABS_WHEEL, 42), syn()]);
}

/// The negative controls. A joystick's absolute X and Y are not a pointer:
/// they stay unmapped, as before, and only its buttons are declared. An
/// empty range declares no axis, so the HELLO is still one the core takes.
/// A value outside the logical range is no position.
#[test]
fn a_joystick_an_empty_range_and_an_out_of_range_value_are_no_position() {
    // Usage (Joystick): one button, then X and Y from 0 to 255.
    let joystick = [
        0x05, 0x01, 0x09, 0x04, 0xA1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x01, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x81, 0x01, //
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x02,
        0x81, 0x02, //
        0xC0,
    ];
    let mut stick = reader(&joystick);
    assert!(!stick.bits().has_type(EV_ABS));
    assert!(stick.bits().has_code(EV_KEY, BTN_MISC));
    assert_eq!(
        stick.axes(),
        [AxisRange::default(); ferrix_inputctl::message::AXES]
    );
    assert_eq!(
        run(&mut stick, &[1, 0x80, 0x80]),
        [key(BTN_MISC, true), syn()]
    );

    // A mouse whose X runs from 100 down to 50: no axis, Y alone kept.
    let backwards = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x01, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x81, 0x01, //
        0x05, 0x01, 0x09, 0x30, 0x16, 0x64, 0x00, 0x26, 0x32, 0x00, 0x75, 0x10, 0x95, 0x01, 0x81,
        0x02, //
        0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xFF, 0x7F, 0x81, 0x02, //
        0xC0,
    ];
    let odd = reader(&backwards);
    assert!(!odd.bits().has_code(EV_ABS, ABS_X));
    assert!(odd.bits().has_code(EV_ABS, ABS_Y));
    assert_eq!(hello_of(&odd).validate(&Hello::HANDLE_RIGHTS), Ok(()));

    // X past 32767 on QEMU's tablet: Y is said, X is not.
    let mut tablet = reader(&QEMU_TABLET);
    assert_eq!(
        run(&mut tablet, &[0x00, 0x00, 0x80, 0x10, 0x00, 0x00]),
        [abs(ABS_Y, 0x10), syn()]
    );
}

#[test]
fn the_switch_reads_the_first_word_and_every_form_of_its_value() {
    let watch = |line: &str| Watch::from_command_line(line.as_bytes(), b"ferrix.usbhid.reports");
    assert_eq!(watch("ferrix.fbcon quiet"), Watch::OFF);
    assert_eq!(watch("ferrix.fbcon ferrix.usbhid.reports"), Watch::ALL);
    assert_eq!(watch("ferrix.usbhid.reports=all"), Watch::ALL);
    assert_eq!(
        watch("ferrix.usbhid.reports=off ferrix.usbhid.reports"),
        Watch::OFF
    );
    assert_eq!(watch("ferrix.usbhid.reportsx=1"), Watch::OFF, "another key");
    assert_eq!(watch("ferrix.usbhid.reports=nonsense"), Watch::OFF);

    let kvm = watch("ferrix.usbhid.reports=1d6b:0104\n");
    assert!(kvm.is_on());
    assert!(kvm.covers(0x1D6B, 0x0104, Kind::Keyboard));
    assert!(kvm.covers(0x1D6B, 0x0104, Kind::Mouse));
    assert!(!kvm.covers(0x0627, 0x0001, Kind::Mouse));
    assert_eq!(std::format!("{kvm}"), "1d6b:0104");

    let keys = Watch::parse(b"1D6B:0104/keyboard").expect("a value");
    assert!(keys.covers(0x1D6B, 0x0104, Kind::Keyboard));
    assert!(!keys.covers(0x1D6B, 0x0104, Kind::Mouse));
    assert_eq!(std::format!("{keys}"), "1d6b:0104/keyboard");

    let mice = Watch::parse(b" mouse\n").expect("a value");
    assert!(mice.covers(0x046D, 0xC08B, Kind::Mouse));
    assert!(!mice.covers(0x046D, 0xC08B, Kind::Keyboard));

    assert_eq!(Watch::parse(b""), Some(Watch::ALL), "an empty file");
    for refused in [
        &b"1d6b"[..],
        b"1d6b:",
        b"12345:1",
        b"xyzw:0104",
        b"1d6b:0104/pen",
    ] {
        assert_eq!(Watch::parse(refused), None, "{refused:?}");
    }
    assert!(!Watch::OFF.covers(0x1D6B, 0x0104, Kind::Keyboard));
}

/// The watch file is looked for at the system's `/run` as a driver started
/// from the initramfs sees it (`/sysroot/run`) first, then at its own
/// `/run`; the first found wins, and none found is `None`, which leaves the
/// boot word's choice. Before 2026-10-25 only `/run` was read, which for
/// usbhid on the N150 was the initramfs's tmpfs, so the file an
/// administrator wrote was never seen.
#[test]
fn the_watch_file_is_found_under_sysroot_or_run() {
    use crate::watch::{FILES, from_files};
    let file = |found: &'static [u8], value: &'static [u8]| {
        move |path: &[u8], buf: &mut [u8]| {
            (path == found).then(|| {
                buf[..value.len()].copy_from_slice(value);
                value.len()
            })
        }
    };
    let keys = Watch::parse(b"1d6b:0104/keyboard");
    assert_eq!(FILES[0], b"/sysroot/run/usbhid-reports\0");
    assert_eq!(FILES[1], b"/run/usbhid-reports\0");
    assert_eq!(
        from_files(&mut file(
            b"/sysroot/run/usbhid-reports\0",
            b"1d6b:0104/keyboard\n"
        )),
        keys
    );
    assert_eq!(
        from_files(&mut file(b"/run/usbhid-reports\0", b"1d6b:0104/keyboard")),
        keys
    );
    assert_eq!(from_files(&mut |_: &[u8], _: &mut [u8]| None), None);
    // Both there: the system's wins.
    let mut both = |path: &[u8], buf: &mut [u8]| {
        let value: &[u8] = if path == FILES[0] {
            b"mouse"
        } else {
            b"keyboard"
        };
        buf[..value.len()].copy_from_slice(value);
        Some(value.len())
    };
    assert_eq!(from_files(&mut both), Watch::parse(b"mouse"));
    // A value that does not parse watches nothing.
    assert_eq!(from_files(&mut file(FILES[1], b"zz:zz")), Some(Watch::OFF));
}
