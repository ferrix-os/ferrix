//! `inet`: the numbers against a probe compiled from the UAPI headers, and the
//! two address layouts byte by byte.

use crate::inet::{
    self, InetAddress, SOCKADDR_IN_SIZE, SOCKADDR_IN6_RFC2133_SIZE, SOCKADDR_IN6_SIZE,
    SOCKADDR_STORAGE_SIZE,
};
use crate::inet::{InetTunables, TunableError, TunableValue};
use crate::socket::{self, AddressError, Linger};

#[test]
fn protocols_levels_and_ipv4_options_match_the_headers() {
    for (value, expected, name) in [
        (inet::IPPROTO_IP, 0, "IPPROTO_IP"),
        (inet::IPPROTO_HOPOPTS, 0, "IPPROTO_HOPOPTS"),
        (inet::IPPROTO_ICMP, 1, "IPPROTO_ICMP"),
        (inet::IPPROTO_TCP, 6, "IPPROTO_TCP"),
        (inet::IPPROTO_UDP, 17, "IPPROTO_UDP"),
        (inet::IPPROTO_IPV6, 41, "IPPROTO_IPV6"),
        (inet::IPPROTO_ROUTING, 43, "IPPROTO_ROUTING"),
        (inet::IPPROTO_FRAGMENT, 44, "IPPROTO_FRAGMENT"),
        (inet::IPPROTO_ICMPV6, 58, "IPPROTO_ICMPV6"),
        (inet::IPPROTO_NONE, 59, "IPPROTO_NONE"),
        (inet::IPPROTO_DSTOPTS, 60, "IPPROTO_DSTOPTS"),
        (inet::IPPROTO_RAW, 255, "IPPROTO_RAW"),
        (inet::SOL_IP, 0, "SOL_IP"),
        (inet::SOL_TCP, 6, "SOL_TCP"),
        (inet::SOL_UDP, 17, "SOL_UDP"),
        (inet::SOL_IPV6, 41, "SOL_IPV6"),
        (inet::SOL_ICMPV6, 58, "SOL_ICMPV6"),
        (inet::SOL_RAW, 255, "SOL_RAW"),
        (inet::IP_TOS, 1, "IP_TOS"),
        (inet::IP_TTL, 2, "IP_TTL"),
        (inet::IP_HDRINCL, 3, "IP_HDRINCL"),
        (inet::IP_OPTIONS, 4, "IP_OPTIONS"),
        (inet::IP_PKTINFO, 8, "IP_PKTINFO"),
        (inet::IP_MTU_DISCOVER, 10, "IP_MTU_DISCOVER"),
        (inet::IP_RECVERR, 11, "IP_RECVERR"),
        (inet::IP_RECVTTL, 12, "IP_RECVTTL"),
        (inet::IP_RECVTOS, 13, "IP_RECVTOS"),
        (inet::IP_MTU, 14, "IP_MTU"),
        (inet::IP_FREEBIND, 15, "IP_FREEBIND"),
        (inet::IP_MULTICAST_IF, 32, "IP_MULTICAST_IF"),
        (inet::IP_MULTICAST_TTL, 33, "IP_MULTICAST_TTL"),
        (inet::IP_MULTICAST_LOOP, 34, "IP_MULTICAST_LOOP"),
        (inet::IP_ADD_MEMBERSHIP, 35, "IP_ADD_MEMBERSHIP"),
        (inet::IP_DROP_MEMBERSHIP, 36, "IP_DROP_MEMBERSHIP"),
        (inet::IP_PMTUDISC_DONT, 0, "IP_PMTUDISC_DONT"),
        (inet::IP_PMTUDISC_WANT, 1, "IP_PMTUDISC_WANT"),
        (inet::IP_PMTUDISC_DO, 2, "IP_PMTUDISC_DO"),
        (inet::IP_PMTUDISC_PROBE, 3, "IP_PMTUDISC_PROBE"),
        (inet::IP_PMTUDISC_INTERFACE, 4, "IP_PMTUDISC_INTERFACE"),
        (inet::IP_PMTUDISC_OMIT, 5, "IP_PMTUDISC_OMIT"),
    ] {
        assert_eq!(value, expected, "{name}");
    }
    for (value, expected, name) in [
        (inet::INADDR_ANY, 0, "INADDR_ANY"),
        (inet::INADDR_BROADCAST, 4_294_967_295, "INADDR_BROADCAST"),
        (inet::INADDR_NONE, 4_294_967_295, "INADDR_NONE"),
        (inet::INADDR_LOOPBACK, 2_130_706_433, "INADDR_LOOPBACK"),
    ] {
        assert_eq!(value, expected, "{name}");
    }
}

#[test]
fn ipv6_and_tcp_options_match_the_headers() {
    for (value, expected, name) in [
        (inet::IPV6_ADDRFORM, 1, "IPV6_ADDRFORM"),
        (inet::IPV6_CHECKSUM, 7, "IPV6_CHECKSUM"),
        (inet::IPV6_2292HOPLIMIT, 8, "IPV6_2292HOPLIMIT"),
        (inet::ICMPV6_FILTER, 1, "ICMPV6_FILTER"),
        (inet::IPV6_UNICAST_HOPS, 16, "IPV6_UNICAST_HOPS"),
        (inet::IPV6_MULTICAST_IF, 17, "IPV6_MULTICAST_IF"),
        (inet::IPV6_MULTICAST_HOPS, 18, "IPV6_MULTICAST_HOPS"),
        (inet::IPV6_MULTICAST_LOOP, 19, "IPV6_MULTICAST_LOOP"),
        (inet::IPV6_ADD_MEMBERSHIP, 20, "IPV6_ADD_MEMBERSHIP"),
        (inet::IPV6_DROP_MEMBERSHIP, 21, "IPV6_DROP_MEMBERSHIP"),
        (inet::IPV6_MTU_DISCOVER, 23, "IPV6_MTU_DISCOVER"),
        (inet::IPV6_MTU, 24, "IPV6_MTU"),
        (inet::IPV6_RECVERR, 25, "IPV6_RECVERR"),
        (inet::IPV6_V6ONLY, 26, "IPV6_V6ONLY"),
        (inet::IPV6_RECVPKTINFO, 49, "IPV6_RECVPKTINFO"),
        (inet::IPV6_PKTINFO, 50, "IPV6_PKTINFO"),
        (inet::IPV6_RECVHOPLIMIT, 51, "IPV6_RECVHOPLIMIT"),
        (inet::IPV6_HOPLIMIT, 52, "IPV6_HOPLIMIT"),
        (inet::IPV6_DONTFRAG, 62, "IPV6_DONTFRAG"),
        (inet::IPV6_RECVTCLASS, 66, "IPV6_RECVTCLASS"),
        (inet::IPV6_TCLASS, 67, "IPV6_TCLASS"),
        (inet::TCP_NODELAY, 1, "TCP_NODELAY"),
        (inet::TCP_MAXSEG, 2, "TCP_MAXSEG"),
        (inet::TCP_CORK, 3, "TCP_CORK"),
        (inet::TCP_KEEPIDLE, 4, "TCP_KEEPIDLE"),
        (inet::TCP_KEEPINTVL, 5, "TCP_KEEPINTVL"),
        (inet::TCP_KEEPCNT, 6, "TCP_KEEPCNT"),
        (inet::TCP_SYNCNT, 7, "TCP_SYNCNT"),
        (inet::TCP_LINGER2, 8, "TCP_LINGER2"),
        (inet::TCP_DEFER_ACCEPT, 9, "TCP_DEFER_ACCEPT"),
        (inet::TCP_WINDOW_CLAMP, 10, "TCP_WINDOW_CLAMP"),
        (inet::TCP_INFO, 11, "TCP_INFO"),
        (inet::TCP_QUICKACK, 12, "TCP_QUICKACK"),
        (inet::TCP_CONGESTION, 13, "TCP_CONGESTION"),
        (inet::TCP_USER_TIMEOUT, 18, "TCP_USER_TIMEOUT"),
        (inet::TCP_FASTOPEN, 23, "TCP_FASTOPEN"),
    ] {
        assert_eq!(value, expected, "{name}");
    }
    assert_eq!(SOCKADDR_IN_SIZE, 16, "sizeof(struct sockaddr_in)");
    assert_eq!(SOCKADDR_IN6_SIZE, 28, "sizeof(struct sockaddr_in6)");
    assert_eq!(SOCKADDR_IN6_RFC2133_SIZE, 24, "SIN6_LEN_RFC2133");
    assert_eq!(
        SOCKADDR_STORAGE_SIZE, 128,
        "sizeof(struct sockaddr_storage)"
    );
}

/// A `struct sockaddr_in` for 127.0.0.1 port 8080, in a buffer as large as a
/// `struct sockaddr_storage`, with the family given.
fn sockaddr_in(family: u16) -> [u8; SOCKADDR_STORAGE_SIZE] {
    let mut bytes = [0_u8; SOCKADDR_STORAGE_SIZE];
    bytes[..2].copy_from_slice(&family.to_le_bytes());
    bytes[2..4].copy_from_slice(&8080_u16.to_be_bytes());
    bytes[4..8].copy_from_slice(&[127, 0, 0, 1]);
    bytes
}

/// A `struct sockaddr_in6` for `::1` port 443, flow information 0x12345 and
/// scope 3, laid out at the offsets `offsetof` gave: port 2, flow 4, address
/// 8, scope 24.
fn sockaddr_in6() -> [u8; SOCKADDR_IN6_SIZE] {
    let mut bytes = [0_u8; SOCKADDR_IN6_SIZE];
    bytes[..2].copy_from_slice(&socket::AF_INET6.to_le_bytes());
    bytes[2..4].copy_from_slice(&443_u16.to_be_bytes());
    bytes[4..8].copy_from_slice(&0x12345_u32.to_be_bytes());
    bytes[23] = 1;
    bytes[24..28].copy_from_slice(&3_u32.to_le_bytes());
    bytes
}

const LOOPBACK6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

#[test]
fn a_sockaddr_in_is_read_with_its_port_and_address_in_network_order() {
    let v4 = InetAddress::V4 {
        port: 8080,
        address: [127, 0, 0, 1],
    };
    let bytes = sockaddr_in(socket::AF_INET);
    assert_eq!(InetAddress::parse(&bytes, SOCKADDR_IN_SIZE), Ok(v4));
    assert_eq!(
        InetAddress::parse(&bytes, SOCKADDR_STORAGE_SIZE),
        Ok(v4),
        "a longer length is allowed, up to sockaddr_storage"
    );
    assert_eq!(
        InetAddress::parse(&bytes, SOCKADDR_IN_SIZE - 1),
        Err(AddressError::TooShort)
    );
    assert_eq!(
        InetAddress::parse(&bytes, SOCKADDR_STORAGE_SIZE + 1),
        Err(AddressError::TooLong)
    );
    assert_eq!(
        InetAddress::parse(&bytes[..8], SOCKADDR_IN_SIZE),
        Err(AddressError::TooShort),
        "fewer bytes than the length claims"
    );
    assert_eq!(InetAddress::parse(&bytes, 0), Err(AddressError::TooShort));
    for family in [socket::AF_UNIX, socket::AF_UNSPEC, 0x0200] {
        assert_eq!(
            InetAddress::parse(&sockaddr_in(family), SOCKADDR_IN_SIZE),
            Err(AddressError::WrongFamily),
            "family {family:#x}"
        );
    }
    assert_eq!(v4.family(), socket::AF_INET);
    assert_eq!(v4.port(), 8080);
}

#[test]
fn a_sockaddr_in6_has_a_scope_only_when_the_length_reaches_it() {
    let bytes = sockaddr_in6();
    let full = InetAddress::V6 {
        port: 443,
        flow_info: 0x12345,
        address: LOOPBACK6,
        scope_id: 3,
    };
    assert_eq!(InetAddress::parse(&bytes, SOCKADDR_IN6_SIZE), Ok(full));
    assert_eq!(
        InetAddress::parse(&bytes, SOCKADDR_IN6_RFC2133_SIZE),
        Ok(InetAddress::V6 {
            port: 443,
            flow_info: 0x12345,
            address: LOOPBACK6,
            scope_id: 0,
        }),
        "RFC 2133's length has no scope"
    );
    assert_eq!(
        InetAddress::parse(&bytes, SOCKADDR_IN6_RFC2133_SIZE - 1),
        Err(AddressError::TooShort)
    );
    assert_eq!(
        InetAddress::parse(&bytes, SOCKADDR_IN_SIZE),
        Err(AddressError::TooShort),
        "an IPv4 length is not enough for IPv6"
    );
    assert_eq!(full.family(), socket::AF_INET6);
    assert_eq!(full.port(), 443);
}

#[test]
fn an_inet_address_encodes_to_the_bytes_it_was_read_from() {
    let mut out = [0xaa_u8; SOCKADDR_IN6_SIZE + 1];
    let v4 = InetAddress::parse(&sockaddr_in(socket::AF_INET), SOCKADDR_IN_SIZE);
    assert_eq!(v4.map(|a| a.encode(&mut out)), Ok(Some(SOCKADDR_IN_SIZE)));
    assert_eq!(
        &out[..SOCKADDR_IN_SIZE],
        &sockaddr_in(socket::AF_INET)[..SOCKADDR_IN_SIZE],
        "sin_zero is written zero"
    );
    assert_eq!(out[SOCKADDR_IN_SIZE], 0xaa, "nothing past the structure");

    let v6 = InetAddress::parse(&sockaddr_in6(), SOCKADDR_IN6_SIZE);
    assert_eq!(v6.map(|a| a.encode(&mut out)), Ok(Some(SOCKADDR_IN6_SIZE)));
    assert_eq!(&out[..SOCKADDR_IN6_SIZE], &sockaddr_in6());

    let mut short = [0xaa_u8; SOCKADDR_IN6_SIZE - 1];
    assert_eq!(v6.map(|a| a.encode(&mut short)), Ok(None));
    assert_eq!(short, [0xaa; SOCKADDR_IN6_SIZE - 1], "nothing written");
}

/// `n` as the bytes of the `int` a program passes.
fn int(n: i32) -> [u8; 4] {
    n.to_le_bytes()
}

#[test]
fn tunables_read_as_linux_defaults_them() {
    let tunables = InetTunables::default();
    let read = |level, name, v6| tunables.read(level, name, v6);
    assert_eq!(
        read(socket::SOL_SOCKET, socket::SO_LINGER, false),
        Some(TunableValue::Linger(Linger {
            onoff: 0,
            linger: 0
        }))
    );
    assert_eq!(
        read(socket::SOL_SOCKET, socket::SO_OOBINLINE, true),
        Some(TunableValue::Int(0))
    );
    assert_eq!(
        read(inet::SOL_TCP, inet::TCP_KEEPIDLE, false),
        Some(TunableValue::Int(7_200))
    );
    assert_eq!(
        read(inet::SOL_TCP, inet::TCP_KEEPINTVL, false),
        Some(TunableValue::Int(75))
    );
    assert_eq!(
        read(inet::SOL_TCP, inet::TCP_KEEPCNT, false),
        Some(TunableValue::Int(9))
    );
    assert_eq!(
        read(inet::SOL_TCP, inet::TCP_SYNCNT, false),
        Some(TunableValue::Int(6))
    );
    assert_eq!(
        read(inet::SOL_IP, inet::IP_MTU_DISCOVER, true),
        Some(TunableValue::Int(inet::IP_PMTUDISC_WANT))
    );
    assert_eq!(
        read(inet::SOL_IPV6, inet::IPV6_MTU_DISCOVER, true),
        Some(TunableValue::Int(1))
    );
    assert_eq!(
        read(inet::SOL_IPV6, inet::IPV6_DONTFRAG, true),
        Some(TunableValue::Int(0))
    );
    // An IPv4 socket has no IPv6 options, and nothing else is a tunable.
    assert_eq!(read(inet::SOL_IPV6, inet::IPV6_MTU_DISCOVER, false), None);
    assert_eq!(read(inet::SOL_IPV6, inet::IPV6_DONTFRAG, false), None);
    assert_eq!(read(inet::SOL_TCP, inet::TCP_NODELAY, false), None);
    assert_eq!(read(socket::SOL_SOCKET, socket::SO_KEEPALIVE, false), None);
}

#[test]
fn tunables_give_back_what_was_set() {
    let mut tunables = InetTunables::default();
    for (level, name, value) in [
        (socket::SOL_SOCKET, socket::SO_OOBINLINE, 5),
        (inet::SOL_TCP, inet::TCP_KEEPIDLE, 30),
        (inet::SOL_TCP, inet::TCP_KEEPINTVL, inet::MAX_TCP_KEEPINTVL),
        (inet::SOL_TCP, inet::TCP_KEEPCNT, 1),
        (inet::SOL_TCP, inet::TCP_SYNCNT, 4),
        (inet::SOL_IP, inet::IP_MTU_DISCOVER, inet::IP_PMTUDISC_DO),
        (
            inet::SOL_IPV6,
            inet::IPV6_MTU_DISCOVER,
            inet::IP_PMTUDISC_OMIT,
        ),
        (inet::SOL_IPV6, inet::IPV6_DONTFRAG, 1),
    ] {
        assert_eq!(
            tunables.write(level, name, &int(value), true),
            Some(Ok(())),
            "{name}"
        );
        // Booleans read back as 0 or 1, as `valbool` makes them.
        let expected = if (level, name) == (socket::SOL_SOCKET, socket::SO_OOBINLINE) {
            1
        } else {
            value
        };
        assert_eq!(
            tunables.read(level, name, true),
            Some(TunableValue::Int(expected)),
            "{name}"
        );
    }
}

#[test]
fn tunables_refuse_what_linux_refuses() {
    let mut tunables = InetTunables::default();
    let before = tunables;
    for (level, name, value) in [
        (inet::SOL_TCP, inet::TCP_KEEPIDLE, 0),
        (
            inet::SOL_TCP,
            inet::TCP_KEEPIDLE,
            inet::MAX_TCP_KEEPIDLE + 1,
        ),
        (inet::SOL_TCP, inet::TCP_KEEPINTVL, -1),
        (inet::SOL_TCP, inet::TCP_KEEPCNT, inet::MAX_TCP_KEEPCNT + 1),
        (inet::SOL_TCP, inet::TCP_SYNCNT, 0),
        (
            inet::SOL_IP,
            inet::IP_MTU_DISCOVER,
            inet::IP_PMTUDISC_OMIT + 1,
        ),
        (inet::SOL_IPV6, inet::IPV6_MTU_DISCOVER, -1),
    ] {
        assert_eq!(
            tunables.write(level, name, &int(value), true),
            Some(Err(TunableError::Invalid)),
            "{name} {value}"
        );
    }
    // Shorter than an int.
    assert_eq!(
        tunables.write(inet::SOL_TCP, inet::TCP_KEEPCNT, &[1, 0], true),
        Some(Err(TunableError::Invalid))
    );
    // The IPv6 options on an IPv4 socket.
    assert_eq!(
        tunables.write(inet::SOL_IPV6, inet::IPV6_DONTFRAG, &int(1), false),
        Some(Err(TunableError::NoOption))
    );
    assert_eq!(tunables, before, "a refused value changed nothing");
    assert_eq!(
        tunables.write(inet::SOL_TCP, inet::TCP_NODELAY, &int(1), false),
        None
    );
}

#[test]
fn linger_is_kept_as_sk_setsockopt_keeps_it() {
    let mut tunables = InetTunables::default();
    let linger = |onoff, seconds| {
        Linger {
            onoff,
            linger: seconds,
        }
        .to_bytes()
    };
    let read =
        |tunables: &InetTunables| tunables.read(socket::SOL_SOCKET, socket::SO_LINGER, false);
    assert_eq!(
        tunables.write(socket::SOL_SOCKET, socket::SO_LINGER, &linger(7, 30), false),
        Some(Ok(()))
    );
    assert_eq!(
        read(&tunables),
        Some(TunableValue::Linger(Linger {
            onoff: 1,
            linger: 30
        }))
    );
    // Off keeps the time.
    assert_eq!(
        tunables.write(socket::SOL_SOCKET, socket::SO_LINGER, &linger(0, 99), false),
        Some(Ok(()))
    );
    assert_eq!(
        read(&tunables),
        Some(TunableValue::Linger(Linger {
            onoff: 0,
            linger: 30
        }))
    );
    // A time too long to hold is the longest.
    assert_eq!(
        tunables.write(socket::SOL_SOCKET, socket::SO_LINGER, &linger(1, -1), false),
        Some(Ok(()))
    );
    assert_eq!(
        read(&tunables),
        Some(TunableValue::Linger(Linger {
            onoff: 1,
            linger: i32::MAX
        }))
    );
    // Shorter than the structure.
    assert_eq!(
        tunables.write(socket::SOL_SOCKET, socket::SO_LINGER, &int(1), false),
        Some(Err(TunableError::Invalid))
    );
}
