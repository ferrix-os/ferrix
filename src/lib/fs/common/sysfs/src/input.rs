//! An input device's files: its capability bitmaps and its `uevent`.
//!
//! A bitmap is printed as `input_print_bitmap` prints it: the words of the
//! kernel's `unsigned long`, most significant first, each in `%lx`, separated
//! by spaces, with the leading zero words left out and a lone `0` for an
//! empty map. The word is the *kernel's* `long` -- 64 bits on the 64-bit
//! pair, 32 on ARMv7-A -- so the same keyboard prints differently on the two,
//! and a reader such as libevdev parses with its own `long`, which matches.
//!
//! How many words is `BITS_TO_LONGS(max)`, where `max` is the type's `*_MAX`
//! and not its count: Linux's own off-by-one, kept, because it decides
//! whether a trailing zero word is printed.

use alloc::vec::Vec;

use crate::text::put;
use crate::uevent;

/// `EV_MAX`: the event types.
pub const EV_MAX: u32 = 0x1f;
/// `KEY_MAX`.
pub const KEY_MAX: u32 = 0x2ff;
/// `REL_MAX`.
pub const REL_MAX: u32 = 0x0f;
/// `ABS_MAX`.
pub const ABS_MAX: u32 = 0x3f;
/// `MSC_MAX`.
pub const MSC_MAX: u32 = 0x07;
/// `LED_MAX`.
pub const LED_MAX: u32 = 0x0f;
/// `SND_MAX`.
pub const SND_MAX: u32 = 0x07;
/// `FF_MAX`.
pub const FF_MAX: u32 = 0x7f;
/// `SW_MAX`.
pub const SW_MAX: u32 = 0x10;
/// `INPUT_PROP_MAX`.
pub const PROP_MAX: u32 = 0x1f;

/// Word `index` of a little-endian bit array, `word_bits` wide; bits past
/// the array's end are zero.
fn word(bits: &[u8], index: u32, word_bits: u32) -> u64 {
    let bytes = word_bits / 8;
    let first = index.saturating_mul(bytes);
    (0..bytes).fold(0_u64, |value, offset| {
        let byte = usize::try_from(first.saturating_add(offset))
            .ok()
            .and_then(|at| bits.get(at))
            .copied()
            .unwrap_or(0);
        value | (u64::from(byte) << (8 * offset))
    })
}

/// A bitmap, as `input_print_bitmap`: `bits` little-endian, `max` the type's
/// `*_MAX`, `word_bits` 64 or 32. Without the newline, which the attribute
/// adds and the `uevent` line does too.
fn put_bitmap(out: &mut Vec<u8>, bits: &[u8], max: u32, word_bits: u32) {
    let words = max.div_ceil(word_bits);
    let start = out.len();
    for index in (0..words).rev() {
        let value = word(bits, index, word_bits);
        let printed = out.len() > start;
        if printed || value != 0 {
            put(out, format_args!("{value:x}"));
            if index > 0 {
                out.push(b' ');
            }
        }
    }
    if out.len() == start {
        out.push(b'0');
    }
}

/// A capability file: `capabilities/ev`, `capabilities/key`, `properties`.
pub fn bitmap(out: &mut Vec<u8>, bits: &[u8], max: u32, word_bits: u32) {
    put_bitmap(out, bits, max, word_bits);
    out.push(b'\n');
}

/// One bitmap of an input device, with the `*_MAX` it is printed to and the
/// `uevent` key it goes under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Map<'a> {
    /// `KEY`, `REL`, `ABS` and the rest.
    pub key: &'static str,
    /// The bits, little-endian.
    pub bits: &'a [u8],
    /// The type's `*_MAX`.
    pub max: u32,
}

/// What an input device's `uevent` holds, as `input_dev_uevent` builds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device<'a> {
    /// Bus, vendor, product and version, as `EVIOCGID` reports them.
    pub id: [u16; 4],
    /// The name the driver gave.
    pub name: &'a [u8],
    /// The unique identifier, a serial number; left out when empty, as
    /// Linux leaves out a null one.
    pub uniq: &'a [u8],
    /// `INPUT_PROP_*`.
    pub properties: &'a [u8],
    /// `EV_*`: which of `maps` are printed.
    pub types: &'a [u8],
    /// The maps of every type the device has, in `input_dev_uevent`'s order:
    /// `KEY`, `REL`, `ABS`, `MSC`, `LED`, `SND`, `FF`, `SW`. A type whose bit
    /// is not in `types` is skipped.
    pub maps: &'a [(u32, Map<'a>)],
}

/// Whether bit `bit` of a little-endian bit array is set.
fn has(bits: &[u8], bit: u32) -> bool {
    usize::try_from(bit / 8)
        .ok()
        .and_then(|at| bits.get(at))
        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
}

/// The `uevent` file of `input<N>`.
pub fn uevent(out: &mut Vec<u8>, device: &Device<'_>, word_bits: u32) {
    let [bus, vendor, product, version] = device.id;
    put(
        out,
        format_args!("PRODUCT={bus:x}/{vendor:x}/{product:x}/{version:x}\n"),
    );
    out.extend_from_slice(b"NAME=\"");
    out.extend_from_slice(device.name);
    out.extend_from_slice(b"\"\n");
    if !device.uniq.is_empty() {
        out.extend_from_slice(b"UNIQ=\"");
        out.extend_from_slice(device.uniq);
        out.extend_from_slice(b"\"\n");
    }
    out.extend_from_slice(b"PROP=");
    put_bitmap(out, device.properties, PROP_MAX, word_bits);
    out.push(b'\n');
    out.extend_from_slice(b"EV=");
    put_bitmap(out, device.types, EV_MAX, word_bits);
    out.push(b'\n');
    for (kind, map) in device.maps {
        if has(device.types, *kind) {
            out.extend_from_slice(map.key.as_bytes());
            out.push(b'=');
            put_bitmap(out, map.bits, map.max, word_bits);
            out.push(b'\n');
        }
    }
}

/// The `uevent` of `event<N>`: its node and nothing else.
pub fn event_uevent(out: &mut Vec<u8>, major: u32, minor: u32, name: &[u8]) {
    uevent::node(
        out,
        &uevent::Node {
            major,
            minor,
            name,
            mode: None,
            kind: None,
        },
    );
}

/// One of `id/bustype`, `id/vendor`, `id/product`, `id/version`: `%04x`.
pub fn id(out: &mut Vec<u8>, value: u16) {
    put(out, format_args!("{value:04x}\n"));
}
