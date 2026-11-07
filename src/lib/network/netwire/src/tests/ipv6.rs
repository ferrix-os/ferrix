//! IPv6 fixed headers and the extension-header walk.

use crate::Error;
use crate::ipv6::{Fragment, HEADER_LEN, Header, MAX_EXTENSION_HEADERS, next_header, upper_layer};

const LINK_LOCAL: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const ALL_NODES: [u8; 16] = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

fn header() -> Header {
    Header {
        traffic_class: 0xAB,
        flow_label: 0x1_2345,
        next_header: next_header::ICMPV6,
        hop_limit: 255,
        source: LINK_LOCAL,
        destination: ALL_NODES,
    }
}

#[test]
fn a_header_round_trips_and_its_first_word_packs_the_fields() {
    let mut out = [0u8; HEADER_LEN + 4];
    assert_eq!(header().emit(4, &mut out), Ok(HEADER_LEN));
    assert_eq!(&out[..4], &[0x6A, 0xB1, 0x23, 0x45]);
    assert_eq!(&out[4..6], &[0, 4]);
    let parsed = Header::parse(&out).expect("parses");
    assert_eq!(parsed.header, header());
    assert_eq!(parsed.payload.len(), 4);
}

#[test]
fn the_payload_length_bounds_the_payload_and_short_bytes_are_truncated() {
    let mut out = [0u8; HEADER_LEN + 8];
    let _ = header().emit(4, &mut out);
    assert_eq!(
        Header::parse(&out).expect("parses").payload.len(),
        4,
        "padding ignored"
    );
    for cut in 0..HEADER_LEN + 4 {
        assert_eq!(
            Header::parse(&out[..cut]),
            Err(Error::Truncated),
            "cut at {cut}"
        );
    }
}

#[test]
fn a_wrong_version_and_out_of_range_fields_are_refused() {
    let mut out = [0u8; HEADER_LEN];
    let _ = header().emit(0, &mut out);
    out[0] = 0x4A;
    assert!(matches!(Header::parse(&out), Err(Error::Malformed(_))));
    let wide = Header {
        flow_label: 0x10_0000,
        ..header()
    };
    assert!(matches!(
        wide.emit(0, &mut [0u8; HEADER_LEN]),
        Err(Error::Malformed(_))
    ));
    assert!(matches!(
        header().emit(65_536, &mut [0u8; HEADER_LEN]),
        Err(Error::Malformed(_))
    ));
    assert_eq!(
        header().emit(0, &mut [0u8; HEADER_LEN - 1]),
        Err(Error::NoSpace)
    );
}

#[test]
fn a_transport_header_straight_after_is_the_upper_layer() {
    let payload = [0x12u8; 8];
    let upper = upper_layer(next_header::UDP, &payload).expect("walks");
    assert_eq!(
        (upper.protocol, upper.offset, upper.fragment),
        (next_header::UDP, 0, None)
    );
    assert_eq!(upper.bytes.len(), 8);
}

#[test]
fn hop_by_hop_then_fragment_then_udp_is_walked_with_the_fragment_kept() {
    let mut payload = [0u8; 24];
    // Hop-by-hop: next = fragment, length 0 (eight bytes), PadN filling the rest.
    payload[..8].copy_from_slice(&[next_header::FRAGMENT, 0, 1, 4, 0, 0, 0, 0]);
    // Fragment: next = UDP, offset 185 (1480 bytes), more fragments, id 0xdeadbeef.
    payload[8..16].copy_from_slice(&[next_header::UDP, 0, 0x05, 0xc9, 0xde, 0xad, 0xbe, 0xef]);
    let upper = upper_layer(next_header::HOP_BY_HOP, &payload).expect("walks");
    assert_eq!((upper.protocol, upper.offset), (next_header::UDP, 16));
    assert_eq!(
        upper.fragment,
        Some(Fragment {
            offset: 185,
            more: true,
            identification: 0xdead_beef
        })
    );
    assert_eq!(upper.bytes.len(), 8);
}

#[test]
fn a_late_hop_by_hop_a_second_fragment_and_a_short_header_are_refused() {
    let mut late = [0u8; 16];
    late[..8].copy_from_slice(&[next_header::HOP_BY_HOP, 0, 0, 0, 0, 0, 0, 0]);
    assert!(matches!(
        upper_layer(next_header::DESTINATION_OPTIONS, &late),
        Err(Error::Malformed(_))
    ));

    let mut twice = [0u8; 16];
    twice[..8].copy_from_slice(&[next_header::FRAGMENT, 0, 0, 0, 0, 0, 0, 1]);
    assert!(matches!(
        upper_layer(next_header::FRAGMENT, &twice),
        Err(Error::Malformed(_))
    ));

    let short = [next_header::UDP, 1, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        upper_layer(next_header::ROUTING, &short),
        Err(Error::Truncated)
    );
}

#[test]
fn a_chain_past_the_bound_is_refused_and_no_next_header_ends_it() {
    let mut long = [0u8; 8 * (MAX_EXTENSION_HEADERS + 2)];
    for chunk in long.as_chunks_mut::<8>().0 {
        chunk[0] = next_header::DESTINATION_OPTIONS;
    }
    assert!(matches!(
        upper_layer(next_header::DESTINATION_OPTIONS, &long),
        Err(Error::Malformed(_))
    ));

    let mut ends = [0u8; 8];
    ends[0] = next_header::NO_NEXT_HEADER;
    let upper = upper_layer(next_header::DESTINATION_OPTIONS, &ends).expect("walks");
    assert_eq!(
        (upper.protocol, upper.offset),
        (next_header::NO_NEXT_HEADER, 8)
    );
    assert!(upper.bytes.is_empty());
}
