//! Running out of memory, at every allocation an operation makes.
//!
//! The write path allocates through `crate::fallible`, which asks the
//! `ferrix_fallible` injection policy before every allocation. The policy
//! installed here fails the `n`th allocation this thread makes -- or every
//! one from the `n`th on -- so a test runs one script of file operations and
//! a commit once for each `n`, and requires of every run:
//!
//! * the failing operation answers [`Error::OutOfMemory`], never anything
//!   else and never a panic;
//! * if the transaction was not aborted, the failure changed nothing: the
//!   rest of the script, run again from the operation that failed, ends in
//!   exactly the volume an undisturbed run makes;
//! * if it was aborted, the volume reopens at the commit before the script
//!   -- never the script's own: nothing allocates once its superblock is
//!   down, so no error is ever reported for a commit on the disk -- and
//!   [`check`] finds it consistent.
//!
//! `ferrix_fallible`'s policy is process-wide but this one reads a
//! thread-local budget, so the other tests in the binary, running beside
//! these, never see a failure.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::items::{FS_TREE_OBJECTID, S_IFDIR, S_IFREG, Timespec};
use ferrix_btrfs::tree::BtrfsKey;
use ferrix_btrfs::volume::{Device, ReadKind};
use ferrix_fallible::map_node_bound;

use super::{BLANK, MemDevice, POPULATED, check, items};
use crate::chunks::Chunk;
use crate::extent::Backref;
use crate::node::TreeNode;
use crate::ranges::RangeSet;
use crate::refs::Head;
use crate::space::BlockGroup;
use crate::volume::Root;
use crate::{Error, NewInode, WriteDevice, WriteVolume};

const ROOT: u64 = 256;

/// A tree's items, in order.
type Tree = Vec<(BtrfsKey, Vec<u8>)>;
const NOW: Timespec = Timespec {
    sec: 1_790_000_000,
    nsec: 5,
};

super::std::thread_local! {
    /// The allocation to fail, counting from one; `None` for none.
    static FAIL_AT: Cell<Option<u64>> = const { Cell::new(None) };
    /// Whether every allocation from [`FAIL_AT`] on fails, or that one only.
    static PERSIST: Cell<bool> = const { Cell::new(false) };
    /// Allocations asked about since [`arm`].
    static SEEN: Cell<u64> = const { Cell::new(0) };
}

/// The policy `ferrix_fallible` asks before each allocation.
fn policy() -> bool {
    FAIL_AT.with(|fail| {
        let Some(at) = fail.get() else {
            return false;
        };
        let seen = SEEN.with(|seen| {
            seen.set(seen.get() + 1);
            seen.get()
        });
        if PERSIST.with(Cell::get) {
            seen >= at
        } else {
            seen == at
        }
    })
}

/// Fail this thread's `at`th allocation from now, or with `persist` every
/// one from it on.
fn arm(at: u64, persist: bool) {
    let _ = ferrix_fallible::set_injector(policy);
    ferrix_fallible::arm(true);
    PERSIST.with(|p| p.set(persist));
    SEEN.with(|seen| seen.set(0));
    FAIL_AT.with(|fail| fail.set(Some(at)));
}

/// Stop failing, and say how many allocations were asked about.
fn disarm() -> u64 {
    FAIL_AT.with(|fail| fail.set(None));
    SEEN.with(Cell::get)
}

fn new(mode: u32) -> NewInode {
    NewInode {
        mode,
        uid: 1000,
        gid: 100,
        rdev: 0,
        now: NOW,
    }
}

/// Bytes different at every offset.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// The inode `name` in `dir` names.
fn find(volume: &mut WriteVolume<MemDevice>, dir: u64, name: &[u8]) -> crate::Result<u64> {
    Ok(volume.lookup(dir, name)?.ok_or(Error::NotFound)?.0)
}

/// What the script remembers between its steps.
#[derive(Default)]
struct Script {
    /// The inode of `small`, which is unlinked and then evicted.
    small: u64,
}

/// How many steps [`Script::step`] has.
const STEPS: usize = 12;

impl Script {
    /// Step `index` of a script that touches every kind of edit: a file
    /// written across several sectors, a directory, an inline file, a
    /// rename, an unlink and the eviction after it, a truncation, a file
    /// large enough to need a new data chunk, and the commit. One operation
    /// a step, so a step run again after a failure that changed nothing is
    /// the same operation, not a repeat of one that succeeded.
    fn step(&mut self, volume: &mut WriteVolume<MemDevice>, index: usize) -> crate::Result<()> {
        match index {
            0 => volume.create(ROOT, b"a", &new(S_IFREG | 0o644)).map(drop),
            1 => {
                let ino = find(volume, ROOT, b"a")?;
                let data = pattern(3 * 4096 + 100);
                volume.write_file(ino, 0, &data, data.len() as u64)
            }
            2 => volume.create(ROOT, b"d", &new(S_IFDIR | 0o755)).map(drop),
            3 => {
                self.small = volume.create(ROOT, b"small", &new(S_IFREG | 0o644))?;
                Ok(())
            }
            4 => volume.write_file(self.small, 0, &[7; 100], 100),
            5 => {
                let dir = find(volume, ROOT, b"d")?;
                volume.rename(ROOT, b"a", dir, b"b", NOW)
            }
            6 => volume.unlink(ROOT, b"small", NOW).map(drop),
            7 => volume.evict(self.small),
            8 => {
                let dir = find(volume, ROOT, b"d")?;
                let ino = find(volume, dir, b"b")?;
                volume.truncate(ino, 5000)
            }
            9 => {
                let dir = find(volume, ROOT, b"d")?;
                volume.create(dir, b"big", &new(S_IFREG | 0o600)).map(drop)
            }
            10 => {
                let dir = find(volume, ROOT, b"d")?;
                let big = find(volume, dir, b"big")?;
                let data = pattern(10 * 1024 * 1024);
                volume.write_file(big, 0, &data, data.len() as u64)
            }
            _ => volume.commit(),
        }
    }
}

/// A volume over `packed`, its generation and its fs tree.
fn fresh(packed: &[u8]) -> (WriteVolume<MemDevice>, u64, Tree) {
    let mut volume = WriteVolume::open(MemDevice::new(packed)).unwrap();
    let generation = volume.generation();
    let tree = items(&mut volume, FS_TREE_OBJECTID);
    (volume, generation, tree)
}

/// The script run undisturbed: the generation it commits and the fs tree it
/// leaves, and how many allocations it makes.
fn reference(packed: &[u8]) -> (u64, Tree, u64) {
    let (mut volume, _, _) = fresh(packed);
    let mut script = Script::default();
    arm(u64::MAX, false);
    for index in 0..STEPS {
        script.step(&mut volume, index).unwrap();
    }
    let made = disarm();
    let generation = volume.generation();
    (generation, items(&mut volume, FS_TREE_OBJECTID), made)
}

/// What became of one run of the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    /// No operation failed: `at` was past the script's last allocation, or
    /// what failed was only a cache insert.
    Undisturbed,
    /// An operation failed before it changed anything.
    Refused,
    /// An operation failed part-way, and the transaction was aborted.
    Aborted,
}

/// Run the script on `packed` with allocation `at` failing (and with
/// `persist`, every one after it), and check what it left.
fn run_failing_at(packed: &[u8], at: u64, persist: bool, after: &(u64, Tree)) -> Outcome {
    let (mut volume, before_generation, before) = fresh(packed);
    let mut script = Script::default();
    arm(at, persist);
    let mut failed = None;
    for index in 0..STEPS {
        if let Err(error) = script.step(&mut volume, index) {
            failed = Some((index, error));
            break;
        }
    }
    let _ = disarm();
    let Some((index, error)) = failed else {
        // Nothing failed, or what failed was the cache of clean nodes, which
        // goes without: the run is an undisturbed one.
        assert_eq!(volume.generation(), after.0, "allocation {at}");
        let tree = items(&mut volume, FS_TREE_OBJECTID);
        assert!(
            tree == after.1,
            "allocation {at}: a run with a cache miss differs"
        );
        check(&volume.into_device());
        return Outcome::Undisturbed;
    };
    assert_eq!(
        error,
        Error::OutOfMemory,
        "allocation {at} (persist {persist}) failed step {index} with {error:?}"
    );
    let outcome = if volume.aborted().is_some() {
        Outcome::Aborted
    } else {
        Outcome::Refused
    };
    let volume = if let Some(cause) = volume.aborted() {
        assert_eq!(cause, Error::OutOfMemory, "allocation {at}: abort cause");
        // The transaction is gone: what reopens is the last commit, whole.
        // Never the script's own: nothing allocates once its superblock is
        // down, so no failure can be reported for a commit on the disk.
        let mut volume = volume.abort().unwrap();
        assert_eq!(
            volume.generation(),
            before_generation,
            "allocation {at}: an error was reported for a commit that is on the disk"
        );
        let tree = items(&mut volume, FS_TREE_OBJECTID);
        assert!(tree == before, "allocation {at}: not the last commit");
        volume
    } else {
        // Nothing changed: the script goes on from the step that failed and
        // ends where an undisturbed run does.
        for index in index..STEPS {
            script
                .step(&mut volume, index)
                .unwrap_or_else(|e| panic!("allocation {at}: step {index} again: {e:?}"));
        }
        assert_eq!(volume.generation(), after.0, "allocation {at}");
        let tree = items(&mut volume, FS_TREE_OBJECTID);
        assert!(
            tree == after.1,
            "allocation {at}: a run retried from step {index} differs"
        );
        volume
    };
    check(&volume.into_device());
    outcome
}

/// Which allocations of `made` to fail: every one, and the one after the
/// last; past a few thousand, every one of the first thousand, where opening
/// and the first edits are, and then a spread over the rest that reaches the
/// last.
fn sample(made: u64) -> Vec<u64> {
    if made <= 5000 {
        return (1..=made + 1).collect();
    }
    let mut out: Vec<u64> = (1..=1000).collect();
    let mut at = 1000;
    while at < made {
        at += 37;
        out.push(at.min(made));
    }
    out.push(made + 1);
    out
}

#[test]
fn every_map_node_fits_the_kernels_reserve() {
    // The kernel serves a map insert's nodes from a reserve of size-class
    // objects, and refuses one whose nodes are larger than the largest
    // class: every map this crate keeps has to fit.
    let largest = ferrix_heap::LARGEST_CLASS;
    let bounds = [
        ("dirty and clean nodes", map_node_bound::<u64, TreeNode>()),
        ("roots", map_node_bound::<u64, Root>()),
        ("stale roots, taken slots", map_node_bound::<u64, ()>()),
        ("delayed ref heads", map_node_bound::<u64, Head>()),
        ("delayed ref deltas", map_node_bound::<Backref, i64>()),
        ("extent record refs", map_node_bound::<Backref, u64>()),
        ("block groups", map_node_bound::<u64, BlockGroup>()),
        ("range sets", map_node_bound::<u64, u64>()),
        ("chunks", map_node_bound::<u64, Chunk>()),
        ("logged ranges", map_node_bound::<u64, Vec<(u64, u64)>>()),
    ];
    for (what, bound) in bounds {
        assert!(bound <= largest, "{what}: {bound}-byte nodes");
    }
}

#[test]
fn reads_out_of_memory_change_nothing() {
    let (mut volume, _, before) = fresh(POPULATED);
    let generation = volume.generation();
    let mut at = 1;
    loop {
        // A volume of its own each time, so every read starts cold.
        let (mut cold, _, _) = fresh(POPULATED);
        arm(at, false);
        let read = cold.range(FS_TREE_OBJECTID, &BtrfsKey::MIN, &BtrfsKey::MAX);
        let _ = disarm();
        match read {
            Ok(tree) => {
                assert!(tree == before);
                break;
            }
            Err(error) => assert_eq!(error, Error::OutOfMemory, "allocation {at}"),
        }
        assert_eq!(cold.aborted(), None, "a read aborts nothing");
        assert!(items(&mut cold, FS_TREE_OBJECTID) == before);
        at += 1;
    }
    assert!(at > 10, "the read allocated too little to test");
    assert_eq!(volume.generation(), generation);
    assert!(items(&mut volume, FS_TREE_OBJECTID) == before);
}

#[test]
fn opening_out_of_memory_answers_out_of_memory() {
    for packed in [BLANK, POPULATED] {
        arm(u64::MAX, false);
        let _ = WriteVolume::open(MemDevice::new(packed)).unwrap();
        let made = disarm();
        let (_, generation, tree) = fresh(packed);
        for at in sample(made) {
            for persist in [false, true] {
                let disk = Shared(Rc::new(RefCell::new(MemDevice::new(packed))));
                arm(at, persist);
                let opened = WriteVolume::open(disk.clone());
                let _ = disarm();
                match opened {
                    // Past the last allocation, or one only the node cache
                    // missed.
                    Ok(_) => {}
                    Err(error) => assert_eq!(error, Error::OutOfMemory, "allocation {at}"),
                }
                // A failed open wrote nothing: the disk opens again and checks
                // clean, holding what an undisturbed open found.
                let mut volume = WriteVolume::open(disk.clone()).unwrap();
                assert_eq!(volume.generation(), generation, "allocation {at}");
                let reopened = items_shared(&mut volume);
                drop(volume);
                check(&disk.0.borrow());
                assert!(reopened == tree, "allocation {at}: the tree changed");
            }
        }
    }
}

/// The fs tree of a volume over a [`Shared`] disk.
fn items_shared(volume: &mut WriteVolume<Shared>) -> Tree {
    volume
        .range(FS_TREE_OBJECTID, &BtrfsKey::MIN, &BtrfsKey::MAX)
        .unwrap()
}

#[test]
fn a_transaction_out_of_memory_aborts_or_changes_nothing() {
    let (generation, tree, made) = reference(POPULATED);
    let after = (generation, tree);
    assert!(made > 300, "the script allocated only {made} times");
    let mut outcomes = BTreeMap::new();
    for at in sample(made) {
        *outcomes
            .entry(run_failing_at(POPULATED, at, false, &after))
            .or_insert(0) += 1;
    }
    // Both ways a failure can go are taken, many times over.
    for outcome in [Outcome::Refused, Outcome::Aborted] {
        let runs = outcomes.get(&outcome).copied().unwrap_or(0);
        assert!(runs > 50, "{outcome:?} only {runs} times: {outcomes:?}");
    }
}

#[test]
fn memory_that_stays_out_aborts_cleanly() {
    let (generation, tree, made) = reference(BLANK);
    let after = (generation, tree);
    for at in sample(made) {
        let _ = run_failing_at(BLANK, at, true, &after);
    }
}

/// A device whose bytes outlive the volume it was handed to: what a failed
/// open wrote is still there for the next one, as on a disk.
#[derive(Debug, Clone)]
struct Shared(Rc<RefCell<MemDevice>>);

impl Device for Shared {
    fn read_at(&mut self, physical: u64, buf: &mut [u8], kind: ReadKind) -> Result<(), BtrfsError> {
        self.0.borrow_mut().read_at(physical, buf, kind)
    }
}

impl WriteDevice for Shared {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> crate::Result<()> {
        self.0.borrow_mut().write_at(physical, data)
    }

    fn flush(&mut self) -> crate::Result<()> {
        self.0.borrow_mut().flush()
    }
}

#[test]
fn a_log_replay_out_of_memory_leaves_a_volume_that_replays_later() {
    // A file committed, written again, logged and made durable by a log
    // commit, and then the power goes: the next open replays the log.
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let ino = volume.create(ROOT, b"f", &new(S_IFREG | 0o644)).unwrap();
    volume.write_file(ino, 0, &[1; 4096], 4096).unwrap();
    volume.commit().unwrap();
    let data = pattern(5 * 4096);
    volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    let crashed = volume.into_device();

    arm(u64::MAX, false);
    let _ = WriteVolume::open(crashed.clone()).unwrap();
    let made = disarm();
    let mut failed = 0;
    for at in sample(made) {
        let disk = Shared(Rc::new(RefCell::new(crashed.clone())));
        arm(at, false);
        let opened = WriteVolume::open(disk.clone());
        let _ = disarm();
        match opened {
            // Past the last allocation, or one only the node cache missed.
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error, Error::OutOfMemory, "allocation {at}");
                failed += 1;
            }
        }
        // Whatever the failed replay left on the disk -- nothing, or a
        // replay committed before memory ran out -- opens, holds the file,
        // and checks clean.
        let mut volume = WriteVolume::open(disk.clone())
            .unwrap_or_else(|e| panic!("allocation {at}: reopening: {e:?}"));
        let mut back = alloc::vec![0u8; data.len()];
        let ino = find_shared(&mut volume, b"f");
        assert_eq!(volume.read_file(ino, 0, &mut back).unwrap(), data.len());
        assert!(back == data, "allocation {at}: the logged file");
        volume.commit().unwrap();
        drop(volume);
        check(&disk.0.borrow());
    }
    assert!(failed > 100);
}

/// The inode `name` in the root directory names.
fn find_shared(volume: &mut WriteVolume<Shared>, name: &[u8]) -> u64 {
    volume
        .lookup(ROOT, name)
        .unwrap()
        .unwrap_or_else(|| panic!("{} is gone", name.escape_ascii()))
        .0
}

/// `set` after `edit` runs with its first allocation failing: the error, and
/// whether the set is as it was.
fn edit_failing<T: core::fmt::Debug>(
    set: &RangeSet,
    edit: impl FnOnce(&mut RangeSet) -> crate::Result<T>,
) -> (crate::Result<T>, bool) {
    let mut edited = set.clone();
    arm(1, false);
    let result = edit(&mut edited);
    let _ = disarm();
    let unchanged = edited == *set;
    (result, unchanged)
}

/// One edit of a range set, answering whether it took.
type Edit = fn(&mut RangeSet) -> crate::Result<bool>;

#[test]
fn a_range_set_edit_out_of_memory_leaves_the_set_as_it_was() {
    let mut set = RangeSet::new();
    assert!(set.insert(100, 100));
    assert!(set.insert(400, 100));
    // Each edit below has to make a run of its own: one allocation, failed.
    let cases: [(&str, Edit); 6] = [
        ("a new run on its own", |s| s.try_insert(300, 10)),
        ("a new run joining the one after", |s| s.try_insert(350, 50)),
        ("a cut in the middle of a run", |s| s.try_remove(120, 10)),
        ("a cut at the start of a run", |s| s.try_remove(100, 10)),
        ("a union starting a new run", |s| {
            s.try_add(50, 100).map(|()| true)
        }),
        ("a union across two runs", |s| {
            s.try_add(90, 400).map(|()| true)
        }),
    ];
    for (what, edit) in cases {
        let (result, unchanged) = edit_failing(&set, edit);
        assert_eq!(result.err(), Some(Error::OutOfMemory), "{what}");
        assert!(unchanged, "{what}: the set changed");
    }
    // An edit of a run in place allocates nothing, so nothing can fail it.
    let in_place: [(&str, Edit); 3] = [
        ("joining the run before", |s| s.try_insert(200, 10)),
        ("a cut at the end of a run", |s| s.try_remove(190, 10)),
        ("a union extending a run", |s| {
            s.try_add(150, 100).map(|()| true)
        }),
    ];
    for (what, edit) in in_place {
        let (result, _) = edit_failing(&set, edit);
        assert_eq!(result.ok(), Some(true), "{what}");
    }
}

/// A device that answers its `fail_at`th read with `OutOfMemory`.
#[derive(Debug, Clone)]
struct Starving {
    disk: MemDevice,
    reads: u64,
    fail_at: u64,
}

impl Device for Starving {
    fn read_at(&mut self, physical: u64, buf: &mut [u8], kind: ReadKind) -> Result<(), BtrfsError> {
        self.reads += 1;
        if self.reads == self.fail_at {
            return Err(BtrfsError::OutOfMemory);
        }
        self.disk.read_at(physical, buf, kind)
    }
}

impl WriteDevice for Starving {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> crate::Result<()> {
        self.disk.write_at(physical, data)
    }

    fn flush(&mut self) -> crate::Result<()> {
        self.disk.flush()
    }
}

#[test]
fn a_node_read_out_of_memory_is_not_retried_from_the_other_copy() {
    let device = Starving {
        disk: MemDevice::new(BLANK),
        reads: 0,
        fail_at: 0,
    };
    let mut volume = WriteVolume::open(device).unwrap();
    let root = volume.root(FS_TREE_OBJECTID).unwrap();
    let copies = volume.chunks.copies(root.bytenr, 1).unwrap();
    assert_eq!(copies.len(), 2, "the fixture's metadata is DUP");
    // Nothing cached: the next lookup reads the fs tree's root, and its
    // first copy answers that memory ran out. The second copy would read
    // fine, and must not be asked.
    volume.clean.clear();
    volume.device.reads = 0;
    volume.device.fail_at = 1;
    let read = volume.get(FS_TREE_OBJECTID, &BtrfsKey::MIN);
    assert_eq!(read.err(), Some(Error::OutOfMemory));
    assert_eq!(volume.device.reads, 1, "the other copy was read");
    assert_eq!(volume.aborted(), None, "a read aborts nothing");
    volume.device.fail_at = 0;
    assert!(volume.get(FS_TREE_OBJECTID, &BtrfsKey::MIN).is_ok());
}
