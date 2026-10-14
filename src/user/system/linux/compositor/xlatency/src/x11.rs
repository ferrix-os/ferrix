//! The little of X11 the probe speaks, on the wire, little-endian: the
//! connection setup, `QueryExtension`, a window, pixmaps and a GC to fill
//! them, `XInput2`'s `XIQueryVersion` and `XISelectEvents`, and Present's
//! `QueryVersion`, `SelectInput` and `PresentPixmap`. Xlib is not linked, so
//! the probe is one static program that runs on Ferrix and on Linux alike,
//! as `xmoveresize` is.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Core requests' major opcodes.
const CREATE_WINDOW: u8 = 1;
const MAP_WINDOW: u8 = 8;
const CHANGE_PROPERTY: u8 = 18;
const CREATE_PIXMAP: u8 = 53;
const CREATE_GC: u8 = 55;
const CHANGE_GC: u8 = 56;
const POLY_FILL_RECTANGLE: u8 = 70;
const QUERY_EXTENSION: u8 = 98;

/// `XInput2`'s minor opcodes, its raw motion event and its mask for every
/// master device.
const XI_SELECT_EVENTS: u8 = 46;
const XI_QUERY_VERSION: u8 = 47;
const XI_RAW_MOTION: u16 = 17;
const XI_ALL_MASTER_DEVICES: u16 = 1;

/// Present's minor opcodes, its completion event and that event's mask.
const PRESENT_QUERY_VERSION: u8 = 0;
const PRESENT_PIXMAP: u8 = 1;
const PRESENT_SELECT_INPUT: u8 = 3;
const PRESENT_COMPLETE_NOTIFY: u16 = 1;
const PRESENT_COMPLETE_MASK: u32 = 2;

/// Core events by their code.
const GENERIC_EVENT: u8 = 35;
const MAP_NOTIFY: u8 = 19;

/// Four bytes at `at`, little-endian.
pub(crate) fn word(bytes: &[u8], at: usize) -> Option<u32> {
    let four: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(four))
}

/// Two bytes at `at`, little-endian.
pub(crate) fn half(bytes: &[u8], at: usize) -> Option<u16> {
    let two: [u8; 2] = bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(two))
}

/// Eight bytes at `at`, little-endian.
pub(crate) fn quad(bytes: &[u8], at: usize) -> Option<u64> {
    let eight: [u8; 8] = bytes.get(at..at.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(eight))
}

/// `n` rounded up to four.
const fn padded(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// A request: its opcode, the byte after it, then `body`, padded, with the
/// length in words between.
pub(crate) fn request(opcode: u8, data: u8, body: &[u8]) -> Vec<u8> {
    let length = u16::try_from((4 + padded(body.len())) / 4).unwrap_or(0);
    let mut bytes = vec![opcode, data];
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(body);
    bytes.resize(usize::from(length) * 4, 0);
    bytes
}

/// Words, little-endian, one after the other.
fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// `PresentPixmap` of `pixmap` to `window` with `serial`, at `target_msc`
/// (0: the next), copying or flipping as the server chooses: 72 bytes.
pub(crate) fn present_pixmap(
    present: u8,
    window: u32,
    pixmap: u32,
    serial: u32,
    target_msc: u64,
) -> Vec<u8> {
    // valid and update regions, offsets, crtc, the two fences, the options
    // and the pad are all zero: the whole pixmap, any crtc, no fence.
    let mut body = words(&[window, pixmap, serial, 0, 0, 0, 0, 0, 0, 0, 0]);
    for value in [target_msc, 0, 0] {
        body.extend_from_slice(&value.to_le_bytes());
    }
    request(present, PRESENT_PIXMAP, &body)
}

/// What the screen is, from the setup reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Screen {
    pub(crate) id_base: u32,
    pub(crate) root: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
}

/// The first screen, from the server's whole setup reply (X11 protocol,
/// "Connection Setup"): the resource id base at 12, the vendor's length at
/// 24 and the number of pixmap formats at 29; the vendor starts at 40, the
/// formats, eight bytes each, after it, then the screens.
pub(crate) fn screen_of(setup: &[u8]) -> Option<Screen> {
    let vendor = usize::from(half(setup, 24)?);
    let formats = usize::from(*setup.get(29)?);
    let at = 40 + padded(vendor) + 8 * formats;
    Some(Screen {
        id_base: word(setup, 12)?,
        root: word(setup, at)?,
        width: half(setup, at + 20)?,
        height: half(setup, at + 22)?,
        depth: *setup.get(at + 38)?,
    })
}

/// One thing the server sent, as far as the probe cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Packet {
    /// A reply, whole.
    Reply(Vec<u8>),
    /// An error: its code and the request's major and minor opcodes.
    Error(u8, u8, u16),
    /// `XInput2` raw motion, and its X time in milliseconds: the server's
    /// time for the input, which yserver takes from the device's stamp.
    RawMotion { time: u32 },
    /// Present's completion of the pixmap with `serial`.
    Complete {
        serial: u32,
        mode: u8,
        ust: u64,
        msc: u64,
    },
    /// The window was mapped.
    Mapped,
    /// Anything else.
    Other,
}

/// The extensions' major opcodes the packets are told apart by.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Opcodes {
    pub(crate) xi: u8,
    pub(crate) present: u8,
}

/// The packet `bytes` holds, whole.
pub(crate) fn packet(bytes: &[u8], opcodes: Opcodes) -> Packet {
    match bytes.first().copied() {
        Some(0) => Packet::Error(
            bytes.get(1).copied().unwrap_or(0),
            bytes.get(10).copied().unwrap_or(0),
            half(bytes, 8).unwrap_or(0),
        ),
        Some(1) => Packet::Reply(bytes.to_vec()),
        Some(code) if code & 0x7f == GENERIC_EVENT => {
            let extension = bytes.get(1).copied();
            let kind = half(bytes, 8);
            if extension == Some(opcodes.xi) && kind == Some(XI_RAW_MOTION) {
                return Packet::RawMotion {
                    time: word(bytes, 12).unwrap_or(0),
                };
            }
            if extension == Some(opcodes.present) && kind == Some(PRESENT_COMPLETE_NOTIFY) {
                return Packet::Complete {
                    serial: word(bytes, 20).unwrap_or(0),
                    mode: bytes.get(11).copied().unwrap_or(0),
                    ust: quad(bytes, 24).unwrap_or(0),
                    msc: quad(bytes, 32).unwrap_or(0),
                };
            }
            Packet::Other
        }
        Some(code) if code & 0x7f == MAP_NOTIFY => Packet::Mapped,
        _ => Packet::Other,
    }
}

/// How long the packet at the start of `bytes` is, once 32 bytes are there:
/// a reply's and a generic event's length words count past the 32.
fn length_of(bytes: &[u8]) -> Option<usize> {
    let head = bytes.get(..32)?;
    match head.first().copied() {
        Some(1) => Some(32 + 4 * usize::try_from(word(head, 4)?).ok()?),
        Some(code) if code & 0x7f == GENERIC_EVENT => {
            Some(32 + 4 * usize::try_from(word(head, 4)?).ok()?)
        }
        _ => Some(32),
    }
}

/// The connection to the server.
#[derive(Debug)]
pub(crate) struct Display {
    stream: UnixStream,
    pending: Vec<u8>,
    next_id: u32,
    pub(crate) screen: Screen,
    pub(crate) opcodes: Opcodes,
}

/// The socket a local `$DISPLAY` names: `:0` and `:0.0` are
/// `/tmp/.X11-unix/X0`.
pub(crate) fn socket_of(display: &str) -> Option<String> {
    let (host, rest) = display.split_once(':')?;
    if !host.is_empty() && host != "unix" {
        return None;
    }
    let number = rest.split('.').next()?;
    let _: u32 = number.parse().ok()?;
    Some(format!("/tmp/.X11-unix/X{number}"))
}

impl Display {
    /// Connect to `socket` and set up, little-endian, with no
    /// authorization; then find `XInput2` and Present.
    pub(crate) fn open(socket: &str) -> Result<Self, String> {
        let mut stream = UnixStream::connect(socket)
            .map_err(|error| format!("connecting to {socket}: {error}"))?;
        let hello = [b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        stream
            .write_all(&hello)
            .map_err(|error| format!("setting up: {error}"))?;
        let mut head = [0u8; 8];
        stream
            .read_exact(&mut head)
            .map_err(|error| format!("reading the setup: {error}"))?;
        let mut setup = head.to_vec();
        setup.resize(8 + usize::from(half(&head, 6).unwrap_or(0)) * 4, 0);
        stream
            .read_exact(setup.get_mut(8..).unwrap_or_default())
            .map_err(|error| format!("reading the setup: {error}"))?;
        if head.first() != Some(&1) {
            return Err("the server refused the connection".to_owned());
        }
        let screen = screen_of(&setup).ok_or("the setup reply names no screen")?;
        let mut display = Self {
            stream,
            pending: Vec::new(),
            next_id: 0,
            screen,
            opcodes: Opcodes::default(),
        };
        display.opcodes = Opcodes {
            xi: display.extension("XInputExtension")?,
            present: display.extension("Present")?,
        };
        // `XInput2` must be asked for by version before a selection.
        let xi = display.opcodes.xi;
        let _ = display.round_trip(&request(xi, XI_QUERY_VERSION, &[2, 0, 2, 0]))?;
        let present = display.opcodes.present;
        let _ = display.round_trip(&request(present, PRESENT_QUERY_VERSION, &words(&[1, 2])))?;
        Ok(display)
    }

    /// A new resource id.
    pub(crate) fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.screen.id_base | self.next_id
    }

    /// Send `bytes`.
    pub(crate) fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .map_err(|error| format!("writing a request: {error}"))
    }

    /// Send a request and wait for its reply; events before it are dropped,
    /// which is only done before anything is selected.
    fn round_trip(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.send(bytes)?;
        loop {
            match self.next(None)? {
                Some(Packet::Reply(reply)) => return Ok(reply),
                Some(Packet::Error(code, major, minor)) => {
                    return Err(format!("error {code} for request {major}.{minor}"));
                }
                _ => {}
            }
        }
    }

    /// `QueryExtension`: the extension's major opcode.
    fn extension(&mut self, name: &str) -> Result<u8, String> {
        let length = u16::try_from(name.len()).map_err(|_| "a long name")?;
        let mut body = length.to_le_bytes().to_vec();
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(name.as_bytes());
        let reply = self.round_trip(&request(QUERY_EXTENSION, 0, &body))?;
        match (reply.get(8), reply.get(9)) {
            (Some(1), Some(&opcode)) => Ok(opcode),
            _ => Err(format!("the server has no {name}")),
        }
    }

    /// The next packet, waiting at most `wait` (`None`: for ever).
    pub(crate) fn next(&mut self, wait: Option<Duration>) -> Result<Option<Packet>, String> {
        loop {
            if let Some(length) = length_of(&self.pending)
                && self.pending.len() >= length
            {
                let bytes: Vec<u8> = self.pending.drain(..length).collect();
                return Ok(Some(packet(&bytes, self.opcodes)));
            }
            self.stream
                .set_read_timeout(wait)
                .map_err(|error| format!("setting a timeout: {error}"))?;
            let mut buffer = [0u8; 4096];
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err("the server closed the connection".to_owned()),
                Ok(n) => self
                    .pending
                    .extend_from_slice(buffer.get(..n).unwrap_or_default()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(format!("reading: {error}")),
            }
        }
    }

    /// A window the size of the screen, black, named `name`, telling of its
    /// mapping, with `XInput2`'s raw motion selected on the root (as SDL does
    /// for a relative mouse) and Present's completions on the window.
    pub(crate) fn window(&mut self, name: &str) -> Result<u32, String> {
        let window = self.id();
        let screen = self.screen;
        // back pixel (bit 1) black, event mask (bit 11) StructureNotify.
        let mut body = words(&[window, screen.root]);
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(&screen.width.to_le_bytes());
        body.extend_from_slice(&screen.height.to_le_bytes());
        body.extend_from_slice(&[0, 0, 1, 0]);
        body.extend_from_slice(&words(&[0, (1 << 1) | (1 << 11), 0, 1 << 17]));
        self.send(&request(CREATE_WINDOW, 0, &body))?;
        // WM_NAME (39) and WM_CLASS (67), both STRING (31).
        let class = format!("{name}\0{name}\0");
        for (property, text) in [(39, name), (67, class.as_str())] {
            let length = u32::try_from(text.len()).unwrap_or(0);
            let mut body = words(&[window, property, 31, 8, length]);
            body.extend_from_slice(text.as_bytes());
            self.send(&request(CHANGE_PROPERTY, 0, &body))?;
        }
        let mask = 1_u32 << XI_RAW_MOTION;
        let mut body = words(&[screen.root]);
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&XI_ALL_MASTER_DEVICES.to_le_bytes());
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&mask.to_le_bytes());
        self.send(&request(self.opcodes.xi, XI_SELECT_EVENTS, &body))?;
        let event = self.id();
        let body = words(&[event, window, PRESENT_COMPLETE_MASK]);
        self.send(&request(self.opcodes.present, PRESENT_SELECT_INPUT, &body))?;
        Ok(window)
    }

    /// Map `window`.
    pub(crate) fn map(&mut self, window: u32) -> Result<(), String> {
        self.send(&request(MAP_WINDOW, 0, &words(&[window])))
    }

    /// A pixmap the size of the screen at its depth, for `window`.
    pub(crate) fn pixmap(&mut self, window: u32) -> Result<u32, String> {
        let pixmap = self.id();
        let mut body = words(&[pixmap, window]);
        body.extend_from_slice(&self.screen.width.to_le_bytes());
        body.extend_from_slice(&self.screen.height.to_le_bytes());
        self.send(&request(CREATE_PIXMAP, self.screen.depth, &body))?;
        Ok(pixmap)
    }

    /// A GC for `drawable` with no graphics exposures.
    pub(crate) fn gc(&mut self, drawable: u32) -> Result<u32, String> {
        let gc = self.id();
        // foreground (bit 2), graphics exposures (bit 16) off.
        let body = words(&[gc, drawable, (1 << 2) | (1 << 16), 0, 0]);
        self.send(&request(CREATE_GC, 0, &body))?;
        Ok(gc)
    }

    /// Fill all of `pixmap` with `pixel`.
    pub(crate) fn fill(&mut self, pixmap: u32, gc: u32, pixel: u32) -> Result<(), String> {
        self.send(&request(CHANGE_GC, 0, &words(&[gc, 1 << 2, pixel])))?;
        let mut body = words(&[pixmap, gc, 0]);
        body.extend_from_slice(&self.screen.width.to_le_bytes());
        body.extend_from_slice(&self.screen.height.to_le_bytes());
        self.send(&request(POLY_FILL_RECTANGLE, 0, &body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_present_pixmap_request_is_72_bytes_with_the_msc_at_48() {
        let bytes = present_pixmap(140, 0x20_0001, 0x20_0005, 7, 1234);
        assert_eq!(bytes.len(), 72);
        assert_eq!(bytes.get(..4), Some(&[140, PRESENT_PIXMAP, 18, 0][..]));
        assert_eq!(word(&bytes, 4), Some(0x20_0001));
        assert_eq!(word(&bytes, 8), Some(0x20_0005));
        assert_eq!(word(&bytes, 12), Some(7));
        assert_eq!(quad(&bytes, 48), Some(1234));
    }

    #[test]
    fn the_screen_is_found_past_the_vendor_and_the_formats() {
        // The id base at 12, a five-byte vendor (padded to eight) at 24,
        // two formats at 29.
        let mut setup = vec![0u8; 12];
        setup.extend_from_slice(&0x0020_0000_u32.to_le_bytes());
        setup.resize(24, 0);
        setup.extend_from_slice(&[5, 0, 0, 0, 0, 2]);
        setup.resize(40, 0);
        setup.extend_from_slice(b"yserv\0\0\0");
        setup.extend_from_slice(&[0u8; 16]);
        // The screen: its root, then its size at 20, its depth at 38.
        setup.extend_from_slice(&0x533_u32.to_le_bytes());
        setup.extend_from_slice(&[0u8; 16]);
        setup.extend_from_slice(&1920_u16.to_le_bytes());
        setup.extend_from_slice(&1080_u16.to_le_bytes());
        setup.extend_from_slice(&[0u8; 14]);
        setup.extend_from_slice(&[24, 0]);
        let found = screen_of(&setup);
        assert_eq!(
            found,
            Some(Screen {
                id_base: 0x20_0000,
                root: 0x533,
                width: 1920,
                height: 1080,
                depth: 24,
            })
        );
    }

    #[test]
    fn a_completion_is_read_from_its_generic_event() {
        let opcodes = Opcodes {
            xi: 131,
            present: 148,
        };
        let mut event = vec![GENERIC_EVENT, 148, 0, 0, 2, 0, 0, 0, 1, 0, 0, 1];
        event.extend_from_slice(&words(&[9, 0x20_0001, 42]));
        event.extend_from_slice(&5_000_123_u64.to_le_bytes());
        event.extend_from_slice(&300_u64.to_le_bytes());
        assert_eq!(length_of(&event), Some(40));
        assert_eq!(
            packet(&event, opcodes),
            Packet::Complete {
                serial: 42,
                mode: 1,
                ust: 5_000_123,
                msc: 300,
            }
        );
        let mut raw = vec![GENERIC_EVENT, 131, 0, 0, 4, 0, 0, 0, 17, 0, 2, 0];
        raw.extend_from_slice(&77_000_u32.to_le_bytes());
        raw.resize(48, 0);
        assert_eq!(length_of(&raw), Some(48));
        assert_eq!(packet(&raw, opcodes), Packet::RawMotion { time: 77_000 });
        let mut other = vec![GENERIC_EVENT, 99, 0, 0, 4, 0, 0, 0, 17, 0];
        other.resize(48, 0);
        assert_eq!(packet(&other, opcodes), Packet::Other);
    }

    #[test]
    fn a_local_display_is_its_socket() {
        assert_eq!(socket_of(":0").as_deref(), Some("/tmp/.X11-unix/X0"));
        assert_eq!(socket_of("unix:1.0").as_deref(), Some("/tmp/.X11-unix/X1"));
        assert_eq!(socket_of("host:0"), None);
    }
}
