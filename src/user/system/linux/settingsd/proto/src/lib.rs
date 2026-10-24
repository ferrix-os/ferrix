//! What `settingsd` and its clients say to each other
//! (`docs/SETTINGS-WRITER.md` §3), and the requests' own rules (§4).
//!
//! ```text
//! client -> settingsd
//!   REQUEST   version, flags (bit 0: dry run), name, the request's own fields
//!   RESPOND   secret                      the answer to the last PROMPT
//!   CANCEL
//!
//! settingsd -> client
//!   PROMPT    visible, text               relayed from authd
//!   INFO      text                        relayed from authd
//!   DONE      changed, text               over: done, or what a dry run would do
//!   REFUSED   kind, retry_after_ms, text  over: nothing was written
//! ```
//!
//! One record a `SOCK_SEQPACKET` packet, at most [`MAX_RECORD`] bytes,
//! little-endian, each string a `u16` length and its UTF-8. A packet with a
//! byte left over, a flag that is not 0 or 1, or a string outside its
//! alphabet is not a record. [`Request`] is the closed list of what may be
//! asked: no field of any request is a path, a command or file content.

mod record;
mod request;
#[cfg(test)]
mod tests;

pub use ferrix_auth_proto::Secret;
pub use record::{DecodeError, Packet, Refusal, RefusalKind, ToClient, ToServer, Unencodable};
pub use request::{Area, Invalid, Request, SWITCHABLE_UNITS, valid_hostname, valid_zone};

/// Where `settingsd` listens.
pub const SOCKET: &str = "/run/ferrix/settings";

/// The protocol's version, the first byte of every REQUEST after its kind.
pub const VERSION: u8 = 1;

/// The longest record.
pub const MAX_RECORD: usize = 4096;

/// The longest text for a person in a record.
pub const MAX_TEXT: usize = 1024;
