//! The messages byte for byte, strict decoding, and a session driven through
//! a frame's life and through each way a driver can break it.

extern crate std;

use std::vec::Vec;

use ferrix_native_abi::rights::Rights;

use crate::message::*;
use crate::session::*;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn hello() -> Hello {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    modes[0] = ScanoutMode {
        width: 1280,
        height: 800,
        enabled: true,
        refresh_mhz: 74_998,
    };
    Hello {
        version: VERSION,
        scanouts: 1,
        location: 0x0000_0800,
        modes,
        virgl: false,
        capsets: 0,
        capset: 0,
        capset_bytes: 0,
        cursor: true,
        timings: Timings::NONE,
        copies: false,
    }
}

/// A board's HDMI card: 1920x1080 at 30 Hz running, 1280x720 at 60 beside it.
fn hdmi_hello() -> Hello {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    modes[0] = ScanoutMode {
        width: 1920,
        height: 1080,
        enabled: true,
        refresh_mhz: 0,
    };
    let mut timings = Timings::NONE;
    timings.count = 2;
    timings.list[0] = Timing {
        clock_khz: 74_250,
        hdisplay: 1920,
        hsync_start: 2008,
        hsync_end: 2052,
        htotal: 2200,
        vdisplay: 1080,
        vsync_start: 1084,
        vsync_end: 1089,
        vtotal: 1125,
        hsync_high: true,
        vsync_high: true,
    };
    timings.list[1] = Timing {
        clock_khz: 74_250,
        hdisplay: 1280,
        hsync_start: 1390,
        hsync_end: 1430,
        htotal: 1650,
        vdisplay: 720,
        vsync_start: 725,
        vsync_end: 730,
        vtotal: 750,
        hsync_high: true,
        vsync_high: false,
    };
    Hello {
        modes,
        cursor: false,
        timings,
        ..hello()
    }
}

#[test]
fn a_hello_carries_the_timings_a_card_runs() {
    let card = hdmi_hello();
    assert_eq!(card.validate(&Hello::HANDLE_RIGHTS), Ok(()));
    let encoded = Message::Hello(card).encode();
    let bytes = encoded.as_bytes();
    assert_eq!(u32_at(bytes, 224), 2, "two timings");
    assert_eq!(u32_at(bytes, 228), 74_250, "the first's clock");
    assert_eq!(
        u16::from_le_bytes([bytes[232], bytes[233]]),
        1920,
        "and its width"
    );
    assert_eq!(u32_at(bytes, 248), 0b11, "both syncs positive");
    assert_eq!(
        u32_at(bytes, 228 + 24 + 20),
        0b01,
        "the second's hsync only"
    );
    assert!(
        bytes[228 + 48..].iter().all(|&byte| byte == 0),
        "the rest zero"
    );
    assert_eq!(Message::decode(bytes), Ok(Message::Hello(card)));
    assert_eq!(card.timings.as_slice()[1].refresh_hz(), 60);
    assert_eq!(card.timings.as_slice()[0].refresh_hz(), 30);

    // Flags past the polarities are malformed.
    let mut odd = bytes.to_vec();
    odd[248] |= 0x4;
    assert_eq!(Message::decode(&odd), Err(MessageError::Field));
}

#[test]
fn timings_that_do_not_describe_the_scanout_are_refused() {
    let rights = Hello::HANDLE_RIGHTS;
    // The first is not scanout 0's size.
    let mut other = hdmi_hello();
    other.timings.list.swap(0, 1);
    assert_eq!(other.validate(&rights), Err(Refusal::Mode));
    // More than there is room for.
    let mut many = hdmi_hello();
    many.timings.count = MAX_TIMINGS as u32 + 1;
    assert_eq!(many.validate(&rights), Err(Refusal::Mode));
    // One past the count that is not zero.
    let mut stale = hdmi_hello();
    stale.timings.count = 1;
    assert_eq!(stale.validate(&rights), Err(Refusal::Mode));
    // Spans out of order.
    let mut backwards = hdmi_hello();
    backwards.timings.list[1].hsync_end = 1300;
    assert_eq!(backwards.validate(&rights), Err(Refusal::Mode));
    // On a card of two scanouts, which of them would they be.
    let mut two = hdmi_hello();
    two.scanouts = 2;
    assert_eq!(two.validate(&rights), Err(Refusal::Mode));
}

fn buffer(id: u32, offset: u64) -> Attach {
    Attach {
        buffer: id,
        format: FORMAT,
        offset,
        // 1280 × 800 × 4 = 4 096 000 bytes, rounded up to 1000 pages.
        length: 1000 * PAGE_SIZE,
        width: 1280,
        height: 800,
        stride: 5120,
    }
}

/// A buffer whose pixels the device holds already: the same shape, named by
/// the renderer's object rather than by a range.
fn object_buffer(id: u32) -> AttachObject {
    AttachObject {
        buffer: id,
        object: 1 << 30,
        format: FORMAT,
        width: 1280,
        height: 800,
        stride: 5120,
    }
}

const CARD: u64 = 256 * 1024 * 1024;

fn every_message() -> Vec<Message> {
    let rect = Rect {
        x: 1,
        y: 2,
        width: 3,
        height: 4,
    };
    std::vec![
        Message::Hello(hello()),
        Message::Ready(Ready {
            card: 0,
            card_bytes: CARD,
        }),
        Message::Refused(Refusal::Protocol),
        Message::Attach(buffer(7, 4096)),
        Message::AttachObject(object_buffer(9)),
        Message::Attached {
            buffer: 7,
            status: Status::PinFailed,
        },
        Message::Scanout {
            scanout: 0,
            buffer: 7,
            rect,
        },
        Message::Flush {
            buffer: 7,
            sequence: 1 << 40,
            rect,
        },
        Message::Flipped {
            sequence: 1 << 40,
            status: Status::Ok,
        },
        Message::Detach { buffer: 7 },
        Message::Detached {
            buffer: 7,
            status: Status::DeviceRefused,
        },
        Message::Stop,
        Message::Stopped,
        Message::Cursor {
            scanout: 1,
            buffer: 7,
            sequence: 1 << 41,
            hot_x: 63,
            hot_y: 2,
            x: -5,
            y: 1079,
        },
        Message::Move {
            scanout: 1,
            x: -5,
            y: i32::MAX,
        },
        Message::Modes { modes: resized() },
    ]
}

/// Scanout 0 at a window's new size, scanout 1 unplugged.
fn resized() -> [ScanoutMode; MAX_SCANOUTS] {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    modes[0] = ScanoutMode {
        width: 2554,
        height: 1377,
        enabled: true,
        refresh_mhz: 143_912,
    };
    modes[1] = ScanoutMode {
        width: 1024,
        height: 768,
        enabled: false,
        refresh_mhz: 0,
    };
    modes
}

#[test]
fn modes_carries_every_scanout_where_hello_does_after_its_header() {
    let encoded = Message::Modes { modes: resized() }.encode();
    let bytes = encoded.as_bytes();
    assert_eq!(bytes.len(), 8 + 16 * 12 + 16 * 4);
    assert_eq!(
        [u32_at(bytes, 8), u32_at(bytes, 12), u32_at(bytes, 16)],
        [2554, 1377, 1]
    );
    assert_eq!(
        [u32_at(bytes, 20), u32_at(bytes, 24), u32_at(bytes, 28)],
        [1024, 768, 0]
    );
    // Each scanout's refresh after all sixteen scanouts, so that version
    // 6's offsets hold: the unplugged one has none.
    assert_eq!([u32_at(bytes, 200), u32_at(bytes, 204)], [143_912, 0]);
    assert!(bytes[208..].iter().all(|&byte| byte == 0));
    // An `enabled` other than 0 or 1 is not a MODES.
    let mut bent = bytes.to_vec();
    bent[16] = 2;
    assert_eq!(Message::decode(&bent), Err(MessageError::Field));
}

#[test]
fn a_mode_list_is_checked_against_the_card_hello_described() {
    assert_eq!(validate_modes(&resized(), 2), Ok(()));
    // A scanout past the count HELLO gave.
    assert_eq!(validate_modes(&resized(), 1), Err(Refusal::Mode));
    let mut empty = resized();
    empty[0].width = 0;
    assert_eq!(validate_modes(&empty, 2), Err(Refusal::Mode));
    let mut huge = resized();
    huge[0].height = MAX_DIMENSION + 1;
    assert_eq!(validate_modes(&huge, 2), Err(Refusal::Mode));
    // A refresh no display has, and one for a display that is not there.
    let mut fastest = resized();
    fastest[0].refresh_mhz = MAX_REFRESH_MHZ;
    assert_eq!(validate_modes(&fastest, 2), Ok(()));
    fastest[0].refresh_mhz = MAX_REFRESH_MHZ + 1;
    assert_eq!(validate_modes(&fastest, 2), Err(Refusal::Mode));
    let mut unplugged = resized();
    unplugged[1].refresh_mhz = 60_000;
    assert_eq!(validate_modes(&unplugged, 2), Err(Refusal::Mode));
    // And 0, a refresh the driver does not know, is no refusal.
    let mut unknown = resized();
    unknown[0].refresh_mhz = 0;
    assert_eq!(validate_modes(&unknown, 2), Ok(()));
}

// -- Bytes ----------------------------------------------------------------------

#[test]
fn every_message_round_trips_at_its_fixed_length() {
    for message in every_message() {
        let encoded = message.encode();
        let bytes = encoded.as_bytes();
        assert_eq!(u32_at(bytes, 0), message.kind());
        assert_eq!(u32_at(bytes, 4) as usize, bytes.len());
        assert_eq!(Message::length_of(message.kind()), Some(bytes.len()));
        assert_eq!(Message::decode(bytes), Ok(message), "{message:?}");
    }
}

#[test]
fn fields_lie_where_the_specification_puts_them() {
    let plain = Message::Hello(hello()).encode();
    let bytes = plain.as_bytes();
    // 16 of header and fields, 16 scanouts of 12, 12 for what the card said
    // about 3D, 4 for whether it has a cursor plane, and 4 + 16 x 24 for
    // the timings a card that runs only some lists, 16 x 4 for each
    // scanout's refresh, and 4 for the flags.
    assert_eq!(bytes.len(), 680);
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 8, "VERSION");
    assert_eq!(u32_at(bytes, 612), 74_998, "scanout 0's refresh");
    assert!(bytes[616..].iter().all(|&byte| byte == 0));
    assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), 1);
    assert_eq!(u32_at(bytes, 12), 0x800);
    assert_eq!(
        [u32_at(bytes, 16), u32_at(bytes, 20), u32_at(bytes, 24)],
        [1280, 800, 1]
    );
    // What the card said about 3D, after the modes: a 2D card says nothing.
    assert_eq!(u16::from_le_bytes([bytes[208], bytes[209]]), 0, "virgl");
    assert_eq!(u16::from_le_bytes([bytes[210], bytes[211]]), 0, "capsets");
    assert_eq!(u32_at(bytes, 212), 0, "the first capset");
    assert_eq!(u32_at(bytes, 216), 0, "and how much of it was fetched");
    // And after that, whether the card has a cursor plane: this one has.
    assert_eq!(u32_at(bytes, 220), 1, "cursor");

    assert!(bytes[28..220].iter().all(|&byte| byte == 0));
    // And a 3D card's, which is what a `virtio-gpu-gl` answers.
    let mut card = hello();
    card.virgl = true;
    card.capsets = 2;
    card.capset = 1;
    card.capset_bytes = 308;
    let encoded = Message::Hello(card).encode();
    let bytes = encoded.as_bytes();
    assert_eq!(u16::from_le_bytes([bytes[208], bytes[209]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[210], bytes[211]]), 2);
    assert_eq!(u32_at(bytes, 212), 1);
    assert_eq!(u32_at(bytes, 216), 308);
    assert_eq!(Message::decode(bytes), Ok(Message::Hello(card)));

    let attach = Message::Attach(buffer(7, 8192)).encode();
    let bytes = attach.as_bytes();
    assert_eq!(bytes.len(), 48);
    assert_eq!([u32_at(bytes, 8), u32_at(bytes, 12)], [7, 0x3432_5258]);
    assert_eq!([u64_at(bytes, 16), u64_at(bytes, 24)], [8192, 4_096_000]);
    assert_eq!(
        [u32_at(bytes, 32), u32_at(bytes, 36), u32_at(bytes, 40)],
        [1280, 800, 5120]
    );

    let flush = Message::Flush {
        buffer: 7,
        sequence: 9,
        rect: Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        },
    }
    .encode();
    let bytes = flush.as_bytes();
    assert_eq!(bytes.len(), 40);
    assert_eq!((u32_at(bytes, 8), u64_at(bytes, 16)), (7, 9));
    assert_eq!([24, 28, 32, 36].map(|at| u32_at(bytes, at)), [1, 2, 3, 4]);

    let ready = Message::Ready(Ready {
        card: 3,
        card_bytes: CARD,
    })
    .encode();
    assert_eq!(
        (u32_at(ready.as_bytes(), 8), u64_at(ready.as_bytes(), 16)),
        (3, CARD)
    );
}

#[test]
fn decoding_is_strict() {
    let stop = Message::Stop.encode();
    assert_eq!(
        Message::decode(&stop.as_bytes()[..7]),
        Err(MessageError::Short)
    );

    let mut unknown = stop.as_bytes().to_vec();
    unknown[0] = 99;
    assert_eq!(Message::decode(&unknown), Err(MessageError::Type(99)));

    // The length field disagrees with the type, or the bytes run on.
    let mut lying = stop.as_bytes().to_vec();
    lying[4] = 9;
    assert_eq!(Message::decode(&lying), Err(MessageError::Length));
    let mut longer = stop.as_bytes().to_vec();
    longer.push(0);
    assert_eq!(Message::decode(&longer), Err(MessageError::Length));

    // A reserved byte set, a status nobody defined, an enabled flag of 2.
    let mut reserved = Message::Detach { buffer: 1 }.encode().as_bytes().to_vec();
    reserved[12] = 1;
    assert_eq!(Message::decode(&reserved), Err(MessageError::Field));
    let mut status = Message::Attached {
        buffer: 1,
        status: Status::Ok,
    }
    .encode()
    .as_bytes()
    .to_vec();
    status[12] = 5;
    assert_eq!(Message::decode(&status), Err(MessageError::Field));
    let mut enabled = Message::Hello(hello()).encode().as_bytes().to_vec();
    enabled[24] = 2;
    assert_eq!(Message::decode(&enabled), Err(MessageError::Field));
    let mut refusal = Message::Refused(Refusal::Version)
        .encode()
        .as_bytes()
        .to_vec();
    refusal[8] = 0;
    assert_eq!(Message::decode(&refusal), Err(MessageError::Field));
}

// -- HELLO and ATTACH ------------------------------------------------------------

#[test]
fn hello_is_validated_field_by_field_and_by_its_rights() {
    let rights = Hello::HANDLE_RIGHTS;
    assert_eq!(hello().validate(&rights), Ok(()));

    let mut wrong = hello();
    wrong.version = 1;
    assert_eq!(wrong.validate(&rights), Err(Refusal::Version));

    // A 2D card with capability sets, and a card naming a set it has none
    // of: both are a driver saying something it cannot know.
    let mut wrong = hello();
    wrong.capsets = 1;
    assert_eq!(wrong.validate(&rights), Err(Refusal::Capsets));
    let mut wrong = hello();
    wrong.virgl = true;
    wrong.capset = 2;
    assert_eq!(wrong.validate(&rights), Err(Refusal::Capsets));
    // Bytes of a set that was never named are bytes of nothing.
    let mut wrong = hello();
    wrong.virgl = true;
    wrong.capsets = 1;
    wrong.capset_bytes = 8;
    assert_eq!(wrong.validate(&rights), Err(Refusal::Capsets));
    // And a 3D card that hangs together.
    let mut fine = hello();
    fine.virgl = true;
    fine.capsets = 2;
    fine.capset = 1;
    fine.capset_bytes = 308;
    assert_eq!(fine.validate(&rights), Ok(()));

    for scanouts in [0, 17] {
        let mut wrong = hello();
        wrong.scanouts = scanouts;
        assert_eq!(wrong.validate(&rights), Err(Refusal::Scanouts));
    }

    for mode in [
        ScanoutMode {
            width: 0,
            height: 800,
            enabled: true,
            refresh_mhz: 0,
        },
        ScanoutMode {
            width: 8193,
            height: 800,
            enabled: true,
            refresh_mhz: 0,
        },
        ScanoutMode {
            width: 8193,
            height: 0,
            enabled: false,
            refresh_mhz: 0,
        },
    ] {
        let mut wrong = hello();
        wrong.modes[0] = mode;
        assert_eq!(wrong.validate(&rights), Err(Refusal::Mode), "{mode:?}");
    }
    let mut past = hello();
    past.modes[1].width = 1;
    assert_eq!(
        past.validate(&rights),
        Err(Refusal::Mode),
        "a mode past the count"
    );

    let mut disconnected = hello();
    disconnected.modes[0] = ScanoutMode::default();
    assert_eq!(
        disconnected.validate(&rights),
        Ok(()),
        "nothing attached is fine"
    );

    for handles in [
        &[][..],
        &[Rights::WRITE],
        &[PORT_RIGHTS, PORT_RIGHTS],
        &[Rights(PORT_RIGHTS.0 | Rights::DUPLICATE.0)],
    ] {
        assert_eq!(hello().validate(handles), Err(Refusal::Rights));
    }
}

#[test]
fn attach_is_validated_against_itself_and_the_card() {
    assert_eq!(buffer(7, 0).validate(CARD), Ok(()));
    let with = |change: fn(&mut Attach)| {
        let mut attach = buffer(7, 0);
        change(&mut attach);
        attach.validate(CARD)
    };
    assert_eq!(with(|a| a.buffer = 0), Err(AttachError::Id));
    assert_eq!(with(|a| a.format = 0x3432_4241), Err(AttachError::Format));
    assert_eq!(with(|a| a.width = 0), Err(AttachError::Size));
    assert_eq!(with(|a| a.height = 8193), Err(AttachError::Size));
    assert_eq!(with(|a| a.stride = 5119), Err(AttachError::Stride));
    assert_eq!(
        with(|a| a.length = 999 * PAGE_SIZE),
        Err(AttachError::Stride)
    );
    assert_eq!(with(|a| a.offset = 1), Err(AttachError::Range));
    assert_eq!(
        with(|a| a.length = 1000 * PAGE_SIZE + 1),
        Err(AttachError::Range)
    );
    assert_eq!(
        with(|a| a.offset = CARD - 999 * PAGE_SIZE),
        Err(AttachError::Range)
    );
    assert_eq!(
        with(|a| a.offset = u64::MAX - 4095),
        Err(AttachError::Range)
    );
    assert_eq!(with(|a| a.offset = CARD - 1000 * PAGE_SIZE), Ok(()));
}

// -- The session -----------------------------------------------------------------

fn session() -> Session {
    Session::accept(&hello(), &Hello::HANDLE_RIGHTS, CARD).expect("a good HELLO")
}

#[test]
fn modes_replaces_what_hello_said_and_a_bad_list_breaks_the_session() {
    let mut session = session();
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    modes[0] = ScanoutMode {
        width: 2554,
        height: 1377,
        enabled: true,
        refresh_mhz: 0,
    };
    assert_eq!(session.receive(&Message::Modes { modes }), Ok(Event::Modes));
    assert_eq!(session.modes(), &modes[..1]);
    // HELLO's count stays: a second scanout is one the card never had.
    modes[1] = modes[0];
    assert_eq!(
        session.receive(&Message::Modes { modes }),
        Err(Refusal::Protocol)
    );
    assert!(session.is_broken());
}

fn full() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: 1280,
        height: 800,
    }
}

#[test]
fn a_frame_from_attach_to_detach() {
    let mut session = session();
    assert_eq!(session.modes().len(), 1);

    assert_eq!(
        session.attach(buffer(7, 0)),
        Ok(Message::Attach(buffer(7, 0)))
    );
    assert_eq!(
        session.scanout(0, 7, full()),
        Err(RequestError::NotAttached),
        "not before the driver says it attached"
    );
    assert_eq!(
        session.receive(&Message::Attached {
            buffer: 7,
            status: Status::Ok
        }),
        Ok(Event::Attached {
            buffer: 7,
            status: Status::Ok
        })
    );

    assert!(session.scanout(0, 7, full()).is_ok());
    let Ok(Message::Flush { sequence, .. }) = session.flush(7, full()) else {
        panic!("a flush");
    };
    assert_eq!(sequence, 1);
    assert_eq!(
        session.detach(7),
        Err(RequestError::Busy),
        "shown and in flight"
    );
    assert_eq!(
        session.receive(&Message::Flipped {
            sequence: 1,
            status: Status::Ok
        }),
        Ok(Event::Flipped {
            sequence: 1,
            buffer: 7,
            status: Status::Ok
        })
    );
    assert_eq!(session.detach(7), Err(RequestError::Busy), "still shown");
    assert!(session.scanout(0, 0, Rect::default()).is_ok());
    assert_eq!(session.detach(7), Ok(Message::Detach { buffer: 7 }));
    assert_eq!(
        session.receive(&Message::Detached {
            buffer: 7,
            status: Status::Ok
        }),
        Ok(Event::Detached {
            buffer: 7,
            status: Status::Ok
        })
    );
    assert_eq!(
        session.attach(buffer(7, 0)),
        Ok(Message::Attach(buffer(7, 0))),
        "id free again"
    );
    assert!(
        session
            .receive(&Message::Attached {
                buffer: 7,
                status: Status::Ok
            })
            .is_ok()
    );
    assert!(session.detach(7).is_ok());
    assert_eq!(
        session.receive(&Message::Detached {
            buffer: 7,
            status: Status::DeviceRefused
        }),
        Ok(Event::Detached {
            buffer: 7,
            status: Status::DeviceRefused
        })
    );
    assert_eq!(
        session.attach(buffer(7, 0)),
        Err(RequestError::InUse),
        "a refused detach loses the id for good"
    );
    assert_eq!(session.detach(7), Err(RequestError::NotAttached));

    assert_eq!(session.stop(), Ok(Message::Stop));
    assert_eq!(session.attach(buffer(8, 0)), Err(RequestError::Closed));
    assert_eq!(session.receive(&Message::Stopped), Ok(Event::Stopped));
}

#[test]
fn a_failed_attach_frees_the_id() {
    let mut session = session();
    assert!(session.attach(buffer(7, 0)).is_ok());
    assert_eq!(
        session.receive(&Message::Attached {
            buffer: 7,
            status: Status::PinFailed
        }),
        Ok(Event::Attached {
            buffer: 7,
            status: Status::PinFailed
        })
    );
    assert_eq!(session.flush(7, full()), Err(RequestError::NotAttached));
    assert!(session.attach(buffer(7, 0)).is_ok());
}

#[test]
fn requests_the_protocol_has_no_state_for_are_refused() {
    let mut session = session();
    assert_eq!(
        session.attach(buffer(0, 0)),
        Err(RequestError::Attach(AttachError::Id))
    );
    assert!(session.attach(buffer(7, 0)).is_ok());
    assert_eq!(session.attach(buffer(7, 0)), Err(RequestError::InUse));
    assert!(
        session
            .receive(&Message::Attached {
                buffer: 7,
                status: Status::Ok
            })
            .is_ok()
    );

    assert_eq!(
        session.scanout(1, 7, full()),
        Err(RequestError::NoSuchScanout)
    );
    let outside = Rect { x: 1, ..full() };
    assert_eq!(session.scanout(0, 7, outside), Err(RequestError::Rect));
    assert_eq!(session.flush(7, Rect::default()), Err(RequestError::Rect));
    assert_eq!(session.flush(8, full()), Err(RequestError::NotAttached));

    for _ in 0..MAX_IN_FLIGHT {
        assert!(session.flush(7, full()).is_ok());
    }
    assert_eq!(session.flush(7, full()), Err(RequestError::TooManyFlushes));

    for id in 8..=u32::try_from(MAX_BUFFERS + 6).expect("small") {
        assert!(session.attach(buffer(id, 0)).is_ok(), "buffer {id}");
    }
    assert_eq!(session.attach(buffer(99, 0)), Err(RequestError::Full));
}

#[test]
fn a_driver_that_answers_what_nobody_asked_breaks_the_session() {
    type Setup = fn(&mut Session);
    let cases: [(Setup, Message); 7] = [
        (
            |_| {},
            Message::Attached {
                buffer: 7,
                status: Status::Ok,
            },
        ),
        (
            |_| {},
            Message::Flipped {
                sequence: 1,
                status: Status::Ok,
            },
        ),
        (
            |_| {},
            Message::Detached {
                buffer: 7,
                status: Status::Ok,
            },
        ),
        (|_| {}, Message::Stopped),
        (|_| {}, Message::Hello(hello())),
        (|_| {}, Message::Attach(buffer(7, 0))),
        // Flushes 1 and 2 in flight; the driver finishes 2 first.
        (
            |session| {
                assert!(session.attach(buffer(7, 0)).is_ok());
                assert!(
                    session
                        .receive(&Message::Attached {
                            buffer: 7,
                            status: Status::Ok
                        })
                        .is_ok()
                );
                assert!(session.flush(7, full()).is_ok());
                assert!(session.flush(7, full()).is_ok());
            },
            Message::Flipped {
                sequence: 2,
                status: Status::Ok,
            },
        ),
    ];
    for (setup, message) in cases {
        let mut session = session();
        setup(&mut session);
        assert_eq!(
            session.receive(&message),
            Err(Refusal::Protocol),
            "{message:?}"
        );
        assert!(session.is_broken());
        assert_eq!(session.attach(buffer(50, 0)), Err(RequestError::Closed));
        assert_eq!(
            session.receive(&Message::Stopped),
            Err(Refusal::Protocol),
            "a broken session stays broken"
        );
    }

    // Detached for a buffer that is attached but was never asked to detach.
    let mut session = session();
    assert!(session.attach(buffer(7, 0)).is_ok());
    assert_eq!(
        session.receive(&Message::Detached {
            buffer: 7,
            status: Status::Ok
        }),
        Err(Refusal::Protocol)
    );
}

#[test]
fn accept_refuses_what_validate_refuses() {
    let mut wrong = hello();
    wrong.scanouts = 0;
    assert_eq!(
        Session::accept(&wrong, &Hello::HANDLE_RIGHTS, CARD).map(|_| ()),
        Err(Refusal::Scanouts)
    );
}

/// `ATTACH_OBJ`'s fields lie where the specification puts them, and it is
/// checked the way an ATTACH is but for the range it has not got.
#[test]
fn an_object_buffer_is_checked_without_a_range() {
    let encoded = Message::AttachObject(object_buffer(9)).encode();
    let bytes = encoded.as_bytes();
    assert_eq!(bytes.len(), 32);
    assert_eq!(u32_at(bytes, 0), 13, "ATTACH_OBJ");
    assert_eq!(u32_at(bytes, 8), 9, "the buffer");
    assert_eq!(u32_at(bytes, 12), 1 << 30, "the object");
    assert_eq!(u32_at(bytes, 16), FORMAT);
    assert_eq!(u32_at(bytes, 20), 1280);
    assert_eq!(u32_at(bytes, 24), 800);
    assert_eq!(u32_at(bytes, 28), 5120);

    let refused = |change: fn(&mut AttachObject)| {
        let mut attach = object_buffer(9);
        change(&mut attach);
        attach.validate().expect_err("refused")
    };
    assert_eq!(refused(|a| a.buffer = 0), AttachError::Id);
    assert_eq!(refused(|a| a.object = 0), AttachError::Id, "no object");
    assert_eq!(refused(|a| a.format = FORMAT + 1), AttachError::Format);
    assert_eq!(refused(|a| a.width = 0), AttachError::Size);
    assert_eq!(refused(|a| a.height = MAX_DIMENSION + 1), AttachError::Size);
    assert_eq!(refused(|a| a.stride = 5119), AttachError::Stride);
    // A card VMO it is not in: there is no range to be outside of, and a
    // buffer of pixels the device holds is not bounded by one.
    object_buffer(9).validate().expect("nothing else to check");
}

/// A buffer made either way is the same buffer to the conversation: it is
/// attached once, shown and flushed inside its own shape, and detached.
#[test]
fn an_object_buffer_is_shown_and_flushed_like_any_other() {
    let mut core = session();
    let _ = core
        .attach_object(object_buffer(9))
        .expect("a buffer with no range");
    assert_eq!(
        core.attach_object(object_buffer(9)),
        Err(RequestError::InUse),
        "one id, one buffer"
    );
    // An id a range-backed buffer has is the same id.
    assert_eq!(
        core.attach(buffer(9, 4096)),
        Err(RequestError::InUse),
        "whichever way it was made"
    );
    assert_eq!(
        core.scanout(0, 9, full()),
        Err(RequestError::NotAttached),
        "not until the driver has answered"
    );
    let _ = core
        .receive(&Message::Attached {
            buffer: 9,
            status: Status::Ok,
        })
        .expect("attached");
    let _ = core.scanout(0, 9, full()).expect("shown");
    assert_eq!(
        core.scanout(
            0,
            9,
            Rect {
                x: 0,
                y: 0,
                width: 1281,
                height: 800,
            }
        ),
        Err(RequestError::Rect),
        "outside its own shape"
    );
    let Message::Flush { sequence, .. } = core.flush(9, full()).expect("flushed") else {
        panic!("a flush is a FLUSH");
    };
    // Not while the device may still be reading it, whatever its pixels are.
    assert_eq!(core.detach(9), Err(RequestError::Busy));
    let _ = core
        .receive(&Message::Flipped {
            sequence,
            status: Status::Ok,
        })
        .expect("flipped");
    let _ = core.scanout(0, 0, full()).expect("the screen off");
    let _ = core.detach(9).expect("let go of");
}

// -- The cursor -------------------------------------------------------------------

/// A 64 × 64 buffer, which is the one size a cursor can be.
fn cursor_buffer(id: u32, offset: u64) -> Attach {
    Attach {
        buffer: id,
        format: FORMAT,
        offset,
        length: 16384,
        width: CURSOR_SIZE,
        height: CURSOR_SIZE,
        stride: CURSOR_SIZE * 4,
    }
}

#[test]
fn a_cursor_and_a_move_lie_where_the_protocol_puts_them() {
    let cursor = Message::Cursor {
        scanout: 1,
        buffer: 7,
        sequence: 1 << 33,
        hot_x: 3,
        hot_y: 4,
        x: -2,
        y: 600,
    }
    .encode();
    let bytes = cursor.as_bytes();
    assert_eq!((u32_at(bytes, 0), bytes.len()), (14, 40));
    assert_eq!(
        [u32_at(bytes, 8), u32_at(bytes, 12)],
        [1, 7],
        "scanout and buffer"
    );
    assert_eq!(u64_at(bytes, 16), 1 << 33);
    assert_eq!(
        [
            u32_at(bytes, 24),
            u32_at(bytes, 28),
            u32_at(bytes, 32),
            u32_at(bytes, 36)
        ],
        [3, 4, (-2i32) as u32, 600]
    );
    let moved = Message::Move {
        scanout: 1,
        x: 9,
        y: -9,
    }
    .encode();
    let bytes = moved.as_bytes();
    assert_eq!((u32_at(bytes, 0), bytes.len()), (15, 24));
    assert_eq!(
        [
            u32_at(bytes, 8),
            u32_at(bytes, 12),
            u32_at(bytes, 16),
            u32_at(bytes, 20)
        ],
        [1, 0, 9, (-9i32) as u32]
    );

    // A hotspot outside the image, and a MOVE with its reserved word set,
    // are not messages.
    let mut outside = cursor.as_bytes().to_vec();
    outside[24] = 64;
    assert_eq!(Message::decode(&outside), Err(MessageError::Field));
    let mut reserved = moved.as_bytes().to_vec();
    reserved[12] = 1;
    assert_eq!(Message::decode(&reserved), Err(MessageError::Field));
}

#[test]
fn a_cursor_waits_in_the_flushes_line_and_a_move_in_none() {
    let mut session = session();
    for attach in [cursor_buffer(3, 0), buffer(7, 16384)] {
        assert!(session.attach(attach).is_ok());
        assert!(
            session
                .receive(&Message::Attached {
                    buffer: attach.buffer,
                    status: Status::Ok
                })
                .is_ok()
        );
    }

    // A frame's flush, then the cursor, then another flush: one line, and
    // FLIPPED answers them in it.
    let Ok(Message::Flush {
        sequence: first, ..
    }) = session.flush(7, full())
    else {
        panic!("a flush");
    };
    let Ok(Message::Cursor { sequence, .. }) = session.cursor(0, 3, (1, 2), (10, 20)) else {
        panic!("a cursor");
    };
    assert_eq!(sequence, first + 1);
    assert_eq!(
        session.move_cursor(0, (11, 21)),
        Ok(Message::Move {
            scanout: 0,
            x: 11,
            y: 21
        })
    );
    // The cursor's buffer is on its way to the device, as a flushed one is,
    // and cannot be let go of before FLIPPED.
    assert_eq!(session.detach(3), Err(RequestError::Busy));
    for done in [first, sequence] {
        assert!(matches!(
            session.receive(&Message::Flipped {
                sequence: done,
                status: Status::Ok
            }),
            Ok(Event::Flipped { .. })
        ));
    }
    assert!(session.detach(3).is_ok());
    // No cursor at all is buffer 0, and it too is answered in line.
    assert!(matches!(
        session.cursor(0, 0, (0, 0), (0, 0)),
        Ok(Message::Cursor { buffer: 0, .. })
    ));
}

#[test]
fn a_cursor_the_device_cannot_show_is_refused() {
    let mut session = session();
    assert!(session.attach(buffer(7, 0)).is_ok());
    assert!(
        session
            .receive(&Message::Attached {
                buffer: 7,
                status: Status::Ok
            })
            .is_ok()
    );
    // Not 64 square; not attached; a hotspot outside it; no such scanout.
    assert_eq!(
        session.cursor(0, 7, (0, 0), (0, 0)),
        Err(RequestError::Cursor)
    );
    assert_eq!(
        session.cursor(0, 8, (0, 0), (0, 0)),
        Err(RequestError::NotAttached)
    );
    assert_eq!(
        session.cursor(0, 0, (CURSOR_SIZE, 0), (0, 0)),
        Err(RequestError::Cursor)
    );
    assert_eq!(
        session.cursor(1, 0, (0, 0), (0, 0)),
        Err(RequestError::NoSuchScanout)
    );
    assert_eq!(
        session.move_cursor(1, (0, 0)),
        Err(RequestError::NoSuchScanout)
    );
    // A card that said it has no cursor plane is sent neither.
    let mut plain = Session::accept(
        &Hello {
            cursor: false,
            ..hello()
        },
        &Hello::HANDLE_RIGHTS,
        CARD,
    )
    .expect("a good HELLO");
    assert!(!plain.has_cursor());
    assert_eq!(
        plain.cursor(0, 0, (0, 0), (0, 0)),
        Err(RequestError::Cursor)
    );
    assert_eq!(plain.move_cursor(0, (0, 0)), Err(RequestError::Cursor));
    // And a driver never answers a MOVE: a FLIPPED with nothing in line
    // breaks the session.
    assert!(session.move_cursor(0, (5, 5)).is_ok());
    assert_eq!(
        session.receive(&Message::Flipped {
            sequence: 1,
            status: Status::Ok
        }),
        Err(Refusal::Protocol)
    );
}

#[test]
fn a_hello_says_whether_its_driver_copies() {
    let copying = Hello {
        copies: true,
        ..hdmi_hello()
    };
    assert_eq!(copying.validate(&Hello::HANDLE_RIGHTS), Ok(()));
    let encoded = Message::Hello(copying).encode();
    let bytes = encoded.as_bytes();
    assert_eq!(bytes.len(), HELLO_BYTES);
    assert_eq!(
        u32_at(bytes, 676),
        HELLO_COPIES,
        "the flag follows the refreshes"
    );
    assert_eq!(Message::decode(bytes), Ok(Message::Hello(copying)));

    let plain = Message::Hello(hdmi_hello()).encode();
    assert_eq!(
        u32_at(plain.as_bytes(), 676),
        0,
        "a driver that does not copy"
    );
}

#[test]
fn a_reserved_hello_flag_is_malformed() {
    let encoded = Message::Hello(hello()).encode();
    for bit in 1..32 {
        let mut bent = encoded.as_bytes().to_vec();
        bent[676..680].copy_from_slice(&(1_u32 << bit).to_le_bytes());
        assert_eq!(
            Message::decode(&bent),
            Err(MessageError::Field),
            "flag bit {bit}"
        );
    }
}

#[test]
fn only_a_copying_driver_is_handed_a_card_it_can_map() {
    assert_eq!(Ready::handle_rights(false), Ready::HANDLE_RIGHTS);
    assert_eq!(Ready::card_rights(false), CARD_VMO_RIGHTS);
    assert_eq!(Ready::handle_rights(true), Ready::COPY_HANDLE_RIGHTS);
    assert_eq!(Ready::card_rights(true), CARD_VMO_COPY_RIGHTS);
    for rights in [CARD_VMO_RIGHTS, CARD_VMO_COPY_RIGHTS] {
        assert_eq!(rights.0 & Rights::WRITE.0, 0, "never WRITE");
    }
    assert_eq!(CARD_VMO_RIGHTS.0 & Rights::MAP.0, 0);
    assert_ne!(CARD_VMO_COPY_RIGHTS.0 & Rights::MAP.0, 0);
}
