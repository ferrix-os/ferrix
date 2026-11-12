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
//! # `io.weight` and `io.latency`
//!
//! While any cgroup has a weight other than the default or a latency target
//! ([`SHAPED`]), a disk serves one request at a time in a fair queue
//! (`ferrix_block::Fair`): a request costs its bytes scaled by the weights on
//! the way up (`ferrix_block::weighted_cost`), and the disk goes to the waiting request
//! with the least virtual finish, so cgroups that contend for it get it in
//! proportion to their weights. A request of a cgroup that is missing its
//! `io.latency` target on the disk, or under one that is, is urgent and goes
//! before any that is not: the cgroups beside it wait while it is behind, as
//! Linux's `blk-iolatency` throttles a missing group's siblings. A cgroup's
//! latency is the moving average, an eighth a request, of the time from
//! queueing to completion. A task waits for its own request, so a group often
//! has one request at a time: as BFQ idles, the disk is held for up to
//! [`HOLD_NANOS`] for the group just served when its next request would win
//! anyway, or the disk would alternate between groups whatever their weights.
//! With nothing set, requests go straight to the disk as before, with no
//! queue and no single request in flight.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_block::{Fair, Throttle, weighted_cost};
use ferrix_cgroupfs::io::{Limits, Stat};
use ferrix_kmem::Charge;
use ferrix_vfs::Errno;

use super::block::BlockDevice;
use crate::fallible;
use crate::object::quota;
use crate::sched::WaitQueue;
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

/// One job's `io.weight` and `io.latency` state for one disk, in a table of
/// its own ([`SHAPES`]): in [`Entry`] it made the table's nodes larger than
/// any size class the kernel's heap reserve holds.
#[derive(Debug)]
struct Shape {
    /// Its `io.weight` on this disk, 0 for the job's default.
    weight: u32,
    /// Its `io.latency` target on this disk, in nanoseconds; 0 for none.
    target: u64,
    /// The moving average of its requests' latency here, in nanoseconds.
    latency: u64,
    /// The virtual finish of its last request in the disk's fair queue.
    finish: u128,
    /// Its heap, charged to the job.
    _charge: Charge,
}

/// Every job's shape, by job slot, major and minor: made by a write to
/// `io.weight` or `io.latency`, or by a request while [`SHAPED`].
static SHAPES: SpinLock<BTreeMap<(u32, u32, u32), Shape>> = SpinLock::new(BTreeMap::new());

/// What a shape holds of the heap, at most.
const SHAPE_HEAP: usize = 2 * size_of::<(u32, u32, u32)>() + 3 * size_of::<Shape>();

/// The shape for `slot` and `device`, made if there is none.
fn shape_of(
    shapes: &mut BTreeMap<(u32, u32, u32), Shape>,
    slot: u32,
    device: (u32, u32),
) -> Option<&mut Shape> {
    let key = (slot, device.0, device.1);
    if !shapes.contains_key(&key) {
        if quota::job_ended(slot) {
            return None;
        }
        let charge = Charge::to(slot, SHAPE_HEAP).ok()?;
        let fresh = Shape {
            weight: 0,
            target: 0,
            latency: 0,
            finish: 0,
            _charge: charge,
        };
        let _ = fallible::insert(shapes, key, fresh).ok()?;
    }
    shapes.get_mut(&key)
}

/// Every entry, by job slot ([`quota::NONE`] for the machine), major and
/// minor.
static TABLE: SpinLock<BTreeMap<(u32, u32, u32), Entry>> = SpinLock::new(BTreeMap::new());

/// What an entry holds of the heap, at most: the node it is in and itself.
const ENTRY_HEAP: usize = 2 * size_of::<(u32, u32, u32)>() + 3 * size_of::<Entry>();

/// Each job's default `io.weight`, where it is not the default, and its
/// heap's charge, by quota slot.
static DEFAULTS: SpinLock<BTreeMap<u32, (u32, Charge)>> = SpinLock::new(BTreeMap::new());

/// What a default weight holds of the heap, at most.
const DEFAULT_HEAP: usize = 2 * size_of::<u32>() + 3 * size_of::<(u32, Charge)>();

/// How long a disk is held for the group it served last, when that group's
/// next request would win anyway (`ferrix_block::Fair`'s hold): 2 ms, time
/// for a task to queue its next request, where BFQ's `slice_idle` is 8 ms.
const HOLD_NANOS: u64 = 2_000_000;

/// Whether any job has an `io.weight` other than the default or an
/// `io.latency` target: until one has, a disk keeps no fair queue.
static SHAPED: AtomicBool = AtomicBool::new(false);

/// Whether a disk keeps a fair queue now ([`SHAPED`]).
pub(crate) fn shaped() -> bool {
    SHAPED.load(Ordering::Acquire)
}

/// Set [`SHAPED`] from the tables. Called with neither locked.
///
/// Lock order in this module: [`SHAPES`] before [`DEFAULTS`] where both are
/// held ([`reshape`], [`Accounted::in_turn`]); [`TABLE`] is never held with
/// either, and a disk's fair lock never with any of them.
fn reshape() {
    let shapes = SHAPES.lock();
    let any = shapes
        .values()
        .any(|shape| shape.weight != 0 || shape.target != 0)
        || !DEFAULTS.lock().is_empty();
    SHAPED.store(any, Ordering::Release);
}

/// The weight the job in `slot` has on `device`: its own there, else its
/// default.
fn weight_on(
    shapes: &BTreeMap<(u32, u32, u32), Shape>,
    defaults: &BTreeMap<u32, (u32, Charge)>,
    slot: u32,
    device: (u32, u32),
) -> u32 {
    shapes
        .get(&(slot, device.0, device.1))
        .map(|shape| shape.weight)
        .filter(|&weight| weight != 0)
        .or_else(|| defaults.get(&slot).map(|&(weight, _)| weight))
        .unwrap_or(ferrix_cgroupfs::io::WEIGHT_DEFAULT)
}

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
        // A task asleep in the kernel in a job that has gone: the release hook
        // has cleared its entries and will not run again, so one made now
        // would hold the slot, and every slot above it, for ever.
        if quota::job_ended(slot) {
            return None;
        }
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
    let shapes: Vec<Shape> = {
        let mut held = SHAPES.lock();
        let keys: Vec<(u32, u32, u32)> = held
            .range((slot, 0, 0)..=(slot, u32::MAX, u32::MAX))
            .map(|(&key, _)| key)
            .collect();
        keys.iter().filter_map(|key| held.remove(key)).collect()
    };
    let default = DEFAULTS.lock().remove(&slot);
    drop((gone, shapes, default));
    reshape();
}

/// The job in `slot`'s default `io.weight`.
pub(crate) fn default_weight(slot: u32) -> u32 {
    DEFAULTS
        .lock()
        .get(&slot)
        .map_or(ferrix_cgroupfs::io::WEIGHT_DEFAULT, |&(weight, _)| weight)
}

/// Set the job in `slot`'s default `io.weight`.
///
/// # Errors
///
/// `ENOMEM` when the job cannot pay for keeping it.
pub(crate) fn set_default_weight(slot: u32, weight: u32) -> Result<(), Errno> {
    let gone = if weight == ferrix_cgroupfs::io::WEIGHT_DEFAULT {
        DEFAULTS.lock().remove(&slot)
    } else {
        let charge = Charge::to(slot, DEFAULT_HEAP).map_err(|_| Errno::ENOMEM)?;
        let mut defaults = DEFAULTS.lock();
        if let Some(held) = defaults.get_mut(&slot) {
            held.0 = weight;
            drop(defaults);
            drop(charge);
        } else if quota::job_ended(slot) {
            // As `entry_of`: the release hook has run and would not again.
            drop(defaults);
            drop(charge);
        } else {
            let _ = fallible::insert(&mut defaults, slot, (weight, charge))
                .map_err(|_| Errno::ENOMEM)?;
        }
        None
    };
    drop(gone);
    reshape();
    Ok(())
}

/// The disks the job in `slot` has an `io.weight` of its own on.
pub(crate) fn weights(slot: u32) -> Vec<((u32, u32), u32)> {
    SHAPES
        .lock()
        .range((slot, 0, 0)..=(slot, u32::MAX, u32::MAX))
        .filter(|(_, shape)| shape.weight != 0)
        .map(|(&(_, major, minor), shape)| ((major, minor), shape.weight))
        .collect()
}

/// Set the job in `slot`'s `io.weight` on `device`, `None` for its default.
///
/// # Errors
///
/// `ENOMEM` when the job cannot pay for the entry.
pub(crate) fn set_weight(slot: u32, device: (u32, u32), weight: Option<u32>) -> Result<(), Errno> {
    {
        let mut shapes = SHAPES.lock();
        let shape = shape_of(&mut shapes, slot, device).ok_or(Errno::ENOMEM)?;
        shape.weight = weight.unwrap_or(0);
    }
    reshape();
    Ok(())
}

/// The job in `slot`'s `io.latency` targets, in microseconds, by disk.
pub(crate) fn latencies(slot: u32) -> Vec<((u32, u32), u64)> {
    SHAPES
        .lock()
        .range((slot, 0, 0)..=(slot, u32::MAX, u32::MAX))
        .filter(|(_, shape)| shape.target != 0)
        .map(|(&(_, major, minor), shape)| ((major, minor), shape.target / 1000))
        .collect()
}

/// The job in `slot`'s `io.latency` target on `device`, in microseconds.
pub(crate) fn latency_on(slot: u32, device: (u32, u32)) -> Option<u64> {
    SHAPES
        .lock()
        .get(&(slot, device.0, device.1))
        .map(|shape| shape.target / 1000)
        .filter(|&micros| micros != 0)
}

/// Set the job in `slot`'s `io.latency` target on `device`, in
/// microseconds, `None` for none.
///
/// # Errors
///
/// `ENOMEM` when the job cannot pay for the entry.
pub(crate) fn set_latency(slot: u32, device: (u32, u32), micros: Option<u64>) -> Result<(), Errno> {
    {
        let mut shapes = SHAPES.lock();
        let shape = shape_of(&mut shapes, slot, device).ok_or(Errno::ENOMEM)?;
        shape.target = micros.map_or(0, |micros| micros.saturating_mul(1000));
        shape.latency = 0;
    }
    reshape();
    Ok(())
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
    /// Its fair queue, while [`SHAPED`].
    fair: SpinLock<Fair>,
    /// Where requests wait their turn in it.
    turn: WaitQueue,
}

impl Accounted {
    /// `inner`, numbered `major:minor`.
    pub(crate) fn new(inner: Arc<dyn BlockDevice>, major: u32, minor: u32) -> Accounted {
        Accounted {
            inner,
            major,
            minor,
            fair: SpinLock::new(Fair::new()),
            turn: WaitQueue::new(),
        }
    }

    /// Make the running task's request of `bytes` with `io`, in its turn
    /// while [`SHAPED`], straight away otherwise or when there is no memory
    /// to queue it. Sleeps, so is called with no spin lock held, as
    /// [`account`] is.
    fn in_turn<T>(&self, bytes: u64, io: impl FnOnce() -> T) -> T {
        if !SHAPED.load(Ordering::Acquire) {
            return io();
        }
        let device = (self.major, self.minor);
        let slots = chain();
        let own = slots.first().copied().unwrap_or(quota::NONE);
        let (last, cost, urgent) = {
            let shapes = SHAPES.lock();
            let defaults = DEFAULTS.lock();
            // The weights from the submitter's job up, the machine's last:
            // the root has no weight, and charges none.
            let weights = slots
                .iter()
                .filter(|&&slot| slot != quota::NONE)
                .map(|&slot| weight_on(&shapes, &defaults, slot, device));
            let cost = weighted_cost(bytes, weights);
            let urgent = slots.iter().any(|&slot| {
                shapes
                    .get(&(slot, device.0, device.1))
                    .is_some_and(|shape| shape.target != 0 && shape.latency > shape.target)
            });
            let last = shapes
                .get(&(own, device.0, device.1))
                .map_or(0, |shape| shape.finish);
            (last, cost, urgent)
        };
        let Ok(ticket) = self.fair.lock().enqueue(u64::from(own), last, cost, urgent) else {
            return io();
        };
        // Kept where the group's next request finds it: a group with no
        // shape yet gets one, or starts again at the clock next time.
        if let Some(shape) = shape_of(&mut SHAPES.lock(), own, device) {
            shape.finish = ticket.finish;
        }
        let queued = crate::timer::now_nanos();
        loop {
            let now = crate::timer::now_nanos();
            let held_until = {
                let mut fair = self.fair.lock();
                if fair.may_go(ticket.id, now) {
                    let _ = fair.go(ticket.id);
                    break;
                }
                fair.held_until(now)
            };
            // A hold ends without a wake: a waiter it keeps out looks again
            // when it ends. One set after this looked is a change it wakes
            // for, so that it takes the new deadline rather than the wait's
            // own recheck.
            let _ = self.turn.wait_until_deadline(
                || {
                    let now = crate::timer::now_nanos();
                    let fair = self.fair.lock();
                    fair.may_go(ticket.id, now) || fair.held_until(now) != held_until
                },
                held_until.unwrap_or(u64::MAX),
            );
        }
        let done = io();
        {
            // Held for this group if its next request would win anyway: it
            // is urgent, or its next finish is below every waiting one.
            let mut fair = self.fair.lock();
            let next = ticket.finish.saturating_add(u128::from(cost));
            let wins = urgent || fair.least_finish().is_none_or(|least| next < least);
            let until = crate::timer::now_nanos().saturating_add(HOLD_NANOS);
            fair.done(wins.then_some((u64::from(own), until)));
        }
        self.turn.wake_all();
        let took = crate::timer::now_nanos().saturating_sub(queued);
        let mut shapes = SHAPES.lock();
        for &slot in &slots {
            if let Some(shape) = shapes.get_mut(&(slot, device.0, device.1))
                && shape.target != 0
            {
                shape.latency = if shape.latency == 0 {
                    took
                } else {
                    (shape.latency.saturating_mul(7).saturating_add(took)) / 8
                };
            }
        }
        done
    }
}

impl BlockDevice for Accounted {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        account((self.major, self.minor), Direction::Read, buf.len() as u64);
        self.in_turn(buf.len() as u64, || self.inner.read(sector, buf))
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

    fn offline(&self) -> bool {
        self.inner.offline()
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        account((self.major, self.minor), Direction::Write, buf.len() as u64);
        self.in_turn(buf.len() as u64, || self.inner.write(sector, buf))
    }

    fn flush(&self) -> Result<(), Errno> {
        self.inner.flush()
    }
}
