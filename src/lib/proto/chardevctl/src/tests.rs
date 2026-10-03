use crate::message::{Hello, MAX_NODES, Malformed, Message, Op, Request, VERSION};
use crate::node::{self, CONTROL_MINOR, MODESET_MINOR};
use crate::session::{self, Refusal};

fn hello(minors: &[u16]) -> Hello {
    let mut list = [0; MAX_NODES];
    for (slot, minor) in list.iter_mut().zip(minors) {
        *slot = *minor;
    }
    Hello {
        version: VERSION,
        location: 0x0200,
        count: minors.len(),
        minors: list,
    }
}

#[test]
fn every_minor_has_nvidias_name_and_no_other_name_is_had() {
    assert_eq!(node::name(CONTROL_MINOR).unwrap().as_bytes(), b"nvidiactl");
    assert_eq!(node::name(MODESET_MINOR).unwrap().as_bytes(), b"nvidia-modeset");
    assert_eq!(node::name(0).unwrap().as_bytes(), b"nvidia0");
    assert_eq!(node::name(7).unwrap().as_bytes(), b"nvidia7");
    assert_eq!(node::name(42).unwrap().as_bytes(), b"nvidia42");
    assert_eq!(node::name(253).unwrap().as_bytes(), b"nvidia253");
    assert_eq!(node::name(256), None);
    for minor in 0..=255 {
        let name = node::name(minor).unwrap();
        assert_eq!(node::minor_of(name.as_bytes()), Some(minor));
    }
    for name in [&b"kvm"[..], b"nvidia", b"nvidia007", b"nvidia254", b"nvidiax", b"fuse", b"nvidia-uvm"] {
        assert_eq!(node::minor_of(name), None, "{:?}", core::str::from_utf8(name));
    }
}

#[test]
fn messages_round_trip() {
    let request = Request {
        id: 9,
        file: 3,
        op: Op::Ioctl,
        minor: 0,
        pid: 41,
        euid: 1000,
        egid: 100,
        cmd: 0xc020_462b,
        arg: 0x7fff_0000_1000,
    };
    for message in [
        Message::Hello(hello(&[255, 0])),
        Message::Ready(2),
        Message::Refused(Refusal::Taken),
        Message::Request(request),
    ] {
        assert_eq!(Message::decode(message.encode().as_bytes()), Ok(message));
    }
}

#[test]
fn malformed_bytes_are_refused() {
    let good = Message::Hello(hello(&[255])).encode();
    let bytes = good.as_bytes();
    assert_eq!(Message::decode(&bytes[..bytes.len() - 1]), Err(Malformed));
    let mut longer = [0_u8; 11];
    longer[..10].copy_from_slice(bytes);
    assert_eq!(Message::decode(&longer), Err(Malformed));
    assert_eq!(Message::decode(&[9, 0, 0, 0]), Err(Malformed));
    assert_eq!(Message::decode(&[3, 99, 0, 0]), Err(Malformed));
    assert_eq!(Message::decode(&[2, 1, 1, 0]), Err(Malformed));
    assert_eq!(Message::decode(&[1, 1, 0, 0, 0, 0, 0, 0]), Err(Malformed));
    assert_eq!(Message::decode(&[]), Err(Malformed));
    let encoded = Message::Request(Request {
        id: 1,
        file: 1,
        op: Op::Open,
        minor: 255,
        pid: 1,
        euid: 0,
        egid: 0,
        cmd: 0,
        arg: 0,
    })
    .encode();
    let mut request = [0_u8; crate::message::REQUEST_BYTES];
    request.copy_from_slice(encoded.as_bytes());
    request[1] = 9;
    assert_eq!(Message::decode(&request), Err(Malformed));
}

#[test]
fn the_judge_takes_nvidias_minors_and_refuses_the_rest() {
    let taken = session::judge(&hello(&[255, 0]), 0x0200).unwrap();
    assert_eq!(taken.minors(), &[255, 0]);
    assert_eq!(session::judge(&hello(&[255]), 0x0300), Err(Refusal::Location));
    assert_eq!(session::judge(&hello(&[255, 255]), 0x0200), Err(Refusal::Duplicate));
    assert_eq!(session::judge(&hello(&[256]), 0x0200), Err(Refusal::Minor));
    let mut old = hello(&[255]);
    old.version = 0;
    assert_eq!(session::judge(&old, 0x0200), Err(Refusal::Version));
}
