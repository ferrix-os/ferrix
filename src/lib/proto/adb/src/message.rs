//! A message: a 24-byte header, and `data_length` bytes of payload after it.
//!
//! Every field is a little-endian `u32` (`protocol.txt`, "message
//! format"): the command, two arguments, the payload's length and checksum,
//! and a magic that is the command with every bit flipped. The commands are
//! four ASCII letters read as a word.

use alloc::string::String;
use alloc::vec::Vec;

/// Bytes in a header.
pub const HEADER_BYTES: usize = 24;

/// `CNXN`: connect, and say who you are.
pub const CNXN: u32 = u32::from_le_bytes(*b"CNXN");
/// `AUTH`: a token, a signature or a public key.
pub const AUTH: u32 = u32::from_le_bytes(*b"AUTH");
/// `OPEN`: open a stream to a service.
pub const OPEN: u32 = u32::from_le_bytes(*b"OPEN");
/// `OKAY`: a stream is open, or a write was taken.
pub const OKAY: u32 = u32::from_le_bytes(*b"OKAY");
/// `WRTE`: bytes on a stream.
pub const WRTE: u32 = u32::from_le_bytes(*b"WRTE");
/// `CLSE`: a stream is closed, or refused.
pub const CLSE: u32 = u32::from_le_bytes(*b"CLSE");

/// The protocol version this device speaks: the one from which the checksum
/// is no longer checked (`VERSION_SKIP_CHECKSUM`).
pub const VERSION: u32 = 0x0100_0001;

/// The largest payload this device sends or takes: 256 KiB, as adbd's own
/// `MAX_PAYLOAD` is on a device of its age. The connection uses the smaller
/// of this and what the host says in its `CNXN`.
pub const MAX_PAYLOAD: u32 = 256 * 1024;

/// A header, as decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Which message: [`CNXN`] and the rest.
    pub command: u32,
    /// Its first argument.
    pub arg0: u32,
    /// Its second argument.
    pub arg1: u32,
    /// How many payload bytes follow.
    pub data_length: u32,
    /// The payload's checksum, the sum of its bytes. Not checked from
    /// [`VERSION`] on, but sent, for a host that still does.
    pub data_check: u32,
}

/// What can be wrong with a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// The magic is not the command with every bit flipped.
    Magic,
    /// The payload is longer than the connection allows.
    TooLong(u32),
}

impl Header {
    /// A header for `command` with a payload of `data`.
    #[must_use]
    pub fn new(command: u32, arg0: u32, arg1: u32, data: &[u8]) -> Self {
        Header {
            command,
            arg0,
            arg1,
            data_length: u32::try_from(data.len()).unwrap_or(u32::MAX),
            data_check: checksum(data),
        }
    }

    /// The header's 24 bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let words = [
            self.command,
            self.arg0,
            self.arg1,
            self.data_length,
            self.data_check,
            !self.command,
        ];
        let mut bytes = [0; HEADER_BYTES];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// Decode a header, refusing one whose magic is wrong or whose payload
    /// is longer than `max_payload`.
    ///
    /// # Errors
    ///
    /// [`HeaderError`].
    pub fn decode(bytes: &[u8; HEADER_BYTES], max_payload: u32) -> Result<Self, HeaderError> {
        let word = |at: usize| {
            let mut word = [0; 4];
            if let Some(slice) = bytes.get(at..at + 4) {
                word.copy_from_slice(slice);
            }
            u32::from_le_bytes(word)
        };
        let header = Header {
            command: word(0),
            arg0: word(4),
            arg1: word(8),
            data_length: word(12),
            data_check: word(16),
        };
        if word(20) != !header.command {
            return Err(HeaderError::Magic);
        }
        if header.data_length > max_payload {
            return Err(HeaderError::TooLong(header.data_length));
        }
        Ok(header)
    }
}

/// The payload checksum older hosts check: the sum of the bytes.
#[must_use]
pub fn checksum(data: &[u8]) -> u32 {
    data.iter()
        .fold(0_u32, |sum, &byte| sum.wrapping_add(u32::from(byte)))
}

/// A whole message: a header and its payload, as bytes to send.
#[must_use]
pub fn message(command: u32, arg0: u32, arg1: u32, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + data.len());
    bytes.extend_from_slice(&Header::new(command, arg0, arg1, data).encode());
    bytes.extend_from_slice(data);
    bytes
}

/// The banner a device answers `CNXN` with: `device::`, then properties the
/// host shows (`adb devices -l`), then the features it may use.
///
/// No features are offered: without `shell_v2` the host opens `shell:` and
/// takes raw bytes, without `stat_v2`, `ls_v2` or `sendrecv_v2` it uses the
/// first sync protocol, and without `delayed_ack` it waits for an `OKAY`
/// after each `WRTE`. That is the subset `docs/ADB.md` §2 plans for first.
#[must_use]
pub fn banner(product: &str, model: &str, device: &str) -> String {
    let mut banner = String::from("device::");
    for (key, value) in [
        ("ro.product.name", product),
        ("ro.product.model", model),
        ("ro.product.device", device),
    ] {
        banner.push_str(key);
        banner.push('=');
        banner.push_str(value);
        banner.push(';');
    }
    banner.push_str("features=");
    banner
}

/// A service name as `OPEN` carries it: the payload up to its NUL.
#[must_use]
pub fn service_name(payload: &[u8]) -> &[u8] {
    let end = payload
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(payload.len());
    payload.get(..end).unwrap_or_default()
}
