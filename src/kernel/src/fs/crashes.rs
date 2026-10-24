//! `/dev/ferrix-crashes`: a process a fault ends, held for a moment so that
//! a root collector (`lode-crashd`, `docs/LOGGING.md` §21) can read its
//! threads, status, maps and descriptors before it goes.
//!
//! Linux holds a crashing process while its `core_pattern` pipe reads the
//! dump; this is that hold without the dump. What it does (the consultant's
//! review, ledger lines 3501 and 3524):
//!
//! * **Armed by an open.** One open file description at a time, read-write,
//!   by a caller privileged in the first user namespace, checked by device
//!   number wherever the node is (`devfs::attach_device`); a second open is
//!   `EBUSY`. The last close of that description disarms, and releases every
//!   process held at once. A description held by another process, or in
//!   flight in a socket, keeps it armed; each hold is still bounded.
//! * **Unarmed, nothing changes.** A fault looks at one atomic and goes on
//!   as before (LOGGING.md §21.7 crash-1).
//! * **The hold** (LOGGING.md §21.7 crash-2). A fatal fault of a program's own
//!   instruction, while armed, parks the process -- a third park beside a
//!   stop and a freeze, which `SIGCONT` and a thaw do not undo and no parent
//!   is told of -- and the faulting thread waits in the kernel until the
//!   reader writes `done <pid>`, the reader closes, the process is ended, or
//!   [`HOLD_NANOS`] pass from the fault. Then the process ends as it would
//!   have. Every end while held takes the fault's signal
//!   (`syscall::process::Process::end`), `SIGKILL` and an `exit_group`
//!   included.
//! * **The queue** (LOGGING.md §21.7 crash-3). At most [`SLOTS`] processes held at once,
//!   in a fixed ring that is never allocated; a fault past them is not held
//!   and counted in the next line. Never held: the reader's own process, any
//!   process whose descriptors hold the reader's description, a process an
//!   `execve` has claimed, and a thread already leaving.
//! * **Reading**: one whole line a `read`, as `/dev/kmsg` gives a record
//!   (`EINVAL` for a buffer it does not fit, `EAGAIN` under `O_NONBLOCK`,
//!   else a wait a signal ends): `held PID TID SIGNAL CODE ADDRESS PC threads
//!   N unparked M dropped D`, pids as the first namespace numbers them,
//!   written once every other live thread has parked or after
//!   [`SETTLE_NANOS`], `unparked` saying how many had not. `poll` is
//!   readable while a line is unread, and never hangs up.
//! * **Writing**: whole lines of `done PID`, each releasing that pid;
//!   `EINVAL` for a pid not held, a malformed line or a partial one, with
//!   nothing kept between writes.

use alloc::sync::Arc;
use core::any::Any;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileType, Inode, Metadata, Readiness, Result, Timespec, WakeSource};

use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::thread::Thread;
use crate::syscall::{credentials, process, registry};

/// The node's device number: major 1, minor 13, Ferrix's own (Linux leaves
/// 1:13 unassigned; it moves if Linux assigns it).
pub(crate) const MINOR: u32 = 13;

/// How many processes may be held at once.
pub(crate) const SLOTS: usize = 16;

/// The longest a fault is held, from the fault: 2 s.
pub(crate) const HOLD_NANOS: u64 = 2_000_000_000;

/// The longest the line waits for the other threads to park, within the hold.
pub(crate) const SETTLE_NANOS: u64 = 200_000_000;

/// The longest line: `held` and nine numbers.
const LINE_MAX: usize = 160;

/// Whether a reader holds the device open. The one look an unarmed fault
/// takes.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The ring, and who armed it.
static RING: SpinLock<Ring> = SpinLock::new(Ring::EMPTY);

/// Whether each slot was released, read by the faulting thread's wait
/// without the ring's lock.
static RELEASED: [AtomicBool; SLOTS] = [const { AtomicBool::new(false) }; SLOTS];

/// Woken when a line is queued: the reader's wait, and its `poll`.
static QUEUED: WaitQueue = WaitQueue::new();

/// Woken when a thread parks for a hold: the faulting thread's settle.
static PARKED: WaitQueue = WaitQueue::new();

/// Moves with every line queued, read or released, for `epoll`.
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// Where a slot is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nobody's.
    Free,
    /// Taken by a fault whose other threads are parking.
    Settling,
    /// Its line is ready and unread.
    Queued,
    /// Its line has been read.
    Read,
    /// Released by `done` or by the reader's close.
    Released,
}

/// One held fault.
#[derive(Debug, Clone, Copy)]
struct Slot {
    phase: Phase,
    /// The order lines were queued in.
    seq: u64,
    pid: u32,
    tid: u32,
    signal: u32,
    code: i32,
    address: u64,
    pc: u64,
    threads: u32,
    unparked: u32,
}

impl Slot {
    const FREE: Slot = Slot {
        phase: Phase::Free,
        seq: 0,
        pid: 0,
        tid: 0,
        signal: 0,
        code: 0,
        address: 0,
        pc: 0,
        threads: 0,
        unparked: 0,
    };
}

/// The fixed ring.
#[derive(Debug)]
struct Ring {
    /// Whether an open description arms it: set after [`ARMED`]'s exchange,
    /// cleared by the last close.
    open: bool,
    /// The arming process's pid; zero for the kernel's own check.
    armer: u32,
    /// The arming description's object, to find it in a descriptor table.
    file: usize,
    slots: [Slot; SLOTS],
    next_seq: u64,
    /// Faults not held because the ring was full, since the last line.
    dropped: u32,
}

impl Ring {
    const EMPTY: Ring = Ring {
        open: false,
        armer: 0,
        file: 0,
        slots: [Slot::FREE; SLOTS],
        next_seq: 0,
        dropped: 0,
    };
}

/// Whether the caller may open the device: privileged in the first user
/// namespace. A check with no process behind it may.
///
/// # Errors
///
/// `EPERM` for a caller without the privilege; `EINVAL` for an open that is
/// not read-write (LOGGING.md §21.7 crash-4).
pub(crate) fn may_open(readable: bool, writable: bool) -> Result<()> {
    if !(readable && writable) {
        return Err(Errno::EINVAL);
    }
    privileged()
}

/// The privilege half of [`may_open`], asked again where the object is made,
/// so that only a privileged caller ever arms, even for the moment before a
/// read-only open is refused.
fn privileged() -> Result<()> {
    match process::current() {
        Some(caller) => credentials::require_privilege(&caller),
        None => Ok(()),
    }
}

/// A thread parked for a hold: the faulting thread's settle looks again.
pub(crate) fn thread_parked() {
    PARKED.wake_all();
}

/// Hold the running thread's process for its fatal fault while a reader is
/// armed, and answer whether it was held. Called by the fault path before
/// the process is ended, with interrupts open and no lock held; returns once
/// it is released, or at once when it is not to be held.
pub(crate) fn hold(thread: &Thread, signal: u32, code: i32, address: u64, pc: u64) -> bool {
    if !ARMED.load(Ordering::Acquire) {
        return false;
    }
    let process = thread.process();
    if process.must_leave(thread) || process.exec_claimed() {
        return false;
    }
    let file = RING.lock().file;
    // Fail closed: a process holding the reader's description is the
    // reader's, however it came by it.
    let shares = process
        .files()
        .lock()
        .iter()
        .any(|(_, open)| Arc::as_ptr(open.io()).cast::<()>() as usize == file);
    if shares {
        return false;
    }
    let pid = process.pid();
    let start = crate::timer::now_nanos();
    let deadline = start.saturating_add(HOLD_NANOS);
    let index = {
        let mut ring = RING.lock();
        if !ring.open || ring.file != file || ring.armer == pid {
            return false;
        }
        let Some(index) = ring.slots.iter().position(|slot| slot.phase == Phase::Free) else {
            ring.dropped = ring.dropped.saturating_add(1);
            return false;
        };
        let Some(slot) = ring.slots.get_mut(index) else {
            return false;
        };
        *slot = Slot {
            phase: Phase::Settling,
            pid,
            tid: thread.tid(),
            signal,
            code,
            address,
            pc,
            ..Slot::FREE
        };
        if let Some(released) = RELEASED.get(index) {
            released.store(false, Ordering::Release);
        }
        index
    };
    // Held from here: every end takes the fault's signal.
    process.set_held(signal);
    let released = || {
        RELEASED
            .get(index)
            .is_none_or(|released| released.load(Ordering::Acquire))
            || !ARMED.load(Ordering::Acquire)
            || process.must_leave(thread)
    };
    let settle = start.saturating_add(SETTLE_NANOS).min(deadline);
    let _ = WaitQueue::wait_on_any(
        &[&PARKED, process.resumed()],
        || {
            let (live, parked) = process.held_threads();
            parked.saturating_add(1) >= live || released()
        },
        settle,
        1_000_000,
    );
    let (live, parked) = process.held_threads();
    {
        let mut ring = RING.lock();
        let seq = ring.next_seq;
        ring.next_seq = seq.wrapping_add(1);
        if let Some(slot) = ring.slots.get_mut(index) {
            if slot.phase == Phase::Settling {
                slot.phase = Phase::Queued;
            }
            slot.seq = seq;
            slot.threads = live;
            slot.unparked = live.saturating_sub(parked.saturating_add(1));
        }
    }
    let _ = CHANGES.fetch_add(1, Ordering::AcqRel);
    QUEUED.wake_all();
    debug_assert!(
        crate::sched::may_block(),
        "a crash hold waits with a lock held"
    );
    let _ = process.resumed().wait_until_deadline(released, deadline);
    if let Some(slot) = RING.lock().slots.get_mut(index) {
        *slot = Slot::FREE;
    }
    let _ = CHANGES.fetch_add(1, Ordering::AcqRel);
    true
}

/// The pids of every held slot, released at once: the reader's close.
fn release_all() {
    let mut pids = [0_u32; SLOTS];
    {
        let mut ring = RING.lock();
        ring.open = false;
        ring.file = 0;
        ring.armer = 0;
        ring.dropped = 0;
        ARMED.store(false, Ordering::Release);
        for ((slot, pid), released) in ring.slots.iter_mut().zip(pids.iter_mut()).zip(&RELEASED) {
            if slot.phase != Phase::Free {
                slot.phase = Phase::Released;
                released.store(true, Ordering::Release);
                *pid = slot.pid;
            }
        }
    }
    let _ = CHANGES.fetch_add(1, Ordering::AcqRel);
    for pid in pids.into_iter().filter(|&pid| pid != 0) {
        if let Some(held) = registry::find(pid) {
            held.resumed().wake_all();
        }
    }
}

/// One open of `/dev/ferrix-crashes`: the armed reader.
#[derive(Debug)]
pub(crate) struct CrashFile {
    /// Whether this open armed the device: false for one refused `EBUSY`,
    /// whose drop must not disarm the reader that holds it.
    arms: AtomicBool,
}

impl CrashFile {
    /// Arm the device, or `EBUSY` when it is armed already (LOGGING.md §21.7 crash-4).
    ///
    /// # Errors
    ///
    /// `EPERM` for an unprivileged caller; `EBUSY`; `ENOMEM` for the object,
    /// which leaves it unarmed.
    pub(crate) fn open() -> Result<Arc<CrashFile>> {
        privileged()?;
        let file = crate::fallible::try_arc(CrashFile {
            arms: AtomicBool::new(false),
        })
        .map_err(|_| Errno::ENOMEM)?;
        if ARMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Errno::EBUSY);
        }
        file.arms.store(true, Ordering::Release);
        let mut ring = RING.lock();
        ring.open = true;
        ring.armer = process::current().map_or(0, |caller| caller.pid());
        ring.file = Arc::as_ptr(&file).cast::<()>() as usize;
        ring.dropped = 0;
        Ok(file)
    }

    /// The oldest unread line into `buf`, if there is one.
    fn take(&self, buf: &mut [u8]) -> Option<Result<usize>> {
        let mut line = heapless_line();
        {
            let mut ring = RING.lock();
            let dropped = ring.dropped;
            let slot = ring
                .slots
                .iter_mut()
                .filter(|slot| slot.phase == Phase::Queued)
                .min_by_key(|slot| slot.seq)?;
            let _ = writeln!(
                line,
                "held {} {} {} {} {:#x} {:#x} threads {} unparked {} dropped {}",
                slot.pid,
                slot.tid,
                slot.signal,
                slot.code,
                slot.address,
                slot.pc,
                slot.threads,
                slot.unparked,
                dropped
            );
            if buf.len() < line.len {
                return Some(Err(Errno::EINVAL));
            }
            slot.phase = Phase::Read;
            ring.dropped = 0;
        }
        let _ = CHANGES.fetch_add(1, Ordering::AcqRel);
        let room = buf.get_mut(..line.len)?;
        room.copy_from_slice(line.bytes());
        Some(Ok(line.len))
    }

    /// Release the held `pid`, by a `done` line.
    fn done(pid: u32) -> Result<()> {
        {
            let mut ring = RING.lock();
            let index = ring
                .slots
                .iter()
                .position(|slot| {
                    slot.pid == pid && matches!(slot.phase, Phase::Queued | Phase::Read)
                })
                .ok_or(Errno::EINVAL)?;
            if let (Some(slot), Some(released)) = (ring.slots.get_mut(index), RELEASED.get(index)) {
                slot.phase = Phase::Released;
                released.store(true, Ordering::Release);
            }
        }
        let _ = CHANGES.fetch_add(1, Ordering::AcqRel);
        if let Some(held) = registry::find(pid) {
            held.resumed().wake_all();
        }
        Ok(())
    }
}

impl Drop for CrashFile {
    /// The last close of the arming description: disarmed, and every held
    /// process released at once (LOGGING.md §21.7 crash-2).
    fn drop(&mut self) {
        if *self.arms.get_mut() {
            release_all();
        }
    }
}

/// A line's room, on the stack: the fault path and the reader allocate
/// nothing for it.
struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl Line {
    fn bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or_default()
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(core::fmt::Error)?;
        self.buf
            .get_mut(self.len..end)
            .ok_or(core::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn heapless_line() -> Line {
    Line {
        buf: [0; LINE_MAX],
        len: 0,
    }
}

/// The pids of whole `done PID` lines, or `EINVAL` for anything else.
fn parse_done(data: &[u8], mut each: impl FnMut(u32) -> Result<()>) -> Result<()> {
    if data.last() != Some(&b'\n') {
        return Err(Errno::EINVAL);
    }
    for line in data
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let digits = line.strip_prefix(b"done ").ok_or(Errno::EINVAL)?;
        if digits.is_empty() || digits.len() > 10 || !digits.iter().all(u8::is_ascii_digit) {
            return Err(Errno::EINVAL);
        }
        let pid = digits
            .iter()
            .try_fold(0_u32, |pid, &digit| {
                pid.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
            })
            .ok_or(Errno::EINVAL)?;
        each(pid)?;
    }
    Ok(())
}

impl Inode for CrashFile {
    fn metadata(&self) -> Metadata {
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        Metadata {
            ino: 0,
            kind: FileType::CharDevice,
            permissions: 0o400,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: makedev(1, MINOR),
            blocks: 0,
            block_size: 4096,
            atime: zero,
            mtime: zero,
            ctime: zero,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.read_stream(buf, false)
    }

    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize> {
        let caller = process::current();
        loop {
            if let Some(answer) = self.take(buf) {
                return answer;
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let Some(caller) = caller.as_ref() else {
                return Err(Errno::EAGAIN);
            };
            let seen = CHANGES.load(Ordering::Acquire);
            let _ = WaitQueue::wait_on_any(
                &[&QUEUED, caller.signalled()],
                || CHANGES.load(Ordering::Acquire) != seen || caller.signal_pending(),
                u64::MAX,
                crate::fs::wake::TRUSTED_RECHECK_NANOS,
            );
            if caller.signal_pending() {
                return Err(Errno::ERESTARTSYS);
            }
        }
    }

    fn write_at(&self, _offset: u64, data: &[u8], _append: bool) -> Result<(usize, u64)> {
        self.write_stream(data, false).map(|count| (count, 0))
    }

    fn write_stream(&self, data: &[u8], _nonblock: bool) -> Result<usize> {
        parse_done(data, CrashFile::done)?;
        Ok(data.len())
    }

    fn poll(&self) -> Readiness {
        let readable = RING
            .lock()
            .slots
            .iter()
            .any(|slot| slot.phase == Phase::Queued);
        Readiness {
            readable,
            writable: true,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(CHANGES.load(Ordering::Acquire))
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(WakeSource)) -> bool {
        visit(crate::fs::wake::lent(&QUEUED));
        true
    }

    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn splices_out(&self) -> bool {
        false
    }
}
