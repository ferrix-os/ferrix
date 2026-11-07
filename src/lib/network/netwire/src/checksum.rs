//! The internet checksum of RFC 1071, and the pseudo-headers that feed it.
//!
//! IPv4, ICMP, UDP and TCP all carry the same checksum: the one's complement of
//! the one's-complement sum of the data taken as big-endian 16-bit words, an odd
//! byte at the end padded with a zero. UDP and TCP also sum a pseudo-header of
//! the addresses around them, which is why [`Checksum`] accumulates rather than
//! being a single function of one slice.
//!
//! A header received intact verifies by summing it *with* its checksum field:
//! the result [`Checksum::finish`]es as zero.

/// A running one's-complement sum.
///
/// Bytes may be added in any number of pieces of any length; the sum is the
/// same as adding them in one piece, because an odd byte left over from one
/// piece is paired with the first byte of the next.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Checksum {
    /// The sum of the whole words so far, not yet folded.
    sum: u64,
    /// A byte waiting for its partner, when the bytes so far were odd.
    odd: Option<u8>,
}

impl Checksum {
    /// An empty sum.
    #[must_use]
    pub const fn new() -> Self {
        Checksum { sum: 0, odd: None }
    }

    /// Add `bytes` to the sum.
    pub fn add_bytes(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        if let Some(high) = self.odd.take() {
            let Some((&low, tail)) = rest.split_first() else {
                self.odd = Some(high);
                return;
            };
            self.sum += u64::from(u16::from_be_bytes([high, low]));
            rest = tail;
        }
        let (words, tail) = rest.as_chunks::<2>();
        for word in words {
            self.sum += u64::from(u16::from_be_bytes(*word));
        }
        self.odd = tail.first().copied();
    }

    /// Add a big-endian 16-bit field.
    pub fn add_u16(&mut self, value: u16) {
        self.add_bytes(&value.to_be_bytes());
    }

    /// Add a big-endian 32-bit field.
    pub fn add_u32(&mut self, value: u32) {
        self.add_bytes(&value.to_be_bytes());
    }

    /// The checksum of everything added: the one's complement of the folded sum.
    ///
    /// For a header summed with its own checksum field, zero means it verifies.
    #[must_use]
    pub fn finish(self) -> u16 {
        let mut sum = self.sum;
        if let Some(high) = self.odd {
            sum += u64::from(u16::from_be_bytes([high, 0]));
        }
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !u16::try_from(sum).unwrap_or(u16::MAX)
    }
}

/// The internet checksum of `bytes`.
#[must_use]
pub fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = Checksum::new();
    sum.add_bytes(bytes);
    sum.finish()
}

/// A sum started with the IPv4 pseudo-header of RFC 768 and RFC 9293: the two
/// addresses, a zero byte, the protocol number, and the upper-layer length.
#[must_use]
pub fn ipv4_pseudo_header(
    source: [u8; 4],
    destination: [u8; 4],
    protocol: u8,
    length: u16,
) -> Checksum {
    let mut sum = Checksum::new();
    sum.add_bytes(&source);
    sum.add_bytes(&destination);
    sum.add_bytes(&[0, protocol]);
    sum.add_u16(length);
    sum
}

/// A sum started with the IPv6 pseudo-header of RFC 8200 section 8.1: the two
/// addresses, the 32-bit upper-layer length, three zero bytes, and the next
/// header value of the upper layer.
#[must_use]
pub fn ipv6_pseudo_header(
    source: [u8; 16],
    destination: [u8; 16],
    next_header: u8,
    length: u32,
) -> Checksum {
    let mut sum = Checksum::new();
    sum.add_bytes(&source);
    sum.add_bytes(&destination);
    sum.add_u32(length);
    sum.add_bytes(&[0, 0, 0, next_header]);
    sum
}

/// The addresses around a UDP or TCP message, which its checksum covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pseudo {
    /// Carried in IPv4.
    V4 {
        /// The IPv4 source address.
        source: [u8; 4],
        /// The IPv4 destination address.
        destination: [u8; 4],
    },
    /// Carried in IPv6.
    V6 {
        /// The IPv6 source address.
        source: [u8; 16],
        /// The IPv6 destination address.
        destination: [u8; 16],
    },
}

impl Pseudo {
    /// The pseudo-header sum for an upper-layer message of `protocol` and
    /// `length` bytes, or `None` if the length does not fit the version's field.
    #[must_use]
    pub fn sum(self, protocol: u8, length: usize) -> Option<Checksum> {
        match self {
            Pseudo::V4 {
                source,
                destination,
            } => Some(ipv4_pseudo_header(
                source,
                destination,
                protocol,
                u16::try_from(length).ok()?,
            )),
            Pseudo::V6 {
                source,
                destination,
            } => Some(ipv6_pseudo_header(
                source,
                destination,
                protocol,
                u32::try_from(length).ok()?,
            )),
        }
    }
}
