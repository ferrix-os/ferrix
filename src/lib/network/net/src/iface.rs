//! Interfaces: what a packet leaves by, and what it may claim to come from.

use alloc::vec::Vec;
use core::fmt;

use ferrix_netwire::ethernet::Mac;

use crate::addr::{IpAddress, IpCidr, Ipv4, Ipv6};

/// The longest interface name, which is Linux's `IFNAMSIZ` less its
/// terminator.
pub const MAX_NAME: usize = 15;

/// The interface is up, administratively.
pub const IFF_UP: u32 = 1 << 0;
/// The interface supports broadcast.
pub const IFF_BROADCAST: u32 = 1 << 1;
/// The interface is the loopback.
pub const IFF_LOOPBACK: u32 = 1 << 3;
/// The interface is a point-to-point link.
pub const IFF_POINTOPOINT: u32 = 1 << 4;
/// The interface is up and carrier is present.
pub const IFF_RUNNING: u32 = 1 << 6;
/// The interface needs no address resolution.
pub const IFF_NOARP: u32 = 1 << 7;
/// The interface supports multicast.
pub const IFF_MULTICAST: u32 = 1 << 12;
/// The link below the interface is up.
pub const IFF_LOWER_UP: u32 = 1 << 16;

/// An interface's name: at most fifteen bytes, and never a nul.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    /// The bytes, of which the first `len` are the name.
    bytes: [u8; MAX_NAME],
    /// How many bytes are the name.
    len: u8,
}

impl Name {
    /// The name those bytes spell, truncated to fit and cut at a nul.
    #[must_use]
    pub fn new(name: &[u8]) -> Name {
        let end = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        let mut bytes = [0_u8; MAX_NAME];
        let mut len = 0_u8;
        for (slot, byte) in bytes.iter_mut().zip(name.iter().take(end)) {
            *slot = *byte;
            len += 1;
        }
        Name { bytes, len }
    }

    /// The name's bytes, without a terminator.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

impl fmt::Display for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.as_bytes() {
            write!(formatter, "{}", char::from(*byte))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// What kind of link an interface is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Medium {
    /// Frames carry an Ethernet header and next hops are resolved.
    Ethernet,
    /// Packets go straight back into the input path with no header at all.
    Loopback,
}

/// What an interface is made of, which the kernel needs to know to carry a
/// frame somewhere other than a wire (`docs/NETNS.md` section 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backing {
    /// The loopback, or anything the stack makes itself: nothing outside the
    /// stack carries its frames.
    Software,
    /// A device a driver serves. The key is stable when the interface moves
    /// to another stack, which its index is not.
    Device(u32),
    /// One end of a pair of virtual Ethernet links: what leaves one arrives at
    /// the other, wherever the other is.
    Veth {
        /// Which pair.
        pair: u64,
        /// Which of its two ends, 0 or 1.
        end: u8,
    },
}

/// An address configured on an interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Address {
    /// The address and its prefix.
    pub cidr: IpCidr,
    /// The address this one is a peer of, on a point-to-point link.
    pub peer: Option<IpAddress>,
}

/// What an interface has carried.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    /// Packets received.
    pub received: u64,
    /// Bytes received.
    pub received_bytes: u64,
    /// Packets that arrived and were not understood.
    pub received_errors: u64,
    /// Packets that arrived with nowhere to go.
    pub received_dropped: u64,
    /// Packets sent.
    pub sent: u64,
    /// Bytes sent.
    pub sent_bytes: u64,
    /// Packets that could not be sent.
    pub sent_errors: u64,
    /// Packets dropped rather than queued.
    pub sent_dropped: u64,
}

/// One network interface.
#[derive(Clone, Debug)]
pub struct Interface {
    /// The number `if_nametoindex` answers, which is never zero.
    pub index: u32,
    /// The name `ip link` shows.
    pub name: Name,
    /// What kind of link it is.
    pub medium: Medium,
    /// The hardware address, all zero on a loopback.
    pub hardware: Mac,
    /// The largest payload a frame may carry.
    pub mtu: u32,
    /// The `IFF_` flags.
    pub flags: u32,
    /// The addresses configured on it.
    pub addresses: Vec<Address>,
    /// What it has carried.
    pub counters: Counters,
    /// What carries its frames.
    pub backing: Backing,
}

impl Interface {
    /// The addresses a loopback interface owns when it is up.
    #[must_use]
    pub fn loopback_addresses() -> [Address; 2] {
        [
            Address {
                cidr: IpCidr::new(IpAddress::V4(Ipv4::LOOPBACK), 8),
                peer: None,
            },
            Address {
                cidr: IpCidr::new(IpAddress::V6(Ipv6::LOOPBACK), 128),
                peer: None,
            },
        ]
    }

    /// A loopback interface that is down and owns nothing: what a new network
    /// namespace starts with. Bringing it up gives it its addresses
    /// ([`crate::Stack::set_up`]), as Linux does.
    #[must_use]
    pub fn loopback_down(index: u32) -> Interface {
        Interface {
            index,
            name: Name::new(b"lo"),
            medium: Medium::Loopback,
            hardware: [0; 6],
            mtu: 65_536,
            flags: IFF_LOOPBACK,
            addresses: Vec::new(),
            counters: Counters::default(),
            backing: Backing::Software,
        }
    }

    /// A loopback interface, which is up from the moment it exists.
    #[must_use]
    pub fn loopback(index: u32) -> Interface {
        Interface {
            flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP,
            addresses: Interface::loopback_addresses().to_vec(),
            ..Interface::loopback_down(index)
        }
    }

    /// An Ethernet interface, down and without addresses: what enumeration
    /// produces, before `ip` has said anything about it.
    #[must_use]
    pub fn ethernet(index: u32, name: &[u8], hardware: Mac, mtu: u32) -> Interface {
        Interface {
            index,
            name: Name::new(name),
            medium: Medium::Ethernet,
            hardware,
            mtu,
            flags: IFF_BROADCAST | IFF_MULTICAST,
            addresses: Vec::new(),
            counters: Counters::default(),
            backing: Backing::Software,
        }
    }

    /// Whether the interface is up and may carry a packet.
    #[must_use]
    pub const fn is_up(&self) -> bool {
        self.flags & IFF_UP != 0
    }

    /// Whether the interface resolves next hops rather than sending to nobody.
    #[must_use]
    pub const fn resolves(&self) -> bool {
        matches!(self.medium, Medium::Ethernet) && self.flags & IFF_NOARP == 0
    }

    /// Whether `address` is one of this interface's own.
    #[must_use]
    pub fn owns(&self, address: IpAddress) -> bool {
        self.addresses
            .iter()
            .any(|configured| configured.cidr.address() == address)
    }

    /// Whether a packet to `address` is for this interface: its own address,
    /// its broadcast, or a multicast group.
    #[must_use]
    pub fn accepts(&self, address: IpAddress) -> bool {
        if self.owns(address) || address.is_multicast() {
            return true;
        }
        let IpAddress::V4(four) = address else {
            return false;
        };
        if four.is_broadcast() {
            return true;
        }
        self.addresses
            .iter()
            .any(|configured| configured.cidr.broadcast() == Some(four))
    }

    /// An address of the same family as `destination` to send from, preferring
    /// one whose prefix contains it.
    #[must_use]
    pub fn source_for(&self, destination: IpAddress) -> Option<IpAddress> {
        let same_family =
            |candidate: &&Address| candidate.cidr.address().is_v4() == destination.is_v4();
        self.addresses
            .iter()
            .filter(same_family)
            .find(|candidate| candidate.cidr.contains(destination))
            .or_else(|| self.addresses.iter().find(same_family))
            .map(|candidate| candidate.cidr.address())
    }

    /// Add an address, replacing one with the same address.
    pub fn add_address(&mut self, address: Address) {
        self.addresses
            .retain(|existing| existing.cidr.address() != address.cidr.address());
        self.addresses.push(address);
    }

    /// Remove an address. Answers whether there was one.
    pub fn remove_address(&mut self, address: IpAddress) -> bool {
        let before = self.addresses.len();
        self.addresses
            .retain(|existing| existing.cidr.address() != address);
        self.addresses.len() != before
    }
}
