//! UDP and the echo socket: over the loopback, and over a link that has to
//! resolve an address first.

use alloc::vec::Vec;

use crate::addr::{Endpoint, IpAddress, Ipv4};
use crate::socket::{Error, Family};
use crate::tests::harness::{Alone, ONE, ONE_V6, TWO, TWO_V6, Wire, at, at_v6};

/// Set `SO_REUSEADDR`, so two sockets may hold one port.
fn reuse(stack: &mut crate::stack::Stack, id: crate::socket::SocketId) {
    if let Some(socket) = stack.socket_mut(id) {
        socket.options_mut().reuse_address = true;
    }
}

/// A body whose every byte says where it is.
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

#[test]
fn a_datagram_crosses_the_loopback() {
    let mut host = Alone::new();
    let server = host.stack.open_udp(Family::V4);
    host.stack
        .bind(server, at(Ipv4::LOOPBACK, 7_000))
        .expect("a loopback address may be bound");
    let client = host.stack.open_udp(Family::V4);

    let sent = host
        .stack
        .send(client, b"hello", Some(at(Ipv4::LOOPBACK, 7_000)), host.now)
        .expect("a datagram to a bound port is sent");
    assert_eq!(sent, 5);
    host.settle();

    let mut out = [0_u8; 32];
    let received = host
        .stack
        .recv(server, &mut out, false)
        .expect("the datagram arrived");
    assert_eq!(received.bytes, 5);
    assert_eq!(out.get(..5), Some(b"hello".as_slice()));
    assert_eq!(
        received.remote.map(|endpoint| endpoint.address),
        Some(IpAddress::V4(Ipv4::LOOPBACK))
    );
}

#[test]
fn a_datagram_crosses_a_link_after_the_address_is_resolved() {
    let mut wire = Wire::new();
    let server = wire.two.open_udp(Family::V4);
    wire.two
        .bind(server, at(TWO, 9_000))
        .expect("the host owns that address");
    let client = wire.one.open_udp(Family::V4);

    let now = wire.now;
    let sent = wire
        .one
        .send(client, b"over the wire", Some(at(TWO, 9_000)), now)
        .expect("the destination is on the link");
    assert_eq!(sent, 13);
    wire.settle();

    let mut out = [0_u8; 64];
    let received = wire
        .two
        .recv(server, &mut out, false)
        .expect("the datagram arrived");
    assert_eq!(out.get(..received.bytes), Some(b"over the wire".as_slice()));
    assert_eq!(
        received.remote,
        Some(at(
            ONE,
            wire.one.local_endpoint(client).expect("bound").port
        ))
    );
    // The first packet needed an ARP request and its answer before it could
    // go, so more frames crossed than the one datagram.
    assert!(wire.frames >= 3, "{} frames crossed", wire.frames);
}

#[test]
fn the_second_datagram_needs_no_resolution() {
    let mut wire = Wire::new();
    let server = wire.two.open_udp(Family::V4);
    wire.two.bind(server, at(TWO, 9_000)).expect("bound");
    let client = wire.one.open_udp(Family::V4);
    let now = wire.now;
    let _ = wire.one.send(client, b"one", Some(at(TWO, 9_000)), now);
    wire.settle();
    let after_first = wire.frames;

    let now = wire.now;
    let _ = wire.one.send(client, b"two", Some(at(TWO, 9_000)), now);
    // Without letting the clock move, so the answer cannot have grown stale.
    wire.exchange();
    assert_eq!(
        wire.frames - after_first,
        1,
        "the second datagram should be one frame"
    );
}

#[test]
fn a_datagram_to_a_port_nobody_holds_earns_an_unreachable() {
    let mut wire = Wire::new();
    let client = wire.one.open_udp(Family::V4);
    wire.one
        .connect(client, at(TWO, 9_999), 0)
        .expect("a connect to a routable address");
    let now = wire.now;
    let _ = wire.one.send(client, b"anyone?", None, now);
    wire.settle();

    let mut out = [0_u8; 16];
    let answer = wire.one.recv(client, &mut out, false);
    assert_eq!(answer, Err(Error::PortUnreachable));
}

#[test]
fn a_datagram_larger_than_the_buffer_is_truncated_and_says_so() {
    let mut host = Alone::new();
    let server = host.stack.open_udp(Family::V4);
    host.stack
        .bind(server, at(Ipv4::LOOPBACK, 7_001))
        .expect("bound");
    let client = host.stack.open_udp(Family::V4);
    let sent = body(100);
    let _ = host
        .stack
        .send(client, &sent, Some(at(Ipv4::LOOPBACK, 7_001)), host.now)
        .expect("sent");
    host.settle();

    let mut out = [0_u8; 10];
    let received = host.stack.recv(server, &mut out, false).expect("arrived");
    assert_eq!(received.bytes, 10);
    assert_eq!(received.truncated, 90);
    // The rest of the datagram is gone, not queued: the next read is the next
    // datagram, and there is none.
    assert_eq!(
        host.stack.recv(server, &mut out, false),
        Err(Error::WouldBlock)
    );
}

#[test]
fn a_connected_socket_still_receives_when_a_wildcard_one_is_open() {
    let mut wire = Wire::new();
    let wildcard = wire.two.open_udp(Family::V4);
    reuse(&mut wire.two, wildcard);
    wire.two
        .bind(
            wildcard,
            Endpoint::new(IpAddress::V4(Ipv4::UNSPECIFIED), 9_100),
        )
        .expect("the unspecified address may be bound");
    let connected = wire.two.open_udp(Family::V4);
    reuse(&mut wire.two, connected);
    wire.two.bind(connected, at(TWO, 9_100)).expect("bound");
    wire.two
        .connect(connected, at(ONE, 5_000), 0)
        .expect("connected");

    let client = wire.one.open_udp(Family::V4);
    wire.one.bind(client, at(ONE, 5_000)).expect("bound");
    let now = wire.now;
    let _ = wire
        .one
        .send(client, b"for the connected one", Some(at(TWO, 9_100)), now);
    wire.settle();

    let mut out = [0_u8; 64];
    let received = wire.two.recv(connected, &mut out, false).expect("arrived");
    assert_eq!(received.bytes, 21);
    assert_eq!(
        wire.two.recv(wildcard, &mut out, false),
        Err(Error::WouldBlock),
        "the wildcard socket should not have taken it"
    );
}

#[test]
fn a_large_datagram_is_fragmented_and_put_back_together() {
    let mut wire = Wire::new();
    let server = wire.two.open_udp(Family::V4);
    wire.two.bind(server, at(TWO, 9_200)).expect("bound");
    let client = wire.one.open_udp(Family::V4);
    // Larger than the link's 1500-byte MTU, so it has to be fragmented.
    let sent = body(4_000);
    let now = wire.now;
    let count = wire
        .one
        .send(client, &sent, Some(at(TWO, 9_200)), now)
        .expect("a datagram that needs fragmenting is still sent");
    assert_eq!(count, 4_000);
    wire.settle();

    let mut out = [0_u8; 8_192];
    let received = wire.two.recv(server, &mut out, false).expect("arrived");
    assert_eq!(received.bytes, 4_000);
    assert_eq!(out.get(..4_000), Some(sent.as_slice()));
    assert_eq!(wire.two.counters().reassembled, 1);
}

#[test]
fn a_peek_leaves_the_datagram_where_it_was() {
    let mut host = Alone::new();
    let server = host.stack.open_udp(Family::V4);
    host.stack
        .bind(server, at(Ipv4::LOOPBACK, 7_002))
        .expect("bound");
    let client = host.stack.open_udp(Family::V4);
    let _ = host
        .stack
        .send(client, b"twice", Some(at(Ipv4::LOOPBACK, 7_002)), host.now)
        .expect("sent");
    host.settle();

    let mut out = [0_u8; 16];
    let peeked = host.stack.recv(server, &mut out, true).expect("arrived");
    assert_eq!(peeked.bytes, 5);
    let taken = host
        .stack
        .recv(server, &mut out, false)
        .expect("still there");
    assert_eq!(taken.bytes, 5);
    assert_eq!(
        host.stack.recv(server, &mut out, false),
        Err(Error::WouldBlock)
    );
}

#[test]
fn an_echo_request_is_answered_and_the_reply_reaches_the_socket() {
    let mut wire = Wire::new();
    let ping = wire.one.open_icmp(Family::V4);
    wire.one
        .bind(ping, at(ONE, 1_234))
        .expect("an echo socket binds its identifier as a port");
    // An echo request: type 8, code 0, identifier and sequence, then a body.
    let request = [8_u8, 0, 0, 0, 0, 0, 0, 1, b'p', b'i', b'n', b'g'];
    let now = wire.now;
    let _ = wire
        .one
        .send(ping, &request, Some(at(TWO, 0)), now)
        .expect("an echo goes to an address, not a port");
    wire.settle();

    let mut out = [0_u8; 64];
    let received = wire
        .one
        .recv(ping, &mut out, false)
        .expect("a reply arrived");
    assert_eq!(out.first(), Some(&0), "an echo reply is type 0");
    assert_eq!(
        out.get(8..received.bytes),
        Some(b"ping".as_slice()),
        "the body comes back unchanged"
    );
}

#[test]
fn binding_an_address_this_host_does_not_have_is_refused() {
    let mut host = Alone::new();
    let socket = host.stack.open_udp(Family::V4);
    let answer = host
        .stack
        .bind(socket, at(Ipv4::new([192, 0, 2, 1]), 5_000));
    assert_eq!(answer, Err(Error::AddressNotAvailable));
}

#[test]
fn two_sockets_cannot_hold_the_same_port() {
    let mut host = Alone::new();
    let first = host.stack.open_udp(Family::V4);
    host.stack
        .bind(first, at(Ipv4::LOOPBACK, 7_003))
        .expect("bound");
    let second = host.stack.open_udp(Family::V4);
    assert_eq!(
        host.stack.bind(second, at(Ipv4::LOOPBACK, 7_003)),
        Err(Error::AddressInUse)
    );
}

#[test]
fn an_unbound_socket_is_given_an_ephemeral_port_by_its_first_send() {
    let mut host = Alone::new();
    let socket = host.stack.open_udp(Family::V4);
    assert_eq!(host.stack.local_endpoint(socket).map(|e| e.port), Some(0));
    let _ = host
        .stack
        .send(socket, b"x", Some(at(Ipv4::LOOPBACK, 7_004)), host.now);
    let port = host
        .stack
        .local_endpoint(socket)
        .map(|endpoint| endpoint.port)
        .expect("the socket is there");
    assert!(
        (crate::ports::FIRST..=crate::ports::LAST).contains(&port),
        "port {port} is not in the ephemeral range"
    );
}

#[test]
fn a_datagram_crosses_over_ipv6_after_neighbour_discovery() {
    let mut wire = Wire::new();
    let server = wire.two.open_udp(Family::V6);
    wire.two
        .bind(server, at_v6(TWO_V6, 9_300))
        .expect("the host owns that address");
    let client = wire.one.open_udp(Family::V6);

    let now = wire.now;
    let sent = wire
        .one
        .send(client, b"six", Some(at_v6(TWO_V6, 9_300)), now)
        .expect("the destination is on the link");
    assert_eq!(sent, 3);
    wire.settle();

    let mut out = [0_u8; 32];
    let received = wire.two.recv(server, &mut out, false).expect("arrived");
    assert_eq!(out.get(..received.bytes), Some(b"six".as_slice()));
    assert_eq!(
        received.remote.map(|endpoint| endpoint.address),
        Some(IpAddress::V6(ONE_V6))
    );
}

#[test]
fn a_datagram_to_an_empty_port_on_this_host_earns_an_unreachable() {
    let mut host = Alone::new();
    let client = host.stack.open_udp(Family::V4);
    host.stack
        .connect(client, at(Ipv4::LOOPBACK, 9), host.now)
        .expect("the loopback is routable");
    let now = host.now;
    let sent = host.stack.send(client, b"anyone?", None, now);
    assert!(
        sent.is_ok(),
        "the datagram itself should have gone: {sent:?}"
    );
    host.settle();

    let mut out = [0_u8; 16];
    assert_eq!(
        host.stack.recv(client, &mut out, false),
        Err(Error::PortUnreachable)
    );
}

#[test]
fn an_unreachable_is_earned_even_with_other_sockets_open() {
    // The kernel's boot check does exactly this, in this order, and found a
    // hole the shorter test above did not.
    let mut host = Alone::new();
    let server = host.stack.open_udp(Family::V4);
    host.stack
        .bind(server, at(Ipv4::LOOPBACK, 7_777))
        .expect("bound");
    let client = host.stack.open_udp(Family::V4);
    let now = host.now;
    let _ = host
        .stack
        .send(client, b"hello", Some(at(Ipv4::LOOPBACK, 7_777)), now)
        .expect("sent");
    host.settle();
    let mut out = [0_u8; 128];
    let _ = host.stack.recv(server, &mut out, false).expect("arrived");

    let lonely = host.stack.open_udp(Family::V4);
    host.stack
        .connect(lonely, at(Ipv4::LOOPBACK, 9), host.now)
        .expect("connected");
    let before = host.stack.counters();
    let now = host.now;
    let _ = host
        .stack
        .send(lonely, b"anyone?", None, now)
        .expect("sent");
    host.settle();
    let after = host.stack.counters();
    assert_eq!(
        after.malformed, before.malformed,
        "the datagram came back malformed"
    );
    assert_ne!(
        after.no_socket, before.no_socket,
        "the datagram never reached the demultiplexer"
    );
    assert_ne!(
        after.unreachable_sent, before.unreachable_sent,
        "no unreachable was sent"
    );
    assert_ne!(
        after.errors_reported, before.errors_reported,
        "the unreachable never came back up"
    );
    assert_eq!(
        host.stack.recv(lonely, &mut out, false),
        Err(Error::PortUnreachable)
    );
}

#[test]
fn an_unreachable_comes_back_over_ipv6_as_well() {
    let mut wire = Wire::new();
    let client = wire.one.open_udp(Family::V6);
    let now = wire.now;
    wire.one
        .connect(client, at_v6(TWO_V6, 9), now)
        .expect("the address is routable");
    let now = wire.now;
    let _ = wire.one.send(client, b"anyone?", None, now).expect("sent");
    wire.settle();
    assert_eq!(
        wire.one.counters().malformed,
        0,
        "a message came back unreadable"
    );
    assert_eq!(wire.two.counters().unreachable_sent, 1);
    let mut out = [0_u8; 16];
    assert_eq!(
        wire.one.recv(client, &mut out, false),
        Err(Error::PortUnreachable)
    );
}

#[test]
fn a_datagram_to_the_unspecified_address_crosses_the_loopback() {
    let mut host = Alone::new();
    let server = host.stack.open_udp(Family::V4);
    host.stack
        .bind(server, at(Ipv4::LOOPBACK, 7_090))
        .expect("a loopback address may be bound");
    let client = host.stack.open_udp(Family::V4);
    let sent = host
        .stack
        .send(
            client,
            b"self",
            Some(at(Ipv4::UNSPECIFIED, 7_090)),
            host.now,
        )
        .expect("0.0.0.0 means this host, as on Linux");
    assert_eq!(sent, 4);
    host.settle();
    let mut out = [0_u8; 8];
    let received = host
        .stack
        .recv(server, &mut out, false)
        .expect("the datagram arrived");
    assert_eq!(out.get(..received.bytes), Some(b"self".as_slice()));
}

#[test]
fn a_bound_socket_s_datagram_to_the_unspecified_address_goes_to_its_own_address_on_the_wire() {
    // As the stream case: the bound address is Linux's choice, and with no
    // local table the datagram leaves on eth0 rather than being delivered
    // locally as Linux would. Before 0.0.0.0 meant this host, the send was
    // refused as unreachable.
    let mut wire = Wire::new();
    let client = wire.one.open_udp(Family::V4);
    wire.one
        .bind(client, at(ONE, 9_200))
        .expect("the host owns that address");
    let now = wire.now;
    let sent = wire
        .one
        .send(client, b"own", Some(at(Ipv4::UNSPECIFIED, 9_201)), now)
        .expect("0.0.0.0 means the bound address");
    assert_eq!(sent, 3);
    let frame = wire.one.poll_transmit(now).expect("a frame for the wire");
    assert!(!wire.one.is_loopback(frame.interface));
}
