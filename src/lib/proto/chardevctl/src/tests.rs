use crate::message::{
    Hello, MAX_NODES, Malformed, Message, Op, Request, VERSION, VERSION_INTERRUPTS,
};
use crate::node::{self, CONTROL_MINOR, MODESET_MINOR, RENDER_MINOR};
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
    assert_eq!(
        node::name(MODESET_MINOR).unwrap().as_bytes(),
        b"nvidia-modeset"
    );
    assert_eq!(node::name(0).unwrap().as_bytes(), b"nvidia0");
    assert_eq!(node::name(7).unwrap().as_bytes(), b"nvidia7");
    assert_eq!(node::name(42).unwrap().as_bytes(), b"nvidia42");
    assert_eq!(node::name(253).unwrap().as_bytes(), b"nvidia253");
    assert_eq!(node::name(256), None);
    for minor in 0..=255 {
        let name = node::name(minor).unwrap();
        assert_eq!(node::minor_of(name.as_bytes()), Some(minor));
        assert!(node::may_serve(minor));
    }
    // The render node has no name of major 195: the kernel names it.
    assert_eq!(node::name(RENDER_MINOR), None);
    assert!(node::may_serve(RENDER_MINOR));
    assert!(!node::may_serve(256));
    assert!(!node::may_serve(RENDER_MINOR + 1));
    assert_eq!(node::minor_of(b"renderD128"), None);
    for name in [
        &b"kvm"[..],
        b"nvidia",
        b"nvidia007",
        b"nvidia254",
        b"nvidiax",
        b"fuse",
        b"nvidia-uvm",
    ] {
        assert_eq!(
            node::minor_of(name),
            None,
            "{:?}",
            core::str::from_utf8(name)
        );
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
        pages: 0,
    };
    let release = Request {
        id: 0,
        file: 0,
        op: Op::DmabufRelease,
        minor: 0,
        pid: 0,
        euid: 0,
        egid: 0,
        cmd: 0,
        arg: 0xdead_beef_0000_0001,
        pages: 0,
    };
    for message in [
        Message::Hello(hello(&[255, 0, RENDER_MINOR])),
        Message::Ready(3),
        Message::Refused(Refusal::Taken),
        Message::Refused(Refusal::Untranslated),
        Message::Request(request),
        Message::Request(release),
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
        pages: 0,
    })
    .encode();
    let mut request = [0_u8; crate::message::REQUEST_BYTES];
    request.copy_from_slice(encoded.as_bytes());
    request[1] = 9;
    assert_eq!(Message::decode(&request), Err(Malformed));
    // DMABUF_RELEASE is op 5, and carries no pages.
    request[1] = 5;
    assert!(matches!(
        Message::decode(&request),
        Ok(Message::Request(Request {
            op: Op::DmabufRelease,
            ..
        }))
    ));
    request[36] = 1;
    assert_eq!(Message::decode(&request), Err(Malformed));
}

#[test]
fn the_judge_takes_nvidias_minors_and_refuses_the_rest() {
    let taken = session::judge(&hello(&[255, 0]), 0x0200).unwrap();
    assert_eq!(taken.minors(), &[255, 0]);
    assert_eq!(
        session::judge(&hello(&[255]), 0x0300),
        Err(Refusal::Location)
    );
    assert_eq!(
        session::judge(&hello(&[255, 255]), 0x0200),
        Err(Refusal::Duplicate)
    );
    assert_eq!(session::judge(&hello(&[256]), 0x0200), Err(Refusal::Minor));
    let rendering = session::judge(&hello(&[255, 0, RENDER_MINOR]), 0x0200).unwrap();
    assert_eq!(rendering.minors(), &[255, 0, RENDER_MINOR]);
    assert_eq!(
        session::judge(&hello(&[RENDER_MINOR, RENDER_MINOR]), 0x0200),
        Err(Refusal::Duplicate)
    );
    assert_eq!(
        session::judge(&hello(&[RENDER_MINOR + 1]), 0x0200),
        Err(Refusal::Minor)
    );
    let mut old = hello(&[255]);
    old.version = 0;
    assert_eq!(session::judge(&old, 0x0200), Err(Refusal::Version));
    assert!(!session::judge(&hello(&[255]), 0x0200).unwrap().interrupts);
    let mut interrupts = hello(&[RENDER_MINOR]);
    interrupts.version = VERSION_INTERRUPTS;
    assert!(session::judge(&interrupts, 0x0200).unwrap().interrupts);
    interrupts.version = VERSION_INTERRUPTS + 1;
    assert_eq!(session::judge(&interrupts, 0x0200), Err(Refusal::Version));
}

/// An interrupt names its request and goes there and back.
#[test]
fn an_interrupt_round_trips() {
    let message = Message::Request(Request {
        id: 41,
        file: 0,
        op: Op::Interrupt,
        minor: 0,
        pid: 0,
        euid: 0,
        egid: 0,
        cmd: 0,
        arg: 0,
        pages: 0,
    });
    assert_eq!(Message::decode(message.encode().as_bytes()), Ok(message));
}

/// Every refusal's byte names it again, and no two share one.
#[test]
fn refusal_codes_round_trip() {
    let all = [
        Refusal::Protocol,
        Refusal::Version,
        Refusal::Location,
        Refusal::Minor,
        Refusal::Duplicate,
        Refusal::Taken,
        Refusal::NoMemory,
        Refusal::Untranslated,
    ];
    for (index, refusal) in all.iter().enumerate() {
        assert_eq!(Refusal::from_code(refusal.code()), Some(*refusal));
        assert!(
            all.iter()
                .skip(index + 1)
                .all(|other| other.code() != refusal.code())
        );
    }
    assert_eq!(Refusal::from_code(9), None);
}

/// A HELLO listing the render node alone is judged as any other: what a
/// render-only driver such as the Intel GPU's says (`docs/INTEL-GPU.md`
/// §4.2).
#[test]
fn a_render_only_hello_is_taken() {
    let judged = session::judge(&hello(&[RENDER_MINOR]), 0x0200);
    assert_eq!(
        judged.map(|p| (p.count, p.minors[0])),
        Ok((1, RENDER_MINOR))
    );
}

/// An import's REQUEST carries its fence's kind in `pages` and goes there
/// and back; another operation's nonzero `pages` but an mmap's is refused.
#[test]
fn an_import_request_round_trips() {
    use crate::message::{IMPORT_OWN, IMPORT_SIGNALLED};
    for (kind, file) in [
        (IMPORT_OWN, 0x5107_0001),
        (IMPORT_SIGNALLED, (-22_i64) as u64),
    ] {
        let request = Request {
            id: 9,
            file,
            op: Op::DmabufImport,
            minor: 0,
            pid: 7,
            euid: 0,
            egid: 0,
            cmd: 2,
            arg: 0x00D3_AB5F,
            pages: kind,
        };
        let message = Message::Request(request);
        assert_eq!(Message::decode(message.encode().as_bytes()), Ok(message));
    }
    let ioctl = Message::Request(Request {
        id: 9,
        file: 1,
        op: Op::Ioctl,
        minor: 0,
        pid: 7,
        euid: 0,
        egid: 0,
        cmd: 2,
        arg: 0,
        pages: 1,
    });
    assert_eq!(Message::decode(ioctl.encode().as_bytes()), Err(Malformed));
}
