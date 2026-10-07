//! A new version of a driver, without a reboot (`docs/DEVMGR.md` §4.1): what
//! `/bin/drvupdate` sends `drvupdated` over its socket, what `drvupdated`
//! hands `devmgr` with the image's VMO, and the answer that comes back the
//! same way.
//!
//! ```text
//! REQUEST  client -> drvupdated, and drvupdated -> devmgr, 48 bytes
//!   0  magic    "FXDU"
//!   4  location u32, the device's PCI address word, or ANY
//!   8  length   u64, the image's bytes, which follow on the socket and
//!               are the VMO's on the channel
//!   16 driver   [u8; 32], its program name, NUL-padded
//! ANSWER   devmgr -> drvupdated, and drvupdated -> client, 16 bytes
//!   0  magic    "FXDA"
//!   4  outcome  u32
//!   8  updated  u32, devices now driven by the new image
//!   12 tried    u32, devices the request named
//! ```
//!
//! Pure functions over bytes, so the client (a Linux program), the helper
//! and `devmgr` share one definition and it is tested on the host.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// Bytes in a driver's name, NUL-padded: `ferrix_devmgr_proto::NAME_BYTES`,
/// the kernel's `PROCESS_NAME_MAX`. A crate of its own rather than a module
/// of that one because the kernel links that one and has no use for this
/// (the certification consultant's condition C8, 2026-10-07).
pub const NAME_BYTES: usize = 32;

/// A request's first four bytes.
pub const REQUEST_MAGIC: [u8; 4] = *b"FXDU";
/// An answer's first four bytes.
pub const ANSWER_MAGIC: [u8; 4] = *b"FXDA";
/// Bytes in a request's header.
pub const REQUEST_BYTES: usize = 48;
/// Bytes in an answer.
pub const ANSWER_BYTES: usize = 16;
/// The location that names every device the driver drives.
pub const ANY: u32 = u32::MAX;
/// The largest image an update takes: 8 MiB, past every driver's image.
pub const MAX_IMAGE: u64 = 8 << 20;
/// The abstract `AF_UNIX` name `drvupdated` listens on, without its leading
/// NUL.
pub const SOCKET_NAME: &[u8] = b"ferrix.devmgr.update";

/// What a request asks: put the image on the devices `driver` drives at
/// `location`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    /// The driver's program name, NUL-padded.
    pub driver: [u8; NAME_BYTES],
    /// The device's PCI address word, or [`ANY`].
    pub location: u32,
    /// The image's length in bytes.
    pub length: u64,
}

/// Why bytes are not a request or an answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Not the size the message has.
    Length,
    /// The magic is not this message's.
    Magic,
    /// The driver's name is empty, or not NUL-padded.
    Name,
    /// The image is empty, or longer than [`MAX_IMAGE`].
    ImageLength,
    /// The outcome is not one [`Outcome`] has.
    Outcome,
}

impl Request {
    /// A request for `driver`, which must be shorter than [`NAME_BYTES`].
    #[must_use]
    pub fn new(driver: &[u8], location: u32, length: u64) -> Option<Request> {
        if driver.is_empty() || driver.len() >= NAME_BYTES || driver.contains(&0) {
            return None;
        }
        let mut name = [0_u8; NAME_BYTES];
        name.get_mut(..driver.len())?.copy_from_slice(driver);
        Some(Request {
            driver: name,
            location,
            length,
        })
    }

    /// The driver's name without its padding.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        let end = self
            .driver
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(NAME_BYTES);
        self.driver.get(..end).unwrap_or_default()
    }

    /// The 48 bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; REQUEST_BYTES] {
        let mut out = [0_u8; REQUEST_BYTES];
        put(&mut out, 0, &REQUEST_MAGIC);
        put(&mut out, 4, &self.location.to_le_bytes());
        put(&mut out, 8, &self.length.to_le_bytes());
        put(&mut out, 16, &self.driver);
        out
    }

    /// Read a request back.
    ///
    /// # Errors
    ///
    /// [`Refused`], naming the first thing wrong.
    pub fn decode(bytes: &[u8]) -> Result<Request, Refused> {
        if bytes.len() != REQUEST_BYTES {
            return Err(Refused::Length);
        }
        if bytes.get(..4) != Some(&REQUEST_MAGIC[..]) {
            return Err(Refused::Magic);
        }
        let location = u32_at(bytes, 4).ok_or(Refused::Length)?;
        let length = bytes
            .get(8..16)
            .and_then(|word| word.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or(Refused::Length)?;
        let mut driver = [0_u8; NAME_BYTES];
        driver.copy_from_slice(bytes.get(16..REQUEST_BYTES).ok_or(Refused::Length)?);
        let request = Request {
            driver,
            location,
            length,
        };
        let name = request.name().len();
        // Empty, or a byte after the first NUL: not a name padded with NULs.
        if name == 0
            || driver
                .get(name..)
                .is_some_and(|rest| rest.iter().any(|&b| b != 0))
        {
            return Err(Refused::Name);
        }
        if length == 0 || length > MAX_IMAGE {
            return Err(Refused::ImageLength);
        }
        Ok(request)
    }
}

/// How an update ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Outcome {
    /// Every device the request named is driven by the new image.
    Updated = 0,
    /// No device the request named is driven by that driver.
    NoDevice = 1,
    /// A device it named is of a kind `devmgr` does not start again.
    NotRestarted = 2,
    /// A device it named has no driver up to replace.
    Busy = 3,
    /// The kernel's loader refused the image; nothing was stopped.
    Refused = 4,
    /// The new image did not publish, and the old one drives the device
    /// again.
    RolledBack = 5,
    /// Neither image published: the device is quiesced.
    Failed = 6,
    /// The peer is not root.
    NotAllowed = 7,
    /// The request did not decode, or its image did not arrive whole.
    Malformed = 8,
    /// `devmgr` did not answer: it has no memory for the image, or the
    /// channel to it is gone.
    Unanswered = 9,
}

impl Outcome {
    /// The outcome numbered `value`.
    #[must_use]
    pub const fn from_u32(value: u32) -> Option<Outcome> {
        Some(match value {
            0 => Outcome::Updated,
            1 => Outcome::NoDevice,
            2 => Outcome::NotRestarted,
            3 => Outcome::Busy,
            4 => Outcome::Refused,
            5 => Outcome::RolledBack,
            6 => Outcome::Failed,
            7 => Outcome::NotAllowed,
            8 => Outcome::Malformed,
            9 => Outcome::Unanswered,
            _ => return None,
        })
    }

    /// What it is called on the client's line and `devmgr`'s.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Outcome::Updated => "updated",
            Outcome::NoDevice => "no device",
            Outcome::NotRestarted => "not restarted",
            Outcome::Busy => "busy",
            Outcome::Refused => "refused",
            Outcome::RolledBack => "rolled back",
            Outcome::Failed => "failed",
            Outcome::NotAllowed => "not allowed",
            Outcome::Malformed => "malformed",
            Outcome::Unanswered => "unanswered",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.word())
    }
}

/// The answer to a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Answer {
    /// How it ended.
    pub outcome: Outcome,
    /// Devices now driven by the new image.
    pub updated: u32,
    /// Devices the request named.
    pub tried: u32,
}

impl Answer {
    /// `outcome`, with no device touched.
    #[must_use]
    pub const fn of(outcome: Outcome) -> Answer {
        Answer {
            outcome,
            updated: 0,
            tried: 0,
        }
    }

    /// The 16 bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; ANSWER_BYTES] {
        let mut out = [0_u8; ANSWER_BYTES];
        put(&mut out, 0, &ANSWER_MAGIC);
        put(&mut out, 4, &(self.outcome as u32).to_le_bytes());
        put(&mut out, 8, &self.updated.to_le_bytes());
        put(&mut out, 12, &self.tried.to_le_bytes());
        out
    }

    /// Read an answer back.
    ///
    /// # Errors
    ///
    /// [`Refused`], naming the first thing wrong.
    pub fn decode(bytes: &[u8]) -> Result<Answer, Refused> {
        if bytes.len() != ANSWER_BYTES {
            return Err(Refused::Length);
        }
        if bytes.get(..4) != Some(&ANSWER_MAGIC[..]) {
            return Err(Refused::Magic);
        }
        let word = |at| u32_at(bytes, at).ok_or(Refused::Length);
        Ok(Answer {
            outcome: Outcome::from_u32(word(4)?).ok_or(Refused::Outcome)?,
            updated: word(8)?,
            tried: word(12)?,
        })
    }
}

/// FNV-1a over 64 bits: the fingerprint `devmgr`'s line gives an image, so
/// the log shows which bytes drive a device. It identifies bytes and
/// proves nothing against an adversary, who can make other bytes with the
/// same fingerprint (`docs/DEVMGR.md` §4.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fingerprint(u64);

impl Default for Fingerprint {
    fn default() -> Fingerprint {
        Fingerprint::new()
    }
}

impl Fingerprint {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Nothing fingerprinted yet.
    #[must_use]
    pub const fn new() -> Fingerprint {
        Fingerprint(Fingerprint::OFFSET)
    }

    /// Take in the next `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(Fingerprint::PRIME);
        }
    }

    /// The fingerprint so far.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at.checked_add(4)?)
        .and_then(|word| word.try_into().ok())
        .map(u32::from_le_bytes)
}

fn put(out: &mut [u8], at: usize, source: &[u8]) {
    if let Some(slot) = out.get_mut(at..) {
        for (to, from) in slot.iter_mut().zip(source) {
            *to = *from;
        }
    }
}

#[cfg(test)]
mod tests;
