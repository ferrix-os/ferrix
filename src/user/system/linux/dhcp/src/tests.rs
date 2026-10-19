//! The client's logic on the host: messages both ways, the headers around
//! them, a lease walked through T1, T2 and its end on a clock the test
//! moves, the netlink requests, and the command line.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test that cannot go on is a failed test, and says where"
)]

use std::net::Ipv4Addr;
use std::time::Duration;

use ferrix_linux_abi::netlink::{
    IFA_BROADCAST, IFA_LOCAL, IFLA_ADDRESS, IFLA_IFNAME, IfAddrMsg, IfInfoMsg, NLM_F_ACK,
    NLM_F_REQUEST, NLMSG_DONE, NLMSG_ERROR, NlMsgErr, NlMsgHdr, RTA_GATEWAY, RTA_OIF, RTM_NEWADDR,
    RTM_NEWLINK, RTM_NEWROUTE, RtMsg,
};
use ferrix_netlink::{Address, Attr, Messages, Value, Writer};

use crate::args::{self, Args};
use crate::client::{Action, Client, Config, Destination, lease_of};
use crate::message::{FIXED_LEN, MAGIC, MIN_LEN, Malformed, Reply, Request, kind, option};
use crate::{Lease, broadcast_of, frame, prefix_of, rtnl};

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const SERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// A reply as a server writes it: the gateway's shape (`gateway/dhcp.rs`),
/// with `options` after the message type.
fn reply_bytes(
    kind: u8,
    xid: u32,
    mac: [u8; 6],
    yiaddr: Ipv4Addr,
    options: &[(u8, &[u8])],
) -> Vec<u8> {
    let mut bytes = vec![0_u8; FIXED_LEN];
    bytes[..4].copy_from_slice(&[2, 1, 6, 0]);
    bytes[4..8].copy_from_slice(&xid.to_be_bytes());
    bytes[16..20].copy_from_slice(&yiaddr.octets());
    bytes[20..24].copy_from_slice(&SERVER.octets());
    bytes[28..34].copy_from_slice(&mac);
    bytes[236..240].copy_from_slice(&MAGIC);
    bytes.extend_from_slice(&[option::KIND, 1, kind]);
    for (code, value) in options {
        bytes.push(*code);
        bytes.push(u8::try_from(value.len()).unwrap());
        bytes.extend_from_slice(value);
    }
    bytes.push(option::END);
    bytes.resize(bytes.len().max(MIN_LEN), 0);
    bytes
}

/// The gateway's OFFER or ACK: a day's lease, a /24, router and DNS.
fn gateway(kind: u8, xid: u32, lease: u32) -> Reply {
    let bytes = reply_bytes(
        kind,
        xid,
        MAC,
        GUEST,
        &[
            (option::SERVER, &SERVER.octets()),
            (option::LEASE, &lease.to_be_bytes()),
            (option::NETMASK, &[255, 255, 255, 0]),
            (option::ROUTER, &SERVER.octets()),
            (option::DNS, &DNS.octets()),
        ],
    );
    Reply::parse(&bytes).unwrap()
}

fn nak(xid: u32) -> Reply {
    Reply::parse(&reply_bytes(
        kind::NAK,
        xid,
        MAC,
        Ipv4Addr::UNSPECIFIED,
        &[(option::SERVER, &SERVER.octets())],
    ))
    .unwrap()
}

fn config(tries: u32, exit_without_lease: bool, quit_with_lease: bool) -> Config {
    Config {
        mac: MAC,
        tries,
        timeout: Duration::from_secs(2),
        pause: Duration::from_secs(20),
        exit_without_lease,
        quit_with_lease,
    }
}

/// The one message among `actions`, read back as the server would.
fn sent(actions: &[Action]) -> (u8, Vec<u8>, Destination) {
    let sends: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Send { kind, message, to } => Some((*kind, message.clone(), to.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(sends.len(), 1, "one message in {actions:?}");
    sends.into_iter().next().unwrap()
}

/// The value of option `code` in a request's bytes.
fn request_option(message: &[u8], code: u8) -> Option<Vec<u8>> {
    let mut rest = &message[FIXED_LEN..];
    while let Some((&at, tail)) = rest.split_first() {
        match at {
            option::END => return None,
            option::PAD => rest = tail,
            _ => {
                let len = usize::from(tail[0]);
                if at == code {
                    return Some(tail[1..=len].to_vec());
                }
                rest = &tail[1 + len..];
            }
        }
    }
    None
}

fn xid_of(message: &[u8]) -> u32 {
    u32::from_be_bytes(message[4..8].try_into().unwrap())
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// --- Messages -----------------------------------------------------------

#[test]
fn a_discover_is_a_bootrequest_with_the_options_a_server_needs() {
    let message = Request {
        kind: kind::DISCOVER,
        xid: 0x1234_5678,
        secs: 3,
        mac: MAC,
        ciaddr: Ipv4Addr::UNSPECIFIED,
        requested: None,
        server: None,
    }
    .encode();
    assert!(message.len() >= MIN_LEN);
    assert_eq!(&message[..4], &[1, 1, 6, 0]);
    assert_eq!(xid_of(&message), 0x1234_5678);
    assert_eq!(&message[8..10], &[0, 3]);
    assert_eq!(&message[28..34], &MAC);
    assert_eq!(&message[236..240], &MAGIC);
    assert_eq!(
        request_option(&message, option::KIND),
        Some(vec![kind::DISCOVER])
    );
    let mut id = vec![1];
    id.extend_from_slice(&MAC);
    assert_eq!(request_option(&message, option::CLIENT_ID), Some(id));
    let wanted = request_option(&message, option::PARAMETERS).unwrap();
    for code in [option::NETMASK, option::ROUTER, option::DNS, option::LEASE] {
        assert!(wanted.contains(&code), "option {code} asked for");
    }
    assert_eq!(request_option(&message, option::REQUESTED), None);
    assert_eq!(request_option(&message, option::SERVER), None);
}

#[test]
fn a_reply_is_read_as_the_gateway_writes_it() {
    let reply = gateway(kind::OFFER, 7, 86_400);
    assert_eq!(reply.kind, kind::OFFER);
    assert_eq!(reply.xid, 7);
    assert_eq!(reply.mac, MAC);
    assert_eq!(reply.yiaddr, GUEST);
    assert_eq!(reply.server, Some(SERVER));
    assert_eq!(reply.lease, Some(86_400));
    assert_eq!(reply.netmask, Some(Ipv4Addr::new(255, 255, 255, 0)));
    assert_eq!(reply.routers, [SERVER]);
    assert_eq!(reply.dns, [DNS]);
}

#[test]
fn a_reply_that_is_not_one_is_refused_for_its_reason() {
    let good = reply_bytes(kind::ACK, 1, MAC, GUEST, &[]);
    assert_eq!(Reply::parse(&good[..FIXED_LEN - 1]), Err(Malformed::Short));
    let mut request = good.clone();
    request[0] = 1;
    assert_eq!(Reply::parse(&request), Err(Malformed::NotAReply));
    let mut cookie = good.clone();
    cookie[239] = 0;
    assert_eq!(Reply::parse(&cookie), Err(Malformed::NotAReply));
    // An option whose length runs past the end of the message.
    let mut long = good[..FIXED_LEN].to_vec();
    long.extend_from_slice(&[option::KIND, 1, kind::ACK, option::DNS, 200, 1, 2]);
    assert_eq!(Reply::parse(&long), Err(Malformed::Option));
    let mut untyped = good[..FIXED_LEN].to_vec();
    untyped.push(option::END);
    assert_eq!(Reply::parse(&untyped), Err(Malformed::NoKind));
}

#[test]
fn options_split_in_two_are_joined_and_overloaded_fields_are_read() {
    // Two name servers, sent as two instances of option 6 (RFC 3396).
    let split = reply_bytes(
        kind::ACK,
        1,
        MAC,
        GUEST,
        &[(option::DNS, &[1, 1, 1, 1]), (option::DNS, &[8, 8, 8, 8])],
    );
    assert_eq!(
        Reply::parse(&split).unwrap().dns,
        [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
    );
    // The router in `file`, which option 52 says holds options.
    let mut overloaded = reply_bytes(kind::ACK, 1, MAC, GUEST, &[(option::OVERLOAD, &[1])]);
    overloaded[108..115].copy_from_slice(&[option::ROUTER, 4, 10, 0, 2, 1, option::END]);
    assert_eq!(
        Reply::parse(&overloaded).unwrap().routers,
        [Ipv4Addr::new(10, 0, 2, 1)]
    );
}

#[test]
fn a_domain_is_kept_only_when_it_is_a_name() {
    let named = reply_bytes(
        kind::ACK,
        1,
        MAC,
        GUEST,
        &[(option::DOMAIN, b"ferrix.test\0")],
    );
    assert_eq!(
        Reply::parse(&named).unwrap().domain.as_deref(),
        Some("ferrix.test")
    );
    // A line break would write a second line into resolv.conf.
    let hostile = reply_bytes(
        kind::ACK,
        1,
        MAC,
        GUEST,
        &[(option::DOMAIN, b"a\nnameserver 6.6.6.6")],
    );
    assert_eq!(Reply::parse(&hostile).unwrap().domain, None);
}

// --- Frames -------------------------------------------------------------

#[test]
fn a_message_goes_out_in_udp_in_ipv4_and_comes_back_out_of_them() {
    let message = b"a dhcp message".to_vec();
    let packet = frame::wrap(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, &message).unwrap();
    assert_eq!(packet[0], 0x45);
    assert_eq!(packet[9], 17);
    assert_eq!(&packet[16..20], &[255; 4]);
    assert_eq!(&packet[20..24], &[0, 68, 0, 67]);
    // What the client sends is what a server unwraps; turn the ports round
    // to read it as the client reads a reply.
    let mut reply = frame::wrap(SERVER, Ipv4Addr::BROADCAST, &message).unwrap();
    reply[20..24].copy_from_slice(&[0, 67, 0, 68]);
    // The UDP checksum covered the ports: recompute it as a server would.
    reply[26..28].copy_from_slice(&[0, 0]);
    let (source, payload) = frame::unwrap(&reply).expect("an unchecksummed reply is read");
    assert_eq!(source, SERVER);
    assert_eq!(payload, message.as_slice());
}

#[test]
fn somebody_elses_traffic_and_a_damaged_packet_are_not_read() {
    let mut reply = frame::wrap(SERVER, Ipv4Addr::BROADCAST, b"x").unwrap();
    // Ports 68 to 67: a client's message, not a server's.
    assert_eq!(frame::unwrap(&reply), None);
    reply[20..24].copy_from_slice(&[0, 67, 0, 68]);
    reply[26..28].copy_from_slice(&[0, 0]);
    assert!(frame::unwrap(&reply).is_some());
    // A bad IP header checksum.
    let mut damaged = reply.clone();
    damaged[8] ^= 1;
    assert_eq!(frame::unwrap(&damaged), None);
    // A UDP checksum that does not hold.
    // (Swapping the two ports keeps the ones'-complement sum, so the
    // checksum still holds after the swap; a changed payload byte does not.)
    let mut checked = frame::wrap(SERVER, Ipv4Addr::BROADCAST, b"x").unwrap();
    checked[20..24].copy_from_slice(&[0, 67, 0, 68]);
    assert!(frame::unwrap(&checked).is_some());
    checked[28] ^= 0x40;
    assert_eq!(
        frame::unwrap(&checked),
        None,
        "the payload changed under the checksum"
    );
    // TCP is not UDP.
    let mut tcp = reply;
    tcp[9] = 6;
    tcp[10..12].copy_from_slice(&[0, 0]);
    let sum = ferrix_netwire::checksum::checksum(&tcp[..20]);
    tcp[10..12].copy_from_slice(&sum.to_be_bytes());
    assert_eq!(frame::unwrap(&tcp), None);
}

// --- The state machine --------------------------------------------------

/// A client walked to a day-long lease at 1 s: DISCOVER, OFFER, REQUEST,
/// ACK.
fn bound(quit: bool) -> (Client, Vec<Action>) {
    let mut client = Client::new(config(5, true, quit), 42);
    let (what, discover, to) = sent(&client.tick(Duration::ZERO));
    assert_eq!(what, kind::DISCOVER);
    assert_eq!(
        request_option(&discover, option::KIND),
        Some(vec![kind::DISCOVER])
    );
    assert_eq!(
        to,
        Destination::Broadcast {
            from: Ipv4Addr::UNSPECIFIED
        }
    );
    let xid = xid_of(&discover);
    let (what, request, to) = sent(&client.receive(secs(1), &gateway(kind::OFFER, xid, 86_400)));
    assert_eq!(what, kind::REQUEST);
    assert_eq!(xid_of(&request), xid);
    assert_eq!(
        request_option(&request, option::REQUESTED),
        Some(GUEST.octets().to_vec())
    );
    assert_eq!(
        request_option(&request, option::SERVER),
        Some(SERVER.octets().to_vec())
    );
    assert_eq!(
        to,
        Destination::Broadcast {
            from: Ipv4Addr::UNSPECIFIED
        }
    );
    let actions = client.receive(secs(1), &gateway(kind::ACK, xid, 86_400));
    (client, actions)
}

#[test]
fn discover_offer_request_ack_binds_the_lease_and_says_it_in_one_line() {
    let (client, actions) = bound(false);
    let [Action::Bound(lease)] = actions.as_slice() else {
        panic!("one Bound in {actions:?}");
    };
    assert_eq!(
        lease.describe(),
        "10.0.2.15/24 by DHCP, router 10.0.2.2, DNS 10.0.2.3, lease 86400 s"
    );
    assert_eq!((lease.renew, lease.rebind), (43_200, 75_600));
    assert!(client.is_bound());
    assert_eq!(client.wake(), Some(secs(1 + 43_200)));
}

#[test]
fn with_q_the_client_exits_once_the_lease_is_applied() {
    let (client, actions) = bound(true);
    assert!(matches!(
        actions.as_slice(),
        [Action::Bound(_), Action::Exit(0)]
    ));
    assert!(client.is_done());
    assert_eq!(client.wake(), None);
}

#[test]
fn at_t1_it_renews_with_its_server_and_at_t2_asks_any_server() {
    let (mut client, _) = bound(false);
    let t1 = secs(1 + 43_200);
    let t2 = secs(1 + 75_600);
    assert!(client.tick(t1 - secs(1)).is_empty(), "nothing before T1");
    let (what, renew, to) = sent(&client.tick(t1));
    assert_eq!(what, kind::REQUEST);
    assert_eq!(
        to,
        Destination::Server {
            from: GUEST,
            to: SERVER
        }
    );
    assert_eq!(
        &renew[12..16],
        &GUEST.octets(),
        "ciaddr is the address held"
    );
    assert_eq!(request_option(&renew, option::REQUESTED), None);
    assert_eq!(request_option(&renew, option::SERVER), None);
    // Unanswered: tried again at half the time left to T2, and again, until
    // T2 itself, where the REQUEST is broadcast.
    let mut now = t1;
    let mut tries = 1;
    loop {
        let due = client.wake().unwrap();
        assert!(due > now && due <= t2, "the next try is between now and T2");
        now = due;
        let (_, _, to) = sent(&client.tick(now));
        if now == t2 {
            assert_eq!(to, Destination::Broadcast { from: GUEST });
            break;
        }
        assert_eq!(
            to,
            Destination::Server {
                from: GUEST,
                to: SERVER
            }
        );
        tries += 1;
        assert!(tries < 20, "renewing ends at T2");
    }
    assert!(tries > 2, "renewing tried more than once: {tries}");
    // Any server's ACK extends the lease, from now.
    let xid = xid_of(&renew);
    let actions = client.receive(now, &gateway(kind::ACK, xid, 86_400));
    let [Action::Renewed { old, new }] = actions.as_slice() else {
        panic!("one Renewed in {actions:?}");
    };
    assert!(old.same_configuration(new));
    assert!(client.is_bound());
    assert_eq!(client.wake(), Some(now + secs(43_200)));
}

#[test]
fn a_lease_nobody_extends_is_lost_at_its_end_and_the_client_asks_again() {
    let (mut client, _) = bound(false);
    let ends = secs(1 + 86_400);
    let mut now = Duration::ZERO;
    let mut lost = None;
    for _ in 0..100 {
        now = client.wake().unwrap();
        let actions = client.tick(now);
        if let Some(Action::Lost(lease)) = actions.first() {
            lost = Some(lease.clone());
            let (what, _, to) = sent(&actions);
            assert_eq!(what, kind::DISCOVER);
            assert_eq!(
                to,
                Destination::Broadcast {
                    from: Ipv4Addr::UNSPECIFIED
                }
            );
            break;
        }
    }
    assert_eq!(now, ends, "lost exactly when it ran out");
    assert_eq!(lost.unwrap().address, GUEST);
    assert!(client.lease().is_none());
}

#[test]
fn a_nak_while_renewing_loses_the_lease_at_once() {
    let (mut client, _) = bound(false);
    let t1 = client.wake().unwrap();
    let (_, renew, _) = sent(&client.tick(t1));
    let actions = client.receive(t1, &nak(xid_of(&renew)));
    assert!(matches!(actions.first(), Some(Action::Lost(_))));
    let (what, discover, _) = sent(&actions);
    assert_eq!(what, kind::DISCOVER);
    assert_ne!(xid_of(&discover), xid_of(&renew), "a new transaction");
}

#[test]
fn a_nak_to_the_request_starts_over_with_a_new_transaction() {
    let mut client = Client::new(config(3, true, false), 9);
    let (_, discover, _) = sent(&client.tick(Duration::ZERO));
    let xid = xid_of(&discover);
    let _ = client.receive(secs(1), &gateway(kind::OFFER, xid, 3600));
    let (what, again, _) = sent(&client.receive(secs(1), &nak(xid)));
    assert_eq!(what, kind::DISCOVER);
    assert_ne!(xid_of(&again), xid);
}

#[test]
fn replies_to_another_transaction_or_another_client_change_nothing() {
    let mut client = Client::new(config(3, true, false), 5);
    let (_, discover, _) = sent(&client.tick(Duration::ZERO));
    let xid = xid_of(&discover);
    assert!(
        client
            .receive(secs(1), &gateway(kind::OFFER, xid ^ 1, 3600))
            .is_empty()
    );
    let mut other = gateway(kind::OFFER, xid, 3600);
    other.mac[5] ^= 1;
    assert!(client.receive(secs(1), &other).is_empty());
    // An ACK where an OFFER is awaited is not taken either.
    assert!(
        client
            .receive(secs(1), &gateway(kind::ACK, xid, 3600))
            .is_empty()
    );
    assert!(client.lease().is_none());
}

#[test]
fn with_n_a_round_with_no_answer_exits_with_status_1() {
    let mut client = Client::new(config(5, true, false), 1);
    let mut discovers = 0;
    let mut now = Duration::ZERO;
    let mut last = Vec::new();
    while let Some(due) = client.wake() {
        now = due;
        last = client.tick(now);
        discovers += last
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    Action::Send {
                        kind: kind::DISCOVER,
                        ..
                    }
                )
            })
            .count();
    }
    assert_eq!(discovers, 5);
    assert_eq!(now, secs(10), "five tries two seconds apart");
    assert!(matches!(
        last.as_slice(),
        [Action::NoLease, Action::Exit(1)]
    ));
}

#[test]
fn without_n_it_pauses_and_tries_again() {
    let mut client = Client::new(config(2, false, false), 1);
    let _ = client.tick(Duration::ZERO);
    let _ = client.tick(secs(2));
    assert!(matches!(client.tick(secs(4)).as_slice(), [Action::NoLease]));
    assert_eq!(client.wake(), Some(secs(24)));
    let (what, _, _) = sent(&client.tick(secs(24)));
    assert_eq!(what, kind::DISCOVER);
}

#[test]
fn t1_and_t2_come_from_the_server_only_when_they_are_in_order() {
    let mut reply = gateway(kind::ACK, 1, 1000);
    reply.renew = Some(100);
    reply.rebind = Some(200);
    let lease = lease_of(&reply, SERVER);
    assert_eq!((lease.renew, lease.rebind), (100, 200));
    reply.renew = Some(300);
    let lease = lease_of(&reply, SERVER);
    assert_eq!(
        (lease.renew, lease.rebind),
        (500, 875),
        "T1 after T2 is ignored"
    );
    reply.lease = Some(u32::MAX);
    let lease = lease_of(&reply, SERVER);
    assert_eq!(lease.seconds, u32::MAX);
    assert!(lease.describe().ends_with("lease for ever"));
}

#[test]
fn a_lease_for_ever_has_no_timer() {
    let mut client = Client::new(config(3, true, false), 3);
    let (_, discover, _) = sent(&client.tick(Duration::ZERO));
    let xid = xid_of(&discover);
    let _ = client.receive(secs(1), &gateway(kind::OFFER, xid, u32::MAX));
    let _ = client.receive(secs(1), &gateway(kind::ACK, xid, u32::MAX));
    assert!(client.is_bound());
    assert_eq!(client.wake(), None);
}

// --- The lease's files and numbers --------------------------------------

#[test]
fn masks_prefixes_and_broadcast_addresses() {
    assert_eq!(prefix_of(Ipv4Addr::new(255, 255, 255, 0)), Some(24));
    assert_eq!(prefix_of(Ipv4Addr::new(255, 255, 240, 0)), Some(20));
    assert_eq!(prefix_of(Ipv4Addr::new(0, 0, 0, 0)), Some(0));
    assert_eq!(prefix_of(Ipv4Addr::new(255, 255, 255, 255)), Some(32));
    assert_eq!(prefix_of(Ipv4Addr::new(255, 0, 255, 0)), None);
    assert_eq!(broadcast_of(GUEST, 24), Ipv4Addr::new(10, 0, 2, 255));
    assert_eq!(broadcast_of(GUEST, 32), GUEST);
    assert_eq!(broadcast_of(GUEST, 0), Ipv4Addr::BROADCAST);
}

#[test]
fn resolv_conf_names_the_servers_and_the_domain() {
    let mut reply = gateway(kind::ACK, 1, 3600);
    reply.domain = Some("ferrix.test".to_owned());
    reply.dns.push(Ipv4Addr::new(1, 1, 1, 1));
    let lease: Lease = lease_of(&reply, SERVER);
    assert_eq!(
        lease.resolv_conf().unwrap(),
        "# Written by dhcpc from its lease.\nsearch ferrix.test\nnameserver 10.0.2.3\nnameserver 1.1.1.1\n"
    );
    reply.dns.clear();
    assert_eq!(lease_of(&reply, SERVER).resolv_conf(), None);
}

// --- Netlink ------------------------------------------------------------

#[test]
fn an_address_is_added_as_ip_addr_add_asks() {
    let bytes = rtnl::add_address(7, 2, GUEST, 24);
    let message = Messages::new(&bytes).next().unwrap().unwrap();
    assert_eq!(message.header.kind, RTM_NEWADDR);
    assert_eq!(message.header.seq, 7);
    assert_ne!(message.header.flags & NLM_F_REQUEST, 0);
    assert_ne!(message.header.flags & NLM_F_ACK, 0);
    let body = IfAddrMsg::from_bytes(message.payload).unwrap();
    assert_eq!((body.family, body.prefix_len, body.index), (2, 24, 2));
    let attributes = message.attributes(IfAddrMsg::SIZE);
    assert_eq!(
        attributes.find(IFA_LOCAL).unwrap().as_address(),
        Some(Address::V4(GUEST.octets()))
    );
    assert_eq!(
        attributes.find(IFA_BROADCAST).unwrap().as_address(),
        Some(Address::V4([10, 0, 2, 255]))
    );
}

#[test]
fn the_default_route_names_its_gateway_and_interface() {
    let bytes = rtnl::add_default_route(8, 2, SERVER);
    let message = Messages::new(&bytes).next().unwrap().unwrap();
    assert_eq!(message.header.kind, RTM_NEWROUTE);
    let body = RtMsg::from_bytes(message.payload).unwrap();
    assert_eq!((body.family, body.dst_len, body.protocol), (2, 0, 16));
    let attributes = message.attributes(RtMsg::SIZE);
    assert_eq!(
        attributes.find(RTA_GATEWAY).unwrap().as_address(),
        Some(Address::V4(SERVER.octets()))
    );
    assert_eq!(attributes.find(RTA_OIF).unwrap().as_u32(), Some(2));
}

#[test]
fn the_link_is_brought_up_by_index() {
    let bytes = rtnl::set_up(1, 3);
    let message = Messages::new(&bytes).next().unwrap().unwrap();
    assert_eq!(message.header.kind, RTM_NEWLINK);
    let body = IfInfoMsg::from_bytes(message.payload).unwrap();
    assert_eq!((body.index, body.flags & 1, body.change & 1), (3, 1, 1));
}

/// A buffer holding `messages` as the kernel writes them.
fn kernel_says(messages: &[(NlMsgHdr, Vec<u8>, Vec<Attr<'_>>)]) -> Vec<u8> {
    let mut buffer = vec![0_u8; 1024];
    let mut writer = Writer::new(&mut buffer);
    for (header, body, attributes) in messages {
        let _ = writer.message(*header, body, attributes).unwrap();
    }
    writer.written().to_vec()
}

fn header(kind: u16, seq: u32) -> NlMsgHdr {
    NlMsgHdr {
        len: 0,
        kind,
        flags: 0,
        seq,
        pid: 0,
    }
}

#[test]
fn an_acknowledgement_a_refusal_and_a_dump_are_told_apart() {
    let error = |errno: i32| {
        NlMsgErr {
            error: -errno,
            msg: header(RTM_NEWADDR, 4),
        }
        .to_bytes()
        .to_vec()
    };
    let ack = kernel_says(&[(header(NLMSG_ERROR, 4), error(0), vec![])]);
    assert_eq!(rtnl::outcome(&ack, 4, |_, _| {}), rtnl::Outcome::Done);
    let refused = kernel_says(&[(header(NLMSG_ERROR, 4), error(17), vec![])]);
    assert_eq!(
        rtnl::outcome(&refused, 4, |_, _| {}),
        rtnl::Outcome::Refused(17)
    );
    // Another request's answer is not this one's.
    assert_eq!(rtnl::outcome(&refused, 5, |_, _| {}), rtnl::Outcome::More);

    let link = IfInfoMsg {
        family: 0,
        kind: 1,
        index: 2,
        flags: 0,
        change: 0,
    };
    let dump = kernel_says(&[
        (
            header(RTM_NEWLINK, 9),
            link.to_bytes().to_vec(),
            vec![
                Attr::new(IFLA_IFNAME, Value::Name(b"eth0")),
                Attr::new(IFLA_ADDRESS, Value::Bytes(&MAC)),
            ],
        ),
        (header(NLMSG_DONE, 9), vec![0; 4], vec![]),
    ]);
    let mut links = Vec::new();
    let said = rtnl::outcome(&dump, 9, |kind, payload| {
        assert_eq!(kind, RTM_NEWLINK);
        links.push(rtnl::link_of(payload).unwrap());
    });
    assert_eq!(said, rtnl::Outcome::Done);
    assert_eq!(
        links,
        [rtnl::Link {
            index: 2,
            name: "eth0".to_owned(),
            mac: Some(MAC),
        }]
    );
}

#[test]
fn an_address_dump_is_read_back() {
    let body = IfAddrMsg {
        family: 2,
        prefix_len: 24,
        flags: 0,
        scope: 0,
        index: 2,
    };
    let dump = kernel_says(&[(
        header(RTM_NEWADDR, 3),
        body.to_bytes().to_vec(),
        vec![Attr::new(
            IFA_LOCAL,
            Value::Address(Address::V4(GUEST.octets())),
        )],
    )]);
    let mut found = Vec::new();
    assert_eq!(
        rtnl::outcome(&dump, 3, |_, payload| found
            .push(rtnl::address_of(payload).unwrap())),
        rtnl::Outcome::More
    );
    assert_eq!(
        found,
        [rtnl::Assigned {
            index: 2,
            address: GUEST,
            prefix: 24,
        }]
    );
}

// --- The command line ---------------------------------------------------

fn parse(words: &[&str]) -> Result<Args, String> {
    args::parse(words.iter().map(|word| (*word).to_owned()))
}

#[test]
fn the_command_line_every_caller_used_for_udhcpc_is_taken() {
    let args = parse(&["-i", "eth0", "-n", "-q", "-t", "5", "-T", "2"]).unwrap();
    assert_eq!(
        args,
        Args {
            interface: "eth0".to_owned(),
            tries: 5,
            timeout: secs(2),
            pause: secs(20),
            exit_without_lease: true,
            quit_with_lease: true,
        }
    );
    assert_eq!(
        parse(&["-nqieth1", "-t30", "-f"]).unwrap().interface,
        "eth1"
    );
    assert_eq!(parse(&[]).unwrap(), Args::default());
}

#[test]
fn a_command_line_it_does_not_take_is_refused() {
    assert!(parse(&["-x"]).is_err());
    assert!(parse(&["-i"]).is_err());
    assert!(parse(&["-t", "many"]).is_err());
    assert!(parse(&["eth0"]).is_err());
    assert!(parse(&["-i", "../../etc"]).is_err());
}
