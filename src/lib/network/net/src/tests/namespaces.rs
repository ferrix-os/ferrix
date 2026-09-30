//! The pieces a network namespace is made of: a loopback that starts down,
//! an interface that moves from one stack to another, two stacks joined by a
//! pair of virtual links, and a reassembler with a ceiling of its own
//! (`docs/NETNS.md` sections 2 and 3).

use alloc::vec::Vec;

use crate::addr::{IpAddress, IpCidr, Ipv4};
use crate::iface::{Address, Backing, IFF_LOOPBACK, IFF_UP, Interface};
use crate::reassembly::{Key, NAMESPACE_BYTES, Reassembler};
use crate::socket::{Error, Family};
use crate::stack::{Config, Stack};
use crate::tests::harness::at;

/// A namespace's stack with its loopback up.
fn with_loopback_up() -> Stack {
    let mut stack = Stack::new_namespace(Config::default());
    stack.seed(0x5EED_0000_0000_0001);
    stack.set_up(1, true).expect("the loopback is always there");
    stack
}

/// Drive a stack until it has no frame to hand out, as a loopback needs.
fn idle(stack: &mut Stack) {
    let mut now = 0;
    for _ in 0..1_000 {
        if stack.poll_transmit(now).is_some() {
            continue;
        }
        let Some(at) = stack.poll_at() else {
            return;
        };
        now = at.max(now);
        stack.on_timer(now);
    }
}

/// An address with a prefix.
fn address(octets: [u8; 4], prefix: u8) -> Address {
    Address {
        cidr: IpCidr::new(IpAddress::V4(Ipv4::new(octets)), prefix),
        peer: None,
    }
}

/// An Ethernet interface backed as one end of a pair, up and running.
fn veth_end(stack: &mut Stack, name: &[u8], mac: u8, pair: u64, end: u8, ip: [u8; 4]) -> u32 {
    let mut interface = Interface::ethernet(0, name, [0x02, 0, 0, 0, 0, mac], 1500);
    interface.backing = Backing::Veth { pair, end };
    let index = stack.add_interface(interface);
    stack.set_up(index, true).expect("just added");
    stack.interface_mut(index).expect("just added").flags |=
        crate::iface::IFF_RUNNING | crate::iface::IFF_LOWER_UP;
    stack
        .add_address(index, address(ip, 24))
        .expect("an interface takes an address");
    index
}

#[test]
fn a_new_namespace_has_a_loopback_that_is_down_and_owns_nothing() {
    let stack = Stack::new_namespace(Config::default());
    assert_eq!(stack.interfaces().len(), 1);
    let lo = stack.interface(1).expect("the loopback is interface 1");
    assert_eq!(lo.flags, IFF_LOOPBACK);
    assert!(lo.addresses.is_empty());
    assert!(stack.routes().entries().is_empty());
}

#[test]
fn a_down_loopback_refuses_its_address_and_a_route_to_it() {
    let mut stack = Stack::new_namespace(Config::default());
    let socket = stack.open_udp(Family::V4);
    assert_eq!(
        stack.bind(socket, at(Ipv4::LOOPBACK, 7_000)),
        Err(Error::AddressNotAvailable)
    );
    let other = stack.open_udp(Family::V4);
    assert_eq!(
        stack.send(other, b"x", Some(at(Ipv4::LOOPBACK, 7_000)), 0),
        Err(Error::Unreachable)
    );
}

#[test]
fn bringing_the_loopback_up_gives_its_addresses_and_carries_datagrams() {
    let mut stack = with_loopback_up();
    let lo = stack.interface(1).expect("there");
    assert!(lo.is_up());
    assert!(lo.owns(IpAddress::V4(Ipv4::LOOPBACK)));
    assert_eq!(lo.addresses.len(), 2);
    assert!(!stack.routes().entries().is_empty());

    let server = stack.open_udp(Family::V4);
    stack
        .bind(server, at(Ipv4::LOOPBACK, 7_000))
        .expect("the loopback owns 127.0.0.1 now");
    let client = stack.open_udp(Family::V4);
    let _ = stack
        .send(client, b"hello", Some(at(Ipv4::LOOPBACK, 7_000)), 0)
        .expect("there is a route");
    idle(&mut stack);
    let mut out = [0_u8; 16];
    let got = stack.recv(server, &mut out, false).expect("arrived");
    assert_eq!(out.get(..got.bytes), Some(b"hello".as_slice()));
}

#[test]
fn taking_the_loopback_down_takes_its_addresses_and_routes_with_it() {
    let mut stack = with_loopback_up();
    stack.set_up(1, false).expect("there");
    let lo = stack.interface(1).expect("there");
    assert_eq!(lo.flags & IFF_UP, 0);
    assert!(lo.addresses.is_empty());
    assert!(stack.routes().entries().is_empty());
    // And up again gives them back: nothing remembered, nothing lost.
    stack.set_up(1, true).expect("there");
    assert_eq!(stack.interface(1).expect("there").addresses.len(), 2);
}

#[test]
fn two_namespaces_share_no_port_and_no_socket() {
    let mut first = with_loopback_up();
    let mut second = with_loopback_up();
    let one = first.open_udp(Family::V4);
    let two = second.open_udp(Family::V4);
    first
        .bind(one, at(Ipv4::LOOPBACK, 7_000))
        .expect("free here");
    second
        .bind(two, at(Ipv4::LOOPBACK, 7_000))
        .expect("free there too: the port space is the namespace's");

    // A datagram sent in one is received by the socket of that one.
    let client = second.open_udp(Family::V4);
    let _ = second
        .send(client, b"second", Some(at(Ipv4::LOOPBACK, 7_000)), 0)
        .expect("routed");
    idle(&mut second);
    idle(&mut first);
    let mut out = [0_u8; 16];
    assert!(first.recv(one, &mut out, true).is_err(), "nothing crossed");
    assert_eq!(
        second.recv(two, &mut out, false).map(|got| got.bytes),
        Ok(6)
    );
}

#[test]
fn an_interface_moves_with_its_addresses_and_leaves_its_routes() {
    let mut from = Stack::new(Config::default());
    let index = veth_end(&mut from, b"eth1", 1, 7, 0, [192, 168, 7, 1]);
    assert!(
        from.routes()
            .entries()
            .iter()
            .any(|route| route.interface == index)
    );

    let moved = from.detach_interface(index).expect("it is there");
    assert!(from.interface(index).is_none());
    assert!(
        from.routes()
            .entries()
            .iter()
            .all(|route| route.interface != index)
    );
    assert_eq!(moved.addresses.len(), 1);
    assert_eq!(moved.backing, Backing::Veth { pair: 7, end: 0 });

    let mut to = Stack::new_namespace(Config::default());
    let new_index = to
        .attach_interface(moved.clone())
        .expect("the name is free");
    assert_eq!(new_index, 2, "the next free index there");
    assert!(
        to.routes()
            .entries()
            .iter()
            .any(|route| route.interface == new_index),
        "an up interface's address implies its on-link route"
    );
    assert_eq!(to.attach_interface(moved), Err(Error::AddressInUse));
}

#[test]
fn a_name_is_unique_in_a_stack_and_can_change() {
    let mut stack = Stack::new(Config::default());
    let one = veth_end(&mut stack, b"a", 1, 1, 0, [10, 1, 0, 1]);
    let two = veth_end(&mut stack, b"b", 2, 2, 0, [10, 2, 0, 1]);
    assert_eq!(stack.rename_interface(two, b"a"), Err(Error::AddressInUse));
    stack.rename_interface(two, b"c").expect("free");
    assert_eq!(
        stack.interface(two).map(|each| each.name.as_bytes()),
        Some(b"c".as_slice())
    );
    stack
        .rename_interface(one, b"a")
        .expect("its own name again");
}

/// Carry frames between two stacks joined by the two ends of pair 9, which is
/// what the kernel's forwarding does with a `Backing::Veth`.
fn carry(a: &mut Stack, b: &mut Stack, to_b: u32, to_a: u32) {
    let now = 0;
    for _ in 0..2_000 {
        if let Some(out) = a.poll_transmit(now) {
            if matches!(
                a.interface(out.interface).map(|each| each.backing),
                Some(Backing::Veth { pair: 9, end: 0 })
            ) {
                b.receive(to_b, &out.frame, now);
            }
            continue;
        }
        if let Some(out) = b.poll_transmit(now) {
            if matches!(
                b.interface(out.interface).map(|each| each.backing),
                Some(Backing::Veth { pair: 9, end: 1 })
            ) {
                a.receive(to_a, &out.frame, now);
            }
            continue;
        }
        return;
    }
    panic!("the frames never stopped");
}

#[test]
fn two_namespaces_talk_over_a_pair_and_a_third_hears_nothing() {
    let mut a = with_loopback_up();
    let mut b = with_loopback_up();
    let mut c = with_loopback_up();
    let ia = veth_end(&mut a, b"veth0", 1, 9, 0, [10, 9, 0, 1]);
    let ib = veth_end(&mut b, b"veth1", 2, 9, 1, [10, 9, 0, 2]);

    let server = b.open_udp(Family::V4);
    b.bind(server, at(Ipv4::new([10, 9, 0, 2]), 5_353))
        .expect("the end owns that address");
    let listener = c.open_udp(Family::V4);
    c.bind(listener, at(Ipv4::new([0, 0, 0, 0]), 5_353))
        .expect("a wildcard in the third namespace");

    let client = a.open_udp(Family::V4);
    let _ = a
        .send(
            client,
            b"across",
            Some(at(Ipv4::new([10, 9, 0, 2]), 5_353)),
            0,
        )
        .expect("the pair's prefix is routed");
    carry(&mut a, &mut b, ib, ia);

    let mut out = [0_u8; 32];
    let got = b
        .recv(server, &mut out, false)
        .expect("it crossed the pair");
    assert_eq!(out.get(..got.bytes), Some(b"across".as_slice()));
    assert_eq!(
        got.remote.map(|endpoint| endpoint.address),
        Some(IpAddress::V4(Ipv4::new([10, 9, 0, 1])))
    );
    idle(&mut c);
    assert!(
        c.recv(listener, &mut out, true).is_err(),
        "the third namespace saw nothing"
    );
}

#[test]
fn a_namespace_reassembles_no_more_than_its_ceiling() {
    let key = |identification| Key {
        source: Ipv4::new([10, 0, 0, 1]),
        destination: Ipv4::new([10, 0, 0, 2]),
        identification,
        protocol: 17,
    };
    let mut small = Reassembler::with_limit(NAMESPACE_BYTES);
    let chunk: Vec<u8> = alloc::vec![0_u8; 1_480];
    let mut held = 0;
    for piece in 0..40_usize {
        // Pieces of four datagrams that are never finished.
        let key = key((piece % 4) as u16);
        let _ = small.insert(key, (piece / 4) * chunk.len(), true, &chunk, 0);
        held = small.held();
    }
    assert!(held <= NAMESPACE_BYTES, "held {held} past the ceiling");
    assert!(
        held > NAMESPACE_BYTES - 2 * chunk.len(),
        "and it held up to it"
    );
    small.flush();
    assert_eq!(small.held(), 0);
}
