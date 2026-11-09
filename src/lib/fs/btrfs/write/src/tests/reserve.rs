//! The reservation a mount hands the volume (W1-b, `crate::reserve`): kept
//! across an abort and a reload read-only, which shrinks it; dropped with
//! the volume. And its admission (W1-c): every operation and commit asks it
//! once, before its first change, and a refusal answers `OutOfMemory` with
//! the transaction exactly as it was.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_btrfs::items::{FS_TREE_OBJECTID, S_IFDIR, S_IFLNK, S_IFREG, Timespec};
use ferrix_btrfs::tree::BtrfsKey;

use super::{BLANK, MemDevice};
use crate::{Error, NewInode, Reserve, WriteVolume};

const ROOT: u64 = 256;
const NOW: Timespec = Timespec {
    sec: 1_790_000_000,
    nsec: 9,
};

fn file() -> NewInode {
    NewInode {
        mode: S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        rdev: 0,
        now: NOW,
    }
}

/// What a recording reserve was asked.
#[derive(Debug, Default)]
pub(super) struct Asked {
    pub(super) shrinks: AtomicU64,
    pub(super) drops: AtomicU64,
    /// Growths asked for, granted or not.
    pub(super) grows: AtomicU64,
    /// Operations run inside it.
    pub(super) entered: AtomicU64,
    /// Whether it refuses every growth.
    pub(super) refuse: AtomicBool,
}

/// A reserve that grants every growth, unless told to refuse, and counts
/// what it is asked.
#[derive(Debug)]
pub(super) struct Recording(pub(super) Arc<Asked>);

impl Reserve for Recording {
    fn grow_to(&self, _frames: u64) -> bool {
        let _ = self.0.grows.fetch_add(1, Ordering::Relaxed);
        !self.0.refuse.load(Ordering::Relaxed)
    }

    fn drawn(&self) -> u64 {
        0
    }

    fn most(&self) -> u64 {
        u64::MAX
    }

    fn within(&self, body: &mut dyn FnMut()) {
        let _ = self.0.entered.fetch_add(1, Ordering::Relaxed);
        body();
    }

    fn shrink(&self) {
        let _ = self.0.shrinks.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.0.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// A volume with a recording reserve, and what it records.
fn reserved() -> (WriteVolume<MemDevice>, Arc<Asked>) {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let asked = Arc::new(Asked::default());
    volume.set_reserve(Box::new(Recording(Arc::clone(&asked))));
    (volume, asked)
}

#[test]
fn a_reload_read_only_keeps_the_reserve_and_shrinks_it() {
    let (mut volume, asked) = reserved();
    let _ = volume.create(ROOT, b"a", &file()).unwrap();
    volume.abort_with(Error::OutOfMemory);
    let shrinks = asked.shrinks.load(Ordering::Relaxed);
    volume.reload_read_only().unwrap();
    assert!(volume.is_read_only());
    assert!(
        volume.is_reserved(),
        "the reload dropped the mount's reserve"
    );
    assert_eq!(asked.drops.load(Ordering::Relaxed), 0);
    assert_eq!(
        asked.shrinks.load(Ordering::Relaxed),
        shrinks + 1,
        "the reload did not shrink the reserve"
    );
    drop(volume);
    assert_eq!(
        asked.drops.load(Ordering::Relaxed),
        1,
        "the reserve outlived its volume"
    );
}

#[test]
fn an_abort_keeps_the_reserve() {
    let (mut volume, asked) = reserved();
    let _ = volume.create(ROOT, b"a", &file()).unwrap();
    let volume = volume.abort().unwrap();
    assert!(
        volume.is_reserved(),
        "the reopened volume lost the mount's reserve"
    );
    assert_eq!(asked.drops.load(Ordering::Relaxed), 0);
    drop(volume);
    assert_eq!(asked.drops.load(Ordering::Relaxed), 1);
}

#[test]
fn a_volume_with_no_reserve_is_as_before() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    assert!(!volume.is_reserved());
    let _ = volume.create(ROOT, b"a", &file()).unwrap();
    volume.commit().unwrap();
}

fn node(mode: u32) -> NewInode {
    NewInode {
        mode,
        uid: 0,
        gid: 0,
        rdev: 0,
        now: NOW,
    }
}

/// Everything an operation may change in the running transaction: the
/// trees' nodes, the delayed references, the roots, the space, the count
/// of edits, the transaction and whether it aborted.
fn transaction(volume: &WriteVolume<MemDevice>) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}|{}|{}|{}",
        volume.dirty,
        volume.refs,
        volume.roots,
        volume.stale_roots,
        volume.space,
        volume.edits,
        volume.transid,
        volume.chunks_changed,
        volume.aborted,
        volume.has_log()
    )
}

/// One operation of each kind the write path has, by name, on a volume
/// holding the inodes `prepare` made.
type Operation = fn(&mut WriteVolume<MemDevice>, &Made) -> crate::Result<()>;

/// What `prepare` made for the operations to act on.
struct Made {
    dir: u64,
    file: u64,
    link: u64,
    orphan: u64,
}

/// A volume with a recording reserve, holding a directory, a file with
/// data, a symlink and an orphan, committed, then a name made in the next
/// transaction so that a commit has work.
fn prepare() -> (WriteVolume<MemDevice>, Arc<Asked>, Made) {
    let (mut volume, asked) = reserved();
    let dir = volume.create(ROOT, b"dir", &node(S_IFDIR | 0o755)).unwrap();
    let regular = volume.create(dir, b"file", &file()).unwrap();
    let data = alloc::vec![0x5au8; 3 * 4096];
    volume
        .write_file(regular, 0, &data, data.len() as u64)
        .unwrap();
    let link = volume
        .create(ROOT, b"link", &node(S_IFLNK | 0o777))
        .unwrap();
    volume.set_symlink(link, b"dir/file").unwrap();
    let orphan = volume.create(ROOT, b"orphan", &file()).unwrap();
    let _ = volume.unlink(ROOT, b"orphan", NOW).unwrap();
    volume.commit().unwrap();
    let _ = volume.create(ROOT, b"pending", &file()).unwrap();
    (
        volume,
        asked,
        Made {
            dir,
            file: regular,
            link,
            orphan,
        },
    )
}

const OPERATIONS: [(&str, Operation); 17] = [
    ("create", |v, m| v.create(m.dir, b"new", &file()).map(drop)),
    ("link", |v, m| v.link(ROOT, b"again", m.file, NOW)),
    ("unlink", |v, m| v.unlink(m.dir, b"file", NOW).map(drop)),
    ("rename", |v, m| {
        v.rename(m.dir, b"file", ROOT, b"moved", NOW)
    }),
    ("evict", |v, m| v.evict(m.orphan)),
    ("set_symlink", |v, m| v.set_symlink(m.link, b"elsewhere")),
    ("drop_extents", |v, m| v.drop_extents(m.file, 0, 4096)),
    ("write_file", |v, m| {
        v.write_file(m.file, 4096, &[7u8; 8192], 3 * 4096)
    }),
    ("truncate", |v, m| v.truncate(m.file, 100)),
    ("insert", |v, _| {
        v.insert(
            FS_TREE_OBJECTID,
            BtrfsKey::new(900_000, 24, 0),
            alloc::vec![1u8; 300],
        )
    }),
    ("put", |v, _| {
        v.put(
            FS_TREE_OBJECTID,
            BtrfsKey::new(900_001, 24, 0),
            alloc::vec![2u8; 300],
        )
    }),
    ("update", |v, m| {
        let item = v.inode(m.file)?.ok_or(Error::NotFound)?;
        v.write_inode(m.file, &item)
    }),
    ("delete", |v, m| {
        v.delete(FS_TREE_OBJECTID, &BtrfsKey::new(m.link, 1, 0))
    }),
    ("alloc_data", |v, _| v.alloc_data(4096, 4096).map(drop)),
    ("log_inode", |v, m| v.log_inode(m.file)),
    ("commit", |v, _| v.commit()),
    // The log is made before, granted (see the test); only its commit is
    // asked here.
    ("commit_log", |v, _| v.commit_log()),
];

/// Verifies: L.btrfs.124
#[test]
fn a_refused_admission_answers_out_of_memory_and_changes_nothing() {
    for (name, operation) in OPERATIONS {
        let (mut volume, asked, made) = prepare();
        if name == "commit_log" {
            volume.log_inode(made.file).unwrap();
        }
        let before = transaction(&volume);
        let grows = asked.grows.load(Ordering::Relaxed);
        let entered = asked.entered.load(Ordering::Relaxed);
        asked.refuse.store(true, Ordering::Relaxed);
        assert_eq!(
            operation(&mut volume, &made),
            Err(Error::OutOfMemory),
            "{name}: a refused admission did not answer OutOfMemory"
        );
        assert_eq!(
            asked.grows.load(Ordering::Relaxed),
            grows + 1,
            "{name}: the reservation was not asked once"
        );
        assert_eq!(
            asked.entered.load(Ordering::Relaxed),
            entered,
            "{name}: a refused operation ran inside the reservation"
        );
        assert_eq!(
            volume.aborted(),
            None,
            "{name}: a refused admission aborted"
        );
        assert_eq!(
            transaction(&volume),
            before,
            "{name}: a refused admission changed the transaction"
        );
        // Granted, the same operation runs, once inside the reservation.
        asked.refuse.store(false, Ordering::Relaxed);
        operation(&mut volume, &made).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(
            asked.entered.load(Ordering::Relaxed),
            entered + 1,
            "{name}: the operation and its nested ones did not run inside one admission"
        );
        assert_eq!(volume.aborted(), None, "{name}");
        volume.commit().unwrap();
    }
}

#[test]
fn an_operation_and_the_ones_it_nests_are_admitted_once() {
    let (mut volume, asked, made) = prepare();
    let grows = asked.grows.load(Ordering::Relaxed);
    let shrinks = asked.shrinks.load(Ordering::Relaxed);
    // A write of several extents: each extent's allocation and every edit
    // it makes are nested in the write's own admission.
    let data = alloc::vec![9u8; 300 * 1024];
    volume
        .write_file(made.file, 0, &data, data.len() as u64)
        .unwrap();
    assert_eq!(asked.grows.load(Ordering::Relaxed), grows + 1);
    // And it promises nothing once it is done.
    assert_eq!(asked.shrinks.load(Ordering::Relaxed), shrinks + 1);
}

#[test]
fn a_commit_with_nothing_to_write_asks_nothing() {
    let (mut volume, asked, _) = prepare();
    volume.commit().unwrap();
    let grows = asked.grows.load(Ordering::Relaxed);
    asked.refuse.store(true, Ordering::Relaxed);
    volume.commit().unwrap();
    assert_eq!(asked.grows.load(Ordering::Relaxed), grows);
}
