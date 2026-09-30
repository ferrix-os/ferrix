//! Advisory locks: `flock`'s whole-file locks, and `fcntl`'s record locks.
//!
//! # Whose lock it is
//!
//! A `flock` lock belongs to the open file description, not to the process and
//! not to the descriptor number. Two descriptors made by `dup`, or inherited
//! across `fork`, share one description and so one lock: either may convert or
//! release it, and it lasts until the last of them is closed. Two separate
//! `open`s of the same file are two descriptions, and contend. That is what
//! busybox's `flock FILE -c CMD` relies on: it opens the file, locks it, and
//! forks the command, which inherits the description and the lock with it.
//!
//! A record lock has one of two owners. A classic one (`F_SETLK`) belongs to
//! the process -- to its descriptor table, as Linux's `fl_owner` is
//! `current->files` -- so a fork child does not inherit it, and any `close` by
//! that process of a descriptor on the file releases it, whichever descriptor
//! set it. An open file description lock (`F_OFD_SETLK`) belongs to the
//! description, as a `flock` lock does. The two kinds conflict with each
//! other, even within one process, as on Linux. `flock` locks and record locks
//! never meet.
//!
//! # Released when the owner goes, with the smallest hook in it
//!
//! The tables hold [`Weak`] references to their owners, and every look at one
//! first sweeps out the entries whose owner is gone. So a `flock` or OFD lock
//! ends when its description does -- on `close`, on `exit`, on `execve`
//! closing a close-on-exec descriptor -- without `ferrix_vfs::OpenFile` having
//! to know that locks exist. Nothing is woken at that moment, because nothing
//! of this module runs then; a waiter sees the lock gone at its wait's next
//! recheck, which `WaitQueue::wait_until_deadline` makes every few
//! milliseconds whether or not it is woken.
//!
//! A classic record lock cannot wait for its owner to go, because its owner --
//! the descriptor table -- outlives the `close` that must release it. So the
//! four places a descriptor is closed call [`closed`]: `close`, a `dup2` or
//! `dup3` that displaces one, `execve`'s close-on-exec, and exit.
//!
//! The tables never upgrade a reference. Liveness is read with
//! [`Weak::strong_count`] and identity by address, so a table's lock is never
//! where an owner's last strong reference is dropped -- which would run a
//! description's teardown, and a pipe end's wake-ups, under it.
//!
//! # Which file
//!
//! A lock is on the file, so it is keyed by the filesystem the description's
//! mount belongs to and the inode number within it -- not by the inode's
//! `Arc`, of which a filesystem may hand out more than one for one file. The
//! filesystem's address cannot be reused by another filesystem while a holder
//! is live, because the holder's description keeps its mount, and so the
//! filesystem, alive; and dead holders are swept before any key is compared.
//!
//! # A conversion is not atomic
//!
//! Asking for a `flock` lock a description already holds in the other mode
//! first gives up the one it has, then asks, as Linux's `flock_lock_inode`
//! does. A `LOCK_NB` conversion that is refused therefore leaves the
//! description with no lock at all, which is what `flock(2)` documents.
//!
//! # What record locks do not do
//!
//! `F_SETLKW` does not detect deadlock: two processes each waiting for the
//! other's classic lock wait until a signal, where Linux answers one of them
//! `EDEADLK`.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use ferrix_kmem::Charge;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    F_GETLK, F_GETLK64, F_OFD_GETLK, F_OFD_SETLK, F_OFD_SETLKW, F_RDLCK, F_SETLK, F_SETLK64,
    F_SETLKW, F_SETLKW64, F_UNLCK, F_WRLCK, LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN, SEEK_CUR, SEEK_END,
    SEEK_SET,
};
use ferrix_vfs::fd::FdTable;
use ferrix_vfs::{OpenFile, Whence};

use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::{fd, signal};
use crate::trap::Abi;

pub(crate) mod check;
use crate::syscall::process::Process;
use crate::syscall::uaccess;

/// A wait with no deadline of its own: a release or a signal ends it.
const FOREVER: u64 = u64::MAX;

/// The file a lock is on. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    /// The filesystem instance's address, as a number.
    filesystem: usize,
    /// The inode number within it.
    ino: u64,
}

/// The two modes a lock is held in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `LOCK_SH`: any number of descriptions at once.
    Shared,
    /// `LOCK_EX`: one description, with no shared holder beside it.
    Exclusive,
}

/// One description's lock on one file.
#[derive(Debug)]
struct Holder {
    /// The file.
    key: Key,
    /// The description holding it. Dead once the description is dropped.
    owner: Weak<OpenFile>,
    /// How it is held.
    mode: Mode,
}

/// What one look at the table found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// The description holds the lock in the mode it asked for.
    Taken,
    /// Another description holds a lock that conflicts with it.
    Contended,
}

/// Every lock held, on every file. A list rather than a map: a system holds a
/// handful at once, and a sweep has to visit every holder anyway.
static HOLDERS: SpinLock<Vec<Holder>> = SpinLock::new(Vec::new());

/// Woken when a call releases, converts or narrows a lock of either kind.
static RELEASED: WaitQueue = WaitQueue::new();

/// `flock(fd, operation)`.
///
/// # Errors
///
/// `EBADF` for a descriptor that names nothing, or only a name (`O_PATH`);
/// `EINVAL` for an operation that is not `LOCK_SH`, `LOCK_EX` or `LOCK_UN`,
/// with or without `LOCK_NB`; `EAGAIN` -- `EWOULDBLOCK` -- when `LOCK_NB` meets
/// a conflicting lock; `EINTR` when a handler runs during the wait, which a
/// stop and a continue, running none, take up again instead.
pub(crate) fn sys_flock(process: &Process, fd: i32, operation: u32) -> Result<usize, Errno> {
    // Held for the whole call, as Linux's `fdget` holds the file: a `close`
    // on another thread must not end the description out from under a wait.
    let file = fd::file(process, fd)?;
    if file.is_path() {
        return Err(Errno::EBADF);
    }
    let mode = match operation & !LOCK_NB {
        LOCK_SH => Mode::Shared,
        LOCK_EX => Mode::Exclusive,
        LOCK_UN => {
            release(&file);
            RELEASED.wake_all();
            return Ok(0);
        }
        _ => return Err(Errno::EINVAL),
    };
    let key = key_of(&file);
    let owner = Arc::downgrade(&file);

    let (first, converted) = attempt(key, &owner, mode);
    if converted {
        RELEASED.wake_all();
    }
    if first == Attempt::Taken {
        return Ok(0);
    }
    if operation & LOCK_NB != 0 {
        return Err(Errno::EAGAIN);
    }

    // A restart code, as Linux's lock waits answer: `EINTR` once a handler
    // runs, and the wait taken up again when none does.
    let mut outcome = Err(Errno::ERESTARTSYS);
    let mut converted = false;
    let _ = RELEASED.wait_until_deadline(
        || {
            let (now, gave_up) = attempt(key, &owner, mode);
            converted |= gave_up;
            if now == Attempt::Taken {
                outcome = Ok(0);
                return true;
            }
            process.signal_pending()
        },
        FOREVER,
    );
    // Only if another descriptor sharing this description took the other
    // mode while this one waited, and this call then converted it.
    if converted {
        RELEASED.wake_all();
    }
    outcome
}

/// One look at the table for `owner`, which wants `key` in `mode`.
///
/// Sweeps out the dead holders, gives up `owner`'s lock if it holds one in the
/// other mode, and takes `mode` if nothing else conflicts. Answers what it
/// found, and whether it gave a lock up.
fn attempt(key: Key, owner: &Weak<OpenFile>, mode: Mode) -> (Attempt, bool) {
    let mut holders = HOLDERS.lock();
    holders.retain(|holder| holder.owner.strong_count() > 0);
    let mut converted = false;
    if let Some(at) = holders
        .iter()
        .position(|holder| Weak::ptr_eq(&holder.owner, owner))
    {
        if holders.get(at).is_some_and(|holder| holder.mode == mode) {
            return (Attempt::Taken, false);
        }
        let _ = holders.swap_remove(at);
        converted = true;
    }
    let contended = holders.iter().any(|holder| {
        holder.key == key && (mode == Mode::Exclusive || holder.mode == Mode::Exclusive)
    });
    if contended {
        return (Attempt::Contended, converted);
    }
    holders.push(Holder {
        key,
        owner: Weak::clone(owner),
        mode,
    });
    (Attempt::Taken, converted)
}

/// Give up whatever lock `file` holds, sweeping the dead holders as it goes.
fn release(file: &Arc<OpenFile>) {
    let mine = Arc::as_ptr(file);
    HOLDERS.lock().retain(|holder| {
        holder.owner.strong_count() > 0 && !core::ptr::eq(holder.owner.as_ptr(), mine)
    });
}

/// The file `file` is open on.
fn key_of(file: &OpenFile) -> Key {
    Key {
        filesystem: Arc::as_ptr(file.location().mount.filesystem())
            .cast::<()>()
            .addr(),
        ino: file.inode().metadata().ino,
    }
}

// ---------------------------------------------------------------------------
// Record locks
// ---------------------------------------------------------------------------

/// A process's descriptor table: what owns its classic record locks.
type Table = SpinLock<FdTable<Arc<OpenFile>>>;

/// Linux's `OFFSET_MAX`: the last byte of a lock that runs to the end of the
/// file however far the file grows.
const OFFSET_MAX: u64 = i64::MAX as u64;

/// Who holds a record lock.
#[derive(Debug, Clone)]
enum Owner {
    /// A classic lock: the descriptor table of the process that set it, and
    /// that process's pid, which `F_GETLK` reports.
    Table(Weak<Table>, u32),
    /// An open file description lock, which `F_GETLK` reports with pid -1.
    Description(Weak<OpenFile>),
}

impl Owner {
    /// Whether the owner still exists.
    fn alive(&self) -> bool {
        match self {
            Owner::Table(table, _) => table.strong_count() > 0,
            Owner::Description(file) => file.strong_count() > 0,
        }
    }

    /// Whether two owners are one: a lock never conflicts with its own owner's.
    fn same(&self, other: &Owner) -> bool {
        match (self, other) {
            (Owner::Table(mine, _), Owner::Table(theirs, _)) => Weak::ptr_eq(mine, theirs),
            (Owner::Description(mine), Owner::Description(theirs)) => Weak::ptr_eq(mine, theirs),
            _ => false,
        }
    }

    /// The pid `F_GETLK` reports for a lock of this owner's.
    fn pid(&self) -> i32 {
        match self {
            Owner::Table(_, pid) => i32::try_from(*pid).unwrap_or(-1),
            Owner::Description(_) => -1,
        }
    }
}

/// One owner's lock on a range of one file.
#[derive(Debug)]
struct Record {
    /// The file.
    key: Key,
    /// Who holds it.
    owner: Owner,
    /// `F_WRLCK` rather than `F_RDLCK`.
    exclusive: bool,
    /// Its first byte.
    start: u64,
    /// Its last byte, inclusive; [`OFFSET_MAX`] to the end of the file.
    end: u64,
    /// Its slot in [`RECORDS`], charged to the job that set it (F-37): one
    /// owner may lock any number of disjoint ranges, as on Linux, whose
    /// `file_lock_cache` is charged the same way. A piece a split leaves
    /// keeps the charge of the lock it came from.
    charge: Charge,
}

/// Charge the running task's job for one record: what a lock or an unlock
/// that splits a lock in two adds. `ENOLCK` past its limit, as Linux answers
/// a lock it has no memory for.
fn record_charge() -> Result<Charge, Errno> {
    Charge::bytes(size_of::<Record>()).map_err(|_| Errno::ENOLCK)
}

impl Record {
    /// Whether it and a lock of `owner` on `key` over `start..=end` cannot both
    /// be held.
    fn conflicts(&self, key: Key, owner: &Owner, exclusive: bool, start: u64, end: u64) -> bool {
        self.key == key
            && !self.owner.same(owner)
            && self.start <= end
            && start <= self.end
            && (exclusive || self.exclusive)
    }
}

/// Every record lock held, on every file.
static RECORDS: SpinLock<Vec<Record>> = SpinLock::new(Vec::new());

/// Where a `struct flock`'s fields are, and how wide its offsets.
///
/// From `asm-generic/fcntl.h`, which none of the three architectures overrides
/// (QEMU's `linux-user/generic/fcntl.h` transcribes it the same way): two
/// `short`s, then `l_start` and `l_len`, then `l_pid`, each aligned to its own
/// size. Offsets are a `long` in `struct flock` and a `loff_t` in `struct
/// flock64`, so on a 64-bit build the two are one layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    /// Bytes in the structure, tail padding included.
    size: usize,
    /// Where `l_start` is; `l_len` follows it.
    start: usize,
    /// Where `l_pid` is.
    pid: usize,
    /// Whether the offsets are 64 bits.
    wide: bool,
}

/// `struct flock` on a 64-bit build, and `struct flock64` for every program
/// but an i386 one.
const WIDE_FLOCK: Layout = Layout {
    size: 32,
    start: 8,
    pid: 24,
    wide: true,
};

/// `struct flock` on ARMv7-A and i386: 32-bit offsets, no padding.
const NARROW_FLOCK: Layout = Layout {
    size: 16,
    start: 4,
    pid: 12,
    wide: false,
};

/// `struct flock64` for an i386 program: x86-32 aligns a `long long` to four,
/// so `l_start` follows the two `short`s directly and nothing pads the end --
/// Linux's `compat_flock64`, packed under `__ARCH_NEED_COMPAT_FLOCK64_PACKED`.
const I386_FLOCK64: Layout = Layout {
    size: 24,
    start: 4,
    pid: 20,
    wide: true,
};

/// What a record-lock command asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// `F_GETLK`: which lock, if any, would stop this one.
    Get,
    /// `F_SETLK`: take or release it now, or `EAGAIN`.
    Set,
    /// `F_SETLKW`: take it, waiting if necessary.
    Wait,
}

/// A `struct flock`, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flock {
    /// `l_type`.
    kind: i16,
    /// `l_whence`.
    whence: i16,
    /// `l_start`.
    start: i64,
    /// `l_len`.
    len: i64,
    /// `l_pid`.
    pid: i32,
}

/// What `cmd` is to `call` from a program of `abi`, or `None` if it is not a
/// record-lock command there.
///
/// The plain commands read `struct flock`, whose offsets are a `long`. For a
/// 32-bit program only `fcntl64` has the `64` commands and the OFD ones, which
/// read `struct flock64` -- Linux's `do_fcntl` takes `F_OFD_*` only where a
/// long is 64 bits, and says 32-bit architectures must use `fcntl64` -- so
/// plain `fcntl` gets `EINVAL` for them from `fd::sys_fcntl`, as on Linux.
fn command(cmd: u32, call: Syscall, abi: Abi) -> Option<(Request, bool, Layout)> {
    let narrow = signal::word_of(abi) == 4;
    let plain = if narrow { NARROW_FLOCK } else { WIDE_FLOCK };
    let flock64 = if abi == Abi::Compat {
        I386_FLOCK64
    } else {
        WIDE_FLOCK
    };
    let wide_call = !narrow || call == Syscall::Fcntl64;
    let request = match cmd {
        F_GETLK | F_GETLK64 | F_OFD_GETLK => Request::Get,
        F_SETLK | F_SETLK64 | F_OFD_SETLK => Request::Set,
        F_SETLKW | F_SETLKW64 | F_OFD_SETLKW => Request::Wait,
        _ => return None,
    };
    match cmd {
        F_GETLK | F_SETLK | F_SETLKW => Some((request, false, plain)),
        F_GETLK64 | F_SETLK64 | F_SETLKW64 if narrow && wide_call => {
            Some((request, false, flock64))
        }
        F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW if wide_call => Some((request, true, flock64)),
        _ => None,
    }
}

/// Whether `fcntl` or `fcntl64` with `cmd` is a record-lock command, which
/// [`sys_fcntl_lock`] answers.
pub(crate) fn is_record_lock(cmd: u32, call: Syscall, abi: Abi) -> bool {
    command(cmd, call, abi).is_some()
}

/// `fcntl(fd, F_GETLK | F_SETLK | F_SETLKW, lock)`, the `64` forms, and the
/// OFD forms.
///
/// # Errors
///
/// In Linux's order: `EBADF` for a closed or `O_PATH` descriptor; `EFAULT` for
/// a structure that cannot be read; `EINVAL` for a bad `l_whence` or `l_type`,
/// a range that starts before the file, or an OFD request whose `l_pid` is not
/// 0; `EOVERFLOW` for a range past `OFFSET_MAX`, or a lock `F_GETLK` cannot
/// describe in a 32-bit `struct flock`; `EBADF` for a read lock on a
/// descriptor not open for reading or a write lock on one not open for
/// writing; `EAGAIN` for `F_SETLK` meeting a conflicting lock; `EINTR` when a
/// handler runs during `F_SETLKW`'s wait, which a stop and a continue take up
/// again instead.
pub(crate) fn sys_fcntl_lock(
    process: &Process,
    fd: i32,
    cmd: u32,
    arg: u64,
    call: Syscall,
    abi: Abi,
) -> Result<usize, Errno> {
    let file = fd::file(process, fd)?;
    if file.is_path() {
        return Err(Errno::EBADF);
    }
    let (request, ofd, layout) = command(cmd, call, abi).ok_or(Errno::EINVAL)?;
    let mut raw = [0_u8; 32];
    let bytes = raw.get_mut(..layout.size).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), arg, bytes).map_err(|_| Errno::EFAULT)?;
    let flock = decode(bytes, layout);
    let owner = if ofd {
        Owner::Description(Arc::downgrade(&file))
    } else {
        Owner::Table(Arc::downgrade(process.files()), process.pid())
    };
    match request {
        Request::Get => {
            let reported = get_lock(&file, &owner, flock, layout, ofd)?;
            encode(reported, layout, bytes);
            uaccess::copy_to_user(process.space(), arg, bytes).map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        Request::Set | Request::Wait => {
            set_lock(process, &file, &owner, flock, ofd, request == Request::Wait)
        }
    }
}

/// `F_GETLK`: the first lock that would stop `flock`, described in its place,
/// or `flock` with `l_type` set to `F_UNLCK` and nothing else changed.
fn get_lock(
    file: &OpenFile,
    owner: &Owner,
    flock: Flock,
    layout: Layout,
    ofd: bool,
) -> Result<Flock, Errno> {
    let exclusive = match flock.kind {
        F_RDLCK => false,
        F_WRLCK => true,
        _ => return Err(Errno::EINVAL),
    };
    let (start, end) = range(file, flock)?;
    if ofd && flock.pid != 0 {
        return Err(Errno::EINVAL);
    }
    let key = key_of(file);
    let found = {
        let mut records = RECORDS.lock();
        records.retain(|record| record.owner.alive());
        records
            .iter()
            .find(|record| record.conflicts(key, owner, exclusive, start, end))
            .map(|record| {
                (
                    record.exclusive,
                    record.start,
                    record.end,
                    record.owner.pid(),
                )
            })
    };
    let Some((held_exclusive, held_start, held_end, pid)) = found else {
        return Ok(Flock {
            kind: F_UNLCK,
            ..flock
        });
    };
    let narrow_limit = u64::from(i32::MAX.unsigned_abs());
    if !layout.wide
        && (held_start > narrow_limit || (held_end != OFFSET_MAX && held_end > narrow_limit))
    {
        return Err(Errno::EOVERFLOW);
    }
    let len = if held_end == OFFSET_MAX {
        0
    } else {
        held_end - held_start + 1
    };
    Ok(Flock {
        kind: if held_exclusive { F_WRLCK } else { F_RDLCK },
        whence: SEEK_SET as i16,
        start: i64::try_from(held_start).unwrap_or(i64::MAX),
        len: i64::try_from(len).unwrap_or(i64::MAX),
        // The holder, as the asking process's namespace numbers it (-1 for a
        // description lock stays -1).
        pid: u32::try_from(pid).map_or(pid, |kernel| {
            i32::try_from(crate::syscall::pidns::show_pid(kernel)).unwrap_or(0)
        }),
    })
}

/// `F_SETLK` and `F_SETLKW`: take `flock`'s range for `owner`, or release it.
fn set_lock(
    process: &Process,
    file: &OpenFile,
    owner: &Owner,
    flock: Flock,
    ofd: bool,
    wait: bool,
) -> Result<usize, Errno> {
    let (start, end) = range(file, flock)?;
    let exclusive = match flock.kind {
        F_RDLCK if !file.readable() => return Err(Errno::EBADF),
        F_WRLCK if !file.writable() => return Err(Errno::EBADF),
        F_RDLCK => Some(false),
        F_WRLCK => Some(true),
        F_UNLCK => None,
        _ => return Err(Errno::EINVAL),
    };
    if ofd && flock.pid != 0 {
        return Err(Errno::EINVAL);
    }
    let key = key_of(file);
    // What the range may add: its own record, and one more when it falls
    // inside a lock and splits it. Charged before anything changes; whatever
    // is not used goes back as it drops.
    let mut spare = Some(record_charge()?);
    let Some(exclusive) = exclusive else {
        carve(&mut RECORDS.lock(), key, owner, start, end, &mut spare);
        RELEASED.wake_all();
        return Ok(0);
    };
    let mut charge = Some(record_charge()?);
    if place(key, owner, exclusive, start, end, &mut charge, &mut spare) {
        // Taking a range can narrow what the owner held there, from a write
        // lock to a read lock, which may let a waiter in.
        RELEASED.wake_all();
        return Ok(0);
    }
    if !wait {
        return Err(Errno::EAGAIN);
    }
    let mut placed = false;
    let _ = RELEASED.wait_until_deadline(
        || {
            placed = place(key, owner, exclusive, start, end, &mut charge, &mut spare);
            placed || process.signal_pending()
        },
        FOREVER,
    );
    if !placed {
        // A restart code, as for `flock` above.
        return Err(Errno::ERESTARTSYS);
    }
    RELEASED.wake_all();
    Ok(0)
}

/// Take `start..=end` of `key` for `owner` if nothing conflicts: replace
/// whatever `owner` held there, and merge the result with `owner`'s
/// neighbouring locks of the same type, as Linux's `posix_lock_inode` does.
///
/// `charge` pays for the new record and `spare` for a lock the range splits;
/// both are taken only when the range is placed, so a caller that waits
/// keeps them for its next try.
fn place(
    key: Key,
    owner: &Owner,
    exclusive: bool,
    start: u64,
    end: u64,
    charge: &mut Option<Charge>,
    spare: &mut Option<Charge>,
) -> bool {
    let mut records = RECORDS.lock();
    records.retain(|record| record.owner.alive());
    if records
        .iter()
        .any(|record| record.conflicts(key, owner, exclusive, start, end))
    {
        return false;
    }
    carve(&mut records, key, owner, start, end, spare);
    records.push(Record {
        key,
        owner: owner.clone(),
        exclusive,
        start,
        end,
        charge: charge.take().unwrap_or_default(),
    });
    coalesce(&mut records, key, owner);
    true
}

/// Remove `start..=end` from every lock `owner` holds on `key`, splitting a
/// lock the range falls inside into what is left either side. A lock cut on
/// one side keeps its charge; the second piece of one split in two takes
/// `spare`. At most one lock is split, since one owner's locks on a file
/// never overlap.
fn carve(
    records: &mut Vec<Record>,
    key: Key,
    owner: &Owner,
    start: u64,
    end: u64,
    spare: &mut Option<Charge>,
) {
    let mut left = Vec::new();
    let mut at = 0;
    while let Some(record) = records.get(at) {
        if record.key != key
            || !record.owner.same(owner)
            || record.end < start
            || end < record.start
        {
            at += 1;
            continue;
        }
        let Record {
            key: held,
            owner: holder,
            exclusive,
            start: first,
            end: last,
            charge,
        } = records.remove(at);
        let mut charge = Some(charge);
        if first < start {
            left.push(Record {
                key: held,
                owner: holder.clone(),
                exclusive,
                start: first,
                end: start - 1,
                charge: charge.take().or_else(|| spare.take()).unwrap_or_default(),
            });
        }
        if last > end {
            left.push(Record {
                key: held,
                owner: holder,
                exclusive,
                start: end + 1,
                end: last,
                charge: charge.take().or_else(|| spare.take()).unwrap_or_default(),
            });
        }
    }
    records.extend(left);
}

/// Merge `owner`'s locks on `key` of one type that overlap or touch.
fn coalesce(records: &mut Vec<Record>, key: Key, owner: &Owner) {
    loop {
        let mut pair = None;
        for (i, a) in records.iter().enumerate() {
            let j = records.iter().enumerate().skip(i + 1).find(|(_, b)| {
                a.key == key
                    && b.key == key
                    && a.owner.same(owner)
                    && b.owner.same(owner)
                    && a.exclusive == b.exclusive
                    && a.start <= b.end.saturating_add(1)
                    && b.start <= a.end.saturating_add(1)
            });
            if let Some((j, _)) = j {
                pair = Some((i, j));
                break;
            }
        }
        let Some((i, j)) = pair else {
            return;
        };
        // `j` is after `i`, so moving the last entry into `j` leaves `i` where
        // it was.
        let merged = records.swap_remove(j);
        if let Some(kept) = records.get_mut(i) {
            kept.start = kept.start.min(merged.start);
            kept.end = kept.end.max(merged.end);
        }
    }
}

/// The bytes `flock` names, as Linux's `flock_to_posix_lock` computes them:
/// from the start of the file, the current offset or the end; forward for a
/// positive length, backward for a negative one, to the end of the file for 0.
fn range(file: &OpenFile, flock: Flock) -> Result<(u64, u64), Errno> {
    let base = match u32::try_from(flock.whence).map_err(|_| Errno::EINVAL)? {
        SEEK_SET => 0,
        SEEK_CUR => file
            .seek(0, Whence::Current)
            .ok()
            .and_then(|at| i64::try_from(at).ok())
            .unwrap_or(0),
        SEEK_END => i64::try_from(file.inode().metadata().size).unwrap_or(i64::MAX),
        _ => return Err(Errno::EINVAL),
    };
    let start = base.checked_add(flock.start).ok_or(Errno::EOVERFLOW)?;
    if start < 0 {
        return Err(Errno::EINVAL);
    }
    let (first, last) = match flock.len {
        0 => (start, i64::MAX),
        len if len > 0 => (start, start.checked_add(len - 1).ok_or(Errno::EOVERFLOW)?),
        len => {
            let first = start.checked_add(len).ok_or(Errno::EINVAL)?;
            if first < 0 {
                return Err(Errno::EINVAL);
            }
            (first, start - 1)
        }
    };
    let first = u64::try_from(first).map_err(|_| Errno::EINVAL)?;
    let last = u64::try_from(last).map_err(|_| Errno::EINVAL)?;
    Ok((first, last))
}

/// A little-endian field of `bytes`, or zeros past its end.
fn field<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    bytes
        .get(at..at + N)
        .and_then(|slice| <[u8; N]>::try_from(slice).ok())
        .unwrap_or([0; N])
}

/// Read a `struct flock` of `layout`.
fn decode(bytes: &[u8], layout: Layout) -> Flock {
    let offset = |at: usize| {
        if layout.wide {
            i64::from_le_bytes(field(bytes, at))
        } else {
            i64::from(i32::from_le_bytes(field(bytes, at)))
        }
    };
    let width = if layout.wide { 8 } else { 4 };
    Flock {
        kind: i16::from_le_bytes(field(bytes, 0)),
        whence: i16::from_le_bytes(field(bytes, 2)),
        start: offset(layout.start),
        len: offset(layout.start + width),
        pid: i32::from_le_bytes(field(bytes, layout.pid)),
    }
}

/// Write `flock` over `bytes` in `layout`, leaving the padding as it was.
fn encode(flock: Flock, layout: Layout, bytes: &mut [u8]) {
    let mut put = |at: usize, value: &[u8]| {
        if let Some(slot) = bytes.get_mut(at..at + value.len()) {
            slot.copy_from_slice(value);
        }
    };
    put(0, &flock.kind.to_le_bytes());
    put(2, &flock.whence.to_le_bytes());
    if layout.wide {
        put(layout.start, &flock.start.to_le_bytes());
        put(layout.start + 8, &flock.len.to_le_bytes());
    } else {
        put(layout.start, &(flock.start as i32).to_le_bytes());
        put(layout.start + 4, &(flock.len as i32).to_le_bytes());
    }
    put(layout.pid, &flock.pid.to_le_bytes());
}

/// A descriptor on `file` was closed by `process`: its classic record locks on
/// the file go, as Linux's `locks_remove_posix` takes them on every close,
/// whichever descriptor set them.
pub(crate) fn closed(process: &Process, file: &OpenFile) {
    if RECORDS.lock().is_empty() {
        return;
    }
    // Outside the table's lock: reading an inode's metadata takes its own.
    let key = key_of(file);
    let table = Arc::as_ptr(process.files());
    let released = {
        let mut records = RECORDS.lock();
        let before = records.len();
        records.retain(|record| {
            let mine = matches!(&record.owner, Owner::Table(owner, _) if core::ptr::eq(owner.as_ptr(), table));
            record.owner.alive() && !(mine && record.key == key)
        });
        records.len() != before
    };
    if released {
        RELEASED.wake_all();
    }
}
