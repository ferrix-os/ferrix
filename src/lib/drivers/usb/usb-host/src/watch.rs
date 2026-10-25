//! Which functions' raw input reports the driver says, for finding out what
//! a device sent: the debug switch `ferrix.usbhid.reports`.
//!
//! Off unless asked. Asked, each interrupt report of a watched function is
//! handed on as it came off the pipe, with the time the driver took it, and
//! the driver says it at debug with the events it made (`docs/XHCI.md` §13,
//! "Watching a device's reports"). What is watched:
//!
//! | Value | Watched |
//! |---|---|
//! | (none: the word alone), `all`, `on` | every function |
//! | `off` | nothing |
//! | `1d6b:0104` | every function of the device with that `idVendor:idProduct` |
//! | `keyboard`, `mouse`, `media`, `system`, `input` | every function of that kind |
//! | `1d6b:0104/keyboard` | that device's functions of that kind |
//!
//! The boot word is read once, the first word with the key winning as the
//! kernel's options do. The driver also reads the file [`FILE`] at each port
//! poll: while it is there its contents say what is watched (empty: every
//! function), so a report can be watched on a running machine without a
//! boot; once it is gone the boot word's choice holds again.

use crate::hid::Kind;

/// The file that overrides the boot word while it exists.
pub const FILE: &[u8] = b"/run/usbhid-reports\0";

/// What is watched.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Watch {
    /// Whether anything is.
    on: bool,
    /// Only the device with this `idVendor` and `idProduct`.
    device: Option<(u16, u16)>,
    /// Only functions of this kind.
    kind: Option<Kind>,
}

impl Watch {
    /// Nothing watched.
    pub const OFF: Watch = Watch {
        on: false,
        device: None,
        kind: None,
    };

    /// Every function watched.
    pub const ALL: Watch = Watch {
        on: true,
        device: None,
        kind: None,
    };

    /// What the kernel command line `line` asks for: the first word `key`
    /// in it, alone or with a value; [`Watch::OFF`] without one, or for a
    /// value this does not read. The key is the driver's to name, as a boot
    /// word belongs to the program that reads it.
    #[must_use]
    pub fn from_command_line(line: &[u8], key: &[u8]) -> Watch {
        for word in line.split(u8::is_ascii_whitespace) {
            let Some(rest) = word.strip_prefix(key) else {
                continue;
            };
            return match rest.split_first() {
                None => Watch::ALL,
                Some((b'=', value)) => Watch::parse(value).unwrap_or(Watch::OFF),
                // A longer key that starts with this one.
                Some(_) => continue,
            };
        }
        Watch::OFF
    }

    /// A value, as the table above reads it, surrounding white space
    /// ignored; `None` for one it does not read.
    #[must_use]
    pub fn parse(value: &[u8]) -> Option<Watch> {
        let value = value.trim_ascii();
        match value {
            b"" | b"all" | b"on" => return Some(Watch::ALL),
            b"off" => return Some(Watch::OFF),
            _ => {}
        }
        let (device, kind) = match value.iter().position(|&byte| byte == b'/') {
            Some(at) => (value.get(..at)?, Some(value.get(at + 1..)?)),
            None if value.contains(&b':') => (value, None),
            None => (&b""[..], Some(value)),
        };
        let device = match device {
            b"" => None,
            device => {
                let at = device.iter().position(|&byte| byte == b':')?;
                Some((hex16(device.get(..at)?)?, hex16(device.get(at + 1..)?)?))
            }
        };
        let kind = match kind {
            None => None,
            Some(word) => Some(kind_named(word)?),
        };
        Some(Watch {
            on: true,
            device,
            kind,
        })
    }

    /// Whether anything is watched.
    #[must_use]
    pub const fn is_on(&self) -> bool {
        self.on
    }

    /// Whether a function of `kind` on the device `vendor:product` is.
    #[must_use]
    pub fn covers(&self, vendor: u16, product: u16, kind: Kind) -> bool {
        self.on
            && self.device.is_none_or(|device| device == (vendor, product))
            && self.kind.is_none_or(|wanted| wanted == kind)
    }
}

impl core::fmt::Display for Watch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if !self.on {
            return f.write_str("off");
        }
        match (self.device, self.kind) {
            (None, None) => f.write_str("all"),
            (Some((vendor, product)), None) => write!(f, "{vendor:04x}:{product:04x}"),
            (None, Some(kind)) => f.write_str(name_of(kind)),
            (Some((vendor, product)), Some(kind)) => {
                write!(f, "{vendor:04x}:{product:04x}/{}", name_of(kind))
            }
        }
    }
}

/// Four hex digits at most, as `lsusb` writes an ID.
fn hex16(digits: &[u8]) -> Option<u16> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    digits.iter().try_fold(0_u16, |value, &digit| {
        let nibble = char::from(digit).to_digit(16)?;
        Some((value << 4) | nibble as u16)
    })
}

const KINDS: [(&[u8], Kind); 5] = [
    (b"keyboard", Kind::Keyboard),
    (b"mouse", Kind::Mouse),
    (b"media", Kind::Consumer),
    (b"system", Kind::System),
    (b"input", Kind::Other),
];

fn kind_named(word: &[u8]) -> Option<Kind> {
    KINDS
        .iter()
        .find(|(name, _)| *name == word)
        .map(|&(_, kind)| kind)
}

fn name_of(kind: Kind) -> &'static str {
    KINDS
        .iter()
        .find(|(_, named)| *named == kind)
        .and_then(|(name, _)| core::str::from_utf8(name).ok())
        .unwrap_or("input")
}
