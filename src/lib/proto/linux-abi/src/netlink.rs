//! Netlink and its routing family: the numbers and fixed headers `ip`,
//! `ifconfig`, `udhcpc` and a C library's `getifaddrs` exchange with the
//! kernel over an `AF_NETLINK` socket.
//!
//! # Where the numbers come from
//!
//! `include/uapi/linux/netlink.h`, `rtnetlink.h`, `if_link.h`, `if_addr.h`,
//! `neighbour.h`, `if.h` and `if_arp.h`, which all three architectures take
//! unchanged. [`SOL_NETLINK`] is from the kernel's non-UAPI
//! `include/linux/socket.h`, as C libraries copy it.
//!
//! # Layouts without a width
//!
//! Netlink was designed to be read the same by 32- and 64-bit programs: every
//! header here is built from 8-, 16- and 32-bit fields, so each is one layout
//! on every architecture Ferrix runs, and the codecs take bytes alone. The
//! fields are host order, which under Ferrix is little-endian -- unlike the
//! addresses the attributes carry, which are network order and which these
//! headers do not read.
//!
//! # What is not here
//!
//! Walking a buffer of messages, and the attributes after each header, is
//! `src/lib/network/netlink`'s: this crate names the numbers and lays out the fixed
//! parts, and [`nlmsg_align`] and [`nla_align`] are the only arithmetic.

use crate::socket::{AF_NETLINK, AddressError};
use crate::wire;

// ---------------------------------------------------------------------------
// Protocols and socket options
// ---------------------------------------------------------------------------

/// `socket(AF_NETLINK, _, NETLINK_ROUTE)`: links, addresses, routes and
/// neighbours.
pub const NETLINK_ROUTE: i32 = 0;
/// `socket(AF_NETLINK, _, NETLINK_KOBJECT_UEVENT)`: device events, as `mdev`
/// and `udev` listen for them.
pub const NETLINK_KOBJECT_UEVENT: i32 = 15;

/// The level of netlink's own socket options.
pub const SOL_NETLINK: i32 = 270;
/// Join a multicast group by number, beyond the 32 `nl_groups` can name.
pub const NETLINK_ADD_MEMBERSHIP: i32 = 1;
/// Leave a multicast group.
pub const NETLINK_DROP_MEMBERSHIP: i32 = 2;
/// Receive the group a message came from as a control message.
pub const NETLINK_PKTINFO: i32 = 3;
/// Leave the request out of an error acknowledgement, keeping only its header.
pub const NETLINK_CAP_ACK: i32 = 10;
/// Accept extended acknowledgements, with attributes after the error.
pub const NETLINK_EXT_ACK: i32 = 11;
/// Check a dump request's header strictly and honour its filters.
pub const NETLINK_GET_STRICT_CHK: i32 = 12;

// ---------------------------------------------------------------------------
// Message types and flags
// ---------------------------------------------------------------------------

/// Header and attribute alignment: messages pad to a multiple of four bytes.
pub const NLMSG_ALIGNTO: usize = 4;
/// Nothing: a message to skip.
pub const NLMSG_NOOP: u16 = 1;
/// An error or acknowledgement, carrying an [`NlMsgErr`].
pub const NLMSG_ERROR: u16 = 2;
/// The end of a multipart dump.
pub const NLMSG_DONE: u16 = 3;
/// Data was lost.
pub const NLMSG_OVERRUN: u16 = 4;
/// The lowest type a family defines; those below are netlink's own.
pub const NLMSG_MIN_TYPE: u16 = 16;

/// A request, as opposed to a notification.
pub const NLM_F_REQUEST: u16 = 0x1;
/// Part of a multipart reply, which [`NLMSG_DONE`] ends.
pub const NLM_F_MULTI: u16 = 0x2;
/// Answer with an acknowledgement.
pub const NLM_F_ACK: u16 = 0x4;
/// Echo the request back.
pub const NLM_F_ECHO: u16 = 0x8;
/// The dump was interrupted by a change and may be inconsistent.
pub const NLM_F_DUMP_INTR: u16 = 0x10;
/// The dump was filtered as the request asked.
pub const NLM_F_DUMP_FILTERED: u16 = 0x20;
/// Get: return the whole table, not one entry.
pub const NLM_F_ROOT: u16 = 0x100;
/// Get: return every entry that matches.
pub const NLM_F_MATCH: u16 = 0x200;
/// Get: return an atomic snapshot.
pub const NLM_F_ATOMIC: u16 = 0x400;
/// Get: a dump, [`NLM_F_ROOT`] and [`NLM_F_MATCH`] together.
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;
/// New: replace an existing entry.
pub const NLM_F_REPLACE: u16 = 0x100;
/// New: fail if the entry exists.
pub const NLM_F_EXCL: u16 = 0x200;
/// New: create the entry if it does not exist.
pub const NLM_F_CREATE: u16 = 0x400;
/// New: add to the end of the list.
pub const NLM_F_APPEND: u16 = 0x800;
/// Delete: do not delete recursively.
pub const NLM_F_NONREC: u16 = 0x100;
/// Delete: delete several entries.
pub const NLM_F_BULK: u16 = 0x200;
/// Acknowledgement: the request was left out, per [`NETLINK_CAP_ACK`].
pub const NLM_F_CAPPED: u16 = 0x100;
/// Acknowledgement: extended attributes follow, per [`NETLINK_EXT_ACK`].
pub const NLM_F_ACK_TLVS: u16 = 0x200;

/// Attribute alignment, the same four bytes as a message's.
pub const NLA_ALIGNTO: usize = 4;
/// An attribute holding further attributes.
pub const NLA_F_NESTED: u16 = 0x8000;
/// An attribute whose payload is network order.
pub const NLA_F_NET_BYTEORDER: u16 = 0x4000;
/// The bits of an attribute's type that are the type.
pub const NLA_TYPE_MASK: u16 = !(NLA_F_NESTED | NLA_F_NET_BYTEORDER);

/// `NLMSG_ALIGN`: `len` rounded up to a multiple of four.
/// It saturates rather than wrapping, so a length within four of `usize::MAX`
/// is never made small.
#[must_use]
pub const fn nlmsg_align(len: usize) -> usize {
    len.div_ceil(NLMSG_ALIGNTO).saturating_mul(NLMSG_ALIGNTO)
}

/// `NLA_ALIGN`: the same rounding as [`nlmsg_align`], named for attributes.
#[must_use]
pub const fn nla_align(len: usize) -> usize {
    nlmsg_align(len)
}

// ---------------------------------------------------------------------------
// struct sockaddr_nl
// ---------------------------------------------------------------------------

/// A netlink socket address, `struct sockaddr_nl`: 12 bytes, the family, two
/// of padding, a port identifier and a group mask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetlinkAddress {
    /// `nl_pid`: the socket's port identifier, 0 for the kernel.
    pub pid: u32,
    /// `nl_groups`: the multicast groups, one bit per group below 33.
    pub groups: u32,
}

impl NetlinkAddress {
    /// Bytes in the structure.
    pub const SIZE: usize = 12;

    /// Read the address in the first `addr_len` bytes of `bytes`. Linux
    /// refuses a shorter length or another family with `EINVAL`, and does
    /// not look at `nl_pad`.
    pub fn parse(bytes: &[u8], addr_len: usize) -> Result<NetlinkAddress, AddressError> {
        if addr_len > crate::inet::SOCKADDR_STORAGE_SIZE {
            return Err(AddressError::TooLong);
        }
        let bytes = bytes.get(..addr_len).ok_or(AddressError::TooShort)?;
        let family = wire::array(bytes, 0).ok_or(AddressError::TooShort)?;
        if addr_len < Self::SIZE {
            return Err(AddressError::TooShort);
        }
        if u16::from_le_bytes(family) != AF_NETLINK {
            return Err(AddressError::WrongFamily);
        }
        Ok(NetlinkAddress {
            pid: le32(bytes, 4).ok_or(AddressError::TooShort)?,
            groups: le32(bytes, 8).ok_or(AddressError::TooShort)?,
        })
    }

    /// The structure's bytes, family included and padding zero.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0_u8; Self::SIZE];
        let fields: [(usize, &[u8]); 3] = [
            (0, &AF_NETLINK.to_le_bytes()),
            (4, &self.pid.to_le_bytes()),
            (8, &self.groups.to_le_bytes()),
        ];
        for (at, field) in fields {
            // Every field lies inside the twelve bytes.
            let _ = wire::put(&mut bytes, at, field);
        }
        bytes
    }
}

// ---------------------------------------------------------------------------
// struct nlmsghdr and struct nlmsgerr
// ---------------------------------------------------------------------------

/// The header every netlink message starts with, `struct nlmsghdr`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NlMsgHdr {
    /// `nlmsg_len`: bytes in the message, this header included, padding not.
    pub len: u32,
    /// `nlmsg_type`, renamed because `type` is a keyword: an `NLMSG_` or
    /// `RTM_` number.
    pub kind: u16,
    /// `nlmsg_flags`: the `NLM_F_` bits.
    pub flags: u16,
    /// `nlmsg_seq`: the sender's sequence number, echoed in the reply.
    pub seq: u32,
    /// `nlmsg_pid`: the sending socket's port identifier.
    pub pid: u32,
}

impl NlMsgHdr {
    /// Bytes in the header.
    pub const SIZE: usize = 16;

    /// Read a header from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<NlMsgHdr> {
        Some(NlMsgHdr {
            len: le32(bytes, 0)?,
            kind: le16(bytes, 4)?,
            flags: le16(bytes, 6)?,
            seq: le32(bytes, 8)?,
            pid: le32(bytes, 12)?,
        })
    }

    /// The header's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0_u8; Self::SIZE];
        let fields: [(usize, &[u8]); 5] = [
            (0, &self.len.to_le_bytes()),
            (4, &self.kind.to_le_bytes()),
            (6, &self.flags.to_le_bytes()),
            (8, &self.seq.to_le_bytes()),
            (12, &self.pid.to_le_bytes()),
        ];
        for (at, field) in fields {
            // Every field lies inside the sixteen bytes.
            let _ = wire::put(&mut bytes, at, field);
        }
        bytes
    }
}

/// The payload of an [`NLMSG_ERROR`] message, `struct nlmsgerr`: a negative
/// `errno`, or 0 for an acknowledgement, and the header of the request it
/// answers. The request's payload may follow, unless [`NLM_F_CAPPED`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NlMsgErr {
    /// `error`: `-errno`, or 0.
    pub error: i32,
    /// `msg`: the request's header.
    pub msg: NlMsgHdr,
}

impl NlMsgErr {
    /// Bytes in the structure.
    pub const SIZE: usize = 20;

    /// Read the structure from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<NlMsgErr> {
        Some(NlMsgErr {
            error: wire::array(bytes, 0).map(i32::from_le_bytes)?,
            msg: NlMsgHdr::from_bytes(bytes.get(4..)?)?,
        })
    }

    /// The structure's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0_u8; Self::SIZE];
        let _ = wire::put(&mut bytes, 0, &self.error.to_le_bytes());
        let _ = wire::put(&mut bytes, 4, &self.msg.to_bytes());
        bytes
    }
}

/// An attribute's header, `struct nlattr` and routing's `struct rtattr`, which
/// are the same four bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NlAttr {
    /// `nla_len`: bytes in the attribute, this header included, padding not.
    pub len: u16,
    /// `nla_type`: the attribute's number, with the `NLA_F_` bits.
    pub kind: u16,
}

impl NlAttr {
    /// Bytes in the header.
    pub const SIZE: usize = 4;

    /// Read a header from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<NlAttr> {
        Some(NlAttr {
            len: le16(bytes, 0)?,
            kind: le16(bytes, 2)?,
        })
    }

    /// The header's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let [a, b] = self.len.to_le_bytes();
        let [c, d] = self.kind.to_le_bytes();
        [a, b, c, d]
    }
}

// ---------------------------------------------------------------------------
// Routing messages
// ---------------------------------------------------------------------------

/// Create or change a link.
pub const RTM_NEWLINK: u16 = 16;
/// Delete a link.
pub const RTM_DELLINK: u16 = 17;
/// Get one link, or dump them all.
pub const RTM_GETLINK: u16 = 18;
/// Change a link's settings.
pub const RTM_SETLINK: u16 = 19;
/// Add an address.
pub const RTM_NEWADDR: u16 = 20;
/// Delete an address.
pub const RTM_DELADDR: u16 = 21;
/// Get or dump addresses.
pub const RTM_GETADDR: u16 = 22;
/// Add a route.
pub const RTM_NEWROUTE: u16 = 24;
/// Delete a route.
pub const RTM_DELROUTE: u16 = 25;
/// Get or dump routes.
pub const RTM_GETROUTE: u16 = 26;
/// Add a neighbour entry.
pub const RTM_NEWNEIGH: u16 = 28;
/// Delete a neighbour entry.
pub const RTM_DELNEIGH: u16 = 29;
/// Get or dump neighbour entries.
pub const RTM_GETNEIGH: u16 = 30;

/// `nl_groups` bit: link changes.
pub const RTMGRP_LINK: u32 = 0x1;
/// `nl_groups` bit: neighbour changes.
pub const RTMGRP_NEIGH: u32 = 0x4;
/// `nl_groups` bit: IPv4 address changes.
pub const RTMGRP_IPV4_IFADDR: u32 = 0x10;
/// `nl_groups` bit: IPv4 route changes.
pub const RTMGRP_IPV4_ROUTE: u32 = 0x40;
/// `nl_groups` bit: IPv6 address changes.
pub const RTMGRP_IPV6_IFADDR: u32 = 0x100;
/// `nl_groups` bit: IPv6 route changes.
pub const RTMGRP_IPV6_ROUTE: u32 = 0x400;

/// The header of a link message, `struct ifinfomsg`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfInfoMsg {
    /// `ifi_family`: `AF_UNSPEC` for links.
    pub family: u8,
    /// `ifi_type`: the hardware type, an `ARPHRD_` number.
    pub kind: u16,
    /// `ifi_index`: the link's index, or 0 for any.
    pub index: i32,
    /// `ifi_flags`: the `IFF_` bits.
    pub flags: u32,
    /// `ifi_change`: which of `flags` a request changes.
    pub change: u32,
}

impl IfInfoMsg {
    /// Bytes in the header.
    pub const SIZE: usize = 16;

    /// Read a header from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<IfInfoMsg> {
        Some(IfInfoMsg {
            family: *bytes.first()?,
            kind: le16(bytes, 2)?,
            index: wire::array(bytes, 4).map(i32::from_le_bytes)?,
            flags: le32(bytes, 8)?,
            change: le32(bytes, 12)?,
        })
    }

    /// The header's bytes, padding zero.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0_u8; Self::SIZE];
        let fields: [(usize, &[u8]); 5] = [
            (0, &[self.family]),
            (2, &self.kind.to_le_bytes()),
            (4, &self.index.to_le_bytes()),
            (8, &self.flags.to_le_bytes()),
            (12, &self.change.to_le_bytes()),
        ];
        for (at, field) in fields {
            // Every field lies inside the sixteen bytes.
            let _ = wire::put(&mut bytes, at, field);
        }
        bytes
    }
}

/// The header of an address message, `struct ifaddrmsg`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfAddrMsg {
    /// `ifa_family`: `AF_INET` or `AF_INET6`.
    pub family: u8,
    /// `ifa_prefixlen`: the prefix length.
    pub prefix_len: u8,
    /// `ifa_flags`: the low eight `IFA_F_` bits; [`IFA_FLAGS`] carries all.
    pub flags: u8,
    /// `ifa_scope`: an `RT_SCOPE_` number.
    pub scope: u8,
    /// `ifa_index`: the link the address is on.
    pub index: u32,
}

impl IfAddrMsg {
    /// Bytes in the header.
    pub const SIZE: usize = 8;

    /// Read a header from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<IfAddrMsg> {
        let [family, prefix_len, flags, scope] = wire::array(bytes, 0)?;
        Some(IfAddrMsg {
            family,
            prefix_len,
            flags,
            scope,
            index: le32(bytes, 4)?,
        })
    }

    /// The header's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let [a, b, c, d] = self.index.to_le_bytes();
        [
            self.family,
            self.prefix_len,
            self.flags,
            self.scope,
            a,
            b,
            c,
            d,
        ]
    }
}

/// The header of a route message, `struct rtmsg`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtMsg {
    /// `rtm_family`.
    pub family: u8,
    /// `rtm_dst_len`: the destination prefix length.
    pub dst_len: u8,
    /// `rtm_src_len`: the source prefix length.
    pub src_len: u8,
    /// `rtm_tos`: the type of service matched.
    pub tos: u8,
    /// `rtm_table`: an `RT_TABLE_` number; [`RTA_TABLE`] carries larger ones.
    pub table: u8,
    /// `rtm_protocol`: who installed the route, an `RTPROT_` number.
    pub protocol: u8,
    /// `rtm_scope`: an `RT_SCOPE_` number.
    pub scope: u8,
    /// `rtm_type`: an `RTN_` number.
    pub kind: u8,
    /// `rtm_flags`.
    pub flags: u32,
}

impl RtMsg {
    /// Bytes in the header.
    pub const SIZE: usize = 12;

    /// Read a header from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<RtMsg> {
        let [family, dst_len, src_len, tos, table, protocol, scope, kind] = wire::array(bytes, 0)?;
        Some(RtMsg {
            family,
            dst_len,
            src_len,
            tos,
            table,
            protocol,
            scope,
            kind,
            flags: le32(bytes, 8)?,
        })
    }

    /// The header's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let [a, b, c, d] = self.flags.to_le_bytes();
        [
            self.family,
            self.dst_len,
            self.src_len,
            self.tos,
            self.table,
            self.protocol,
            self.scope,
            self.kind,
            a,
            b,
            c,
            d,
        ]
    }
}

/// The header of a neighbour message, `struct ndmsg`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NdMsg {
    /// `ndm_family`.
    pub family: u8,
    /// `ndm_ifindex`: the link.
    pub ifindex: i32,
    /// `ndm_state`: the `NUD_` bits.
    pub state: u16,
    /// `ndm_flags`.
    pub flags: u8,
    /// `ndm_type`: an `RTN_` number.
    pub kind: u8,
}

impl NdMsg {
    /// Bytes in the header.
    pub const SIZE: usize = 12;

    /// Read a header from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<NdMsg> {
        let [flags, kind] = wire::array(bytes, 10)?;
        Some(NdMsg {
            family: *bytes.first()?,
            ifindex: wire::array(bytes, 4).map(i32::from_le_bytes)?,
            state: le16(bytes, 8)?,
            flags,
            kind,
        })
    }

    /// The header's bytes, padding zero.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let [a, b, c, d] = self.ifindex.to_le_bytes();
        let [e, f] = self.state.to_le_bytes();
        [
            self.family,
            0,
            0,
            0,
            a,
            b,
            c,
            d,
            e,
            f,
            self.flags,
            self.kind,
        ]
    }
}

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

/// Link attribute: none.
pub const IFLA_UNSPEC: u16 = 0;
/// Link attribute: the hardware address.
pub const IFLA_ADDRESS: u16 = 1;
/// Link attribute: the hardware broadcast address.
pub const IFLA_BROADCAST: u16 = 2;
/// Link attribute: the name, NUL-terminated, at most [`IFNAMSIZ`] bytes.
pub const IFLA_IFNAME: u16 = 3;
/// Link attribute: the MTU, a `u32`.
pub const IFLA_MTU: u16 = 4;
/// Link attribute: the index of the link this one is stacked on.
pub const IFLA_LINK: u16 = 5;
/// Link attribute: the queueing discipline's name.
pub const IFLA_QDISC: u16 = 6;
/// Link attribute: `struct rtnl_link_stats`, 32-bit counters.
pub const IFLA_STATS: u16 = 7;
/// Link attribute: the transmit queue length.
pub const IFLA_TXQLEN: u16 = 13;
/// Link attribute: the RFC 2863 operational state, one byte.
pub const IFLA_OPERSTATE: u16 = 16;
/// Link attribute: the link mode, one byte.
pub const IFLA_LINKMODE: u16 = 17;
/// Link attribute: `struct rtnl_link_stats64`.
pub const IFLA_STATS64: u16 = 23;
/// Link attribute: nested per-family settings.
pub const IFLA_AF_SPEC: u16 = 26;
/// Link attribute: the group the link belongs to.
pub const IFLA_GROUP: u16 = 27;
/// Link attribute: whether the carrier is up, one byte.
pub const IFLA_CARRIER: u16 = 33;
/// Link attribute: nested, what kind of link this is and its settings.
pub const IFLA_LINKINFO: u16 = 18;
/// Link attribute: the process whose network namespace the link goes to, a
/// `u32`.
pub const IFLA_NET_NS_PID: u16 = 19;
/// Link attribute: the descriptor of the network namespace the link goes to, a
/// `u32`.
pub const IFLA_NET_NS_FD: u16 = 28;

/// Inside `IFLA_LINKINFO`: the kind's name, `veth` for a virtual pair.
pub const IFLA_INFO_KIND: u16 = 1;
/// Inside `IFLA_LINKINFO`: the kind's own attributes, nested.
pub const IFLA_INFO_DATA: u16 = 2;
/// Inside a `veth`'s `IFLA_INFO_DATA`: the other end, as an `ifinfomsg` and
/// the attributes of a link.
pub const VETH_INFO_PEER: u16 = 1;

/// Address attribute: none.
pub const IFA_UNSPEC: u16 = 0;
/// Address attribute: the prefix address, or the peer's on a point-to-point
/// link.
pub const IFA_ADDRESS: u16 = 1;
/// Address attribute: the local address.
pub const IFA_LOCAL: u16 = 2;
/// Address attribute: the label, a name.
pub const IFA_LABEL: u16 = 3;
/// Address attribute: the broadcast address.
pub const IFA_BROADCAST: u16 = 4;
/// Address attribute: an anycast address.
pub const IFA_ANYCAST: u16 = 5;
/// Address attribute: `struct ifa_cacheinfo`, lifetimes.
pub const IFA_CACHEINFO: u16 = 6;
/// Address attribute: a multicast address.
pub const IFA_MULTICAST: u16 = 7;
/// Address attribute: every `IFA_F_` bit, a `u32`.
pub const IFA_FLAGS: u16 = 8;

/// Address flag: a secondary address on its prefix.
pub const IFA_F_SECONDARY: u32 = 0x01;
/// Address flag: skip duplicate address detection.
pub const IFA_F_NODAD: u32 = 0x02;
/// Address flag: duplicate address detection has not finished.
pub const IFA_F_TENTATIVE: u32 = 0x40;
/// Address flag: configured, not learned.
pub const IFA_F_PERMANENT: u32 = 0x80;

/// Route attribute: none.
pub const RTA_UNSPEC: u16 = 0;
/// Route attribute: the destination.
pub const RTA_DST: u16 = 1;
/// Route attribute: the source prefix.
pub const RTA_SRC: u16 = 2;
/// Route attribute: the input link.
pub const RTA_IIF: u16 = 3;
/// Route attribute: the output link.
pub const RTA_OIF: u16 = 4;
/// Route attribute: the gateway.
pub const RTA_GATEWAY: u16 = 5;
/// Route attribute: the metric.
pub const RTA_PRIORITY: u16 = 6;
/// Route attribute: the preferred source address.
pub const RTA_PREFSRC: u16 = 7;
/// Route attribute: nested `RTAX_` metrics.
pub const RTA_METRICS: u16 = 8;
/// Route attribute: next hops of a multipath route.
pub const RTA_MULTIPATH: u16 = 9;
/// Route attribute: the table, a `u32`, for tables above 255.
pub const RTA_TABLE: u16 = 15;

/// Route type: none.
pub const RTN_UNSPEC: u8 = 0;
/// Route type: a gateway or direct route.
pub const RTN_UNICAST: u8 = 1;
/// Route type: accept locally.
pub const RTN_LOCAL: u8 = 2;
/// Route type: accept locally as broadcast, send as broadcast.
pub const RTN_BROADCAST: u8 = 3;

/// Table: none.
pub const RT_TABLE_UNSPEC: u8 = 0;
/// Table: the main table, which `ip route` shows.
pub const RT_TABLE_MAIN: u8 = 254;
/// Table: local and broadcast addresses.
pub const RT_TABLE_LOCAL: u8 = 255;

/// Scope: anywhere.
pub const RT_SCOPE_UNIVERSE: u8 = 0;
/// Scope: this link.
pub const RT_SCOPE_LINK: u8 = 253;
/// Scope: this host.
pub const RT_SCOPE_HOST: u8 = 254;

/// Route origin: unknown.
pub const RTPROT_UNSPEC: u8 = 0;
/// Route origin: the kernel, for an address's own prefix.
pub const RTPROT_KERNEL: u8 = 2;
/// Route origin: installed at boot.
pub const RTPROT_BOOT: u8 = 3;
/// Route origin: the administrator.
pub const RTPROT_STATIC: u8 = 4;
/// Route origin: router advertisements.
pub const RTPROT_RA: u8 = 9;
/// Route origin: a DHCP client.
pub const RTPROT_DHCP: u8 = 16;

/// Neighbour attribute: none.
pub const NDA_UNSPEC: u16 = 0;
/// Neighbour attribute: the network address.
pub const NDA_DST: u16 = 1;
/// Neighbour attribute: the link-layer address.
pub const NDA_LLADDR: u16 = 2;
/// Neighbour attribute: `struct nda_cacheinfo`.
pub const NDA_CACHEINFO: u16 = 3;
/// Neighbour attribute: probes sent.
pub const NDA_PROBES: u16 = 4;

/// Neighbour state: none.
pub const NUD_NONE: u16 = 0x00;
/// Neighbour state: resolving.
pub const NUD_INCOMPLETE: u16 = 0x01;
/// Neighbour state: confirmed reachable.
pub const NUD_REACHABLE: u16 = 0x02;
/// Neighbour state: not confirmed lately.
pub const NUD_STALE: u16 = 0x04;
/// Neighbour state: waiting before probing.
pub const NUD_DELAY: u16 = 0x08;
/// Neighbour state: probing.
pub const NUD_PROBE: u16 = 0x10;
/// Neighbour state: resolution failed.
pub const NUD_FAILED: u16 = 0x20;
/// Neighbour state: no resolution needed.
pub const NUD_NOARP: u16 = 0x40;
/// Neighbour state: configured, never expires.
pub const NUD_PERMANENT: u16 = 0x80;

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// Link flag: administratively up.
pub const IFF_UP: u32 = 1 << 0;
/// Link flag: has a broadcast address.
pub const IFF_BROADCAST: u32 = 1 << 1;
/// Link flag: debugging.
pub const IFF_DEBUG: u32 = 1 << 2;
/// Link flag: the loopback link.
pub const IFF_LOOPBACK: u32 = 1 << 3;
/// Link flag: point-to-point.
pub const IFF_POINTOPOINT: u32 = 1 << 4;
/// Link flag: operationally up.
pub const IFF_RUNNING: u32 = 1 << 6;
/// Link flag: no address resolution.
pub const IFF_NOARP: u32 = 1 << 7;
/// Link flag: receives every packet.
pub const IFF_PROMISC: u32 = 1 << 8;
/// Link flag: receives every multicast packet.
pub const IFF_ALLMULTI: u32 = 1 << 9;
/// Link flag: supports multicast.
pub const IFF_MULTICAST: u32 = 1 << 12;
/// Link flag: the carrier is up.
pub const IFF_LOWER_UP: u32 = 1 << 16;

/// Bytes in a link name, the terminator included.
pub const IFNAMSIZ: usize = 16;

/// Hardware type: Ethernet.
pub const ARPHRD_ETHER: u16 = 1;
/// Hardware type: loopback.
pub const ARPHRD_LOOPBACK: u16 = 772;

/// The little-endian `u16` at `at`.
fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    wire::array(bytes, at).map(u16::from_le_bytes)
}

/// The little-endian `u32` at `at`.
fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    wire::array(bytes, at).map(u32::from_le_bytes)
}
