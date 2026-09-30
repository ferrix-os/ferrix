//! The net core's self-check, run at boot like every other subsystem's.
//!
//! It uses the loopback and nothing else, so it passes on a machine with no
//! network device at all — which every machine is until a driver lands. What
//! it proves is that the whole path is wired: a socket call reaches the stack,
//! the stack builds a packet, the packet goes round the loopback and back up
//! through the input path, and the bytes come out of the other socket.
//!
//! Five things are required:
//!
//! * the loopback interface is up and owns `127.0.0.1` and `::1`;
//! * a UDP datagram sent to a bound port arrives with the sender's address,
//!   and one sent to a port nobody holds earns `ECONNREFUSED` from the
//!   unreachable the host sends itself;
//! * a TCP connection to a listening port is made and accepted, carries bytes
//!   in both directions, and ends as a clean close at both ends;
//! * a connection to a port nobody listens on is refused rather than left to
//!   time out;
//! * a port a socket listens on is its own: a second socket may not bind an
//!   overlapping address on it, nor listen beside it having bound first,
//!   `EADDRINUSE` both, though both set `SO_REUSEADDR` -- as Linux keeps a
//!   listener's port, so a second listener cannot take its connections;
//! * the same over IPv6, so that the second family is not a claim;
//! * a raw ICMP socket reads the echo it sent and the reply to it, each with
//!   its IPv4 header, and one filtering replies with `ICMP_FILTER` reads the
//!   request and not the reply; and an `IPPROTO_RAW` socket's packet, whose
//!   header leaves the source, identification, length and checksum at zero,
//!   is completed and reaches a UDP socket.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_net::socket::Family;
use ferrix_net::{Endpoint, IpAddress, Ipv4, Ipv6};

use super::socket::{InetKind, InetSocket, of};
use crate::net;

/// What the check saw.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Why it did not run, if it did not.
    pub(crate) skipped: Option<&'static str>,
    /// How many interfaces are up.
    pub(crate) interfaces: usize,
    /// Bytes carried over the loopback, both directions and both families.
    pub(crate) bytes: usize,
    /// How many calls were refused exactly as specified.
    pub(crate) refusals: usize,
    /// How many connections were made and accepted.
    pub(crate) connections: usize,
    /// How many packets raw sockets read.
    pub(crate) raw_packets: usize,
}

/// The body every check sends, chosen so a truncation shows.
const BODY: &[u8] = b"Ferrix carries this over its own loopback, both ways.";

/// Run the check.
///
/// # Errors
///
/// A string naming what did not hold, which the caller turns into a panic.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let core = net::core();
    report.interfaces = core.with(|stack, _| {
        stack
            .interfaces()
            .iter()
            .filter(|interface| interface.is_up())
            .count()
    });
    if report.interfaces == 0 {
        return Err("the loopback interface is not up");
    }
    let owns = core.with(|stack, _| {
        stack.interfaces().iter().any(|interface| {
            interface.owns(IpAddress::V4(Ipv4::LOOPBACK))
                && interface.owns(IpAddress::V6(Ipv6::LOOPBACK))
        })
    });
    if !owns {
        return Err("the loopback interface does not own 127.0.0.1 and ::1");
    }

    datagrams(
        &mut report,
        Family::V4,
        IpAddress::V4(Ipv4::LOOPBACK),
        7_777,
    )?;
    datagrams(
        &mut report,
        Family::V6,
        IpAddress::V6(Ipv6::LOOPBACK),
        7_778,
    )?;
    streams(
        &mut report,
        Family::V4,
        IpAddress::V4(Ipv4::LOOPBACK),
        7_779,
    )?;
    streams(
        &mut report,
        Family::V6,
        IpAddress::V6(Ipv6::LOOPBACK),
        7_780,
    )?;
    refused(
        &mut report,
        Family::V4,
        IpAddress::V4(Ipv4::LOOPBACK),
        7_781,
    )?;
    listeners_keep_their_port(&mut report, 7_783)?;
    raw_icmp(&mut report)?;
    raw_icmpv6(&mut report)?;
    raw_header_included(&mut report, 7_782)?;

    // Nothing here leaves this host, so nothing should be waiting for a
    // driver. A frame queued for one is a route pointing at an interface with
    // nobody behind it, which is a mistake worth catching here rather than as
    // silence on a wire.
    if core.queued() != 0 || core.dropped() != 0 {
        return Err("the loopback check left frames waiting for a driver");
    }
    Ok(report)
}

/// A socket of a kind, as the inode behind the open file it comes as.
fn socket(family: Family, kind: InetKind) -> Result<Arc<InetSocket>, &'static str> {
    let file = InetSocket::open(net::first(), family, kind, false, (0, 0))
        .map_err(|_| "a socket could not be opened")?;
    of(&file).ok_or("a socket's open file does not hold a socket")
}

/// A datagram to a bound port arrives; one to an empty port is refused.
fn datagrams(
    report: &mut Report,
    family: Family,
    address: IpAddress,
    port: u16,
) -> Result<(), &'static str> {
    let core = net::core();
    let server = socket(family, InetKind::Datagram)?;
    server
        .bind(&encode(address, port))
        .map_err(|_| "a datagram socket could not bind the loopback")?;
    let client = socket(family, InetKind::Datagram)?;
    let sent = client
        .send(BODY, 0, false, Some(&encode(address, port)))
        .map_err(|_| "a datagram to a bound port was refused")?;
    if sent != BODY.len() {
        return Err("a datagram was sent short");
    }
    let mut out = [0_u8; 128];
    let (received, from) = server
        .recv(&mut out, 0, true)
        .map_err(|_| "a datagram that was sent did not arrive")?;
    if out.get(..received.bytes) != Some(BODY) {
        return Err("a datagram arrived as something else");
    }
    if from.is_none() {
        return Err("a datagram arrived without a sender");
    }
    report.bytes += received.bytes;

    // A port nobody holds: the host sends itself an unreachable, and the next
    // call on the socket reports it.
    let lonely = socket(family, InetKind::Datagram)?;
    lonely
        .connect(&encode(address, 9), false)
        .map_err(|_| "a datagram socket could not be connected")?;
    let before = core.with(|stack, _| stack.counters());
    if lonely.send(BODY, 0, false, None).is_err() {
        return Err("a datagram to an empty port could not even be sent");
    }
    let after = core.with(|stack, _| stack.counters());
    if after.malformed != before.malformed {
        return Err("a datagram to an empty port came back malformed");
    }
    if after.delivered != before.delivered {
        return Err("a datagram to an empty port was delivered to a socket");
    }
    if after.no_socket == before.no_socket {
        return Err("a datagram to an empty port never reached the demultiplexer");
    }
    if after.unreachable_sent == before.unreachable_sent {
        return Err("a datagram to an empty port earned no unreachable message");
    }
    if after.errors_reported == before.errors_reported {
        return Err("an unreachable message was sent and never came back up");
    }
    let answer = lonely.recv(&mut out, 0, true);
    match answer {
        Err(Errno::ECONNREFUSED) => report.refusals += 1,
        Err(Errno::EAGAIN) => {
            return Err("a datagram to an empty port answered EAGAIN: no unreachable came back");
        }
        Ok(_) => return Err("a datagram to an empty port answered with data"),
        Err(_) => return Err("a datagram to an empty port was refused with the wrong errno"),
    }
    Ok(())
}

/// A connection is made, accepted, carries bytes both ways, and closes.
fn streams(
    report: &mut Report,
    family: Family,
    address: IpAddress,
    port: u16,
) -> Result<(), &'static str> {
    let listener = socket(family, InetKind::Stream)?;
    listener
        .bind(&encode(address, port))
        .map_err(|_| "a stream socket could not bind the loopback")?;
    listener
        .listen(4)
        .map_err(|_| "a stream socket could not listen")?;

    let client = socket(family, InetKind::Stream)?;
    client
        .connect(&encode(address, port), false)
        .map_err(|_| "a connection to a listening port was refused")?;
    let (accepted, peer) = listener
        .accept(false, false, (0, 0))
        .map_err(|_| "a connection that was made was not accepted")?;
    if peer.is_empty() {
        return Err("an accepted connection has no peer address");
    }
    if accepted.status().nonblock {
        return Err("a connection accepted without SOCK_NONBLOCK is non-blocking");
    }
    // A non-blocking listener's accept does not wait, and the socket it takes
    // is non-blocking only when `accept4` asked for it, as on Linux.
    for asked in [true, false] {
        let other = socket(family, InetKind::Stream)?;
        other
            .connect(&encode(address, port), false)
            .map_err(|_| "a second connection to a listening port was refused")?;
        let (taken, _) = listener
            .accept(true, asked, (0, 0))
            .map_err(|_| "a non-blocking accept did not take a finished connection")?;
        if taken.status().nonblock != asked {
            return Err(if asked {
                "a connection accepted with SOCK_NONBLOCK is not non-blocking"
            } else {
                "a non-blocking listener gave its accepted connection O_NONBLOCK"
            });
        }
        report.connections += 1;
    }
    let server = of(&accepted).ok_or("an accepted connection is not a socket")?;
    report.connections += 1;

    report.bytes += carry(&client, &server, "from the connecting end")?;
    report.bytes += carry(&server, &client, "from the accepting end")?;

    // A close at one end is the end of the stream at the other.
    client
        .shutdown(ferrix_linux_abi::socket::SHUT_WR)
        .map_err(|_| "a connection could not be shut down")?;
    let mut out = [0_u8; 8];
    let (end, _) = server
        .recv(&mut out, 0, false)
        .map_err(|_| "a closed connection did not end its stream")?;
    if end.bytes != 0 {
        return Err("a closed connection gave bytes after its close");
    }
    Ok(())
}

/// Send the body one way and require it whole at the other end.
fn carry(
    from: &Arc<InetSocket>,
    to: &Arc<InetSocket>,
    which: &'static str,
) -> Result<usize, &'static str> {
    let mut written = 0;
    while written < BODY.len() {
        let rest = BODY.get(written..).unwrap_or_default();
        written += from.send(rest, 0, false, None).map_err(|_| which)?;
    }
    let mut out = Vec::new();
    let mut chunk = [0_u8; 128];
    while out.len() < BODY.len() {
        let (received, _) = to.recv(&mut chunk, 0, false).map_err(|_| which)?;
        if received.bytes == 0 {
            return Err(which);
        }
        out.extend(chunk.iter().take(received.bytes).copied());
    }
    if out.as_slice() != BODY {
        return Err(which);
    }
    Ok(out.len())
}

/// A connection to a port nobody listens on is refused.
fn refused(
    report: &mut Report,
    family: Family,
    address: IpAddress,
    port: u16,
) -> Result<(), &'static str> {
    let client = socket(family, InetKind::Stream)?;
    match client.connect(&encode(address, port), false) {
        Err(Errno::ECONNREFUSED) => {
            report.refusals += 1;
            Ok(())
        }
        _ => Err("a connection to a port nobody listens on was not refused"),
    }
}

/// A listener keeps its port against sockets that set `SO_REUSEADDR`, as
/// every listener tokio makes and init's socket units do: a bind of the
/// loopback on a port a wildcard listener holds, and a listen of a socket
/// bound beside a listener before it listened, are both `EADDRINUSE`.
fn listeners_keep_their_port(report: &mut Report, port: u16) -> Result<(), &'static str> {
    use ferrix_linux_abi::socket::{SO_REUSEADDR, SOL_SOCKET, Width};
    let reusing = || {
        let made = socket(Family::V4, InetKind::Stream)?;
        made.set_option(
            SOL_SOCKET,
            SO_REUSEADDR,
            &1_i32.to_ne_bytes(),
            Width::Bits64,
        )
        .map_err(|_| "SO_REUSEADDR could not be set on a stream socket")?;
        Ok::<_, &'static str>(made)
    };
    let wildcard = IpAddress::V4(Ipv4::UNSPECIFIED);
    let loopback = IpAddress::V4(Ipv4::LOOPBACK);

    let listener = reusing()?;
    let beside = reusing()?;
    listener
        .bind(&encode(wildcard, port))
        .map_err(|_| "a stream socket could not bind the wildcard address")?;
    beside
        .bind(&encode(wildcard, port))
        .map_err(|_| "SO_REUSEADDR did not let two sockets bind a port nobody listens on")?;
    listener
        .listen(4)
        .map_err(|_| "a stream socket could not listen")?;
    if beside.listen(4) != Err(Errno::EADDRINUSE) {
        return Err("a second socket listened on a port another listens on");
    }
    report.refusals += 1;

    let second = reusing()?;
    if second.bind(&encode(loopback, port)) != Err(Errno::EADDRINUSE) {
        return Err("a second socket bound a port a wildcard listener holds");
    }
    report.refusals += 1;
    Ok(())
}

/// An echo request's identifier the raw check uses, which no other socket on
/// the loopback answers to.
const RAW_IDENTIFIER: u16 = 0x5245;

/// ICMP's protocol number.
const ICMP: u8 = 1;

/// ICMPv6's.
const ICMPV6: u8 = 58;

/// A raw ICMP socket reads what `ping` needs, and `ICMP_FILTER` holds back
/// exactly the type it names.
///
/// Over the loopback a raw ICMP socket is handed the request on its way in,
/// then the reply the host sends itself. The filtered socket is the negative
/// control: it must read the request, so it was not simply deaf, and not the
/// reply.
fn raw_icmp(report: &mut Report) -> Result<(), &'static str> {
    let loopback = encode(IpAddress::V4(Ipv4::LOOPBACK), 0);
    let ping = socket(Family::V4, InetKind::Raw { protocol: ICMP })?;
    let filtered = socket(Family::V4, InetKind::Raw { protocol: ICMP })?;
    let replies_only = 1_u32 << ferrix_netwire::icmpv4::kind::ECHO_REPLY;
    filtered
        .set_option(
            ferrix_linux_abi::inet::SOL_RAW,
            ferrix_linux_abi::inet::ICMP_FILTER,
            &replies_only.to_ne_bytes(),
            ferrix_linux_abi::socket::Width::Bits64,
        )
        .map_err(|_| "ICMP_FILTER was refused on a raw ICMP socket")?;

    let request = echo_request(RAW_IDENTIFIER, BODY)?;
    let sent = ping
        .send(&request, 0, false, Some(&loopback))
        .map_err(|_| "a raw ICMP socket could not send an echo request")?;
    if sent != request.len() {
        return Err("a raw ICMP socket sent its echo request short");
    }

    let seen = drain(&ping)?;
    report.raw_packets += seen.len();
    let types = echo_types(&seen)?;
    if !types.contains(&ferrix_netwire::icmpv4::kind::ECHO_REQUEST) {
        return Err("a raw ICMP socket was not handed its own echo request off the loopback");
    }
    let reply = seen.iter().find(|packet| {
        icmp_at(packet).is_some_and(|message| {
            message.first() == Some(&ferrix_netwire::icmpv4::kind::ECHO_REPLY)
                && message.get(4..6) == Some(RAW_IDENTIFIER.to_be_bytes().as_slice())
                && message.get(8..) == Some(BODY)
        })
    });
    if reply.is_none() {
        return Err("a raw ICMP socket did not read the echo reply with its identifier and body");
    }

    let held_back = drain(&filtered)?;
    report.raw_packets += held_back.len();
    let filtered_types = echo_types(&held_back)?;
    if !filtered_types.contains(&ferrix_netwire::icmpv4::kind::ECHO_REQUEST) {
        return Err("a raw ICMP socket filtering replies read nothing at all");
    }
    if filtered_types.contains(&ferrix_netwire::icmpv4::kind::ECHO_REPLY) {
        return Err("ICMP_FILTER let the echo reply it names through");
    }
    report.refusals += 1;
    Ok(())
}

/// `ping6` over the loopback, as busybox does it: a raw ICMPv6 socket sends an
/// echo request with its checksum left zero, and reads the reply as the
/// ICMPv6 message alone, its checksum written by the stack, with the hop limit
/// it arrived with as an `IPV6_2292HOPLIMIT` control message.
///
/// The filtered socket is the negative control, as in [`raw_icmp`]: it reads
/// the request, so it was not deaf, and not the reply its `ICMP6_FILTER`
/// blocks. `IPV6_CHECKSUM` at `SOL_IPV6` on an ICMPv6 socket and an odd offset
/// are refused, as RFC 3542 and `do_rawv6_setsockopt` refuse them.
fn raw_icmpv6(report: &mut Report) -> Result<(), &'static str> {
    use ferrix_linux_abi::inet::{IPV6_2292HOPLIMIT, SOL_IPV6};
    use ferrix_netwire::icmpv6::kind::{ECHO_REPLY, ECHO_REQUEST};

    let loopback = encode(IpAddress::V6(Ipv6::LOOPBACK), 0);
    let ping = socket(Family::V6, InetKind::Raw { protocol: ICMPV6 })?;
    let filtered = socket(Family::V6, InetKind::Raw { protocol: ICMPV6 })?;
    set_up_ping6(report, &ping, &filtered)?;

    let mut request = alloc::vec![ECHO_REQUEST, 0, 0, 0];
    request.extend_from_slice(&RAW_IDENTIFIER.to_be_bytes());
    request.extend_from_slice(&[0, 1]);
    request.extend_from_slice(BODY);
    let sent = ping
        .send(&request, 0, false, Some(&loopback))
        .map_err(|_| "a raw ICMPv6 socket could not send an echo request")?;
    if sent != request.len() {
        return Err("a raw ICMPv6 socket sent its echo request short");
    }

    let mut reply_seen = false;
    loop {
        let mut out = [0_u8; 256];
        let (received, _) = match ping.recv(&mut out, 0, true) {
            Ok(taken) => taken,
            Err(Errno::EAGAIN) => break,
            Err(_) => return Err("a raw ICMPv6 socket's receive failed"),
        };
        report.raw_packets += 1;
        let message = out.get(..received.bytes).unwrap_or_default();
        let sum = {
            let pseudo = ferrix_netwire::checksum::Pseudo::V6 {
                source: Ipv6::LOOPBACK.octets(),
                destination: Ipv6::LOOPBACK.octets(),
            };
            let mut sum = pseudo
                .sum(ICMPV6, message.len())
                .ok_or("a raw ICMPv6 message too long to sum")?;
            sum.add_bytes(message);
            sum.finish()
        };
        if sum != 0 {
            return Err("a raw ICMPv6 socket read a message whose checksum does not verify");
        }
        if message.first() != Some(&ECHO_REPLY) {
            continue;
        }
        if message.get(4..6) != Some(RAW_IDENTIFIER.to_be_bytes().as_slice())
            || message.get(8..) != Some(BODY)
        {
            return Err("a raw ICMPv6 socket's echo reply lost its identifier or body");
        }
        let expected = super::socket::Control {
            level: SOL_IPV6,
            kind: IPV6_2292HOPLIMIT,
            data: 64_i32.to_ne_bytes().to_vec(),
        };
        if ping.control_messages(&received) != alloc::vec![expected] {
            return Err("an echo reply did not come with its hop limit, 64, as IPV6_2292HOPLIMIT");
        }
        reply_seen = true;
    }
    if !reply_seen {
        return Err("a raw ICMPv6 socket did not read the echo reply off the loopback");
    }

    let held_back = drain(&filtered)?;
    report.raw_packets += held_back.len();
    let kinds: Vec<u8> = held_back
        .iter()
        .filter_map(|message| message.first().copied())
        .collect();
    if !kinds.contains(&ECHO_REQUEST) {
        return Err("a raw ICMPv6 socket letting only requests through read nothing at all");
    }
    if kinds.contains(&ECHO_REPLY) {
        return Err("ICMP6_FILTER let the echo reply it blocks through");
    }
    report.refusals += 1;
    Ok(())
}

/// The options [`raw_icmpv6`] sets as `ping6` sets them: `IPV6_CHECKSUM` 2 at
/// `SOL_RAW` and the two refusals beside it, and `IPV6_2292HOPLIMIT` on the
/// pinging socket; an `ICMP6_FILTER` passing only requests on the other.
fn set_up_ping6(
    report: &mut Report,
    ping: &Arc<InetSocket>,
    filtered: &Arc<InetSocket>,
) -> Result<(), &'static str> {
    use ferrix_linux_abi::inet::{
        ICMPV6_FILTER, IPV6_2292HOPLIMIT, IPV6_CHECKSUM, SOL_ICMPV6, SOL_IPV6, SOL_RAW,
    };
    use ferrix_linux_abi::socket::Width;
    use ferrix_netwire::icmpv6::kind::ECHO_REQUEST;

    let two = 2_i32.to_ne_bytes();
    ping.set_option(SOL_RAW, IPV6_CHECKSUM, &two, Width::Bits64)
        .map_err(|_| "IPV6_CHECKSUM 2 at SOL_RAW was refused on a raw ICMPv6 socket")?;
    if ping.set_option(SOL_IPV6, IPV6_CHECKSUM, &two, Width::Bits64) != Err(Errno::EINVAL) {
        return Err("IPV6_CHECKSUM at SOL_IPV6 on a raw ICMPv6 socket was not EINVAL");
    }
    if ping.set_option(SOL_RAW, IPV6_CHECKSUM, &3_i32.to_ne_bytes(), Width::Bits64)
        != Err(Errno::EINVAL)
    {
        return Err("an odd IPV6_CHECKSUM offset was not EINVAL");
    }
    report.refusals += 2;
    ping.set_option(
        SOL_IPV6,
        IPV6_2292HOPLIMIT,
        &1_i32.to_ne_bytes(),
        Width::Bits64,
    )
    .map_err(|_| "IPV6_2292HOPLIMIT was refused on an IPv6 socket")?;
    // Block everything, then let the request through: `ICMP6_FILTER_SETPASS`.
    let mut blocks = [u32::MAX; 8];
    if let Some(word) = blocks.get_mut(usize::from(ECHO_REQUEST >> 5)) {
        *word &= !(1 << (ECHO_REQUEST & 31));
    }
    let filter: Vec<u8> = blocks.iter().flat_map(|word| word.to_ne_bytes()).collect();
    filtered
        .set_option(SOL_ICMPV6, ICMPV6_FILTER, &filter, Width::Bits64)
        .map_err(|_| "ICMP6_FILTER was refused on a raw ICMPv6 socket")?;
    Ok(())
}

/// An `IPPROTO_RAW` socket sends a UDP datagram inside a header it wrote with
/// the source, identification, total length and checksum left zero, and the
/// datagram reaches a UDP socket: the stack filled those in, or the input path
/// would have dropped the packet for its checksum.
fn raw_header_included(report: &mut Report, port: u16) -> Result<(), &'static str> {
    let loopback = IpAddress::V4(Ipv4::LOOPBACK);
    let server = socket(Family::V4, InetKind::Datagram)?;
    server
        .bind(&encode(loopback, port))
        .map_err(|_| "a datagram socket could not bind the loopback for the raw check")?;
    let sender = socket(Family::V4, InetKind::Raw { protocol: 255 })?;

    let pseudo = ferrix_netwire::checksum::Pseudo::V4 {
        source: Ipv4::LOOPBACK.octets(),
        destination: Ipv4::LOOPBACK.octets(),
    };
    let mut datagram = alloc::vec![0_u8; ferrix_netwire::udp::HEADER_LEN + BODY.len()];
    let _ = ferrix_netwire::udp::Header {
        source_port: port.wrapping_add(1),
        destination_port: port,
    }
    .emit(BODY, pseudo, &mut datagram)
    .map_err(|_| "the raw check's UDP datagram did not fit")?;
    let mut packet = alloc::vec![0_u8; ferrix_netwire::ipv4::MIN_HEADER_LEN];
    let fields: [(usize, u8); 3] = [(0, 0x45), (8, 64), (9, ferrix_netwire::udp::PROTOCOL)];
    for (at, value) in fields {
        *packet
            .get_mut(at)
            .ok_or("the raw check's header is too short")? = value;
    }
    packet
        .get_mut(16..20)
        .ok_or("the raw check's header is too short")?
        .copy_from_slice(&Ipv4::LOOPBACK.octets());
    packet.extend_from_slice(&datagram);

    let sent = sender
        .send(&packet, 0, false, Some(&encode(loopback, 0)))
        .map_err(|_| "an IPPROTO_RAW socket could not send a packet with its own header")?;
    if sent != packet.len() {
        return Err("an IPPROTO_RAW socket sent its packet short");
    }
    let mut out = [0_u8; 128];
    let (received, _) = server
        .recv(&mut out, 0, true)
        .map_err(|_| "a header-included packet was not completed and delivered")?;
    if out.get(..received.bytes) != Some(BODY) {
        return Err("a header-included packet arrived as something else");
    }
    report.bytes += received.bytes;

    // `IPPROTO_RAW` receives nothing, even a packet of its own number.
    let mut nothing = [0_u8; 8];
    match sender.recv(&mut nothing, 0, true) {
        Err(Errno::EAGAIN) => Ok(()),
        _ => Err("an IPPROTO_RAW socket was handed a packet"),
    }
}

/// An echo request with its checksum, as `ping` builds one.
fn echo_request(identifier: u16, body: &[u8]) -> Result<Vec<u8>, &'static str> {
    let [high, low] = identifier.to_be_bytes();
    let header = ferrix_netwire::icmpv4::Header {
        kind: ferrix_netwire::icmpv4::kind::ECHO_REQUEST,
        code: 0,
        rest: [high, low, 0, 1],
    };
    let mut message = alloc::vec![0_u8; ferrix_netwire::icmpv4::HEADER_LEN + body.len()];
    let _ = header
        .emit(body, &mut message)
        .map_err(|_| "the raw check's echo request did not fit")?;
    Ok(message)
}

/// Every packet waiting on a raw socket.
fn drain(raw: &Arc<InetSocket>) -> Result<Vec<Vec<u8>>, &'static str> {
    let mut packets = Vec::new();
    loop {
        let mut out = [0_u8; 256];
        match raw.recv(&mut out, 0, true) {
            Ok((received, _)) => {
                packets.push(out.get(..received.bytes).unwrap_or_default().to_vec());
            }
            Err(Errno::EAGAIN) => return Ok(packets),
            Err(_) => return Err("a raw socket's receive failed"),
        }
    }
}

/// The ICMP message of a packet a raw socket read, after its IPv4 header.
fn icmp_at(packet: &[u8]) -> Option<&[u8]> {
    let first = *packet.first()?;
    if first >> 4 != 4 || packet.get(9) != Some(&ICMP) {
        return None;
    }
    packet.get(usize::from(first & 0x0F) * 4..)
}

/// The ICMP types of what a raw ICMP socket read, refusing a packet that does
/// not start with an IPv4 header naming ICMP.
fn echo_types(packets: &[Vec<u8>]) -> Result<Vec<u8>, &'static str> {
    packets
        .iter()
        .map(|packet| {
            icmp_at(packet)
                .and_then(|message| message.first().copied())
                .ok_or("a raw ICMP socket read a packet without an IPv4 header naming ICMP")
        })
        .collect()
}

/// A `sockaddr` for an address and a port.
fn encode(address: IpAddress, port: u16) -> Vec<u8> {
    let endpoint = Endpoint::new(address, port);
    let inet = match endpoint.address {
        IpAddress::V4(four) => ferrix_linux_abi::inet::InetAddress::V4 {
            port: endpoint.port,
            address: four.octets(),
        },
        IpAddress::V6(six) => ferrix_linux_abi::inet::InetAddress::V6 {
            port: endpoint.port,
            flow_info: 0,
            address: six.octets(),
            scope_id: 0,
        },
    };
    let mut bytes = alloc::vec![0_u8; ferrix_linux_abi::inet::SOCKADDR_STORAGE_SIZE];
    let written = inet.encode(&mut bytes).unwrap_or(0);
    bytes.truncate(written);
    bytes
}
