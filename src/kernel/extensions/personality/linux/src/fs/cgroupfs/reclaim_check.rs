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
//! * `/check-rt`, `memory.high` 16 pages, writes 32 pages to a tmpfs file
//!   and reads them back through the file: the read is throttled, reclaim
//!   finds nothing it may take, and the reader sleeps -- holding the file's
//!   I/O lock and no spin lock, so the machine is not stopped (F-70) -- and
//!   reads what it wrote;
//! * `/check-rh`, at its `memory.max`, reads a hot file again between the
//!   reads of a stream that need room, with a cold file beside it: reclaim
//!   goes round the cache from where it last stopped, so the hot file is
//!   taken back at most twice and the cold one gives the rest -- where a
//!   reclaim that always began at the first file took the hot one back for
//!   every read, the livelock of Endfield's install8 boot (`user::cache`,
//!   "Where a reclaim starts");
//! * `/check-rr` reads 64 pages and holds 4 of a tmpfs file, with
//!   `/check-rq` beside it holding 8 pages: a write of 32 pages' bytes to
//!   `memory.reclaim` gives back at least that, a write of more than the
//!   clean cache is `EAGAIN` with the cache given back and the tmpfs pages
//!   kept, `max` is `EINVAL`, the sibling loses nothing, and the file reads
//!   as its source has it again;
//! * `pgfault` counts the faults of a job's program, `memory.stat`'s `anon`
//!   its pages and `pagetables` the tables made to map them, which with
//!   `file`, `shmem` and `kernel` are `memory.current`, and an unmap gives
//!   back the pages and the emptied tables;
//! * `/check-rd` looks up 64 names that are not there, as `/check-rc` beside
//!   it does: the negative dentries are charged to it as kernel heap, a write
//!   of `1G` to its `memory.reclaim`, with no cache to give, is `EAGAIN` and
//!   gives them back, and the sibling's stay charged; a write of 8192 bytes
//!   after more misses succeeds on the dentries alone; a `reclaim` of one page
//!   at a scope holding no pages and more misses gives back heap alone.

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
    // The sibling's file first: a reclaim walks the objects in the order
    // they were made, so its pages come before the limited cgroup's own and a
    // reclaim that reached outside its scope would take them before any.
    let (file_b, source_b) = file(SIBLING_PAGES)?;
    let (file_a, _) = file(PAGES)?;
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
    let hot_and_cold = hot_and_cold(harness)?;
    let protected = protection(harness)?;
    tmpfs_read_throttled(harness)?;
    let asked = on_demand(harness)?;
    dentries(harness)?;
    faults(harness)?;
    Ok(stolen + at_max + hot_and_cold + protected + asked)
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
    // The sibling lost nothing: asked first after each read, since a reclaim
    // that took the sibling's pages would also leave this cgroup over its mark.
    let untouched = || -> Checked<()> {
        if other.number(harness, "memory.current", "")? != before
            || beside.committed_bytes() != SIBLING_PAGES * PAGE_SIZE
            || memory.committed_bytes() != SHMEM_PAGES * PAGE_SIZE
            || source_b.filled.load(Ordering::Relaxed) != beside_filled
            || other.number(harness, "memory.stat", "pgsteal")? != 0
        {
            return Err("a sibling cgroup's pages were reclaimed for another's memory.high");
        }
        Ok(())
    };

    if !group.read_all(file, PAGES)? {
        return Err(
            "a file read in a cgroup over its memory.high did not read as its source has it",
        );
    }
    untouched()?;
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
    untouched()?;
    if other.number(harness, "memory.stat", "file")? != SIBLING_PAGES * PAGE_SIZE
        || other.number(harness, "memory.stat", "shmem")? != SHMEM_PAGES * PAGE_SIZE
    {
        return Err("memory.stat does not split a cgroup's cache into file and shmem");
    }
    Ok(stolen)
}

/// Pages `/check-rt` writes to its tmpfs file: twice its `memory.high`.
const TMPFS_PAGES: u64 = 32;

/// `/check-rt` reads a tmpfs file while over its `memory.high`: the read
/// throttles and sleeps, which is allowed only because the file's data
/// operations hold its sleeping I/O lock and not its inode spin lock (F-70;
/// with the spin lock held, FX-0503 stops the machine here).
fn tmpfs_read_throttled(harness: &mut Harness) -> Checked<()> {
    use ferrix_vfs::{FileSystem, NewNode};
    let group = Group::make(harness, b"/check-rt")?;
    group.set(harness, "memory.high", MARK)?;
    let page = PAGE_SIZE as usize;
    let outcome = group.as_task(|| -> Checked<bool> {
        let fs = crate::fs::new_tmpfs().map_err(|_| "reclaim check: no tmpfs")?;
        let file = fs
            .root()
            .create(b"f", NewNode::Regular, 0o600)
            .map_err(|_| "reclaim check: no tmpfs file")?;
        for index in 0..TMPFS_PAGES {
            let fill = [index as u8; PAGE_SIZE as usize];
            let _ = file
                .write_at(index * PAGE_SIZE, &fill, false)
                .map_err(|_| "a tmpfs file over memory.high would not take a write")?;
        }
        let mut buf = fallible::try_filled(0_u8, page).map_err(|_| "reclaim check: no memory")?;
        let mut whole = true;
        for index in 0..TMPFS_PAGES {
            let read = file
                .read_at(index * PAGE_SIZE, &mut buf)
                .map_err(|_| "a tmpfs read over memory.high failed")?;
            whole &= read == page && buf.iter().all(|&b| b == index as u8);
        }
        Ok(whole)
    });
    let throttled = group.number(harness, "memory.events", "high")?;
    group.end(harness)?;
    if !outcome? {
        return Err("a tmpfs file read over memory.high did not read back what was written");
    }
    if throttled == 0 {
        return Err("a tmpfs read over memory.high was not throttled");
    }
    Ok(())
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

/// Pages of `/check-rh`'s hot file, read again after each read of the stream.
const HOT_PAGES: u64 = 8;
/// Pages of its cold file, read once.
const COLD_PAGES: u64 = 128;
/// Pages of the stream each step reads, and the steps.
const STEP_PAGES: u64 = 8;
const STEPS: u64 = 16;

/// `/check-rh` at its `memory.max`, which holds the hot file, the cold one
/// and one step of the stream: each step reads the next pages of the
/// stream, which need room, and then the hot file again, as a program's
/// running code is faulted again. Reclaim goes round the cache from where
/// the last one stopped (`user::cache`'s hand), so over the 16 steps the hot
/// file's pages are taken back at most twice -- once a lap, and the lap is
/// longer than what the steps take -- and the cold file gives the rest. A
/// reclaim that began at the first file every time took the hot file back
/// at every reclaim and never reached the cold one. Answers the pages the
/// job's reclaim gave back.
///
/// Verifies: L.object.191
fn hot_and_cold(harness: &mut Harness) -> Checked<u64> {
    let group = Group::make(harness, b"/check-rh")?;
    let limit = (HOT_PAGES + COLD_PAGES + STEP_PAGES) * PAGE_SIZE;
    group.set(
        harness,
        "memory.max",
        alloc::format!("{limit}\n").as_bytes(),
    )?;
    // The hot file made first, so it comes first in the cache's list.
    let (hot, hot_source) = file(HOT_PAGES)?;
    let (cold, _) = file(COLD_PAGES)?;
    let (stream, _) = file(STEPS * STEP_PAGES)?;
    let outcome = hot_and_cold_steps(&group, &*hot, &*cold, &*stream, &hot_source);
    let stolen = group.number(harness, "memory.stat", "pgsteal")?;
    let killed = group.number(harness, "memory.events", "oom_kill")?;
    drop((hot, cold, stream));
    group.end(harness)?;
    let refilled = outcome?;
    if killed != 0 {
        return Err("a job reading at its memory.max was killed where its clean cache was room");
    }
    crate::console::println!(
        "  reclaim  at memory.max a file read between the reclaims was taken back {refilled} \
         pages in {STEPS} steps, the cold file beside it giving the rest: {stolen} pages"
    );
    if refilled > 2 * HOT_PAGES {
        return Err(
            "reclaim at a job's memory.max took back a file read between its reclaims again and \
             again, and not the cold file beside it",
        );
    }
    Ok(stolen)
}

/// [`hot_and_cold`]'s reads, as a task of `group`: the pages of the hot file
/// its source filled again after the first read of each file.
fn hot_and_cold_steps(
    group: &Group,
    hot: &dyn Pages,
    cold: &dyn Pages,
    stream: &dyn Pages,
    hot_source: &Pattern,
) -> Checked<u64> {
    if !group.read_all(hot, HOT_PAGES)? || !group.read_all(cold, COLD_PAGES)? {
        return Err("a file read at memory.max did not read as its source has it");
    }
    let filled = hot_source.filled.load(Ordering::Relaxed);
    let len = usize::try_from(STEP_PAGES * PAGE_SIZE).map_err(|_| "reclaim check: too long")?;
    let mut buf = fallible::try_filled(0_u8, len).map_err(|_| "reclaim check: no memory")?;
    for step in 0..STEPS {
        let first = step * STEP_PAGES;
        let read = group.as_task(|| stream.read(first * PAGE_SIZE, &mut buf));
        read.map_err(|_| "a read of a stream at memory.max failed")?;
        let whole = buf
            .chunks(PAGE_SIZE as usize)
            .zip(first..)
            .all(|(page, index)| {
                page.iter()
                    .enumerate()
                    .all(|(at, &got)| got == byte(index, at))
            });
        if !whole || !group.read_all(hot, HOT_PAGES)? {
            return Err("a file read at memory.max did not read as its source has it");
        }
    }
    Ok(hot_source.filled.load(Ordering::Relaxed) - filled)
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

/// Pages `/check-rr`'s first `memory.reclaim` write asks for.
const ASKED_PAGES: u64 = 32;

/// `/check-rr` gives back what a write to its `memory.reclaim` asks, from its
/// own clean cache and nobody else's. Answers the pages it gave back.
///
/// Verifies: L.object.183
fn on_demand(harness: &mut Harness) -> Checked<u64> {
    let sibling = Group::make(harness, b"/check-rq")?;
    let group = Group::make(harness, b"/check-rr")?;
    let (beside, _) = file(SIBLING_PAGES)?;
    let (cached, _) = file(PAGES)?;
    let memory = VmoStorage
        .allocate()
        .map_err(|_| "reclaim check: no tmpfs store")?;
    let outcome = on_demand_steps(harness, &group, &*cached, &*memory, &sibling, &*beside);
    let stolen = group.number(harness, "memory.stat", "pgsteal")?;
    drop((beside, cached, memory));
    group.end(harness)?;
    sibling.end(harness)?;
    outcome?;
    crate::console::println!("  reclaim  memory.reclaim gave back {stolen} pages when asked");
    Ok(stolen)
}

/// [`on_demand`]'s reads, writes and the counts they leave.
fn on_demand_steps(
    harness: &mut Harness,
    group: &Group,
    cached: &dyn Pages,
    memory: &dyn Pages,
    sibling: &Group,
    beside: &dyn Pages,
) -> Checked<()> {
    if !sibling.read_all(beside, SIBLING_PAGES)? || !group.read_all(cached, PAGES)? {
        return Err("a file read before memory.reclaim did not read as its source has it");
    }
    let wrote = group.as_task(|| {
        (0..SHMEM_PAGES)
            .try_for_each(|index| memory.write(index * PAGE_SIZE, &[9; PAGE_SIZE as usize]))
    });
    wrote.map_err(|_| "a tmpfs file would not take its pages")?;
    if group.number(harness, "memory.stat", "file")? != PAGES * PAGE_SIZE {
        return Err("a cgroup with no limit did not hold the cache it read");
    }

    group.set(harness, "memory.reclaim", b"0\n")?;
    if group.number(harness, "memory.stat", "pgsteal")? != 0 {
        return Err("a memory.reclaim of 0 bytes gave pages back");
    }
    let asked = alloc::format!("{}\n", ASKED_PAGES * PAGE_SIZE);
    group.set(harness, "memory.reclaim", asked.as_bytes())?;
    let held = group.number(harness, "memory.stat", "file")?;
    if held > (PAGES - ASKED_PAGES) * PAGE_SIZE
        || group.number(harness, "memory.stat", "pgsteal")? < ASKED_PAGES
    {
        crate::console::println!(
            "  reclaim  memory.reclaim of {ASKED_PAGES} pages left {held} bytes"
        );
        return Err("a memory.reclaim write did not give back what it asked for");
    }

    let short = harness.write(&group.file("memory.reclaim"), b"1G\n");
    harness.refused(
        short.err(),
        Errno::EAGAIN,
        "a memory.reclaim of more than a cgroup's clean cache was not EAGAIN",
    )?;
    if group.number(harness, "memory.stat", "file")? != 0
        || group.number(harness, "memory.stat", "shmem")? != SHMEM_PAGES * PAGE_SIZE
    {
        return Err(
            "a memory.reclaim short of its size did not give back all the clean cache, \
             and only it",
        );
    }
    // Asked to stop before a round, nothing is taken; at no scope, nothing
    // may be asked for.
    use crate::user::cache::{Asked, reclaim_asked};
    let refilled = group.read_all(cached, PAGES)?;
    let before = group.number(harness, "memory.stat", "pgsteal")?;
    if !refilled
        || reclaim_asked(group.slot(), ASKED_PAGES, &|| true) != Asked::Interrupted
        || group.number(harness, "memory.stat", "pgsteal")? != before
    {
        return Err("a reclaim asked for and interrupted before its first round took pages");
    }
    if reclaim_asked(crate::object::quota::NONE, 1, &|| false) != Asked::Short
        || reclaim_asked(crate::object::quota::NONE, 0, &|| false) != Asked::Given
    {
        return Err("a reclaim asked for at no scope was not Short, or of nothing not Given");
    }
    let bad = harness.write(&group.file("memory.reclaim"), b"max\n");
    harness.refused(
        bad.err(),
        Errno::EINVAL,
        "a memory.reclaim of `max` was not EINVAL",
    )?;

    if sibling.number(harness, "memory.stat", "file")? != SIBLING_PAGES * PAGE_SIZE
        || sibling.number(harness, "memory.stat", "pgsteal")? != 0
        || beside.committed_bytes() != SIBLING_PAGES * PAGE_SIZE
    {
        return Err("a memory.reclaim took a sibling cgroup's pages");
    }
    if !group.read_all(cached, PAGES)? {
        return Err("pages memory.reclaim took did not read back as the source has them");
    }
    Ok(())
}

/// Names each of `/check-rd` and `/check-rc` looks up and does not find.
const MISSES: u64 = 64;
/// The least a dentry holds of the heap: what each miss must give back.
const DENTRY_BYTES: u64 = 64;

/// The cached dentries `/check-rd`'s lookups made are given back by its
/// reclaim, and its sibling's are not; a `memory.reclaim` its dentries alone
/// can meet succeeds.
///
/// Verifies: L.object.186
/// Verifies: L.object.183
fn dentries(harness: &mut Harness) -> Checked<()> {
    let sibling = Group::make(harness, b"/check-rc")?;
    let group = Group::make(harness, b"/check-rd")?;
    let outcome = dentries_steps(harness, &group, &sibling);
    group.end(harness)?;
    sibling.end(harness)?;
    let given = outcome?;
    crate::console::println!(
        "  reclaim  memory.reclaim gave back {given} bytes of {MISSES} cached dentries, a sibling's kept"
    );
    Ok(())
}

/// `MISSES` lookups of names that are not there, as a task of `group`.
fn misses(group: &Group, round: u32) {
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    group.as_task(|| {
        for at in 0..MISSES {
            let name = alloc::format!("/check-rd-{}-{round}-{at}", group.slot());
            let _ = ns.resolve(&ctx, None, name.as_bytes(), true);
        }
    });
}

/// [`dentries`]'s lookups and reclaims; the bytes the write gave back.
fn dentries_steps(harness: &mut Harness, group: &Group, sibling: &Group) -> Checked<u64> {
    let kernel = |harness: &Harness, group: &Group| group.number(harness, "memory.stat", "kernel");
    let (before, theirs_before) = (kernel(harness, group)?, kernel(harness, sibling)?);
    misses(sibling, 0);
    misses(group, 0);
    let (cached, theirs) = (kernel(harness, group)?, kernel(harness, sibling)?);
    if cached < before + MISSES * DENTRY_BYTES || theirs < theirs_before + MISSES * DENTRY_BYTES {
        return Err("lookups that missed did not charge their dentries to the cgroup");
    }
    let short = harness.write(&group.file("memory.reclaim"), b"1G\n");
    harness.refused(
        short.err(),
        Errno::EAGAIN,
        "a memory.reclaim of more than a cgroup's cache and dentries was not EAGAIN",
    )?;
    let after = kernel(harness, group)?;
    if after.saturating_add(MISSES * DENTRY_BYTES) > cached {
        crate::console::println!("  reclaim  kernel {cached} before memory.reclaim, {after} after");
        return Err("a memory.reclaim did not give back a cgroup's cached dentries");
    }
    if kernel(harness, sibling)? != theirs {
        return Err("a memory.reclaim gave back a sibling cgroup's cached dentries");
    }
    // What is asked for is bytes, and the heap counts towards them.
    misses(group, 2);
    group
        .set(harness, "memory.reclaim", b"8192\n")
        .map_err(|_| "a memory.reclaim its cached dentries could meet was refused")?;
    misses(group, 1);
    let done = crate::user::cache::reclaim(group.slot(), 1);
    #[allow(
        clippy::disallowed_methods,
        reason = "a raised check: `Reclaimed::stolen` is made pub for the checks alone"
    )]
    let stolen = done.stolen();
    if stolen != 0 || done.heap < DENTRY_BYTES {
        return Err("a reclaim at a cgroup with no pages and cached dentries gave back no heap");
    }
    Ok(cached - after)
}

/// `memory.stat`'s `anon` and `pagetables` with the pgfault check's pages
/// faulted: its pages counted in `anon`, the tables made to map them in
/// `pagetables`, and with `file`, `shmem` and `kernel` all of
/// `memory.current`.
///
/// Verifies: L.object.182
fn tables_and_pages(harness: &Harness, group: &Group) -> Checked<(u64, u64)> {
    let number = |key| group.number(harness, "memory.stat", key);
    let (anon, tables) = (number("anon")?, number("pagetables")?);
    let rest = number("file")? + number("shmem")? + number("kernel")?;
    let current = group.number(harness, "memory.current", "")?;
    if anon < FAULTS * PAGE_SIZE || tables < PAGE_SIZE {
        crate::console::println!("  reclaim  anon {anon} pagetables {tables} of {FAULTS} pages");
        return Err("memory.stat's anon or pagetables did not count a program's pages or tables");
    }
    if anon + tables + rest != current {
        return Err(
            "memory.stat's anon, file, kernel, pagetables and shmem are not memory.current",
        );
    }
    Ok((anon, tables))
}

/// `pgfault` counts the faults of a job's program; `anon` and `pagetables`
/// its pages and the tables that map them, and give both back when the
/// mapping goes.
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
    let tables_and_pages = tables_and_pages(harness, &group)?;
    space
        .unmap(FAULTS_AT, FAULTS * PAGE_SIZE)
        .map_err(|_| "the pgfault check's mapping would not go")?;
    let (anon, tables) = tables_and_pages;
    let after = (
        group.number(harness, "memory.stat", "anon")?,
        group.number(harness, "memory.stat", "pagetables")?,
    );
    if after.0 + FAULTS * PAGE_SIZE != anon || after.1 >= tables {
        crate::console::println!(
            "  reclaim  anon {anon} pagetables {tables} mapped, {} and {} unmapped",
            after.0,
            after.1
        );
        return Err(
            "memory.stat's anon and pagetables did not give back a mapping's pages and its \
             emptied tables",
        );
    }
    group.end(harness)
}
