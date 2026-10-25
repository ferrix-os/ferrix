//! The messages, byte for byte.

use super::{
    ANSWER_BUSY, ANSWER_DONE, BIND, BOUND, BUS_PLATFORM, DEVICES, DEVICES_HEADER_BYTES,
    DEVICES_MAX_BYTES, DIED, DONE, DRIVER, Devices, DevicesView, FLAG_FIRST,
    FLAG_UNISOLATED_ALLOWED, MAX_DRIVERS, Malformed, Message, NAME_BYTES, PUBLISHED, REPORT,
    RESTARTED, SHORT_BYTES, UNBIND, UNBOUND,
};

fn name(text: &str) -> [u8; NAME_BYTES] {
    let mut name = [0; NAME_BYTES];
    name[..text.len()].copy_from_slice(text.as_bytes());
    name
}

#[test]
fn devices_round_trips_and_counts_its_handles() {
    let names = [name("blk"), name("gpu")];
    let message = Devices {
        devices: 6,
        more: 30,
        first: true,
        names: &names,
        unisolated_allowed: false,
    };
    let mut bytes = [0_u8; DEVICES_MAX_BYTES];
    let len = message.encode(&mut bytes).expect("fits");
    assert_eq!(len, DEVICES_HEADER_BYTES + 2 * NAME_BYTES, "length");
    assert_eq!(
        message.handles(),
        1 + 12 + 2,
        "job, two per device, one per driver"
    );
    assert_eq!(bytes[0..4], DEVICES.to_le_bytes(), "type");
    assert_eq!(bytes[4..8], (len as u32).to_le_bytes(), "length");
    assert_eq!(bytes[8..12], 6_u32.to_le_bytes(), "devices");
    assert_eq!(bytes[12..16], 2_u32.to_le_bytes(), "drivers");
    assert_eq!(bytes[16..20], 30_u32.to_le_bytes(), "more");
    assert_eq!(bytes[20..24], FLAG_FIRST.to_le_bytes(), "flags");
    assert_eq!(&bytes[24..27], b"blk", "first name");
    assert_eq!(bytes[27], 0, "NUL-padded");
    assert_eq!(&bytes[56..59], b"gpu", "second name");

    let view = DevicesView::decode(&bytes[..len]).expect("decodes");
    assert_eq!((view.devices, view.drivers, view.more), (6, 2, 30));
    assert!(view.first, "first");
    assert_eq!(view.handles(), message.handles());
    assert_eq!(view.name(0), Some(name("blk")));
    assert_eq!(view.name_bytes(1), Some(&b"gpu"[..]));
    assert_eq!(view.name(2), None, "no third driver");
}

#[test]
fn a_continuation_carries_devices_only() {
    let message = Devices {
        devices: 30,
        more: 0,
        first: false,
        names: &[],
        unisolated_allowed: false,
    };
    let mut bytes = [0_u8; DEVICES_MAX_BYTES];
    let len = message.encode(&mut bytes).expect("fits");
    assert_eq!(len, DEVICES_HEADER_BYTES);
    assert_eq!(message.handles(), 60, "two per device and nothing else");
    let view = DevicesView::decode(&bytes[..len]).expect("decodes");
    assert!(!view.first);
    assert_eq!(view.handles(), 60);
    let named = [name("blk")];
    assert_eq!(
        Devices {
            devices: 1,
            more: 0,
            first: false,
            names: &named,
            unisolated_allowed: false,
        }
        .encode(&mut bytes),
        None,
        "names ride only in the first message"
    );
}

/// A directory of [`MAX_DRIVERS`] drivers goes in one first message, and
/// its job and images leave room in a channel message's handles for a device
/// beside them: the kernel's loop sends the rest in later messages, but one
/// with no room for any would carry the images and never a device.
#[test]
fn devices_carries_max_drivers_with_room_for_a_device() {
    let names = [name("x"); MAX_DRIVERS];
    let message = Devices {
        devices: 1,
        more: 0,
        first: true,
        names: &names,
        unisolated_allowed: false,
    };
    let mut bytes = [0_u8; DEVICES_MAX_BYTES];
    let len = message.encode(&mut bytes).expect("MAX_DRIVERS names fit");
    let view = DevicesView::decode(&bytes[..len]).expect("and decode");
    assert_eq!(view.drivers as usize, MAX_DRIVERS);
    assert!(
        message.handles() <= ferrix_native_abi::types::CHANNEL_MAX_HANDLES,
        "the job, one device and every image in one channel message"
    );
}

#[test]
fn devices_refuses_too_many_drivers_and_a_small_buffer() {
    let names = [name("x"); MAX_DRIVERS + 1];
    let mut bytes = [0_u8; DEVICES_MAX_BYTES + NAME_BYTES];
    assert_eq!(
        Devices {
            devices: 0,
            more: 0,
            first: true,
            names: &names,
            unisolated_allowed: false,
        }
        .encode(&mut bytes),
        None,
        "over MAX_DRIVERS"
    );
    let one = [name("blk")];
    let mut small = [0_u8; DEVICES_HEADER_BYTES];
    assert_eq!(
        Devices {
            devices: 1,
            more: 0,
            first: true,
            names: &one,
            unisolated_allowed: false,
        }
        .encode(&mut small),
        None,
        "a buffer without room for the name"
    );
}

#[test]
fn the_short_messages_round_trip_and_are_sixteen_bytes() {
    for message in [
        Message::Report {
            started: 1,
            failed: 0,
        },
        Message::Published {
            location: 0x0001_0318,
        },
        Message::Died {
            location: 0x0001_0318,
            status: -1,
        },
        Message::Restarted {
            location: 0x0001_0318,
            restarts: 2,
        },
    ] {
        let bytes = message.encode();
        assert_eq!(
            bytes[4..8],
            (SHORT_BYTES as u32).to_le_bytes(),
            "{message:?}"
        );
        assert_eq!(Message::decode(&bytes), Ok(message), "{message:?}");
    }
    let died = Message::Died {
        location: 7,
        status: 137,
    }
    .encode();
    assert_eq!(died[0..4], DIED.to_le_bytes());
    assert_eq!(died[12..16], 137_u32.to_le_bytes(), "status");
    assert_eq!(
        Message::Report {
            started: 2,
            failed: 3
        }
        .encode()[0..4],
        REPORT.to_le_bytes()
    );
    assert_eq!(
        Message::Published { location: 0 }.encode()[0..4],
        PUBLISHED.to_le_bytes()
    );
    let restarted = Message::Restarted {
        location: 7,
        restarts: 3,
    }
    .encode();
    assert_eq!(restarted[0..4], RESTARTED.to_le_bytes());
    assert_eq!(restarted[12..16], 3_u32.to_le_bytes(), "restarts");
}

#[test]
fn bytes_that_are_not_a_message_are_malformed() {
    assert_eq!(Message::decode(&[1, 0, 0]), Err(Malformed::Short));
    assert_eq!(
        Message::decode(&[99, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Err(Malformed::UnknownType(99))
    );
    assert_eq!(
        Message::decode(&[2, 0, 0, 0, 16, 0, 0, 0]),
        Err(Malformed::Length),
        "a short REPORT"
    );
    let devices = Devices {
        devices: 1,
        more: 0,
        first: true,
        names: &[],
        unisolated_allowed: false,
    };
    let mut bytes = [0_u8; DEVICES_MAX_BYTES];
    let len = devices.encode(&mut bytes).expect("fits");
    assert_eq!(
        Message::decode(&bytes[..len]),
        Err(Malformed::UnknownType(DEVICES)),
        "DEVICES has its own decoder"
    );
    assert_eq!(
        DevicesView::decode(&Message::Published { location: 0 }.encode()),
        Err(Malformed::UnknownType(PUBLISHED))
    );
    let mut lying = bytes;
    lying[12..16].copy_from_slice(&1_u32.to_le_bytes());
    assert_eq!(
        DevicesView::decode(&lying[..len]),
        Err(Malformed::Length),
        "a name count the bytes do not carry"
    );
}

#[test]
fn the_binding_messages_round_trip() {
    for message in [
        Message::Driver {
            driver: 3,
            bus: BUS_PLATFORM,
        },
        Message::Bound {
            device: 5,
            driver: 2,
        },
        Message::Unbound { device: 35 },
        Message::Bind {
            device: 5,
            driver: 2,
            token: 0xfffe,
        },
        Message::Unbind {
            device: 5,
            driver: 2,
            token: 1,
        },
        Message::Done {
            token: 0xfffe,
            answer: ANSWER_BUSY,
        },
        Message::Done {
            token: 0,
            answer: ANSWER_DONE,
        },
    ] {
        let bytes = message.encode();
        assert_eq!(
            bytes[4..8],
            (SHORT_BYTES as u32).to_le_bytes(),
            "{message:?}"
        );
        assert_eq!(Message::decode(&bytes), Ok(message), "{message:?}");
    }
}

#[test]
fn the_binding_messages_are_laid_out_as_documented() {
    let bind = Message::Bind {
        device: 7,
        driver: 0x0102,
        token: 0x0304,
    }
    .encode();
    assert_eq!(bind[0..4], BIND.to_le_bytes());
    assert_eq!(bind[8..12], 7_u32.to_le_bytes(), "device");
    assert_eq!(bind[12..14], 0x0102_u16.to_le_bytes(), "driver");
    assert_eq!(bind[14..16], 0x0304_u16.to_le_bytes(), "token");
    let unbind = Message::Unbind {
        device: 7,
        driver: 1,
        token: 2,
    }
    .encode();
    assert_eq!(unbind[0..4], UNBIND.to_le_bytes());
    assert_eq!(unbind[12..16], [1, 0, 2, 0], "driver, then token");
    let done = Message::Done {
        token: 9,
        answer: 3,
    }
    .encode();
    assert_eq!(done[0..4], DONE.to_le_bytes());
    assert_eq!(done[8..12], 9_u32.to_le_bytes(), "token");
    assert_eq!(done[12..16], 3_u32.to_le_bytes(), "answer");
    assert_eq!(
        Message::Driver { driver: 1, bus: 1 }.encode()[0..4],
        DRIVER.to_le_bytes()
    );
    assert_eq!(
        Message::Bound {
            device: 1,
            driver: 1
        }
        .encode()[0..4],
        BOUND.to_le_bytes()
    );
    assert_eq!(
        Message::Unbound { device: 1 }.encode()[0..4],
        UNBOUND.to_le_bytes()
    );
}

/// The opt-out word's flag rides in DEVICES' flags, and only alone with
/// the bits this crate defines does it give the opt-out (ledger 3906 (2),
/// (4)).
///
/// Verifies: L.device.58
#[test]
fn the_unisolated_flag_round_trips_and_an_unknown_bit_gives_no_opt_out() {
    let message = Devices {
        devices: 1,
        more: 0,
        first: false,
        names: &[],
        unisolated_allowed: true,
    };
    let mut bytes = [0_u8; DEVICES_MAX_BYTES];
    let len = message.encode(&mut bytes).expect("fits");
    assert_eq!(bytes[20..24], FLAG_UNISOLATED_ALLOWED.to_le_bytes());
    assert!(
        DevicesView::decode(&bytes[..len])
            .unwrap()
            .unisolated_allowed
    );
    let off = Devices {
        unisolated_allowed: false,
        ..message
    };
    let len = off.encode(&mut bytes).expect("fits");
    assert!(
        !DevicesView::decode(&bytes[..len])
            .unwrap()
            .unisolated_allowed
    );
    bytes[20..24].copy_from_slice(&(FLAG_UNISOLATED_ALLOWED | 1 << 7).to_le_bytes());
    assert!(
        !DevicesView::decode(&bytes[..len])
            .unwrap()
            .unisolated_allowed,
        "an unknown bit beside it"
    );
}
