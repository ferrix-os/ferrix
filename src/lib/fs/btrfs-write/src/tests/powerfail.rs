//! Power failures, hundreds of them, without a machine to switch off.
//!
//! A commit's promise is made of write order: everything the transaction
//! wrote is on the disk, then a flush, then the superblock. A log commit
//! promises the same about one file. Both promises are only as good as what
//! survives when the power goes at the worst moment, and the worst moment is
//! not one anybody picks by hand.
//!
//! So the device records what it was told ([`Op`]), and this rebuilds the
//! disk as it would have been after a cut at an arbitrary point:
//!
//! * every write before the last flush is on the disk — that is what a flush
//!   means;
//! * the writes after it, up to the cut, may or may not be: each one is kept
//!   or dropped at random, which is exactly the freedom the device has.
//!
//! Then the volume is opened, which replays a log if one is named, and two
//! things must hold. It must be consistent — [`super::check`], the whole of
//! it, down to free space and checksums. And **no completed promise may be
//! rolled back**: a file that was `fsync`ed, or committed, must never be
//! found holding something older than what that call promised. A file may
//! hold something newer, because a write after the last flush may well have
//! landed; it may never hold something older.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_btrfs::items::{S_IFREG, Timespec};

use super::{BLANK, MemDevice, Op, Rng, check};
use crate::{NewInode, WriteVolume};

const ROOT: u64 = 256;
const NOW: Timespec = Timespec {
    sec: 1_790_000_000,
    nsec: 5,
};

/// How many files the scenario keeps, and their names.
const NAMES: [&[u8]; 4] = [b"alpha", b"beta", b"gamma", b"delta"];

/// What a file held when a promise about it completed.
#[derive(Debug, Clone)]
struct Promise {
    /// Where in the device's log the promise completed: after this index,
    /// the contents below are on the disk for good.
    at: usize,
    ino: u64,
    contents: Vec<u8>,
}

/// A run of the scenario: the disk it started from, everything the device
/// was told after that, and every promise made along the way.
struct Run {
    /// The volume as it stood when the recording began — the files exist and
    /// are committed — which is what every rebuilt crash starts from.
    base: MemDevice,
    log: Vec<Op>,
    promises: Vec<Promise>,
    inodes: Vec<u64>,
}

/// `len` bytes of file `seed`.
fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed | 1);
    (0..len).map(|_| rng.next() as u8).collect()
}

/// Write files and make them durable, sometimes with `fsync` and sometimes
/// with a commit, recording every promise and everything the device was
/// told.
fn run_scenario(seed: u64, steps: usize) -> Run {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let new = NewInode {
        mode: S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        rdev: 0,
        now: NOW,
    };
    let mut inodes = Vec::new();
    for name in NAMES {
        inodes.push(volume.create(ROOT, name, &new).unwrap());
    }
    volume.commit().unwrap();
    // The recording starts after the files exist: every promise below is
    // about contents, and the names are already on the disk.
    let base = volume.device.clone();
    volume.device.log.clear();
    let mut rng = Rng::new(seed);
    let mut promises = Vec::new();
    let mut held: BTreeMap<u64, Vec<u8>> = inodes.iter().map(|&ino| (ino, Vec::new())).collect();
    for step in 0..steps {
        let ino = inodes[(rng.below(inodes.len() as u64)) as usize];
        let len = match rng.below(4) {
            0 => rng.below(2000) as usize,
            1 => rng.below(20_000) as usize + 4096,
            _ => rng.below(200_000) as usize + 20_000,
        };
        let data = pattern(len, seed.wrapping_mul(31).wrapping_add(step as u64));
        volume.truncate(ino, 0).unwrap();
        if !data.is_empty() {
            volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
        }
        let _ = held.insert(ino, data.clone());
        // Half the promises are an fsync, half a commit; an fsync after
        // nothing structural is a log commit, which is the interesting one.
        if rng.below(2) == 0 {
            volume.log_inode(ino).unwrap();
            volume.commit_log().unwrap();
            promises.push(Promise {
                at: volume.device.log.len(),
                ino,
                contents: data,
            });
        } else {
            volume.commit().unwrap();
            // A commit promises everything, so every file's contents are on
            // the disk from here.
            for (&ino, contents) in &held {
                promises.push(Promise {
                    at: volume.device.log.len(),
                    ino,
                    contents: contents.clone(),
                });
            }
        }
    }
    Run {
        base,
        log: volume.into_device().log,
        promises,
        inodes,
    }
}

/// The disk as a cut at `crash` would have left it: everything before the
/// last flush, and a random part of what came after.
fn crashed(run: &Run, crash: usize, rng: &mut Rng) -> MemDevice {
    let mut device = run.base.clone();
    device.log.clear();
    let last_flush = run
        .log
        .get(..crash)
        .unwrap_or_default()
        .iter()
        .rposition(|op| matches!(op, Op::Flush))
        .map_or(0, |at| at + 1);
    for (index, op) in run.log.iter().enumerate().take(crash) {
        let Op::Write(at, data) = op else { continue };
        // Before the last flush it is certainly there; after it, it is
        // anyone's guess, which is what makes this a power failure and not a
        // clean shutdown.
        if index < last_flush || rng.below(2) == 0 {
            device.store(*at, data);
        }
    }
    device
}

/// Read a whole file back.
fn read_all(volume: &mut WriteVolume<MemDevice>, ino: u64) -> Vec<u8> {
    let size = volume.inode(ino).unwrap().unwrap().size as usize;
    let mut out = vec![0u8; size];
    let read = volume.read_file(ino, 0, &mut out).unwrap();
    out.truncate(read);
    out
}

/// Check one crash point: the volume must open, be consistent, and hold
/// nothing older than the last completed promise about each file.
fn check_recovery(run: &Run, crash: usize, rng: &mut Rng) {
    let device = crashed(run, crash, rng);
    // Opening replays a log if the superblock names one.
    let mut volume = match WriteVolume::open(device) {
        Ok(volume) => volume,
        Err(error) => panic!("a crash at {crash} left a volume that will not open: {error}"),
    };
    volume
        .commit()
        .unwrap_or_else(|e| panic!("a crash at {crash} left a volume that will not commit: {e}"));
    check(&volume.device);
    for &ino in &run.inodes {
        let found = read_all(&mut volume, ino);
        // What this file was promised to hold, from the last promise that
        // completed before the cut onwards. Holding a later one is allowed:
        // a write after the last flush may have landed.
        let allowed: Vec<&Vec<u8>> = run
            .promises
            .iter()
            .filter(|promise| promise.ino == ino)
            .skip_while(|promise| promise.at <= crash)
            .map(|promise| &promise.contents)
            .collect();
        let last_completed = run
            .promises
            .iter()
            .rfind(|promise| promise.ino == ino && promise.at <= crash);
        let Some(promised) = last_completed else {
            continue;
        };
        let acceptable = found == promised.contents || allowed.iter().any(|later| **later == found);
        assert!(
            acceptable,
            "a crash at {crash} rolled back a completed promise about inode {ino}: \
             it holds {} bytes, and was promised {} or something later",
            found.len(),
            promised.contents.len()
        );
    }
}

/// One scenario, cut at many points: the test the stage's exit asks for,
/// without a machine to switch off.
fn power_fail(seed: u64, steps: usize, cuts: usize) {
    let run = run_scenario(seed, steps);
    assert!(run.log.len() > 32, "the scenario wrote something");
    let mut rng = Rng::new(seed ^ 0x5eed);
    for cut in 0..cuts {
        // Spread the cuts over the whole log, and land some of them exactly
        // on a superblock write, which is the moment that matters most.
        let crash = if cut % 4 == 0 {
            let supers: Vec<usize> = run
                .log
                .iter()
                .enumerate()
                .filter(|(_, op)| {
                    matches!(op, Op::Write(at, _) if ferrix_btrfs::superblock::SUPERBLOCK_OFFSETS.contains(at))
                })
                .map(|(index, _)| index)
                .collect();
            supers
                .get(rng.below(supers.len().max(1) as u64) as usize)
                .copied()
                .unwrap_or(run.log.len() / 2)
        } else {
            rng.below(run.log.len() as u64) as usize
        };
        check_recovery(&run, crash, &mut rng);
    }
}

/// Verifies: L.btrfs.14, H.STORE.4
#[test]
fn a_cut_anywhere_leaves_a_volume_that_mounts_and_holds_its_promises() {
    for seed in 1..=4 {
        power_fail(seed, 12, 25);
    }
}

#[test]
#[ignore = "hundreds of seeds; run by tools/common/test/btrfs-check-writer.sh and by hand"]
fn hundreds_of_cuts() {
    for seed in 1..=200 {
        power_fail(seed, 20, 25);
    }
}
