//! TCP through the stack: connecting, accepting, carrying bytes, and closing.

use alloc::vec::Vec;

use ferrix_nettcp::State;

use crate::addr::{Endpoint, IpAddress, Ipv4};
use crate::socket::{Error, Family, Socket};
use crate::tests::harness::{Alone, Host, ONE, TWO, TWO_V6, Wire, at, at_v6};

/// A body whose every byte says where it is.
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

/// The state a stream socket's connection is in.
fn state(stack: &crate::stack::Stack, id: crate::socket::SocketId) -> Option<State> {
    match stack.socket(id)? {
        Socket::Stream(stream) => Some(stream.connection.state()),
        _ => None,
    }
}

#[test]
fn a_connection_is_made_and_accepted_over_the_loopback() {
    let mut host = Alone::new();
    let listener = host.stack.open_tcp(Family::V4);
    host.stack
        .bind(listener, at(Ipv4::LOOPBACK, 8_080))
        .expect("bound");
    host.stack.listen(listener, 4).expect("listening");

    let client = host.stack.open_tcp(Family::V4);
    host.stack
        .connect(client, at(Ipv4::LOOPBACK, 8_080), host.now)
        .expect("a connect to a listening port");
    host.settle();

    assert_eq!(state(&host.stack, client), Some(State::Established));
    let server = host
        .stack
        .accept(listener)
        .expect("a connection is waiting");
    assert_eq!(state(&host.stack, server), Some(State::Established));
    assert_eq!(
        host.stack.remote_endpoint(server).map(|e| e.address),
        Some(IpAddress::V4(Ipv4::LOOPBACK))
    );
}

#[test]
fn bytes_cross_a_connection_in_both_directions() {
    let mut host = Alone::new();
    let listener = host.stack.open_tcp(Family::V4);
    host.stack
        .bind(listener, at(Ipv4::LOOPBACK, 8_081))
        .expect("bound");
    host.stack.listen(listener, 4).expect("listening");
    let client = host.stack.open_tcp(Family::V4);
    host.stack
        .connect(client, at(Ipv4::LOOPBACK, 8_081), host.now)
        .expect("connecting");
    host.settle();
    let server = host.stack.accept(listener).expect("accepted");

    let now = host.now;
    let _ = host
        .stack
        .send(client, b"question", None, now)
        .expect("sent");
    host.settle();
    assert_eq!(host.drain(server), b"question");

    let now = host.now;
    let _ = host.stack.send(server, b"answer", None, now).expect("sent");
    host.settle();
    assert_eq!(host.drain(client), b"answer");
}

#[test]
fn a_connection_crosses_a_link_and_resolves_its_peer_first() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V4);
    wire.two.bind(listener, at(TWO, 80)).expect("bound");
    wire.two.listen(listener, 4).expect("listening");

    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(TWO, 80), now)
        .expect("connecting");
    wire.settle();

    assert_eq!(state(&wire.one, client), Some(State::Established));
    let server = wire.two.accept(listener).expect("accepted");
    assert_eq!(
        wire.two.remote_endpoint(server).map(|e| e.address),
        Some(IpAddress::V4(ONE))
    );
}

#[test]
fn a_body_larger_than_the_windows_arrives_in_order_over_a_link() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V4);
    wire.two.bind(listener, at(TWO, 81)).expect("bound");
    wire.two.listen(listener, 4).expect("listening");
    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(TWO, 81), now)
        .expect("connecting");
    wire.settle();
    let server = wire.two.accept(listener).expect("accepted");

    let sent = body(200_000);
    let mut offset = 0;
    let mut received = Vec::new();
    for _ in 0..4_000 {
        if offset < sent.len() {
            let now = wire.now;
            let rest = sent.get(offset..).unwrap_or_default();
            if let Ok(taken) = wire.one.send(client, rest, None, now) {
                offset += taken;
            }
        }
        wire.settle();
        received.extend(wire.drain(Host::Two, server));
        if offset == sent.len() && received.len() >= sent.len() {
            break;
        }
    }
    assert_eq!(received.len(), sent.len());
    assert_eq!(received, sent);
}

#[test]
fn a_connection_to_a_port_nobody_listens_on_is_refused() {
    let mut wire = Wire::new();
    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(TWO, 9_999), now)
        .expect("the address is routable");
    wire.settle();
    assert_eq!(state(&wire.one, client), Some(State::Closed));
    assert_eq!(wire.one.take_error(client), Some(Error::Refused));
    assert!(wire.two.counters().resets_sent >= 1);
}

#[test]
fn closing_one_end_is_seen_as_the_end_of_the_stream_at_the_other() {
    let mut host = Alone::new();
    let listener = host.stack.open_tcp(Family::V4);
    host.stack
        .bind(listener, at(Ipv4::LOOPBACK, 8_082))
        .expect("bound");
    host.stack.listen(listener, 4).expect("listening");
    let client = host.stack.open_tcp(Family::V4);
    host.stack
        .connect(client, at(Ipv4::LOOPBACK, 8_082), host.now)
        .expect("connecting");
    host.settle();
    let server = host.stack.accept(listener).expect("accepted");

    let now = host.now;
    let _ = host
        .stack
        .send(client, b"last words", None, now)
        .expect("sent");
    host.stack
        .shutdown(client, crate::socket::Shutdown::Write)
        .expect("a stream may be shut down");
    host.settle();

    assert_eq!(host.drain(server), b"last words");
    let mut out = [0_u8; 16];
    let end = host
        .stack
        .recv(server, &mut out, false)
        .expect("a clean end");
    assert_eq!(end.bytes, 0, "the end of a stream is a read of nothing");
}

#[test]
fn a_listener_takes_several_connections_and_hands_them_out_in_order() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V4);
    wire.two.bind(listener, at(TWO, 82)).expect("bound");
    wire.two.listen(listener, 8).expect("listening");

    let mut clients = Vec::new();
    for _ in 0..4 {
        let client = wire.one.open_tcp(Family::V4);
        let now = wire.now;
        wire.one
            .connect(client, at(TWO, 82), now)
            .expect("connecting");
        clients.push(client);
        wire.settle();
    }
    wire.settle();

    let mut accepted = Vec::new();
    while let Ok(server) = wire.two.accept(listener) {
        accepted.push(server);
    }
    assert_eq!(accepted.len(), 4);
    for server in &accepted {
        assert_eq!(state(&wire.two, *server), Some(State::Established));
    }
}

#[test]
fn an_accept_with_nobody_waiting_says_so_rather_than_failing() {
    let mut host = Alone::new();
    let listener = host.stack.open_tcp(Family::V4);
    host.stack
        .bind(listener, at(Ipv4::LOOPBACK, 8_083))
        .expect("bound");
    host.stack.listen(listener, 4).expect("listening");
    assert_eq!(host.stack.accept(listener), Err(Error::WouldBlock));
}

#[test]
fn a_connection_to_an_unroutable_address_fails_at_once() {
    let mut host = Alone::new();
    let client = host.stack.open_tcp(Family::V4);
    let answer = host
        .stack
        .connect(client, at(Ipv4::new([203, 0, 113, 1]), 80), host.now);
    assert_eq!(answer, Err(Error::Unreachable));
}

#[test]
fn a_listener_and_a_connection_may_share_a_port() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V4);
    wire.two.bind(listener, at(TWO, 83)).expect("bound");
    wire.two.listen(listener, 4).expect("listening");
    let first = wire.one.open_tcp(Family::V4);
    let second = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(first, at(TWO, 83), now)
        .expect("connecting");
    wire.settle();
    let now = wire.now;
    wire.one
        .connect(second, at(TWO, 83), now)
        .expect("connecting");
    wire.settle();

    assert_eq!(state(&wire.one, first), Some(State::Established));
    assert_eq!(state(&wire.one, second), Some(State::Established));
    let one = wire.two.accept(listener).expect("the first");
    let two = wire.two.accept(listener).expect("the second");
    assert_ne!(one, two);
}

#[test]
fn a_send_on_a_socket_that_is_still_connecting_says_so() {
    let mut wire = Wire::new();
    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(TWO, 84), now)
        .expect("connecting");
    let answer = wire.one.send(client, b"too early", None, now);
    assert_eq!(answer, Err(Error::InProgress));
}

#[test]
fn a_wildcard_listener_answers_a_connection_to_any_of_this_hosts_addresses() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V4);
    wire.two
        .bind(
            listener,
            Endpoint::new(IpAddress::V4(Ipv4::UNSPECIFIED), 85),
        )
        .expect("the unspecified address may be bound");
    wire.two.listen(listener, 4).expect("listening");
    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(TWO, 85), now)
        .expect("connecting");
    wire.settle();
    assert_eq!(state(&wire.one, client), Some(State::Established));
    assert!(wire.two.accept(listener).is_ok());
}

#[test]
fn a_connection_that_both_ends_closed_is_forgotten_after_time_wait() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V4);
    wire.two.bind(listener, at(TWO, 86)).expect("bound");
    wire.two.listen(listener, 4).expect("listening");
    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(TWO, 86), now)
        .expect("connecting");
    wire.settle();
    let server = wire.two.accept(listener).expect("accepted");

    // Closing is exchanged without letting the clock move, so the wait that
    // follows is still ahead rather than already over.
    wire.one.close(client);
    wire.exchange();
    wire.two.close(server);
    wire.exchange();

    // One end waits out the segments that may still be in the network; the
    // other is done at once.
    assert_eq!(state(&wire.one, client), Some(State::TimeWait));
    wire.advance(ferrix_nettcp::conn::TIME_WAIT + 1);
    wire.exchange();
    assert!(
        wire.one.socket(client).is_none(),
        "a connection that finished should not still be in the table"
    );
}

#[test]
fn a_connection_carries_bytes_over_ipv6() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V6);
    wire.two
        .bind(listener, at_v6(TWO_V6, 90))
        .expect("the host owns that address");
    wire.two.listen(listener, 4).expect("listening");

    let client = wire.one.open_tcp(Family::V6);
    let now = wire.now;
    wire.one
        .connect(client, at_v6(TWO_V6, 90), now)
        .expect("connecting");
    wire.settle();
    assert_eq!(state(&wire.one, client), Some(State::Established));
    let server = wire.two.accept(listener).expect("accepted");

    let now = wire.now;
    let _ = wire.one.send(client, b"over six", None, now).expect("sent");
    wire.settle();
    assert_eq!(wire.drain(Host::Two, server), b"over six");
}

#[test]
fn the_host_that_started_the_connection_can_also_be_the_one_that_reads() {
    let mut wire = Wire::new();
    let listener = wire.two.open_tcp(Family::V4);
    wire.two.bind(listener, at(TWO, 87)).expect("bound");
    wire.two.listen(listener, 4).expect("listening");
    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(TWO, 87), now)
        .expect("connecting");
    wire.settle();
    let server = wire.two.accept(listener).expect("accepted");

    let now = wire.now;
    let _ = wire
        .two
        .send(server, b"greetings", None, now)
        .expect("sent");
    wire.settle();
    assert_eq!(wire.drain(Host::One, client), b"greetings");
}

/// Two sockets that both set `SO_REUSEADDR`, as tokio's listeners and a
/// service manager's do.
fn reusing(host: &mut Alone) -> [crate::socket::SocketId; 2] {
    [0, 1].map(|_| {
        let socket = host.stack.open_tcp(Family::V4);
        host.stack
            .socket_mut(socket)
            .expect("made")
            .options_mut()
            .reuse_address = true;
        socket
    })
}

#[test]
fn a_port_a_socket_listens_on_is_its_own_whatever_so_reuseaddr_says() {
    let mut host = Alone::new();
    let [first, second] = reusing(&mut host);
    host.stack
        .bind(first, at(Ipv4::UNSPECIFIED, 2_200))
        .expect("bound");
    host.stack.listen(first, 4).expect("listening");
    assert_eq!(
        host.stack.bind(second, at(Ipv4::UNSPECIFIED, 2_200)),
        Err(Error::AddressInUse),
        "the same wildcard"
    );
    assert_eq!(
        host.stack.bind(second, at(Ipv4::LOOPBACK, 2_200)),
        Err(Error::AddressInUse),
        "an address the wildcard covers"
    );
    host.stack
        .bind(second, at(Ipv4::LOOPBACK, 2_201))
        .expect("another port");
}

#[test]
fn of_two_sockets_bound_side_by_side_only_the_first_may_listen() {
    let mut host = Alone::new();
    let [first, second] = reusing(&mut host);
    host.stack
        .bind(first, at(Ipv4::UNSPECIFIED, 2_202))
        .expect("bound");
    host.stack
        .bind(second, at(Ipv4::UNSPECIFIED, 2_202))
        .expect("SO_REUSEADDR lets two sockets bind a port nobody listens on");
    host.stack.listen(first, 4).expect("listening");
    assert_eq!(host.stack.listen(second, 4), Err(Error::AddressInUse));
}

#[test]
fn a_port_is_free_again_once_its_listener_has_gone() {
    let mut host = Alone::new();
    let [first, second] = reusing(&mut host);
    host.stack
        .bind(first, at(Ipv4::UNSPECIFIED, 2_203))
        .expect("bound");
    host.stack.listen(first, 4).expect("listening");
    host.stack.close(first);
    host.stack
        .bind(second, at(Ipv4::UNSPECIFIED, 2_203))
        .expect("free again");
    host.stack
        .listen(second, 4)
        .expect("listening in its place");
}

#[test]
fn a_connection_to_the_unspecified_address_is_refused_by_the_loopback_at_once() {
    // A hosts file's `0.0.0.0 blocked.example` must fail the connect at
    // once, as on Linux, not after the SYN's retransmits out of the default
    // route: the clock does not move here.
    let mut wire = Wire::new();
    let client = wire.one.open_tcp(Family::V4);
    let now = wire.now;
    wire.one
        .connect(client, at(Ipv4::UNSPECIFIED, 443), now)
        .expect("the unspecified address means this host");
    wire.exchange();
    assert_eq!(state(&wire.one, client), Some(State::Closed));
    assert_eq!(wire.one.take_error(client), Some(Error::Refused));
    assert_eq!(
        wire.one.remote_endpoint(client),
        Some(at(Ipv4::LOOPBACK, 443))
    );
}

#[test]
fn a_connection_to_the_unspecified_address_reaches_a_loopback_listener() {
    let mut host = Alone::new();
    let listener = host.stack.open_tcp(Family::V4);
    host.stack
        .bind(listener, at(Ipv4::LOOPBACK, 8_090))
        .expect("bound");
    host.stack.listen(listener, 4).expect("listening");
    let client = host.stack.open_tcp(Family::V4);
    host.stack
        .connect(client, at(Ipv4::UNSPECIFIED, 8_090), host.now)
        .expect("a connect to 0.0.0.0");
    host.settle();
    assert_eq!(state(&host.stack, client), Some(State::Established));
    assert!(host.stack.accept(listener).is_ok());
}

#[test]
fn a_connection_to_the_ipv6_unspecified_address_is_refused_by_the_loopback_at_once() {
    let mut wire = Wire::new();
    let client = wire.one.open_tcp(Family::V6);
    let now = wire.now;
    wire.one
        .connect(client, at_v6(crate::addr::Ipv6::UNSPECIFIED, 443), now)
        .expect("`::` means this host");
    wire.exchange();
    assert_eq!(state(&wire.one, client), Some(State::Closed));
    assert_eq!(wire.one.take_error(client), Some(Error::Refused));
    assert_eq!(
        wire.one.remote_endpoint(client),
        Some(at_v6(crate::addr::Ipv6::LOOPBACK, 443))
    );
}

#[test]
fn a_bound_socket_s_connection_to_the_unspecified_address_goes_to_its_own_address_on_the_wire() {
    // Linux's __ip_route_output_key: daddr 0 becomes the bound saddr, not
    // 127.0.0.1. Linux then delivers it locally; this stack has no local
    // table yet, so the SYN leaves on eth0 (an ARP for its own address
    // first). This pins today's behaviour; the local table's backlog row
    // changes it.
    let mut wire = Wire::new();
    let client = wire.one.open_tcp(Family::V4);
    wire.one
        .bind(client, at(ONE, 0))
        .expect("the host owns that address");
    let now = wire.now;
    wire.one
        .connect(client, at(Ipv4::UNSPECIFIED, 444), now)
        .expect("0.0.0.0 means the bound address");
    assert_eq!(wire.one.remote_endpoint(client), Some(at(ONE, 444)));
    let frame = wire.one.poll_transmit(now).expect("a frame for the wire");
    assert!(!wire.one.is_loopback(frame.interface));
    assert_eq!(state(&wire.one, client), Some(State::SynSent));
}
