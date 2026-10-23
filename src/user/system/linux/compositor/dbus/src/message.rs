//! A D-Bus message: the fixed header, the header fields, and the body as
//! bytes with its signature (the D-Bus Specification, "Message Format").

use crate::wire::{Malformed, Reader, Writer};

/// A message's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `METHOD_CALL`.
    Call,
    /// `METHOD_RETURN`.
    Return,
    /// `ERROR`.
    Error,
    /// `SIGNAL`.
    Signal,
}

impl Kind {
    const fn code(self) -> u8 {
        match self {
            Self::Call => 1,
            Self::Return => 2,
            Self::Error => 3,
            Self::Signal => 4,
        }
    }

    const fn of(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Call),
            2 => Some(Self::Return),
            3 => Some(Self::Error),
            4 => Some(Self::Signal),
            _ => None,
        }
    }
}

/// `NO_REPLY_EXPECTED`.
pub const NO_REPLY_EXPECTED: u8 = 0x1;

/// One message.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Message {
    /// Its type; `None` only for a default made to be filled in.
    pub kind: Option<Kind>,
    /// Its flags.
    pub flags: u8,
    /// Its serial, which a reply names.
    pub serial: u32,
    /// `PATH`.
    pub path: Option<String>,
    /// `INTERFACE`.
    pub interface: Option<String>,
    /// `MEMBER`.
    pub member: Option<String>,
    /// `ERROR_NAME`.
    pub error_name: Option<String>,
    /// `REPLY_SERIAL`.
    pub reply_serial: Option<u32>,
    /// `DESTINATION`.
    pub destination: Option<String>,
    /// `SENDER`.
    pub sender: Option<String>,
    /// `SIGNATURE`: the body's, empty for none.
    pub signature: String,
    /// The body, marshalled from offset 0 of its own (its alignment is the
    /// message's, since the header is padded to 8).
    pub body: Vec<u8>,
}

impl Message {
    /// A method call.
    #[must_use]
    pub fn call(destination: &str, path: &str, interface: &str, member: &str) -> Self {
        Self {
            kind: Some(Kind::Call),
            destination: Some(destination.to_owned()),
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            ..Self::default()
        }
    }

    /// A signal.
    #[must_use]
    pub fn signal(path: &str, interface: &str, member: &str) -> Self {
        Self {
            kind: Some(Kind::Signal),
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            ..Self::default()
        }
    }

    /// The return for `call`.
    #[must_use]
    pub fn reply_to(call: &Self) -> Self {
        Self {
            kind: Some(Kind::Return),
            flags: NO_REPLY_EXPECTED,
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            ..Self::default()
        }
    }

    /// An error answering `call`, with a message.
    #[must_use]
    pub fn error_to(call: &Self, name: &str, text: &str) -> Self {
        let mut body = Writer::new();
        body.string(text);
        Self {
            kind: Some(Kind::Error),
            flags: NO_REPLY_EXPECTED,
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            error_name: Some(name.to_owned()),
            signature: "s".to_owned(),
            body: body.into_bytes(),
            ..Self::default()
        }
    }

    /// With this body and its signature.
    #[must_use]
    pub fn with_body(mut self, signature: &str, body: Writer) -> Self {
        signature.clone_into(&mut self.signature);
        self.body = body.into_bytes();
        self
    }

    /// Whether this is a call of `interface.member`.
    #[must_use]
    pub fn is_call(&self, interface: &str, member: &str) -> bool {
        self.kind == Some(Kind::Call)
            && self.member.as_deref() == Some(member)
            && self
                .interface
                .as_deref()
                .is_none_or(|named| named == interface)
    }

    /// A reader over the body.
    #[must_use]
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.body)
    }

    /// The message's bytes, with `serial`.
    #[must_use]
    pub fn encode(&self, serial: u32) -> Vec<u8> {
        let mut out = Writer::new();
        out.byte(b'l');
        out.byte(self.kind.map_or(0, Kind::code));
        out.byte(self.flags);
        out.byte(1);
        out.u32(u32::try_from(self.body.len()).unwrap_or(u32::MAX));
        out.u32(serial);
        out.array(8, |fields| {
            let mut field = |code: u8, signature: &str, write: &dyn Fn(&mut Writer)| {
                fields.align(8);
                fields.byte(code);
                fields.signature(signature);
                write(fields);
            };
            if let Some(path) = &self.path {
                field(1, "o", &|w| w.string(path));
            }
            if let Some(interface) = &self.interface {
                field(2, "s", &|w| w.string(interface));
            }
            if let Some(member) = &self.member {
                field(3, "s", &|w| w.string(member));
            }
            if let Some(name) = &self.error_name {
                field(4, "s", &|w| w.string(name));
            }
            if let Some(reply) = self.reply_serial {
                field(5, "u", &|w| w.u32(reply));
            }
            if let Some(destination) = &self.destination {
                field(6, "s", &|w| w.string(destination));
            }
            if !self.signature.is_empty() {
                field(8, "g", &|w| w.signature(&self.signature));
            }
        });
        out.align(8);
        let mut bytes = out.into_bytes();
        bytes.extend(&self.body);
        bytes
    }

    /// How many bytes the message at the start of `bytes` takes, once its
    /// fixed header and field array's length have arrived.
    #[must_use]
    pub fn length(bytes: &[u8]) -> Option<usize> {
        let word = |at: usize| {
            bytes
                .get(at..at + 4)
                .and_then(|slot| <[u8; 4]>::try_from(slot).ok())
                .map(u32::from_le_bytes)
        };
        let body = usize::try_from(word(4)?).ok()?;
        let fields = usize::try_from(word(12)?).ok()?;
        let header = (16 + fields).next_multiple_of(8);
        Some(header + body)
    }

    /// The message at the start of `bytes`, whole.
    ///
    /// # Errors
    ///
    /// [`Malformed`] for bytes that are not one, or a big-endian message,
    /// which this reader does not take.
    pub fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        let mut reader = Reader::new(bytes);
        if reader.byte()? != b'l' {
            return Err(Malformed::Signature);
        }
        let kind = Kind::of(reader.byte()?);
        let flags = reader.byte()?;
        let _version = reader.byte()?;
        let body_length = usize::try_from(reader.u32()?).map_err(|_| Malformed::Short)?;
        let serial = reader.u32()?;
        let fields_length = usize::try_from(reader.u32()?).map_err(|_| Malformed::Short)?;
        let fields_end = reader.position() + fields_length;
        let mut message = Self {
            kind,
            flags,
            serial,
            ..Self::default()
        };
        while reader.position() < fields_end {
            reader.align(8)?;
            let code = reader.byte()?;
            let signature = reader.signature()?;
            match (code, signature) {
                (1, "o") => message.path = Some(reader.string()?.to_owned()),
                (2, "s") => message.interface = Some(reader.string()?.to_owned()),
                (3, "s") => message.member = Some(reader.string()?.to_owned()),
                (4, "s") => message.error_name = Some(reader.string()?.to_owned()),
                (5, "u") => message.reply_serial = Some(reader.u32()?),
                (6, "s") => message.destination = Some(reader.string()?.to_owned()),
                (7, "s") => message.sender = Some(reader.string()?.to_owned()),
                (8, "g") => message.signature = reader.signature()?.to_owned(),
                (_, other) => {
                    let mut left = other;
                    while !left.is_empty() {
                        left = reader.skip(left)?;
                    }
                }
            }
        }
        reader.align(8)?;
        let start = reader.position();
        message.body = bytes
            .get(start..start + body_length)
            .ok_or(Malformed::Short)?
            .to_vec();
        Ok(message)
    }
}
