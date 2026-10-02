//! System V shared memory, proved at boot: `shmget`, the attach count and
//! permission of `shmat`, `shmctl`'s layouts and `IPC_RMID` deferred to the
//! last detach, through the functions the system calls reach
//! (`syscall::shm`), and the per-job bound with each segment's heap charged
//! to its maker and given back (F-37).
//!
//! An attach is held here as the [`shm::Attachment`] an address space would
//! keep for it, without a mapping: what is proved is the count and the
//! removal it drives. The mapping itself, across two processes, is
//! `cargo xtask test-shm`'s.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;

use crate::object::job::Job;
use crate::object::quota::Resource;
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::sem::{self, Caller, Layout, Memory};
use crate::syscall::shm::{self, cmd};

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls answered as Linux answers them.
    pub(crate) calls: usize,
    /// Of them, refusals with Linux's error.
    pub(crate) refusals: usize,
    /// Segments a job made before the per-job bound (at the check's bound)
    /// refused one, while a sibling made one.
    pub(crate) per_job: usize,
}

/// The per-job bound the check fills to, standing in for
/// [`shm::SEGMENTS_PER_JOB`].
const PER_JOB: usize = 5;

/// A key of the check's own.
const KEY: i32 = 0x5348_4d31;

/// Memory of the check's own for `shmctl` to read and write: the address is
/// an offset into it.
#[derive(Debug)]
struct Buffer(SpinLock<Vec<u8>>);

impl Buffer {
    /// 256 zero bytes.
    fn new() -> Buffer {
        Buffer(SpinLock::new(vec![0; 256]))
    }

    /// The little-endian `u32` at `at`.
    fn u32_at(&self, at: usize) -> u32 {
        let bytes = self.0.lock();
        bytes
            .get(at..at + 4)
            .and_then(|four| four.try_into().ok())
            .map_or(0, u32::from_le_bytes)
    }
}

impl Memory for Buffer {
    fn write(&self, at: u64, bytes: &[u8]) -> Result<(), Errno> {
        let at = usize::try_from(at).map_err(|_| Errno::EFAULT)?;
        let mut held = self.0.lock();
        let to = held
            .get_mut(at..at.saturating_add(bytes.len()))
            .ok_or(Errno::EFAULT)?;
        to.copy_from_slice(bytes);
        Ok(())
    }

    fn read(&self, at: u64, bytes: &mut [u8]) -> Result<(), Errno> {
        let at = usize::try_from(at).map_err(|_| Errno::EFAULT)?;
        let held = self.0.lock();
        let from = held
            .get(at..at.saturating_add(bytes.len()))
            .ok_or(Errno::EFAULT)?;
        bytes.copy_from_slice(from);
        Ok(())
    }
}

/// A caller of `uid` in the first namespace; root is privileged.
fn caller(uid: u32) -> Caller {
    Caller {
        pid: 4242 + uid,
        uid,
        gid: uid,
        groups: Vec::new(),
        privileged: uid == 0,
        ns: Arc::clone(sem::initial_ipc()),
    }
}

/// Root, as the check's caller.
fn root() -> Caller {
    caller(0)
}

/// A caller of uid and gid 1000, and nothing more: Steam's web helper.
fn stranger() -> Caller {
    caller(1000)
}

/// A segment the check holds, removed as it is dropped.
#[derive(Debug)]
pub(crate) struct Held(pub(crate) i32);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = shm::shmctl(
            &root(),
            &Buffer::new(),
            Layout::Narrow,
            self.0,
            cmd::RMID,
            0,
        );
    }
}

/// Make a private segment of `size` bytes with `mode`, as root would.
pub(crate) fn private_segment(size: u64, mode: i32) -> Result<Held, Errno> {
    shm::shmget(&root(), 0, size, cmd::CREAT | mode).map(Held)
}

/// Require `answer` to be `expected`, counting it.
fn expect<T: PartialEq + Copy + core::fmt::Debug>(
    report: &mut Report,
    answer: Result<T, Errno>,
    expected: Result<T, Errno>,
    what: &'static str,
) -> Result<(), &'static str> {
    if answer != expected {
        crate::console::println!("  shm      {what}: answered {answer:?}");
        return Err(what);
    }
    report.calls += 1;
    if expected.is_err() {
        report.refusals += 1;
    }
    Ok(())
}

/// Run every check.
///
/// # Errors
///
/// Which property failed.
///
/// Verifies: H.QUOTA.7
pub(crate) fn run() -> Result<Report, &'static str> {
    let before = shm::segments_in_use();
    let mut report = Report::default();
    keys(&mut report)?;
    layouts(&mut report)?;
    attaches(&mut report)?;
    removal(&mut report)?;
    {
        let tree = Job::new_root().map_err(|_| "shm: no memory for a job")?;
        report.per_job = per_job(&tree)?;
    }
    if shm::segments_in_use() != before {
        return Err("shm: the check's segments were not all removed");
    }
    Ok(report)
}

/// `shmget` by key: made, found, refused as Linux refuses.
fn keys(report: &mut Report) -> Result<(), &'static str> {
    let made = shm::shmget(&root(), KEY, 8192, cmd::CREAT | cmd::EXCL | 0o600)
        .map(Held)
        .map_err(|_| "shm: a keyed segment was not made")?;
    report.calls += 1;
    let id = made.0;
    expect(
        report,
        shm::shmget(&root(), KEY, 8192, cmd::CREAT | cmd::EXCL | 0o600),
        Err(Errno::EEXIST),
        "shm: IPC_EXCL on a used key was not EEXIST",
    )?;
    expect(
        report,
        shm::shmget(&root(), KEY, 4096, 0o600),
        Ok(id),
        "shm: a key did not find its segment",
    )?;
    expect(
        report,
        shm::shmget(&root(), KEY, 8193, 0),
        Err(Errno::EINVAL),
        "shm: a size past the segment's was not EINVAL",
    )?;
    expect(
        report,
        shm::shmget(&root(), KEY + 1, 4096, 0),
        Err(Errno::ENOENT),
        "shm: a key with no segment and no IPC_CREAT was not ENOENT",
    )?;
    expect(
        report,
        shm::shmget(&root(), 0, 0, cmd::CREAT),
        Err(Errno::EINVAL),
        "shm: a segment of no bytes was not EINVAL",
    )?;
    expect(
        report,
        shm::shmget(&root(), 0, shm::SHMMAX + 1, cmd::CREAT),
        Err(Errno::EINVAL),
        "shm: a segment past SHMMAX was not EINVAL",
    )?;
    expect(
        report,
        shm::shmget(&stranger(), KEY, 4096, 0o600),
        Err(Errno::EACCES),
        "shm: a stranger found root's mode-0600 segment",
    )?;
    drop(made);
    Ok(())
}

/// `IPC_STAT` in each layout, and `IPC_INFO`.
fn layouts(report: &mut Report) -> Result<(), &'static str> {
    let made = private_segment(12_345, 0o640).map_err(|_| "shm: no segment for the layouts")?;
    let held = shm::attach(&root(), made.0, 0).map_err(|_| "shm: root could not attach")?;
    for layout in [Layout::Narrow, Layout::X86_64, Layout::Generic64] {
        let buffer = Buffer::new();
        let answer = shm::shmctl(&root(), &buffer, layout, made.0, cmd::STAT | cmd::IPC_64, 0);
        let [segsz, _, _, _, cpid, _, attaches] = shm::fields(layout);
        if answer != Ok(0)
            || buffer.u32_at(20) & 0o777 != 0o640
            || buffer.u32_at(segsz) != 12_345
            || buffer.u32_at(cpid) != 4242
            || buffer.u32_at(attaches) != 1
            || buffer.u32_at(shm::shmid_bytes(layout)) != 0
        {
            crate::console::println!(
                "  shm      IPC_STAT in {layout:?}: {answer:?}, mode {:o}, size {}, cpid {}, \
                 nattch {}",
                buffer.u32_at(20),
                buffer.u32_at(segsz),
                buffer.u32_at(cpid),
                buffer.u32_at(attaches),
            );
            return Err("shm: IPC_STAT put a field where the UAPI headers do not");
        }
        report.calls += 1;
    }
    expect(
        report,
        shm::shmctl(
            &root(),
            &Buffer::new(),
            Layout::Narrow,
            made.0,
            cmd::STAT,
            0,
        ),
        Err(Errno::EINVAL),
        "shm: a 32-bit IPC_STAT without IPC_64 was not EINVAL",
    )?;
    let info = Buffer::new();
    let answered = shm::shmctl(
        &root(),
        &info,
        Layout::Narrow,
        0,
        cmd::INFO | cmd::IPC_64,
        0,
    );
    if answered.is_err() || u64::from(info.u32_at(0)) != shm::SHMMAX || info.u32_at(4) != 1 {
        return Err("shm: IPC_INFO did not report SHMMAX and SHMMIN");
    }
    report.calls += 1;
    let usage = Buffer::new();
    let answered = shm::shmctl(&root(), &usage, Layout::Generic64, 0, cmd::SHM_INFO, 0);
    if answered.is_err() || usage.u32_at(0) == 0 || usage.u32_at(8) < 4 {
        return Err("shm: SHM_INFO did not count the segment and its pages");
    }
    report.calls += 1;
    drop(held);
    drop(made);
    Ok(())
}

/// Who may attach: the mode's class, `SHM_RDONLY`, and root over a
/// stranger's mode-0600 segment, as the X server attaches Chromium's.
fn attaches(report: &mut Report) -> Result<(), &'static str> {
    let rooted = private_segment(4096, 0o600).map_err(|_| "shm: no segment for the modes")?;
    expect(
        report,
        shm::attach(&stranger(), rooted.0, cmd::RDONLY).map(|_| ()),
        Err(Errno::EACCES),
        "shm: a stranger attached root's mode-0600 segment",
    )?;
    let readable = private_segment(4096, 0o644).map_err(|_| "shm: no segment for the modes")?;
    expect(
        report,
        shm::attach(&stranger(), readable.0, 0).map(|_| ()),
        Err(Errno::EACCES),
        "shm: a stranger attached a mode-0644 segment for writing",
    )?;
    let read_only = shm::attach(&stranger(), readable.0, cmd::RDONLY);
    expect(
        report,
        read_only.as_ref().map(|_| ()).map_err(|error| *error),
        Ok(()),
        "shm: a stranger could not attach a mode-0644 segment read-only",
    )?;
    drop(read_only);
    // The web helper's segment, attached by the X server as root.
    let helper = shm::shmget(&stranger(), 0, 4096, cmd::CREAT | 0o600)
        .map(Held)
        .map_err(|_| "shm: a stranger could not make a segment")?;
    let server = shm::attach(&root(), helper.0, 0);
    expect(
        report,
        server.as_ref().map(|_| ()).map_err(|error| *error),
        Ok(()),
        "shm: root could not attach a stranger's mode-0600 segment",
    )?;
    expect(
        report,
        shm::shmctl(
            &caller(1001),
            &Buffer::new(),
            Layout::Generic64,
            helper.0,
            cmd::RMID,
            0,
        ),
        Err(Errno::EPERM),
        "shm: neither owner nor creator removed a segment",
    )?;
    // IPC_SET by its owner opens it to its group's reading; by anyone else
    // it is refused.
    let buffer = Buffer::new();
    for (at, value) in [(4, 1000_u32), (8, 1000), (20, 0o640)] {
        buffer
            .write(at, &value.to_le_bytes())
            .map_err(|_| "shm: buffer")?;
    }
    let mut colleague = caller(1001);
    colleague.groups.push(1000);
    expect(
        report,
        shm::shmctl(
            &colleague,
            &buffer,
            Layout::Generic64,
            helper.0,
            cmd::SET,
            0,
        ),
        Err(Errno::EPERM),
        "shm: neither owner nor creator changed a segment's mode",
    )?;
    expect(
        report,
        shm::attach(&colleague, helper.0, cmd::RDONLY).map(|_| ()),
        Err(Errno::EACCES),
        "shm: a group member attached a mode-0600 segment",
    )?;
    expect(
        report,
        shm::shmctl(
            &stranger(),
            &buffer,
            Layout::Generic64,
            helper.0,
            cmd::SET,
            0,
        ),
        Ok(0),
        "shm: the owner could not change its segment's mode",
    )?;
    expect(
        report,
        shm::attach(&colleague, helper.0, cmd::RDONLY).map(|_| ()),
        Ok(()),
        "shm: IPC_SET's mode 0640 did not let the group read",
    )?;
    drop(server);
    drop((rooted, readable, helper));
    Ok(())
}

/// `IPC_RMID` while attached: the key is free at once, the segment stays,
/// found by its id with `SHM_DEST`, counted as its attaches come and go,
/// and goes with the last one.
fn removal(report: &mut Report) -> Result<(), &'static str> {
    let id = shm::shmget(&stranger(), KEY, 4096, cmd::CREAT | cmd::EXCL | 0o600)
        .map_err(|_| "shm: no keyed segment for the removal")?;
    let client = shm::attach(&stranger(), id, 0).map_err(|_| "shm: the maker could not attach")?;
    let server = shm::attach(&root(), id, 0).map_err(|_| "shm: root could not attach")?;
    if shm::attaches(&root(), id) != Some(2) {
        return Err("shm: two attaches were not counted as two");
    }
    expect(
        report,
        shm::shmctl(
            &stranger(),
            &Buffer::new(),
            Layout::Generic64,
            id,
            cmd::RMID,
            0,
        ),
        Ok(0),
        "shm: the maker could not remove its segment",
    )?;
    expect(
        report,
        shm::shmget(&stranger(), KEY, 4096, 0o600),
        Err(Errno::ENOENT),
        "shm: a removed segment's key still found it",
    )?;
    let buffer = Buffer::new();
    let stat = shm::shmctl(&stranger(), &buffer, Layout::X86_64, id, cmd::STAT, 0);
    if stat != Ok(0) || buffer.u32_at(20) & cmd::DEST == 0 || buffer.u32_at(0) != 0 {
        return Err("shm: a removed, attached segment was not SHM_DEST under IPC_PRIVATE");
    }
    report.calls += 1;
    drop(client);
    if shm::attaches(&root(), id) != Some(1) {
        return Err("shm: a removed segment went before its last detach");
    }
    // An attach to a removed segment still attached, as Linux allows.
    let again = shm::attach(&root(), id, 0).map_err(|_| "shm: a removed segment refused root")?;
    drop(server);
    drop(again);
    expect(
        report,
        shm::shmctl(&root(), &Buffer::new(), Layout::X86_64, id, cmd::STAT, 0),
        Err(Errno::EINVAL),
        "shm: a removed segment outlived its last detach",
    )?;
    expect(
        report,
        shm::attach(&root(), id, 0).map(|_| ()),
        Err(Errno::EINVAL),
        "shm: a gone segment's id was attached",
    )?;
    Ok(())
}

/// A job makes segments to the per-job bound and is refused `ENOSPC`,
/// while a sibling makes one; the heap each took is its maker's, and comes
/// back.
///
/// Verifies: H.QUOTA.7
fn per_job(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let job = tree.new_child().map_err(|_| "shm: a job refused a child")?;
    let sibling = tree.new_child().map_err(|_| "shm: a job refused a child")?;
    let make = || shm::shmget_capped(&root(), 0, 4096, cmd::CREAT | 0o600, PER_JOB).map(Held);
    let mut held = Vec::new();
    let refused = as_task_of(&job, || {
        loop {
            match make() {
                Ok(one) => held.push(one),
                Err(error) => return error,
            }
            if held.len() > PER_JOB {
                return Errno::E2BIG;
            }
        }
    });
    if refused != Errno::ENOSPC || held.len() != PER_JOB {
        return Err("shm: a job's segments were not bounded with ENOSPC");
    }
    let other = as_task_of(&sibling, make);
    let charged = job.usage(Resource::Kernel).map_or(0, |usage| usage.used);
    if other.is_err() || charged == 0 {
        return Err("shm: a job at its bound held back its sibling, or was not charged");
    }
    let made = held.len();
    drop(held);
    drop(other);
    for one in [&job, &sibling] {
        if Resource::ALL
            .iter()
            .any(|&resource| one.usage(resource).is_some_and(|usage| usage.used != 0))
        {
            return Err("shm: removed segments were still charged to their job");
        }
    }
    Ok(made)
}

/// Run `work` charged to `job`, as a task of it would be.
fn as_task_of<T>(job: &Job, work: impl FnOnce() -> T) -> T {
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let done = work();
    sched::set_current_group(own);
    done
}
