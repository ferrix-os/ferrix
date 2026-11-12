//! Eventfds: a 64-bit counter behind a descriptor, the object behind
//! `eventfd2`.
//!
//! A write adds the eight-byte value it carries to the counter; a read takes
//! the whole counter and leaves zero, or with `EFD_SEMAPHORE` takes one. A
//! read of a zero counter waits, and so does a write that would carry it to
//! `u64::MAX`, the one value it never holds. It is how one thread wakes
//! another's event loop: the eventfd is registered in the loop's epoll set,
//! and a write makes it readable.
//!
//! Linux's `fs/eventfd.c`, check for check: a buffer shorter than eight bytes
//! is `EINVAL` for either direction, and so is a write of `u64::MAX`; the
//! counter is read and written in the machine's byte order. `poll` answers
//! readable while the counter is not zero and writable while one more could be
//! added. A read wakes writers and a write wakes readers, and those wakes are
//! what epoll's edge-triggered mode counts.

use alloc::sync::Arc;
use core::any::Any;
use core::fmt;

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_vfs::{Errno, Inode, Metadata, OpenFile, Readiness};

use crate::fs;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::process;

/// The name `/proc/self/fd` shows.
const NAME: &[u8] = b"anon_inode:[eventfd]";

/// Bytes in what a read returns and a write carries.
pub(crate) const VALUE_BYTES: usize = 8;

/// The deadline an eventfd's wait passes: none, as a pipe's.
const FOREVER: u64 = u64::MAX;

/// How long a blocked read or write sleeps between its own looks: the long
/// one `poll` gives files it trusts, because every change to the counter
/// wakes the queue its waiters are on -- a write the readable one, a read the
/// writable one -- and a signal wakes the task itself.
///
/// A queue's own 5 ms slices took the waiter off the queue and put it back
/// two hundred times a second, and a write that landed in between found
/// nobody to wake: the reader saw the value by looking, which is correct but
/// is not the wake the self-check asks about. The check's 20 ms is four of
/// those slices, so the write and a slice's end can fall on the same tick, and
/// on a loaded host the check failed that way (FX-0882, twice on ARMv7-A in
/// test-compositor).
const RECHECK: u64 = fs::wake::TRUSTED_RECHECK_NANOS;

/// An eventfd.
pub(crate) struct EventFd {
    /// The counter.
    count: SpinLock<u64>,
    /// `EFD_SEMAPHORE`: a read takes one.
    semaphore: bool,
    /// Woken when a read may no longer wait: the counter went above zero.
    readable: Arc<WaitQueue>,
    /// Woken when a write may no longer wait: a read took from the counter.
    writable: Arc<WaitQueue>,
    /// Its heap, charged to the job that made it (F-37).
    _charge: Charge,
}

impl fmt::Debug for EventFd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventFd")
            .field("semaphore", &self.semaphore)
            .finish_non_exhaustive()
    }
}

/// A new eventfd holding `initial`, as the open file `eventfd2` installs.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for an eventfd is nothing.
pub(crate) fn create(
    initial: u32,
    semaphore: bool,
    nonblock: bool,
) -> Result<Arc<OpenFile>, Errno> {
    let charge = Charge::bytes(
        arc_footprint::<EventFd>().saturating_add(arc_footprint::<WaitQueue>().saturating_mul(2)),
    )
    .map_err(|_| Errno::ENOMEM)?;
    let eventfd = Arc::new(EventFd {
        count: SpinLock::new(u64::from(initial)),
        semaphore,
        readable: Arc::new(WaitQueue::new()),
        writable: Arc::new(WaitQueue::new()),
        _charge: charge,
    });
    fs::anon::open(eventfd, NAME, nonblock)
}

/// Whether `add` more fits in a counter holding `count`: the counter stays
/// below `u64::MAX`.
const fn fits(count: u64, add: u64) -> bool {
    u64::MAX - count > add
}

impl EventFd {
    /// Take from the counter, if there is anything: the whole of it, or one.
    fn take(&self) -> Option<u64> {
        let mut count = self.count.lock();
        if *count == 0 {
            return None;
        }
        let taken = if self.semaphore { 1 } else { *count };
        *count -= taken;
        Some(taken)
    }

    /// Add to the counter, if it fits.
    fn add(&self, value: u64) -> bool {
        let mut count = self.count.lock();
        if !fits(*count, value) {
            return false;
        }
        *count += value;
        true
    }

    /// How many waits on the counter a wake has ended, for the checks.
    pub(crate) fn waits_ended_by_a_wake(&self) -> u32 {
        self.readable
            .waits_ended_by_a_wake()
            .wrapping_add(self.writable.waits_ended_by_a_wake())
    }

    /// How many tasks wait on the counter becoming readable now, for the
    /// checks: a blocked read, and a `poll` or `epoll_wait` watching for it.
    pub(crate) fn readers_listed(&self) -> usize {
        self.readable.listed()
    }

    /// The counter now, for the checks.
    pub(crate) fn count(&self) -> u64 {
        *self.count.lock()
    }
}

/// Whether the process a wait is on behalf of has a signal to take.
fn interrupted(caller: Option<&Arc<process::Process>>) -> bool {
    caller.is_some_and(|process| process.signal_pending())
}

impl Inode for EventFd {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// `lseek` on an eventfd is 0, as Linux's `noop_llseek` answers, while
    /// `pread64` and `pwrite64` stay `ESPIPE`.
    fn seek_is_noop(&self) -> bool {
        true
    }

    fn poll(&self) -> Readiness {
        let count = *self.count.lock();
        Readiness {
            readable: count > 0,
            writable: fits(count, 1),
            hangup: false,
            error: count == u64::MAX,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(&self.readable));
        visit(fs::wake::shared(&self.writable));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.readable.wakes().wrapping_add(self.writable.wakes()))
    }

    /// Eight bytes of the counter, taken; `EAGAIN` or a wait while it is zero.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        let out = buf.get_mut(..VALUE_BYTES).ok_or(Errno::EINVAL)?;
        let caller = process::current();
        loop {
            if let Some(taken) = self.take() {
                out.copy_from_slice(&taken.to_ne_bytes());
                self.writable.wake_all();
                return Ok(VALUE_BYTES);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _ = WaitQueue::wait_on_any(
                &[&self.readable],
                || *self.count.lock() > 0 || interrupted(caller.as_ref()),
                FOREVER,
                RECHECK,
            );
            if interrupted(caller.as_ref()) {
                // A restart code: an eventfd read restarts under `SA_RESTART`,
                // as `eventfd_read`'s interruptible wait does.
                return Err(Errno::ERESTARTSYS);
            }
        }
    }

    /// Add the eight-byte value; `EAGAIN` or a wait while it does not fit.
    fn write_stream(&self, data: &[u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        let bytes = data.first_chunk::<VALUE_BYTES>().ok_or(Errno::EINVAL)?;
        let value = u64::from_ne_bytes(*bytes);
        if value == u64::MAX {
            return Err(Errno::EINVAL);
        }
        let caller = process::current();
        loop {
            if self.add(value) {
                self.readable.wake_all();
                return Ok(VALUE_BYTES);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _ = WaitQueue::wait_on_any(
                &[&self.writable],
                || fits(*self.count.lock(), value) || interrupted(caller.as_ref()),
                FOREVER,
                RECHECK,
            );
            if interrupted(caller.as_ref()) {
                return Err(Errno::ERESTARTSYS);
            }
        }
    }
}

/// The eventfd an open file is, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<EventFd>> {
    Arc::clone(file.io()).into_any().downcast::<EventFd>().ok()
}
