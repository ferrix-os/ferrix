//! Every message and op through its bytes and back, and the records that
//! must be refused.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::*;

fn rect() -> Rect {
    Rect::new(-3, 7, 640, 480)
}

fn rounding() -> Rounding {
    Rounding {
        radius: 12,
        power: 2.0,
    }
}

fn blur() -> Blur {
    Blur {
        size: 8,
        passes: 1,
        noise: 0.0117,
        contrast: 0.8916,
        brightness: 0.8172,
        vibrancy: 0.1696,
        vibrancy_darkness: 0.0,
    }
}

fn gradient() -> Gradient {
    Gradient {
        degrees: 45,
        colors: vec![0xff33_ccff, 0xff00_ff99],
    }
}

fn every_op() -> Vec<Op> {
    vec![
        Op::Damage(vec![rect(), Rect::new(0, 0, 1, 1)]),
        Op::Clear(0xff00_0000),
        Op::Fill(rect(), 0x8011_2233),
        Op::FillRounded(rect(), rounding(), 0xffff_ffff),
        Op::FillRoundedGradient(rect(), rounding(), gradient()),
        Op::BorderGradient(rect(), 2, gradient()),
        Op::Shadow(
            rect(),
            Shadow {
                rounding: rounding(),
                range: 20,
                power: 3,
                color: 0xee1a_1a1a,
                offset: (0, 4),
            },
        ),
        Op::Blur(rect(), rounding(), blur()),
        Op::Composite {
            texture: 0x1_0000_0001,
            src: Rect::new(0, 0, 640, 480),
            rect: rect(),
        },
        Op::CompositeScaled {
            texture: 9,
            src: Rect::new(10, 10, 100, 100),
            rect: rect(),
            rounding: rounding(),
            opacity: 0.5,
            nearest: true,
        },
        Op::KeepBackdrop,
        Op::BlurBackdrop(rect(), rounding(), blur()),
        Op::Upload {
            texture: 9,
            rect: Rect::new(0, 0, 100, 100),
            arena: 1,
            offset: 4096,
            stride: 400,
        },
    ]
}

fn every_message() -> Vec<Message> {
    let mut ops = Vec::new();
    for op in every_op() {
        assert!(op.encode(&mut ops).is_ok());
    }
    vec![
        Message::Hello { version: VERSION },
        Message::Ready {
            version: VERSION,
            caps: CAP_SYNC_FILE | CAP_HOST_MEMORY,
            device: String::from("Intel(R) Graphics (ADL-N)"),
            driver: String::from("Intel open-source Mesa driver"),
        },
        Message::Modifiers {
            fourcc: ARGB8888,
            modifiers: vec![(0, false), (0x0100_0000_0000_0001, true)],
        },
        Message::Configure {
            canvas: 1,
            width: 1920,
            height: 1080,
            outputs: 1,
        },
        Message::Outputs {
            canvas: 1,
            width: 1920,
            height: 1080,
            fourcc: XRGB8888,
            modifier: 0,
            planes: vec![
                Plane {
                    stride: 7680,
                    offset: 0,
                },
                Plane {
                    stride: 7680,
                    offset: 0,
                },
            ],
        },
        Message::Drop { canvas: 1 },
        Message::Arena {
            arena: 1,
            size: 32 << 20,
        },
        Message::ImportDmabuf {
            texture: 77,
            width: 1920,
            height: 1080,
            fourcc: ARGB8888,
            modifier: 0,
            planes: vec![Plane {
                stride: 7680,
                offset: 0,
            }],
        },
        Message::CreateTexture {
            texture: 9,
            width: 100,
            height: 100,
            fourcc: XRGB8888,
        },
        Message::Forget { texture: 77 },
        Message::FrameBegin {
            canvas: 1,
            frame: 42,
            output: 0,
        },
        Message::Ops(ops),
        Message::FrameEnd {
            frame: 42,
            damage: vec![rect()],
        },
        Message::Done {
            canvas: 1,
            frame: 42,
            output: 0,
            flags: 0,
        },
        Message::Done {
            canvas: 1,
            frame: 43,
            output: 0,
            flags: DONE_SYNC_FILE,
        },
        Message::Failed {
            canvas: 1,
            frame: 44,
            code: failure::DEVICE_LOST,
            why: String::from("VK_ERROR_DEVICE_LOST"),
        },
        Message::Release {
            canvas: 1,
            output: 0,
        },
        Message::Read {
            canvas: 1,
            rect: rect(),
            arena: 1,
            offset: 0,
            stride: 2560,
        },
        Message::ReadDone {
            canvas: 1,
            serial: 5,
        },
    ]
}

#[test]
fn every_message_round_trips() {
    for (serial, message) in (1u32..).zip(every_message()) {
        let record = message.encode(serial);
        assert!(record.is_ok(), "{message:?}");
        let record = record.unwrap_or_default();
        assert!(record.len() <= MAX_RECORD);
        let (header, back) = Message::decode(&record).unwrap_or_else(|error| {
            panic!("{message:?}: {error}");
        });
        assert_eq!(back, message);
        assert_eq!(header.kind, message.kind());
        assert_eq!(header.serial, serial);
        assert_eq!(usize::from(header.fds), message.fds());
        assert_eq!(header.len as usize, record.len() - HEADER);
    }
}

#[test]
fn every_op_round_trips() {
    let ops = every_op();
    let mut bytes = Vec::new();
    for op in &ops {
        assert!(op.encode(&mut bytes).is_ok());
        assert_eq!(bytes.len() % 4, 0, "{op:?} leaves the next op unaligned");
    }
    assert_eq!(Op::decode_all(&bytes), Ok(ops));
}

#[test]
fn descriptors_are_counted() {
    let fds: Vec<usize> = every_message().iter().map(Message::fds).collect();
    // Outputs with two, Arena, ImportDmabuf with one, the second Done.
    assert_eq!(fds.iter().sum::<usize>(), 2 + 1 + 1 + 1);
}

#[test]
fn a_record_with_the_wrong_descriptor_count_is_refused() {
    let message = Message::Arena { arena: 1, size: 64 };
    let mut record = message.encode(1).unwrap_or_default();
    if let Some(field) = record.get_mut(6..8) {
        field.copy_from_slice(&0u16.to_le_bytes());
    }
    assert_eq!(Message::decode(&record), Err(Error::Fds));
}

#[test]
fn short_long_and_foreign_records_are_refused() {
    let record = Message::Hello { version: VERSION }
        .encode(1)
        .unwrap_or_default();
    // Every truncation, header included.
    for len in 0..record.len() {
        assert!(Message::decode(record.get(..len).unwrap_or_default()).is_err());
    }
    // A byte too many, with the header's length fixed to match.
    let mut long = record.clone();
    long.push(0);
    if let Some(field) = long.get_mut(8..12) {
        field.copy_from_slice(&5u32.to_le_bytes());
    }
    assert_eq!(Message::decode(&long), Err(Error::Long));
    // Not ours.
    let mut foreign = record.clone();
    if let Some(byte) = foreign.first_mut() {
        *byte = b'X';
    }
    assert_eq!(Message::decode(&foreign), Err(Error::Magic));
    // A kind from a later version.
    let mut later = record;
    if let Some(field) = later.get_mut(4..6) {
        field.copy_from_slice(&999u16.to_le_bytes());
    }
    assert_eq!(Message::decode(&later), Err(Error::Kind(999)));
}

#[test]
fn bad_ops_are_refused() {
    assert_eq!(Op::decode_all(&[200, 0, 0, 0]), Err(Error::Tag(200)));
    assert_eq!(Op::decode_all(&[11, 1, 0, 0]), Err(Error::Value));
    assert_eq!(Op::decode_all(&[2, 0, 0, 0, 1]), Err(Error::Short));
    // A gradient longer than any.
    let mut bytes = vec![5, 0, 0, 0];
    put_rect(&mut bytes, rect());
    put_rounding(&mut bytes, rounding());
    put_i32(&mut bytes, 0);
    put_u32(&mut bytes, u32::MAX);
    assert_eq!(Op::decode_all(&bytes), Err(Error::TooBig));
}

#[test]
fn limits_are_kept_when_writing() {
    let damage = Op::Damage(vec![rect(); MAX_RECTS + 1]);
    assert_eq!(damage.encode(&mut Vec::new()), Err(Error::TooBig));
    let ops = Message::Ops(vec![0; MAX_BODY + 4]);
    assert_eq!(ops.encode(1), Err(Error::TooBig));
    let why = Message::Failed {
        canvas: 0,
        frame: 0,
        code: 0,
        why: "x".repeat(MAX_STRING + 1),
    };
    assert_eq!(why.encode(1), Err(Error::TooBig));
}

#[test]
fn a_long_display_list_is_split_between_ops_and_joined_again() {
    let mut writer = OpsWriter::new();
    assert!(writer.is_empty());
    let mut sent = Vec::new();
    for i in 0..20_000u32 {
        let op = if i % 7 == 0 {
            Op::Damage(vec![Rect::new(0, 0, i, 1); 3])
        } else {
            Op::Fill(Rect::new(0, 0, i, 1), i)
        };
        assert!(writer.push(&op).is_ok());
        sent.push(op);
    }
    let bodies = writer.finish();
    assert!(writer.is_empty());
    assert!(bodies.len() > 1);
    let mut got = Vec::new();
    for body in bodies {
        let record = Message::Ops(body).encode(1);
        assert!(record.is_ok());
        let record = record.unwrap_or_default();
        assert!(record.len() <= MAX_RECORD);
        let Ok((_, Message::Ops(body))) = Message::decode(&record) else {
            panic!("an ops record did not decode");
        };
        got.extend(Op::decode_all(&body).unwrap_or_default());
    }
    assert_eq!(got, sent);
}
