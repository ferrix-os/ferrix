//! evdev: the ioctls, event codes and structures a compositor uses on
//! `/dev/input/eventN`.
//!
//! This is the subset `docs/INPUT.md` §3.3 answers in the input iteration:
//! identifying a device (`EVIOCGVERSION`, `EVIOCGID`, the name, properties
//! and event bitmaps, each axis's `input_absinfo`), reading its state after
//! `SYN_DROPPED`, grabbing it, choosing the timestamps' clock, and the
//! `input_event` stream itself. The codes are the ones the input test and
//! QEMU's virtio keyboard and tablet use, and each type's `*_MAX` and `*_CNT`,
//! not the whole of `input-event-codes.h`.
//!
//! # Where the numbers come from
//!
//! `include/uapi/linux/input.h`, `input-event-codes.h` and `major.h`, which
//! every architecture takes unchanged, and `asm-generic/ioctl.h` for how a
//! request number is put together, which x86-64, AArch64 and ARMv7-A share.
//! `probe/input.c` prints every number and layout below from those headers,
//! natively for 64-bit and under `qemu-arm` for ARMv7-A, into
//! `probe/input-64.txt` and `probe/input-32.txt`; the tests read both files
//! and require this module to agree with every line.
//!
//! # Requests that carry an argument
//!
//! `EVIOCGNAME(len)`, `EVIOCGBIT(ev, len)` and their kind put a length into
//! the request's size field, and `EVIOCGABS(abs)` puts an axis into its
//! number, so no single constant names them. They are functions here, checked
//! against the probe at sample arguments, and [`ioc_dir`], [`ioc_type`],
//! [`ioc_nr`] and [`ioc_size`] take a request apart the way the kernel's
//! `_IOC_*` macros do, for a core that has to recognise them by their parts.
//!
//! # One structure and one request have a width
//!
//! `struct input_event` starts with two `__kernel_ulong_t`s, seconds and
//! microseconds, so it is 24 bytes on 64-bit and 16 on ARMv7-A: [`Event`]
//! takes a [`Width`]. `EVIOCSFF` encodes the size of `struct ff_effect`, which
//! holds a pointer, so its number differs too: [`eviocsff`].
//!
//! Which fields a *program* sees there depends on its libc. `linux/input.h`
//! gives user space a `struct timeval` unless the libc defines
//! `__USE_TIME_BITS64`, and on a 32-bit libc whose `time_t` is 64 bits a
//! `struct timeval` is 16 bytes, which would make the program's
//! `input_event` 24 bytes against the kernel's 16. The probe prints that view
//! as well (`view.time64-undef.*` in `input-32.txt`). glibc defines the macro
//! whenever it builds with `_TIME_BITS=64`, and so does ferrousli, whose
//! `include/bits/alltypes.h` defines it as musl's does, so its programs see the
//! kernel's 16 bytes; that is read from the header, not yet shown by a
//! program built against ferrousli for ARMv7-A. x86-64 and AArch64 have one
//! view.
//!
//! # Reading and writing
//!
//! Every structure is read from and written into bytes, little-endian,
//! returning `None` rather than panicking when the buffer is short. What the
//! fields mean is the input core's business; nothing here validates a value.

use crate::layout::{Field, layout};
use crate::socket::Width;

// ---------------------------------------------------------------------------
// The protocol and the device node
// ---------------------------------------------------------------------------

/// `EV_VERSION`: what `EVIOCGVERSION` answers.
pub const EV_VERSION: i32 = 0x01_0001;
/// `INPUT_MAJOR`: the character device major of `/dev/input/*`.
pub const INPUT_MAJOR: u32 = 13;

// ---------------------------------------------------------------------------
// How a request number is put together
// ---------------------------------------------------------------------------

/// `_IOC_NRBITS`.
pub const IOC_NRBITS: u32 = 8;
/// `_IOC_TYPEBITS`.
pub const IOC_TYPEBITS: u32 = 8;
/// `_IOC_SIZEBITS`: a length of `1 << 14` or more does not fit.
pub const IOC_SIZEBITS: u32 = 14;
/// `_IOC_DIRBITS`.
pub const IOC_DIRBITS: u32 = 2;
/// `_IOC_NRSHIFT`.
pub const IOC_NRSHIFT: u32 = 0;
/// `_IOC_TYPESHIFT`.
pub const IOC_TYPESHIFT: u32 = 8;
/// `_IOC_SIZESHIFT`.
pub const IOC_SIZESHIFT: u32 = 16;
/// `_IOC_DIRSHIFT`.
pub const IOC_DIRSHIFT: u32 = 30;
/// `_IOC_NONE`.
pub const IOC_NONE: u32 = 0;
/// `_IOC_WRITE`: user space writes, the kernel reads.
pub const IOC_WRITE: u32 = 1;
/// `_IOC_READ`: the kernel writes, user space reads.
pub const IOC_READ: u32 = 2;

/// The type byte of every evdev request, `'E'`.
pub const IOC_TYPE_EVDEV: u32 = b'E' as u32;

/// `_IOC(dir, 'E', nr, size)`. As in C, a `size` too wide for its field
/// spills into the direction bits rather than being refused.
#[must_use]
pub const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    (dir << IOC_DIRSHIFT)
        | (IOC_TYPE_EVDEV << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)
}

/// `_IOC_DIR(request)`.
#[must_use]
pub const fn ioc_dir(request: u32) -> u32 {
    (request >> IOC_DIRSHIFT) & ((1 << IOC_DIRBITS) - 1)
}

/// `_IOC_TYPE(request)`.
#[must_use]
pub const fn ioc_type(request: u32) -> u32 {
    (request >> IOC_TYPESHIFT) & ((1 << IOC_TYPEBITS) - 1)
}

/// `_IOC_NR(request)`.
#[must_use]
pub const fn ioc_nr(request: u32) -> u32 {
    (request >> IOC_NRSHIFT) & ((1 << IOC_NRBITS) - 1)
}

/// `_IOC_SIZE(request)`.
#[must_use]
pub const fn ioc_size(request: u32) -> u32 {
    (request >> IOC_SIZESHIFT) & ((1 << IOC_SIZEBITS) - 1)
}

// ---------------------------------------------------------------------------
// ioctls with fixed numbers
// ---------------------------------------------------------------------------

/// `EVIOCGVERSION`: `_IOR('E', 0x01, int)`.
pub const EVIOCGVERSION: u32 = 0x8004_4501;
/// `EVIOCGID`: `_IOR('E', 0x02, struct input_id)`.
pub const EVIOCGID: u32 = 0x8008_4502;
/// `EVIOCGREP`: `_IOR('E', 0x03, unsigned int[2])`, delay and period.
pub const EVIOCGREP: u32 = 0x8008_4503;
/// `EVIOCSREP`: `_IOW('E', 0x03, unsigned int[2])`.
pub const EVIOCSREP: u32 = 0x4008_4503;
/// `EVIOCGKEYCODE`: `_IOR('E', 0x04, unsigned int[2])`.
pub const EVIOCGKEYCODE: u32 = 0x8008_4504;
/// `EVIOCGKEYCODE_V2`: `_IOR('E', 0x04, struct input_keymap_entry)`.
pub const EVIOCGKEYCODE_V2: u32 = 0x8028_4504;
/// `EVIOCSKEYCODE`: `_IOW('E', 0x04, unsigned int[2])`.
pub const EVIOCSKEYCODE: u32 = 0x4008_4504;
/// `EVIOCSKEYCODE_V2`: `_IOW('E', 0x04, struct input_keymap_entry)`.
pub const EVIOCSKEYCODE_V2: u32 = 0x4028_4504;
/// `EVIOCSFF` on 64-bit: `_IOW('E', 0x80, struct ff_effect)`, 48 bytes.
pub const EVIOCSFF_64: u32 = 0x4030_4580;
/// `EVIOCSFF` on ARMv7-A, where `struct ff_effect` is 44 bytes.
pub const EVIOCSFF_32: u32 = 0x402C_4580;
/// `EVIOCRMFF`: `_IOW('E', 0x81, int)`.
pub const EVIOCRMFF: u32 = 0x4004_4581;
/// `EVIOCGEFFECTS`: `_IOR('E', 0x84, int)`.
pub const EVIOCGEFFECTS: u32 = 0x8004_4584;
/// `EVIOCGRAB`: `_IOW('E', 0x90, int)`; 1 grabs, 0 releases.
pub const EVIOCGRAB: u32 = 0x4004_4590;
/// `EVIOCREVOKE`: `_IOW('E', 0x91, int)`.
pub const EVIOCREVOKE: u32 = 0x4004_4591;
/// `EVIOCGMASK`: `_IOR('E', 0x92, struct input_mask)`.
pub const EVIOCGMASK: u32 = 0x8010_4592;
/// `EVIOCSMASK`: `_IOW('E', 0x93, struct input_mask)`.
pub const EVIOCSMASK: u32 = 0x4010_4593;
/// `EVIOCSCLOCKID`: `_IOW('E', 0xa0, int)`, a `CLOCK_*` id.
pub const EVIOCSCLOCKID: u32 = 0x4004_45A0;

/// `EVIOCSFF` at `width`.
#[must_use]
pub const fn eviocsff(width: Width) -> u32 {
    match width {
        Width::Bits32 => EVIOCSFF_32,
        Width::Bits64 => EVIOCSFF_64,
    }
}

// ---------------------------------------------------------------------------
// ioctls with an argument in the number
// ---------------------------------------------------------------------------

/// `EVIOCGNAME(len)`: the device's name into a buffer of `len` bytes.
#[must_use]
pub const fn eviocgname(len: u32) -> u32 {
    ioc(IOC_READ, 0x06, len)
}

/// `EVIOCGPHYS(len)`: the physical path.
#[must_use]
pub const fn eviocgphys(len: u32) -> u32 {
    ioc(IOC_READ, 0x07, len)
}

/// `EVIOCGUNIQ(len)`: the unique identifier.
#[must_use]
pub const fn eviocguniq(len: u32) -> u32 {
    ioc(IOC_READ, 0x08, len)
}

/// `EVIOCGPROP(len)`: the `INPUT_PROP_*` bitmap.
#[must_use]
pub const fn eviocgprop(len: u32) -> u32 {
    ioc(IOC_READ, 0x09, len)
}

/// `EVIOCGMTSLOTS(len)`: multi-touch slot values.
#[must_use]
pub const fn eviocgmtslots(len: u32) -> u32 {
    ioc(IOC_READ, 0x0a, len)
}

/// `EVIOCGKEY(len)`: the bitmap of keys held down.
#[must_use]
pub const fn eviocgkey(len: u32) -> u32 {
    ioc(IOC_READ, 0x18, len)
}

/// `EVIOCGLED(len)`: the bitmap of LEDs lit.
#[must_use]
pub const fn eviocgled(len: u32) -> u32 {
    ioc(IOC_READ, 0x19, len)
}

/// `EVIOCGSND(len)`: the bitmap of sounds playing.
#[must_use]
pub const fn eviocgsnd(len: u32) -> u32 {
    ioc(IOC_READ, 0x1a, len)
}

/// `EVIOCGSW(len)`: the bitmap of switches on.
#[must_use]
pub const fn eviocgsw(len: u32) -> u32 {
    ioc(IOC_READ, 0x1b, len)
}

/// `EVIOCGBIT(ev, len)`: with `ev` 0 the bitmap of event types, otherwise the
/// bitmap of that type's codes. `ev` past [`EV_MAX`] is not a request.
#[must_use]
pub const fn eviocgbit(ev: u16, len: u32) -> u32 {
    ioc(IOC_READ, 0x20 + ev as u32, len)
}

/// `EVIOCGABS(abs)`: the axis's [`AbsInfo`].
#[must_use]
pub const fn eviocgabs(abs: u16) -> u32 {
    ioc(IOC_READ, 0x40 + abs as u32, AbsInfo::SIZE as u32)
}

/// `EVIOCSABS(abs)`: set the axis's [`AbsInfo`].
#[must_use]
pub const fn eviocsabs(abs: u16) -> u32 {
    ioc(IOC_WRITE, 0xc0 + abs as u32, AbsInfo::SIZE as u32)
}

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// `EV_SYN`: report boundaries and drops.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY`: keys and buttons.
pub const EV_KEY: u16 = 0x01;
/// `EV_REL`: relative axes.
pub const EV_REL: u16 = 0x02;
/// `EV_ABS`: absolute axes.
pub const EV_ABS: u16 = 0x03;
/// `EV_MSC`: miscellaneous, such as scan codes.
pub const EV_MSC: u16 = 0x04;
/// `EV_SW`: switches.
pub const EV_SW: u16 = 0x05;
/// `EV_LED`: LEDs.
pub const EV_LED: u16 = 0x11;
/// `EV_SND`: sounds.
pub const EV_SND: u16 = 0x12;
/// `EV_REP`: autorepeat settings.
pub const EV_REP: u16 = 0x14;
/// `EV_FF`: force feedback.
pub const EV_FF: u16 = 0x15;
/// `EV_PWR`: power.
pub const EV_PWR: u16 = 0x16;
/// `EV_FF_STATUS`: force feedback status.
pub const EV_FF_STATUS: u16 = 0x17;
/// `EV_MAX`.
pub const EV_MAX: u16 = 0x1f;
/// `EV_CNT`.
pub const EV_CNT: u16 = EV_MAX + 1;

/// `SYN_REPORT`: the end of a report.
pub const SYN_REPORT: u16 = 0;
/// `SYN_CONFIG`.
pub const SYN_CONFIG: u16 = 1;
/// `SYN_MT_REPORT`.
pub const SYN_MT_REPORT: u16 = 2;
/// `SYN_DROPPED`: events were lost; re-read the state.
pub const SYN_DROPPED: u16 = 3;
/// `SYN_MAX`.
pub const SYN_MAX: u16 = 0xf;
/// `SYN_CNT`.
pub const SYN_CNT: u16 = SYN_MAX + 1;

/// `INPUT_PROP_POINTER`: needs a pointer on screen.
pub const INPUT_PROP_POINTER: u16 = 0x00;
/// `INPUT_PROP_DIRECT`: positions are on the screen itself.
pub const INPUT_PROP_DIRECT: u16 = 0x01;
/// `INPUT_PROP_MAX`.
pub const INPUT_PROP_MAX: u16 = 0x1f;
/// `INPUT_PROP_CNT`.
pub const INPUT_PROP_CNT: u16 = INPUT_PROP_MAX + 1;

// ---------------------------------------------------------------------------
// Codes
// ---------------------------------------------------------------------------

/// `KEY_RESERVED`.
pub const KEY_RESERVED: u16 = 0;
/// `KEY_ESC`.
pub const KEY_ESC: u16 = 1;
/// `KEY_A`: the key the input test presses.
pub const KEY_A: u16 = 30;
/// `BTN_MISC`: the first button code.
pub const BTN_MISC: u16 = 0x100;
/// `BTN_MOUSE`: the first mouse button.
pub const BTN_MOUSE: u16 = 0x110;
/// `BTN_LEFT`: the button the input test clicks.
pub const BTN_LEFT: u16 = 0x110;
/// `BTN_RIGHT`.
pub const BTN_RIGHT: u16 = 0x111;
/// `BTN_MIDDLE`.
pub const BTN_MIDDLE: u16 = 0x112;
/// `BTN_SIDE`.
pub const BTN_SIDE: u16 = 0x113;
/// `BTN_EXTRA`.
pub const BTN_EXTRA: u16 = 0x114;
/// `BTN_TOUCH`.
pub const BTN_TOUCH: u16 = 0x14a;
/// `BTN_GEAR_DOWN`: QEMU's tablet sends it for the wheel turned down.
pub const BTN_GEAR_DOWN: u16 = 0x150;
/// `BTN_GEAR_UP`: and for the wheel turned up.
pub const BTN_GEAR_UP: u16 = 0x151;
/// `KEY_MAX`.
pub const KEY_MAX: u16 = 0x2ff;
/// `KEY_CNT`.
pub const KEY_CNT: u16 = KEY_MAX + 1;

/// `REL_X`.
pub const REL_X: u16 = 0x00;
/// `REL_Y`.
pub const REL_Y: u16 = 0x01;
/// `REL_HWHEEL`.
pub const REL_HWHEEL: u16 = 0x06;
/// `REL_WHEEL`.
pub const REL_WHEEL: u16 = 0x08;
/// `REL_MAX`.
pub const REL_MAX: u16 = 0x0f;
/// `REL_CNT`.
pub const REL_CNT: u16 = REL_MAX + 1;

/// `ABS_X`: the tablet's horizontal position.
pub const ABS_X: u16 = 0x00;
/// `ABS_Y`: its vertical position.
pub const ABS_Y: u16 = 0x01;
/// `ABS_WHEEL`: an absolute wheel's position, which Linux's HID driver makes
/// of a Generic Desktop wheel that is not relative.
pub const ABS_WHEEL: u16 = 0x08;
/// `ABS_MT_SLOT`: the first multi-touch axis, which the input iteration
/// leaves out with every axis after it.
pub const ABS_MT_SLOT: u16 = 0x2f;
/// `ABS_MT_POSITION_X`.
pub const ABS_MT_POSITION_X: u16 = 0x35;
/// `ABS_MT_POSITION_Y`.
pub const ABS_MT_POSITION_Y: u16 = 0x36;
/// `ABS_MT_TRACKING_ID`.
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
/// `ABS_MAX`.
pub const ABS_MAX: u16 = 0x3f;
/// `ABS_CNT`.
pub const ABS_CNT: u16 = ABS_MAX + 1;

/// `MSC_SCAN`.
pub const MSC_SCAN: u16 = 0x04;
/// `MSC_MAX`.
pub const MSC_MAX: u16 = 0x07;
/// `MSC_CNT`.
pub const MSC_CNT: u16 = MSC_MAX + 1;

/// `SW_MAX`.
pub const SW_MAX: u16 = 0x11;
/// `SW_CNT`.
pub const SW_CNT: u16 = SW_MAX + 1;

/// `LED_NUML`.
pub const LED_NUML: u16 = 0x00;
/// `LED_CAPSL`.
pub const LED_CAPSL: u16 = 0x01;
/// `LED_SCROLLL`.
pub const LED_SCROLLL: u16 = 0x02;
/// `LED_MAX`.
pub const LED_MAX: u16 = 0x0f;
/// `LED_CNT`.
pub const LED_CNT: u16 = LED_MAX + 1;

/// `SND_MAX`.
pub const SND_MAX: u16 = 0x07;
/// `SND_CNT`.
pub const SND_CNT: u16 = SND_MAX + 1;

/// `REP_DELAY`: index of the delay in `EVIOCGREP`'s pair, and its code.
pub const REP_DELAY: u16 = 0x00;
/// `REP_PERIOD`.
pub const REP_PERIOD: u16 = 0x01;
/// `REP_MAX`.
pub const REP_MAX: u16 = 0x01;
/// `REP_CNT`.
pub const REP_CNT: u16 = REP_MAX + 1;

/// `FF_MAX`.
pub const FF_MAX: u16 = 0x7f;
/// `FF_CNT`.
pub const FF_CNT: u16 = FF_MAX + 1;

// ---------------------------------------------------------------------------
// Device ids and clocks
// ---------------------------------------------------------------------------

/// `ID_BUS`: the index of the bus type in the legacy id array.
pub const ID_BUS: usize = 0;
/// `ID_VENDOR`.
pub const ID_VENDOR: usize = 1;
/// `ID_PRODUCT`.
pub const ID_PRODUCT: usize = 2;
/// `ID_VERSION`.
pub const ID_VERSION: usize = 3;
/// `BUS_USB`: what a USB keyboard or mouse reports.
pub const BUS_USB: u16 = 0x03;
/// `BUS_VIRTUAL`: what QEMU's virtio-input devices report.
pub const BUS_VIRTUAL: u16 = 0x06;

/// `CLOCK_REALTIME`: evdev's default timestamp clock.
pub const CLOCK_REALTIME: i32 = 0;
/// `CLOCK_MONOTONIC`.
pub const CLOCK_MONOTONIC: i32 = 1;
/// `CLOCK_BOOTTIME`.
pub const CLOCK_BOOTTIME: i32 = 7;

// ---------------------------------------------------------------------------
// Layouts
// ---------------------------------------------------------------------------

layout! {
    /// `struct input_id`: `EVIOCGID`'s answer.
    InputId = "input_id", 8 {
        /// `BUS_*`.
        bustype: u16 = 0 / "bustype",
        /// Vendor.
        vendor: u16 = 2 / "vendor",
        /// Product.
        product: u16 = 4 / "product",
        /// Version.
        version: u16 = 6 / "version",
    }
}

layout! {
    /// `struct input_absinfo`: `EVIOCGABS`'s answer for one axis.
    AbsInfo = "input_absinfo", 24 {
        /// The axis's current value.
        value: i32 = 0 / "value",
        /// Smallest value.
        minimum: i32 = 4 / "minimum",
        /// Largest value.
        maximum: i32 = 8 / "maximum",
        /// Noise filtered out.
        fuzz: i32 = 12 / "fuzz",
        /// Dead zone around the centre.
        flat: i32 = 16 / "flat",
        /// Units per millimetre, or per radian for rotation.
        resolution: i32 = 20 / "resolution",
    }
}

/// `struct input_event` as the kernel's `read` returns it: seconds and
/// microseconds as `__kernel_ulong_t`, then type, code and value. The module
/// comment says why a program's view of it can differ on a 32-bit libc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Seconds of the timestamp.
    pub sec: u64,
    /// Microseconds of the timestamp.
    pub usec: u64,
    /// `EV_*`.
    pub r#type: u16,
    /// The type's code, such as [`KEY_A`] or [`SYN_REPORT`].
    pub code: u16,
    /// 1 pressed, 0 released, an axis's position, a relative motion.
    pub value: i32,
}

impl Event {
    /// The C name.
    pub const C_NAME: &'static str = "input_event";

    /// `sizeof(struct input_event)` at `width`.
    #[must_use]
    pub const fn size(width: Width) -> usize {
        2 * width.bytes() + 8
    }

    /// Every field's name and `offsetof` at `width`, in order. The two time
    /// fields are named `sec` and `usec` whichever view the headers take.
    #[must_use]
    pub const fn fields(width: Width) -> [(&'static str, usize); 5] {
        let word = width.bytes();
        [
            ("sec", 0),
            ("usec", word),
            ("type", 2 * word),
            ("code", 2 * word + 2),
            ("value", 2 * word + 4),
        ]
    }

    /// Read it from the start of `bytes`.
    #[must_use]
    pub fn read(width: Width, bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::size(width) {
            return None;
        }
        let [(_, sec), (_, usec), (_, r#type), (_, code), (_, value)] = Self::fields(width);
        Some(Self {
            sec: width.word(bytes, sec)?,
            usec: width.word(bytes, usec)?,
            r#type: u16::get(bytes, r#type)?,
            code: u16::get(bytes, code)?,
            value: i32::get(bytes, value)?,
        })
    }

    /// Write it into the start of `out`. Seconds or microseconds too wide
    /// for a 32-bit word are refused rather than cut, and nothing is written.
    pub fn write(&self, width: Width, out: &mut [u8]) -> Option<()> {
        if out.len() < Self::size(width) {
            return None;
        }
        if matches!(width, Width::Bits32)
            && (u32::try_from(self.sec).is_err() || u32::try_from(self.usec).is_err())
        {
            return None;
        }
        let [(_, sec), (_, usec), (_, r#type), (_, code), (_, value)] = Self::fields(width);
        width.put_word(out, sec, self.sec)?;
        width.put_word(out, usec, self.usec)?;
        self.r#type.put(out, r#type)?;
        self.code.put(out, code)?;
        self.value.put(out, value)
    }
}
