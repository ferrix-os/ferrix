//! What the front end asks of the process: the chardev core's calls
//! (`src/lib/proto/native/src/chardev.rs`), bound to the request being
//! answered, and the time.

use core::fmt;

use ferrix_linux_abi::errno::Errno;

/// What a dmabuf install answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Installed {
    /// The descriptor in the program.
    pub fd: i32,
    /// Whether this install made the dmabuf, rather than finding the
    /// cookie's alive; a `DmabufRelease` for the cookie follows exactly when
    /// it did.
    pub made: bool,
}

/// The process side, for memory of type `M`.
pub trait Host<M> {
    /// Fill `out` from the waiting program's memory at `address`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn copy_in(&mut self, request: u64, address: u64, out: &mut [u8]) -> Result<(), Errno>;

    /// Write `bytes` into the waiting program's memory at `address`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn copy_out(&mut self, request: u64, address: u64, bytes: &[u8]) -> Result<(), Errno>;

    /// Answer `request` with `status`, 0 or a negative errno. A reply to an
    /// abandoned request is refused by the kernel; the host ignores that.
    fn reply(&mut self, request: u64, status: i32);

    /// Answer an mmap `request` with `offset` bytes into `memory`, mapped
    /// write-combining if asked (K6).
    ///
    /// # Errors
    ///
    /// What the kernel refused it with; the front end then answers the
    /// request with that error.
    fn reply_map(
        &mut self,
        request: u64,
        memory: &M,
        offset: u64,
        write_combining: bool,
    ) -> Result<(), Errno>;

    /// A dmabuf over the whole of `memory`, named `cookie`, as a new
    /// descriptor in the program.
    ///
    /// # Errors
    ///
    /// `EMFILE`, `ENOMEM`, `EAGAIN` while the cookie's last dmabuf is going.
    fn dmabuf_install(
        &mut self,
        request: u64,
        memory: &M,
        cookie: u64,
        writable: bool,
        cloexec: bool,
    ) -> Result<Installed, Errno>;

    /// The cookie of the dmabuf the program's `fd` names, if this driver
    /// made it.
    ///
    /// # Errors
    ///
    /// `EBADF` for any other descriptor.
    fn dmabuf_resolve(&mut self, request: u64, fd: i32) -> Result<u64, Errno>;

    /// A new unsignalled kernel fence `fence` in the slot of this driver's
    /// dmabuf `dmabuf` (K5, `chardev_dmabuf_fence`).
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the control's fences; `EBADF` for a dmabuf no longer
    /// alive.
    fn dmabuf_fence(&mut self, dmabuf: u64, fence: u64) -> Result<(), Errno>;

    /// A kernel fence named `fence` as a new sync-file descriptor in the
    /// program.
    ///
    /// # Errors
    ///
    /// `EMFILE`, `ENOMEM`.
    fn sync_install(&mut self, request: u64, fence: u64, cloexec: bool) -> Result<i32, Errno>;

    /// The cookie of the sync file the program's `fd` names, if this
    /// driver made it, and whether it is signalled. The kernel's own
    /// signalled fence (an empty dmabuf slot's export) is signalled with
    /// cookie 0.
    ///
    /// # Errors
    ///
    /// `EINVAL` for any other descriptor, as `sync_file_get_fence` fails.
    fn sync_resolve(&mut self, request: u64, fd: i32) -> Result<(u64, bool), Errno>;

    /// Signal kernel fence `fence` with `status`, once.
    fn sync_signal(&mut self, fence: u64, status: i32);

    /// Monotonic nanoseconds, as `CLOCK_MONOTONIC`.
    fn now_ns(&self) -> u64;

    /// Whether the program waiting in `request` may raise its priority above
    /// the default (`CAP_SYS_NICE`) or is the administrator (`CAP_SYS_ADMIN`):
    /// Ferrix has no capabilities, so the host answers for root.
    fn privileged(&self, request: u64) -> bool;

    /// A line on the driver's console.
    fn say(&mut self, line: fmt::Arguments<'_>);
}
