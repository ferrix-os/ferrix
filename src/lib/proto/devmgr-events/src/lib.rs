//! A driver's death and restart, as `devmgr` publishes them to programs
//! (`docs/DESKTOP.md` §4.2): what `devmgr` writes to its `devmgr-events`
//! helper on a channel, and the line the helper writes to every reader of
//! its socket.
//!
//! ```text
//! EVENT    devmgr -> devmgr-events, 48 bytes
//!   0  magic     "FXDE"
//!   4  what      u32: 1 died, 2 restarted, 3 gave up, 4 reinit,
//!                5 quarantined
//!   8  location  u32, the device's PCI address word
//!   12 value     u32: the exit status (died), the restarts so far
//!                (restarted, gave up, reinit, quarantined)
//!   16 driver    [u8; 32], the driver's program name, NUL-padded
//! LINE     devmgr-events -> reader, text
//!   "died <driver> <location> <status>\n"
//!   "restarted <driver> <location> <restarts>\n"
//!   "gave-up <driver> <location> <restarts>\n"
//!   "reinit <driver> <location> <restarts>\n"
//!   "quarantined <driver> <location> <restarts>\n"
//! ```
//!
//! A crate of its own: the kernel links `ferrix-devmgr-proto` and has no
//! use for this, and the kernel is not where these events go
//! (`docs/DESKTOP.md` §4.2: `devmgr` publishes, the kernel adds nothing).
//! Pure functions over bytes, tested on the host.

#![no_std]
#![forbid(unsafe_code)]

/// Bytes in a driver's name, NUL-padded.
pub const NAME_BYTES: usize = 32;
/// An event's first four bytes.
pub const MAGIC: [u8; 4] = *b"FXDE";
/// Bytes in an event.
pub const EVENT_BYTES: usize = 48;
/// The abstract `AF_UNIX` name the helper listens on, without its leading
/// NUL.
pub const SOCKET_NAME: &[u8] = b"ferrix.devmgr.events";
/// The longest line an event becomes.
pub const LINE_BYTES: usize = 80;

/// What happened to a driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    /// It died; `value` is its exit status (128 and the signal for one a
    /// signal ended).
    Died,
    /// It was started again and published; `value` is how many times.
    Restarted,
    /// It died once too often: its device stays quiesced; `value` is the
    /// restarts it had.
    GaveUp,
    /// Its restarts died at their start, so its device is reinitialised and
    /// it is started once more, the crash loop's last start (`docs/DEVMGR.md`
    /// §4); `value` is the restarts so far, that one counted.
    Reinit,
    /// That start died too: its device is quarantined, and nothing is
    /// started on it again but by an explicit bind; `value` is the restarts
    /// it had.
    Quarantined,
}

impl What {
    const fn code(self) -> u32 {
        match self {
            Self::Died => 1,
            Self::Restarted => 2,
            Self::GaveUp => 3,
            Self::Reinit => 4,
            Self::Quarantined => 5,
        }
    }

    const fn of(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Died),
            2 => Some(Self::Restarted),
            3 => Some(Self::GaveUp),
            4 => Some(Self::Reinit),
            5 => Some(Self::Quarantined),
            _ => None,
        }
    }

    /// The word a line starts with.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Died => "died",
            Self::Restarted => "restarted",
            Self::GaveUp => "gave-up",
            Self::Reinit => "reinit",
            Self::Quarantined => "quarantined",
        }
    }
}

/// One event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    /// What happened.
    pub what: What,
    /// The device's PCI address word.
    pub location: u32,
    /// The status or the restarts, as [`What`] says.
    pub value: u32,
    /// The driver's program name, NUL-padded.
    pub driver: [u8; NAME_BYTES],
}

/// Why bytes are not an event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Not [`EVENT_BYTES`] long, or not [`MAGIC`].
    Shape,
    /// A `what` no event has.
    What,
}

impl Event {
    /// An event about `driver` (its name, cut to [`NAME_BYTES`]).
    #[must_use]
    pub fn new(what: What, location: u32, value: u32, driver: &[u8]) -> Self {
        let mut name = [0_u8; NAME_BYTES];
        for (to, from) in name.iter_mut().zip(driver) {
            *to = *from;
        }
        Self {
            what,
            location,
            value,
            driver: name,
        }
    }

    /// The driver's name, without its padding.
    #[must_use]
    pub fn driver_name(&self) -> &[u8] {
        let end = self
            .driver
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(NAME_BYTES);
        self.driver.get(..end).unwrap_or_default()
    }

    /// The event's bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; EVENT_BYTES] {
        let mut out = [0_u8; EVENT_BYTES];
        let words = [self.what.code(), self.location, self.value];
        if let Some(magic) = out.get_mut(..4) {
            magic.copy_from_slice(&MAGIC);
        }
        for (index, word) in words.iter().enumerate() {
            let at = 4 + index * 4;
            if let Some(slot) = out.get_mut(at..at + 4) {
                slot.copy_from_slice(&word.to_le_bytes());
            }
        }
        if let Some(name) = out.get_mut(16..16 + NAME_BYTES) {
            name.copy_from_slice(&self.driver);
        }
        out
    }

    /// An event from its bytes.
    ///
    /// # Errors
    ///
    /// [`Malformed`] for bytes that are not one.
    pub fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        if bytes.len() != EVENT_BYTES || bytes.get(..4) != Some(&MAGIC[..]) {
            return Err(Malformed::Shape);
        }
        let word = |at: usize| {
            bytes
                .get(at..at + 4)
                .and_then(|slot| <[u8; 4]>::try_from(slot).ok())
                .map_or(0, u32::from_le_bytes)
        };
        let what = What::of(word(4)).ok_or(Malformed::What)?;
        let mut driver = [0_u8; NAME_BYTES];
        if let Some(name) = bytes.get(16..16 + NAME_BYTES) {
            driver.copy_from_slice(name);
        }
        Ok(Self {
            what,
            location: word(8),
            value: word(12),
            driver,
        })
    }

    /// The line a reader is sent: `<what> <driver> <location> <value>\n`,
    /// the location as eight hex digits. Gives the bytes written into
    /// `out`, a line never longer than [`LINE_BYTES`].
    pub fn line(&self, out: &mut [u8; LINE_BYTES]) -> usize {
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            for byte in bytes {
                if let Some(slot) = out.get_mut(at) {
                    *slot = *byte;
                    at += 1;
                }
            }
        };
        put(self.what.word().as_bytes());
        put(b" ");
        // Only printable bytes of the name: it reaches a person's screen.
        for byte in self.driver_name() {
            put(&[if byte.is_ascii_graphic() { *byte } else { b'?' }]);
        }
        put(b" 0x");
        let mut hex = [0_u8; 8];
        for (index, slot) in hex.iter_mut().enumerate() {
            let nibble = (self.location >> (28 - index * 4)) & 0xF;
            *slot = b"0123456789abcdef"
                .get(nibble as usize)
                .copied()
                .unwrap_or(b'0');
        }
        put(&hex);
        put(b" ");
        let mut digits = [0_u8; 10];
        let mut value = self.value;
        let mut count = 0;
        loop {
            if let Some(slot) = digits.get_mut(count) {
                *slot = b'0' + (value % 10) as u8;
            }
            count += 1;
            value /= 10;
            if value == 0 || count == digits.len() {
                break;
            }
        }
        for digit in digits.get(..count).unwrap_or_default().iter().rev() {
            put(&[*digit]);
        }
        put(b"\n");
        at
    }
}

#[cfg(test)]
mod tests {
    use super::{EVENT_BYTES, Event, LINE_BYTES, Malformed, What};

    /// An event survives its bytes, and becomes the line a reader parses.
    #[test]
    fn an_event_round_trips_and_becomes_a_line() {
        let event = Event::new(What::Restarted, 0x0001_0010, 2, b"snd");
        let bytes = event.encode();
        assert_eq!(Event::decode(&bytes), Ok(event));
        let mut line = [0_u8; LINE_BYTES];
        let length = event.line(&mut line);
        assert_eq!(&line[..length], b"restarted snd 0x00010010 2\n");
        let died = Event::new(What::Died, 0x10, 137, b"gpu");
        let length = died.line(&mut line);
        assert_eq!(&line[..length], b"died gpu 0x00000010 137\n");
        for (what, said) in [
            (What::Reinit, &b"reinit intel 0x00000010 3\n"[..]),
            (What::Quarantined, b"quarantined intel 0x00000010 3\n"),
        ] {
            let event = Event::new(what, 0x10, 3, b"intel");
            assert_eq!(Event::decode(&event.encode()), Ok(event));
            let length = event.line(&mut line);
            assert_eq!(&line[..length], said);
        }
    }

    /// Bytes that are not an event are refused.
    #[test]
    fn what_is_not_an_event_is_refused() {
        assert_eq!(Event::decode(&[0; 3]), Err(Malformed::Shape));
        let mut bytes = Event::new(What::Died, 0, 0, b"x").encode();
        bytes[4] = 9;
        assert_eq!(Event::decode(&bytes), Err(Malformed::What));
        assert_eq!(bytes.len(), EVENT_BYTES);
    }
}
