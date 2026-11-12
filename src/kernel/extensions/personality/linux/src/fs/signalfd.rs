//! Signalfds: the signals pending for a process, read through a descriptor,
//! the object behind `signalfd4`.
//!
//! A signalfd holds a mask and nothing else. A read takes the pending signals
//! in the mask -- the reading thread's own first, then its process's, as
//! `rt_sigtimedwait` takes them -- and answers a 128-byte `struct
//! signalfd_siginfo` for each, as many as the buffer holds; with none pending
//! it waits, interruptibly, or is `EAGAIN`. `poll` answers readable while one
//! is pending, never writable. A program blocks the signals first, so that
//! they wait to be read rather than being delivered, and hands the descriptor
//! to its event loop: glib's and Chrome's take `SIGCHLD` and `SIGTERM` this
//! way.
//!
//! Linux's `fs/signalfd.c`, check for check: a buffer shorter than one
//! `signalfd_siginfo` is `EINVAL`, and so is a write, which has no operation;
//! the first signal may wait and the rest are only taken if already there; a
//! read interrupted by a signal outside the mask restarts under `SA_RESTART`.
//!
//! # Whose signals
//!
//! As on Linux, the reader's: the process of the task that reads or polls,
//! not the one that made the descriptor, so a `fork` child's copy reads the
//! child's. A kernel task reading on a process's behalf -- the boot check's --
//! is nobody's thread, and reads the process that made the descriptor, from
//! its own queue only.
//!
//! # Waking
//!
//! `poll` and `epoll_wait` trust a file whose every readiness change wakes one
//! of its queues, and then sleep a second between looks of their own. A
//! signalfd becomes readable when a signal is made pending, which the sender
//! does, not the reader; so every process has a queue of its own for this,
//! [`Process::signal_arrived`], which `Process::notify_signal` and
//! `notify_signal_to` wake for every signal they make pending. That is
//! Linux's `signalfd_wqh`, and the one wake a blocked signal gets: nothing
//! else wakes a thread for a signal it blocks. A change of mask wakes it too,
//! as `signalfd4` on an existing descriptor does on Linux. A read that takes a
//! signal only makes the descriptor less ready, which needs no wake.

use alloc::sync::{Arc, Weak};
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_linux_abi::types::SIGNALFD_SIGINFO_BYTES;
use ferrix_vfs::{Errno, Inode, Metadata, OpenFile, Readiness};

use crate::fs;
use crate::sched::WaitQueue;
use crate::syscall::process::{self, Process};
use crate::syscall::signal::{self, Taken, UNBLOCKABLE};
use crate::syscall::{registry, thread};

/// The name `/proc/self/fd` shows.
const NAME: &[u8] = b"anon_inode:[signalfd]";

/// The deadline a signalfd's wait passes: none, as a pipe's.
const FOREVER: u64 = u64::MAX;

/// How long a blocked read sleeps between its own looks: the long one `poll`
/// gives files it trusts, since every signal made pending wakes the queue it
/// sleeps on and a signal that interrupts it wakes the task itself.
const RECHECK: u64 = fs::wake::TRUSTED_RECHECK_NANOS;

/// A signalfd.
pub(crate) struct SignalFd {
    /// The signals it reads: bit `n - 1` for signal `n`. Never `SIGKILL` or
    /// `SIGSTOP`, which no program may take this way.
    mask: AtomicU64,
    /// The process that made it, whose signals a reader that is no process's
    /// thread reads. Weak: the descriptor does not keep its maker alive.
    maker: Weak<Process>,
    /// Its heap, charged to the job that made it (F-37).
    _charge: Charge,
}

impl fmt::Debug for SignalFd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignalFd")
            .field("mask", &self.mask.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// A new signalfd reading `mask`, made by `maker`, as the open file
/// `signalfd4` installs.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for a signalfd is nothing.
pub(crate) fn create(maker: &Process, mask: u64, nonblock: bool) -> Result<Arc<OpenFile>, Errno> {
    // The registry's reference, since a call is handed its process by
    // reference; a process not in it has no kernel task reading for it.
    let maker = registry::find(maker.pid())
        .filter(|found| core::ptr::eq(Arc::as_ptr(found), maker))
        .as_ref()
        .map_or_else(Weak::new, Arc::downgrade);
    let charge = Charge::bytes(arc_footprint::<SignalFd>()).map_err(|_| Errno::ENOMEM)?;
    let signalfd = Arc::new(SignalFd {
        mask: AtomicU64::new(mask & !UNBLOCKABLE),
        maker,
        _charge: charge,
    });
    fs::anon::open(signalfd, NAME, nonblock)
}

impl SignalFd {
    /// The signals it reads.
    pub(crate) fn mask(&self) -> u64 {
        self.mask.load(Ordering::Acquire)
    }

    /// Read `mask` from now on, and wake whoever waits on `process`'s
    /// signals, which may now include one pending: what `signalfd4` does to a
    /// descriptor it is handed.
    pub(crate) fn set_mask(&self, process: &Process, mask: u64) {
        self.mask.store(mask & !UNBLOCKABLE, Ordering::Release);
        process.signal_arrived().wake_all();
    }

    /// The process whose signals the running task reads: its own, or for a
    /// task that is no process's, the maker's.
    fn reader(&self) -> Option<Arc<Process>> {
        process::current().or_else(|| self.maker.upgrade())
    }

    /// Whether a signal in the mask is pending for `reader`: for the running
    /// thread, if it is one of `reader`'s, or for the process as a whole.
    fn pending(&self, reader: &Process) -> bool {
        let mask = self.mask();
        match thread::current_of(reader) {
            Some(thread) => {
                thread.with_signals(|shared, own| (shared.pending() | own.pending()) & mask != 0)
            }
            None => reader.with_signals(|shared| shared.pending() & mask != 0),
        }
    }

    /// Take the next pending signal in the mask, as `rt_sigtimedwait` takes
    /// one: the thread's own first, a fault's first within each, then the
    /// lowest number.
    fn take(&self, reader: &Process) -> Option<Taken> {
        let mask = self.mask();
        match thread::current_of(reader) {
            Some(thread) => thread.with_signals(|shared, own| signal::take_from(shared, own, mask)),
            None => reader.with_signals(|shared| signal::take_shared(shared, mask)),
        }
    }

    /// One signal for a read: taken at once, or waited for unless
    /// `nonblock`. Linux's `signalfd_dequeue`.
    fn dequeue(&self, reader: &Process, nonblock: bool) -> ferrix_vfs::Result<Taken> {
        loop {
            if let Some(taken) = self.take(reader) {
                return Ok(taken);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _ = WaitQueue::wait_on_any(
                &[reader.signal_arrived().as_ref()],
                || self.pending(reader) || reader.signal_pending(),
                FOREVER,
                RECHECK,
            );
            // Taken before the interruption is looked at, as Linux dequeues
            // before it asks `signal_pending`: a signal in the mask that the
            // thread does not block is read, not delivered.
            if let Some(taken) = self.take(reader) {
                return Ok(taken);
            }
            if reader.signal_pending() {
                return Err(Errno::ERESTARTSYS);
            }
        }
    }

    /// How many waits on its signals a wake has ended, for the checks.
    pub(crate) fn waits_ended_by_a_wake(&self) -> u32 {
        self.maker
            .upgrade()
            .map_or(0, |maker| maker.signal_arrived().waits_ended_by_a_wake())
    }

    /// How many tasks wait on its maker's signals now, for the checks.
    pub(crate) fn waiters_listed(&self) -> usize {
        self.maker
            .upgrade()
            .map_or(0, |maker| maker.signal_arrived().listed())
    }
}

impl Inode for SignalFd {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// `lseek` on a signalfd is 0, as Linux's `noop_llseek` answers, while
    /// `pread64` and `pwrite64` stay `ESPIPE`.
    fn seek_is_noop(&self) -> bool {
        true
    }

    fn poll(&self) -> Readiness {
        Readiness {
            readable: self.reader().is_some_and(|reader| self.pending(&reader)),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        match self.reader() {
            Some(reader) => {
                visit(fs::wake::shared(reader.signal_arrived()));
                true
            }
            None => false,
        }
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(
            self.reader()
                .map_or(0, |reader| reader.signal_arrived().wakes()),
        )
    }

    /// A `signalfd_siginfo` for each signal taken, as many as `buf` holds.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        if buf.len() < SIGNALFD_SIGINFO_BYTES {
            return Err(Errno::EINVAL);
        }
        // A reader always exists for a descriptor a program holds; a kernel
        // task holding one whose maker has gone reads nothing.
        let reader = self.reader().ok_or(Errno::EAGAIN)?;
        let mut filled = 0;
        for slot in buf.as_chunks_mut::<SIGNALFD_SIGINFO_BYTES>().0 {
            // Only the first may wait: `signalfd_read` takes the rest
            // non-blocking, and answers what it has.
            match self.dequeue(&reader, nonblock || filled > 0) {
                Ok(taken) => {
                    slot.copy_from_slice(&taken.origin.encode_signalfd(taken.signal));
                    filled += SIGNALFD_SIGINFO_BYTES;
                }
                Err(error) if filled == 0 => return Err(error),
                Err(_) => break,
            }
        }
        Ok(filled)
    }

    /// No write: `EINVAL`, as for a file with no write operation.
    fn write_stream(&self, data: &[u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        let _ = (data, nonblock);
        Err(Errno::EINVAL)
    }
}

/// The signalfd an open file is, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<SignalFd>> {
    Arc::clone(file.io()).into_any().downcast::<SignalFd>().ok()
}
