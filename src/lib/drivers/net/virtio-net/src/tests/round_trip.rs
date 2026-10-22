//! The driver against a device that behaves.

use std::vec;
use std::vec::Vec;

use ferrix_virtio::net::{
    FEATURE_CSUM, FEATURE_CTRL_VQ, FEATURE_GUEST_CSUM, FEATURE_HOST_TSO4, FEATURE_MAC,
    FEATURE_MRG_RXBUF, FEATURE_MTU, FEATURE_STATUS, HEADER_LEN, Header,
};
use ferrix_virtio::pci::{FEATURE_ACCESS_PLATFORM, FEATURE_VERSION_1};

use super::fake::{MAC, PAGE, Rig, Setup};
use crate::{Event, Frame, RECEIVE_QUEUE, ReleaseError, SubmitError, TRANSMIT_QUEUE};

/// Bytes nobody would mistake for zeros or for another frame's.
fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Send `frame` from `offset` of the transmit region and let the device take
/// it.
fn send(rig: &mut Rig, id: u64, offset: u64, frame: &[u8]) {
    rig.transmit.write(offset as usize, frame);
    assert_eq!(
        rig.driver.submit(&Frame {
            id,
            offset,
            len: frame.len() as u32,
        }),
        Ok(())
    );
}

#[test]
fn bring_up_follows_the_status_protocol_and_takes_only_the_features_it_honours() {
    let rig = Setup::new().build();
    let device = rig.device.borrow();
    // Reset, ACKNOWLEDGE, DRIVER, FEATURES_OK, DRIVER_OK.
    assert_eq!(device.status_writes, vec![0, 1, 3, 11, 15]);
    assert_eq!(
        device.accepted,
        FEATURE_VERSION_1 | FEATURE_ACCESS_PLATFORM | FEATURE_MAC | FEATURE_STATUS | FEATURE_MTU
    );
    for declined in [
        FEATURE_CSUM,
        FEATURE_GUEST_CSUM,
        FEATURE_HOST_TSO4,
        FEATURE_MRG_RXBUF,
        FEATURE_CTRL_VQ,
    ] {
        assert_eq!(
            device.accepted & declined,
            0,
            "feature {declined:#x} should have been declined"
        );
    }
    // Both queues, each with the vector it asked for.
    for (queue, vector) in [(RECEIVE_QUEUE, 1), (TRANSMIT_QUEUE, 2)] {
        let (size, _, _, _, kept, enabled) = device.queue(queue);
        assert_eq!((size, kept, enabled), (16, vector, true));
    }
    drop(device);

    let info = rig.driver.info();
    assert_eq!(info.mac, MAC);
    assert_eq!(info.limits.mtu, 1500);
    assert_eq!(info.limits.frame_capacity, 1514);
    assert_eq!(info.limits.header_len, 12, "VERSION_1 makes it twelve");
    assert_eq!((info.queue_size, info.receive_buffers), (16, 8));
    assert_eq!(info.receive_stride, 2048);
    assert!(info.link_up);
    rig.assert_clean();
}

#[test]
fn the_receive_queue_is_filled_before_any_frame_can_arrive() {
    let rig = Setup::new().build();
    let posted = rig.driver.info().receive_buffers;
    assert_eq!(posted, 8);
    assert_eq!(rig.driver.buffers_posted(), posted);

    let mut device = rig.device.borrow_mut();
    assert_eq!(
        device.take_available(RECEIVE_QUEUE),
        usize::from(posted),
        "every buffer is in the ring before the device is asked for anything"
    );
    // The doorbell was rung for the receive queue and nothing else.
    assert_eq!(device.notifications, vec![RECEIVE_QUEUE]);
    drop(device);
    rig.assert_clean();
}

#[test]
fn a_frame_submitted_appears_on_the_transmit_queue_with_its_header_and_bytes() {
    let mut rig = Setup::new().build();
    let frame = pattern(9, 100);
    send(&mut rig, 7, 64, &frame);

    let mut device = rig.device.borrow_mut();
    assert_eq!(device.transmit_all(), 1);
    let (header, bytes) = device.sent[0].clone();
    assert_eq!(
        header,
        Header::plain().encode()[..HEADER_LEN as usize].to_vec(),
        "a plain header: no offload, one buffer"
    );
    assert_eq!(bytes, frame);
    assert_eq!(device.notifications.last(), Some(&TRANSMIT_QUEUE));
    drop(device);

    assert_eq!(rig.driver.frames_in_flight(), 1);
    assert_eq!(rig.drain(), vec![Event::Sent { id: 7 }]);
    assert_eq!(rig.driver.frames_in_flight(), 0);
    rig.assert_clean();
}

#[test]
fn a_frame_crossing_a_page_boundary_is_one_chain_of_two_descriptors() {
    let mut rig = Setup::new().build();
    let frame = pattern(3, 200);
    // A hundred bytes on one page and a hundred on the next, whose device
    // addresses do not follow on.
    send(&mut rig, 1, PAGE as u64 - 100, &frame);

    let mut device = rig.device.borrow_mut();
    assert_eq!(device.transmit_all(), 1);
    assert_eq!(device.sent[0].1, frame, "the halves arrive in order");
    drop(device);
    assert_eq!(rig.drain(), vec![Event::Sent { id: 1 }]);
    rig.assert_clean();
}

#[test]
fn a_frame_the_device_writes_into_a_receive_buffer_comes_back_with_its_length() {
    let mut rig = Setup::new().build();
    let frame = pattern(5, 342);
    assert!(rig.deliver(&frame));

    let events = rig.drain();
    let Some(&Event::Received {
        buffer,
        offset,
        len,
    }) = events.first()
    else {
        panic!("a frame, not {events:?}");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(
        len, 342,
        "the length is what the device wrote past the header"
    );
    assert_eq!(offset, u64::from(buffer) * 2048);
    assert_eq!(rig.receive.read(offset as usize, 342), frame);

    // The buffer is the caller's until it gives it back, so the ring is one
    // short and no refill put it there again.
    assert_eq!(rig.driver.buffers_held(), 1);
    assert_eq!(rig.driver.buffers_posted(), 7);
    rig.assert_clean();
}

#[test]
fn a_released_buffer_goes_back_into_the_ring() {
    let mut rig = Setup::new().build();
    assert!(rig.deliver(&pattern(1, 60)));
    let events = rig.drain();
    let Some(&Event::Received { buffer, .. }) = events.first() else {
        panic!("a frame");
    };

    assert_eq!(rig.driver.release(buffer), Ok(()));
    assert_eq!(
        rig.driver.release(buffer),
        Err(ReleaseError::NotHeld(buffer)),
        "a buffer cannot be given back twice"
    );
    assert_eq!(rig.driver.release(99), Err(ReleaseError::OutOfRange(99)));
    assert_eq!(rig.driver.buffers_held(), 0);

    assert_eq!(rig.driver.refill(), Ok(1));
    assert_eq!(rig.driver.buffers_posted(), 8);
    assert!(rig.deliver(&pattern(2, 60)), "it can carry another frame");
    rig.assert_clean();
}

#[test]
fn several_frames_arrive_in_the_order_the_device_sent_them() {
    let mut rig = Setup::new().build();
    let frames: Vec<Vec<u8>> = (0..5)
        .map(|seed| pattern(seed, 64 + seed as usize))
        .collect();
    for frame in &frames {
        assert!(rig.deliver(frame));
    }
    let events = rig.drain();
    assert_eq!(events.len(), 5);
    for (event, frame) in events.iter().zip(&frames) {
        let &Event::Received { offset, len, .. } = event else {
            panic!("a frame, not {event:?}");
        };
        assert_eq!(rig.receive.read(offset as usize, len as usize), *frame);
    }
    assert_eq!(rig.driver.buffers_held(), 5);
    rig.assert_clean();
}

/// A receive room of two takes two frames and leaves the third in the used
/// ring, in order, for the next call (2026-10-22: a caller that took and
/// dropped it lost one frame in seventeen of a download).
#[test]
fn a_receive_room_takes_that_many_frames_and_leaves_the_rest_queued() {
    let mut rig = Setup::new().build();
    let frames: Vec<Vec<u8>> = (0..3).map(|seed| pattern(seed, 70)).collect();
    for frame in &frames {
        assert!(rig.deliver(frame));
    }
    let mut out = [Event::Sent { id: u64::MAX }; 8];
    let first = rig
        .driver
        .on_interrupt_receiving(&mut out, 2)
        .expect("no fault");
    assert_eq!(first.events, 2);
    assert!(first.more, "the third is still there");
    let none = rig
        .driver
        .on_interrupt_receiving(&mut out, 0)
        .expect("no fault");
    assert_eq!(none.events, 0);
    let rest = rig.drain();
    assert_eq!(rest.len(), 1);
    let &Event::Received { offset, len, .. } = &rest[0] else {
        panic!("a frame, not {:?}", rest[0]);
    };
    assert_eq!(rig.receive.read(offset as usize, len as usize), frames[2]);
}

#[test]
fn frames_go_out_and_come_in_over_and_over_without_leaking_a_descriptor() {
    let mut rig = Setup::new().build();
    let free = (rig.driver.info().queue_size, rig.driver.buffers_posted());
    for round in 0..20_u64 {
        let frame = pattern(round as u8, 128);
        send(&mut rig, round, 0, &frame);
        assert_eq!(rig.device.borrow_mut().transmit_all(), 1);
        assert!(rig.deliver(&frame));
        let events = rig.drain();
        assert_eq!(events.len(), 2, "a frame each way");
        for event in events {
            if let Event::Received { buffer, .. } = event {
                assert_eq!(rig.driver.release(buffer), Ok(()));
            }
        }
        assert_eq!(rig.driver.refill(), Ok(1));
        assert_eq!(
            (rig.driver.info().queue_size, rig.driver.buffers_posted()),
            free,
            "round {round} left the ring as it found it"
        );
    }
    assert_eq!(rig.driver.frames_in_flight(), 0);
    rig.assert_clean();
}

#[test]
fn a_frame_the_caller_got_wrong_is_refused_without_being_tracked() {
    let mut rig = Setup::new().build();
    for (frame, expected) in [
        (
            Frame {
                id: 1,
                offset: 0,
                len: 0,
            },
            SubmitError::Empty,
        ),
        (
            Frame {
                id: 2,
                offset: 0,
                len: 1515,
            },
            SubmitError::TooLong,
        ),
        (
            Frame {
                id: 3,
                offset: 4 * PAGE as u64 - 10,
                len: 64,
            },
            SubmitError::OutsideData,
        ),
    ] {
        assert_eq!(rig.driver.submit(&frame), Err(expected));
    }
    assert_eq!(rig.driver.frames_in_flight(), 0);
    assert_eq!(rig.device.borrow_mut().transmit_all(), 0);
    rig.assert_clean();
}

#[test]
fn a_transmit_queue_with_no_room_refuses_rather_than_overwriting_anything() {
    let mut rig = Setup::new().build();
    let mut accepted = 0;
    for id in 0..32_u64 {
        match rig.driver.submit(&Frame {
            id,
            offset: 0,
            len: 64,
        }) {
            Ok(()) => accepted += 1,
            Err(SubmitError::QueueFull) => break,
            Err(error) => panic!("{error:?}"),
        }
    }
    assert!(accepted > 0 && accepted <= 16, "{accepted} frames accepted");
    assert_eq!(rig.driver.frames_in_flight(), accepted);
    assert_eq!(
        rig.device.borrow_mut().transmit_all(),
        usize::from(accepted)
    );
    assert_eq!(rig.drain().len(), usize::from(accepted));
    assert_eq!(rig.driver.frames_in_flight(), 0);
    rig.assert_clean();
}
