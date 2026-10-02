//! System V shared memory: `shmget`, `shmat`, `shmdt` and `shmctl`, and
//! i386's `ipc` entry to them.
//!
//! Built for Steam's web helper (2026-10-02, the customer's decision): its
//! Chromium presents software-composited frames to the X server with
//! MIT-SHM, whose classic path is a segment both sides attach. Chromium
//! asks `shmctl(0, IPC_INFO)` for the size limit, makes a segment with
//! `shmget(IPC_PRIVATE, size, IPC_CREAT | 0600)`, attaches it, sends the X
//! server its id, and removes it with `IPC_RMID` while both still have it
//! attached; the X server, as root, attaches it by id. Before this file
//! every one of these calls was `ENOSYS`, and every frame went through the
//! socket as a `PutImage`.
//!
//! # What is kept, and where
//!
//! An [`IpcNamespace`]'s `shm` table holds every segment by slot, numbered
//! as `sem.rs` numbers its sets: a slot and a sequence number, so that a
//! removed segment's id is `EINVAL` afterwards rather than someone else's
//! segment. Whether a caller may use a segment is decided by its `ipc_perm`
//! alone, through [`Perm::allows`], Linux's `ipcperms`: root, standing in
//! for `CAP_IPC_OWNER`, may attach any segment, as the X server does.
//!
//! A segment's memory is an anonymous [`Vmo`] of its size in pages, and an
//! attach maps it shared, as `mmap(MAP_SHARED)` maps a tmpfs file's object:
//! through [`AddressSpace::map_file`], with an [`Attachment`] where a file
//! mapping keeps its open file. The address space keeps that for as long as
//! a region names the attach, clones it into a forked child, and lets go of
//! it when the last region goes -- by `shmdt`, `munmap`, `execve` or the
//! end of the process. So the segment's attach count, `shm_nattch`, is the
//! number of references to its one [`Attachment`]: one per attach in every
//! address space, as Linux counts one per mapping.
//!
//! # Removal
//!
//! `IPC_RMID` marks a segment removed (`SHM_DEST`) and frees its key; the
//! segment stays in the table, found by its id, until its last attach goes,
//! and then the [`Attachment`]'s drop takes it out. With no attach it is
//! taken out at once. Its pages go with the last reference to its object.
//!
//! # Locks
//!
//! Two kinds of [`SpinLock`], never one inside the other: a namespace's
//! table, held to find, add or take out a segment, and a segment's state.
//! An attach and a detach take the address space's layout lock first, as
//! `mmap` does, and neither spin lock is held across the address space's
//! own. An [`Attachment`] may be dropped wherever an address space lets go
//! of a file mapping, the reaper included; its drop takes only the two spin
//! locks, one after the other.
//!
//! # What a job pays for (F-37)
//!
//! A segment's record is charged to the job of the task that made it, and
//! its object to that job's object count, until it is taken out -- a
//! segment outlives its maker, as on Linux. A job may hold at most
//! [`SEGMENTS_PER_JOB`] segments, so that one job cannot take every id
//! from its siblings, and a namespace at most [`SHMALL`] pages of them.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_kmem::Charge;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_vma::VmaFlags;

use crate::sync::SpinLock;
use crate::syscall::credentials;
use crate::syscall::memory;
use crate::syscall::process::Process;
use crate::syscall::sem::{self, Caller, IpcNamespace, Layout, Memory, Perm, READ};
use crate::syscall::uaccess;
use crate::trap::Abi;
use crate::user::space::{AddressSpace, FileMapping, FilePlace, MMAP_MIN_ADDR};
use crate::user::vmo::Vmo;

// ---------------------------------------------------------------------------
// The ABI: `include/uapi/linux/ipc.h` and `include/uapi/linux/shm.h`.
// ---------------------------------------------------------------------------

/// The key that always makes a new segment.
const IPC_PRIVATE: i32 = 0;
/// Make the segment if its key has none.
const IPC_CREAT: i32 = 0o1000;
/// With [`IPC_CREAT`]: fail if its key has one.
const IPC_EXCL: i32 = 0o2000;
/// A huge-page segment, which needs a hugetlbfs there is none of.
const SHM_HUGETLB: i32 = 0o4000;
/// The layout flag a 32-bit caller's command carries for the `ipc64_perm`
/// structures, as glibc and musl always pass it.
const IPC_64: i32 = 0x0100;

/// Remove the segment once its last attach goes.
const IPC_RMID: i32 = 0;
/// Set its owner and mode.
const IPC_SET: i32 = 1;
/// Read its `shmid64_ds`.
const IPC_STAT: i32 = 2;
/// Read the limits, as a `shminfo64`.
const IPC_INFO: i32 = 3;
/// Lock its pages in memory.
const SHM_LOCK: i32 = 11;
/// Unlock them.
const SHM_UNLOCK: i32 = 12;
/// [`IPC_STAT`] by slot, answering the segment's id.
const SHM_STAT: i32 = 13;
/// What is in use, as a `shm_info`.
const SHM_INFO: i32 = 14;
/// [`SHM_STAT`] without the read permission.
const SHM_STAT_ANY: i32 = 15;

/// Attach read-only.
const SHM_RDONLY: i32 = 0o10000;
/// Round the attach address down to [`shmlba`].
const SHM_RND: i32 = 0o20000;
/// Replace what is mapped at the attach address.
const SHM_REMAP: i32 = 0o40000;
/// Attach executable.
const SHM_EXEC: i32 = 0o100000;

/// In `IPC_STAT`'s mode: removed, waiting for its last detach.
const SHM_DEST: u32 = 0o1000;
/// In `IPC_STAT`'s mode: locked by [`SHM_LOCK`].
const SHM_LOCKED: u32 = 0o2000;

/// i386 `ipc` operations, from `linux/ipc.h`.
const IPCOP_SHMAT: u32 = 21;
/// See [`IPCOP_SHMAT`].
const IPCOP_SHMDT: u32 = 22;
/// See [`IPCOP_SHMAT`].
const IPCOP_SHMGET: u32 = 23;
/// See [`IPCOP_SHMAT`].
const IPCOP_SHMCTL: u32 = 24;

/// Write permission, in a class's three bits: Linux's `S_IWUGO`.
const WRITE: u32 = 2;
/// Execute permission: `S_IXUGO`.
const EXECUTE: u32 = 1;

/// The smallest segment: Linux's `SHMMIN`.
pub(crate) const SHMMIN: u64 = 1;
/// The largest segment. Linux's default is nearly the whole address space;
/// this is `INT_MAX`, which is what Linux reports a 32-bit caller anyway,
/// and eight times a 4K frame at four bytes a pixel.
pub(crate) const SHMMAX: u64 = 0x7FFF_FFFF;
/// Pages a namespace's segments may hold together: Linux's `SHMALL` as a
/// bound of 16 GiB, which only counts what is reserved -- a page takes a
/// frame when it is first touched.
pub(crate) const SHMALL: u64 = 1 << 22;
/// Segments one job may hold: Linux's `SHMMNI` as a per-job bound (see the
/// module documentation). Also what `IPC_INFO` reports as `shmmni` and
/// `shmseg`.
pub(crate) const SEGMENTS_PER_JOB: usize = 4096;

/// Bits of an id that are its slot, as `sem.rs` numbers sets.
const SLOT_BITS: u32 = 24;
/// Slots in a table.
const SLOTS: usize = 1 << SLOT_BITS;
/// The sequence numbers an id carries, keeping ids positive.
const SEQ_MASK: u32 = 0x7F;

/// Bytes in a 64-bit `shmid64_ds`, x86-64's and AArch64's alike.
const WIDE_SHMID_BYTES: usize = 112;
/// Bytes in a 32-bit one, i386's and ARMv7-A's.
const NARROW_SHMID_BYTES: usize = 84;

/// The command flags `IPC_STAT`, `IPC_INFO` and the others are checked for.
pub(crate) mod cmd {
    /// `IPC_RMID`.
    pub(crate) const RMID: i32 = super::IPC_RMID;
    /// `IPC_SET`.
    pub(crate) const SET: i32 = super::IPC_SET;
    /// `IPC_STAT`.
    pub(crate) const STAT: i32 = super::IPC_STAT;
    /// `IPC_INFO`.
    pub(crate) const INFO: i32 = super::IPC_INFO;
    /// `SHM_INFO`.
    pub(crate) const SHM_INFO: i32 = super::SHM_INFO;
    /// `IPC_64`.
    pub(crate) const IPC_64: i32 = super::IPC_64;
    /// `IPC_CREAT`.
    pub(crate) const CREAT: i32 = super::IPC_CREAT;
    /// `IPC_EXCL`.
    pub(crate) const EXCL: i32 = super::IPC_EXCL;
    /// `SHM_RDONLY`.
    pub(crate) const RDONLY: i32 = super::SHM_RDONLY;
    /// `SHM_DEST`, in a mode.
    pub(crate) const DEST: u32 = super::SHM_DEST;
}

// ---------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------

/// A segment's changing part.
#[derive(Debug)]
struct State {
    /// Its owner and mode.
    perm: Perm,
    /// Set by `IPC_RMID`: its key is free, and it goes with its last attach.
    removed: bool,
    /// Set when it is taken out of the table: no attach may begin again.
    gone: bool,
    /// Set by [`SHM_LOCK`], which is all it does: nothing here swaps.
    locked: bool,
    /// The real time of its last attach, in seconds; zero before one.
    atime: i64,
    /// The real time of its last detach, in seconds; zero before one.
    dtime: i64,
    /// The real time it was made or last changed by `IPC_SET`.
    ctime: i64,
    /// The process that made it.
    cpid: u32,
    /// The process that last attached or detached it.
    lpid: u32,
    /// What its attaches hold: the count of strong references is
    /// `shm_nattch`.
    attached: Weak<Attachment>,
}

/// One shared memory segment.
#[derive(Debug)]
pub(crate) struct Segment {
    /// Its id, as `shmget` answered it.
    id: i32,
    /// Its key, until it is removed.
    key: i32,
    /// Its size in bytes, as asked.
    size: u64,
    /// Its size in pages.
    pages: u64,
    /// The job that pays for it: [`Charge::owner`] of `charge`.
    job: u32,
    /// Its memory.
    vmo: Arc<Vmo>,
    /// The namespace whose table holds it.
    ns: Weak<IpcNamespace>,
    /// [`State::removed`], readable without the lock for a search by key.
    removed: AtomicBool,
    /// Its heap, charged to the job that made it.
    #[allow(dead_code, reason = "held for its drop, which gives the charge back")]
    charge: Charge,
    /// Everything that changes.
    state: SpinLock<State>,
}

/// What every attach of one segment holds, in every address space it is
/// mapped in: the file a shared file mapping keeps. The last one's drop is
/// the segment's last detach.
#[derive(Debug)]
pub(crate) struct Attachment {
    /// The segment.
    segment: Arc<Segment>,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let segment = &self.segment;
        let last = {
            let mut state = segment.state.lock();
            state.dtime = sem::now_seconds();
            // A new attach since this one's count reached zero holds an
            // attachment of its own, and the segment stays.
            let last = state.removed && !state.gone && state.attached.strong_count() == 0;
            if last {
                state.gone = true;
            }
            last
        };
        if last {
            take_out(segment);
        }
    }
}

/// One slot of a table.
#[derive(Debug, Default)]
struct Slot {
    /// The sequence number the next segment made here takes.
    seq: u32,
    /// The segment in it.
    segment: Option<Arc<Segment>>,
}

/// A namespace's segments, by slot.
#[derive(Debug)]
pub(crate) struct Table {
    /// The slots, grown as they are needed.
    slots: Vec<Slot>,
    /// Segments in the table, removed ones still attached included.
    segments: usize,
    /// Pages those segments hold.
    pages: u64,
}

impl Table {
    /// An empty table.
    pub(crate) const fn new() -> Table {
        Table {
            slots: Vec::new(),
            segments: 0,
            pages: 0,
        }
    }
}

/// The slot an id names.
fn slot_of(id: i32) -> Option<usize> {
    let id = u32::try_from(id).ok()?;
    Some((id & (SLOTS as u32 - 1)) as usize)
}

/// The segment with id `id`, if there is one.
fn lookup(ns: &IpcNamespace, id: i32) -> Option<Arc<Segment>> {
    let slot = slot_of(id)?;
    let table = ns.shm.lock();
    let segment = table.slots.get(slot)?.segment.as_ref()?;
    (segment.id == id).then(|| Arc::clone(segment))
}

/// Take `segment` out of its namespace's table, if it is still there. Its
/// record goes after the lock.
fn take_out(segment: &Arc<Segment>) {
    let Some(ns) = segment.ns.upgrade() else {
        return;
    };
    let taken = {
        let mut table = ns.shm.lock();
        let held = slot_of(segment.id)
            .and_then(|index| table.slots.get_mut(index))
            .filter(|slot| {
                slot.segment
                    .as_ref()
                    .is_some_and(|held| Arc::ptr_eq(held, segment))
            })
            .and_then(|slot| slot.segment.take());
        if held.is_some() {
            table.segments = table.segments.saturating_sub(1);
            table.pages = table.pages.saturating_sub(segment.pages);
        }
        held
    };
    drop(taken);
}

/// The attach count of a segment whose state is `state`.
fn nattch(state: &State) -> usize {
    state.attached.strong_count()
}

// ---------------------------------------------------------------------------
// shmget
// ---------------------------------------------------------------------------

/// `shmget(key, size, flags)`: the id of the segment `key` names, made if
/// `flags` asks and it has none; a new segment every time for
/// [`IPC_PRIVATE`].
///
/// # Errors
///
/// `EINVAL` for a new segment's size below [`SHMMIN`] or past [`SHMMAX`], or
/// a size past an existing segment's; `ENOENT` for a key with no segment and
/// no [`IPC_CREAT`]; `EEXIST` for one with a segment and both flags;
/// `EACCES` for a segment whose mode refuses `flags`' permission bits;
/// `ENOSPC` for a job with [`SEGMENTS_PER_JOB`] segments, or a namespace
/// past [`SHMALL`] pages or out of slots; `ENOMEM` for a job at its memory
/// limit; `ENOSYS` for `SHM_HUGETLB`, as Linux answers without hugetlbfs.
pub(crate) fn shmget(caller: &Caller, key: i32, size: u64, flags: i32) -> Result<i32, Errno> {
    shmget_capped(caller, key, size, flags, SEGMENTS_PER_JOB)
}

/// [`shmget`], with `per_job` in place of [`SEGMENTS_PER_JOB`]: for the
/// check, which cannot make four thousand segments to show the bound.
///
/// # Errors
///
/// As [`shmget`].
pub(crate) fn shmget_capped(
    caller: &Caller,
    key: i32,
    size: u64,
    flags: i32,
    per_job: usize,
) -> Result<i32, Errno> {
    if key == IPC_PRIVATE {
        return create(caller, key, size, flags, per_job);
    }
    loop {
        let found = {
            let table = caller.ns.shm.lock();
            table
                .slots
                .iter()
                .filter_map(|slot| slot.segment.as_ref())
                .find(|segment| segment.key == key && !segment.removed.load(Ordering::Acquire))
                .map(Arc::clone)
        };
        let Some(segment) = found else {
            if flags & IPC_CREAT == 0 {
                return Err(Errno::ENOENT);
            }
            match create(caller, key, size, flags, per_job) {
                // Another caller made one for the key in between: look again.
                Err(Errno::EEXIST) if flags & IPC_EXCL == 0 => continue,
                other => return other,
            }
        };
        if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
            return Err(Errno::EEXIST);
        }
        if size > segment.size {
            return Err(Errno::EINVAL);
        }
        let state = segment.state.lock();
        if state.removed {
            // Removed since it was found: the key is free again.
            continue;
        }
        let mode = flags as u32 & 0o777;
        let wanted = mode >> 6 | mode >> 3 | mode;
        if !state.perm.allows(caller, wanted & 0o7) {
            return Err(Errno::EACCES);
        }
        return Ok(segment.id);
    }
}

/// Make a segment of `size` bytes under `key`, charged to the running
/// task's job. `EEXIST` if `key` is not [`IPC_PRIVATE`] and a segment took
/// it in the meantime.
fn create(caller: &Caller, key: i32, size: u64, flags: i32, per_job: usize) -> Result<i32, Errno> {
    if flags & SHM_HUGETLB != 0 {
        return Err(Errno::ENOSYS);
    }
    if !(SHMMIN..=SHMMAX).contains(&size) {
        return Err(Errno::EINVAL);
    }
    let pages = size.div_ceil(PAGE_SIZE);
    let bytes = ferrix_kmem::arc_footprint::<Segment>().saturating_add(size_of::<Slot>());
    let charge = Charge::bytes(bytes).map_err(|_| Errno::ENOMEM)?;
    let vmo = Vmo::new_anonymous(pages).map_err(|_| Errno::ENOMEM)?;
    let now = sem::now_seconds();
    let state = State {
        perm: Perm {
            uid: caller.uid,
            gid: caller.gid,
            cuid: caller.uid,
            cgid: caller.gid,
            mode: flags as u32 & 0o777,
        },
        removed: false,
        gone: false,
        locked: false,
        atime: 0,
        dtime: 0,
        ctime: now,
        cpid: caller.pid,
        lpid: 0,
        attached: Weak::new(),
    };
    let job = charge.owner();
    let ns = Arc::downgrade(&caller.ns);
    let mut table = caller.ns.shm.lock();
    if key != IPC_PRIVATE
        && table
            .slots
            .iter()
            .filter_map(|slot| slot.segment.as_ref())
            .any(|segment| segment.key == key && !segment.removed.load(Ordering::Acquire))
    {
        return Err(Errno::EEXIST);
    }
    let held = table
        .slots
        .iter()
        .filter_map(|slot| slot.segment.as_ref())
        .filter(|segment| segment.job == job)
        .count();
    if held >= per_job || table.pages.saturating_add(pages) > SHMALL {
        return Err(Errno::ENOSPC);
    }
    let index = match table.slots.iter().position(|slot| slot.segment.is_none()) {
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
    let segment = crate::fallible::try_arc(Segment {
        id,
        key,
        size,
        pages,
        job,
        vmo,
        ns,
        removed: AtomicBool::new(false),
        charge,
        state: SpinLock::new(state),
    })
    .map_err(|_| Errno::ENOMEM)?;
    slot.segment = Some(segment);
    table.segments += 1;
    table.pages = table.pages.saturating_add(pages);
    Ok(id)
}

// ---------------------------------------------------------------------------
// shmat and shmdt
// ---------------------------------------------------------------------------

/// The attach address alignment for a call that came in by `abi`: Linux's
/// `SHMLBA`, four pages on ARMv7-A, whose caches could alias, and one page
/// elsewhere -- i386's on x86-64 included.
pub(crate) fn shmlba(abi: Abi) -> u64 {
    let _ = abi;
    if crate::arch::ARCH == ferrix_bootinfo::Arch::Armv7a {
        4 * PAGE_SIZE
    } else {
        PAGE_SIZE
    }
}

/// An attach of a segment, before it is mapped: the permission checked,
/// the attach counted. What [`shmat`] maps, and what the check holds to
/// stand for a mapping.
///
/// # Errors
///
/// `EINVAL` for an id with no segment; `EIDRM` for one whose last attach
/// has just gone after its removal; `EACCES` without the permission
/// `flags` asks for; `ENOMEM` with no memory for the attachment.
pub(crate) fn attach(caller: &Caller, id: i32, flags: i32) -> Result<Arc<Attachment>, Errno> {
    let mut wanted = READ;
    if flags & SHM_RDONLY == 0 {
        wanted |= WRITE;
    }
    if flags & SHM_EXEC != 0 {
        wanted |= EXECUTE;
    }
    let segment = lookup(&caller.ns, id).ok_or(Errno::EINVAL)?;
    let mut state = segment.state.lock();
    if state.gone {
        return Err(Errno::EIDRM);
    }
    if !state.perm.allows(caller, wanted) {
        return Err(Errno::EACCES);
    }
    if let Some(held) = state.attached.upgrade() {
        return Ok(held);
    }
    let made = crate::fallible::try_arc(Attachment {
        segment: Arc::clone(&segment),
    })
    .map_err(|_| Errno::ENOMEM)?;
    state.attached = Arc::downgrade(&made);
    Ok(made)
}

/// `shmat(id, addr, flags)`: map segment `id` into `process`, at `addr`
/// (rounded down to `shmlba` with `SHM_RND`) or wherever it fits for a null
/// `addr`, and answer where.
///
/// # Errors
///
/// `EINVAL` for an address off its alignment, `SHM_REMAP` without an
/// address, an address whose range is mapped already without `SHM_REMAP`,
/// or a range past the user half; `EPERM` for an address below
/// [`MMAP_MIN_ADDR`]; and as [`attach`] and `mmap`.
pub(crate) fn shmat(
    process: &Process,
    id: i32,
    addr: u64,
    flags: i32,
    shmlba: u64,
) -> Result<u64, Errno> {
    // Linux's `do_shmat`, in its order: the address first.
    let fixed = addr != 0;
    let mut at = addr;
    if fixed {
        if at & (shmlba - 1) != 0 {
            if flags & SHM_RND != 0 {
                at &= !(shmlba - 1);
                if at == 0 && flags & SHM_REMAP != 0 {
                    return Err(Errno::EINVAL);
                }
            } else if shmlba > PAGE_SIZE || at & (PAGE_SIZE - 1) != 0 {
                // ARMv7-A forces `SHMLBA` (`__ARCH_FORCE_SHMLBA`).
                return Err(Errno::EINVAL);
            }
        }
    } else if flags & SHM_REMAP != 0 {
        return Err(Errno::EINVAL);
    }
    let caller = Caller::of(process);
    let attachment = attach(&caller, id, flags)?;
    let segment = Arc::clone(&attachment.segment);
    let len = segment.pages.saturating_mul(PAGE_SIZE);
    let vma = VmaFlags {
        read: true,
        write: flags & SHM_RDONLY == 0,
        execute: flags & SHM_EXEC != 0,
        shared: true,
        ..VmaFlags::NONE
    };
    let space = process.space();
    let mapped = {
        let _layout = space.layout();
        let place = if fixed {
            if at < MMAP_MIN_ADDR {
                return Err(Errno::EPERM);
            }
            let end = at.checked_add(len).ok_or(Errno::EINVAL)?;
            if flags & SHM_REMAP != 0 {
                let _ = space.unmap(at, len);
            } else if space.with_regions(|regions| {
                let mut overlapping =
                    regions.filter(|region| region.start < end && at < region.end);
                overlapping.next().is_some()
            }) {
                return Err(Errno::EINVAL);
            }
            FilePlace::Fixed(at)
        } else {
            FilePlace::Anywhere(None)
        };
        let mapping = FileMapping {
            file: attachment as Arc<dyn Any + Send + Sync>,
            may_write: flags & SHM_RDONLY == 0,
        };
        space
            .map_file(place, len, vma, Arc::clone(&segment.vmo), 0, mapping)
            .map_err(memory::refused)?
    };
    let mut state = segment.state.lock();
    state.atime = sem::now_seconds();
    state.lpid = caller.pid;
    Ok(mapped)
}

/// `shmdt(addr)`: take down the attach that began at `addr`, every region
/// of it that is still mapped within its size, as Linux's `ksys_shmdt`
/// finds them -- a part `munmap` or `mprotect` split off included.
///
/// # Errors
///
/// `EINVAL` for an address off a page boundary, or one no attach began at;
/// `ENOMEM` with no memory to list the regions.
pub(crate) fn shmdt(process: &Process, addr: u64) -> Result<usize, Errno> {
    if addr & (PAGE_SIZE - 1) != 0 {
        return Err(Errno::EINVAL);
    }
    let space = process.space();
    let segment = {
        let _layout = space.layout();
        let regions = space.regions().map_err(|_| Errno::ENOMEM)?;
        // The attach a region belongs to, if it is one that began at `addr`.
        let attach_at = |region: &crate::user::space::Region| {
            let (id, offset) = region.file?;
            if region.start.checked_sub(offset) != Some(addr) {
                return None;
            }
            let attachment = space.mapped_file(id)?.downcast::<Attachment>().ok()?;
            Some((id, Arc::clone(&attachment.segment)))
        };
        let Some((id, segment)) = regions
            .iter()
            .filter(|region| region.end > addr)
            .find_map(attach_at)
        else {
            return Err(Errno::EINVAL);
        };
        let size = segment.pages.saturating_mul(PAGE_SIZE);
        for region in regions
            .iter()
            .filter(|region| region.end > addr && region.end - addr <= size)
            .filter(|region| region.file.is_some_and(|(held, _)| held == id))
        {
            let _ = space.unmap(region.start, region.end - region.start);
        }
        segment
    };
    let pid = process.pid();
    segment.state.lock().lpid = pid;
    Ok(0)
}

/// Detach every attach `space` holds, as a process's exit does: Linux's
/// `exit_mm` unmaps them before the parent is told, so a parent's `wait4`
/// sees the attach count without the child's. The space itself stays until
/// the last task in it is reaped, which a busy machine puts off.
///
/// With no memory to list the regions, nothing is detached here, and the
/// attaches go with the space.
pub(crate) fn exit(space: &AddressSpace) {
    let _layout = space.layout();
    let Ok(regions) = space.regions() else {
        return;
    };
    for region in regions {
        let Some((id, _)) = region.file else {
            continue;
        };
        let attached = space
            .mapped_file(id)
            .is_some_and(|file| file.downcast::<Attachment>().is_ok());
        if attached {
            let _ = space.unmap(region.start, region.end - region.start);
        }
    }
}

// ---------------------------------------------------------------------------
// shmctl
// ---------------------------------------------------------------------------

/// Bytes in the `shmid64_ds` of `layout`.
pub(crate) const fn shmid_bytes(layout: Layout) -> usize {
    match layout {
        Layout::Narrow => NARROW_SHMID_BYTES,
        Layout::X86_64 | Layout::Generic64 => WIDE_SHMID_BYTES,
    }
}

/// Offsets in a `shmid64_ds` of `layout`: `shm_segsz`, `shm_atime`,
/// `shm_dtime`, `shm_ctime`, `shm_cpid`, `shm_lpid` and `shm_nattch`, from
/// `asm-generic/shmbuf.h`. x86-64 has the generic one.
pub(crate) const fn fields(layout: Layout) -> [usize; 7] {
    match layout {
        Layout::Narrow => [36, 40, 48, 56, 64, 68, 72],
        Layout::X86_64 | Layout::Generic64 => [48, 56, 64, 72, 80, 84, 88],
    }
}

/// Put `value` at `at` in `out`, as a `long` of `layout`'s width.
fn put_word(out: &mut [u8], at: usize, value: u64, layout: Layout) {
    if layout == Layout::Narrow {
        put(out, at, &(value as u32).to_le_bytes());
    } else {
        put(out, at, &value.to_le_bytes());
    }
}

/// Put `bytes` at `at` in `out`, if they fit.
fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(to) = out.get_mut(at..at + bytes.len()) {
        to.copy_from_slice(bytes);
    }
}

/// A segment's `shmid64_ds`, in `layout`, in the first
/// [`shmid_bytes`] of the answer.
fn encode_shmid(segment: &Segment, state: &State, layout: Layout) -> [u8; WIDE_SHMID_BYTES] {
    let mut out = [0_u8; WIDE_SHMID_BYTES];
    let seq = u32::try_from(segment.id).map_or(0, |id| id >> SLOT_BITS);
    // Linux sets a removed segment's key to `IPC_PRIVATE`.
    let key = if state.removed {
        IPC_PRIVATE
    } else {
        segment.key
    };
    let mut mode = state.perm.mode;
    if state.removed {
        mode |= SHM_DEST;
    }
    if state.locked {
        mode |= SHM_LOCKED;
    }
    put(&mut out, 0, &key.to_le_bytes());
    put(
        &mut out,
        4,
        &credentials::show_uid(state.perm.uid).to_le_bytes(),
    );
    put(
        &mut out,
        8,
        &credentials::show_gid(state.perm.gid).to_le_bytes(),
    );
    put(
        &mut out,
        12,
        &credentials::show_uid(state.perm.cuid).to_le_bytes(),
    );
    put(
        &mut out,
        16,
        &credentials::show_gid(state.perm.cgid).to_le_bytes(),
    );
    put(&mut out, 20, &mode.to_le_bytes());
    put(&mut out, 24, &(seq as u16).to_le_bytes());
    let [segsz, atime, dtime, ctime, cpid, lpid, attaches] = fields(layout);
    put_word(&mut out, segsz, segment.size, layout);
    // Each time as eight bytes: a 64-bit field, or a 32-bit one's low half
    // and then its `_high` half.
    put(&mut out, atime, &state.atime.to_le_bytes());
    put(&mut out, dtime, &state.dtime.to_le_bytes());
    put(&mut out, ctime, &state.ctime.to_le_bytes());
    put(&mut out, cpid, &state.cpid.to_le_bytes());
    put(&mut out, lpid, &state.lpid.to_le_bytes());
    put_word(&mut out, attaches, nattch(state) as u64, layout);
    out
}

/// `shmctl(id, cmd, buf)`.
///
/// # Errors
///
/// `EINVAL` for an unknown command, an id with no segment, or a 32-bit
/// caller asking for the pre-`IPC_64` layout; `EACCES` without read
/// permission for `IPC_STAT`; `EPERM` for `IPC_SET`, `IPC_RMID`, `SHM_LOCK`
/// or `SHM_UNLOCK` by neither owner, creator nor root; `EFAULT` for memory
/// that cannot be read or written.
pub(crate) fn shmctl(
    caller: &Caller,
    memory: &dyn Memory,
    layout: Layout,
    id: i32,
    cmd: i32,
    arg: u64,
) -> Result<usize, Errno> {
    // A 64-bit caller's `IPC_64` is ignored, a 32-bit one's selects the one
    // layout carried here.
    let versioned = cmd & IPC_64 != 0;
    let cmd = cmd & !IPC_64;
    let old_layout = layout == Layout::Narrow && !versioned;
    match cmd {
        IPC_INFO | IPC_STAT | SHM_STAT | SHM_STAT_ANY | IPC_SET if old_layout => Err(Errno::EINVAL),
        IPC_INFO => info(caller, memory, layout, arg),
        SHM_INFO => usage(caller, memory, layout, arg),
        IPC_STAT | SHM_STAT | SHM_STAT_ANY => stat(caller, memory, layout, id, cmd, arg),
        IPC_SET => set_perm(caller, memory, id, arg),
        IPC_RMID => remove(caller, id),
        SHM_LOCK | SHM_UNLOCK => lock(caller, id, cmd == SHM_LOCK),
        _ => Err(Errno::EINVAL),
    }
}

/// The highest slot in use in `caller`'s namespace, or zero.
fn highest(caller: &Caller) -> usize {
    let table = caller.ns.shm.lock();
    table
        .slots
        .iter()
        .rposition(|slot| slot.segment.is_some())
        .unwrap_or(0)
}

/// `IPC_INFO`: the limits, as a `shminfo64`; the highest slot in use is the
/// answer.
fn info(caller: &Caller, memory: &dyn Memory, layout: Layout, arg: u64) -> Result<usize, Errno> {
    let width = if layout == Layout::Narrow { 4 } else { 8 };
    let mut out = [0_u8; 72];
    let limits = [
        SHMMAX,
        SHMMIN,
        SEGMENTS_PER_JOB as u64,
        SEGMENTS_PER_JOB as u64,
        SHMALL,
    ];
    for (index, value) in limits.into_iter().enumerate() {
        put_word(&mut out, index * width, value, layout);
    }
    let bytes = out.get(..9 * width).ok_or(Errno::EINVAL)?;
    memory.write(arg, bytes)?;
    Ok(highest(caller))
}

/// `SHM_INFO`: what is in use, as a `shm_info` -- the segments, the pages
/// they reserve and the pages that hold a frame; nothing is ever swapped.
fn usage(caller: &Caller, memory: &dyn Memory, layout: Layout, arg: u64) -> Result<usize, Errno> {
    let mut held = Vec::new();
    let (segments, pages) = {
        let table = caller.ns.shm.lock();
        held.try_reserve_exact(table.segments)
            .map_err(|_| Errno::ENOMEM)?;
        held.extend(
            table
                .slots
                .iter()
                .filter_map(|slot| slot.segment.as_ref())
                .map(|segment| Arc::clone(&segment.vmo)),
        );
        (table.segments, table.pages)
    };
    let resident: u64 = held.iter().map(|vmo| vmo.committed() as u64).sum();
    drop(held);
    let width = if layout == Layout::Narrow { 4 } else { 8 };
    let mut out = [0_u8; 48];
    put(
        &mut out,
        0,
        &(segments.min(i32::MAX as usize) as i32).to_le_bytes(),
    );
    // `used_ids`, then five `unsigned long`s from the next aligned word.
    put_word(&mut out, width, pages, layout);
    put_word(&mut out, 2 * width, resident, layout);
    let bytes = out.get(..6 * width).ok_or(Errno::EINVAL)?;
    memory.write(arg, bytes)?;
    Ok(highest(caller))
}

/// `IPC_STAT`, and `SHM_STAT` and `SHM_STAT_ANY`, whose `id` is a slot and
/// which answer the segment's id.
fn stat(
    caller: &Caller,
    memory: &dyn Memory,
    layout: Layout,
    id: i32,
    cmd: i32,
    arg: u64,
) -> Result<usize, Errno> {
    let segment = if cmd == IPC_STAT {
        lookup(&caller.ns, id)
    } else {
        let slot = usize::try_from(id).map_err(|_| Errno::EINVAL)?;
        caller
            .ns
            .shm
            .lock()
            .slots
            .get(slot)
            .and_then(|slot| slot.segment.as_ref())
            .map(Arc::clone)
    }
    .ok_or(Errno::EINVAL)?;
    let bytes = {
        let state = segment.state.lock();
        if cmd != SHM_STAT_ANY && !state.perm.allows(caller, READ) {
            return Err(Errno::EACCES);
        }
        encode_shmid(&segment, &state, layout)
    };
    let answer = bytes.get(..shmid_bytes(layout)).ok_or(Errno::EINVAL)?;
    memory.write(arg, answer)?;
    Ok(if cmd == IPC_STAT {
        0
    } else {
        segment.id as usize
    })
}

/// `IPC_SET`: the owner, group and mode from the caller's `shmid64_ds`.
fn set_perm(caller: &Caller, memory: &dyn Memory, id: i32, arg: u64) -> Result<usize, Errno> {
    let (uid, gid, mode) = sem::read_perm(memory, arg)?;
    let segment = lookup(&caller.ns, id).ok_or(Errno::EINVAL)?;
    let mut state = segment.state.lock();
    if !state.perm.owned_by(caller) {
        return Err(Errno::EPERM);
    }
    state.perm.uid = uid;
    state.perm.gid = gid;
    state.perm.mode = mode;
    state.ctime = sem::now_seconds();
    Ok(0)
}

/// `IPC_RMID`: free the key, and take the segment out now if nothing has it
/// attached, else when its last attach goes.
fn remove(caller: &Caller, id: i32) -> Result<usize, Errno> {
    let segment = lookup(&caller.ns, id).ok_or(Errno::EINVAL)?;
    let now = {
        let mut state = segment.state.lock();
        if state.gone {
            return Err(Errno::EINVAL);
        }
        if !state.perm.owned_by(caller) {
            return Err(Errno::EPERM);
        }
        state.removed = true;
        segment.removed.store(true, Ordering::Release);
        let now = nattch(&state) == 0;
        if now {
            state.gone = true;
        }
        now
    };
    if now {
        take_out(&segment);
    }
    Ok(0)
}

/// `SHM_LOCK` and `SHM_UNLOCK`: recorded for `IPC_STAT`, and nothing else --
/// no page here is ever swapped out.
fn lock(caller: &Caller, id: i32, locked: bool) -> Result<usize, Errno> {
    let segment = lookup(&caller.ns, id).ok_or(Errno::EINVAL)?;
    let mut state = segment.state.lock();
    if !state.perm.owned_by(caller) {
        return Err(Errno::EPERM);
    }
    state.locked = locked;
    Ok(0)
}

// ---------------------------------------------------------------------------
// The system calls
// ---------------------------------------------------------------------------

/// The shared memory calls; `None` for any other.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let int = |value: u64| value as u32 as i32;
    // A 32-bit caller's `size_t` and pointers are 32-bit words.
    let narrow = Layout::of(abi) == Layout::Narrow;
    let word = |value: u64| if narrow { value & 0xFFFF_FFFF } else { value };
    let answer = match call {
        Syscall::Shmget => sys_shmget(process, int(a[0]), word(a[1]), int(a[2])),
        Syscall::Shmat => shmat(process, int(a[0]), word(a[1]), int(a[2]), shmlba(abi))
            .and_then(|at| usize::try_from(at).map_err(|_| Errno::EINVAL)),
        Syscall::Shmdt => shmdt(process, word(a[0])),
        Syscall::Shmctl => sys_shmctl(process, abi, int(a[0]), int(a[1]), word(a[2])),
        _ => return None,
    };
    Some(answer)
}

/// `shmget`.
fn sys_shmget(process: &Process, key: i32, size: u64, flags: i32) -> Result<usize, Errno> {
    shmget(&Caller::of(process), key, size, flags).map(|id| id as usize)
}

/// `shmctl`.
fn sys_shmctl(process: &Process, abi: Abi, id: i32, cmd: i32, arg: u64) -> Result<usize, Errno> {
    shmctl(&Caller::of(process), process, Layout::of(abi), id, cmd, arg)
}

/// i386's `ipc(call, first, second, third, ptr, fifth)` for the shared
/// memory operations, as Linux's `compat_ksys_ipc` unpacks them; `None` for
/// any other operation. `SHMAT` stores the address at `third` and answers
/// zero, and its version 1, the iBCS2 one, is `EINVAL`; `SHMCTL`'s `cmd`
/// carries `IPC_64` as the direct call's does.
pub(crate) fn sys_ipc(process: &Process, abi: Abi, a: &[u64; 6]) -> Option<Result<usize, Errno>> {
    let [call, first, second, third, ptr, _] = *a;
    let operation = call as u32 & 0xFFFF;
    let version = call as u32 >> 16;
    let first = first as u32 as i32;
    let ptr = ptr & 0xFFFF_FFFF;
    let answer = match operation {
        IPCOP_SHMAT if version == 1 => Err(Errno::EINVAL),
        IPCOP_SHMAT => {
            shmat(process, first, ptr, second as u32 as i32, shmlba(abi)).and_then(|at| {
                let at = u32::try_from(at).map_err(|_| Errno::EINVAL)?;
                uaccess::put_u32(process.space(), third & 0xFFFF_FFFF, at).map(|()| 0)
            })
        }
        IPCOP_SHMDT => shmdt(process, ptr),
        IPCOP_SHMGET => sys_shmget(process, first, second & 0xFFFF_FFFF, third as u32 as i32),
        IPCOP_SHMCTL => sys_shmctl(process, abi, first, second as u32 as i32, ptr),
        _ => return None,
    };
    Some(answer)
}

/// Segments in the first namespace's table, for the checks: every one they
/// make must be gone again.
pub(crate) fn segments_in_use() -> usize {
    sem::initial_ipc().shm.lock().segments
}

/// The attach count of segment `id` in `caller`'s namespace, for the
/// checks; `None` once it is out of the table.
pub(crate) fn attaches(caller: &Caller, id: i32) -> Option<usize> {
    let segment = lookup(&caller.ns, id)?;
    let state = segment.state.lock();
    Some(nattch(&state))
}
