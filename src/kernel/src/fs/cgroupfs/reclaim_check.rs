//! Stage 13's reclaim, landing M2 (`docs/CGROUPS.md` §10, `user::cache`).
//!
//! Files on a disk keep their pages in the page cache, charged to the job
//! that read them in. A job over its `memory.high` gets them taken back, a
//! job at its `memory.max` gets them taken back before anything fails or
//! dies, and in both the pages read again are what the file holds. Each
//! claim below is a cgroup, a store over a page source that makes every
//! byte a function of its place, and a read made as a task of the cgroup:
//!
//! * `/check-r`, `memory.high` 16 pages: a read of 64 pages leaves the job at
//!   or under its mark, `memory.events` counting `high`, `memory.stat`
//!   counting `pgscan` and `pgsteal` and naming the cache in `file`; the
//!   bytes read are the source's, and a second read fills the taken pages
//!   again and they are the source's too;
//! * `/check-rs`, beside it, holds 8 pages of a file and 4 of a tmpfs file
//!   (`shmem`), and keeps every one, since reclaim is scoped to the job it
//!   is asked for (never a sibling's) and never takes a memory filesystem's;
//! * `/check-rm`, `memory.max` 16 pages: a read of 64 pages succeeds, with
//!   `max` counted and no `oom` or `oom_kill`: the cache was the room;
//! * `/check-rp`, `memory.high` 16 pages, with a child holding 64 pages:
//!   `memory.min` on the child spares them, `memory.low` spares them only
//!   while anything else can give, and with neither they are taken;
//! * the files: `memory.high` reads `max` and what was written, `memory.low`
//!   and `memory.min` read `0`, a bad value is `EINVAL`, and there is no
//!   `memory.swap.max`;
//! * `pgfault` counts the faults of a job's program.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::tmpfs::{PageSource, Pages, Storage};
use ferrix_vma::VmaFlags;

use super::{Checked, Harness};
use crate::fallible;
use crate::fs::pages::VmoStorage;
use crate::object::job::Job;
use crate::sched;
use crate::syscall::process::{self, Process};
use crate::user::space::Access;

/// Pages of the file each job reads.
const PAGES: u64 = 64;
/// `memory.high` and `memory.max` of the limited jobs: 16 pages.
const MARK: &[u8] = b"65536\n";
/// The same, as a number.
const MARK_BYTES: u64 = 16 * PAGE_SIZE;
/// Pages of the sibling's file.
const SIBLING_PAGES: u64 = 8;
/// Pages of the sibling's tmpfs file.
const SHMEM_PAGES: u64 = 4;
/// Where the pgfault program's memory is mapped.
const FAULTS_AT: u64 = 0x4000_0000;
/// Pages it touches.
const FAULTS: u64 = 4;

/// The byte of page `index` at `at` a [`Pattern`] makes up.
fn byte(index: u64, at: usize) -> u8 {
    (index.wrapping_mul(131) as u8) ^ (at as u8).wrapping_mul(7) ^ 0x5a
}

/// A page source that makes its pages up, and can always make them again:
/// what a read-only mount's files are.
#[derive(Debug, Default)]
struct Pattern {
    /// Pages it has filled.
    filled: AtomicU64,
}

impl PageSource for Pattern {
    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> ferrix_vfs::Result<usize> {
        for (index, page) in (first..).zip(pages.iter_mut()) {
            for (at, slot) in page.iter_mut().enumerate() {
                *slot = byte(index, at);
            }
        }
        let _ = self.filled.fetch_add(pages.len() as u64, Ordering::Relaxed);
        Ok(pages.len())
    }

    fn reclaimable(&self) -> bool {
        true
    }
}

/// A file of `pages` pages over a [`Pattern`].
fn file(pages: u64) -> Checked<(Box<dyn Pages>, Arc<Pattern>)> {
    let source = Arc::new(Pattern::default());
    let store = VmoStorage
        .allocate_with(Arc::clone(&source) as Arc<dyn PageSource>)
        .map_err(|_| "reclaim check: no store over a page source")?;
    store.resize(pages * PAGE_SIZE);
    Ok((store, source))
}

/// A cgroup, and a process in it to stand for its programs.
pub(super) struct Group {
    /// Its name, beneath the mount.
    path: &'static [u8],
    /// The process, if it has one: a cgroup that enables memory for its
    /// children has none.
    process: Option<Arc<Process>>,
    /// Its job.
    job: Option<Arc<Job>>,
}

impl Group {
    /// Make the cgroup `path`, with no process in it.
    pub(super) fn bare(harness: &mut Harness, path: &'static [u8]) -> Checked<Group> {
        harness
            .mkdir(path)
            .map_err(|_| "reclaim check: mkdir of a cgroup failed")?;
        harness.report.made += 1;
        Ok(Group {
            path,
            process: None,
            job: None,
        })
    }

    /// Make the cgroup `path` and move a new process into it.
    pub(super) fn make(harness: &mut Harness, path: &'static [u8]) -> Checked<Group> {
        let mut group = Group::bare(harness, path)?;
        let process =
            process::new_for_check().map_err(|_| "reclaim check: no process for a cgroup")?;
        let listed = alloc::format!("{}\n", process.pid());
        let mut procs = Vec::from(path);
        procs.extend_from_slice(b"/cgroup.procs");
        let _ = harness
            .write(&procs, listed.as_bytes())
            .map_err(|_| "reclaim check: a move into a cgroup failed")?;
        group.job = Some(process.job());
        group.process = Some(process);
        Ok(group)
    }

    /// The slot charges made as a task of this cgroup go to.
    pub(super) fn slot(&self) -> u32 {
        self.job
            .as_ref()
            .map_or(crate::object::quota::NONE, |job| job.quota_index())
    }

    /// `file` of this cgroup, as a path beneath the mount.
    pub(super) fn file(&self, name: &str) -> Vec<u8> {
        let mut path = Vec::from(self.path);
        path.push(b'/');
        path.extend_from_slice(name.as_bytes());
        path
    }

    /// Set `name` to `value`.
    pub(super) fn set(&self, harness: &Harness, name: &str, value: &[u8]) -> Checked<()> {
        harness
            .write(&self.file(name), value)
            .map(drop)
            .map_err(|_| "reclaim check: a controller file refused a value")
    }

    /// The number after `key` in `name`, or `name` itself when `key` is
    /// empty.
    pub(super) fn number(&self, harness: &Harness, name: &str, key: &str) -> Checked<u64> {
        let text = harness
            .read(&self.file(name))
            .map_err(|_| "reclaim check: a controller file did not read")?;
        let text = core::str::from_utf8(&text).map_err(|_| "reclaim check: a file is not text")?;
        if key.is_empty() {
            return text
                .trim()
                .parse()
                .map_err(|_| "reclaim check: a file is not a number");
        }
        text.lines()
            .find_map(|line| {
                let (name, value) = line.split_once(' ')?;
                (name == key).then(|| value.parse().ok()).flatten()
            })
            .ok_or("reclaim check: a key is missing from a file")
    }

    /// Run `work` as a task of this cgroup, charged to it.
    pub(super) fn as_task<T>(&self, work: impl FnOnce() -> T) -> T {
        let own = sched::running_group();
        sched::set_current_group(self.slot());
        let done = work();
        sched::set_current_group(own);
        done
    }

    /// Read all of `store` as a task of this cgroup, and whether the bytes
    /// were the source's.
    fn read_all(&self, store: &dyn Pages, pages: u64) -> Checked<bool> {
        let len = usize::try_from(pages * PAGE_SIZE).map_err(|_| "reclaim check: too long")?;
        let mut buf = fallible::try_filled(0_u8, len).map_err(|_| "reclaim check: no memory")?;
        let own = sched::running_group();
        sched::set_current_group(self.slot());
        let read = store.read(0, &mut buf);
        sched::set_current_group(own);
        read.map_err(|_| "reclaim check: a read of a cached file failed")?;
        Ok(buf
            .chunks(PAGE_SIZE as usize)
            .zip(0..)
            .all(|(page, index)| {
                page.iter()
                    .enumerate()
                    .all(|(at, &got)| got == byte(index, at))
            }))
    }

    /// Kill the process and leave the cgroup empty.
    pub(super) fn end(self, harness: &Harness) -> Checked<()> {
        if let Some(process) = &self.process {
            process::kill(process, crate::object::job::KILLED_STATUS);
        }
        let path = self.path;
        drop(self);
        harness
            .rmdir(path)
            .map_err(|_| "reclaim check: a cgroup would not go")
    }
}

/// Run it. How many pages reclaim gave back across the check.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §10 has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u64> {
    let _ = harness
        .write(b"/cgroup.subtree_control", b"+memory\n")
        .map_err(|_| "the root refused to enable memory for the reclaim check")?;
    let outcome = all(harness);
    let disabled = harness.write(b"/cgroup.subtree_control", b"-memory\n");
    let stolen = outcome?;
    let _ = disabled.map_err(|_| "the root refused to disable memory after the reclaim check")?;
    Ok(stolen)
}

/// Every claim, in order.
fn all(harness: &mut Harness) -> Checked<u64> {
    let limited = Group::make(harness, b"/check-r")?;
    let sibling = Group::make(harness, b"/check-rs")?;
    let (file_a, _) = file(PAGES)?;
    let (file_b, source_b) = file(SIBLING_PAGES)?;
    let memory_b = VmoStorage
        .allocate()
        .map_err(|_| "reclaim check: no tmpfs store")?;
    the_files(harness, &limited, &sibling)?;
    let stolen = high(
        harness, &limited, &*file_a, &sibling, &*file_b, &*memory_b, &source_b,
    )?;
    drop((file_a, file_b, memory_b));
    limited.end(harness)?;
    sibling.end(harness)?;
    let at_max = max(harness)?;
    let protected = protection(harness)?;
    faults(harness)?;
    Ok(stolen + at_max + protected)
}

/// `memory.high`, `.low` and `.min` read and write as Linux's do, and
/// `memory.swap.*` is absent.
fn the_files(harness: &mut Harness, group: &Group, other: &Group) -> Checked<()> {
    let reads =
        |harness: &Harness, name: &str, expected: &[u8]| harness.reads(&group.file(name), expected);
    if !reads(harness, "memory.high", b"max\n")
        || !reads(harness, "memory.low", b"0\n")
        || !reads(harness, "memory.min", b"0\n")
    {
        return Err("a new cgroup's memory.high, memory.low and memory.min do not read max, 0, 0");
    }
    group.set(harness, "memory.high", MARK)?;
    group.set(harness, "memory.low", b"8192\n")?;
    group.set(harness, "memory.min", b"4096\n")?;
    if !reads(harness, "memory.high", MARK)
        || !reads(harness, "memory.low", b"8192\n")
        || !reads(harness, "memory.min", b"4096\n")
    {
        return Err("memory.high, memory.low and memory.min do not read back what was written");
    }
    group.set(harness, "memory.low", b"0\n")?;
    group.set(harness, "memory.min", b"0\n")?;
    let bad = harness.write(&group.file("memory.high"), b"plenty\n");
    harness.refused(
        bad.err(),
        Errno::EINVAL,
        "a memory.high of `plenty` was not EINVAL",
    )?;
    if harness.exists(&group.file("memory.swap.max"))
        || harness.exists(&other.file("memory.swap.current"))
    {
        return Err("a cgroup has a memory.swap file, and Ferrix has no swap");
    }
    Ok(())
}

/// `/check-r` over its `memory.high`, `/check-rs` beside it.
///
/// Verifies: L.object.108, L.object.109, L.object.111, L.object.112, H.QUOTA.12
fn high(
    harness: &mut Harness,
    group: &Group,
    file: &dyn Pages,
    other: &Group,
    beside: &dyn Pages,
    memory: &dyn Pages,
    source_b: &Pattern,
) -> Checked<u64> {
    if !other.read_all(beside, SIBLING_PAGES)? {
        return Err("a sibling's file did not read as its source has it");
    }
    let own = sched::running_group();
    sched::set_current_group(other.slot());
    let wrote = (0..SHMEM_PAGES)
        .try_for_each(|index| memory.write(index * PAGE_SIZE, &[7; PAGE_SIZE as usize]));
    sched::set_current_group(own);
    wrote.map_err(|_| "a tmpfs file would not take its pages")?;
    let before = other.number(harness, "memory.current", "")?;
    let beside_filled = source_b.filled.load(Ordering::Relaxed);

    if !group.read_all(file, PAGES)? {
        return Err(
            "a file read in a cgroup over its memory.high did not read as its source has it",
        );
    }
    let current = group.number(harness, "memory.current", "")?;
    if current > MARK_BYTES {
        crate::console::println!("  reclaim  memory.current {current}, memory.high {MARK_BYTES}");
        return Err("a cgroup over its memory.high was not brought back to it");
    }
    if group.number(harness, "memory.events", "high")? == 0 {
        return Err("memory.events did not count a cgroup going over its memory.high");
    }
    let stolen = group.number(harness, "memory.stat", "pgsteal")?;
    let scanned = group.number(harness, "memory.stat", "pgscan")?;
    if stolen == 0 || scanned < stolen {
        return Err("memory.stat did not count the pages reclaim scanned and gave back");
    }
    if group.number(harness, "memory.stat", "file")? != current {
        return Err("memory.stat's file is not the cache that memory.current holds");
    }
    if file.committed_bytes() > MARK_BYTES {
        return Err("a file's object holds more than its cgroup's memory.high");
    }

    // The taken pages come back from the source, and are its bytes.
    let filled = group.number(harness, "memory.stat", "pgsteal")?;
    if !group.read_all(file, PAGES)? {
        return Err("pages reclaim took did not read back as the source has them");
    }
    if group.number(harness, "memory.stat", "pgsteal")? <= filled {
        return Err("a second read over memory.high took nothing more");
    }

    // The sibling lost nothing.
    if other.number(harness, "memory.current", "")? != before
        || beside.committed_bytes() != SIBLING_PAGES * PAGE_SIZE
        || memory.committed_bytes() != SHMEM_PAGES * PAGE_SIZE
        || source_b.filled.load(Ordering::Relaxed) != beside_filled
        || other.number(harness, "memory.stat", "pgsteal")? != 0
    {
        return Err("a sibling cgroup's pages were reclaimed for another's memory.high");
    }
    if other.number(harness, "memory.stat", "file")? != SIBLING_PAGES * PAGE_SIZE
        || other.number(harness, "memory.stat", "shmem")? != SHMEM_PAGES * PAGE_SIZE
    {
        return Err("memory.stat does not split a cgroup's cache into file and shmem");
    }
    Ok(stolen)
}

/// `/check-rm` at its `memory.max`: the cache is room.
fn max(harness: &mut Harness) -> Checked<u64> {
    let group = Group::make(harness, b"/check-rm")?;
    group.set(harness, "memory.max", MARK)?;
    let (file, _) = file(PAGES)?;
    if !group.read_all(&*file, PAGES)? {
        return Err("a read at memory.max did not read as its source has it, or failed");
    }
    let current = group.number(harness, "memory.current", "")?;
    let killed = group.number(harness, "memory.events", "oom_kill")?;
    let ooms = group.number(harness, "memory.events", "oom")?;
    let refused = group.number(harness, "memory.events", "max")?;
    let stolen = group.number(harness, "memory.stat", "pgsteal")?;
    drop(file);
    group.end(harness)?;
    if current > MARK_BYTES {
        return Err("a cgroup read past its memory.max holds more than it");
    }
    if refused == 0 || stolen == 0 {
        return Err("a read past memory.max did not count `max` and reclaim");
    }
    if ooms != 0 || killed != 0 {
        return Err("a charge past memory.max killed, where its clean cache was room");
    }
    Ok(stolen)
}

/// `memory.min` and `memory.low` on a child of a cgroup over its `memory.high`.
fn protection(harness: &mut Harness) -> Checked<u64> {
    // A cgroup with a member cannot hand memory to children: the parent
    // keeps no process, and the child has it.
    let parent = Group::bare(harness, b"/check-rp")?;
    parent.set(harness, "cgroup.subtree_control", b"+memory\n")?;
    let child = Group::make(harness, b"/check-rp/c")?;
    parent.set(harness, "memory.high", MARK)?;
    let (file, _) = file(PAGES)?;

    child.set(harness, "memory.min", b"1048576\n")?;
    if !child.read_all(&*file, PAGES)? {
        return Err("a protected file did not read as its source has it");
    }
    let held = child.number(harness, "memory.current", "")?;
    if held != PAGES * PAGE_SIZE || parent.number(harness, "memory.stat", "pgsteal")? != 0 {
        return Err("reclaim took pages from a child using no more than its memory.min");
    }
    if parent.number(harness, "memory.events", "high")? == 0 {
        return Err("a cgroup over its memory.high did not count it while its child was spared");
    }

    child.set(harness, "memory.min", b"0\n")?;
    child.set(harness, "memory.low", b"1048576\n")?;
    if !child.read_all(&*file, PAGES)? {
        return Err("a file under memory.low did not read as its source has it");
    }
    let kept = parent.number(harness, "memory.current", "")?;
    if kept > MARK_BYTES || parent.number(harness, "memory.stat", "pgsteal")? == 0 {
        return Err("memory.low held pages that nothing else could give in its place");
    }
    drop(file);
    let stolen = parent.number(harness, "memory.stat", "pgsteal")?;
    child.end(harness)?;
    parent.end(harness)?;
    Ok(stolen)
}

/// `pgfault` counts the faults of a job's program.
fn faults(harness: &mut Harness) -> Checked<()> {
    let group = Group::make(harness, b"/check-rf")?;
    let process = group.process.as_ref().ok_or("reclaim check: no process")?;
    let space = process.space();
    let _ = space
        .map_anonymous(FAULTS_AT, FAULTS * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no mapping for the pgfault check")?;
    let before = group.number(harness, "memory.stat", "pgfault")?;
    let own = sched::running_group();
    sched::set_current_group(group.slot());
    let faulted = (0..FAULTS).try_for_each(|page| {
        crate::object::oom::fault(space, FAULTS_AT + page * PAGE_SIZE, Access::WRITE)
    });
    sched::set_current_group(own);
    faulted.map_err(|_| "a fault of the pgfault check's program failed")?;
    if group.number(harness, "memory.stat", "pgfault")? != before + FAULTS {
        return Err("memory.stat's pgfault did not count the faults of the cgroup's program");
    }
    group.end(harness)
}
