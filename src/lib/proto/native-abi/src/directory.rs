//! The directory's messages (`docs/INIT.md` §6): what a service and init say
//! on the service's bootstrap channel.
//!
//! ```text
//! READY    service -> init   Type=native readiness (§5.3)
//! OFFER    service -> init   name; one handle: a channel end init sends CONNECTs down
//! OPEN     service -> init   name; one handle: the client's end of a new channel
//! CONNECT  init -> provider  name, the client unit; one handle: that end, forwarded
//! REFUSED  init -> service   name; why
//! GRANT    init -> service   name; one handle: a capability of init's, copied
//! ```
//!
//! GRANT is written into a new bootstrap channel before the service is
//! started, so it waits first in the service's queue: init gives lode the
//! processor service token so (`docs/CORE-QUARANTINE.md` §4), named
//! [`CPU_SERVICE`].
//!
//! Every message is little-endian on every target: a `u32` kind, then two
//! strings, each a `u32` length and UTF-8 bytes -- the name and a second
//! field, the client's unit for CONNECT and the reason for REFUSED, empty
//! otherwise. The handle rides in the channel message's handle array. A
//! message is at most [`MAX_MESSAGE`] bytes and a name at most
//! [`MAX_NAME`], so both sides keep one on the stack: native programs have
//! no allocator, and this module allocates nothing.

/// The longest name, in bytes: `ferrix.clipboard` and its kin.
pub const MAX_NAME: usize = 64;

/// The longest message, in bytes.
pub const MAX_MESSAGE: usize = 256;

/// The name a GRANT of the processor service token carries.
pub const CPU_SERVICE: &str = "cpu-service";

/// The name a GRANT of the memory scrub token carries.
pub const MEMORY_SCRUB: &str = "memory-scrub";

/// What a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The service is ready.
    Ready,
    /// The service offers a name, with the channel end CONNECTs go down.
    Offer,
    /// The service asks for a name, with the client's end of a new channel.
    Open,
    /// Init hands a provider a client's end.
    Connect,
    /// Init refuses an OPEN.
    Refused,
    /// Init hands a service a capability of its own.
    Grant,
}

impl Kind {
    const ALL: [Kind; 6] = [
        Kind::Ready,
        Kind::Offer,
        Kind::Open,
        Kind::Connect,
        Kind::Refused,
        Kind::Grant,
    ];

    /// Its number on the wire.
    #[must_use]
    pub const fn number(self) -> u32 {
        match self {
            Kind::Ready => 1,
            Kind::Offer => 2,
            Kind::Open => 3,
            Kind::Connect => 4,
            Kind::Refused => 5,
            Kind::Grant => 6,
        }
    }

    /// How many handles a message of this kind carries.
    #[must_use]
    pub const fn handles(self) -> usize {
        match self {
            Kind::Offer | Kind::Open | Kind::Connect | Kind::Grant => 1,
            Kind::Ready | Kind::Refused => 0,
        }
    }
}

/// A message, its strings borrowed from the bytes it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'a> {
    /// What it is.
    pub kind: Kind,
    /// The name, empty for READY.
    pub name: &'a str,
    /// The client's unit for CONNECT, the reason for REFUSED, else empty.
    pub detail: &'a str,
}

impl<'a> Message<'a> {
    /// READY.
    #[must_use]
    pub const fn ready() -> Message<'static> {
        Message {
            kind: Kind::Ready,
            name: "",
            detail: "",
        }
    }

    /// A message of `kind` for `name`.
    #[must_use]
    pub const fn named(kind: Kind, name: &'a str, detail: &'a str) -> Message<'a> {
        Message { kind, name, detail }
    }

    /// Write the message into `out`; how many bytes it took, or `None`
    /// when a string is too long or `out` too short.
    #[must_use]
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if self.name.len() > MAX_NAME {
            return None;
        }
        let mut at: usize = 0;
        let mut put = |bytes: &[u8]| -> Option<()> {
            let end = at.checked_add(bytes.len())?;
            out.get_mut(at..end)?.copy_from_slice(bytes);
            at = end;
            Some(())
        };
        put(&self.kind.number().to_le_bytes())?;
        for text in [self.name, self.detail] {
            put(&u32::try_from(text.len()).ok()?.to_le_bytes())?;
            put(text.as_bytes())?;
        }
        (at <= MAX_MESSAGE).then_some(at)
    }

    /// The message `bytes` are, or `None` for bytes that are not one: too
    /// short, an unknown kind, a string running past the end or not UTF-8,
    /// a name too long, or bytes left over.
    #[must_use]
    pub fn decode(bytes: &'a [u8]) -> Option<Message<'a>> {
        if bytes.len() > MAX_MESSAGE {
            return None;
        }
        let (kind, mut rest) = bytes.split_first_chunk::<4>()?;
        let number = u32::from_le_bytes(*kind);
        let kind = Kind::ALL.into_iter().find(|kind| kind.number() == number)?;
        let mut text = || -> Option<&'a str> {
            let (len, after) = rest.split_first_chunk::<4>()?;
            let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
            let (field, after) = after.split_at_checked(len)?;
            rest = after;
            core::str::from_utf8(field).ok()
        };
        let name = text()?;
        let detail = text()?;
        if !rest.is_empty() || name.len() > MAX_NAME {
            return None;
        }
        Some(Message { kind, name, detail })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_comes_back_as_it_went() {
        let messages = [
            Message::ready(),
            Message::named(Kind::Offer, "ferrix.test", ""),
            Message::named(Kind::Open, "ferrix.test", ""),
            Message::named(Kind::Connect, "ferrix.test", "asker.service"),
            Message::named(Kind::Refused, "ferrix.test", "not in Uses="),
        ];
        for message in messages {
            let mut out = [0_u8; MAX_MESSAGE];
            let len = message.encode(&mut out).unwrap();
            assert_eq!(Message::decode(&out[..len]), Some(message));
        }
    }

    #[test]
    fn bytes_that_are_not_a_message_are_refused() {
        assert_eq!(Message::decode(&[]), None);
        assert_eq!(Message::decode(&[9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        // A name whose length runs past the end.
        assert_eq!(Message::decode(&[3, 0, 0, 0, 200, 0, 0, 0]), None);
        // Bytes left over.
        let mut out = [0_u8; MAX_MESSAGE];
        let len = Message::ready().encode(&mut out).unwrap();
        assert_eq!(Message::decode(&out[..len + 1]), None);
        let long = [b'x'; MAX_NAME + 1];
        let name = core::str::from_utf8(&long).unwrap();
        assert_eq!(Message::named(Kind::Open, name, "").encode(&mut out), None);
    }

    #[test]
    fn only_offer_open_connect_and_grant_carry_a_handle() {
        let carrying: [usize; 6] = Kind::ALL.map(Kind::handles);
        assert_eq!(carrying, [0, 1, 1, 1, 0, 1]);
    }
}
