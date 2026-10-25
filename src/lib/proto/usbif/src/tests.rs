use super::*;

fn identity() -> Identity {
    Identity {
        vendor: 0x0403,
        product: 0x6014,
        release: 0x0900,
        class: 0xFF,
        subclass: 0xFF,
        protocol: 0xFF,
        interface: 0,
        speed: 2,
        path: 0x0200_0013,
        manufacturer: Text::new(b"FTDI"),
        product_name: Text::new(b"Single RS232-HS"),
        serial: Text::new(b"FT9X\x01Z"),
    }
}

fn endpoints() -> [Endpoint; MAX_ENDPOINTS] {
    let mut all = [Endpoint::default(); MAX_ENDPOINTS];
    all[0] = Endpoint {
        address: 0x81,
        attributes: 2,
        max_packet: 512,
        interval: 0,
    };
    all[1] = Endpoint {
        address: 0x02,
        attributes: 2,
        max_packet: 512,
        interval: 0,
    };
    all
}

#[test]
fn every_message_decodes_as_it_was_encoded() {
    let all = [
        Message::Attach {
            key: 0x1234_5678_9ABC,
            identity: identity(),
        },
        Message::Hello {
            key: 7,
            identity: identity(),
            count: 2,
            endpoints: endpoints(),
        },
        Message::Done {
            tag: 9,
            status: Status::Stall,
            value: 4,
        },
        Message::In {
            endpoint: 0x81,
            slot: 15,
            len: 512,
        },
        Message::Sent {
            tag: 3,
            status: Status::Ok,
        },
        Message::Empty { endpoint: 2 },
        Message::Bind { number: 3 },
        Message::Lost {
            endpoint: 2,
            bytes: 1000,
        },
        Message::Control {
            tag: 1,
            setup: [0x40, 3, 0x38, 0x41, 0, 0, 0, 0],
        },
        Message::Open {
            tag: 2,
            endpoint: 0x81,
        },
        Message::Send {
            tag: 4,
            endpoint: 2,
            slot: 1,
            len: 4096,
        },
        Message::Free { slot: 3 },
    ];
    for message in all {
        assert_eq!(Message::decode(message.encode().as_bytes()), Ok(message));
    }
}

#[test]
fn strings_keep_only_what_a_path_segment_may_hold() {
    assert_eq!(identity().serial.as_bytes(), b"FT9X_Z");
    assert_eq!(Text::new(&[b'a'; 40]).as_bytes().len(), TEXT_BYTES);
}

#[test]
fn out_of_range_fields_and_trailing_bytes_are_refused() {
    let send = Message::Send {
        tag: 4,
        endpoint: 2,
        slot: 1,
        len: 1,
    };
    let mut bytes = send.encode().as_bytes().to_vec();
    bytes[6] = 2; // slot 2 of two
    assert_eq!(Message::decode(&bytes), Err(Malformed));
    let mut long = send.encode().as_bytes().to_vec();
    long.push(0);
    assert_eq!(Message::decode(&long), Err(Malformed));
    let in_message = Message::In {
        endpoint: 0x81,
        slot: 0,
        len: 513,
    };
    assert_eq!(
        Message::decode(in_message.encode().as_bytes()),
        Err(Malformed)
    );
    assert_eq!(
        Message::decode(Message::Free { slot: 16 }.encode().as_bytes()),
        Err(Malformed)
    );
    assert_eq!(Message::decode(&[0x55]), Err(Malformed));
    assert_eq!(Message::decode(&[]), Err(Malformed));
    let mut hello = Message::Hello {
        key: 1,
        identity: identity(),
        count: 1,
        endpoints: endpoints(),
    }
    .encode()
    .as_bytes()
    .to_vec();
    hello[1] = VERSION + 1;
    assert_eq!(Message::decode(&hello), Err(Malformed));
}

#[test]
fn a_class_driver_may_ask_only_what_usbfs_lets_a_claimed_interface_ask() {
    let own = endpoints();
    let ok = |setup: [u8; 8]| allowed(&setup, 0, &own[..2]);
    // FTDI's vendor requests to the device.
    assert!(ok([0x40, 3, 0x38, 0x41, 1, 0, 0, 0]));
    assert!(ok([0xC0, 5, 0, 0, 1, 0, 2, 0]));
    // Standard reads of the device.
    assert!(ok([0x80, 6, 0, 3, 9, 4, 255, 0]));
    assert!(ok([0x80, 0, 0, 0, 0, 0, 2, 0]));
    // What belongs to the host.
    assert!(!ok([0x00, 5, 9, 0, 0, 0, 0, 0]), "SET_ADDRESS");
    assert!(!ok([0x00, 9, 1, 0, 0, 0, 0, 0]), "SET_CONFIGURATION");
    assert!(!ok([0x00, 3, 1, 0, 0, 0, 0, 0]), "SET_FEATURE");
    // Its own interface, not another.
    assert!(ok([0x21, 0x22, 3, 0, 0, 0, 0, 0]));
    assert!(!ok([0x21, 0x22, 3, 0, 1, 0, 0, 0]));
    assert!(!ok([0x01, 11, 1, 0, 0, 0, 0, 0]), "SET_INTERFACE");
    // Its own endpoints, not another's, and no halt cleared behind the
    // host's back.
    assert!(ok([0x82, 0, 0, 0, 0x81, 0, 2, 0]));
    assert!(!ok([0x82, 0, 0, 0, 0x83, 0, 2, 0]));
    assert!(!ok([0x02, 1, 0, 0, 0x81, 0, 0, 0]), "CLEAR_FEATURE");
    // A hub's port, say: nothing.
    assert!(!ok([0x23, 3, 4, 0, 1, 0, 0, 0]));
}
