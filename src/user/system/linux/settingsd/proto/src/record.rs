//! The records on the wire (`docs/SETTINGS-WRITER.md` §3).

use ferrix_auth_proto::Secret;

use crate::request::Request;
use crate::{MAX_RECORD, MAX_TEXT, VERSION};

/// What a client sends.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "a boxed Secret moved out of its box leaves an unzeroed copy behind; one record at a time is small enough"
)]
pub enum ToServer {
    /// The one request a connection carries.
    Request {
        /// Only say what would be done: ask no one, write nothing.
        dry_run: bool,
        /// What to do.
        request: Request,
    },
    /// The answer to the last PROMPT.
    Respond(Secret),
    /// The person will not go on.
    Cancel,
}

/// What `settingsd` sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToClient {
    /// Ask the person; `visible` is false for a secret.
    Prompt {
        /// Whether what they type may be shown.
        visible: bool,
        /// What to ask.
        text: String,
    },
    /// News for the person, from `authd`.
    Info(String),
    /// Over: the change is made, or, for a dry run, what it would make.
    Done {
        /// Whether anything changed (or would).
        changed: bool,
        /// What was done, for a person.
        text: String,
    },
    /// Over: nothing was written.
    Refused(Refusal),
}

/// Why a request was not carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Which kind of no.
    pub kind: RefusalKind,
    /// When the account may try again, for [`RefusalKind::AuthenticationFailed`].
    pub retry_after_ms: u32,
    /// The reason, for a person.
    pub text: String,
}

impl Refusal {
    /// A refusal of `kind` for `text`, with no wait.
    #[must_use]
    pub fn new(kind: RefusalKind, text: impl Into<String>) -> Refusal {
        Refusal {
            kind,
            retry_after_ms: 0,
            text: text.into(),
        }
    }
}

/// The kinds of refusal a UI words differently (§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// The request broke its own rules; no one was asked.
    Invalid,
    /// `authd` refused before any prompt: not in the group, no such policy.
    NotPermitted,
    /// The password was wrong, or the account must wait.
    AuthenticationFailed,
    /// No answer was given, or the client went away.
    Cancelled,
    /// `authd` or init could not be reached.
    Unavailable,
    /// Authorised, but writing or applying it failed; the text says what.
    Failed,
    /// A request or a version this `settingsd` does not have.
    Unsupported,
}

impl RefusalKind {
    const ALL: [RefusalKind; 7] = [
        RefusalKind::Invalid,
        RefusalKind::NotPermitted,
        RefusalKind::AuthenticationFailed,
        RefusalKind::Cancelled,
        RefusalKind::Unavailable,
        RefusalKind::Failed,
        RefusalKind::Unsupported,
    ];

    /// Its name, as the audit record and `settingsctl` say it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            RefusalKind::Invalid => "invalid",
            RefusalKind::NotPermitted => "not-permitted",
            RefusalKind::AuthenticationFailed => "authentication-failed",
            RefusalKind::Cancelled => "cancelled",
            RefusalKind::Unavailable => "unavailable",
            RefusalKind::Failed => "failed",
            RefusalKind::Unsupported => "unsupported",
        }
    }

    /// A heading for a UI to show above the refusal's text.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            RefusalKind::Invalid => "Not a valid setting",
            RefusalKind::NotPermitted => "Not allowed",
            RefusalKind::AuthenticationFailed => "Wrong password",
            RefusalKind::Cancelled => "Cancelled",
            RefusalKind::Unavailable => "Settings cannot be changed right now",
            RefusalKind::Failed => "The change failed",
            RefusalKind::Unsupported => "Not supported",
        }
    }

    const fn byte(self) -> u8 {
        match self {
            RefusalKind::Invalid => 1,
            RefusalKind::NotPermitted => 2,
            RefusalKind::AuthenticationFailed => 3,
            RefusalKind::Cancelled => 4,
            RefusalKind::Unavailable => 5,
            RefusalKind::Failed => 6,
            RefusalKind::Unsupported => 7,
        }
    }

    fn from_byte(byte: u8) -> Option<RefusalKind> {
        RefusalKind::ALL
            .into_iter()
            .find(|kind| kind.byte() == byte)
    }
}

/// Why a packet is not a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Empty, or longer than [`MAX_RECORD`].
    Length,
    /// A kind this side does not have.
    Kind,
    /// A version this side does not speak.
    Version,
    /// A request name this side does not have.
    Request,
    /// Cut short inside a field.
    Short,
    /// A flag that is neither 0 nor 1.
    Flag,
    /// A string too long, not UTF-8, or with control characters.
    Field,
    /// Bytes left after the last field.
    Trailing,
}

/// An encoded record, zeroed when dropped: a RESPOND's bytes are a
/// password.
#[derive(Default)]
pub struct Packet(Vec<u8>);

impl Packet {
    /// The bytes to send.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl core::fmt::Debug for Packet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Packet({} bytes)", self.0.len())
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        self.0.fill(0);
        let _ = core::hint::black_box(&self.0);
    }
}

mod kind {
    pub(super) const REQUEST: u8 = 1;
    pub(super) const RESPOND: u8 = 2;
    pub(super) const CANCEL: u8 = 3;
    pub(super) const PROMPT: u8 = 32;
    pub(super) const INFO: u8 = 33;
    pub(super) const DONE: u8 = 34;
    pub(super) const REFUSED: u8 = 35;
}

/// Whether `text` may go to a person: no control characters but tab and
/// newline, so a relayed line cannot move a terminal's cursor.
fn readable(text: &str) -> bool {
    text.len() <= MAX_TEXT
        && text
            .chars()
            .all(|c| c == '\t' || c == '\n' || !c.is_control())
}

/// A field that would not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unencodable;

struct Writer(Vec<u8>);

impl Writer {
    fn new(kind: u8) -> Writer {
        let mut bytes = Vec::with_capacity(MAX_RECORD);
        bytes.push(kind);
        Writer(bytes)
    }

    fn byte(&mut self, byte: u8) {
        self.0.push(byte);
    }

    fn flag(&mut self, flag: bool) {
        self.0.push(u8::from(flag));
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    /// A length and bytes; refused past `limit`.
    fn bytes(&mut self, bytes: &[u8], limit: usize) -> Result<(), Unencodable> {
        let len = u16::try_from(bytes.len()).map_err(|_| Unencodable)?;
        if bytes.len() > limit {
            return Err(Unencodable);
        }
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<(), Unencodable> {
        if !readable(text) {
            return Err(Unencodable);
        }
        self.bytes(text.as_bytes(), MAX_TEXT)
    }

    fn done(self) -> Result<Packet, Unencodable> {
        if self.0.len() > MAX_RECORD {
            let mut packet = Packet(self.0);
            packet.0.fill(0);
            return Err(Unencodable);
        }
        Ok(Packet(self.0))
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let (&first, rest) = self.0.split_first().ok_or(DecodeError::Short)?;
        self.0 = rest;
        Ok(first)
    }

    fn flag(&mut self) -> Result<bool, DecodeError> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::Flag),
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < count {
            return Err(DecodeError::Short);
        }
        let (taken, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take(4)?;
        let array: [u8; 4] = bytes.try_into().map_err(|_| DecodeError::Short)?;
        Ok(u32::from_le_bytes(array))
    }

    fn bytes(&mut self, limit: usize) -> Result<&'a [u8], DecodeError> {
        let len = self.take(2)?;
        let len = usize::from(u16::from_le_bytes([
            *len.first().ok_or(DecodeError::Short)?,
            *len.get(1).ok_or(DecodeError::Short)?,
        ]));
        if len > limit {
            return Err(DecodeError::Field);
        }
        self.take(len)
    }

    fn text(&mut self) -> Result<String, DecodeError> {
        let bytes = self.bytes(MAX_TEXT)?;
        let text = core::str::from_utf8(bytes).map_err(|_| DecodeError::Field)?;
        if !readable(text) {
            return Err(DecodeError::Field);
        }
        Ok(text.to_owned())
    }

    fn end(&self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Trailing)
        }
    }
}

fn start(packet: &[u8]) -> Result<(u8, Reader<'_>), DecodeError> {
    if packet.is_empty() || packet.len() > MAX_RECORD {
        return Err(DecodeError::Length);
    }
    let mut reader = Reader(packet);
    let kind = reader.byte()?;
    Ok((kind, reader))
}

impl ToServer {
    /// The record as bytes.
    ///
    /// # Errors
    ///
    /// A field too long or outside its alphabet, which would not decode.
    pub fn encode(&self) -> Result<Packet, Unencodable> {
        match self {
            ToServer::Request { dry_run, request } => {
                let mut w = Writer::new(kind::REQUEST);
                w.byte(VERSION);
                w.flag(*dry_run);
                w.text(request.name())?;
                match request {
                    Request::SetHostname { name } => w.text(name)?,
                    Request::ServiceEnablePersistent { unit, enabled } => {
                        w.text(unit)?;
                        w.flag(*enabled);
                    }
                }
                w.done()
            }
            ToServer::Respond(secret) => {
                let mut w = Writer::new(kind::RESPOND);
                w.bytes(secret.expose(), ferrix_auth_proto::MAX_SECRET)?;
                w.done()
            }
            ToServer::Cancel => Writer::new(kind::CANCEL).done(),
        }
    }

    /// The record `packet` holds. The caller zeroes `packet` after: a
    /// RESPOND's secret has been copied out of it.
    ///
    /// # Errors
    ///
    /// Why it is not a record.
    pub fn decode(packet: &[u8]) -> Result<ToServer, DecodeError> {
        let (kind, mut r) = start(packet)?;
        let record = match kind {
            kind::REQUEST => {
                if r.byte()? != VERSION {
                    return Err(DecodeError::Version);
                }
                let dry_run = r.flag()?;
                let name = r.text()?;
                let request = match name.as_str() {
                    "set-hostname" => Request::SetHostname { name: r.text()? },
                    "service-enable-persistent" => Request::ServiceEnablePersistent {
                        unit: r.text()?,
                        enabled: r.flag()?,
                    },
                    _ => return Err(DecodeError::Request),
                };
                ToServer::Request { dry_run, request }
            }
            kind::RESPOND => {
                let bytes = r.bytes(ferrix_auth_proto::MAX_SECRET)?;
                ToServer::Respond(Secret::from_bytes(bytes).ok_or(DecodeError::Field)?)
            }
            kind::CANCEL => ToServer::Cancel,
            _ => return Err(DecodeError::Kind),
        };
        r.end()?;
        Ok(record)
    }
}

impl ToClient {
    /// The record as bytes.
    ///
    /// # Errors
    ///
    /// A text too long or with control characters.
    pub fn encode(&self) -> Result<Packet, Unencodable> {
        match self {
            ToClient::Prompt { visible, text } => {
                let mut w = Writer::new(kind::PROMPT);
                w.flag(*visible);
                w.text(text)?;
                w.done()
            }
            ToClient::Info(text) => {
                let mut w = Writer::new(kind::INFO);
                w.text(text)?;
                w.done()
            }
            ToClient::Done { changed, text } => {
                let mut w = Writer::new(kind::DONE);
                w.flag(*changed);
                w.text(text)?;
                w.done()
            }
            ToClient::Refused(refusal) => {
                let mut w = Writer::new(kind::REFUSED);
                w.byte(refusal.kind.byte());
                w.u32(refusal.retry_after_ms);
                w.text(&refusal.text)?;
                w.done()
            }
        }
    }

    /// The record `packet` holds.
    ///
    /// # Errors
    ///
    /// Why it is not a record.
    pub fn decode(packet: &[u8]) -> Result<ToClient, DecodeError> {
        let (kind, mut r) = start(packet)?;
        let record = match kind {
            kind::PROMPT => ToClient::Prompt {
                visible: r.flag()?,
                text: r.text()?,
            },
            kind::INFO => ToClient::Info(r.text()?),
            kind::DONE => ToClient::Done {
                changed: r.flag()?,
                text: r.text()?,
            },
            kind::REFUSED => {
                let kind = RefusalKind::from_byte(r.byte()?).ok_or(DecodeError::Kind)?;
                let retry_after_ms = r.u32()?;
                ToClient::Refused(Refusal {
                    kind,
                    retry_after_ms,
                    text: r.text()?,
                })
            }
            _ => return Err(DecodeError::Kind),
        };
        r.end()?;
        Ok(record)
    }

    /// Whether the conversation is over with this record.
    #[must_use]
    pub const fn is_final(&self) -> bool {
        matches!(self, ToClient::Done { .. } | ToClient::Refused(_))
    }
}
