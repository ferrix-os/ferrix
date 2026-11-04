//! Host tests: L.object.330's refusals, and one volume per channel.

extern crate std;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::{FileSystem, FileType};

use crate::server::{Slot, Volume};
use crate::wire::{
    ANSWER_BYTES, Answer, DiskUp, FIRST_NODE, MAX_NODES, Malformed, Op, REQUEST_HEADER,
    REQUEST_MAX, Request, SET_ATTR_BYTES, SLOT_BYTES, decode_dirent, encode_set_attr,
};

struct Memory(Vec<u8>);

impl Slot for Memory {
    fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        match offset
            .checked_add(out.len())
            .and_then(|end| self.0.get(offset..end))
        {
            Some(bytes) => {
                out.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> bool {
        match offset
            .checked_add(bytes.len())
            .and_then(|end| self.0.get_mut(offset..end))
        {
            Some(place) => {
                place.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

#[derive(Debug)]
struct Still;

impl ferrix_vfs::Clock for Still {
    fn now(&self) -> ferrix_vfs::Timespec {
        ferrix_vfs::Timespec::default()
    }
}

fn tmpfs() -> Arc<dyn FileSystem> {
    ferrix_vfs::tmpfs::Tmpfs::new(
        1,
        Arc::new(Still),
        Arc::new(ferrix_vfs::tmpfs::HeapStorage::new(1 << 30)),
        0o755,
    )
    .expect("a tmpfs")
}

fn slot() -> Memory {
    Memory(vec![0; SLOT_BYTES])
}

fn ask(volume: &mut Volume, request: Request<'_>, slot: &mut Memory) -> Answer {
    let mut bytes = [0; REQUEST_MAX];
    let length = request.encode(&mut bytes).expect("it fits");
    volume.serve(&bytes[..length], slot)
}

fn made(volume: &mut Volume, name: &'static [u8], mode: u32, slot: &mut Memory) -> u32 {
    let answer = ask(
        volume,
        Request {
            a: 0o644,
            c: mode,
            name,
            ..Request::new(Op::Create, FIRST_NODE)
        },
        slot,
    );
    assert_eq!(answer.status, 0, "{name:?}");
    answer.node
}

const REGULAR: u32 = FileType::Regular.mode_bits();
const DIRECTORY: u32 = FileType::Directory.mode_bits();

#[test]
fn a_file_is_made_written_read_and_listed() {
    let mut volume = Volume::new(tmpfs());
    let mut slot = slot();
    let file = made(&mut volume, b"hello", REGULAR, &mut slot);
    assert!(slot.write(0, b"bytes"));
    let wrote = ask(
        &mut volume,
        Request {
            b: 5,
            ..Request::new(Op::Write, file)
        },
        &mut slot,
    );
    assert_eq!((wrote.status, wrote.value), (0, 5));
    assert_eq!(wrote.attr.map(|attr| attr.size), Some(5));
    slot.0.fill(0);
    let read = ask(
        &mut volume,
        Request {
            b: 64,
            ..Request::new(Op::Read, file)
        },
        &mut slot,
    );
    assert_eq!(read.value, 5);
    assert_eq!(&slot.0[..5], b"bytes");
    let list = ask(
        &mut volume,
        Request {
            a: ferrix_vfs::FIRST_CURSOR,
            ..Request::new(Op::ReadDir, FIRST_NODE)
        },
        &mut slot,
    );
    let (_, _, kind, name, took) = decode_dirent(&slot.0[..list.value as usize]).expect("an entry");
    assert_eq!((kind, name, took), (FileType::Regular, &b"hello"[..], 25));
}

#[test]
fn every_length_is_compared() {
    let mut bytes = [0; REQUEST_MAX];
    let length = Request {
        name: b"name",
        ..Request::new(Op::Lookup, 1)
    }
    .encode(&mut bytes)
    .expect("it fits");
    assert!(Request::decode(&bytes[..length]).is_ok());
    assert_eq!(
        Request::decode(&bytes[..length - 1]),
        Err(Malformed::Length)
    );
    assert_eq!(
        Request::decode(&bytes[..length + 1]),
        Err(Malformed::Length)
    );
    assert_eq!(
        Request::decode(&bytes[..REQUEST_HEADER - 1]),
        Err(Malformed::Length)
    );
    bytes[0] = 99;
    assert_eq!(Request::decode(&bytes[..length]), Err(Malformed::Op));
    bytes[0] = Op::Lookup as u8;
    bytes[32] = 0xFF;
    bytes[33] = 0xFF;
    assert_eq!(Request::decode(&bytes[..length]), Err(Malformed::Name));
}

#[test]
fn a_bad_name_is_refused() {
    let mut bytes = [0; REQUEST_MAX];
    for name in [&b"a/b"[..], b"a\0b", b".", b".."] {
        let length = Request {
            name,
            ..Request::new(Op::Lookup, 1)
        }
        .encode(&mut bytes)
        .expect("it fits");
        assert_eq!(
            Request::decode(&bytes[..length]),
            Err(Malformed::Name),
            "{name:?}"
        );
    }
    let mut volume = Volume::new(tmpfs());
    let mut memory = slot();
    let no_name = ask(
        &mut volume,
        Request::new(Op::Lookup, FIRST_NODE),
        &mut memory,
    );
    assert_eq!(no_name.status, u32::from(Errno::EINVAL.0));
}

#[test]
fn a_bad_node_a_length_past_the_slot_and_an_offset_overflow_are_refused() {
    let mut volume = Volume::new(tmpfs());
    let mut memory = slot();
    let file = made(&mut volume, b"f", REGULAR, &mut memory);
    let bad = u32::from(Errno::EBADF.0);
    for node in [0, 77, u32::MAX] {
        assert_eq!(
            ask(&mut volume, Request::new(Op::GetAttr, node), &mut memory).status,
            bad
        );
    }
    let past = ask(
        &mut volume,
        Request {
            b: SLOT_BYTES as u64 + 1,
            ..Request::new(Op::Read, file)
        },
        &mut memory,
    );
    assert_eq!(past.status, u32::from(Errno::EINVAL.0));
    // Whatever the filesystem says, and never a panic.
    let far = ask(
        &mut volume,
        Request {
            a: u64::MAX,
            b: 16,
            ..Request::new(Op::Write, file)
        },
        &mut memory,
    );
    assert_ne!(far.status, 0);
    let forgot = ask(
        &mut volume,
        Request::new(Op::Forget, FIRST_NODE),
        &mut memory,
    );
    assert_ne!(forgot.status, 0, "the root is the channel's");
    assert_eq!(
        ask(&mut volume, Request::new(Op::Forget, file), &mut memory).status,
        0
    );
    assert_eq!(
        ask(&mut volume, Request::new(Op::GetAttr, file), &mut memory).status,
        bad
    );
}

#[test]
fn one_channels_node_means_nothing_on_another() {
    let mut one = Volume::new(tmpfs());
    let mut other = Volume::new(tmpfs());
    let mut memory = slot();
    let file = made(&mut one, b"mine", REGULAR, &mut memory);
    let answer = ask(&mut other, Request::new(Op::GetAttr, file), &mut memory);
    assert_eq!(answer.status, u32::from(Errno::EBADF.0));
}

#[test]
fn the_table_has_a_bound_and_a_forgotten_slot_comes_back() {
    let mut volume = Volume::new(tmpfs());
    let mut memory = slot();
    let dir = made(&mut volume, b"d", DIRECTORY, &mut memory);
    let mut last = 0;
    for _ in 0..MAX_NODES {
        let looked = ask(
            &mut volume,
            Request {
                name: b"d",
                ..Request::new(Op::Lookup, FIRST_NODE)
            },
            &mut memory,
        );
        if looked.status != 0 {
            assert_eq!(looked.status, u32::from(Errno::ENFILE.0));
            assert_eq!(
                ask(&mut volume, Request::new(Op::Forget, last), &mut memory).status,
                0
            );
            let again = ask(
                &mut volume,
                Request {
                    name: b"d",
                    ..Request::new(Op::Lookup, FIRST_NODE)
                },
                &mut memory,
            );
            assert_eq!((again.status, again.node), (0, last));
            let _ = dir;
            return;
        }
        last = looked.node;
    }
    panic!("no bound");
}

#[test]
fn the_slot_is_read_once_into_this_sides_memory() {
    // A set of attributes whose bits are none of the form's: refused, not guessed.
    let mut volume = Volume::new(tmpfs());
    let mut memory = slot();
    memory.0[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    let answer = ask(
        &mut volume,
        Request::new(Op::SetAttr, FIRST_NODE),
        &mut memory,
    );
    assert_eq!(answer.status, u32::from(Errno::EINVAL.0));
    let good = encode_set_attr(&ferrix_vfs::SetAttributes {
        permissions: Some(0o700),
        ..Default::default()
    });
    assert_eq!(good.len(), SET_ATTR_BYTES);
    memory.0[..SET_ATTR_BYTES].copy_from_slice(&good);
    let answer = ask(
        &mut volume,
        Request::new(Op::SetAttr, FIRST_NODE),
        &mut memory,
    );
    assert_eq!(answer.attr.map(|attr| attr.permissions), Some(0o700));
}

#[test]
fn answers_and_events_go_through_their_forms() {
    let mut volume = Volume::new(tmpfs());
    let root = volume.root_answer();
    let bytes = root.encode();
    assert_eq!(bytes.len(), ANSWER_BYTES);
    assert_eq!(Answer::decode(&bytes), Some(root));
    assert_eq!(Answer::decode(&bytes[1..]), None);
    let up = DiskUp {
        name: *b"vda\0\0\0\0\0",
        sectors: 7,
        sector_size: 512,
        flags: 3,
        location: 0x18,
        max_sectors: 256,
    };
    assert_eq!(DiskUp::decode(&up.encode()), Some(up));
    assert_eq!(DiskUp::decode(&[0; 3]), None);
    let mut memory = slot();
    // Hostile bytes never panic.
    for seed in 0..512_u32 {
        let noise: Vec<u8> = (0..seed % 80)
            .map(|at| (seed.wrapping_mul(at + 31) >> 3) as u8)
            .collect();
        let _ = volume.serve(&noise, &mut memory);
    }
}
