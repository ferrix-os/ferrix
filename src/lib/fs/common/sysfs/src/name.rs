//! The names sysfs directories and links are called by, and the name a write
//! to `bind` or `unbind` gives.

use alloc::vec::Vec;

use crate::text::put;

/// A PCI function's address: what `pci_name` prints and a `bind` write names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// The segment, or domain.
    pub segment: u16,
    /// The bus.
    pub bus: u8,
    /// The device, 0 to 31.
    pub device: u8,
    /// The function, 0 to 7.
    pub function: u8,
}

impl Slot {
    /// A slot from the location word START and HELLO carry: segment in bits
    /// 31:16, bus in 15:8, device and function in 7:0.
    #[must_use]
    pub const fn from_word(word: u32) -> Slot {
        Slot {
            segment: (word >> 16) as u16,
            bus: (word >> 8) as u8,
            device: ((word >> 3) & 0x1f) as u8,
            function: (word & 0x7) as u8,
        }
    }

    /// The name: `0000:00:02.0`.
    pub fn name(self, out: &mut Vec<u8>) {
        put(
            out,
            format_args!(
                "{:04x}:{:02x}:{:02x}.{:x}",
                self.segment, self.bus, self.device, self.function
            ),
        );
    }

    /// The slot a name names, if it is one as `pci_name` would print it:
    /// four, two and two lower-case hexadecimal digits and a function digit
    /// from 0 to 7. Nothing else reads as a slot, so a name that is almost
    /// one is no slot at all rather than a nearby one.
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Slot> {
        let &[s0, s1, s2, s3, b':', b0, b1, b':', d0, d1, b'.', f] = name else {
            return None;
        };
        let segment = hex(&[s0, s1, s2, s3])?;
        let bus = hex(&[b0, b1])?;
        let device = hex(&[d0, d1])?;
        let function = hex(&[f])?;
        if device > 0x1f || function > 7 {
            return None;
        }
        Some(Slot {
            segment: u16::try_from(segment).ok()?,
            bus: u8::try_from(bus).ok()?,
            device: u8::try_from(device).ok()?,
            function: u8::try_from(function).ok()?,
        })
    }
}

/// Lower-case hexadecimal digits, exactly.
fn hex(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0_u32, |value, digit| {
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            _ => return None,
        };
        Some((value << 4) | u32::from(nibble))
    })
}

/// A PCI root bus's directory under `/sys/devices`: `pci0000:00`.
pub fn pci_root(out: &mut Vec<u8>, segment: u16, bus: u8) {
    put(out, format_args!("pci{segment:04x}:{bus:02x}"));
}

/// A device tree node's platform device name, as Linux makes one from the
/// node: its unit address, then its node name, `5a001000.display-controller`.
pub fn platform(out: &mut Vec<u8>, address: u64, node: &str) {
    put(out, format_args!("{address:x}.{node}"));
}

/// A device number's name under `/sys/dev`: `226:0`.
pub fn dev_number(out: &mut Vec<u8>, major: u32, minor: u32) {
    put(out, format_args!("{major}:{minor}"));
}

/// The device number a `/sys/dev` name names: two decimal numbers with no
/// sign and no leading zero, `major:minor`.
#[must_use]
pub fn parse_dev_number(name: &[u8]) -> Option<(u32, u32)> {
    let colon = name.iter().position(|&byte| byte == b':')?;
    let major = decimal(name.get(..colon)?)?;
    let minor = decimal(name.get(colon + 1..)?)?;
    Some((major, minor))
}

/// A decimal number as a name spells one: digits only, no leading zero
/// except `0` itself.
fn decimal(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || (digits.len() > 1 && digits.first() == Some(&b'0')) {
        return None;
    }
    digits.iter().try_fold(0_u32, |value, digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
    })
}

/// The number in a numbered name, `card0` or `cpu12` under `prefix`, if
/// the rest is a number spelt as the kernel spells one: `card00` and
/// `card+1` are nobody.
#[must_use]
pub fn numbered(prefix: &[u8], name: &[u8]) -> Option<u32> {
    decimal(name.strip_prefix(prefix)?)
}

/// The name a write to `bind` or `unbind` gives: the bytes up to the first
/// NUL, less one trailing newline, as `sysfs_streq` compares them, so that
/// `echo 0000:00:02.0 > unbind` names the slot. `None` for a write that names
/// nothing.
#[must_use]
pub fn written(data: &[u8]) -> Option<&[u8]> {
    let end = data
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(data.len());
    let text = data.get(..end)?;
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    if text.is_empty() { None } else { Some(text) }
}
