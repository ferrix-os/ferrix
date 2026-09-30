//! The `io` controller: what each cgroup reads and writes of each disk, and
//! what `io.max` lets it (`docs/CGROUPS.md` §13, stage 13's I).
//!
//! Every registered disk is wrapped in an [`Accounted`] ([`crate::fs::devfs`]'s
//! `register_block_from`), so the one place block I/O passes through the
//! kernel for a program's reads and writes -- a mount's page-cache fill, a
//! write-back, a raw read of `/dev/vda` -- is where it is charged. A request
//! is charged to the job of the task that submits it and to every job above,
//! and to the machine (the root cgroup's `io.stat`), as cgroup v2's `io.stat`
//! is hierarchical; a disk's partitions are not wrapped, since their reads
//! reach the disk they are on, which Linux also counts by whole disk.
//!
//! # `io.max`
//!
//! A limit on a job's bytes or operations a second, each way, on one disk. A
//! request finds, in every job from its submitter's up, the instant that job's
//! limit next lets work start (`ferrix_block::Throttle`), waits in its
//! submitter until the latest of them, and pays them all the same start. A
//! job's children therefore share its limit however they split it.
//!
//! # Where the state lives
//!
//! In one table here, keyed by the job's quota slot and the disk, and not in
//! the job: a charge names a slot (`sched::running_group`), as the memory
//! charge does, and finding the job from it would need a lock the submitter
//! may not hold. The entries of a job go with it (`quota::on_job_release`).
//! An entry's heap is charged to its job as kernel memory (F-37); a job that
//! cannot pay has its I/O counted nowhere, and not refused.
//!
//! Not built: `io.weight` and `io.latency`. A weight needs the dispatch
//! shares of a scheduler with more than one request in flight to divide,
//! and `io.latency` needs one to protect; both would be files that accept a
//! value and do nothing.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_block::Throttle;
use ferrix_cgroupfs::io::{Limits, Stat};
use ferrix_kmem::Charge;
use ferrix_vfs::Errno;

use super::block::BlockDevice;
use crate::fallible;
use crate::object::quota;
use crate::sync::SpinLock;

/// A direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Direction {
    /// A read.
    Read,
    /// A write.
    Write,
}

/// One job's state for one disk.
#[derive(Debug)]
struct Entry {
    /// What it has counted.
    stat: Stat,
    /// Its read limit.
    read: Throttle,
    /// Its write limit.
    write: Throttle,
    /// Its heap, charged to the job.
    _charge: Charge,
}

/// Every entry, by job slot ([`quota::NONE`] for the machine), major and
/// minor.
static TABLE: SpinLock<BTreeMap<(u32, u32, u32), Entry>> = SpinLock::new(BTreeMap::new());

/// What an entry holds of the heap, at most: the node it is in and itself.
const ENTRY_HEAP: usize = 2 * size_of::<(u32, u32, u32)>() + 3 * size_of::<Entry>();

/// The slots a request of the running task is charged to: its job's, then
/// each above, then the machine's.
fn chain() -> Vec<u32> {
    let mut slots = Vec::new();
    let mut at = crate::sched::running_group();
    while at != quota::NONE {
        if fallible::try_push(&mut slots, at).is_err() {
            return slots;
        }
        at = quota::parent_of(at);
    }
    let _ = fallible::try_push(&mut slots, quota::NONE);
    slots
}

/// The entry for `slot` and `device`, made if the table has none for this job.
fn entry_of(
    table: &mut BTreeMap<(u32, u32, u32), Entry>,
    slot: u32,
    device: (u32, u32),
) -> Option<&mut Entry> {
    let key = (slot, device.0, device.1);
    if !table.contains_key(&key) {
        let charge = Charge::to(slot, ENTRY_HEAP).ok()?;
        let fresh = Entry {
            stat: Stat::default(),
            read: Throttle::UNLIMITED,
            write: Throttle::UNLIMITED,
            _charge: charge,
        };
        let _ = fallible::insert(table, key, fresh).ok()?;
    }
    table.get_mut(&key)
}

/// Charge a request of `bytes` in direction `way` on `device` to the running
/// task's job and every one above it, after waiting for each `io.max` on the
/// way up to let it start.
pub(crate) fn account(device: (u32, u32), way: Direction, bytes: u64) {
    let slots = chain();
    let now = crate::timer::now_nanos();
    let start = {
        let mut table = TABLE.lock();
        let mut start = now;
        let throttle = |entry: &Entry| match way {
            Direction::Read => entry.read,
            Direction::Write => entry.write,
        };
        for &slot in &slots {
            if let Some(entry) = table.get(&(slot, device.0, device.1)) {
                start = start.max(throttle(entry).earliest(now));
            }
        }
        for &slot in &slots {
            if let Some(entry) = table.get_mut(&(slot, device.0, device.1)) {
                match way {
                    Direction::Read => entry.read.commit(start, bytes),
                    Direction::Write => entry.write.commit(start, bytes),
                }
            }
        }
        start
    };
    if start > now {
        crate::sched::sleep_until(start);
    }
    let mut table = TABLE.lock();
    for &slot in &slots {
        if let Some(entry) = entry_of(&mut table, slot, device) {
            match way {
                Direction::Read => {
                    entry.stat.rbytes = entry.stat.rbytes.saturating_add(bytes);
                    entry.stat.rios = entry.stat.rios.saturating_add(1);
                }
                Direction::Write => {
                    entry.stat.wbytes = entry.stat.wbytes.saturating_add(bytes);
                    entry.stat.wios = entry.stat.wios.saturating_add(1);
                }
            }
        }
    }
}

/// What the job in `slot` and its descendants have counted, by disk.
pub(crate) fn stats(slot: u32) -> Vec<((u32, u32), Stat)> {
    TABLE
        .lock()
        .range((slot, 0, 0)..=(slot, u32::MAX, u32::MAX))
        .map(|(&(_, major, minor), entry)| ((major, minor), entry.stat))
        .collect()
}

/// A job's `io.max`, by disk with a limit.
pub(crate) fn limits(slot: u32) -> Vec<((u32, u32), Limits)> {
    TABLE
        .lock()
        .range((slot, 0, 0)..=(slot, u32::MAX, u32::MAX))
        .map(|(&(_, major, minor), entry)| ((major, minor), limits_of(entry)))
        .filter(|(_, limits)| *limits != Limits::NONE)
        .collect()
}

/// What an entry's limits are, as `io.max` prints them.
fn limits_of(entry: &Entry) -> Limits {
    Limits {
        rbps: entry.read.bytes.rate(),
        wbps: entry.write.bytes.rate(),
        riops: entry.read.ios.rate(),
        wiops: entry.write.ios.rate(),
    }
}

/// The limits the job in `slot` has on `device`, none if it has never had
/// any.
pub(crate) fn limits_on(slot: u32, device: (u32, u32)) -> Limits {
    TABLE
        .lock()
        .get(&(slot, device.0, device.1))
        .map_or(Limits::NONE, limits_of)
}

/// Set the limits of the job in `slot` on `device`.
///
/// # Errors
///
/// `ENOMEM` when the job cannot pay for the entry.
pub(crate) fn set_limits(slot: u32, device: (u32, u32), limits: Limits) -> Result<(), Errno> {
    let mut table = TABLE.lock();
    let entry = entry_of(&mut table, slot, device).ok_or(Errno::ENOMEM)?;
    entry.read.bytes.set_rate(limits.rbps);
    entry.write.bytes.set_rate(limits.wbps);
    entry.read.ios.set_rate(limits.riops);
    entry.write.ios.set_rate(limits.wiops);
    Ok(())
}

/// Forget a job's entries as it goes.
pub(crate) fn forget(slot: u32) {
    // Taken out before they are dropped: dropping an entry gives back a
    // charge, and is done with the table free.
    let gone: Vec<Entry> = {
        let mut table = TABLE.lock();
        let keys: Vec<(u32, u32, u32)> = table
            .range((slot, 0, 0)..=(slot, u32::MAX, u32::MAX))
            .map(|(&key, _)| key)
            .collect();
        keys.iter().filter_map(|key| table.remove(key)).collect()
    };
    drop(gone);
}

/// `disk`, numbered `major:minor`, with its reads and writes charged to the
/// job that makes them. The first disk so made starts the charging.
pub(crate) fn account_disk(
    disk: Arc<dyn BlockDevice>,
    major: u32,
    minor: u32,
) -> Arc<dyn BlockDevice> {
    quota::on_job_release(forget);
    Arc::new(Accounted::new(disk, major, minor))
}

/// A disk whose reads and writes are charged to the job that makes them.
#[derive(Debug)]
pub(crate) struct Accounted {
    /// The disk.
    inner: Arc<dyn BlockDevice>,
    /// Its device number's major half.
    major: u32,
    /// And minor half.
    minor: u32,
}

impl Accounted {
    /// `inner`, numbered `major:minor`.
    pub(crate) fn new(inner: Arc<dyn BlockDevice>, major: u32, minor: u32) -> Accounted {
        Accounted {
            inner,
            major,
            minor,
        }
    }
}

impl BlockDevice for Accounted {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        account((self.major, self.minor), Direction::Read, buf.len() as u64);
        self.inner.read(sector, buf)
    }

    fn sectors(&self) -> u64 {
        self.inner.sectors()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_only(&self) -> bool {
        self.inner.read_only()
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        account((self.major, self.minor), Direction::Write, buf.len() as u64);
        self.inner.write(sector, buf)
    }

    fn flush(&self) -> Result<(), Errno> {
        self.inner.flush()
    }
}
