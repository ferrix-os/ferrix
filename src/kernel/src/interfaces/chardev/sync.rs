//! A `sync_file` a chardev driver made: one fence, as a descriptor in the
//! program it is answering (`docs/NVIDIA.md` §4.6, N3b sync; the
//! certification consultant's design verdict, ledger 647, on 316's base).
//!
//! nvidia-drm's `SEMSURF_FENCE_CREATE` answers a descriptor that becomes
//! readable once the GPU has reached a semaphore value. The driver serving
//! the render node through this core makes it with `chardev_sync_install`,
//! names it by a cookie, signals it with `chardev_sync_signal`, and asks
//! which of its fences a descriptor is with `chardev_sync_resolve`.
//!
//! # What holds
//!
//! * Signalled once: unsignalled to signalled, with 0 or a negative errno,
//!   under the object's lock; a second signal is refused (S2).
//! * Bounded whatever the driver does: each fence has a deadline, the
//!   driver's capped at [`SYNC_DEADLINE_MAX_MS`] (5 s when it gives 0), and
//!   a fence past it reads as signalled with `ETIMEDOUT` -- to poll and
//!   `SYNC_IOC_FILE_INFO` at once, and the `syncfiles` thread signals it and
//!   wakes its waiters. A control that goes signals every fence it made with
//!   `ENODEV` (S4).
//! * The fence holds its control weakly, as a dmabuf does: it keeps no
//!   claim, and a new control's resolve refuses an old control's fence.
//! * At most [`MAX_SYNC_FILES`] unsignalled fences per control (S7).
//!
//! [`SYNC_DEADLINE_MAX_MS`]: ferrix_native_abi::types::SYNC_DEADLINE_MAX_MS

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    SYNC_CLOEXEC, SYNC_DEADLINE_DEFAULT_MS, SYNC_DEADLINE_MAX_MS, SYNC_FLAGS,
};
use ferrix_vfs::{Inode, Metadata, Readiness, Result as VfsResult};

use crate::fs;
use crate::object::process::Host;
use crate::sched::{self, Task, WaitQueue};
use crate::sync::SpinLock;
use crate::syscall::process::Process;
use crate::syscall::uaccess;

use super::Control;

/// The most unsignalled fences one control has; the next install is
/// `LIMIT_REACHED` (S7).
pub(crate) const MAX_SYNC_FILES: usize = 4096;

/// What `/proc/self/fd` calls one, as Linux names it.
const NAME: &[u8] = b"anon_inode:sync_file";

/// The name `SYNC_IOC_FILE_INFO` gives, as the driver's own fences say.
const DRIVER_NAME: &[u8] = b"nvidia-drm";

/// `SYNC_IOC_FILE_INFO`: `_IOWR('>', 4, struct sync_file_info)`.
const SYNC_IOC_FILE_INFO: u32 = 0xC038_3E04;

/// `struct sync_file_info`: `name[32]`, `status`, `flags`, `num_fences`,
/// `pad`, `sync_fence_info`.
const FILE_INFO_BYTES: usize = 56;
/// `struct sync_fence_info`: `obj_name[32]`, `driver_name[32]`, `status`,
/// `flags`, `timestamp_ns`.
const FENCE_INFO_BYTES: usize = 80;
const _: () = assert!(FILE_INFO_BYTES == 32 + 4 + 4 + 4 + 4 + 8, "sync_file_info's size");
const _: () = assert!(FENCE_INFO_BYTES == 32 + 32 + 4 + 4 + 8, "sync_fence_info's size");

/// The largest errno a signal may carry, as Linux's `MAX_ERRNO`.
const MAX_ERRNO: i64 = 4095;

/// Nanoseconds in a millisecond.
const MILLI: u64 = 1_000_000;

/// The deadline, in milliseconds after install, for a driver's `asked`.
pub(crate) const fn deadline_ms(asked: u64) -> u64 {
    if asked == 0 {
        SYNC_DEADLINE_DEFAULT_MS
    } else if asked > SYNC_DEADLINE_MAX_MS {
        SYNC_DEADLINE_MAX_MS
    } else {
        asked
    }
}

/// One fence. Every descriptor of it holds it.
pub(crate) struct SyncFile {
    cookie: u64,
    /// Its maker, by identity: what resolve compares and signal finds.
    /// Weak, so no claim is kept.
    control: Weak<Control>,
    /// When it reads as signalled with `ETIMEDOUT` at the latest, on the
    /// monotonic counter.
    deadline: u64,
    /// `None` until signalled; then the status and when.
    signalled: SpinLock<Option<(i32, u64)>>,
    /// Woken when it is signalled.
    readable: Arc<WaitQueue>,
}

impl core::fmt::Debug for SyncFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("SyncFile")
            .field("cookie", &self.cookie)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl SyncFile {
    /// The cookie its driver named it by.
    pub(crate) fn cookie(&self) -> u64 {
        self.cookie
    }

    /// Whether `control` made it.
    pub(crate) fn made_by(&self, control: &Arc<Control>) -> bool {
        core::ptr::eq(self.control.as_ptr(), Arc::as_ptr(control))
    }

    /// Its status at `now`: `None` while unsignalled, else 0 or a negative
    /// errno and when. A passed deadline is `ETIMEDOUT`, whether or not the
    /// thread has signalled it yet (S4).
    pub(crate) fn status(&self, now: u64) -> Option<(i32, u64)> {
        let signalled = *self.signalled.lock();
        signalled.or_else(|| {
            (now >= self.deadline).then(|| (-(Errno::ETIMEDOUT.0 as i32), self.deadline))
        })
    }

    /// Signal it with `code`, once: whether this call did. A fence past its
    /// deadline is `ETIMEDOUT` already, whoever signals it, so a late
    /// driver's signal is refused rather than changing what was read.
    /// Wakes its waiters.
    fn signal(&self, code: i32) -> bool {
        let now = crate::timer::now_nanos();
        let timed_out = -(Errno::ETIMEDOUT.0 as i32);
        let (did, woke) = {
            let mut signalled = self.signalled.lock();
            if signalled.is_some() {
                (false, false)
            } else if now >= self.deadline {
                *signalled = Some((timed_out, self.deadline));
                (code == timed_out, true)
            } else {
                *signalled = Some((code, now));
                (true, true)
            }
        };
        if woke {
            self.readable.wake_all();
        }
        did
    }
}

impl Drop for SyncFile {
    fn drop(&mut self) {
        let Some(control) = self.control.upgrade() else {
            return;
        };
        let me: *const SyncFile = self;
        control
            .syncs
            .lock()
            .retain(|(_, held)| !core::ptr::eq(held.as_ptr(), me));
    }
}

impl Inode for SyncFile {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn splices_out(&self) -> bool {
        false
    }

    /// Nothing is read or written through it, as Linux's `sync_file` has no
    /// `read` or `write`.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(Errno::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8], _append: bool) -> VfsResult<(usize, u64)> {
        Err(Errno::EINVAL)
    }

    /// Readable once signalled, with an error as without (S5).
    fn poll(&self) -> Readiness {
        Readiness {
            readable: self.status(crate::timer::now_nanos()).is_some(),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(&self.readable));
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.readable.wakes())
    }

    /// Never mapped: no pages stand behind it.
    fn mapping_at(&self, _offset: u64) -> Option<(Arc<dyn Any + Send + Sync>, u64)> {
        None
    }
}

/// The fence behind an open file, if it is one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<SyncFile>> {
    Arc::clone(io).into_any().downcast::<SyncFile>().ok()
}

/// An ioctl on a fence: `SYNC_IOC_FILE_INFO` as Linux answers it for one
/// fence, anything else -- `SYNC_IOC_MERGE` included -- `ENOTTY` (S6).
///
/// # Errors
///
/// `ENOTTY`, `EFAULT` for an argument that cannot be copied, and `EINVAL`
/// for flags or padding set, or room for no fence at all.
pub(crate) fn ioctl(
    process: &Process,
    fence: &SyncFile,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    if request != SYNC_IOC_FILE_INFO {
        return Err(Errno::ENOTTY);
    }
    let mut info = [0u8; FILE_INFO_BYTES];
    uaccess::copy_from_user(process.space(), arg, &mut info).map_err(|_| Errno::EFAULT)?;
    let word = |at: usize| -> u32 {
        let mut bytes = [0u8; 4];
        if let Some(from) = info.get(at..at + 4) {
            bytes.copy_from_slice(from);
        }
        u32::from_ne_bytes(bytes)
    };
    let (flags, num_fences, pad) = (word(36), word(40), word(44));
    if flags != 0 || pad != 0 {
        return Err(Errno::EINVAL);
    }
    let mut array = [0u8; 8];
    array.copy_from_slice(info.get(48..56).ok_or(Errno::EINVAL)?);
    let array = u64::from_ne_bytes(array);
    let now = crate::timer::now_nanos();
    let (status, at) = match fence.status(now) {
        None => (0i32, 0u64),
        Some((0, at)) => (1, at),
        Some((code, at)) => (code, at),
    };
    if num_fences != 0 {
        // One fence: its entry, written where the program said.
        let mut entry = [0u8; FENCE_INFO_BYTES];
        put(&mut entry, 0, DRIVER_NAME);
        put(&mut entry, 32, DRIVER_NAME);
        put(&mut entry, 64, &status.to_ne_bytes());
        put(&mut entry, 72, &at.to_ne_bytes());
        uaccess::copy_to_user(process.space(), array, &entry).map_err(|_| Errno::EFAULT)?;
    }
    put(&mut info, 0, &[0u8; 32]);
    put(&mut info, 0, DRIVER_NAME);
    put(&mut info, 32, &status.to_ne_bytes());
    put(&mut info, 40, &1u32.to_ne_bytes());
    uaccess::copy_to_user(process.space(), arg, &info).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `bytes` into `out` at `at`, as far as it fits.
fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(to) = out.get_mut(at..at.saturating_add(bytes.len())) {
        to.copy_from_slice(bytes);
    }
}

/// `chardev_sync_install(control, request, cookie, deadline_ms, flags)`: a
/// fence as a new descriptor in the waiting program.
pub(crate) fn install(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, id, cookie, asked, flags, unused] = *registers;
    if flags & !SYNC_FLAGS != 0 || unused != 0 {
        return Err(status::INVALID_ARGS);
    }
    let control = super::control_of(caller, handle)?;
    let request = super::outstanding(&control, id)?;
    super::begin_copy(&request)?;
    let installed = install_for(&control, &request.client, cookie, deadline_ms(asked), flags);
    super::copy_done(&request);
    let descriptor = installed?;
    usize::try_from(descriptor).map_err(|_| status::INVALID_ARGS)
}

/// Make the fence for `cookie` and give `client` a descriptor of it.
fn install_for(
    control: &Arc<Control>,
    client: &Arc<Process>,
    cookie: u64,
    ms: u64,
    flags: u64,
) -> Result<i32, Errno> {
    let fence = make(control, cookie, ms)?;
    let inode: Arc<dyn Inode> = Arc::clone(&fence) as _;
    let open = fs::anon::open_mode(inode, NAME, true, false).map_err(|_| status::NO_MEMORY)?;
    let descriptor = client
        .files()
        .lock()
        .insert(open, flags & SYNC_CLOEXEC != 0)
        .map_err(|_| status::LIMIT_REACHED)?;
    watch(&fence);
    Ok(descriptor)
}

/// A new fence for `cookie`, in the control's table.
fn make(control: &Arc<Control>, cookie: u64, ms: u64) -> Result<Arc<SyncFile>, Errno> {
    let mut syncs = control.syncs.lock();
    if syncs
        .iter()
        .any(|(held, fence)| *held == cookie && fence.strong_count() > 0)
    {
        return Err(status::ALREADY_BOUND);
    }
    if syncs.len() >= MAX_SYNC_FILES {
        return Err(status::LIMIT_REACHED);
    }
    syncs.try_reserve(1).map_err(|_| status::NO_MEMORY)?;
    let deadline = crate::timer::now_nanos().saturating_add(ms.saturating_mul(MILLI));
    let readable = crate::fallible::try_arc(WaitQueue::new()).map_err(|_| status::NO_MEMORY)?;
    // Cyclic only for its failure, which builds nothing: a fence dropped
    // here would take this lock in its drop.
    let fence = crate::fallible::try_arc_cyclic(|_| SyncFile {
        cookie,
        control: Arc::downgrade(control),
        deadline,
        signalled: SpinLock::new(None),
        readable,
    })
    .map_err(|_| status::NO_MEMORY)?;
    // NOALLOC: reserved above.
    syncs.push((cookie, Arc::downgrade(&fence)));
    Ok(fence)
}

/// `chardev_sync_signal(control, cookie, status)`: signal the control's
/// fence `cookie` with 0 or a negative errno, once (S2).
pub(crate) fn signal(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, cookie, code, unused @ ..] = *registers;
    if unused.iter().any(|&register| register != 0) {
        return Err(status::INVALID_ARGS);
    }
    #[expect(clippy::cast_possible_wrap, reason = "the register carries a signed status")]
    let code = code as i64;
    if code > 0 || code < -MAX_ERRNO {
        return Err(status::INVALID_ARGS);
    }
    #[expect(clippy::cast_possible_truncation, reason = "checked to -4095..=0 above")]
    let code = code as i32;
    let control = super::control_of(caller, handle)?;
    let fence = take(&control, cookie).ok_or(status::BAD_STATE)?;
    let did = fence.signal(code);
    // The fence's drop may take the table's lock: dropped unlocked.
    drop(fence);
    if did { Ok(0) } else { Err(status::BAD_STATE) }
}

/// The live fence `cookie` names on `control`, out of its table: a cookie
/// leaves when it is signalled (S3).
fn take(control: &Control, cookie: u64) -> Option<Arc<SyncFile>> {
    let mut syncs = control.syncs.lock();
    let at = syncs.iter().position(|(held, _)| *held == cookie)?;
    let (_, fence) = syncs.swap_remove(at);
    drop(syncs);
    fence.upgrade()
}

/// `chardev_sync_resolve(control, request, descriptor, cookie)`: the cookie
/// of the fence the waiting program's descriptor names, written to `cookie`
/// in the driver, if this same control made it; answers 1 if it is
/// signalled, 0 if not.
pub(crate) fn resolve(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, id, descriptor, out, ..] = *registers;
    let control = super::control_of(caller, handle)?;
    let request = super::outstanding(&control, id)?;
    super::begin_copy(&request)?;
    let found = crate::syscall::fd::file(&request.client, crate::syscall::fd::arg(descriptor))
        .ok()
        .and_then(|file| of(file.io()))
        .filter(|fence| fence.made_by(&control));
    super::copy_done(&request);
    let fence = found.ok_or(status::BAD_HANDLE)?;
    let signalled = fence.status(crate::timer::now_nanos()).is_some();
    uaccess::copy_to_user(caller.core().space(), out, &fence.cookie().to_ne_bytes())
        .map_err(|_| status::FAULT)?;
    Ok(usize::from(signalled))
}

/// The control went: signal every fence it made with `ENODEV` (S4), the
/// table emptied under its lock and the fences signalled outside it.
pub(crate) fn control_gone(control: &Control) {
    let held = core::mem::take(&mut *control.syncs.lock());
    let mut fences = Vec::new();
    if fences.try_reserve(held.len()).is_ok() {
        fences.extend(held.iter().filter_map(|(_, fence)| fence.upgrade()));
    }
    drop(held);
    for fence in &fences {
        let _ = fence.signal(-(Errno::ENODEV.0 as i32));
    }
    drop(fences);
}

/// How many fences `control` has in its table, for the self-check.
pub(crate) fn alive(control: &Control) -> usize {
    control
        .syncs
        .lock()
        .iter()
        .filter(|(_, held)| held.strong_count() > 0)
        .count()
}

/// The table type a control keeps: each unsignalled fence by its cookie.
pub(crate) type Table = Vec<(u64, Weak<SyncFile>)>;

// ---------------------------------------------------------------------------
// The `syncfiles` thread: every fence signalled by its deadline
// ---------------------------------------------------------------------------

/// Every fence not yet known signalled, weakly.
static WATCHED: SpinLock<Vec<Weak<SyncFile>>> = SpinLock::new(Vec::new());
/// Whether the thread runs, claimed under the lock.
static RUNNING: SpinLock<bool> = SpinLock::new(false);
/// Set when a fence is added, so a thread about to exit looks again.
static CHANGED: AtomicBool = AtomicBool::new(false);
/// The thread sleeps on it.
static CLOCK: WaitQueue = WaitQueue::new();
/// The thread, for a check that waits for it to go.
static TASK: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);

/// Watch `fence`'s deadline, starting the thread if it is not running.
fn watch(fence: &Arc<SyncFile>) {
    {
        let mut watched = WATCHED.lock();
        if watched.try_reserve(1).is_ok() {
            // NOALLOC: reserved above.
            watched.push(Arc::downgrade(fence));
        }
        // Out of memory: its readers still see the deadline when they look.
    }
    let start = {
        let mut running = RUNNING.lock();
        CHANGED.store(true, Ordering::Release);
        let start = !*running;
        *running = true;
        start
    };
    if start {
        match sched::spawn("syncfiles", run, 0, ferrix_sched::NICE_0_WEIGHT) {
            Ok(task) => {
                let previous = TASK.lock().replace(task);
                drop(previous);
            }
            Err(_) => *RUNNING.lock() = false,
        }
    }
    CLOCK.wake_all();
}

/// The thread: signal every fence past its deadline, sleep to the next.
fn run(_argument: usize) {
    loop {
        let next = pass();
        if next == u64::MAX {
            let mut running = RUNNING.lock();
            if !CHANGED.swap(false, Ordering::AcqRel) {
                *running = false;
                return;
            }
            continue;
        }
        let _ = WaitQueue::wait_on_any(
            &[&CLOCK],
            || CHANGED.swap(false, Ordering::AcqRel),
            next,
            fs::wake::TRUSTED_RECHECK_NANOS,
        );
    }
}

/// One look: signal what is past its deadline with `ETIMEDOUT`, forget what
/// is signalled or gone, and answer the earliest deadline left.
fn pass() -> u64 {
    let now = crate::timer::now_nanos();
    // Taken under the lock, looked at and dropped outside it: one may be a
    // fence's last reference, whose drop takes its control's table lock.
    let held = {
        let mut watched = WATCHED.lock();
        watched.retain(|fence| fence.strong_count() > 0);
        let mut held = Vec::new();
        if held.try_reserve(watched.len()).is_ok() {
            held.extend(watched.iter().filter_map(Weak::upgrade));
        }
        held
    };
    let live: Vec<Arc<SyncFile>> = held
        .into_iter()
        .filter(|fence| fence.signalled.lock().is_none())
        .collect();
    let mut next = u64::MAX;
    for fence in &live {
        if now >= fence.deadline {
            if let Some(control) = fence.control.upgrade() {
                let taken = take(&control, fence.cookie);
                drop(taken);
            }
            let _ = fence.signal(-(Errno::ETIMEDOUT.0 as i32));
        } else {
            next = next.min(fence.deadline);
        }
    }
    drop(live);
    next
}
