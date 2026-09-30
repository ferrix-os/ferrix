//! System V semaphores: `semget`, `semop`, `semtimedop` and `semctl`, and
//! i386's `ipc` entry to them.
//!
//! Built for the Steam client (2026-09-28, the customer's decision): its
//! thread tools make a one-semaphore set as a mutex between `steam` and its
//! update child, take it with `SEM_UNDO` so that a child that dies lets go,
//! and without these calls it waited on a futex forever. Before this file
//! every one of them was `ENOSYS`.
//!
//! # What is kept, and where
//!
//! [`TABLE`] holds every set by slot, the system's one key space: Ferrix has
//! no namespaces, so this is Linux's `init_ipc_ns`. A set is found by key
//! through `semget`, or by the id `semget` returned, which is its slot and a
//! sequence number, as Linux numbers them: a removed set's id is `EINVAL`
//! afterwards rather than someone else's set. Whether a caller may use a set
//! is decided by its `ipc_perm` alone ([`Perm::allows`]), as Linux's
//! `ipcperms` decides it.
//!
//! A set's values, its blocked callers and its undo records are behind the
//! set's own lock. An undo record is kept in the set rather than in the
//! process, so that an operation and the adjustment it owes change under one
//! lock; the process keeps only the ids of the sets it has records in
//! ([`UndoList`]), for its exit to visit ([`exit`]).
//!
//! # Locks
//!
//! Three kinds of [`SpinLock`], never one inside another: [`TABLE`], held to
//! find, add or take out a set and never longer; a set's lock; and a
//! process's [`UndoList`]. Under a set's lock nothing is taken but the
//! scheduler's run queues, by [`sched::wake`], as `futex`'s buckets do. No
//! user memory is touched under any of them: what a call reads is copied in
//! first, and what it answers is copied out after.
//!
//! # A blocked `semop`
//!
//! A caller that cannot complete is queued on the set with a [`Ticket`] and
//! sleeps. Every change of a set's values runs its queue in arrival order
//! ([`run_queue`]), completing each caller that now can, on its behalf, and
//! waking it with its result in the ticket. A caller leaving for any other
//! reason -- a signal, its timeout, its set removed -- takes the set's lock
//! and looks at its ticket again first, so that a completion which raced it
//! wins and is never lost.
//!
//! # What a job pays for (F-37)
//!
//! A set is charged to the job of the task that made it, and stays charged
//! there until `IPC_RMID` takes it out -- a set outlives its maker, as on
//! Linux, which charges it to the maker's memory cgroup. An undo record is
//! charged to the job of the process it belongs to, and a blocked caller's
//! record to the caller's job, each given back with what it pays for. A job
//! may also hold at most [`SETS_PER_JOB`] sets: the id space is the system's,
//! and without it one job could take every id and leave its siblings none.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use ferrix_kmem::Charge;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;

use crate::arch::{self, StatLayout};
use crate::sched::{self, Task, WaitQueue};
use crate::sync::SpinLock;
use crate::syscall::credentials;
use crate::syscall::process::{self, Process};
use crate::syscall::time::{self, TimeWidth};
use crate::syscall::uaccess;
use crate::trap::Abi;

// ---------------------------------------------------------------------------
// The ABI: `include/uapi/linux/ipc.h` and `include/uapi/linux/sem.h`.
// ---------------------------------------------------------------------------

/// The key that always makes a new set.
const IPC_PRIVATE: i32 = 0;
/// Make the set if its key has none.
const IPC_CREAT: i32 = 0o1000;
/// With [`IPC_CREAT`]: fail if its key has one.
const IPC_EXCL: i32 = 0o2000;
/// On an operation: fail with `EAGAIN` rather than wait.
const IPC_NOWAIT: i16 = 0o4000;
/// On an operation: undo it when the process ends.
const SEM_UNDO: i16 = 0x1000;
/// The layout flag a 32-bit caller's `semctl` command carries for the
/// `ipc64_perm` structures, as glibc and musl always pass it.
const IPC_64: i32 = 0x0100;

/// Remove the set.
const IPC_RMID: i32 = 0;
/// Set its owner and mode.
const IPC_SET: i32 = 1;
/// Read its `semid64_ds`.
const IPC_STAT: i32 = 2;
/// Read the limits, as a `seminfo`.
const IPC_INFO: i32 = 3;
/// Read one semaphore's last operator.
const GETPID: i32 = 11;
/// Read one semaphore's value.
const GETVAL: i32 = 12;
/// Read every value.
const GETALL: i32 = 13;
/// Count the callers waiting for a value to rise.
const GETNCNT: i32 = 14;
/// Count the callers waiting for a value to be zero.
const GETZCNT: i32 = 15;
/// Set one value.
const SETVAL: i32 = 16;
/// Set every value.
const SETALL: i32 = 17;
/// [`IPC_STAT`] by slot, answering the set's id.
const SEM_STAT: i32 = 18;
/// [`IPC_INFO`] with what is in use.
const SEM_INFO: i32 = 19;
/// [`SEM_STAT`] without the read permission.
const SEM_STAT_ANY: i32 = 20;

/// i386 `ipc` operations, from `linux/ipc.h`.
const IPCOP_SEMOP: u32 = 1;
/// See [`IPCOP_SEMOP`].
const IPCOP_SEMGET: u32 = 2;
/// See [`IPCOP_SEMOP`].
const IPCOP_SEMCTL: u32 = 3;
/// See [`IPCOP_SEMOP`].
const IPCOP_SEMTIMEDOP: u32 = 4;

/// Semaphores in one set: Linux's `SEMMSL`.
pub(crate) const SEMMSL: usize = 32_000;
/// Operations in one call: Linux's `SEMOPM`.
pub(crate) const SEMOPM: usize = 500;
/// The largest value: Linux's `SEMVMX`.
pub(crate) const SEMVMX: i32 = 32_767;
/// The largest undo adjustment: Linux's `SEMAEM`.
const SEMAEM: i32 = SEMVMX;
/// Sets one job may hold, Linux's `SEMMNI` as a per-job bound (see the
/// module documentation).
pub(crate) const SETS_PER_JOB: usize = 32_000;
/// Semaphores the system may hold: Linux's `SEMMNS`, `SEMMNI * SEMMSL`.
const SEMMNS: usize = 1_024_000_000;
/// `SEMUSZ`, which `IPC_INFO` reports and nothing uses.
const SEMUSZ: i32 = 20;

/// Bits of an id that are its slot. Linux's extended `IPCMNI`, 2^24, rather
/// than its default 32,768, so that the slots outlast every job's
/// [`SETS_PER_JOB`] together; what is left above them, seven bits, is the
/// slot's sequence number.
const SLOT_BITS: u32 = 24;
/// Slots in [`TABLE`].
const SLOTS: usize = 1 << SLOT_BITS;
/// The sequence numbers an id carries, keeping ids positive.
const SEQ_MASK: u32 = 0x7F;

/// Bytes in one `struct sembuf`, on every architecture.
const SEMBUF_BYTES: usize = 6;
/// Bytes in `struct seminfo`: ten `int`s.
const SEMINFO_BYTES: usize = 40;

// ---------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------

/// One operation of a `semop` call: `struct sembuf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SemBuf {
    /// Which semaphore of the set.
    pub(crate) num: u16,
    /// Added to its value, or zero to wait for it to be zero.
    pub(crate) op: i16,
    /// [`IPC_NOWAIT`] and [`SEM_UNDO`].
    pub(crate) flags: i16,
}

impl SemBuf {
    /// An operation of `op` on semaphore `num` with `flags`: for the checks.
    pub(crate) const fn new(num: u16, op: i16, flags: i16) -> SemBuf {
        SemBuf { num, op, flags }
    }

    /// Whether it asks to be undone at exit.
    const fn undoes(self) -> bool {
        self.flags & SEM_UNDO != 0
    }
}

/// The flag values the checks pass.
pub(crate) mod flags {
    /// `IPC_NOWAIT`.
    pub(crate) const NOWAIT: i16 = super::IPC_NOWAIT;
    /// `SEM_UNDO`.
    pub(crate) const UNDO: i16 = super::SEM_UNDO;
    /// `IPC_CREAT`.
    pub(crate) const CREAT: i32 = super::IPC_CREAT;
    /// `IPC_EXCL`.
    pub(crate) const EXCL: i32 = super::IPC_EXCL;
}

/// Who is calling, as a set's permissions judge it: read once, before any
/// lock is taken.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    /// Its process id, which a semaphore records as its last operator.
    pub(crate) pid: u32,
    /// Its effective user id.
    pub(crate) uid: u32,
    /// Its filesystem group id, which Linux's `in_group_p` compares.
    pub(crate) gid: u32,
    /// Its supplementary groups.
    pub(crate) groups: Vec<u32>,
    /// Whether it is root, standing in for `CAP_IPC_OWNER` and
    /// `CAP_SYS_ADMIN`.
    pub(crate) privileged: bool,
}

impl Caller {
    /// `process`, as it is now.
    pub(crate) fn of(process: &Process) -> Caller {
        let pid = process.pid();
        process.with_credentials(|ids| Caller {
            pid,
            uid: ids.user.effective,
            gid: ids.group.filesystem,
            groups: ids.groups.clone(),
            privileged: ids.privileged(),
        })
    }

    /// Whether it is in group `gid`.
    fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }
}

/// A set's `ipc_perm`, less its key and sequence number, which never change.
#[derive(Debug, Clone, Copy)]
struct Perm {
    /// The owner.
    uid: u32,
    /// The owner's group.
    gid: u32,
    /// The creator.
    cuid: u32,
    /// The creator's group.
    cgid: u32,
    /// The nine permission bits.
    mode: u32,
}

/// Read permission, in a class's three bits.
const READ: u32 = 4;
/// Alter permission: Linux's `S_IWUGO` for a semaphore.
const ALTER: u32 = 2;

impl Perm {
    /// Linux's `ipcperms`: whether `caller` has every bit of `wanted` in the
    /// class it falls in, owner, group or other; root always has.
    fn allows(&self, caller: &Caller, wanted: u32) -> bool {
        let granted = if caller.uid == self.uid || caller.uid == self.cuid {
            self.mode >> 6
        } else if caller.in_group(self.gid) || caller.in_group(self.cgid) {
            self.mode >> 3
        } else {
            self.mode
        };
        wanted & !granted & 0o7 == 0 || caller.privileged
    }

    /// Linux's `ipcctl_obtain_check`: whether `caller` may change or remove
    /// the set -- its owner or creator, or root.
    fn owned_by(&self, caller: &Caller) -> bool {
        caller.uid == self.uid || caller.uid == self.cuid || caller.privileged
    }
}

/// One semaphore.
#[derive(Debug, Clone, Copy, Default)]
struct Semaphore {
    /// Its value, 0 to [`SEMVMX`].
    value: i32,
    /// The process that last changed it.
    pid: u32,
}

/// What a process owes one set at its exit.
#[derive(Debug)]
struct Undo {
    /// The process's [`UndoList::owner`].
    owner: u64,
    /// For each semaphore, what to add to it.
    adjust: Vec<i16>,
    /// Its heap, charged to the process's job.
    #[allow(dead_code, reason = "held for its drop, which gives the charge back")]
    charge: Charge,
}

/// How a queued caller's call ended, shared by it and whoever ends it.
#[derive(Debug)]
struct Ticket {
    /// [`WAITING`], or the call's answer: zero, or a negated errno.
    state: AtomicI32,
    /// The caller's task, woken when the state is set.
    task: Option<Arc<Task>>,
}

/// A [`Ticket`] nobody has answered yet.
const WAITING: i32 = i32::MIN;

impl Ticket {
    /// Answer it with `result` and wake its caller. With the set's lock held.
    fn answer(&self, result: Result<(), Errno>) {
        let value = match result {
            Ok(()) => 0,
            Err(error) => -i32::from(error.0),
        };
        self.state.store(value, Ordering::Release);
        if let Some(task) = &self.task {
            sched::wake(task);
        }
    }
}

/// A caller queued on a set.
#[derive(Debug)]
struct Pending {
    /// Its operations.
    sops: Vec<SemBuf>,
    /// Its process's [`UndoList::owner`].
    owner: u64,
    /// Its process id.
    pid: u32,
    /// The semaphore and kind of the operation it is waiting at: `true` for
    /// one waiting for zero, which [`GETZCNT`] counts, `false` for one
    /// waiting to decrement, which [`GETNCNT`] counts.
    blocked: (u16, bool),
    /// Where its answer goes.
    ticket: Arc<Ticket>,
    /// Its heap, charged to its job.
    #[allow(dead_code, reason = "held for its drop, which gives the charge back")]
    charge: Charge,
}

/// A set's changing part.
#[derive(Debug)]
struct State {
    /// Set by `IPC_RMID`: every caller still holding the set is told `EIDRM`.
    removed: bool,
    /// Its owner and mode.
    perm: Perm,
    /// Its semaphores.
    sems: Vec<Semaphore>,
    /// The real time of its last `semop`, in seconds; zero before one.
    otime: i64,
    /// The real time it was made or last changed by `semctl`, in seconds.
    ctime: i64,
    /// Its blocked callers, oldest first.
    queue: Vec<Pending>,
    /// What each process that used [`SEM_UNDO`] on it owes it.
    undos: Vec<Undo>,
}

/// One semaphore set.
#[derive(Debug)]
struct Set {
    /// Its id, as `semget` answered it.
    id: i32,
    /// Its key.
    key: i32,
    /// How many semaphores it has.
    nsems: usize,
    /// The job that pays for it: [`Charge::owner`] of `charge`.
    job: u32,
    /// Its heap, charged to the job that made it.
    #[allow(dead_code, reason = "held for its drop, which gives the charge back")]
    charge: Charge,
    /// Everything that changes.
    state: SpinLock<State>,
}

/// One slot of [`TABLE`].
#[derive(Debug, Default)]
struct Slot {
    /// The sequence number the next set made here takes.
    seq: u32,
    /// The set in it.
    set: Option<Arc<Set>>,
}

/// Every set, by slot.
#[derive(Debug)]
struct Table {
    /// The slots, grown as they are needed.
    slots: Vec<Slot>,
    /// Sets in use.
    sets: usize,
    /// Semaphores in those sets.
    sems: usize,
}

/// The system's sets.
static TABLE: SpinLock<Table> = SpinLock::new(Table {
    slots: Vec::new(),
    sets: 0,
    sems: 0,
});

/// What a blocked caller sleeps on. Nobody wakes it as a whole: a caller's
/// task is woken by name, as `futex`'s are.
static SLEEP: WaitQueue = WaitQueue::new();

/// How long a blocked caller sleeps between looks of its own, trusting its
/// wakes as `futex` does.
const RECHECK_NANOS: u64 = 1_000_000_000;

/// The next [`UndoList::owner`].
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// The sets a process has undo records in, and the name those records know
/// it by. Shared by the process's threads, as `CLONE_SYSVSEM` shares it;
/// a fork child starts with none, and `execve` keeps it, as POSIX says.
#[derive(Debug)]
pub(crate) struct UndoList {
    /// Unique to the process for the machine's lifetime, so a record is
    /// never taken for another's when a pid is reused.
    owner: u64,
    /// The sets' ids, each with the charge for its entry.
    sets: SpinLock<Vec<(i32, Charge)>>,
}

impl UndoList {
    /// An empty list with an owner nobody else has.
    pub(crate) fn new() -> UndoList {
        UndoList {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            sets: SpinLock::new(Vec::new()),
        }
    }

    /// Record that the process has an undo record in set `id`, once.
    fn note(&self, id: i32) -> Result<(), Errno> {
        if self.sets.lock().iter().any(|(held, _)| *held == id) {
            return Ok(());
        }
        let charge = Charge::bytes(size_of::<(i32, Charge)>()).map_err(|_| Errno::ENOMEM)?;
        let mut sets = self.sets.lock();
        if !sets.iter().any(|(held, _)| *held == id) {
            sets.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
            sets.push((id, charge));
        }
        Ok(())
    }
}

impl Default for UndoList {
    fn default() -> UndoList {
        UndoList::new()
    }
}

/// Seconds of real time, for `sem_otime` and `sem_ctime`.
fn now_seconds() -> i64 {
    i64::try_from(time::realtime_nanos() / 1_000_000_000).unwrap_or(i64::MAX)
}

/// The slot an id names, and the sequence number it carries.
fn split_id(id: i32) -> Option<(usize, u32)> {
    let id = u32::try_from(id).ok()?;
    Some(((id & (SLOTS as u32 - 1)) as usize, id >> SLOT_BITS))
}

/// The set with id `id`, if there is one.
fn lookup(id: i32) -> Option<Arc<Set>> {
    let (slot, _) = split_id(id)?;
    let table = TABLE.lock();
    let set = table.slots.get(slot)?.set.as_ref()?;
    (set.id == id).then(|| Arc::clone(set))
}

// ---------------------------------------------------------------------------
// semget
// ---------------------------------------------------------------------------

/// `semget(key, nsems, flags)`: the id of the set `key` names, made if
/// `flags` asks and it has none; a new set every time for [`IPC_PRIVATE`].
///
/// # Errors
///
/// `EINVAL` for `nsems` below zero or past [`SEMMSL`], zero for a new set,
/// or more than an existing set has; `ENOENT` for a key with no set and no
/// [`IPC_CREAT`]; `EEXIST` for one with a set and both flags; `EACCES` for
/// a set whose mode refuses `flags`' permission bits; `ENOSPC` for a job
/// with [`SETS_PER_JOB`] sets, or a system out of slots or semaphores;
/// `ENOMEM` for a job at its memory limit.
pub(crate) fn semget(caller: &Caller, key: i32, nsems: i32, flags: i32) -> Result<i32, Errno> {
    semget_capped(caller, key, nsems, flags, SETS_PER_JOB)
}

/// [`semget`], with `per_job` in place of [`SETS_PER_JOB`]: for the check,
/// which cannot make thirty-two thousand sets to show the bound.
///
/// # Errors
///
/// As [`semget`].
pub(crate) fn semget_capped(
    caller: &Caller,
    key: i32,
    nsems: i32,
    flags: i32,
    per_job: usize,
) -> Result<i32, Errno> {
    let nsems = usize::try_from(nsems).map_err(|_| Errno::EINVAL)?;
    if nsems > SEMMSL {
        return Err(Errno::EINVAL);
    }
    if key == IPC_PRIVATE {
        return create(caller, key, nsems, flags, per_job);
    }
    loop {
        let found = {
            let table = TABLE.lock();
            table
                .slots
                .iter()
                .filter_map(|slot| slot.set.as_ref())
                .find(|set| set.key == key)
                .map(Arc::clone)
        };
        let Some(set) = found else {
            if flags & IPC_CREAT == 0 {
                return Err(Errno::ENOENT);
            }
            match create(caller, key, nsems, flags, per_job) {
                // Another caller made one for the key in between: look again.
                Err(Errno::EEXIST) if flags & IPC_EXCL == 0 => continue,
                other => return other,
            }
        };
        if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
            return Err(Errno::EEXIST);
        }
        if nsems > set.nsems {
            return Err(Errno::EINVAL);
        }
        let state = set.state.lock();
        if state.removed {
            // Removed since it was found: the key is free again.
            continue;
        }
        let wanted = (flags as u32 & 0o777) >> 6 | (flags as u32 & 0o777) >> 3 | flags as u32;
        if !state.perm.allows(caller, wanted & 0o7) {
            return Err(Errno::EACCES);
        }
        return Ok(set.id);
    }
}

/// Make a set of `nsems` semaphores under `key`, charged to the running
/// task's job. `EEXIST` if `key` is not [`IPC_PRIVATE`] and a set took it
/// in the meantime.
fn create(
    caller: &Caller,
    key: i32,
    nsems: usize,
    flags: i32,
    per_job: usize,
) -> Result<i32, Errno> {
    if nsems == 0 {
        return Err(Errno::EINVAL);
    }
    let bytes = ferrix_kmem::arc_footprint::<Set>()
        .saturating_add(ferrix_kmem::buffer_footprint::<Semaphore>(nsems))
        .saturating_add(size_of::<Slot>());
    let charge = Charge::bytes(bytes).map_err(|_| Errno::ENOMEM)?;
    let mut sems = Vec::new();
    sems.try_reserve_exact(nsems).map_err(|_| Errno::ENOMEM)?;
    sems.resize(nsems, Semaphore::default());
    let mode = flags as u32 & 0o777;
    let perm = Perm {
        uid: caller.uid,
        gid: caller.gid,
        cuid: caller.uid,
        cgid: caller.gid,
        mode,
    };
    let state = State {
        removed: false,
        perm,
        sems,
        otime: 0,
        ctime: now_seconds(),
        queue: Vec::new(),
        undos: Vec::new(),
    };
    let job = charge.owner();
    let mut table = TABLE.lock();
    if key != IPC_PRIVATE
        && table
            .slots
            .iter()
            .filter_map(|slot| slot.set.as_ref())
            .any(|set| set.key == key)
    {
        return Err(Errno::EEXIST);
    }
    let held = table
        .slots
        .iter()
        .filter_map(|slot| slot.set.as_ref())
        .filter(|set| set.job == job)
        .count();
    if held >= per_job || table.sems.saturating_add(nsems) > SEMMNS {
        return Err(Errno::ENOSPC);
    }
    let index = match table.slots.iter().position(|slot| slot.set.is_none()) {
        Some(index) => index,
        None if table.slots.len() < SLOTS => {
            table.slots.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
            table.slots.push(Slot::default());
            table.slots.len() - 1
        }
        None => return Err(Errno::ENOSPC),
    };
    let slot = table.slots.get_mut(index).ok_or(Errno::ENOSPC)?;
    let id = ((slot.seq & SEQ_MASK) << SLOT_BITS | index as u32) as i32;
    slot.seq = slot.seq.wrapping_add(1) & SEQ_MASK;
    let set = crate::fallible::try_arc(Set {
        id,
        key,
        nsems,
        job,
        charge,
        state: SpinLock::new(state),
    })
    .map_err(|_| Errno::ENOMEM)?;
    slot.set = Some(set);
    table.sets += 1;
    table.sems = table.sems.saturating_add(nsems);
    Ok(id)
}

// ---------------------------------------------------------------------------
// semop
// ---------------------------------------------------------------------------

/// What trying a caller's operations came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// All of them done.
    Done,
    /// None done: operation on semaphore `.0` cannot go yet, waiting for
    /// zero if `.1`.
    Blocked(u16, bool),
    /// None done, and the call fails.
    Failed(Errno),
}

/// Try `sops` all at once on `sems`, for the process whose undo owner is
/// `owner`: every one is done, or none is. Linux's `perform_atomic_semop`.
///
/// An operation with [`IPC_NOWAIT`] that cannot go fails the call with
/// `EAGAIN`; a value or an adjustment past its limit fails it with `ERANGE`;
/// one with [`SEM_UNDO`] and no undo record fails it with `EIDRM`, which
/// only a record gone with its set can cause.
fn attempt(sems: &mut [Semaphore], undos: &mut [Undo], sops: &[SemBuf], owner: u64) -> Attempt {
    let mut undo = undos.iter_mut().find(|undo| undo.owner == owner);
    for (done, sop) in sops.iter().enumerate() {
        let num = usize::from(sop.num);
        let op = i32::from(sop.op);
        let Some(current) = sems.get(num).map(|sem| sem.value) else {
            revert(sems, undo.as_deref_mut(), sops.get(..done).unwrap_or(&[]));
            return Attempt::Failed(Errno::EFBIG);
        };
        let result = current + op;
        let blocked = (op == 0 && current != 0) || result < 0;
        let failure = if blocked {
            Some(if sop.flags & IPC_NOWAIT != 0 {
                Attempt::Failed(Errno::EAGAIN)
            } else {
                Attempt::Blocked(sop.num, op == 0)
            })
        } else if result > SEMVMX {
            Some(Attempt::Failed(Errno::ERANGE))
        } else if sop.undoes() {
            match undo.as_deref().and_then(|undo| undo.adjust.get(num)) {
                None => Some(Attempt::Failed(Errno::EIDRM)),
                Some(&adjust) if !(-SEMAEM - 1..=SEMAEM).contains(&(i32::from(adjust) - op)) => {
                    Some(Attempt::Failed(Errno::ERANGE))
                }
                Some(_) => None,
            }
        } else {
            None
        };
        if let Some(failure) = failure {
            revert(sems, undo.as_deref_mut(), sops.get(..done).unwrap_or(&[]));
            return failure;
        }
        if let Some(sem) = sems.get_mut(num) {
            sem.value = result;
        }
        if sop.undoes()
            && let Some(adjust) = undo
                .as_deref_mut()
                .and_then(|undo| undo.adjust.get_mut(num))
        {
            *adjust = (i32::from(*adjust) - op) as i16;
        }
    }
    Attempt::Done
}

/// Take back `done`, the operations [`attempt`] had made before one
/// could not go, last first.
fn revert(sems: &mut [Semaphore], mut undo: Option<&mut Undo>, done: &[SemBuf]) {
    for sop in done.iter().rev() {
        let num = usize::from(sop.num);
        let op = i32::from(sop.op);
        if let Some(sem) = sems.get_mut(num) {
            sem.value -= op;
        }
        if sop.undoes()
            && let Some(adjust) = undo
                .as_deref_mut()
                .and_then(|undo| undo.adjust.get_mut(num))
        {
            *adjust = (i32::from(*adjust) + op) as i16;
        }
    }
}

/// Record a completed call's operator on each semaphore it named, and the
/// time.
fn completed(state: &mut State, sops: &[SemBuf], pid: u32) {
    for sop in sops {
        if let Some(sem) = state.sems.get_mut(usize::from(sop.num)) {
            sem.pid = pid;
        }
    }
    state.otime = now_seconds();
}

/// Complete every queued caller that can go now, oldest first, until a pass
/// completes none: Linux's `update_queue`. Each one completed or failed is
/// answered and woken, and its record dropped with the lock still held --
/// a drop that gives a charge back takes no lock.
fn run_queue(state: &mut State) {
    loop {
        let mut progressed = false;
        let mut index = 0;
        while let Some(pending) = state.queue.get(index) {
            let (sops, owner) = (pending.sops.as_slice(), pending.owner);
            let outcome = attempt(&mut state.sems, &mut state.undos, sops, owner);
            let result = match outcome {
                Attempt::Blocked(num, zero) => {
                    if let Some(pending) = state.queue.get_mut(index) {
                        pending.blocked = (num, zero);
                    }
                    index += 1;
                    continue;
                }
                Attempt::Done => Ok(()),
                Attempt::Failed(error) => Err(error),
            };
            let pending = state.queue.remove(index);
            if result.is_ok() {
                completed(state, &pending.sops, pending.pid);
                progressed = true;
            }
            pending.ticket.answer(result);
        }
        if !progressed {
            return;
        }
    }
}

/// How a blocked `semop` is told to give up besides its deadline: a signal
/// for it, or its process ending. The checks pass one of their own.
pub(crate) trait Interrupt {
    /// Whether it should stop waiting.
    fn interrupted(&self) -> bool;
}

impl Interrupt for Process {
    fn interrupted(&self) -> bool {
        use crate::object::process::Host;
        self.wait_interrupted() || self.is_terminated()
    }
}

/// `semtimedop(id, sops, nsops, timeout)` on operations already read in:
/// done all together, waiting until they can be if none says
/// [`IPC_NOWAIT`], until `deadline` on the counter if there is one.
///
/// # Errors
///
/// `EINVAL` for an id with no set; `EFBIG` for an operation on a semaphore
/// the set does not have; `EACCES` without read permission, or alter
/// permission for an operation that changes a value; `EAGAIN` for an
/// operation with [`IPC_NOWAIT`] that cannot go, or a deadline passed;
/// `ERANGE` for a value or an adjustment past its limit; `EIDRM` for a set
/// removed while the caller waited; `EINTR` for a caller interrupted, never
/// restarted, as Linux's `semop` never is; `ENOMEM` for a job at its limit.
pub(crate) fn semop(
    caller: &Caller,
    undo_list: &UndoList,
    id: i32,
    sops: Vec<SemBuf>,
    deadline: Option<u64>,
    interrupt: &dyn Interrupt,
) -> Result<usize, Errno> {
    let set = lookup(id).ok_or(Errno::EINVAL)?;
    let highest = sops
        .iter()
        .map(|sop| usize::from(sop.num))
        .max()
        .unwrap_or(0);
    let alters = sops.iter().any(|sop| sop.op != 0);
    let undoes = sops.iter().any(|sop| sop.undoes());
    if highest >= set.nsems {
        return Err(Errno::EFBIG);
    }
    {
        let state = set.state.lock();
        if state.removed {
            return Err(Errno::EIDRM);
        }
        if !state.perm.allows(caller, if alters { ALTER } else { READ }) {
            return Err(Errno::EACCES);
        }
    }
    if undoes {
        make_undo(&set, undo_list)?;
    }
    let bytes = size_of::<Pending>()
        .saturating_add(ferrix_kmem::buffer_footprint::<SemBuf>(sops.len()))
        .saturating_add(ferrix_kmem::arc_footprint::<Ticket>());
    let charge = Charge::bytes(bytes).map_err(|_| Errno::ENOMEM)?;
    let ticket = crate::fallible::try_arc(Ticket {
        state: AtomicI32::new(WAITING),
        task: sched::current(),
    })
    .map_err(|_| Errno::ENOMEM)?;
    {
        let mut state = set.state.lock();
        if state.removed {
            return Err(Errno::EIDRM);
        }
        let state = &mut *state;
        match attempt(&mut state.sems, &mut state.undos, &sops, undo_list.owner) {
            Attempt::Done => {
                completed(state, &sops, caller.pid);
                if alters {
                    run_queue(state);
                }
                return Ok(0);
            }
            Attempt::Failed(error) => return Err(error),
            Attempt::Blocked(num, zero) => {
                state.queue.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
                state.queue.push(Pending {
                    sops,
                    owner: undo_list.owner,
                    pid: caller.pid,
                    blocked: (num, zero),
                    ticket: Arc::clone(&ticket),
                    charge,
                });
            }
        }
    }
    wait(&set, &ticket, deadline, interrupt)
}

/// Make sure `set` holds an undo record for `undo_list`'s process, and that
/// the process lists the set: before the call's operations are tried, so a
/// queued caller's completion never has to allocate on its behalf.
fn make_undo(set: &Arc<Set>, undo_list: &UndoList) -> Result<(), Errno> {
    undo_list.note(set.id)?;
    if set
        .state
        .lock()
        .undos
        .iter()
        .any(|undo| undo.owner == undo_list.owner)
    {
        return Ok(());
    }
    let bytes = size_of::<Undo>().saturating_add(ferrix_kmem::buffer_footprint::<i16>(set.nsems));
    let charge = Charge::bytes(bytes).map_err(|_| Errno::ENOMEM)?;
    let mut adjust = Vec::new();
    adjust
        .try_reserve_exact(set.nsems)
        .map_err(|_| Errno::ENOMEM)?;
    adjust.resize(set.nsems, 0);
    let mut state = set.state.lock();
    if state.removed {
        return Err(Errno::EIDRM);
    }
    if !state.undos.iter().any(|undo| undo.owner == undo_list.owner) {
        state.undos.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        state.undos.push(Undo {
            owner: undo_list.owner,
            adjust,
            charge,
        });
    }
    Ok(())
}

/// Sleep until `ticket` is answered, the caller is interrupted or `deadline`
/// passes, and answer the call.
fn wait(
    set: &Arc<Set>,
    ticket: &Arc<Ticket>,
    deadline: Option<u64>,
    interrupt: &dyn Interrupt,
) -> Result<usize, Errno> {
    let deadline = deadline.unwrap_or(u64::MAX);
    loop {
        let _ = WaitQueue::wait_on_any(
            &[&SLEEP],
            || ticket.state.load(Ordering::Acquire) != WAITING || interrupt.interrupted(),
            deadline,
            RECHECK_NANOS,
        );
        let interrupted = interrupt.interrupted();
        let expired = crate::timer::now_nanos() >= deadline;
        // Under the set's lock, so an answer given before this look is seen,
        // and none can be given after it once the record is out.
        let mut state = set.state.lock();
        let answer = ticket.state.load(Ordering::Acquire);
        if answer != WAITING {
            return if answer == 0 {
                Ok(0)
            } else {
                Err(Errno(u16::try_from(-answer).unwrap_or(Errno::EINVAL.0)))
            };
        }
        if !interrupted && !expired {
            // Woken for nothing: still queued, sleep again.
            continue;
        }
        let mine = state
            .queue
            .iter()
            .position(|pending| Arc::ptr_eq(&pending.ticket, ticket))
            .map(|index| state.queue.remove(index));
        drop(state);
        drop(mine);
        return Err(if interrupted {
            Errno::EINTR
        } else {
            Errno::EAGAIN
        });
    }
}

// ---------------------------------------------------------------------------
// semctl
// ---------------------------------------------------------------------------

/// Which `semid64_ds` a caller reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// x86-64's, from `arch/x86/include/uapi/asm/sembuf.h`: 104 bytes, a
    /// padding word after each time. x86-64 kept its own as it kept its own
    /// `struct stat`, which is how the facade's [`StatLayout`] tells it.
    X86_64,
    /// The generic 64-bit one, `asm-generic/sembuf.h`: 88 bytes. AArch64's.
    Generic64,
    /// Every 32-bit one, i386's and ARMv7-A's: 64 bytes, each time in two
    /// 32-bit halves, after a 36-byte `ipc64_perm` with a 16-bit mode.
    Narrow,
}

impl Layout {
    /// The layout a call that came in by `abi` uses.
    pub(crate) fn of(abi: Abi) -> Layout {
        if abi == Abi::Compat || size_of::<usize>() == 4 {
            Layout::Narrow
        } else if arch::STAT_LAYOUT == StatLayout::Legacy {
            Layout::X86_64
        } else {
            Layout::Generic64
        }
    }

    /// Bytes in its `semid64_ds`.
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Layout::X86_64 => 104,
            Layout::Generic64 => 88,
            Layout::Narrow => 64,
        }
    }

    /// Offsets of `sem_otime`, `sem_ctime` and `sem_nsems`, and whether each
    /// field past the `ipc64_perm` is 32 bits.
    const fn fields(self) -> (usize, usize, usize, bool) {
        match self {
            Layout::X86_64 => (48, 64, 80, false),
            Layout::Generic64 => (48, 56, 64, false),
            Layout::Narrow => (36, 44, 52, true),
        }
    }
}

/// A set's `semid64_ds`, in `layout`. The `ipc64_perm` is the same bytes at
/// both widths up to its sequence number: a 16-bit mode is followed by two
/// bytes of padding, which a 32-bit mode below 65,536 leaves zero.
fn encode_semid(set: &Set, state: &State, layout: Layout) -> Vec<u8> {
    let mut out = Vec::new();
    if out.try_reserve_exact(layout.bytes()).is_err() {
        return out;
    }
    out.resize(layout.bytes(), 0);
    let seq = split_id(set.id).map_or(0, |(_, seq)| seq);
    let mut put = |at: usize, bytes: &[u8]| {
        if let Some(to) = out.get_mut(at..at + bytes.len()) {
            to.copy_from_slice(bytes);
        }
    };
    put(0, &set.key.to_le_bytes());
    put(4, &credentials::show_uid(state.perm.uid).to_le_bytes());
    put(8, &credentials::show_gid(state.perm.gid).to_le_bytes());
    put(12, &credentials::show_uid(state.perm.cuid).to_le_bytes());
    put(16, &credentials::show_gid(state.perm.cgid).to_le_bytes());
    put(20, &state.perm.mode.to_le_bytes());
    put(24, &(seq as u16).to_le_bytes());
    let (otime, ctime, nsems, narrow) = layout.fields();
    // Each time as eight bytes: a 64-bit field, or a 32-bit one's low half
    // and then its `_high` half.
    put(otime, &state.otime.to_le_bytes());
    put(ctime, &state.ctime.to_le_bytes());
    if narrow {
        put(nsems, &(set.nsems as u32).to_le_bytes());
    } else {
        put(nsems, &(set.nsems as u64).to_le_bytes());
    }
    out
}

/// The `seminfo` `IPC_INFO` and `SEM_INFO` answer.
fn encode_seminfo(in_use: Option<(usize, usize)>) -> [u8; SEMINFO_BYTES] {
    let (usz, aem) = match in_use {
        Some((sets, sems)) => (sets as i32, sems.min(i32::MAX as usize) as i32),
        None => (SEMUSZ, SEMAEM),
    };
    let fields: [i32; 10] = [
        SEMMNS as i32,       // semmap
        SETS_PER_JOB as i32, // semmni
        SEMMNS as i32,       // semmns
        SEMMNS as i32,       // semmnu
        SEMMSL as i32,       // semmsl
        SEMOPM as i32,       // semopm
        SEMOPM as i32,       // semume
        usz,                 // semusz
        SEMVMX,              // semvmx
        aem,                 // semaem
    ];
    let mut out = [0_u8; SEMINFO_BYTES];
    for (to, field) in out.chunks_exact_mut(4).zip(fields) {
        to.copy_from_slice(&field.to_le_bytes());
    }
    out
}

/// Where `semctl`'s answer goes, and what it reads: the caller's memory, or
/// a check's buffer.
pub(crate) trait Memory {
    /// Copy `bytes` out to `at`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn write(&self, at: u64, bytes: &[u8]) -> Result<(), Errno>;
    /// Copy `bytes.len()` bytes in from `at`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn read(&self, at: u64, bytes: &mut [u8]) -> Result<(), Errno>;
}

impl Memory for Process {
    fn write(&self, at: u64, bytes: &[u8]) -> Result<(), Errno> {
        uaccess::copy_to_user(self.space(), at, bytes).map_err(|_| Errno::EFAULT)
    }

    fn read(&self, at: u64, bytes: &mut [u8]) -> Result<(), Errno> {
        uaccess::copy_from_user(self.space(), at, bytes).map_err(|_| Errno::EFAULT)
    }
}

/// `semctl(id, num, cmd, arg)`: `arg` is the `union semun` as a word --
/// `SETVAL`'s value, or the address the other commands read or write.
///
/// # Errors
///
/// `EINVAL` for an unknown command, an id with no set, a semaphore number
/// out of range, or a 32-bit caller asking for the pre-`IPC_64` layout;
/// `EACCES` without the permission the command needs; `EPERM` for
/// `IPC_SET` or `IPC_RMID` by neither owner, creator nor root; `ERANGE` for
/// a value past [`SEMVMX`]; `EIDRM` for a set removed during the call;
/// `EFAULT` for memory that cannot be read or written.
pub(crate) fn semctl(
    caller: &Caller,
    memory: &dyn Memory,
    layout: Layout,
    id: i32,
    num: i32,
    cmd: i32,
    arg: u64,
) -> Result<usize, Errno> {
    // A 64-bit caller's `IPC_64` is ignored, a 32-bit one's selects the one
    // layout carried here.
    let versioned = cmd & IPC_64 != 0;
    let cmd = cmd & !IPC_64;
    let old_layout = layout == Layout::Narrow && !versioned;
    match cmd {
        IPC_INFO | SEM_INFO => info(memory, cmd, arg),
        IPC_STAT | SEM_STAT | SEM_STAT_ANY | IPC_SET if old_layout => Err(Errno::EINVAL),
        IPC_STAT | SEM_STAT | SEM_STAT_ANY => stat(caller, memory, layout, id, cmd, arg),
        IPC_SET => set_perm(caller, memory, layout, id, arg),
        IPC_RMID => remove(caller, id),
        GETVAL | GETPID | GETNCNT | GETZCNT => read_one(caller, id, num, cmd),
        GETALL => get_all(caller, memory, id, arg),
        SETVAL => set_value(caller, id, num, arg as u32 as i32),
        SETALL => set_all(caller, memory, id, arg),
        _ => Err(Errno::EINVAL),
    }
}

/// `IPC_INFO` and `SEM_INFO`: the limits, and for `SEM_INFO` the sets and
/// semaphores in use; the highest slot in use is the answer.
fn info(memory: &dyn Memory, cmd: i32, arg: u64) -> Result<usize, Errno> {
    let (highest, sets, sems) = {
        let table = TABLE.lock();
        let highest = table.slots.iter().rposition(|slot| slot.set.is_some());
        (highest.unwrap_or(0), table.sets, table.sems)
    };
    let in_use = (cmd == SEM_INFO).then_some((sets, sems));
    memory.write(arg, &encode_seminfo(in_use))?;
    Ok(highest)
}

/// `IPC_STAT`, and `SEM_STAT` and `SEM_STAT_ANY`, whose `id` is a slot and
/// which answer the set's id.
fn stat(
    caller: &Caller,
    memory: &dyn Memory,
    layout: Layout,
    id: i32,
    cmd: i32,
    arg: u64,
) -> Result<usize, Errno> {
    let set = if cmd == IPC_STAT {
        lookup(id)
    } else {
        let slot = usize::try_from(id).map_err(|_| Errno::EINVAL)?;
        TABLE
            .lock()
            .slots
            .get(slot)
            .and_then(|slot| slot.set.as_ref())
            .map(Arc::clone)
    }
    .ok_or(Errno::EINVAL)?;
    let bytes = {
        let state = set.state.lock();
        if state.removed {
            return Err(Errno::EIDRM);
        }
        if cmd != SEM_STAT_ANY && !state.perm.allows(caller, READ) {
            return Err(Errno::EACCES);
        }
        encode_semid(&set, &state, layout)
    };
    if bytes.len() != layout.bytes() {
        return Err(Errno::ENOMEM);
    }
    memory.write(arg, &bytes)?;
    Ok(if cmd == IPC_STAT { 0 } else { set.id as usize })
}

/// `IPC_SET`: the owner, group and mode from the caller's `semid64_ds`.
fn set_perm(
    caller: &Caller,
    memory: &dyn Memory,
    layout: Layout,
    id: i32,
    arg: u64,
) -> Result<usize, Errno> {
    // Only the `ipc64_perm`'s uid, gid and mode are read, as Linux reads
    // them: the first 24 bytes, the same at every width.
    let _ = layout;
    let mut perm = [0_u8; 24];
    memory.read(arg, &mut perm)?;
    let word = |at: usize| {
        let mut four = [0_u8; 4];
        if let Some(from) = perm.get(at..at + 4) {
            four.copy_from_slice(from);
        }
        u32::from_le_bytes(four)
    };
    // As the caller's user namespace names them; one it does not map is
    // `EINVAL`, so an unmapped id is never stored (rule U9).
    let (uid, gid) = match process::current() {
        Some(running) => (
            credentials::kernel_uid(&running, word(4))?,
            credentials::kernel_gid(&running, word(8))?,
        ),
        // The kernel's own checks run with no process: the ids are kernel ids.
        None => (word(4), word(8)),
    };
    // A 16-bit mode's high half is padding a caller need not clear.
    let mode = word(20) & 0o777;
    let set = lookup(id).ok_or(Errno::EINVAL)?;
    let mut state = set.state.lock();
    if state.removed {
        return Err(Errno::EIDRM);
    }
    if !state.perm.owned_by(caller) {
        return Err(Errno::EPERM);
    }
    state.perm.uid = uid;
    state.perm.gid = gid;
    state.perm.mode = mode;
    state.ctime = now_seconds();
    Ok(0)
}

/// `IPC_RMID`: take the set out, and tell everyone waiting on it `EIDRM`.
fn remove(caller: &Caller, id: i32) -> Result<usize, Errno> {
    let set = lookup(id).ok_or(Errno::EINVAL)?;
    let (queue, undos) = {
        let mut state = set.state.lock();
        if state.removed {
            return Err(Errno::EIDRM);
        }
        if !state.perm.owned_by(caller) {
            return Err(Errno::EPERM);
        }
        state.removed = true;
        for pending in &state.queue {
            pending.ticket.answer(Err(Errno::EIDRM));
        }
        (
            core::mem::take(&mut state.queue),
            core::mem::take(&mut state.undos),
        )
    };
    {
        let mut table = TABLE.lock();
        if let Some((index, _)) = split_id(id)
            && let Some(slot) = table.slots.get_mut(index)
            && slot
                .set
                .as_ref()
                .is_some_and(|held| Arc::ptr_eq(held, &set))
        {
            slot.set = None;
            table.sets = table.sets.saturating_sub(1);
            table.sems = table.sems.saturating_sub(set.nsems);
        }
    }
    // The records and the set go after every lock, with their charges; the
    // set itself when its last holder -- a caller still leaving -- lets go.
    drop(queue);
    drop(undos);
    Ok(0)
}

/// `GETVAL`, `GETPID`, `GETNCNT` and `GETZCNT` of semaphore `num`.
fn read_one(caller: &Caller, id: i32, num: i32, cmd: i32) -> Result<usize, Errno> {
    let set = lookup(id).ok_or(Errno::EINVAL)?;
    let state = set.state.lock();
    if state.removed {
        return Err(Errno::EIDRM);
    }
    if !state.perm.allows(caller, READ) {
        return Err(Errno::EACCES);
    }
    let index = usize::try_from(num).map_err(|_| Errno::EINVAL)?;
    let sem = state.sems.get(index).ok_or(Errno::EINVAL)?;
    let counted = |zero: bool| {
        state
            .queue
            .iter()
            .filter(|pending| pending.blocked == (index as u16, zero))
            .count()
    };
    Ok(match cmd {
        GETVAL => sem.value as usize,
        // Kept as the kernel number; told as the asker's namespace numbers it.
        GETPID => crate::syscall::pidns::show_pid(sem.pid) as usize,
        GETNCNT => counted(false),
        _ => counted(true),
    })
}

/// `GETALL`: every value, as `unsigned short`s, to `arg`.
fn get_all(caller: &Caller, memory: &dyn Memory, id: i32, arg: u64) -> Result<usize, Errno> {
    let set = lookup(id).ok_or(Errno::EINVAL)?;
    let mut out = Vec::new();
    out.try_reserve_exact(set.nsems * 2)
        .map_err(|_| Errno::ENOMEM)?;
    {
        let state = set.state.lock();
        if state.removed {
            return Err(Errno::EIDRM);
        }
        if !state.perm.allows(caller, READ) {
            return Err(Errno::EACCES);
        }
        for sem in &state.sems {
            out.extend_from_slice(&(sem.value as u16).to_le_bytes());
        }
    }
    memory.write(arg, &out)?;
    Ok(0)
}

/// `SETVAL`: semaphore `num` to `value`, every process's adjustment for it
/// cleared, and the queue run.
fn set_value(caller: &Caller, id: i32, num: i32, value: i32) -> Result<usize, Errno> {
    if !(0..=SEMVMX).contains(&value) {
        return Err(Errno::ERANGE);
    }
    let set = lookup(id).ok_or(Errno::EINVAL)?;
    let mut state = set.state.lock();
    if state.removed {
        return Err(Errno::EIDRM);
    }
    let index = usize::try_from(num).map_err(|_| Errno::EINVAL)?;
    if index >= set.nsems {
        return Err(Errno::EINVAL);
    }
    if !state.perm.allows(caller, ALTER) {
        return Err(Errno::EACCES);
    }
    for undo in &mut state.undos {
        if let Some(adjust) = undo.adjust.get_mut(index) {
            *adjust = 0;
        }
    }
    if let Some(sem) = state.sems.get_mut(index) {
        sem.value = value;
        sem.pid = caller.pid;
    }
    state.ctime = now_seconds();
    run_queue(&mut state);
    Ok(0)
}

/// `SETALL`: every value from the `unsigned short`s at `arg`, every
/// adjustment cleared, and the queue run.
fn set_all(caller: &Caller, memory: &dyn Memory, id: i32, arg: u64) -> Result<usize, Errno> {
    let set = lookup(id).ok_or(Errno::EINVAL)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(set.nsems * 2)
        .map_err(|_| Errno::ENOMEM)?;
    bytes.resize(set.nsems * 2, 0);
    memory.read(arg, &mut bytes)?;
    let values: Vec<i32> = bytes
        .chunks_exact(2)
        .map(|pair| i32::from(u16::from_le_bytes(pair.try_into().unwrap_or([0, 0]))))
        .collect();
    if values.iter().any(|&value| value > SEMVMX) {
        return Err(Errno::ERANGE);
    }
    let mut state = set.state.lock();
    if state.removed {
        return Err(Errno::EIDRM);
    }
    if !state.perm.allows(caller, ALTER) {
        return Err(Errno::EACCES);
    }
    for (sem, value) in state.sems.iter_mut().zip(values) {
        sem.value = value;
        sem.pid = caller.pid;
    }
    for undo in &mut state.undos {
        undo.adjust.fill(0);
    }
    state.ctime = now_seconds();
    run_queue(&mut state);
    Ok(0)
}

// ---------------------------------------------------------------------------
// Exit
// ---------------------------------------------------------------------------

/// Apply what `undo_list`'s process owes each set it used [`SEM_UNDO`] on,
/// as it ends: each value plus its adjustment, kept within 0 and
/// [`SEMVMX`], with `pid` as its last operator; the record goes, and each
/// set's queue runs. A set removed since is passed over. Linux's `exit_sem`.
///
/// Takes only spin locks and wakes tasks, so it may run wherever the
/// process's release runs.
pub(crate) fn exit(undo_list: &UndoList, pid: u32) {
    let sets = core::mem::take(&mut *undo_list.sets.lock());
    for (id, charge) in sets {
        let Some(set) = lookup(id) else {
            continue;
        };
        let record = {
            let mut state = set.state.lock();
            let Some(at) = state
                .undos
                .iter()
                .position(|undo| undo.owner == undo_list.owner)
            else {
                continue;
            };
            let record = state.undos.swap_remove(at);
            for (sem, &adjust) in state.sems.iter_mut().zip(&record.adjust) {
                if adjust != 0 {
                    sem.value = (sem.value + i32::from(adjust)).clamp(0, SEMVMX);
                    sem.pid = pid;
                }
            }
            run_queue(&mut state);
            record
        };
        drop(record);
        drop(charge);
    }
}

// ---------------------------------------------------------------------------
// The system calls
// ---------------------------------------------------------------------------

/// The semaphore calls, and i386's `ipc`; `None` for any other.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let int = |value: u64| value as u32 as i32;
    let answer = match call {
        Syscall::Semget => sys_semget(process, int(a[0]), int(a[1]), int(a[2])),
        Syscall::Semop => sys_semtimedop(process, int(a[0]), a[1], a[2], None),
        Syscall::Semtimedop => {
            let width = TimeWidth::Native.in_abi(abi);
            sys_semtimedop(process, int(a[0]), a[1], a[2], Some((a[3], width)))
        }
        Syscall::SemtimedopTime64 => {
            let width = TimeWidth::Wide.in_abi(abi);
            sys_semtimedop(process, int(a[0]), a[1], a[2], Some((a[3], width)))
        }
        Syscall::Semctl => {
            // A 32-bit caller's `union semun` is a 32-bit word.
            let arg = if Layout::of(abi) == Layout::Narrow {
                a[3] & 0xFFFF_FFFF
            } else {
                a[3]
            };
            sys_semctl(process, abi, int(a[0]), int(a[1]), int(a[2]), arg)
        }
        Syscall::Ipc => sys_ipc(process, abi, a),
        _ => return None,
    };
    Some(answer)
}

/// `semget`.
fn sys_semget(process: &Process, key: i32, nsems: i32, flags: i32) -> Result<usize, Errno> {
    semget(&Caller::of(process), key, nsems, flags).map(|id| id as usize)
}

/// `semop` and `semtimedop`: `nsops` operations at `at`, with a relative
/// timeout at `timeout`'s address of its width, if given and not null.
fn sys_semtimedop(
    process: &Process,
    id: i32,
    at: u64,
    nsops: u64,
    timeout: Option<(u64, TimeWidth)>,
) -> Result<usize, Errno> {
    // `unsigned int nsops`, as Linux takes it.
    let count = nsops as u32 as usize;
    if count < 1 || id < 0 {
        return Err(Errno::EINVAL);
    }
    if count > SEMOPM {
        return Err(Errno::E2BIG);
    }
    let sops = read_sops(process, at, count)?;
    let deadline = match timeout {
        Some((address, width)) if address != 0 => {
            let relative = crate::syscall::poll::read_timespec(process, address, width)?;
            Some(crate::timer::now_nanos().saturating_add(relative))
        }
        _ => None,
    };
    let caller = Caller::of(process);
    semop(&caller, process.sem_undo(), id, sops, deadline, process)
}

/// Read `count` `struct sembuf`s from `at`.
fn read_sops(process: &Process, at: u64, count: usize) -> Result<Vec<SemBuf>, Errno> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count * SEMBUF_BYTES)
        .map_err(|_| Errno::ENOMEM)?;
    bytes.resize(count * SEMBUF_BYTES, 0);
    process.read(at, &mut bytes)?;
    let mut sops = Vec::new();
    sops.try_reserve_exact(count).map_err(|_| Errno::ENOMEM)?;
    for one in bytes.chunks_exact(SEMBUF_BYTES) {
        let half = |at: usize| {
            one.get(at..at + 2)
                .and_then(|pair| pair.try_into().ok())
                .unwrap_or([0, 0])
        };
        sops.push(SemBuf {
            num: u16::from_le_bytes(half(0)),
            op: i16::from_le_bytes(half(2)),
            flags: i16::from_le_bytes(half(4)),
        });
    }
    Ok(sops)
}

/// `semctl`.
fn sys_semctl(
    process: &Process,
    abi: Abi,
    id: i32,
    num: i32,
    cmd: i32,
    arg: u64,
) -> Result<usize, Errno> {
    semctl(
        &Caller::of(process),
        process,
        Layout::of(abi),
        id,
        num,
        cmd,
        arg,
    )
}

/// i386's `ipc(call, first, second, third, ptr, fifth)` for the semaphore
/// operations, as Linux's `compat_ksys_ipc` unpacks them; the message queue
/// and shared memory ones are `ENOSYS`, as before. The call's upper half is
/// its `IPC_64` version, which only the older message and shared-memory
/// operations read; masked off here.
fn sys_ipc(process: &Process, abi: Abi, a: &[u64; 6]) -> Result<usize, Errno> {
    let [call, first, second, third, ptr, fifth] = *a;
    let operation = call as u32 & 0xFFFF;
    let first = first as u32 as i32;
    let ptr = ptr & 0xFFFF_FFFF;
    match operation {
        IPCOP_SEMOP => sys_semtimedop(process, first, ptr, second, None),
        IPCOP_SEMTIMEDOP => {
            let width = TimeWidth::Native.in_abi(abi);
            sys_semtimedop(
                process,
                first,
                ptr,
                second,
                Some((fifth & 0xFFFF_FFFF, width)),
            )
        }
        IPCOP_SEMGET => sys_semget(process, first, second as u32 as i32, third as u32 as i32),
        IPCOP_SEMCTL => {
            // `ptr` is the address of the `union semun`, read as a word.
            if ptr == 0 {
                return Err(Errno::EINVAL);
            }
            let arg = uaccess::get_u32(process.space(), ptr)?;
            sys_semctl(
                process,
                abi,
                first,
                second as u32 as i32,
                third as u32 as i32,
                u64::from(arg),
            )
        }
        _ => Err(Errno::ENOSYS),
    }
}

/// Sets in use, for the checks: every one they make must be gone again.
pub(crate) fn sets_in_use() -> usize {
    TABLE.lock().sets
}
