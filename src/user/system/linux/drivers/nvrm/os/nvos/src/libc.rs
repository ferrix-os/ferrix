//! The C library calls this layer makes, with the x86-64 Linux ABI's
//! prototypes, which glibc (the host's test build) and ferrousli (nvrm's)
//! share. The futex and thread-id calls are made directly, in [`crate::futex`].

use core::ffi::{c_char, c_int, c_void};

/// `struct timespec` on x86-64 Linux.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Timespec {
    /// Seconds.
    pub(crate) tv_sec: i64,
    /// Nanoseconds, below a second.
    pub(crate) tv_nsec: i64,
}

/// `CLOCK_REALTIME`.
pub(crate) const CLOCK_REALTIME: c_int = 0;
/// `CLOCK_MONOTONIC`.
pub(crate) const CLOCK_MONOTONIC: c_int = 1;
/// `CLOCK_MONOTONIC_RAW`.
pub(crate) const CLOCK_MONOTONIC_RAW: c_int = 4;

/// `O_RDONLY`.
pub(crate) const O_RDONLY: c_int = 0;
/// `O_RDWR`.
pub(crate) const O_RDWR: c_int = 2;
/// `O_CREAT`.
pub(crate) const O_CREAT: c_int = 0o100;
/// `O_EXCL`.
pub(crate) const O_EXCL: c_int = 0o200;
/// `O_CLOEXEC`.
pub(crate) const O_CLOEXEC: c_int = 0o2000000;

/// `PROT_READ | PROT_WRITE`.
pub(crate) const PROT_READ_WRITE: c_int = 3;
/// `MAP_PRIVATE | MAP_ANONYMOUS`.
pub(crate) const MAP_PRIVATE_ANONYMOUS: c_int = 0x22;
/// `MAP_FAILED`.
pub(crate) const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

/// `SEEK_END`.
pub(crate) const SEEK_END: c_int = 2;

unsafe extern "C" {
    pub(crate) fn malloc(size: usize) -> *mut c_void;
    pub(crate) fn calloc(count: usize, size: usize) -> *mut c_void;
    pub(crate) fn free(pointer: *mut c_void);
    pub(crate) fn abort() -> !;
    pub(crate) fn clock_gettime(clock: c_int, time: *mut Timespec) -> c_int;
    pub(crate) fn clock_getres(clock: c_int, time: *mut Timespec) -> c_int;
    pub(crate) fn clock_nanosleep(
        clock: c_int,
        flags: c_int,
        request: *const Timespec,
        remain: *mut Timespec,
    ) -> c_int;
    pub(crate) fn sched_yield() -> c_int;
    pub(crate) fn sched_getcpu() -> c_int;
    pub(crate) fn get_nprocs() -> c_int;
    pub(crate) fn getpid() -> c_int;
    pub(crate) fn geteuid() -> u32;
    pub(crate) fn getrandom(buffer: *mut c_void, length: usize, flags: u32) -> isize;
    pub(crate) fn write(fd: c_int, buffer: *const c_void, length: usize) -> isize;
    pub(crate) fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    pub(crate) fn close(fd: c_int) -> c_int;
    pub(crate) fn unlink(path: *const c_char) -> c_int;
    pub(crate) fn pread(fd: c_int, buffer: *mut c_void, length: usize, offset: i64) -> isize;
    pub(crate) fn pwrite(fd: c_int, buffer: *const c_void, length: usize, offset: i64) -> isize;
    pub(crate) fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64;
    pub(crate) fn mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    pub(crate) fn munmap(address: *mut c_void, length: usize) -> c_int;
    pub(crate) fn pthread_create(
        thread: *mut u64,
        attributes: *const c_void,
        start: extern "C" fn(*mut c_void) -> *mut c_void,
        argument: *mut c_void,
    ) -> c_int;
    pub(crate) fn pthread_detach(thread: u64) -> c_int;
    pub(crate) fn __errno_location() -> *mut c_int;
}

/// The calling thread's `errno`.
pub(crate) fn errno() -> c_int {
    // SAFETY: `__errno_location` returns the calling thread's own errno slot,
    // valid for as long as the thread lives.
    let at = unsafe { __errno_location() };
    // SAFETY: as above; it is an aligned `int` this thread alone writes.
    unsafe { *at }
}
