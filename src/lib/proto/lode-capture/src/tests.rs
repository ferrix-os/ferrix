extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::{FORMAT_QOI, Malformed, Message, qoi};

fn all() -> Vec<Message> {
    vec![
        Message::Register {
            name: String::from("hyprix"),
            seat: String::from("seat0"),
            outputs: String::from("HDMI-A-1,eDP-1"),
            formats: FORMAT_QOI,
            token: [9; 16],
        },
        Message::Accepted {
            formats: FORMAT_QOI,
        },
        Message::Refused {
            why: String::from("no token"),
        },
        Message::Capture {
            request: 7,
            seqnum: 81_234,
            what: String::from("hyprix ended by SIGBUS"),
        },
        Message::Image {
            request: 7,
            format: FORMAT_QOI,
            width: 2,
            height: 1,
            bytes: vec![1, 2, 3],
        },
        Message::Declined {
            request: 7,
            why: String::from("locked"),
        },
        Message::Unregister,
    ]
}

#[test]
fn every_message_round_trips() {
    for message in all() {
        let frame = message.encode();
        assert_eq!(Message::decode(&frame), Ok(Some((message, frame.len()))));
    }
}

#[test]
fn a_frame_not_whole_waits_and_two_frames_read_one_at_a_time() {
    let mut bytes = all()[3].encode();
    let first = bytes.len();
    for cut in 0..first {
        assert_eq!(Message::decode(&bytes[..cut]), Ok(None), "cut at {cut}");
    }
    bytes.extend_from_slice(&all()[6].encode());
    let (message, used) = Message::decode(&bytes).unwrap().unwrap();
    assert_eq!(message, all()[3]);
    assert_eq!(used, first);
}

#[test]
fn malformed_frames_are_refused() {
    assert_eq!(Message::decode(&[99, 0, 0, 0, 0, 0, 0, 0]), Err(Malformed));
    assert_eq!(
        Message::decode(&[1, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]),
        Err(Malformed)
    );
    let mut extra = all()[1].encode();
    extra[4] += 1;
    extra.push(0);
    assert_eq!(Message::decode(&extra), Err(Malformed));
}

#[test]
fn qoi_round_trips_a_screen() {
    let (width, height) = (37_u32, 11_u32);
    let stride = 37 * 4 + 12;
    let mut xrgb = vec![0_u8; stride * 11];
    for y in 0..11 {
        for x in 0..37 {
            let at = y * stride + x * 4;
            let value = if x < 10 {
                200
            } else {
                ((x * 7 + y * 13) % 256) as u8
            };
            xrgb[at..at + 4].copy_from_slice(&[value, value / 2, 255 - value, 0]);
        }
    }
    let encoded = qoi::encode(width, height, stride, &xrgb).unwrap();
    assert!(encoded.len() < xrgb.len());
    let (w, h, rgba) = qoi::decode(&encoded).unwrap();
    assert_eq!((w, h), (width, height));
    for y in 0..11 {
        for x in 0..37 {
            let at = y * stride + x * 4;
            let [b, g, r, _] = <[u8; 4]>::try_from(&xrgb[at..at + 4]).unwrap();
            let p = (y * 37 + x) * 4;
            assert_eq!(&rgba[p..p + 4], &[r, g, b, 255], "pixel {x},{y}");
        }
    }
    assert!(qoi::encode(0, 1, 0, &[]).is_none());
    assert!(qoi::encode(4, 4, 16, &[0; 10]).is_none());
}
