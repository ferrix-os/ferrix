//! A DHCP message both ways: the requests a client writes, and the replies
//! it reads (RFC 2131 §2, options RFC 2132).
//!
//! A request is written whole here; a reply is read with every length
//! checked, since its bytes are whoever on the link answered first. Options
//! split across several instances are joined (RFC 3396), and the `file` and
//! `sname` fields are read for options when option 52 says they hold some.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

/// The port a client listens on.
pub const CLIENT_PORT: u16 = 68;

/// The port a server listens on.
pub const SERVER_PORT: u16 = 67;

/// Bytes before the options: the BOOTP fixed fields and the magic cookie.
pub const FIXED_LEN: usize = 240;

/// The shortest message sent. RFC 951 sized a BOOTP message at 300 bytes,
/// and some servers and relays still drop anything shorter.
pub const MIN_LEN: usize = 300;

/// The four bytes at offset 236 that make the options after them DHCP's.
pub const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

/// `op` for a message from a client.
const BOOTREQUEST: u8 = 1;

/// `op` for a message from a server.
const BOOTREPLY: u8 = 2;

/// `htype` for Ethernet, and the length of its addresses.
const ETHERNET: u8 = 1;
const ETHERNET_LEN: u8 = 6;

/// Where the fixed fields are.
mod at {
    pub(super) const XID: usize = 4;
    pub(super) const SECS: usize = 8;
    pub(super) const CIADDR: usize = 12;
    pub(super) const YIADDR: usize = 16;
    pub(super) const CHADDR: usize = 28;
    pub(super) const SNAME: usize = 44;
    pub(super) const FILE: usize = 108;
    pub(super) const MAGIC: usize = 236;
}

/// The message types (option 53).
pub mod kind {
    /// "Is there a server?"
    pub const DISCOVER: u8 = 1;
    /// "Here is an address."
    pub const OFFER: u8 = 2;
    /// "I will take that address", or "may I keep it?"
    pub const REQUEST: u8 = 3;
    /// "It is yours."
    pub const ACK: u8 = 5;
    /// "No, and stop using it."
    pub const NAK: u8 = 6;
    /// "I am done with it."
    pub const RELEASE: u8 = 7;
}

/// The option codes read and written here.
pub mod option {
    /// Padding: one byte, no length.
    pub const PAD: u8 = 0;
    /// The subnet mask.
    pub const NETMASK: u8 = 1;
    /// Routers, first preferred.
    pub const ROUTER: u8 = 3;
    /// Name servers.
    pub const DNS: u8 = 6;
    /// The host name.
    pub const HOSTNAME: u8 = 12;
    /// The domain name.
    pub const DOMAIN: u8 = 15;
    /// The interface MTU.
    pub const MTU: u8 = 26;
    /// The broadcast address.
    pub const BROADCAST: u8 = 28;
    /// The address a client asks for.
    pub const REQUESTED: u8 = 50;
    /// How long the lease lasts, in seconds.
    pub const LEASE: u8 = 51;
    /// Whether `file` and `sname` hold options.
    pub const OVERLOAD: u8 = 52;
    /// The message type.
    pub const KIND: u8 = 53;
    /// The server that sent it.
    pub const SERVER: u8 = 54;
    /// The options a client wants.
    pub const PARAMETERS: u8 = 55;
    /// The longest message a client takes.
    pub const MAX_SIZE: u8 = 57;
    /// T1: when to renew.
    pub const RENEW: u8 = 58;
    /// T2: when to ask any server.
    pub const REBIND: u8 = 59;
    /// Who the client is.
    pub const CLIENT_ID: u8 = 61;
    /// The end of the list.
    pub const END: u8 = 255;
}

/// What a client asks the server for in option 55: what [`Reply`] reads.
const PARAMETERS: [u8; 9] = [
    option::NETMASK,
    option::ROUTER,
    option::DNS,
    option::DOMAIN,
    option::MTU,
    option::BROADCAST,
    option::LEASE,
    option::RENEW,
    option::REBIND,
];

/// The longest message this client reads: an Ethernet frame's IP payload
/// less the IP and UDP headers it arrives in, as option 57 tells servers.
const MAX_SIZE: u16 = 1500 - 20 - 8;

/// A message a client sends.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    /// DISCOVER, REQUEST or RELEASE.
    pub kind: u8,
    /// The transaction this belongs to.
    pub xid: u32,
    /// Seconds since the client began asking.
    pub secs: u16,
    /// The interface's hardware address.
    pub mac: [u8; 6],
    /// The address the client holds and may use, for a renewal, a
    /// rebinding or a release; unspecified otherwise.
    pub ciaddr: Ipv4Addr,
    /// The address asked for (option 50), in a REQUEST after an OFFER.
    pub requested: Option<Ipv4Addr>,
    /// The server answered (option 54), in a REQUEST after an OFFER and in a
    /// RELEASE.
    pub server: Option<Ipv4Addr>,
}

impl Request {
    /// The message as it goes on the wire, padded to [`MIN_LEN`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![0_u8; FIXED_LEN];
        let fields: [(usize, &[u8]); 7] = [
            (0, &[BOOTREQUEST, ETHERNET, ETHERNET_LEN, 0]),
            (at::XID, &self.xid.to_be_bytes()),
            (at::SECS, &self.secs.to_be_bytes()),
            (at::CIADDR, &self.ciaddr.octets()),
            (at::CHADDR, &self.mac),
            (at::MAGIC, &MAGIC),
            // Flags stay zero: the reply may come to the hardware address,
            // since a packet socket takes it whatever its IP destination.
            (10, &[0, 0]),
        ];
        for (offset, value) in fields {
            if let Some(slot) = bytes.get_mut(offset..offset + value.len()) {
                slot.copy_from_slice(value);
            }
        }
        push(&mut bytes, option::KIND, &[self.kind]);
        let mut id = vec![ETHERNET];
        id.extend_from_slice(&self.mac);
        push(&mut bytes, option::CLIENT_ID, &id);
        if let Some(requested) = self.requested {
            push(&mut bytes, option::REQUESTED, &requested.octets());
        }
        if let Some(server) = self.server {
            push(&mut bytes, option::SERVER, &server.octets());
        }
        if self.kind != kind::RELEASE {
            push(&mut bytes, option::MAX_SIZE, &MAX_SIZE.to_be_bytes());
            push(&mut bytes, option::PARAMETERS, &PARAMETERS);
        }
        bytes.push(option::END);
        bytes.resize(bytes.len().max(MIN_LEN), option::PAD);
        bytes
    }
}

/// Append option `code` holding `value`, which is never longer than 255
/// bytes here.
fn push(bytes: &mut Vec<u8>, code: u8, value: &[u8]) {
    let Ok(len) = u8::try_from(value.len()) else {
        return;
    };
    bytes.push(code);
    bytes.push(len);
    bytes.extend_from_slice(value);
}

/// Why a message is not a reply this client can read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Shorter than the fixed fields.
    Short,
    /// Not a BOOTREPLY for Ethernet, or no magic cookie.
    NotAReply,
    /// An option's length runs past the end of its field.
    Option,
    /// No message type, or one of the wrong length.
    NoKind,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Malformed::Short => "shorter than a DHCP message",
            Malformed::NotAReply => "not a DHCP reply",
            Malformed::Option => "an option runs past its field",
            Malformed::NoKind => "no message type",
        })
    }
}

/// A message a server sent, as far as a client acts on it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reply {
    /// OFFER, ACK or NAK, or anything else, which the client ignores.
    pub kind: u8,
    /// The transaction it answers.
    pub xid: u32,
    /// The hardware address it is for: the first six bytes of `chaddr`.
    pub mac: [u8; 6],
    /// The address offered or given.
    pub yiaddr: Ipv4Addr,
    /// The server that sent it (option 54).
    pub server: Option<Ipv4Addr>,
    /// The subnet mask.
    pub netmask: Option<Ipv4Addr>,
    /// Routers.
    pub routers: Vec<Ipv4Addr>,
    /// Name servers.
    pub dns: Vec<Ipv4Addr>,
    /// The domain name, when it is printable ASCII.
    pub domain: Option<String>,
    /// The lease, in seconds.
    pub lease: Option<u32>,
    /// T1, in seconds.
    pub renew: Option<u32>,
    /// T2, in seconds.
    pub rebind: Option<u32>,
}

impl Reply {
    /// Read the message in `bytes`.
    ///
    /// # Errors
    ///
    /// [`Malformed`], for a message that is not a DHCP reply or whose
    /// options cannot be walked.
    pub fn parse(bytes: &[u8]) -> Result<Reply, Malformed> {
        let fixed = bytes.get(..FIXED_LEN).ok_or(Malformed::Short)?;
        if fixed.get(..3) != Some(&[BOOTREPLY, ETHERNET, ETHERNET_LEN][..])
            || fixed.get(at::MAGIC..FIXED_LEN) != Some(&MAGIC[..])
        {
            return Err(Malformed::NotAReply);
        }
        let mut options = BTreeMap::new();
        collect(bytes.get(FIXED_LEN..).unwrap_or_default(), &mut options)?;
        // Option 52: 1 is `file`, 2 is `sname`, 3 both, in that order.
        let overload = options
            .get(&option::OVERLOAD)
            .and_then(|value: &Vec<u8>| value.first().copied())
            .unwrap_or(0);
        if overload & 1 != 0 {
            collect(
                fixed.get(at::FILE..at::MAGIC).unwrap_or_default(),
                &mut options,
            )?;
        }
        if overload & 2 != 0 {
            collect(
                fixed.get(at::SNAME..at::FILE).unwrap_or_default(),
                &mut options,
            )?;
        }
        let kind = match options.get(&option::KIND).map(Vec::as_slice) {
            Some([kind]) => *kind,
            _ => return Err(Malformed::NoKind),
        };
        let address = |code| {
            options
                .get(&code)
                .and_then(|value| <[u8; 4]>::try_from(value.as_slice()).ok())
                .map(Ipv4Addr::from)
        };
        let addresses = |code| {
            options
                .get(&code)
                .map(|value| {
                    value
                        .chunks_exact(4)
                        .filter_map(|four| <[u8; 4]>::try_from(four).ok())
                        .map(Ipv4Addr::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        let seconds = |code| {
            options
                .get(&code)
                .and_then(|value| <[u8; 4]>::try_from(value.as_slice()).ok())
                .map(u32::from_be_bytes)
        };
        let word = |offset: usize| -> Result<[u8; 4], Malformed> {
            fixed
                .get(offset..offset + 4)
                .and_then(|four| <[u8; 4]>::try_from(four).ok())
                .ok_or(Malformed::Short)
        };
        let mac = fixed
            .get(at::CHADDR..at::CHADDR + 6)
            .and_then(|six| <[u8; 6]>::try_from(six).ok())
            .ok_or(Malformed::Short)?;
        Ok(Reply {
            kind,
            xid: u32::from_be_bytes(word(at::XID)?),
            mac,
            yiaddr: Ipv4Addr::from(word(at::YIADDR)?),
            server: address(option::SERVER),
            netmask: address(option::NETMASK),
            routers: addresses(option::ROUTER),
            dns: addresses(option::DNS),
            domain: options
                .get(&option::DOMAIN)
                .and_then(|value| std::str::from_utf8(value).ok())
                .map(|name| name.trim_end_matches('\0'))
                .filter(|name| {
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_graphic() && byte != b'#')
                })
                .map(str::to_owned),
            lease: seconds(option::LEASE),
            renew: seconds(option::RENEW),
            rebind: seconds(option::REBIND),
        })
    }
}

/// Walk the options in `field` into `into`, joining the values of a code
/// that appears more than once (RFC 3396).
fn collect(field: &[u8], into: &mut BTreeMap<u8, Vec<u8>>) -> Result<(), Malformed> {
    let mut rest = field;
    while let Some((&code, tail)) = rest.split_first() {
        match code {
            option::END => break,
            option::PAD => rest = tail,
            _ => {
                let (&len, tail) = tail.split_first().ok_or(Malformed::Option)?;
                let len = usize::from(len);
                let value = tail.get(..len).ok_or(Malformed::Option)?;
                into.entry(code).or_default().extend_from_slice(value);
                rest = tail.get(len..).unwrap_or_default();
            }
        }
    }
    Ok(())
}
