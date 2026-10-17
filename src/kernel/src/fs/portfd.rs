//! A port as a file descriptor: native `port_fd` (`docs/INIT.md` §9, K4).
//!
//! Native waits and descriptors could not see each other: a program in
//! `epoll_wait` could not hear a port, and one in `port_wait` could not hear a
//! socket. The bridge goes this way round, a port becoming a descriptor,
//! because a descriptor that polls is one [`Inode`] here, while a port that
//! watched descriptors would reach into every file type's wake-ups.
//!
//! The descriptor reports and does nothing else. It is readable while the port
//! has a packet queued -- of any kind: a user packet, a signal registration's,
//! an interrupt's -- and not otherwise; packets are still taken with
//! `port_wait`, so a `read` or a `write` of it is `EINVAL`. Every packet queued
//! wakes the port's queue, which is the queue the descriptor offers `poll` and
//! `epoll_wait`, so a wait ends when a packet arrives and not at its next look;
//! an interrupt's packet wakes it too, from the handler once its locks are let
//! go (`object::interrupt`). Taking a packet wakes nothing, since readiness
//! only falls then, and a level-triggered wait looks again anyway.
//!
//! The file holds the port, so the descriptor keeps it alive after the handle
//! it was made from is closed, as a `dup` keeps a file.

use alloc::sync::Arc;
use core::any::Any;
use core::fmt;

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::PORT_FD_CLOEXEC;
use ferrix_vfs::{Errno, Inode, Metadata, Readiness};

use crate::fs;
use crate::hooks::Full;
use crate::object::port::Port;
use crate::object::process::Host;
use crate::syscall::native;
use crate::syscall::process;

/// The name `/proc/self/fd` shows.
const NAME: &[u8] = b"anon_inode:[ferrix-port]";

/// A port, seen as a file.
pub(crate) struct PortFile {
    /// The port. Held, so the descriptor keeps it.
    port: Arc<Port>,
    /// Its heap, charged to the job that made it (F-37).
    _charge: Charge,
}

impl fmt::Debug for PortFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PortFile").finish_non_exhaustive()
    }
}

impl PortFile {
    /// The port, for the checks.
    pub(crate) fn port(&self) -> &Arc<Port> {
        &self.port
    }
}

/// Answer native `port_fd` from here: a descriptor is the Linux
/// personality's, and the native ABI names no filesystem. Called once from
/// `fs::install`.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::PortFd, port_fd)
}

/// `port_fd(port, flags)`: a descriptor on the port. Needs `WAIT`, since the
/// descriptor tells whether packets are waiting and nothing more.
fn port_fd(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [port, flags, ..] = *registers;
    if flags & !PORT_FD_CLOEXEC != 0 {
        return Err(status::INVALID_ARGS);
    }
    let process = process::of_host(caller).ok_or(status::BAD_HANDLE)?;
    let port = native::port_in(
        caller.core(),
        ferrix_native_abi::handle::Handle::from_register(port),
        Rights::WAIT,
    )?;
    let charge = Charge::bytes(arc_footprint::<PortFile>()).map_err(|_| Errno::ENOMEM)?;
    let file = fs::anon::open(
        Arc::new(PortFile {
            port,
            _charge: charge,
        }),
        NAME,
        false,
    )?;
    let fd = process
        .files()
        .lock()
        .insert(file, flags & PORT_FD_CLOEXEC != 0)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}

impl Inode for PortFile {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll(&self) -> Readiness {
        Readiness {
            readable: !self.port.is_empty(),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(self.port.shared_waiters()));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.port.waiters().wakes())
    }

    /// Packets are taken with `port_wait`, not read.
    fn read_stream(&self, _buf: &mut [u8], _nonblock: bool) -> ferrix_vfs::Result<usize> {
        Err(Errno::EINVAL)
    }

    /// Nor written: `port_queue` queues one.
    fn write_stream(&self, _data: &[u8], _nonblock: bool) -> ferrix_vfs::Result<usize> {
        Err(Errno::EINVAL)
    }
}

/// The port file an open file is, if it is one.
pub(crate) fn of(file: &ferrix_vfs::OpenFile) -> Option<Arc<PortFile>> {
    Arc::clone(file.io()).into_any().downcast::<PortFile>().ok()
}
