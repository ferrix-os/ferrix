//! A volume that runs out: of data room, of room in its trees, or of device
//! space for a chunk in both copies. Each must answer [`Error::NoSpace`]
//! before an edit changes anything, so the transaction is not aborted, the
//! commit still goes through, and what frees space still can.

extern crate std;
use std::vec;

use ferrix_btrfs::items::{S_IFREG, Timespec};

use super::{BLANK, MemDevice, check};
use crate::ranges::RangeSet;
use crate::space::Kind;
use crate::{Error, NewInode, WriteVolume};

const ROOT: u64 = 256;
const NEW: NewInode = NewInode {
    mode: S_IFREG | 0o644,
    uid: 0,
    gid: 0,
    rdev: 0,
    now: Timespec { sec: 1, nsec: 0 },
};

/// The name of the `n`th file: long, so the trees fill in a few seconds.
fn name(n: u32) -> std::string::String {
    std::format!("f{n:0>200}")
}

/// Create files, committing every 64, until `stop` or the first refusal;
/// answers how many were made and the refusal.
fn fill_trees(volume: &mut WriteVolume<MemDevice>, stop: u32) -> (u32, Option<Error>) {
    let mut n = 0u32;
    while n < stop {
        if let Err(error) = volume.create(ROOT, name(n).as_bytes(), &NEW) {
            return (n, Some(error));
        }
        n += 1;
        if n.is_multiple_of(64) {
            volume
                .commit()
                .expect("a commit the admissions left room for");
        }
        assert!(n < 200_000, "the trees never filled");
    }
    (n, None)
}

/// Verifies: L.btrfs.20, H.STORE.6
#[test]
fn a_write_past_the_room_changes_nothing() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let ino = volume.create(ROOT, b"big", &NEW).unwrap();
    volume.commit().unwrap();
    let room = volume.data_room();
    let sector = u64::from(volume.sectorsize());
    let data = vec![7u8; usize::try_from(room + sector).unwrap()];
    assert_eq!(
        volume.write_file(ino, 0, &data, data.len() as u64),
        Err(Error::NoSpace)
    );
    assert_eq!(
        volume.aborted(),
        None,
        "a refused write aborted the transaction"
    );
    volume.commit().unwrap();
    check(&volume.device);
}

/// Verifies: L.btrfs.20, H.STORE.6
#[test]
fn a_write_of_exactly_the_room_fits() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let ino = volume.create(ROOT, b"big", &NEW).unwrap();
    volume.commit().unwrap();
    let data = vec![7u8; usize::try_from(volume.data_room()).unwrap()];
    volume
        .write_file(ino, 0, &data, data.len() as u64)
        .expect("the room data_room promised");
    assert_eq!(volume.data_room(), 0);
    volume.commit().unwrap();
    check(&volume.device);
}

/// Verifies: L.btrfs.20, H.STORE.6
#[test]
fn creates_on_full_trees_are_refused_and_a_deletion_still_makes_room() {
    // 22,738 creates on this fixture used to abort the transaction: the
    // last ran out of tree nodes half-way, and a commit after 25,216 did.
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let (made, refused) = fill_trees(&mut volume, u32::MAX);
    assert_eq!(refused, Some(Error::NoSpace), "after {made} files");
    assert_eq!(
        volume.aborted(),
        None,
        "a refused create aborted the transaction"
    );
    volume.commit().expect("the commit's room was kept back");
    check(&volume.device);
    // A deletion may use the reserve a create may not.
    for n in 0..64 {
        let _ = volume.unlink(ROOT, name(n).as_bytes(), NEW.now).unwrap();
    }
    for n in 0..64 {
        let ino = volume.lookup(ROOT, name(made - 1 - n).as_bytes()).unwrap();
        assert!(ino.is_some());
    }
    volume.commit().unwrap();
    let _ = volume
        .create(ROOT, b"again", &NEW)
        .expect("room a deletion made");
    volume.commit().unwrap();
    check(&volume.device);
}

/// Verifies: L.btrfs.20, H.STORE.6
#[test]
fn a_data_write_on_full_trees_aborts_nothing() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let ino = volume.create(ROOT, b"big", &NEW).unwrap();
    volume.commit().unwrap();
    let _ = fill_trees(&mut volume, u32::MAX);
    volume.commit().unwrap();
    let piece = vec![3u8; 64 * 1024];
    let mut at = 0u64;
    let refused = loop {
        match volume.write_file(ino, at, &piece, at + piece.len() as u64) {
            Ok(()) => at += piece.len() as u64,
            Err(error) => break error,
        }
    };
    assert_eq!(refused, Error::NoSpace);
    assert_eq!(volume.aborted(), None, "a data write on full trees aborted");
    volume.commit().unwrap();
    check(&volume.device);
}

/// Verifies: L.btrfs.20, H.STORE.6
#[test]
fn a_metadata_chunk_made_during_a_write_of_the_room_aborts_nothing() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let ino = volume.create(ROOT, b"big", &NEW).unwrap();
    let reserve = 64 * u64::from(volume.nodesize());
    // Use the trees until the next edit has to make a metadata chunk.
    let mut n = 0u32;
    while volume.space.free_bytes(Kind::Metadata) >= reserve {
        match volume.create(ROOT, name(n).as_bytes(), &NEW) {
            Ok(_) => n += 1,
            Err(error) => {
                assert_eq!(error, Error::NoSpace);
                break;
            }
        }
    }
    assert_eq!(volume.aborted(), None);
    let data = vec![7u8; usize::try_from(volume.data_room()).unwrap()];
    let written = volume.write_file(ino, 0, &data, data.len() as u64);
    assert!(
        matches!(written, Ok(()) | Err(Error::NoSpace)),
        "{written:?}"
    );
    assert_eq!(
        volume.aborted(),
        None,
        "the write the pre-check passed aborted"
    );
    volume.commit().unwrap();
    check(&volume.device);
}

/// Verifies: L.btrfs.19, L.btrfs.20, H.STORE.6
#[test]
fn a_data_extent_the_device_has_no_room_for_aborts_nothing() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let mut taken = 0u64;
    let refused = loop {
        match volume.alloc_data(1024 * 1024, 4096) {
            Ok((_, len)) => taken += len,
            Err(error) => break error,
        }
        assert!(taken <= 128 * 1024 * 1024);
    };
    assert_eq!(refused, Error::NoSpace);
    assert_eq!(volume.aborted(), None, "a data extent with no room aborted");
}

/// Verifies: L.btrfs.19
#[test]
fn every_copy_of_a_chunk_finds_a_stripe() {
    // One 8 MiB hole: a DUP chunk sized to the hole has no room for its
    // second copy, and two of 4 MiB fit.
    let mut holes = RangeSet::new();
    let _ = holes.insert(1024 * 1024, 8 * 1024 * 1024);
    assert_eq!(crate::grow::place_stripes(&holes, 8 * 1024 * 1024, 2), None);
    assert_eq!(
        crate::grow::place_stripes(&holes, 4 * 1024 * 1024, 2),
        Some(vec![1024 * 1024, 5 * 1024 * 1024])
    );
}
