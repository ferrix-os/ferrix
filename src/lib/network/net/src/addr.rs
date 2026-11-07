//! Addresses, prefixes and endpoints.
//!
//! An address here is the bytes as they appear on the wire, never a number:
//! `10.0.2.15` is `[10, 0, 2, 15]` and not `0x0A00_020F`, because the moment it
//! is a number somebody has to remember which end it was written from. The
//! conversions to and from a number exist, named, for the two places that need
//! them -- `sockaddr_in`'s `s_addr` and `/proc/net`'s hexadecimal.

use core::fmt;

/// A 32-bit IPv4 address, in the order it is written and sent.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ipv4([u8; 4]);

impl Ipv4 {
    /// `0.0.0.0`: "any address" to a bind, "this host" in a route.
    pub const UNSPECIFIED: Ipv4 = Ipv4([0, 0, 0, 0]);
    /// `127.0.0.1`.
    pub const LOOPBACK: Ipv4 = Ipv4([127, 0, 0, 1]);
    /// `255.255.255.255`.
    pub const BROADCAST: Ipv4 = Ipv4([255, 255, 255, 255]);

    /// The address made of these four bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 4]) -> Ipv4 {
        Ipv4(bytes)
    }

    /// The four bytes.
    #[must_use]
    pub const fn octets(self) -> [u8; 4] {
        self.0
    }

    /// The address as the number `sockaddr_in` carries, most significant octet
    /// first.
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    /// The address that number names.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Ipv4 {
        Ipv4(bits.to_be_bytes())
    }

    /// Whether this is `0.0.0.0`.
    #[must_use]
    pub const fn is_unspecified(self) -> bool {
        self.to_bits() == 0
    }

    /// Whether this is in `127.0.0.0/8`.
    #[must_use]
    pub const fn is_loopback(self) -> bool {
        self.0[0] == 127
    }

    /// Whether this is in `224.0.0.0/4`.
    #[must_use]
    pub const fn is_multicast(self) -> bool {
        self.0[0] & 0xF0 == 0xE0
    }

    /// Whether this is `255.255.255.255`.
    #[must_use]
    pub const fn is_broadcast(self) -> bool {
        self.to_bits() == u32::MAX
    }

    /// Whether this is in `169.254.0.0/16`, which a host gives itself when no
    /// configuration arrived.
    #[must_use]
    pub const fn is_link_local(self) -> bool {
        self.0[0] == 169 && self.0[1] == 254
    }

    /// The Ethernet address a multicast address maps to: RFC 1112's
    /// `01:00:5e` and the low 23 bits.
    #[must_use]
    pub const fn multicast_mac(self) -> [u8; 6] {
        [0x01, 0x00, 0x5E, self.0[1] & 0x7F, self.0[2], self.0[3]]
    }
}

impl fmt::Display for Ipv4 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(formatter, "{a}.{b}.{c}.{d}")
    }
}

impl fmt::Debug for Ipv4 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// A 128-bit IPv6 address, in the order it is written and sent.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ipv6([u8; 16]);

impl Ipv6 {
    /// `::`.
    pub const UNSPECIFIED: Ipv6 = Ipv6([0; 16]);
    /// `::1`.
    pub const LOOPBACK: Ipv6 = Ipv6([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    /// `ff02::1`, every node on the link.
    pub const ALL_NODES: Ipv6 = Ipv6([0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    /// `ff02::2`, every router on the link.
    pub const ALL_ROUTERS: Ipv6 = Ipv6([0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);

    /// The address made of these sixteen bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Ipv6 {
        Ipv6(bytes)
    }

    /// The sixteen bytes.
    #[must_use]
    pub const fn octets(self) -> [u8; 16] {
        self.0
    }

    /// Whether this is `::`.
    #[must_use]
    pub fn is_unspecified(self) -> bool {
        self.0 == [0; 16]
    }

    /// Whether this is `::1`.
    #[must_use]
    pub fn is_loopback(self) -> bool {
        self == Ipv6::LOOPBACK
    }

    /// Whether this is in `ff00::/8`.
    #[must_use]
    pub const fn is_multicast(self) -> bool {
        self.0[0] == 0xFF
    }

    /// Whether this is in `fe80::/10`.
    #[must_use]
    pub const fn is_link_local(self) -> bool {
        self.0[0] == 0xFE && self.0[1] & 0xC0 == 0x80
    }

    /// Whether this is in `::ffff:0:0/96`, an IPv4 address carried as an IPv6
    /// one.
    #[must_use]
    pub fn is_v4_mapped(self) -> bool {
        let (prefix, suffix) = self.0.split_at(12);
        prefix == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF] && suffix.len() == 4
    }

    /// The IPv4 address this carries, if it carries one.
    #[must_use]
    pub fn v4_mapped(self) -> Option<Ipv4> {
        if !self.is_v4_mapped() {
            return None;
        }
        let mut octets = [0_u8; 4];
        for (slot, byte) in octets.iter_mut().zip(self.0.iter().skip(12)) {
            *slot = *byte;
        }
        Some(Ipv4::new(octets))
    }

    /// The IPv6 address that carries `address` in `::ffff:0:0/96`.
    #[must_use]
    pub const fn from_v4_mapped(address: Ipv4) -> Ipv6 {
        let [a, b, c, d] = address.octets();
        Ipv6([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, a, b, c, d])
    }

    /// `ff02::1:ffXX:XXXX` for this address: the group a neighbour
    /// solicitation for it is sent to.
    #[must_use]
    pub const fn solicited_node(self) -> Ipv6 {
        Ipv6([
            0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xFF, self.0[13], self.0[14], self.0[15],
        ])
    }

    /// The Ethernet address a multicast address maps to: RFC 2464's `33:33`
    /// and the low 32 bits.
    #[must_use]
    pub const fn multicast_mac(self) -> [u8; 6] {
        [0x33, 0x33, self.0[12], self.0[13], self.0[14], self.0[15]]
    }
}

impl fmt::Display for Ipv6 {
    /// RFC 5952: lower case, the longest run of zero groups collapsed once,
    /// and a mapped IPv4 address written as one.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(four) = self.v4_mapped() {
            return write!(formatter, "::ffff:{four}");
        }
        let mut groups = [0_u16; 8];
        for (slot, pair) in groups.iter_mut().zip(self.0.as_chunks::<2>().0) {
            *slot = u16::from_be_bytes(*pair);
        }
        let (start, len) = longest_zero_run(&groups);
        let mut first = true;
        let mut index = 0;
        while index < groups.len() {
            if len > 1 && index == start {
                write!(formatter, "::")?;
                index += len;
                first = true;
                continue;
            }
            if !first {
                write!(formatter, ":")?;
            }
            write!(formatter, "{:x}", groups.get(index).copied().unwrap_or(0))?;
            first = false;
            index += 1;
        }
        Ok(())
    }
}

impl fmt::Debug for Ipv6 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// Where the longest run of zero groups starts, and how long it is.
fn longest_zero_run(groups: &[u16; 8]) -> (usize, usize) {
    let (mut best_at, mut best_len, mut at, mut len) = (0, 0, 0, 0);
    for (index, group) in groups.iter().enumerate() {
        if *group == 0 {
            if len == 0 {
                at = index;
            }
            len += 1;
            if len > best_len {
                best_at = at;
                best_len = len;
            }
        } else {
            len = 0;
        }
    }
    (best_at, best_len)
}

/// An address of either family.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum IpAddress {
    /// An IPv4 address.
    V4(Ipv4),
    /// An IPv6 address.
    V6(Ipv6),
}

impl IpAddress {
    /// The unspecified address of the same family.
    #[must_use]
    pub const fn unspecified(self) -> IpAddress {
        match self {
            IpAddress::V4(_) => IpAddress::V4(Ipv4::UNSPECIFIED),
            IpAddress::V6(_) => IpAddress::V6(Ipv6::UNSPECIFIED),
        }
    }

    /// Whether this is the family's unspecified address.
    #[must_use]
    pub fn is_unspecified(self) -> bool {
        match self {
            IpAddress::V4(address) => address.is_unspecified(),
            IpAddress::V6(address) => address.is_unspecified(),
        }
    }

    /// Whether this is a loopback address.
    #[must_use]
    pub fn is_loopback(self) -> bool {
        match self {
            IpAddress::V4(address) => address.is_loopback(),
            IpAddress::V6(address) => address.is_loopback(),
        }
    }

    /// Whether this is a multicast address.
    #[must_use]
    pub const fn is_multicast(self) -> bool {
        match self {
            IpAddress::V4(address) => address.is_multicast(),
            IpAddress::V6(address) => address.is_multicast(),
        }
    }

    /// Whether this is IPv4.
    #[must_use]
    pub const fn is_v4(self) -> bool {
        matches!(self, IpAddress::V4(_))
    }

    /// How many bits an address of this family has.
    #[must_use]
    pub const fn bit_length(self) -> u8 {
        match self {
            IpAddress::V4(_) => 32,
            IpAddress::V6(_) => 128,
        }
    }

    /// The address's bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            IpAddress::V4(address) => &address.0,
            IpAddress::V6(address) => &address.0,
        }
    }
}

impl From<Ipv4> for IpAddress {
    fn from(address: Ipv4) -> IpAddress {
        IpAddress::V4(address)
    }
}

impl From<Ipv6> for IpAddress {
    fn from(address: Ipv6) -> IpAddress {
        IpAddress::V6(address)
    }
}

impl fmt::Display for IpAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpAddress::V4(address) => fmt::Display::fmt(address, formatter),
            IpAddress::V6(address) => fmt::Display::fmt(address, formatter),
        }
    }
}

/// An address and a prefix length: a network, or an address configured on an
/// interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IpCidr {
    /// The address.
    address: IpAddress,
    /// How many leading bits are the network's.
    prefix_len: u8,
}

impl IpCidr {
    /// A prefix, with the length clamped to the family's width.
    #[must_use]
    pub fn new(address: IpAddress, prefix_len: u8) -> IpCidr {
        IpCidr {
            address,
            prefix_len: prefix_len.min(address.bit_length()),
        }
    }

    /// The address.
    #[must_use]
    pub const fn address(&self) -> IpAddress {
        self.address
    }

    /// How many leading bits are the network's.
    #[must_use]
    pub const fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    /// Whether `address` is in this prefix.
    ///
    /// An address of the other family never is: a route to `10.0.0.0/8` does
    /// not carry an IPv6 packet, however the bits compare.
    #[must_use]
    pub fn contains(&self, address: IpAddress) -> bool {
        if address.is_v4() != self.address.is_v4() {
            return false;
        }
        matching_bits(self.address.bytes(), address.bytes()) >= u32::from(self.prefix_len)
    }

    /// The broadcast address of an IPv4 prefix: every host bit set.
    #[must_use]
    pub fn broadcast(&self) -> Option<Ipv4> {
        let IpAddress::V4(address) = self.address else {
            return None;
        };
        let host_bits = 32_u32.saturating_sub(u32::from(self.prefix_len));
        let mask = if host_bits >= 32 {
            u32::MAX
        } else {
            (1_u32 << host_bits) - 1
        };
        Some(Ipv4::from_bits(address.to_bits() | mask))
    }
}

impl fmt::Display for IpCidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.address, self.prefix_len)
    }
}

/// How many leading bits two byte strings share.
fn matching_bits(left: &[u8], right: &[u8]) -> u32 {
    let mut count = 0;
    for (a, b) in left.iter().zip(right.iter()) {
        let difference = a ^ b;
        if difference == 0 {
            count += 8;
            continue;
        }
        return count + difference.leading_zeros();
    }
    count
}

/// An address and a port: what a socket is bound or connected to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Endpoint {
    /// The address, which may be the unspecified one.
    pub address: IpAddress,
    /// The port, which may be zero for "choose one".
    pub port: u16,
}

impl Endpoint {
    /// An endpoint.
    #[must_use]
    pub const fn new(address: IpAddress, port: u16) -> Endpoint {
        Endpoint { address, port }
    }

    /// Whether this endpoint names a particular address and port.
    #[must_use]
    pub fn is_specified(&self) -> bool {
        !self.address.is_unspecified() && self.port != 0
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.address {
            IpAddress::V4(address) => write!(formatter, "{address}:{}", self.port),
            IpAddress::V6(address) => write!(formatter, "[{address}]:{}", self.port),
        }
    }
}
