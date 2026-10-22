//! The descriptors, the key table and the report translation on their own.

use ferrix_inputctl::message::{RawEvent, bit};
use ferrix_linux_abi::input::{
    BTN_EXTRA, BTN_LEFT, EV_KEY, EV_LED, EV_REL, EV_SYN, REL_WHEEL, SYN_REPORT,
};

use crate::hid::{
    self, BOOT_KEYBOARD, BOOT_MOUSE, EventBuf, Interpreter, KEYBOARD_CODES, Kind, map,
};
use crate::hub::{PortStatus, get_port_status};
use crate::report::{self, Descriptor, Direction};
use crate::usb::{Configuration, DeviceDescriptor, Setup, Speed, first_language, string_ascii};

/// The key codes of `linux/input-event-codes.h` a keyboard's usages must
/// reach, by the HID usage tables' names.
#[test]
fn the_usage_table_gives_linuxs_codes() {
    let cases: [(usize, u8); 18] = [
        (0x04, 30),  // a: KEY_A
        (0x1D, 44),  // z: KEY_Z
        (0x1E, 2),   // 1: KEY_1
        (0x27, 11),  // 0: KEY_0
        (0x28, 28),  // Enter
        (0x29, 1),   // Escape
        (0x2A, 14),  // Backspace
        (0x2C, 57),  // Space
        (0x39, 58),  // Caps Lock
        (0x3A, 59),  // F1
        (0x45, 88),  // F12
        (0x4F, 106), // Right
        (0x52, 103), // Up
        (0x64, 86),  // Non-US backslash: KEY_102ND
        (0xE0, 29),  // Left Control
        (0xE1, 42),  // Left Shift
        (0xE6, 100), // Right Alt
        (0xE7, 126), // Right GUI: KEY_RIGHTMETA
    ];
    for (usage, code) in cases {
        assert_eq!(KEYBOARD_CODES[usage], code, "usage {usage:#04x}");
    }
    assert_eq!(KEYBOARD_CODES[0x00], 0, "no event");
    assert_eq!(KEYBOARD_CODES[0x01], 0, "ErrorRollOver");
}

fn boot(layout: &[u8]) -> Interpreter {
    Interpreter::new(Descriptor::parse(layout).expect("the boot layout parses"))
}

fn run(reader: &mut Interpreter, report: &[u8]) -> std::vec::Vec<RawEvent> {
    let mut out = EventBuf::default();
    reader.report(report, &mut out);
    out.as_slice().to_vec()
}

#[test]
fn a_boot_keyboard_declares_every_code_it_can_send_its_leds_and_nothing_relative() {
    let reader = boot(&BOOT_KEYBOARD);
    let bits = reader.bits();
    assert!(bits.has_type(EV_SYN) && bits.has_type(EV_KEY) && bits.has_type(EV_LED));
    assert!(!bits.has_type(EV_REL));
    // The boot layout's keys are usages 0 to 0x65, and the modifiers.
    for (usage, &code) in KEYBOARD_CODES.iter().enumerate() {
        let reachable = usage <= 0x65 || (0xE0..=0xE7).contains(&usage);
        if code != 0 && reachable {
            assert!(bit(&bits.keys, u16::from(code)), "usage {usage:#x}");
        }
    }
    for led in 0..5 {
        assert!(bit(&bits.leds, led));
    }
    assert!(!bit(&bits.keys, BTN_LEFT));
    assert_eq!(reader.kind(), Kind::Keyboard);
}

#[test]
fn a_boot_mouse_declares_five_buttons_and_three_axes() {
    let reader = boot(&BOOT_MOUSE);
    let bits = reader.bits();
    assert!(bits.has_type(EV_REL) && !bits.has_type(EV_LED));
    for code in BTN_LEFT..=BTN_EXTRA {
        assert!(bit(&bits.keys, code));
    }
    assert!(bit(&bits.rels, REL_WHEEL));
    assert_eq!(reader.kind(), Kind::Mouse);
}

/// A short report reads as if the rest were zeros, as Linux's
/// `hid_report_raw_event` pads it; an empty one or a still mouse is nothing.
#[test]
fn a_short_report_is_padded_and_an_empty_one_changes_nothing() {
    let mut keyboard = boot(&BOOT_KEYBOARD);
    assert_eq!(
        run(&mut keyboard, &[0, 0, 4]),
        [
            RawEvent::new(EV_KEY, 30, 1),
            RawEvent::new(EV_SYN, SYN_REPORT, 0)
        ]
    );
    let mut mouse = boot(&BOOT_MOUSE);
    assert!(run(&mut mouse, &[]).is_empty());
    assert!(run(&mut mouse, &[0, 0, 0]).is_empty(), "nothing moved");
}

#[test]
fn a_three_byte_mouse_report_has_no_wheel() {
    let mut mouse = boot(&BOOT_MOUSE);
    assert_eq!(
        run(&mut mouse, &[0x10, 0, 0]),
        [
            RawEvent::new(EV_KEY, BTN_EXTRA, 1),
            RawEvent::new(EV_SYN, SYN_REPORT, 0)
        ]
    );
}

#[test]
fn the_boot_keyboards_fields_are_where_appendix_b_puts_them() {
    let descriptor = Descriptor::parse(&BOOT_KEYBOARD).unwrap();
    let fields = descriptor.fields();
    assert_eq!(fields.len(), 5);
    let modifiers = fields[0];
    assert_eq!(
        (modifiers.offset, modifiers.size, modifiers.count),
        (0, 1, 8)
    );
    assert!(modifiers.variable && !modifiers.constant);
    assert_eq!(descriptor.usage(&modifiers, 7), Some(0x0007_00E7));
    assert!(fields[1].constant, "the reserved byte");
    let leds = fields[2];
    assert_eq!(leds.direction, Direction::Output);
    assert_eq!((leds.offset, leds.count), (0, 5));
    assert_eq!(descriptor.output_bytes(0), 1);
    let keys = fields[4];
    assert_eq!((keys.offset, keys.size, keys.count), (16, 8, 6));
    assert!(!keys.variable);
    assert_eq!((keys.minimum, keys.maximum), (0, 0x65));
    assert!(!descriptor.numbered);
}

#[test]
fn values_are_read_little_endian_and_sign_extended_when_the_minimum_is_negative() {
    // Sixteen-bit X and Y from -32767, eight-bit wheel from -127.
    let descriptor = Descriptor::parse(&[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95,
        0x02, 0x09, 0x30, 0x09, 0x31, 0x81, 0x06, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01,
        0x09, 0x38, 0x81, 0x06, 0xC0,
    ])
    .unwrap();
    let fields = descriptor.fields();
    assert_eq!(fields[0].minimum, -32767);
    let report = [0x2C, 0x01, 0xFE, 0xFF, 0x81];
    assert_eq!(report::value(&fields[0], &report, 0), Some(300));
    assert_eq!(report::value(&fields[0], &report, 1), Some(-2));
    assert_eq!(report::value(&fields[1], &report, 0), Some(-127));
    assert_eq!(
        report::value(&fields[1], &report, 1),
        None,
        "past the report"
    );
}

#[test]
fn a_32_bit_field_and_an_array_from_i32_min_are_read_without_a_panic() {
    // A vendor report: one 32-bit variable, then an array whose Logical
    // Minimum is i32::MIN, as real keyboards' second interfaces carry.
    let mut reader = Interpreter::new(
        Descriptor::parse(&[
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x15, 0x00, 0x25, 0x01, 0x75, 0x20, 0x95,
            0x01, 0x09, 0x01, 0x81, 0x02, 0x05, 0x07, 0x17, 0x00, 0x00, 0x00, 0x80, 0x27, 0xFF,
            0xFF, 0xFF, 0x7F, 0x19, 0x00, 0x29, 0xFF, 0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xC0,
        ])
        .expect("parses"),
    );
    let fields = reader.descriptor().fields();
    assert_eq!(fields[0].size, 32);
    assert_eq!(fields[1].minimum, i32::MIN);
    assert_eq!(report::value(&fields[0], &[0xFF; 5], 0), Some(-1));
    // 0x7F - i32::MIN does not fit: the value is not a usage, not a panic.
    let _ = run(&mut reader, &[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
    let _ = run(&mut reader, &[0x00; 5]);
}

#[test]
fn a_one_byte_maximum_of_0xff_is_read_unsigned() {
    // Logical Maximum 0xFF in one byte would be -1 read signed.
    let descriptor = Descriptor::parse(&[
        0x05, 0x07, 0x15, 0x00, 0x25, 0xFF, 0x19, 0x00, 0x29, 0xFF, 0x75, 0x08, 0x95, 0x01, 0x81,
        0x00,
    ])
    .unwrap();
    assert_eq!(descriptor.fields()[0].maximum, 0xFF);
}

#[test]
fn push_and_pop_restore_the_globals_and_long_items_are_read_past() {
    let descriptor = Descriptor::parse(&[
        0x05, 0x09, 0x75, 0x01, 0x95, 0x03, 0xA4, 0x75, 0x08, 0x95, 0x01, 0x81, 0x03, 0xB4, //
        0xFE, 0x02, 0x00, 0xAA, 0xBB, // a long item
        0x19, 0x01, 0x29, 0x03, 0x81, 0x02,
    ])
    .unwrap();
    let fields = descriptor.fields();
    assert_eq!((fields[0].size, fields[0].count), (8, 1));
    assert_eq!(
        (fields[1].size, fields[1].count, fields[1].offset),
        (1, 3, 8)
    );
    assert_eq!(descriptor.usage(&fields[1], 2), Some(0x0009_0003));
}

#[test]
fn a_cut_item_is_refused_and_a_four_byte_usage_keeps_its_own_page() {
    assert_eq!(
        Descriptor::parse(&[0x05, 0x01, 0x26, 0xFF]),
        Err(report::Error::Truncated)
    );
    let descriptor = Descriptor::parse(&[
        0x05, 0x01, 0x0B, 0xE9, 0x00, 0x0C, 0x00, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
    ])
    .unwrap();
    assert_eq!(
        descriptor.usage(&descriptor.fields()[0], 0),
        Some(0x000C_00E9)
    );
}

#[test]
fn media_keys_map_as_linux_maps_them() {
    assert_eq!(map(0x000C_00E9, 0x000C_0001, false), Some((EV_KEY, 115)));
    assert_eq!(map(0x000C_00CD, 0x000C_0001, false), Some((EV_KEY, 164)));
    assert_eq!(map(0x000C_0238, 0x000C_0001, true), Some((EV_REL, 6)));
    assert_eq!(map(0x0001_0082, 0x0001_0080, false), Some((EV_KEY, 142)));
    // Buttons count from BTN_MOUSE in a mouse and BTN_MISC elsewhere.
    assert_eq!(map(0x0009_0001, 0x0001_0002, false), Some((EV_KEY, 0x110)));
    assert_eq!(map(0x0009_0001, 0x000C_0001, false), Some((EV_KEY, 0x100)));
    assert_eq!(map(0x0009_0011, 0x0001_0002, false), None, "past sixteen");
    assert_eq!(map(0xFF00_0001, 0xFF00_0001, false), None);
}

#[test]
fn names_are_joined_as_linux_joins_them() {
    let joined = |m: &[u8], p: &[u8], kind| hid::name(m, p, b"USB", kind).as_bytes().to_vec();
    assert_eq!(
        joined(b"Logitech", b"G502 HERO Gaming Mouse", Kind::Mouse),
        b"Logitech G502 HERO Gaming Mouse"
    );
    assert_eq!(
        joined(b"Logitech", b"G502 HERO Gaming Mouse", Kind::Keyboard),
        b"Logitech G502 HERO Gaming Mouse Keyboard"
    );
    assert_eq!(
        joined(b"Logitech", b"Logitech USB Receiver", Kind::Other),
        b"Logitech USB Receiver"
    );
    assert_eq!(joined(b"", b"Mouse", Kind::Mouse), b"Mouse");
    assert_eq!(joined(b"", b"", Kind::Keyboard), b"USB Keyboard");
}

/// The SETUP packet of `GET_DESCRIPTOR(DEVICE)`, byte for byte as USB 2.0's
/// table 9-2 lays it out.
#[test]
fn a_setup_packet_is_laid_out_little_endian() {
    assert_eq!(
        Setup::get_descriptor(1, 0, 0, 18).bytes(),
        [0x80, 6, 0, 1, 0, 0, 18, 0]
    );
    assert_eq!(get_port_status(3).bytes(), [0xA3, 0, 0, 0, 3, 0, 4, 0]);
    assert!(Setup::get_descriptor(1, 0, 0, 18).is_in());
    assert!(!Setup::set_address(5).is_in());
}

#[test]
fn a_device_descriptor_is_read() {
    let bytes = [
        18, 1, 0, 2, 0, 0, 0, 64, 0x6D, 0x04, 0x8B, 0xC0, 0x00, 0x74, 1, 2, 3, 1,
    ];
    let descriptor = DeviceDescriptor::parse(&bytes).unwrap();
    assert_eq!(
        (descriptor.vendor, descriptor.product, descriptor.release),
        (0x046D, 0xC08B, 0x7400)
    );
    assert_eq!(descriptor.max_packet, 64);
    assert_eq!((descriptor.manufacturer, descriptor.product_name), (1, 2));
    assert!(DeviceDescriptor::parse(&bytes[..17]).is_none());
}

/// A keyboard's configuration as such keyboards give it: a boot interface,
/// a HID descriptor between it and its endpoint, and a second interface
/// with an alternate setting that must not count.
#[test]
fn a_configuration_gives_its_interfaces_and_their_interrupt_endpoints() {
    let bytes = [
        9, 2, 75, 0, 2, 1, 0, 0xA0, 49, //
        9, 4, 0, 0, 1, 3, 1, 1, 0, //
        9, 0x21, 0x10, 0x01, 0, 1, 0x22, 65, 0, //
        7, 5, 0x81, 3, 8, 0, 10, //
        9, 4, 1, 0, 2, 3, 0, 0, 0, //
        9, 0x21, 0x10, 0x01, 0, 1, 0x22, 50, 0, //
        7, 5, 0x02, 3, 8, 0, 10, // OUT: not the one
        7, 5, 0x82, 3, 8, 0, 10, //
        9, 4, 1, 1, 1, 3, 0, 0, 0, // alternate setting 1: left out
        7, 5, 0x83, 3, 8, 0, 10,
    ];
    assert_eq!(Configuration::total_length(&bytes[..9]), Some(75));
    let config = Configuration::parse(&bytes).unwrap();
    assert_eq!(config.value, 1);
    let interfaces = config.interfaces();
    assert_eq!(interfaces.len(), 2);
    assert_eq!(
        (
            interfaces[0].class,
            interfaces[0].subclass,
            interfaces[0].protocol
        ),
        (3, 1, 1)
    );
    assert_eq!(interfaces[0].interrupt_in, Some((1, 8)));
    assert_eq!(interfaces[1].interrupt_in, Some((2, 8)));
}

#[test]
fn a_truncated_configuration_keeps_what_was_whole() {
    let bytes = [
        9, 2, 34, 0, 1, 1, 0, 0xA0, 49, 9, 4, 0, 0, 1, 3, 1, 2, 0, 7, 5, 0x81,
    ];
    let config = Configuration::parse(&bytes).unwrap();
    assert_eq!(config.interfaces().len(), 1);
    assert_eq!(config.interfaces()[0].interrupt_in, None);
}

#[test]
fn strings_become_ascii_without_trailing_spaces() {
    let descriptor = [14, 3, b'S', 0, b'E', 0, b'M', 0, 0xE9, 0, b' ', 0, b' ', 0];
    let mut out = [0_u8; 16];
    let len = string_ascii(&descriptor, &mut out);
    assert_eq!(&out[..len], b"SEM?");
    assert_eq!(first_language(&[4, 3, 0x09, 0x04]), Some(0x0409));
    assert_eq!(first_language(&[2, 3]), None);
}

#[test]
fn a_port_status_says_speed_and_changes() {
    let status = PortStatus::parse(&[0x03, 0x03, 0x11, 0x00]).unwrap();
    assert!(status.connected());
    assert_eq!(status.speed(), Speed::Low);
    assert_eq!(status.changes().collect::<std::vec::Vec<_>>(), [16, 20]);
    assert_eq!(
        PortStatus::parse(&[0x03, 0x05, 0, 0]).unwrap().speed(),
        Speed::High
    );
    assert_eq!(
        PortStatus::parse(&[0x03, 0x01, 0, 0]).unwrap().speed(),
        Speed::Full
    );
}

/// The unannounced service backs off while it finds nothing and is back at
/// 8 ms as soon as something moves.
#[test]
fn the_service_poll_backs_off_while_idle() {
    use crate::bus::{SERVICE_POLL, SERVICE_POLL_IDLE_MAX, next_service_poll};
    assert_eq!(next_service_poll(SERVICE_POLL, false), 2 * SERVICE_POLL);
    assert_eq!(
        next_service_poll(2 * SERVICE_POLL, false),
        SERVICE_POLL_IDLE_MAX
    );
    assert_eq!(
        next_service_poll(SERVICE_POLL_IDLE_MAX, false),
        SERVICE_POLL_IDLE_MAX
    );
    assert_eq!(next_service_poll(SERVICE_POLL_IDLE_MAX, true), SERVICE_POLL);
    assert_eq!(next_service_poll(0, false), SERVICE_POLL);
}
