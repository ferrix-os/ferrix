//! The IPv4 and UDP headers around a DHCP message, as a `SOCK_DGRAM`
//! packet socket reads and writes them: the link header is the kernel's,
//! and everything from the IP header on is ours.
//!
//! The headers are written and read by `ferrix-netwire`, the kernel's own
//! wire formats, so a checksum this client gets wrong is one the stack
//! would get wrong too, and its tests would say so.

use std::net::Ipv4Addr;

use ferrix_netwire::checksum::Pseudo;
use ferrix_netwire::{ipv4, udp};

use crate::message::{CLIENT_PORT, SERVER_PORT};

/// The time to live of what the client sends: a DHCP message crosses no
/// router (a relay makes a message of its own).
const TTL: u8 = 64;

/// `message`, from `source` port 68 to `destination` port 67, in a UDP
/// datagram in an IPv4 packet.
///
/// # Errors
///
/// A message too long for one packet, which nothing here writes.
pub fn wrap(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    message: &[u8],
) -> Result<Vec<u8>, ferrix_netwire::Error> {
    let datagram_len = udp::HEADER_LEN + message.len();
    let mut packet = vec![0_u8; ipv4::MIN_HEADER_LEN + datagram_len];
    let header = ipv4::Header {
        dscp: 0,
        ecn: 0,
        identification: 0,
        dont_fragment: false,
        more_fragments: false,
        fragment_offset: 0,
        ttl: TTL,
        protocol: ipv4::protocol::UDP,
        source: source.octets(),
        destination: destination.octets(),
    };
    let header_len = header.emit(&[], datagram_len, &mut packet)?;
    let ports = udp::Header {
        source_port: CLIENT_PORT,
        destination_port: SERVER_PORT,
    };
    let pseudo = Pseudo::V4 {
        source: source.octets(),
        destination: destination.octets(),
    };
    let written = ports.emit(
        message,
        pseudo,
        packet
            .get_mut(header_len..)
            .ok_or(ferrix_netwire::Error::NoSpace)?,
    )?;
    packet.truncate(header_len + written);
    Ok(packet)
}

/// The DHCP message in `packet`, and the address it came from, if `packet`
/// is a whole IPv4 packet holding a UDP datagram from port 67 to port 68
/// whose checksums hold. Anything else is somebody else's traffic, and
/// `None`.
#[must_use]
pub fn unwrap(packet: &[u8]) -> Option<(Ipv4Addr, &[u8])> {
    let parsed = ipv4::Header::parse(packet).ok()?;
    if parsed.header.protocol != ipv4::protocol::UDP || parsed.header.is_fragment() {
        return None;
    }
    let pseudo = Pseudo::V4 {
        source: parsed.header.source,
        destination: parsed.header.destination,
    };
    let datagram = udp::Header::parse(parsed.payload, pseudo).ok()?;
    if datagram.header.source_port != SERVER_PORT || datagram.header.destination_port != CLIENT_PORT
    {
        return None;
    }
    Some((Ipv4Addr::from(parsed.header.source), datagram.payload))
}
