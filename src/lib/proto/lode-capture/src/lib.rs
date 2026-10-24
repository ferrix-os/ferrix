//! The screenshot provider protocol (`docs/LOGGING.md` §19, step 2b).
//!
//! lode never reads a display. A desktop that shows one -- hyprix first, any
//! compositor, kiosk or framebuffer grabber after it -- connects to lode's
//! `/run/lode/capture`, proves it holds the seat with the session's token
//! (§19.1), and is asked for a picture when something crashes.
//!
//! Every message is a frame: its kind and its payload's length, four bytes
//! each, little-endian, then the payload. Strings are a two-byte length and
//! UTF-8; the token is 16 bytes.
//!
//! ```text
//! REGISTER    provider -> lode   name, seat, outputs, formats (u32 mask), token [16]
//! ACCEPTED    lode -> provider   formats lode takes (u32 mask)
//! REFUSED     lode -> provider   why
//! CAPTURE     lode -> provider   request (u32), record seqnum (u64), what crashed
//! IMAGE       provider -> lode   request (u32), format (u32), width (u32), height (u32), bytes
//! DECLINED    provider -> lode   request (u32), why
//! UNREGISTER  provider -> lode   (or the connection closes)
//! ```
//!
//! [`qoi`] is the one image format every provider can write without a
//! library: QOI, lossless, a few dozen lines.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod qoi;

use alloc::string::String;
use alloc::vec::Vec;

/// Where lode listens for providers.
pub const SOCKET: &str = "/run/lode/capture";

/// The longest frame either side sends: an image of 16 MiB and its header.
pub const FRAME_MAX: usize = 16 * 1024 * 1024 + 64;

/// QOI, in a format mask.
pub const FORMAT_QOI: u32 = 1;
/// PNG, in a format mask.
pub const FORMAT_PNG: u32 = 2;

/// One message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A provider offers itself.
    Register {
        /// What it is (`hyprix`).
        name: String,
        /// The seat it shows (`seat0`).
        seat: String,
        /// Its outputs, comma-separated.
        outputs: String,
        /// The formats it can write.
        formats: u32,
        /// The session's token.
        token: [u8; 16],
    },
    /// lode takes it.
    Accepted {
        /// The formats lode takes.
        formats: u32,
    },
    /// lode does not.
    Refused {
        /// Why.
        why: String,
    },
    /// lode asks for a picture.
    Capture {
        /// The request's number, which the answer names.
        request: u32,
        /// The crash record's sequence number.
        seqnum: u64,
        /// One line of what crashed.
        what: String,
    },
    /// A picture.
    Image {
        /// The request it answers.
        request: u32,
        /// Its format.
        format: u32,
        /// Its width.
        width: u32,
        /// Its height.
        height: u32,
        /// The encoded bytes.
        bytes: Vec<u8>,
    },
    /// No picture.
    Declined {
        /// The request it answers.
        request: u32,
        /// Why (locked, nothing shown).
        why: String,
    },
    /// The provider leaves.
    Unregister,
}

/// The kinds, as numbers.
mod kind {
    pub(crate) const REGISTER: u32 = 1;
    pub(crate) const ACCEPTED: u32 = 2;
    pub(crate) const REFUSED: u32 = 3;
    pub(crate) const CAPTURE: u32 = 4;
    pub(crate) const IMAGE: u32 = 5;
    pub(crate) const DECLINED: u32 = 6;
    pub(crate) const UNREGISTER: u32 = 7;
}

impl Message {
    /// The whole frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        let kind = match self {
            Message::Register {
                name,
                seat,
                outputs,
                formats,
                token,
            } => {
                text(&mut payload, name);
                text(&mut payload, seat);
                text(&mut payload, outputs);
                payload.extend_from_slice(&formats.to_le_bytes());
                payload.extend_from_slice(token);
                kind::REGISTER
            }
            Message::Accepted { formats } => {
                payload.extend_from_slice(&formats.to_le_bytes());
                kind::ACCEPTED
            }
            Message::Refused { why } => {
                text(&mut payload, why);
                kind::REFUSED
            }
            Message::Capture {
                request,
                seqnum,
                what,
            } => {
                payload.extend_from_slice(&request.to_le_bytes());
                payload.extend_from_slice(&seqnum.to_le_bytes());
                text(&mut payload, what);
                kind::CAPTURE
            }
            Message::Image {
                request,
                format,
                width,
                height,
                bytes,
            } => {
                for value in [*request, *format, *width, *height] {
                    payload.extend_from_slice(&value.to_le_bytes());
                }
                payload.extend_from_slice(bytes);
                kind::IMAGE
            }
            Message::Declined { request, why } => {
                payload.extend_from_slice(&request.to_le_bytes());
                text(&mut payload, why);
                kind::DECLINED
            }
            Message::Unregister => kind::UNREGISTER,
        };
        let mut frame = Vec::with_capacity(payload.len() + 8);
        frame.extend_from_slice(&kind.to_le_bytes());
        frame.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_le_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// The first whole frame in `bytes`, and how many bytes it took.
    /// `Ok(None)` while the frame is not whole yet.
    ///
    /// # Errors
    ///
    /// A frame that is too long, of a kind there is none of, or whose
    /// payload does not read: the connection is to be dropped.
    pub fn decode(bytes: &[u8]) -> Result<Option<(Message, usize)>, Malformed> {
        let (Some(kind), Some(length)) = (u32_at(bytes, 0), u32_at(bytes, 4)) else {
            return Ok(None);
        };
        let length = usize::try_from(length).map_err(|_| Malformed)?;
        if length + 8 > FRAME_MAX {
            return Err(Malformed);
        }
        let Some(payload) = bytes.get(8..8 + length) else {
            return Ok(None);
        };
        let mut reader = Reader {
            bytes: payload,
            at: 0,
        };
        let message = match kind {
            kind::REGISTER => Message::Register {
                name: reader.text()?,
                seat: reader.text()?,
                outputs: reader.text()?,
                formats: reader.u32()?,
                token: reader.token()?,
            },
            kind::ACCEPTED => Message::Accepted {
                formats: reader.u32()?,
            },
            kind::REFUSED => Message::Refused {
                why: reader.text()?,
            },
            kind::CAPTURE => Message::Capture {
                request: reader.u32()?,
                seqnum: reader.u64()?,
                what: reader.text()?,
            },
            kind::IMAGE => Message::Image {
                request: reader.u32()?,
                format: reader.u32()?,
                width: reader.u32()?,
                height: reader.u32()?,
                bytes: reader.rest(),
            },
            kind::DECLINED => Message::Declined {
                request: reader.u32()?,
                why: reader.text()?,
            },
            kind::UNREGISTER => Message::Unregister,
            _ => return Err(Malformed),
        };
        if reader.at != payload.len() {
            return Err(Malformed);
        }
        Ok(Some((message, length + 8)))
    }
}

/// A frame that does not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// `value` as a string field.
fn text(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let bytes = bytes.get(..usize::from(u16::MAX)).unwrap_or(bytes);
    out.extend_from_slice(&u16::try_from(bytes.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The little-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// A payload, read field by field.
struct Reader<'a> {
    /// The payload.
    bytes: &'a [u8],
    /// The next field.
    at: usize,
}

impl Reader<'_> {
    /// `N` bytes.
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let field = self
            .bytes
            .get(self.at..self.at + N)
            .and_then(|slice| <[u8; N]>::try_from(slice).ok())
            .ok_or(Malformed)?;
        self.at += N;
        Ok(field)
    }

    /// A `u32`.
    fn u32(&mut self) -> Result<u32, Malformed> {
        self.take::<4>().map(u32::from_le_bytes)
    }

    /// A `u64`.
    fn u64(&mut self) -> Result<u64, Malformed> {
        self.take::<8>().map(u64::from_le_bytes)
    }

    /// The token.
    fn token(&mut self) -> Result<[u8; 16], Malformed> {
        self.take::<16>()
    }

    /// A string.
    fn text(&mut self) -> Result<String, Malformed> {
        let length = usize::from(u16::from_le_bytes(self.take::<2>()?));
        let bytes = self.bytes.get(self.at..self.at + length).ok_or(Malformed)?;
        self.at += length;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| Malformed)
    }

    /// Everything left.
    fn rest(&mut self) -> Vec<u8> {
        let rest = Vec::from(self.bytes.get(self.at..).unwrap_or_default());
        self.at = self.bytes.len();
        rest
    }
}

#[cfg(test)]
mod tests;
