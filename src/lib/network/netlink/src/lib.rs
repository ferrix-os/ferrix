//! Netlink messages: walking a buffer of them and their attributes, and
//! building replies into a buffer the caller owns.
//!
//! `src/lib/proto/linux-abi` names netlink's numbers and lays out its fixed headers,
//! and says in as many words that walking a buffer of messages is left to this
//! crate. This is that crate: the byte-level half of `AF_NETLINK`, with no
//! socket, no interface table and no state of any kind.
//!
//! # A walk is a stranger's arithmetic, so it is checked and it ends
//!
//! A netlink buffer is a chain of length-prefixed messages, each holding a
//! chain of length-prefixed attributes, and every one of those lengths was
//! written by whoever called `sendmsg`. Three of them end a kernel that trusts
//! them: a length of zero, which walks the same message for ever; a length
//! below the header it introduces, which makes the payload start after it
//! ends; and a length past the end of the buffer, which reads somebody else's
//! memory. [`Messages`] and [`Attributes`] answer each of the three with
//! [`Error::Truncated`] and stop, and every step forward is at least a
//! header's worth, so a walk over any bytes at all ends after at most one step
//! per four bytes of input. The `netlink_walk` fuzz target holds them to it.
//!
//! # Nothing is allocated and nothing is copied
//!
//! A walk borrows its payloads from the buffer it was given, and the builder
//! writes into a buffer the caller already owns and answers how many bytes it
//! took. That is what lets the kernel encode a reply inside the lock that
//! holds the net core, where an allocation would be a bug: `src/lib/network/net` and its
//! lock are `src/kernel/extensions/personality/linux/src/net`'s, and nothing that could sleep may happen while
//! it is held.
//!
//! ```
//! use ferrix_linux_abi::netlink::{IFLA_IFNAME, IFLA_MTU, NlMsgHdr, RTM_NEWLINK};
//! use ferrix_netlink::{Attr, Messages, Value, Writer};
//!
//! let mut buffer = [0_u8; 128];
//! let mut writer = Writer::new(&mut buffer);
//! let header = NlMsgHdr {
//!     len: 0, // the writer fills this in
//!     kind: RTM_NEWLINK,
//!     flags: 0,
//!     seq: 7,
//!     pid: 1,
//! };
//! let body = [0_u8; 16]; // struct ifinfomsg
//! let attributes = [
//!     Attr::new(IFLA_IFNAME, Value::Name(b"lo")),
//!     Attr::new(IFLA_MTU, Value::U32(65_536)),
//! ];
//! let written = writer.message(header, &body, &attributes)?;
//!
//! let mut walk = Messages::new(&buffer[..written]);
//! let message = walk.next().expect("one message")?;
//! assert_eq!(message.header.kind, RTM_NEWLINK);
//! let names: Vec<&[u8]> = message
//!     .attributes(body.len())
//!     .filter_map(Result::ok)
//!     .filter(|attribute| attribute.kind() == IFLA_IFNAME)
//!     .map(|attribute| attribute.as_name())
//!     .collect();
//! assert_eq!(names, [b"lo"]);
//! assert!(walk.next().is_none());
//! # Ok::<(), ferrix_netlink::Error>(())
//! ```

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate alloc;

pub mod build;
pub mod walk;

#[cfg(test)]
mod tests;

pub use build::{Attr, Value, Writer};
pub use walk::{Attribute, Attributes, Message, Messages};

use ferrix_linux_abi::socket::{AF_INET, AF_INET6};

/// Why a buffer could not be walked, or a message could not be written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A length field claims fewer bytes than the header it introduces, or
    /// more than the buffer still holds. The walk stops there rather than
    /// reading past the end or standing still.
    Truncated,
    /// The buffer the caller offered has no room for what was asked, or a
    /// length would not fit the field that has to carry it.
    NoSpace,
}

/// An address as a routing attribute carries it: four bytes or sixteen, in
/// network order, with nothing to say which family they are but their length.
///
/// `RTA_DST`, `RTA_GATEWAY`, `IFA_ADDRESS`, `IFA_LOCAL` and `NDA_DST` all
/// carry this and nothing else; the family is in the fixed header of the
/// message they belong to, and a sender that disagrees with itself is why
/// [`Address::parse`] answers the length rather than trusting the header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Address {
    /// Four bytes: an IPv4 address.
    V4([u8; 4]),
    /// Sixteen bytes: an IPv6 address.
    V6([u8; 16]),
}

impl Address {
    /// The address those bytes spell, by their length alone: `None` for any
    /// length that is neither four nor sixteen.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Address> {
        if let Ok(four) = <[u8; 4]>::try_from(bytes) {
            return Some(Address::V4(four));
        }
        <[u8; 16]>::try_from(bytes).ok().map(Address::V6)
    }

    /// The bytes an attribute carries.
    #[must_use]
    pub const fn bytes(&self) -> &[u8] {
        match self {
            Address::V4(four) => four,
            Address::V6(six) => six,
        }
    }

    /// The `AF_` number of the family this address belongs to.
    #[must_use]
    pub const fn family(self) -> u16 {
        match self {
            Address::V4(_) => AF_INET,
            Address::V6(_) => AF_INET6,
        }
    }

    /// How many bits long a prefix of this family may be: the whole address.
    #[must_use]
    pub const fn bits(self) -> u8 {
        match self {
            Address::V4(_) => 32,
            Address::V6(_) => 128,
        }
    }
}
