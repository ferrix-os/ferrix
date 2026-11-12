//! Pidfds: a descriptor for a process, the object behind `pidfd_open`.
//!
//! It is how a program waits for a process it did not start, or starts one
//! and waits for it in an event loop: `poll` and epoll answer readable once
//! the process has ended, `waitid(P_PIDFD, …)` reaps a child by it, and
//! `pidfd_send_signal` signals it with no pid to be reused under the call.
//!
//! Linux's `kernel/pid.c` and `fs/pidfs.c`, as far as these three go: the
//! file names the process for as long as it is open, so its pid can come
//! round again while the descriptor still means the first one; reading it is
//! `EINVAL`; it is readable once the whole process has ended -- exited, not
//! reaped -- and stays so.

use alloc::sync::Arc;
use core::any::Any;
use core::fmt;

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_vfs::{Errno, Inode, Metadata, OpenFile, Readiness};

use crate::fs;
use crate::syscall::process::Process;

/// The name `/proc/self/fd` shows.
const NAME: &[u8] = b"anon_inode:[pidfd]";

/// A pidfd.
pub(crate) struct PidFd {
    /// The process it names, kept until the descriptor closes, as Linux keeps
    /// its `struct pid`.
    process: Arc<Process>,
    /// Its heap, charged to the job that made it (F-37).
    _charge: Charge,
}

impl fmt::Debug for PidFd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PidFd")
            .field("pid", &self.process.pid())
            .finish_non_exhaustive()
    }
}

/// A new pidfd for `process`, as the open file `pidfd_open` installs.
///
/// # Errors
///
/// `ENOMEM` past the job's memory limit.
pub(crate) fn create(process: Arc<Process>, nonblock: bool) -> Result<Arc<OpenFile>, Errno> {
    let charge = Charge::bytes(arc_footprint::<PidFd>()).map_err(|_| Errno::ENOMEM)?;
    // Made before the file, so an end that comes in between is heard.
    process.make_pidfd_queue().map_err(|_| Errno::ENOMEM)?;
    fs::anon::open(
        Arc::new(PidFd {
            process,
            _charge: charge,
        }),
        NAME,
        nonblock,
    )
}

impl PidFd {
    /// The process it names.
    pub(crate) fn process(&self) -> &Arc<Process> {
        &self.process
    }
}

impl Inode for PidFd {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// Readable once the process has ended and let go of what it held: the
    /// moment `wait4` could reap it, which is when Linux's `pidfd_poll`
    /// answers `EPOLLIN`. Linux adds `EPOLLHUP` only once it has been
    /// reaped as well, which a pidfd here does not follow.
    fn poll(&self) -> Readiness {
        Readiness {
            readable: self.process.is_released(),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    /// The process's pidfd queue, which [`create`] made: woken as it is
    /// released.
    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        match self.process.pidfd_queue() {
            Some(queue) => {
                visit(fs::wake::shared(&queue));
                true
            }
            None => false,
        }
    }

    fn poll_changes(&self) -> Option<u64> {
        self.process.pidfd_queue().map(|queue| queue.wakes())
    }

    /// A pidfd holds nothing to read.
    fn read_stream(&self, _buf: &mut [u8], _nonblock: bool) -> ferrix_vfs::Result<usize> {
        Err(Errno::EINVAL)
    }

    /// Nor anything to write.
    fn write_stream(&self, _data: &[u8], _nonblock: bool) -> ferrix_vfs::Result<usize> {
        Err(Errno::EINVAL)
    }
}

/// The pidfd an open file is, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<PidFd>> {
    Arc::clone(file.io()).into_any().downcast::<PidFd>().ok()
}
