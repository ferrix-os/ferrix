//! HID input and output: a device's reports turned into the events Linux's
//! HID driver makes of them, the HELLO declaring those events, and the LED
//! output report a keyboard is sent.
//!
//! Every function is read through a report descriptor ([`crate::report`]):
//! the device's own, in the report protocol, or for a boot interface whose
//! descriptor could not be read, the layout the HID specification's appendix
//! B fixes for the boot protocol, written out as a descriptor here
//! ([`BOOT_KEYBOARD`], [`BOOT_MOUSE`]). One interpreter reads both.
//!
//! Usages become codes by the tables of Linux's `drivers/hid/hid-input.c`,
//! as far as keyboards, mice and their media keys need them: the keyboard
//! page by `hid_keyboard`, buttons from `BTN_MOUSE` in a mouse and
//! `BTN_MISC` elsewhere, the relative desktop axes, a pointer's absolute
//! ones, the system controls, and the consumer page's media and application
//! keys. Events come in the report's field order, an array's keys let go
//! before the ones pressed, and a report ends in `SYN_REPORT`, as there.
//!
//! # Absolute pointers
//!
//! A tablet -- QEMU's `usb-tablet`, a KVM switch such as the `JetKVM` --
//! reports X and Y as where the pointer is, in its field's logical range
//! (0 to 32767 for both of those), rather than as how far it moved. Those
//! are `ABS_X` and `ABS_Y`, an absolute wheel `ABS_WHEEL`, with the logical
//! minimum and maximum as the axis's range in the HELLO, which is what
//! `EVIOCGABS` answers and what a compositor scales to its output by;
//! `hidinput_configure_usage` sets the same range, and no fuzz or flat for
//! anything but a joystick or gamepad. Only inside a mouse or pointer
//! application: a joystick's axes are not a pointer, and a compositor that
//! read them as one would move the cursor with the stick. A value outside
//! the logical range is no position and is dropped, as Linux drops one from
//! a field with a null state; one that repeats the last is not sent again.

use ferrix_inputctl::message::{
    AXES, AxisRange, Bitmaps, DeviceId, Hello, MAX_EVENTS, RawEvent, Text, VERSION,
};
use ferrix_linux_abi::input::{
    ABS_WHEEL, ABS_X, ABS_Y, BTN_MISC, BTN_MOUSE, BUS_USB, EV_ABS, EV_KEY, EV_LED, EV_REL, EV_SYN,
    KEY_CNT, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y, SYN_REPORT,
};

use crate::report::{
    self, Descriptor, Direction, Field, PAGE_BUTTON, PAGE_CONSUMER, PAGE_DESKTOP, PAGE_KEYBOARD,
    PAGE_LED, Usage, split,
};

/// `hid_keyboard`: each keyboard usage's key code, zero for none.
pub const KEYBOARD_CODES: [u8; 256] = [
    0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, //
    50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, 2, 3, //
    4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26, //
    27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64, //
    65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104, 111, 107, 109, 106, //
    105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71, //
    72, 73, 82, 83, 86, 127, 116, 117, 183, 184, 185, 186, 187, 188, 189, 190, //
    191, 192, 193, 194, 134, 138, 130, 132, 128, 129, 131, 137, 133, 135, 136, 113, //
    115, 114, 0, 0, 0, 121, 0, 89, 93, 124, 92, 94, 95, 0, 0, 0, //
    122, 123, 90, 91, 85, 0, 0, 0, 0, 0, 0, 0, 111, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 179, 180, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 111, 0, 0, 0, 0, 0, 0, 0, //
    29, 42, 56, 125, 97, 54, 100, 126, 164, 166, 165, 163, 161, 115, 114, 113, //
    150, 158, 159, 128, 136, 177, 178, 176, 142, 152, 173, 140, 0, 0, 0, 0, //
];

/// The consumer page's usages `hid-input.c` maps to keys, and their codes.
pub const CONSUMER_CODES: [(u16, u16); 32] = [
    (0x030, 116), // KEY_POWER
    (0x032, 142), // KEY_SLEEP
    (0x040, 139), // KEY_MENU
    (0x06F, 225), // KEY_BRIGHTNESSUP
    (0x070, 224), // KEY_BRIGHTNESSDOWN
    (0x0B0, 207), // KEY_PLAY
    (0x0B1, 119), // KEY_PAUSE
    (0x0B2, 167), // KEY_RECORD
    (0x0B3, 208), // KEY_FASTFORWARD
    (0x0B4, 168), // KEY_REWIND
    (0x0B5, 163), // KEY_NEXTSONG
    (0x0B6, 165), // KEY_PREVIOUSSONG
    (0x0B7, 166), // KEY_STOPCD
    (0x0B8, 161), // KEY_EJECTCD
    (0x0CD, 164), // KEY_PLAYPAUSE
    (0x0E2, 113), // KEY_MUTE
    (0x0E9, 115), // KEY_VOLUMEUP
    (0x0EA, 114), // KEY_VOLUMEDOWN
    (0x183, 171), // KEY_CONFIG
    (0x18A, 155), // KEY_MAIL
    (0x192, 140), // KEY_CALC
    (0x194, 144), // KEY_FILE
    (0x196, 150), // KEY_WWW
    (0x19E, 152), // KEY_COFFEE
    (0x1A6, 138), // KEY_HELP
    (0x221, 217), // KEY_SEARCH
    (0x223, 172), // KEY_HOMEPAGE
    (0x224, 158), // KEY_BACK
    (0x225, 159), // KEY_FORWARD
    (0x226, 128), // KEY_STOP
    (0x227, 173), // KEY_REFRESH
    (0x22A, 156), // KEY_BOOKMARKS
];

/// Generic Desktop's system controls: power down, sleep, wake up.
const SYSTEM_CODES: [(u16, u16); 3] = [(0x81, 116), (0x82, 142), (0x83, 143)];

/// The consumer page's horizontal wheel, `AC Pan`.
const AC_PAN: u16 = 0x238;

/// The keyboard page's `ErrorRollOver`: an array reporting it says more keys
/// are down than fit, and the report is ignored.
const ROLL_OVER: Usage = 0x0007_0001;

/// The applications, as their collections' usages.
const APPLICATION_POINTER: Usage = 0x0001_0001;
const APPLICATION_MOUSE: Usage = 0x0001_0002;
const APPLICATION_KEYBOARD: Usage = 0x0001_0006;
const APPLICATION_SYSTEM: Usage = 0x0001_0080;
const APPLICATION_CONSUMER: Usage = 0x000C_0001;

/// The most buttons mapped: sixteen, `BTN_MOUSE` to `BTN_MOUSE + 15`.
const MAX_BUTTONS: u16 = 16;

/// The boot keyboard's report descriptor, HID 1.11 appendix B.1: eight
/// modifier bits, a reserved byte, five LED bits out, six key slots.
pub const BOOT_KEYBOARD: [u8; 63] = [
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

/// The boot mouse's: appendix B.2's three bytes, with the two side buttons
/// and the wheel byte many boot mice send too (a report without them reads
/// as their being still).
pub const BOOT_MOUSE: [u8; 52] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03,
    0x81, 0x06, 0xC0, 0xC0,
];

/// The event a usage in `application` makes: a key or button, or with
/// `relative` an axis; a pointer's absolute X, Y and wheel without it.
/// `None` for what is not mapped.
#[must_use]
pub fn map(usage: Usage, application: Usage, relative: bool) -> Option<(u16, u16)> {
    let (page, id) = split(usage);
    let key = |code: u16| (code != 0).then_some((EV_KEY, code));
    match page {
        PAGE_KEYBOARD if !relative => key(u16::from(*KEYBOARD_CODES.get(usize::from(id))?)),
        PAGE_BUTTON if !relative && (1..=MAX_BUTTONS).contains(&id) => {
            let base = match application {
                APPLICATION_MOUSE | APPLICATION_POINTER => BTN_MOUSE,
                _ => BTN_MISC,
            };
            key(base + id - 1)
        }
        PAGE_DESKTOP if relative => match id {
            0x30 => Some((EV_REL, REL_X)),
            0x31 => Some((EV_REL, REL_Y)),
            0x38 => Some((EV_REL, REL_WHEEL)),
            _ => None,
        },
        PAGE_DESKTOP
            if matches!(application, APPLICATION_MOUSE | APPLICATION_POINTER)
                && matches!(id, 0x30 | 0x31 | 0x38) =>
        {
            Some((
                EV_ABS,
                match id {
                    0x30 => ABS_X,
                    0x31 => ABS_Y,
                    _ => ABS_WHEEL,
                },
            ))
        }
        PAGE_DESKTOP => SYSTEM_CODES
            .iter()
            .find(|(usage, _)| *usage == id)
            .and_then(|&(_, code)| key(code)),
        PAGE_CONSUMER if relative => (id == AC_PAN).then_some((EV_REL, REL_HWHEEL)),
        PAGE_CONSUMER => CONSUMER_CODES
            .iter()
            .find(|(usage, _)| *usage == id)
            .and_then(|&(_, code)| key(code)),
        _ => None,
    }
}

/// The LED an LED-page usage names: Num Lock, Caps Lock, Scroll Lock,
/// Compose and Kana are `LED_NUML` to `LED_KANA`, 0 to 4.
#[must_use]
pub fn led(usage: Usage) -> Option<u16> {
    let (page, id) = split(usage);
    (page == PAGE_LED && (1..=5).contains(&id)).then(|| id - 1)
}

/// What a function mostly is, by its first application that maps anything:
/// for its name and the driver's console lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A keyboard.
    Keyboard,
    /// A mouse or other pointer.
    Mouse,
    /// Media and application keys.
    Consumer,
    /// Power, sleep and wake.
    System,
    /// Anything else that maps a key.
    Other,
}

impl Kind {
    /// The suffix Linux's HID core names an input by its application with.
    #[must_use]
    pub const fn suffix(self) -> Option<&'static str> {
        match self {
            Kind::Keyboard => Some("Keyboard"),
            Kind::Mouse => Some("Mouse"),
            Kind::Consumer => Some("Consumer Control"),
            Kind::System => Some("System Control"),
            Kind::Other => None,
        }
    }

    /// A word for the console.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Kind::Keyboard => "keyboard",
            Kind::Mouse => "mouse",
            Kind::Consumer => "media keys",
            Kind::System => "system keys",
            Kind::Other => "input",
        }
    }
}

/// The most events one report makes, leaving room for `SYN_REPORT` in one
/// EVENTS message; a report that makes more is cut there.
pub const MAX_REPORT_EVENTS: usize = MAX_EVENTS - 1;

/// The events one report made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EventBuf {
    events: [RawEvent; MAX_EVENTS],
    len: usize,
}

impl Default for EventBuf {
    fn default() -> Self {
        EventBuf {
            events: [RawEvent::default(); MAX_EVENTS],
            len: 0,
        }
    }
}

impl EventBuf {
    fn push(&mut self, kind: u16, code: u16, value: i32) {
        if self.len < MAX_REPORT_EVENTS
            && let Some(slot) = self.events.get_mut(self.len)
        {
            *slot = RawEvent::new(kind, code, value);
            self.len += 1;
        }
    }

    /// Close the report with `SYN_REPORT`, if it holds anything.
    fn finish(&mut self) {
        if let (true, Some(slot)) = (self.len > 0, self.events.get_mut(self.len)) {
            *slot = RawEvent::new(EV_SYN, SYN_REPORT, 0);
            self.len += 1;
        }
    }

    /// Empty it.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// The events, empty for a report that changed nothing.
    #[must_use]
    pub fn as_slice(&self) -> &[RawEvent] {
        self.events.get(..self.len).unwrap_or(&[])
    }
}

/// Keys and buttons down, a bit per code.
const KEY_BYTES: usize = (KEY_CNT as usize).div_ceil(8);

/// Array values kept between reports, over every array field.
const MAX_HELD: usize = 64;

/// One function's reader: its descriptor, and what its last reports said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Interpreter {
    descriptor: Descriptor,
    /// Keys and buttons down.
    down: [u8; KEY_BYTES],
    /// Each array field's values in the last report, from `held_at`.
    held: [Usage; MAX_HELD],
    held_at: [u8; report::MAX_FIELDS],
    /// LEDs lit, a bit per LED code.
    leds: u16,
    /// Each absolute axis's last value, `None` until a report gave one.
    at: [Option<i32>; ABS_TRACKED],
}

/// The absolute axes a value is kept for: `ABS_X` to `ABS_WHEEL`.
const ABS_TRACKED: usize = ABS_WHEEL as usize + 1;

impl Interpreter {
    /// A reader of reports `descriptor` describes, nothing pressed.
    #[must_use]
    pub fn new(descriptor: Descriptor) -> Interpreter {
        let mut held_at = [u8::MAX; report::MAX_FIELDS];
        let mut next = 0_usize;
        for (slot, field) in held_at.iter_mut().zip(descriptor.fields()) {
            let count = usize::from(field.count);
            if field.direction == Direction::Input && !field.variable && next + count <= MAX_HELD {
                *slot = next as u8;
                next += count;
            }
        }
        Interpreter {
            descriptor,
            down: [0; KEY_BYTES],
            held: [0; MAX_HELD],
            held_at,
            leds: 0,
            at: [None; ABS_TRACKED],
        }
    }

    /// The descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// What the function mostly is.
    #[must_use]
    pub fn kind(&self) -> Kind {
        for field in self.descriptor.fields() {
            let maps = field.direction == Direction::Input
                && !field.constant
                && self
                    .descriptor
                    .all_usages(field)
                    .any(|usage| map(usage, field.application, field.relative).is_some());
            if !maps {
                continue;
            }
            return match field.application {
                APPLICATION_KEYBOARD => Kind::Keyboard,
                APPLICATION_MOUSE | APPLICATION_POINTER => Kind::Mouse,
                APPLICATION_CONSUMER => Kind::Consumer,
                APPLICATION_SYSTEM => Kind::System,
                _ => Kind::Other,
            };
        }
        Kind::Other
    }

    /// What the function declares: every code an input field can report,
    /// and the LEDs its output fields light.
    #[must_use]
    pub fn bits(&self) -> Bitmaps {
        let mut bits = Bitmaps::EMPTY;
        for field in self
            .descriptor
            .fields()
            .iter()
            .filter(|field| !field.constant)
        {
            for usage in self.descriptor.all_usages(field) {
                let mapped = match field.direction {
                    Direction::Input => map(usage, field.application, field.relative),
                    Direction::Output => led(usage).map(|code| (EV_LED, code)),
                };
                let Some((kind, code)) = mapped else {
                    continue;
                };
                // An axis whose range is empty is not declared: the core
                // refuses a HELLO whose axis has its minimum past its
                // maximum. Nor is one in an array, which never reports it.
                if kind == EV_ABS && (!field.variable || axis_range(field).is_none()) {
                    continue;
                }
                set(&mut bits.types, kind);
                if let Some(codes) = bits.codes_mut(kind) {
                    set(codes, code);
                }
            }
        }
        if bits.types.iter().any(|&byte| byte != 0) {
            set(&mut bits.types, EV_SYN);
        }
        bits
    }

    /// Each absolute axis's range, from its field's logical minimum and
    /// maximum; the default for every axis not declared. Where two fields
    /// give the same axis, the first one's.
    #[must_use]
    pub fn axes(&self) -> [AxisRange; AXES] {
        let mut axes = [AxisRange::default(); AXES];
        let mut given = [false; AXES];
        for field in self.descriptor.fields().iter().filter(|field| {
            field.direction == Direction::Input && field.variable && !field.constant
        }) {
            let Some(range) = axis_range(field) else {
                continue;
            };
            for usage in self.descriptor.all_usages(field) {
                let Some((EV_ABS, code)) = map(usage, field.application, field.relative) else {
                    continue;
                };
                if let (Some(slot), Some(done)) = (
                    axes.get_mut(usize::from(code)),
                    given.get_mut(usize::from(code)),
                ) && !*done
                {
                    *slot = range;
                    *done = true;
                }
            }
        }
        axes
    }

    /// Whether anything is declared at all.
    #[must_use]
    pub fn maps_anything(&self) -> bool {
        let bits = self.bits();
        bits.has_type(EV_KEY) || bits.has_type(EV_REL) || bits.has_type(EV_ABS)
    }

    /// The events `report` makes, given what came before it. `report` is
    /// the whole report, its ID byte first when the descriptor numbers them.
    pub fn report(&mut self, report: &[u8], out: &mut EventBuf) {
        out.clear();
        let (id, body) = match (self.descriptor.numbered, report.split_first()) {
            (true, Some((&id, body))) => (id, body),
            (true, None) => return,
            (false, _) => (0, report),
        };
        for index in 0..self.descriptor.fields().len() {
            let Some(field) = self.descriptor.fields().get(index).copied() else {
                continue;
            };
            if field.direction != Direction::Input || field.report != id || field.constant {
                continue;
            }
            if field.variable {
                self.variable(&field, body, out);
            } else {
                self.array(index, &field, body, out);
            }
        }
        out.finish();
    }

    fn variable(&mut self, field: &Field, body: &[u8], out: &mut EventBuf) {
        for index in 0..u32::from(field.count) {
            let (Some(usage), Some(value)) = (
                self.descriptor.usage(field, index),
                report::value(field, body, index),
            ) else {
                continue;
            };
            match map(usage, field.application, field.relative) {
                Some((EV_REL, code)) if value != 0 => out.push(EV_REL, code, value),
                Some((EV_ABS, code)) if (field.minimum..=field.maximum).contains(&value) => {
                    self.absolute(code, value, out);
                }
                Some((EV_KEY, code)) if !field.relative => self.key(code, value != 0, out),
                _ => {}
            }
        }
    }

    fn array(&mut self, index: usize, field: &Field, body: &[u8], out: &mut EventBuf) {
        let count = usize::from(field.count);
        let Some(start) = self
            .held_at
            .get(index)
            .copied()
            .filter(|&start| start != u8::MAX)
            .map(usize::from)
        else {
            return;
        };
        let mut now = [0_u32; report::MAX_COUNT];
        for (slot, position) in now.iter_mut().zip(0..count as u32) {
            let usage = report::value(field, body, position)
                .filter(|value| (field.minimum..=field.maximum).contains(value))
                .and_then(|value| value.checked_sub(field.minimum))
                .and_then(|offset| u32::try_from(offset).ok())
                .and_then(|offset| self.descriptor.usage(field, offset));
            *slot = usage.unwrap_or(0);
        }
        let now = now.get(..count).unwrap_or(&[]);
        if now.contains(&ROLL_OVER) {
            return;
        }
        let mut before = [0_u32; report::MAX_COUNT];
        if let (Some(dest), Some(held)) =
            (before.get_mut(..count), self.held.get(start..start + count))
        {
            dest.copy_from_slice(held);
        }
        let before = before.get(..count).unwrap_or(&[]);
        for &usage in before
            .iter()
            .filter(|usage| **usage != 0 && !now.contains(usage))
        {
            if let Some((EV_KEY, code)) = map(usage, field.application, false) {
                self.key(code, false, out);
            }
        }
        for &usage in now
            .iter()
            .filter(|usage| **usage != 0 && !before.contains(usage))
        {
            if let Some((EV_KEY, code)) = map(usage, field.application, false) {
                self.key(code, true, out);
            }
        }
        if let Some(held) = self.held.get_mut(start..start + count) {
            held.copy_from_slice(now);
        }
    }

    /// An absolute event if axis `code` moved, or has not been said yet.
    fn absolute(&mut self, code: u16, value: i32, out: &mut EventBuf) {
        let Some(last) = self.at.get_mut(usize::from(code)) else {
            return;
        };
        if *last != Some(value) {
            *last = Some(value);
            out.push(EV_ABS, code, value);
        }
    }

    /// A key event if `code` changes.
    fn key(&mut self, code: u16, down: bool, out: &mut EventBuf) {
        let Some(byte) = self.down.get_mut(usize::from(code / 8)) else {
            return;
        };
        let bit = 1 << (code % 8);
        if (*byte & bit != 0) != down {
            *byte ^= bit;
            out.push(EV_KEY, code, i32::from(down));
        }
    }

    /// Take the LED events the core sent: whether any LED changed.
    pub fn set_leds(&mut self, events: &[RawEvent]) -> bool {
        let before = self.leds;
        for event in events
            .iter()
            .filter(|event| event.kind == EV_LED && event.code < 16)
        {
            if event.value != 0 {
                self.leds |= 1 << event.code;
            } else {
                self.leds &= !(1 << event.code);
            }
        }
        self.leds != before
    }

    /// Each output report holding an LED, with the LEDs as they are: the
    /// report's ID (0 when reports are not numbered) and its bytes, the ID
    /// first when they are, handed to `send` one report at a time.
    pub fn led_reports(&self, mut send: impl FnMut(u8, &[u8])) {
        let mut sent = [false; 256];
        for field in self.descriptor.fields() {
            let is_led = field.direction == Direction::Output
                && self
                    .descriptor
                    .all_usages(field)
                    .any(|usage| led(usage).is_some());
            let Some(done) = sent.get_mut(usize::from(field.report)) else {
                continue;
            };
            if !is_led || *done {
                continue;
            }
            *done = true;
            let id = field.report;
            let mut bytes = [0_u8; 16];
            let skip = usize::from(self.descriptor.numbered);
            let length = skip + self.descriptor.output_bytes(id);
            let Some(report) = bytes.get_mut(..length) else {
                continue;
            };
            if let Some(first) = report.first_mut().filter(|_| skip == 1) {
                *first = id;
            }
            self.fill_leds(id, report.get_mut(skip..).unwrap_or(&mut []));
            send(id, report);
        }
    }
}

impl Interpreter {
    /// Set every LED bit of output report `id`'s body as the LEDs are.
    fn fill_leds(&self, id: u8, body: &mut [u8]) {
        let fields = self.descriptor.fields().iter().filter(|field| {
            field.direction == Direction::Output && field.report == id && field.variable
        });
        for field in fields {
            for index in 0..u32::from(field.count) {
                let lit = self
                    .descriptor
                    .usage(field, index)
                    .and_then(led)
                    .is_some_and(|code| self.leds & (1 << code) != 0);
                report::set_value(field, body, index, u32::from(lit));
            }
        }
    }
}

/// Who a device says it is, for its HELLO.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// `bcdDevice`.
    pub release: u16,
    /// Its name: the manufacturer's and product's strings, as Linux joins
    /// them, and the application's suffix.
    pub name: Text,
}

/// The HELLO for a function reading reports as `interpreter` does, at
/// `location` -- which for a device tree node is `DEVICE_NOT_PCI`, what START
/// said.
#[must_use]
pub fn hello(interpreter: &Interpreter, identity: &Identity, location: u32) -> Hello {
    let mut hello = Hello::EMPTY;
    hello.version = VERSION;
    hello.location = location;
    hello.id = DeviceId {
        bustype: BUS_USB,
        vendor: identity.vendor,
        product: identity.product,
        version: identity.release,
    };
    hello.name = identity.name;
    hello.bits = interpreter.bits();
    hello.axes = interpreter.axes();
    hello
}

/// An absolute field's range: its logical minimum and maximum, no fuzz, no
/// flat and no resolution, as `hidinput_configure_usage` gives a pointer's.
/// `None` for an empty range.
fn axis_range(field: &Field) -> Option<AxisRange> {
    (field.minimum <= field.maximum).then_some(AxisRange {
        minimum: field.minimum,
        maximum: field.maximum,
        ..AxisRange::default()
    })
}

fn set(bits: &mut [u8], code: u16) {
    if let Some(byte) = bits.get_mut(usize::from(code / 8)) {
        *byte |= 1 << (code % 8);
    }
}

/// A device's name from its manufacturer's and product's strings, as Linux's
/// HID core joins them: the product alone if it already starts with the
/// manufacturer, either alone if the other is missing, and `fallback` if
/// both are; then the application's suffix, unless the name already ends
/// with it, as `hidinput_allocate` names an input per application.
#[must_use]
pub fn name(manufacturer: &[u8], product: &[u8], fallback: &[u8], kind: Kind) -> Text {
    let mut joined = Joined {
        bytes: [0; 128],
        len: 0,
    };
    match (manufacturer.is_empty(), product.is_empty()) {
        (true, true) => joined.add(fallback),
        (true, false) => joined.add(product),
        (false, true) => joined.add(manufacturer),
        (false, false) if product.starts_with(manufacturer) => joined.add(product),
        (false, false) => {
            joined.add(manufacturer);
            joined.add(b" ");
            joined.add(product);
        }
    }
    if let Some(suffix) = kind.suffix()
        && !joined.as_bytes().ends_with(suffix.as_bytes())
    {
        joined.add(b" ");
        joined.add(suffix.as_bytes());
    }
    Text::new(joined.as_bytes()).unwrap_or(Text::NONE)
}

/// A name being put together.
struct Joined {
    bytes: [u8; 128],
    len: usize,
}

impl Joined {
    fn add(&mut self, part: &[u8]) {
        for &byte in part {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}
