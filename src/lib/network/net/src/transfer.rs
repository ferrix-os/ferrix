//! Moving bytes between a program and a socket.
//!
//! A datagram send builds a packet and puts it on a link there and then, so a
//! program that gets `Ok` knows the packet left. A stream send only fills the
//! send queue: the segments go out when the windows allow, which is the whole
//! difference between the two protocols stated as an API.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_netwire::{icmpv4, icmpv6, udp};

use crate::addr::{Endpoint, IpAddress, Ipv6};
use crate::socket::{Error, Socket};
use crate::stack::{Millis, Stack, towards_self, unspecified};

/// What a receive answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Received {
    /// How many bytes were copied out.
    pub bytes: usize,
    /// How many more the datagram held than there was room for, which
    /// `MSG_TRUNC` reports.
    pub truncated: usize,
    /// Who sent it, for a socket that is not connected.
    pub remote: Option<Endpoint>,
    /// The hop limit it arrived with, for a datagram.
    pub hop_limit: Option<u8>,
}

impl Stack {
    /// Send `data`, to `to` if the socket is not connected.
    pub fn send(
        &mut self,
        id: crate::socket::SocketId,
        data: &[u8],
        to: Option<Endpoint>,
        now: Millis,
    ) -> Result<usize, Error> {
        match self.sockets.get(&id.0).ok_or(Error::NoSocket)? {
            Socket::Udp(_) => self.send_udp(id, data, to, now),
            Socket::Icmp(_) => self.send_icmp(id, data, to, now),
            Socket::Raw(_) => self.send_raw(id, data, to, now),
            Socket::Stream(_) => self.send_stream(id, data),
            Socket::Listen(_) => Err(Error::NotConnected),
        }
    }

    /// Fill in what a datagram send needs and answer it, or say why not.
    fn addresses_for(
        &mut self,
        id: crate::socket::SocketId,
        to: Option<Endpoint>,
    ) -> Result<(Endpoint, Endpoint, Option<u32>, u8), Error> {
        let family = self.sockets.get(&id.0).ok_or(Error::NoSocket)?.family();
        let Some(Socket::Udp(socket) | Socket::Icmp(socket)) = self.sockets.get(&id.0) else {
            return Err(Error::WrongKind);
        };
        if socket.write_shut {
            return Err(Error::ShutDown);
        }
        let destination = towards_self(
            normalise(to.or(socket.remote).ok_or(Error::NotConnected)?),
            socket.local.address,
        );
        let options = socket.options;
        let bound = socket.local;
        let source = if bound.address.is_unspecified() {
            self.source_for(destination.address)?
        } else {
            bound.address
        };
        let port = if bound.port == 0 {
            self.bind_ephemeral(id, family)?
        } else {
            bound.port
        };
        Ok((
            Endpoint::new(source, port),
            destination,
            options.device,
            options.hop_limit,
        ))
    }

    /// Give an unbound socket a port, and remember it.
    fn bind_ephemeral(
        &mut self,
        id: crate::socket::SocketId,
        family: crate::socket::Family,
    ) -> Result<u16, Error> {
        let address = unspecified(family);
        self.bind(id, Endpoint::new(address, 0))?;
        let _ = address;
        self.local_endpoint(id)
            .map(|endpoint| endpoint.port)
            .ok_or(Error::NoSocket)
    }

    /// Build and send a UDP datagram.
    fn send_udp(
        &mut self,
        id: crate::socket::SocketId,
        data: &[u8],
        to: Option<Endpoint>,
        now: Millis,
    ) -> Result<usize, Error> {
        let (local, remote, device, hop_limit) = self.addresses_for(id, to)?;
        if local.address.is_v4() != remote.address.is_v4() {
            return Err(Error::Invalid);
        }
        let header = udp::Header {
            source_port: local.port,
            destination_port: remote.port,
        };
        let pseudo = crate::input::pseudo_of(local.address, remote.address);
        let mut packet = vec![0_u8; udp::HEADER_LEN + data.len()];
        let _ = header
            .emit(data, pseudo, &mut packet)
            .map_err(|_| Error::TooLarge)?;
        self.send_ip(
            local.address,
            remote.address,
            udp::PROTOCOL,
            &packet,
            hop_limit,
            device,
            now,
        )?;
        Ok(data.len())
    }

    /// Send an ICMP echo the program built, with its identifier replaced by
    /// the socket's port.
    ///
    /// That replacement is the whole trick of an unprivileged ping socket: the
    /// kernel owns the identifier, so one program's replies cannot be read by
    /// another, and no program needs the privilege a raw socket would.
    fn send_icmp(
        &mut self,
        id: crate::socket::SocketId,
        data: &[u8],
        to: Option<Endpoint>,
        now: Millis,
    ) -> Result<usize, Error> {
        let (local, remote, device, hop_limit) = self.addresses_for(id, to)?;
        let kind = *data.first().ok_or(Error::Invalid)?;
        let code = *data.get(1).ok_or(Error::Invalid)?;
        let rest = data.get(4..).ok_or(Error::Invalid)?;
        let mut body = rest.to_vec();
        let identifier = local.port.to_be_bytes();
        for (slot, byte) in body.iter_mut().zip(identifier.iter()) {
            *slot = *byte;
        }
        let packet = match (local.address, remote.address) {
            (IpAddress::V4(_), IpAddress::V4(_)) => {
                let header = icmpv4::Header {
                    kind,
                    code,
                    rest: *data
                        .get(4..8)
                        .and_then(|bytes| bytes.first_chunk::<4>())
                        .ok_or(Error::Invalid)?,
                };
                let payload = data.get(8..).unwrap_or_default();
                let mut header = header;
                header.rest = with_identifier(header.rest, local.port);
                let mut packet = vec![0_u8; icmpv4::HEADER_LEN + payload.len()];
                let _ = header
                    .emit(payload, &mut packet)
                    .map_err(|_| Error::TooLarge)?;
                packet
            }
            (IpAddress::V6(from), IpAddress::V6(to)) => {
                let message = icmpv6::Message {
                    kind,
                    code,
                    body: &body,
                };
                let mut packet = vec![0_u8; icmpv6::HEADER_LEN + body.len()];
                let _ = message
                    .emit(from.octets(), to.octets(), &mut packet)
                    .map_err(|_| Error::TooLarge)?;
                packet
            }
            _ => return Err(Error::Invalid),
        };
        let protocol = if local.address.is_v4() {
            ferrix_netwire::ipv4::protocol::ICMP
        } else {
            icmpv6::PROTOCOL
        };
        self.send_ip(
            local.address,
            remote.address,
            protocol,
            &packet,
            hop_limit,
            device,
            now,
        )?;
        Ok(data.len())
    }

    /// Send a packet from a raw socket.
    ///
    /// Without `IP_HDRINCL` the program wrote the payload, and the stack builds
    /// the header with the socket's protocol, fragmenting if it must. With it
    /// the program wrote the header too, and the stack fills in what
    /// `raw_send_hdrinc` fills in: the total length, the checksum, an
    /// identification left zero and a source left zero. That packet goes out
    /// whole, to where the call named, which need not be where the header does.
    ///
    /// An IPv6 socket always writes only the payload, and the stack puts the
    /// checksum at the offset `IPV6_CHECKSUM` names, as `rawv6_send_hdrinc`
    /// is not offered: `IPPROTO_RAW` in IPv6 is `EINVAL` on send.
    fn send_raw(
        &mut self,
        id: crate::socket::SocketId,
        data: &[u8],
        to: Option<Endpoint>,
        now: Millis,
    ) -> Result<usize, Error> {
        let Some(Socket::Raw(raw)) = self.sockets.get(&id.0) else {
            return Err(Error::WrongKind);
        };
        if raw.datagram.write_shut {
            return Err(Error::ShutDown);
        }
        let destination = normalise(to.or(raw.datagram.remote).ok_or(Error::NotConnected)?).address;
        let v6 = matches!(raw.datagram.family, crate::socket::Family::V6);
        if destination.is_unspecified() || destination.is_v4() == v6 {
            return Err(Error::Invalid);
        }
        let options = raw.datagram.options;
        let bound = raw.datagram.local.address;
        let protocol = raw.protocol;
        let header_included = raw.header_included;
        let checksum = raw.checksum;
        let source = if bound.is_unspecified() {
            self.source_for(destination)?
        } else {
            bound
        };
        if v6 {
            if header_included {
                return Err(Error::Invalid);
            }
            let mut packet = data.to_vec();
            if let Some(offset) = checksum {
                put_checksum(&mut packet, offset, source, destination, protocol)?;
            }
            self.send_ip(
                source,
                destination,
                protocol,
                &packet,
                options.hop_limit,
                options.device,
                now,
            )?;
            return Ok(data.len());
        }
        if !header_included {
            self.send_ip(
                source,
                destination,
                protocol,
                data,
                options.hop_limit,
                options.device,
                now,
            )?;
            return Ok(data.len());
        }
        let packet = self.complete_header(data, source)?;
        self.send_whole_v4(packet, destination, options.device, now)?;
        Ok(data.len())
    }

    /// An `IP_HDRINCL` packet with the fields the stack fills in filled in.
    fn complete_header(&mut self, data: &[u8], source: IpAddress) -> Result<Vec<u8>, Error> {
        let first = *data.first().ok_or(Error::Invalid)?;
        let header_len = usize::from(first & 0x0F) * 4;
        if data.len() < ferrix_netwire::ipv4::MIN_HEADER_LEN
            || first >> 4 != 4
            || header_len < ferrix_netwire::ipv4::MIN_HEADER_LEN
            || header_len > data.len()
        {
            return Err(Error::Invalid);
        }
        let total = u16::try_from(data.len()).map_err(|_| Error::TooLarge)?;
        let mut packet = data.to_vec();
        put(&mut packet, 2, &total.to_be_bytes())?;
        if packet.get(4..6) == Some(&[0, 0]) {
            let identification = self.next_identification();
            put(&mut packet, 4, &identification.to_be_bytes())?;
        }
        if packet.get(12..16) == Some(&[0, 0, 0, 0])
            && let IpAddress::V4(address) = source
        {
            put(&mut packet, 12, &address.octets())?;
        }
        put(&mut packet, 10, &[0, 0])?;
        let header = packet.get(..header_len).ok_or(Error::Invalid)?;
        let sum = ferrix_netwire::checksum::checksum(header);
        put(&mut packet, 10, &sum.to_be_bytes())?;
        Ok(packet)
    }

    /// Route a finished IPv4 packet and put it on its link, unfragmented.
    fn send_whole_v4(
        &mut self,
        packet: Vec<u8>,
        destination: IpAddress,
        device: Option<u32>,
        now: Millis,
    ) -> Result<(), Error> {
        let hop = self.routes.lookup(destination).ok_or(Error::Unreachable)?;
        if device.is_some_and(|wanted| wanted != hop.interface) {
            return Err(Error::Unreachable);
        }
        let interface = self.interface(hop.interface).ok_or(Error::Unreachable)?;
        if !interface.is_up() {
            return Err(Error::Unreachable);
        }
        if packet.len() > interface.mtu as usize {
            return Err(Error::TooLarge);
        }
        self.dispatch(
            hop.interface,
            hop.address,
            packet,
            ferrix_netwire::ethernet::ethertype::IPV4,
            now,
        )
    }

    /// Put bytes in a connection's send queue.
    fn send_stream(&mut self, id: crate::socket::SocketId, data: &[u8]) -> Result<usize, Error> {
        let Some(Socket::Stream(stream)) = self.sockets.get_mut(&id.0) else {
            return Err(Error::WrongKind);
        };
        if let Some(error) = stream.error.take() {
            return Err(error);
        }
        if let Some(error) = Stack::stream_error(&stream.connection) {
            return Err(error);
        }
        let state = stream.connection.state();
        if state == ferrix_nettcp::State::SynSent {
            return Err(Error::InProgress);
        }
        if !state.can_send() {
            return Err(Error::ShutDown);
        }
        let taken = stream.connection.write(data);
        if taken == 0 && !data.is_empty() {
            // A queue with room that its job could not be charged to grow
            // into is not full, and waiting for room would never end.
            if stream.connection.write_refused() {
                return Err(Error::NoMemory);
            }
            return Err(Error::WouldBlock);
        }
        Ok(taken)
    }

    /// Take received bytes.
    pub fn recv(
        &mut self,
        id: crate::socket::SocketId,
        out: &mut [u8],
        peek: bool,
    ) -> Result<Received, Error> {
        match self.sockets.get_mut(&id.0).ok_or(Error::NoSocket)? {
            Socket::Udp(socket)
            | Socket::Icmp(socket)
            | Socket::Raw(crate::socket::RawSocket {
                datagram: socket, ..
            }) => {
                if let Some(error) = socket.take_error() {
                    return Err(error);
                }
                let Some(datagram) = socket.peek() else {
                    if socket.read_shut {
                        return Ok(Received::default());
                    }
                    return Err(Error::WouldBlock);
                };
                let copied = copy(out, &datagram.payload);
                let truncated = datagram.payload.len().saturating_sub(copied);
                let remote = datagram.remote;
                let hop_limit = datagram.hop_limit;
                if !peek {
                    let _ = socket.take();
                }
                Ok(Received {
                    bytes: copied,
                    truncated,
                    remote: Some(remote),
                    hop_limit: Some(hop_limit),
                })
            }
            Socket::Stream(stream) => {
                if let Some(error) = stream.error.take() {
                    return Err(error);
                }
                let taken = if peek {
                    stream.connection.peek(out)
                } else {
                    stream.connection.read(out)
                };
                if taken > 0 {
                    return Ok(Received {
                        bytes: taken,
                        truncated: 0,
                        remote: Some(stream.remote),
                        hop_limit: None,
                    });
                }
                if let Some(error) = Stack::stream_error(&stream.connection) {
                    return Err(error);
                }
                if stream.connection.state().can_receive() || out.is_empty() {
                    return Err(Error::WouldBlock);
                }
                // The peer closed and everything it sent has been read: this
                // is the end of the stream, which is a read of zero bytes.
                Ok(Received {
                    bytes: 0,
                    truncated: 0,
                    remote: Some(stream.remote),
                    hop_limit: None,
                })
            }
            Socket::Listen(_) => Err(Error::NotConnected),
        }
    }
}

/// Write the checksum of an IPv6 upper-layer `packet` from `source` to
/// `destination` at `offset`, the field zeroed while summing, as
/// `rawv6_push_pending_frames` does. A packet too short to hold the field is
/// `EINVAL`, as there.
fn put_checksum(
    packet: &mut [u8],
    offset: usize,
    source: IpAddress,
    destination: IpAddress,
    protocol: u8,
) -> Result<(), Error> {
    if offset.checked_add(2).is_none_or(|end| end > packet.len()) {
        return Err(Error::Invalid);
    }
    put(packet, offset, &[0, 0])?;
    let sum = crate::input::upper_layer_sum(packet, source, destination, protocol)
        .ok_or(Error::TooLarge)?;
    put(packet, offset, &sum.to_be_bytes())
}

/// Overwrite `field.len()` bytes of `packet` at `at`.
fn put(packet: &mut [u8], at: usize, field: &[u8]) -> Result<(), Error> {
    packet
        .get_mut(at..at + field.len())
        .ok_or(Error::Invalid)?
        .copy_from_slice(field);
    Ok(())
}

/// Copy as much of `from` as fits, and say how much that was.
fn copy(out: &mut [u8], from: &[u8]) -> usize {
    let mut copied = 0;
    for (slot, byte) in out.iter_mut().zip(from.iter()) {
        *slot = *byte;
        copied += 1;
    }
    copied
}

/// An echo header's four trailing bytes with the identifier replaced.
fn with_identifier(rest: [u8; 4], identifier: u16) -> [u8; 4] {
    let [high, low] = identifier.to_be_bytes();
    [high, low, rest[2], rest[3]]
}

/// An IPv4-mapped IPv6 endpoint written as the IPv4 one it is.
///
/// The stack keeps one form on the wire and in its tables; the kernel is what
/// hands a program back the mapped spelling when the socket was opened as
/// `AF_INET6`.
pub(crate) fn normalise(endpoint: Endpoint) -> Endpoint {
    match endpoint.address {
        IpAddress::V6(address) => match address.v4_mapped() {
            Some(four) => Endpoint::new(IpAddress::V4(four), endpoint.port),
            None => endpoint,
        },
        IpAddress::V4(_) => endpoint,
    }
}

/// The IPv6 spelling of an address, for a socket opened as `AF_INET6`.
#[must_use]
pub fn to_v6(address: IpAddress) -> IpAddress {
    match address {
        IpAddress::V4(four) => IpAddress::V6(Ipv6::from_v4_mapped(four)),
        IpAddress::V6(_) => address,
    }
}
