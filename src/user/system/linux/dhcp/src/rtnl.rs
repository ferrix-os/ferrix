//! The rtnetlink requests that apply a lease, and the walk of what comes
//! back: what `ip link set up`, `ip addr add` and `ip route add default`
//! send, written by `ferrix-netlink`, the crate the kernel answers them
//! with.
//!
//! Every function here builds bytes or reads them; the socket is the
//! caller's. A change is sent with `NLM_F_ACK`, so each has an answer --
//! an `NLMSG_ERROR` whose error is 0 for success -- and [`outcome`] reads it.

use std::net::Ipv4Addr;

use ferrix_linux_abi::netlink::{
    IFA_ADDRESS, IFA_BROADCAST, IFA_LOCAL, IFF_UP, IFLA_ADDRESS, IFLA_IFNAME, IfAddrMsg, IfInfoMsg,
    NLM_F_ACK, NLM_F_CREATE, NLM_F_DUMP, NLM_F_EXCL, NLM_F_REQUEST, NLMSG_DONE, NLMSG_ERROR,
    NlMsgErr, NlMsgHdr, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN, RTA_GATEWAY, RTA_OIF, RTM_DELADDR,
    RTM_DELROUTE, RTM_GETADDR, RTM_GETLINK, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWROUTE, RTN_UNICAST,
    RtMsg,
};
use ferrix_linux_abi::socket::{AF_INET, AF_UNSPEC};
use ferrix_netlink::{Address, Attr, Messages, Value, Writer};

/// `RTPROT_DHCP`: the routes this client adds say who added them, as
/// `dhcpcd`'s do.
const RTPROT_DHCP: u8 = 16;

/// Room for any one request here.
const REQUEST_ROOM: usize = 256;

/// An interface, as a link dump names it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Link {
    /// Its index.
    pub index: u32,
    /// Its name.
    pub name: String,
    /// Its hardware address, when it has a six-byte one.
    pub mac: Option<[u8; 6]>,
}

/// An IPv4 address on an interface, as an address dump names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Assigned {
    /// The interface.
    pub index: u32,
    /// The address.
    pub address: Ipv4Addr,
    /// Its prefix length.
    pub prefix: u8,
}

/// One request: `kind` with `flags`, the fixed `body` and `attributes`.
fn request(kind: u16, flags: u16, seq: u32, body: &[u8], attributes: &[Attr<'_>]) -> Vec<u8> {
    let mut buffer = [0_u8; REQUEST_ROOM];
    let mut writer = Writer::new(&mut buffer);
    let header = NlMsgHdr {
        len: 0,
        kind,
        flags: NLM_F_REQUEST | flags,
        seq,
        pid: 0,
    };
    // Every request here is a few dozen bytes; one that did not fit would
    // be sent empty and refused by the kernel, which says so.
    let _ = writer.message(header, body, attributes);
    writer.written().to_vec()
}

/// Every link: `RTM_GETLINK` as a dump.
#[must_use]
pub fn get_links(seq: u32) -> Vec<u8> {
    let body = IfInfoMsg {
        family: u8::try_from(AF_UNSPEC).unwrap_or(0),
        kind: 0,
        index: 0,
        flags: 0,
        change: 0,
    };
    request(RTM_GETLINK, NLM_F_DUMP, seq, &body.to_bytes(), &[])
}

/// Every IPv4 address: `RTM_GETADDR` as a dump.
#[must_use]
pub fn get_addresses(seq: u32) -> Vec<u8> {
    let body = IfAddrMsg {
        family: inet(),
        prefix_len: 0,
        flags: 0,
        scope: 0,
        index: 0,
    };
    request(RTM_GETADDR, NLM_F_DUMP, seq, &body.to_bytes(), &[])
}

/// Interface `index` up, as `ip link set dev IFACE up` asks.
#[must_use]
pub fn set_up(seq: u32, index: u32) -> Vec<u8> {
    let body = IfInfoMsg {
        family: u8::try_from(AF_UNSPEC).unwrap_or(0),
        kind: 0,
        index: i32::try_from(index).unwrap_or(0),
        flags: IFF_UP,
        change: IFF_UP,
    };
    request(RTM_NEWLINK, NLM_F_ACK, seq, &body.to_bytes(), &[])
}

/// `address`/`prefix` on interface `index`, with its subnet's broadcast
/// address, as `ip addr add ADDRESS/PREFIX brd + dev IFACE` asks.
#[must_use]
pub fn add_address(seq: u32, index: u32, address: Ipv4Addr, prefix: u8) -> Vec<u8> {
    let broadcast = crate::broadcast_of(address, prefix);
    let attributes = [
        Attr::new(IFA_LOCAL, Value::Address(Address::V4(address.octets()))),
        Attr::new(IFA_ADDRESS, Value::Address(Address::V4(address.octets()))),
        Attr::new(
            IFA_BROADCAST,
            Value::Address(Address::V4(broadcast.octets())),
        ),
    ];
    request(
        RTM_NEWADDR,
        NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        seq,
        &address_body(index, prefix).to_bytes(),
        &attributes,
    )
}

/// `address`/`prefix` off interface `index`.
#[must_use]
pub fn delete_address(seq: u32, index: u32, address: Ipv4Addr, prefix: u8) -> Vec<u8> {
    let attributes = [
        Attr::new(IFA_LOCAL, Value::Address(Address::V4(address.octets()))),
        Attr::new(IFA_ADDRESS, Value::Address(Address::V4(address.octets()))),
    ];
    request(
        RTM_DELADDR,
        NLM_F_ACK,
        seq,
        &address_body(index, prefix).to_bytes(),
        &attributes,
    )
}

/// The default route through `gateway` on interface `index`, as `ip route
/// add default via GATEWAY dev IFACE proto dhcp` asks.
#[must_use]
pub fn add_default_route(seq: u32, index: u32, gateway: Ipv4Addr) -> Vec<u8> {
    let attributes = [
        Attr::new(RTA_GATEWAY, Value::Address(Address::V4(gateway.octets()))),
        Attr::new(RTA_OIF, Value::U32(index)),
    ];
    request(
        RTM_NEWROUTE,
        NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        seq,
        &route_body().to_bytes(),
        &attributes,
    )
}

/// The default route on interface `index`, whichever gateway it has, as
/// `ip route del default dev IFACE` asks.
#[must_use]
pub fn delete_default_route(seq: u32, index: u32) -> Vec<u8> {
    let attributes = [Attr::new(RTA_OIF, Value::U32(index))];
    request(
        RTM_DELROUTE,
        NLM_F_ACK,
        seq,
        &route_body().to_bytes(),
        &attributes,
    )
}

/// `AF_INET` as a fixed header's family byte.
fn inet() -> u8 {
    u8::try_from(AF_INET).unwrap_or(2)
}

/// The `ifaddrmsg` of an IPv4 address on `index`.
fn address_body(index: u32, prefix: u8) -> IfAddrMsg {
    IfAddrMsg {
        family: inet(),
        prefix_len: prefix,
        flags: 0,
        scope: RT_SCOPE_UNIVERSE,
        index,
    }
}

/// The `rtmsg` of an IPv4 default route of this client's.
fn route_body() -> RtMsg {
    RtMsg {
        family: inet(),
        dst_len: 0,
        src_len: 0,
        tos: 0,
        table: RT_TABLE_MAIN,
        protocol: RTPROT_DHCP,
        scope: RT_SCOPE_UNIVERSE,
        kind: RTN_UNICAST,
        flags: 0,
    }
}

/// What one buffer of replies to request `seq` said.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// More replies are coming: a dump's parts without its `NLMSG_DONE`.
    More,
    /// The request was answered: an `NLMSG_ERROR` of 0, or a dump's
    /// `NLMSG_DONE`.
    Done,
    /// The request was refused with this `errno`.
    Refused(i32),
}

/// What `buffer`, replies read from the socket, says about request `seq`;
/// the parts of a dump are passed to `part`, as `(type, payload)`.
#[must_use]
pub fn outcome(buffer: &[u8], seq: u32, mut part: impl FnMut(u16, &[u8])) -> Outcome {
    for message in Messages::new(buffer) {
        let Ok(message) = message else {
            return Outcome::Refused(libc_eproto());
        };
        if message.header.seq != seq {
            continue;
        }
        match message.header.kind {
            NLMSG_DONE => return Outcome::Done,
            NLMSG_ERROR => {
                let errno = NlMsgErr::from_bytes(message.payload).map_or(libc_eproto(), |error| {
                    error.error.checked_neg().unwrap_or(i32::MAX)
                });
                return if errno == 0 {
                    Outcome::Done
                } else {
                    Outcome::Refused(errno)
                };
            }
            kind => part(kind, message.payload),
        }
    }
    Outcome::More
}

/// `EPROTO`, for a reply that cannot be walked.
const fn libc_eproto() -> i32 {
    71
}

/// The link in an `RTM_NEWLINK` part of a dump.
#[must_use]
pub fn link_of(payload: &[u8]) -> Option<Link> {
    let body = IfInfoMsg::from_bytes(payload)?;
    let message = ferrix_netlink::Message {
        header: NlMsgHdr {
            len: 0,
            kind: RTM_NEWLINK,
            flags: 0,
            seq: 0,
            pid: 0,
        },
        payload,
    };
    let attributes = message.attributes(IfInfoMsg::SIZE);
    let name = attributes.find(IFLA_IFNAME)?.as_name();
    let mac = attributes
        .find(IFLA_ADDRESS)
        .and_then(|found| <[u8; 6]>::try_from(found.as_bytes()).ok());
    Some(Link {
        index: u32::try_from(body.index).ok()?,
        name: String::from_utf8_lossy(name).into_owned(),
        mac,
    })
}

/// The IPv4 address in an `RTM_NEWADDR` part of a dump.
#[must_use]
pub fn address_of(payload: &[u8]) -> Option<Assigned> {
    let body = IfAddrMsg::from_bytes(payload)?;
    if body.family != inet() {
        return None;
    }
    let message = ferrix_netlink::Message {
        header: NlMsgHdr {
            len: 0,
            kind: RTM_NEWADDR,
            flags: 0,
            seq: 0,
            pid: 0,
        },
        payload,
    };
    let attributes = message.attributes(IfAddrMsg::SIZE);
    let carried = attributes
        .find(IFA_LOCAL)
        .or_else(|| attributes.find(IFA_ADDRESS))?
        .as_address()?;
    let Address::V4(octets) = carried else {
        return None;
    };
    Some(Assigned {
        index: body.index,
        address: Ipv4Addr::from(octets),
        prefix: body.prefix_len,
    })
}
