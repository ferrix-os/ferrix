//! RM's `os_*` calls with no better home: memory, strings, RM's locks and
//! wait queues, the processor, randomness, files, and the questions a
//! GeForce build answers "no" to.
//!
//! Each answer follows Linux's `os-interface.c` at 580.173.02. Where Linux
//! asks the kernel, this asks the C library; where Linux's answer depends on
//! a build option a GeForce driver leaves off (vGPU, GRID, Tegra, NUMA
//! onlining, confidential computing), the answer is that build's.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use crate::libc;
use crate::log::{c_str, say};
use crate::status::{self, NvBool, NvStatus};
use crate::sync::{Completion, Mutex, RwLock, Semaphore};
use crate::thread;
use crate::time;

// ---------------------------------------------------------------------------
// What RM reads as data (`os-interface.h`).
// ---------------------------------------------------------------------------

/// `os_page_size`. Mutable, as RM declares it, though nothing writes it.
#[unsafe(no_mangle)]
pub static mut os_page_size: u64 = 4096;
/// `os_max_page_size`: 4 KiB pages up to Linux's `MAX_PAGE_ORDER` of 10.
#[unsafe(no_mangle)]
pub static mut os_max_page_size: u64 = 4096 << 10;
/// `os_page_mask`.
#[unsafe(no_mangle)]
pub static mut os_page_mask: u64 = !4095;
/// `os_page_shift`.
#[unsafe(no_mangle)]
pub static mut os_page_shift: u8 = 12;
/// `os_cc_enabled`: no confidential computing.
#[unsafe(no_mangle)]
pub static mut os_cc_enabled: NvBool = 0;
/// `os_cc_sev_snp_enabled`.
#[unsafe(no_mangle)]
pub static mut os_cc_sev_snp_enabled: NvBool = 0;
/// `os_cc_sme_enabled`.
#[unsafe(no_mangle)]
pub static mut os_cc_sme_enabled: NvBool = 0;
/// `os_cc_snp_vtom_enabled`.
#[unsafe(no_mangle)]
pub static mut os_cc_snp_vtom_enabled: NvBool = 0;
/// `os_cc_tdx_enabled`.
#[unsafe(no_mangle)]
pub static mut os_cc_tdx_enabled: NvBool = 0;
/// `os_dma_buf_enabled`: no dma-buf until N3b.
#[unsafe(no_mangle)]
pub static mut os_dma_buf_enabled: NvBool = 0;
/// `os_imex_channel_is_supported`: no IMEX channels.
#[unsafe(no_mangle)]
pub static mut os_imex_channel_is_supported: NvBool = 0;

// ---------------------------------------------------------------------------
// Memory and strings.
// ---------------------------------------------------------------------------

/// `os_alloc_mem`: from the process heap (§4.2). Linux picks kmalloc or
/// vmalloc by size; the heap serves both.
///
/// # Safety
///
/// `address` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_alloc_mem(address: *mut *mut c_void, size: u64) -> NvStatus {
    if address.is_null() {
        return status::INVALID_ARGUMENT;
    }
    let Ok(size) = usize::try_from(size) else {
        return status::INVALID_PARAMETER;
    };
    // SAFETY: malloc takes any size, and answers null when it cannot.
    let block = unsafe { libc::malloc(size.max(1)) };
    // SAFETY: the caller vouches for it.
    unsafe { address.write(block) };
    if block.is_null() {
        status::NO_MEMORY
    } else {
        status::OK
    }
}

/// `os_free_mem`.
///
/// # Safety
///
/// `address` is null or a block `os_alloc_mem` returned, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_free_mem(address: *mut c_void) {
    // SAFETY: the caller vouches for it; free takes null.
    unsafe { libc::free(address) };
}

/// `os_mem_copy`.
///
/// # Safety
///
/// Both ranges are valid for `length` bytes and do not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_mem_copy(to: *mut c_void, from: *const c_void, length: u32) -> *mut c_void {
    // SAFETY: the caller vouches for both ranges.
    unsafe { ptr::copy_nonoverlapping(from.cast::<u8>(), to.cast::<u8>(), length as usize) };
    to
}

/// `os_mem_set`.
///
/// # Safety
///
/// `to` is valid for `length` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_mem_set(to: *mut c_void, value: u8, length: u32) -> *mut c_void {
    // SAFETY: the caller vouches for the range.
    unsafe { ptr::write_bytes(to.cast::<u8>(), value, length as usize) };
    to
}

/// `os_mem_cmp`: as `memcmp`.
///
/// # Safety
///
/// Both ranges are valid for `length` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_mem_cmp(a: *const u8, b: *const u8, length: u32) -> i32 {
    // SAFETY: the caller vouches for both ranges.
    let a = unsafe { core::slice::from_raw_parts(a, length as usize) };
    // SAFETY: as above.
    let b = unsafe { core::slice::from_raw_parts(b, length as usize) };
    a.iter()
        .zip(b)
        .find(|(x, y)| x != y)
        .map_or(0, |(x, y)| i32::from(*x) - i32::from(*y))
}

/// A NUL-terminated string's bytes, without the NUL.
///
/// # Safety
///
/// `text` is a NUL-terminated string.
unsafe fn bytes<'a>(text: *const c_char) -> &'a [u8] {
    // SAFETY: the caller vouches for it.
    unsafe { core::ffi::CStr::from_ptr(text) }.to_bytes()
}

/// `os_string_copy`: as `strcpy`.
///
/// # Safety
///
/// `from` is a string, and `to` has room for it and its NUL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_string_copy(to: *mut c_char, from: *const c_char) -> *mut c_char {
    // SAFETY: the caller vouches for the string.
    let length = unsafe { bytes(from) }.len() + 1;
    // SAFETY: the caller vouches for the room.
    unsafe { ptr::copy(from, to, length) };
    to
}

/// `os_string_length`: as `strlen`.
///
/// # Safety
///
/// `text` is a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_string_length(text: *const c_char) -> u32 {
    // SAFETY: the caller vouches for it.
    u32::try_from(unsafe { bytes(text) }.len()).unwrap_or(u32::MAX)
}

/// `os_string_compare`: as `strcmp`.
///
/// # Safety
///
/// Both are strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_string_compare(a: *const c_char, b: *const c_char) -> i32 {
    // SAFETY: the caller vouches for both.
    let (a, b) = unsafe { (bytes(a), bytes(b)) };
    match a.cmp(b) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// `os_strtoul`: Linux's `simple_strtoul` -- base 0 takes `0x` as hex and
/// a leading `0` as octal, base 16 skips an `0x`, and no sign is taken.
///
/// # Safety
///
/// `text` is a string; `end` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_strtoul(text: *const c_char, end: *mut *mut c_char, base: u32) -> u32 {
    // SAFETY: the caller vouches for the string.
    let digits = unsafe { bytes(text) };
    let (value, used) = strtoul(digits, base);
    if !end.is_null() {
        // SAFETY: the caller vouches for it; `used` is within the string.
        unsafe { end.write(text.cast_mut().add(used)) };
    }
    value
}

/// [`os_strtoul`]'s parse: the value and how many bytes it used.
fn strtoul(text: &[u8], base: u32) -> (u32, usize) {
    let hex_prefix = matches!(text, [b'0', b'x' | b'X', next, ..] if next.is_ascii_hexdigit());
    let (base, mut at) = match base {
        0 if hex_prefix => (16, 2),
        0 if text.first() == Some(&b'0') => (8, 1),
        0 => (10, 0),
        16 if hex_prefix => (16, 2),
        base => (base, 0),
    };
    let mut value: u32 = 0;
    while let Some(digit) = text.get(at).and_then(|&byte| char::from(byte).to_digit(base)) {
        value = value.wrapping_mul(base).wrapping_add(digit);
        at += 1;
    }
    (value, at)
}

// ---------------------------------------------------------------------------
// RM's locks: each allocated, as Linux's are.
// ---------------------------------------------------------------------------

/// A block for one `T`, initialized; null without memory.
fn boxed<T>(value: T) -> *mut T {
    // SAFETY: a fresh block for one `T`; malloc's alignment of 16 suffices
    // for every type here.
    let block = unsafe { libc::malloc(size_of::<T>()) }.cast::<T>();
    if !block.is_null() {
        // SAFETY: a fresh, aligned block.
        unsafe { block.write(value) };
    }
    block
}

/// Whether the caller may sleep: anywhere but the interrupt handler.
fn may_sleep() -> bool {
    !thread::in_isr()
}

/// `os_alloc_mutex`: Linux's mutexes here are semaphores of one, which any
/// thread may release; [`Mutex`] is that.
///
/// # Safety
///
/// `mutex` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_alloc_mutex(mutex: *mut *mut c_void) -> NvStatus {
    let block = boxed(Mutex::new());
    // SAFETY: the caller vouches for it.
    unsafe { mutex.write(block.cast()) };
    if block.is_null() {
        say!("failed to allocate a mutex");
        status::NO_MEMORY
    } else {
        status::OK
    }
}

/// `os_free_mutex`.
///
/// # Safety
///
/// `mutex` is null or one `os_alloc_mutex` made, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_free_mutex(mutex: *mut c_void) {
    // SAFETY: the caller vouches for it; free takes null.
    unsafe { libc::free(mutex) };
}

/// `os_acquire_mutex`.
///
/// # Safety
///
/// `mutex` is one `os_alloc_mutex` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_acquire_mutex(mutex: *mut c_void) -> NvStatus {
    if !may_sleep() {
        return status::INVALID_REQUEST;
    }
    // SAFETY: the caller vouches for it.
    unsafe { &*mutex.cast::<Mutex>() }.lock();
    status::OK
}

/// `os_cond_acquire_mutex`.
///
/// # Safety
///
/// `mutex` is one `os_alloc_mutex` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_cond_acquire_mutex(mutex: *mut c_void) -> NvStatus {
    if !may_sleep() {
        return status::INVALID_REQUEST;
    }
    // SAFETY: the caller vouches for it.
    if unsafe { &*mutex.cast::<Mutex>() }.try_lock() {
        status::OK
    } else {
        status::TIMEOUT_RETRY
    }
}

/// `os_release_mutex`.
///
/// # Safety
///
/// `mutex` is one `os_alloc_mutex` made, held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_release_mutex(mutex: *mut c_void) {
    // SAFETY: the caller vouches for it.
    unsafe { &*mutex.cast::<Mutex>() }.unlock();
}

/// `os_alloc_semaphore`.
#[unsafe(no_mangle)]
pub extern "C" fn os_alloc_semaphore(initial: u32) -> *mut c_void {
    let block = boxed(Semaphore::new(initial));
    if block.is_null() {
        say!("failed to allocate a semaphore");
    }
    block.cast()
}

/// `os_free_semaphore`.
///
/// # Safety
///
/// `semaphore` is null or one `os_alloc_semaphore` made, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_free_semaphore(semaphore: *mut c_void) {
    // SAFETY: the caller vouches for it; free takes null.
    unsafe { libc::free(semaphore) };
}

/// `os_acquire_semaphore`.
///
/// # Safety
///
/// `semaphore` is one `os_alloc_semaphore` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_acquire_semaphore(semaphore: *mut c_void) -> NvStatus {
    if !may_sleep() {
        return status::INVALID_REQUEST;
    }
    // SAFETY: the caller vouches for it.
    unsafe { &*semaphore.cast::<Semaphore>() }.down();
    status::OK
}

/// `os_cond_acquire_semaphore`.
///
/// # Safety
///
/// `semaphore` is one `os_alloc_semaphore` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_cond_acquire_semaphore(semaphore: *mut c_void) -> NvStatus {
    // SAFETY: the caller vouches for it.
    if unsafe { &*semaphore.cast::<Semaphore>() }.try_down() {
        status::OK
    } else {
        status::TIMEOUT_RETRY
    }
}

/// `os_release_semaphore`.
///
/// # Safety
///
/// `semaphore` is one `os_alloc_semaphore` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_release_semaphore(semaphore: *mut c_void) -> NvStatus {
    // SAFETY: the caller vouches for it.
    unsafe { &*semaphore.cast::<Semaphore>() }.up();
    status::OK
}

/// `os_alloc_rwlock`.
#[unsafe(no_mangle)]
pub extern "C" fn os_alloc_rwlock() -> *mut c_void {
    let block = boxed(RwLock::new());
    if block.is_null() {
        say!("failed to allocate a reader-writer lock");
    }
    block.cast()
}

/// `os_free_rwlock`.
///
/// # Safety
///
/// `lock` is null or one `os_alloc_rwlock` made, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_free_rwlock(lock: *mut c_void) {
    // SAFETY: the caller vouches for it; free takes null.
    unsafe { libc::free(lock) };
}

/// A reader-writer lock RM allocated.
///
/// # Safety
///
/// `lock` is one `os_alloc_rwlock` made.
unsafe fn rwlock<'a>(lock: *mut c_void) -> &'a RwLock {
    // SAFETY: the caller vouches for it.
    unsafe { &*lock.cast::<RwLock>() }
}

/// `os_acquire_rwlock_read`.
///
/// # Safety
///
/// `lock` is one `os_alloc_rwlock` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_acquire_rwlock_read(lock: *mut c_void) -> NvStatus {
    if !may_sleep() {
        return status::INVALID_REQUEST;
    }
    // SAFETY: the caller vouches for it.
    unsafe { rwlock(lock) }.read();
    status::OK
}

/// `os_acquire_rwlock_write`.
///
/// # Safety
///
/// `lock` is one `os_alloc_rwlock` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_acquire_rwlock_write(lock: *mut c_void) -> NvStatus {
    if !may_sleep() {
        return status::INVALID_REQUEST;
    }
    // SAFETY: the caller vouches for it.
    unsafe { rwlock(lock) }.write();
    status::OK
}

/// `os_cond_acquire_rwlock_read`: `NV_OK` when taken.
///
/// Linux's 580.173.02 returns `NV_ERR_TIMEOUT_RETRY` when
/// `down_read_trylock` *succeeds* (it returns 1 on success), which would
/// leave the lock held by a caller told it failed. This answers as the
/// call's name and RM's callers mean.
///
/// # Safety
///
/// `lock` is one `os_alloc_rwlock` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_cond_acquire_rwlock_read(lock: *mut c_void) -> NvStatus {
    // SAFETY: the caller vouches for it.
    if unsafe { rwlock(lock) }.try_read() {
        status::OK
    } else {
        status::TIMEOUT_RETRY
    }
}

/// `os_cond_acquire_rwlock_write`: as [`os_cond_acquire_rwlock_read`].
///
/// # Safety
///
/// `lock` is one `os_alloc_rwlock` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_cond_acquire_rwlock_write(lock: *mut c_void) -> NvStatus {
    // SAFETY: the caller vouches for it.
    if unsafe { rwlock(lock) }.try_write() {
        status::OK
    } else {
        status::TIMEOUT_RETRY
    }
}

/// `os_release_rwlock_read`.
///
/// # Safety
///
/// `lock` is one `os_alloc_rwlock` made, read-held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_release_rwlock_read(lock: *mut c_void) {
    // SAFETY: the caller vouches for it.
    unsafe { rwlock(lock) }.read_unlock();
}

/// `os_release_rwlock_write`.
///
/// # Safety
///
/// `lock` is one `os_alloc_rwlock` made, write-held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_release_rwlock_write(lock: *mut c_void) {
    // SAFETY: the caller vouches for it.
    unsafe { rwlock(lock) }.write_unlock();
}

/// `os_semaphore_may_sleep`.
#[unsafe(no_mangle)]
pub extern "C" fn os_semaphore_may_sleep() -> NvBool {
    status::bool(may_sleep())
}

/// `os_is_isr`.
#[unsafe(no_mangle)]
pub extern "C" fn os_is_isr() -> NvBool {
    status::bool(thread::in_isr())
}

/// `os_alloc_spinlock`: a [`Mutex`] taken after a spin (§4.2).
///
/// # Safety
///
/// `lock` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_alloc_spinlock(lock: *mut *mut c_void) -> NvStatus {
    let block = boxed(Mutex::new());
    // SAFETY: the caller vouches for it.
    unsafe { lock.write(block.cast()) };
    if block.is_null() {
        say!("failed to allocate a spinlock");
        status::NO_MEMORY
    } else {
        status::OK
    }
}

/// `os_free_spinlock`.
///
/// # Safety
///
/// `lock` is null or one `os_alloc_spinlock` made, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_free_spinlock(lock: *mut c_void) {
    // SAFETY: the caller vouches for it; free takes null.
    unsafe { libc::free(lock) };
}

/// The x86 `EFLAGS.IF` bit Linux returns: interrupts were on, as they
/// always are in ring 3.
const EFLAGS_IF: u64 = 0x200;

/// `os_acquire_spinlock`: returns what Linux returns from a thread with
/// interrupts on.
///
/// # Safety
///
/// `lock` is one `os_alloc_spinlock` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_acquire_spinlock(lock: *mut c_void) -> u64 {
    // SAFETY: the caller vouches for it.
    unsafe { &*lock.cast::<Mutex>() }.lock_spinning(crate::sync::spinlock_spins());
    EFLAGS_IF
}

/// `os_release_spinlock`.
///
/// # Safety
///
/// `lock` is one `os_alloc_spinlock` made, held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_release_spinlock(lock: *mut c_void, _: u64) {
    // SAFETY: the caller vouches for it.
    unsafe { &*lock.cast::<Mutex>() }.unlock();
}

/// `os_alloc_wait_queue`: a completion (`sync::Completion`).
///
/// # Safety
///
/// `queue` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_alloc_wait_queue(queue: *mut *mut c_void) -> NvStatus {
    let block = boxed(Completion::new());
    // SAFETY: the caller vouches for it.
    unsafe { queue.write(block.cast()) };
    if block.is_null() {
        status::NO_MEMORY
    } else {
        status::OK
    }
}

/// `os_free_wait_queue`.
///
/// # Safety
///
/// `queue` is null or one `os_alloc_wait_queue` made, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_free_wait_queue(queue: *mut c_void) {
    // SAFETY: the caller vouches for it; free takes null.
    unsafe { libc::free(queue) };
}

/// `os_wait_uninterruptible`.
///
/// # Safety
///
/// `queue` is one `os_alloc_wait_queue` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_wait_uninterruptible(queue: *mut c_void) {
    // SAFETY: the caller vouches for it.
    unsafe { &*queue.cast::<Completion>() }.wait();
}

/// `os_wait_interruptible`: nvrm takes no signals, so as above.
///
/// # Safety
///
/// `queue` is one `os_alloc_wait_queue` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_wait_interruptible(queue: *mut c_void) {
    // SAFETY: the caller vouches for it.
    unsafe { &*queue.cast::<Completion>() }.wait();
}

/// `os_wake_up`: `complete_all`.
///
/// # Safety
///
/// `queue` is one `os_alloc_wait_queue` made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_wake_up(queue: *mut c_void) {
    // SAFETY: the caller vouches for it.
    unsafe { &*queue.cast::<Completion>() }.complete_all();
}

// ---------------------------------------------------------------------------
// The processor, the thread, the process.
// ---------------------------------------------------------------------------

/// `os_get_cpu_count`.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_cpu_count() -> u32 {
    // SAFETY: `get_nprocs` takes nothing.
    u32::try_from(unsafe { libc::get_nprocs() }).unwrap_or(1).max(1)
}

/// `os_get_cpu_number`.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_cpu_number() -> u32 {
    // SAFETY: `sched_getcpu` takes nothing.
    u32::try_from(unsafe { libc::sched_getcpu() }).unwrap_or(0)
}

/// `os_get_current_thread`: the thread id, or 0 in the interrupt handler,
/// as Linux answers 0 in interrupt context.
///
/// # Safety
///
/// `thread` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_current_thread(thread_id: *mut u64) -> NvStatus {
    let id = if thread::in_isr() {
        0
    } else {
        u64::from(crate::futex::tid())
    };
    // SAFETY: the caller vouches for it.
    unsafe { thread_id.write(id) };
    status::OK
}

/// `os_get_current_process_flags`: "kernel thread" on a thread this layer
/// started, as Linux says on its kthreads.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_current_process_flags() -> u32 {
    /// `OS_CURRENT_PROCESS_FLAG_KERNEL_THREAD`.
    const KERNEL_THREAD: u32 = 1;
    if thread::is_ours() { KERNEL_THREAD } else { 0 }
}

/// `os_schedule`: yield the processor.
#[unsafe(no_mangle)]
pub extern "C" fn os_schedule() -> NvStatus {
    if !may_sleep() {
        say!("os_schedule: attempted to yield inside the interrupt handler");
        return status::ILLEGAL_ACTION;
    }
    // SAFETY: `sched_yield` takes nothing.
    let _ = unsafe { libc::sched_yield() };
    status::OK
}

/// `os_get_max_user_va`: the top of x86-64's 47-bit user half, Linux's
/// `TASK_SIZE`, which Ferrix's user half matches.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_max_user_va() -> u64 {
    0x7fff_ffff_f000
}

/// `os_get_random_bytes`.
///
/// # Safety
///
/// `buffer` is writable for `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_random_bytes(buffer: *mut u8, count: u16) -> NvStatus {
    let mut done = 0_usize;
    let count = usize::from(count);
    while done < count {
        // SAFETY: the caller vouches for the room; `done` is within it.
        let got = unsafe { libc::getrandom(buffer.add(done).cast(), count - done, 0) };
        match usize::try_from(got) {
            Ok(got) if got > 0 => done += got,
            _ => return status::NOT_READY,
        }
    }
    status::OK
}

/// `os_version_info`, as `os-interface.h` lays it out.
#[repr(C)]
#[derive(Debug)]
pub struct VersionInfo {
    /// `os_major_version`.
    major: u32,
    /// `os_minor_version`.
    minor: u32,
    /// `os_build_number`.
    build: u32,
    /// `os_build_version_str`.
    version: *const c_char,
    /// `os_build_date_plus_str`.
    date: *const c_char,
}

/// `os_get_version_info`: Ferrix, which RM only reports.
///
/// # Safety
///
/// `info` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_version_info(info: *mut VersionInfo) -> NvStatus {
    // SAFETY: the caller vouches for it; the strings are static.
    unsafe {
        info.write(VersionInfo {
            major: 0,
            minor: 1,
            build: 0,
            version: c"Ferrix".as_ptr(),
            date: c"nvrm".as_ptr(),
        });
    }
    status::OK
}

/// `os_get_is_openrm`: this is the open RM.
///
/// # Safety
///
/// `open` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_is_openrm(open: *mut NvBool) -> NvStatus {
    // SAFETY: the caller vouches for it.
    unsafe { open.write(status::TRUE) };
    status::OK
}

/// `os_pat_supported`: Ferrix programs the PAT on x86-64 (N0c).
#[unsafe(no_mangle)]
pub extern "C" fn os_pat_supported() -> NvBool {
    status::TRUE
}

/// `os_is_efi_enabled`: nvrm reaches no EFI tables, so RM is told it did
/// not boot from EFI and looks for none.
#[unsafe(no_mangle)]
pub extern "C" fn os_is_efi_enabled() -> NvBool {
    status::FALSE
}

/// `os_is_vgx_hyper`: not a vGPU host.
#[unsafe(no_mangle)]
pub extern "C" fn os_is_vgx_hyper() -> NvBool {
    status::FALSE
}

/// `os_is_grid_supported`: not a GRID build.
#[unsafe(no_mangle)]
pub extern "C" fn os_is_grid_supported() -> NvBool {
    status::FALSE
}

/// `os_get_grid_csp_support`.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_grid_csp_support() -> u32 {
    0
}

/// `os_inject_vgx_msi`: not a vGPU host.
#[unsafe(no_mangle)]
pub extern "C" fn os_inject_vgx_msi(_: u16, _: u64, _: u32) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_call_vgpu_vfio`: not a vGPU host.
#[unsafe(no_mangle)]
pub extern "C" fn os_call_vgpu_vfio(_: *mut c_void, _: u32) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_device_vm_present`: not a device-VM build.
#[unsafe(no_mangle)]
pub extern "C" fn os_device_vm_present() -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_get_tegra_platform`: not a Tegra.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_tegra_platform(_: *mut u32) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_iommu_sva_bind`: no shared virtual addressing, as Linux answers
/// without `CONFIG_IOMMU_SVA`.
#[unsafe(no_mangle)]
pub extern "C" fn os_iommu_sva_bind(_: *mut c_void, _: *mut *mut c_void, _: *mut u32) -> NvStatus {
    say!("os_iommu_sva_bind: no shared virtual addressing");
    status::INVALID_STATE
}

/// `os_iommu_sva_unbind`.
#[unsafe(no_mangle)]
pub extern "C" fn os_iommu_sva_unbind(_: *mut c_void) {}

/// `os_is_nvswitch_present`: nvrm enumerates nothing but its GPU.
#[unsafe(no_mangle)]
pub extern "C" fn os_is_nvswitch_present() -> NvBool {
    status::FALSE
}

/// `os_imex_channel_count`: no IMEX channels.
#[unsafe(no_mangle)]
pub extern "C" fn os_imex_channel_count() -> i32 {
    0
}

/// `os_imex_channel_get`: no IMEX channels.
#[unsafe(no_mangle)]
pub extern "C" fn os_imex_channel_get(_: u64) -> i32 {
    -1
}

/// `os_numa_memblock_size`: Linux answers `NV_ERR_INVALID_STATE` until a
/// client set the size, which only NUMA-onlining platforms do.
#[unsafe(no_mangle)]
pub extern "C" fn os_numa_memblock_size(_: *mut u64) -> NvStatus {
    status::INVALID_STATE
}

/// `os_get_numa_node_memory_usage`: no NUMA nodes of the GPU's.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_numa_node_memory_usage(_: i32, _: *mut u64, _: *mut u64) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_numa_add_gpu_memory`: GPU memory is never onlined (§4.2).
#[unsafe(no_mangle)]
pub extern "C" fn os_numa_add_gpu_memory(_: *mut c_void, _: u64, _: u64, _: *mut u32) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_numa_remove_gpu_memory`.
#[unsafe(no_mangle)]
pub extern "C" fn os_numa_remove_gpu_memory(_: *mut c_void, _: u64, _: u64, _: u32) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_offline_page_at_address`: no `memory_failure_queue` to ask.
#[unsafe(no_mangle)]
pub extern "C" fn os_offline_page_at_address(address: u64) -> NvStatus {
    say!("os_offline_page_at_address({address:#x}): page offlining is not supported");
    status::NOT_SUPPORTED
}

/// `os_alloc_pages_node`: no NUMA node allocation.
#[unsafe(no_mangle)]
pub extern "C" fn os_alloc_pages_node(_: i32, _: u32, _: u32, _: *mut u64) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_get_smbios_header`: nvrm reaches no firmware tables.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_smbios_header(_: *mut u64) -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_get_acpi_rsdp_from_uefi`: as above.
///
/// # Safety
///
/// `address` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_acpi_rsdp_from_uefi(address: *mut u32) -> NvStatus {
    if address.is_null() {
        return status::INVALID_STATE;
    }
    // SAFETY: the caller vouches for it.
    unsafe { address.write(0) };
    status::NOT_SUPPORTED
}

/// `os_flush_cpu_cache_all`: x86 is coherent; Linux answers the same.
#[unsafe(no_mangle)]
pub extern "C" fn os_flush_cpu_cache_all() -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_flush_user_cache`: as above.
#[unsafe(no_mangle)]
pub extern "C" fn os_flush_user_cache() -> NvStatus {
    status::NOT_SUPPORTED
}

/// `os_flush_cpu_write_combine_buffer`: `sfence`.
#[unsafe(no_mangle)]
pub extern "C" fn os_flush_cpu_write_combine_buffer() {
    // SAFETY: a store fence has no other effect.
    unsafe { core::arch::x86_64::_mm_sfence() };
}

/// `os_disable_console_access`: Linux takes the console lock while RM
/// touches the VGA console; Ferrix's console is not the GPU's.
#[unsafe(no_mangle)]
pub extern "C" fn os_disable_console_access() {}

/// `os_enable_console_access`: as above.
#[unsafe(no_mangle)]
pub extern "C" fn os_enable_console_access() {}

/// `os_add_record_for_crashLog`: Linux's is empty too.
#[unsafe(no_mangle)]
pub extern "C" fn os_add_record_for_crashLog(_: *mut c_void, _: u32) {}

/// `os_delete_record_for_crashLog`: Linux's is empty too.
#[unsafe(no_mangle)]
pub extern "C" fn os_delete_record_for_crashLog(_: *mut c_void) {}

/// `os_dump_stack`: no unwinder; say where.
#[unsafe(no_mangle)]
pub extern "C" fn os_dump_stack() {
    say!(
        "os_dump_stack: RM asked for a stack dump (thread {}); nvrm has no unwinder",
        crate::futex::tid()
    );
}

/// `os_bug_check`: Linux panics the machine; nvrm stops itself.
///
/// # Safety
///
/// `text` is null or a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_bug_check(code: u32, text: *const c_char) {
    // SAFETY: the caller vouches for it.
    let text = unsafe { c_str(text) };
    say!("os_bug_check {code:#x}: {text}; nvrm stops");
    // SAFETY: `abort` ends the process.
    unsafe { libc::abort() };
}

/// `os_dbg_breakpoint`: Linux's does nothing without a kernel debugger.
#[unsafe(no_mangle)]
pub extern "C" fn os_dbg_breakpoint() {}

// ---------------------------------------------------------------------------
// Files: the temporary file RM saves video memory to across suspend.
// ---------------------------------------------------------------------------

/// `os_open_temporary_file`: an unlinked file in `/tmp`, as Linux's
/// `O_TMPFILE`.
///
/// # Safety
///
/// `file` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_open_temporary_file(file: *mut *mut c_void) -> NvStatus {
    let mut path = *b"/tmp/nvrm-XXXXXXXXXXXXXXXX\0";
    for attempt in 0..16_u64 {
        let tag = time::monotonic() ^ (attempt << 48);
        for (index, slot) in path.iter_mut().skip(10).take(16).enumerate() {
            let nibble = (tag >> (index * 4)) & 0xf;
            *slot = b"0123456789abcdef".get(nibble as usize).copied().unwrap_or(b'0');
        }
        // SAFETY: `path` is NUL-terminated.
        let fd = unsafe {
            libc::open(
                path.as_ptr().cast(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                0o600 as c_int,
            )
        };
        if fd >= 0 {
            // SAFETY: as above.
            let _ = unsafe { libc::unlink(path.as_ptr().cast()) };
            // SAFETY: the caller vouches for it. The descriptor travels as
            // the pointer, plus one so that 0 is not null.
            unsafe { file.write((fd as usize + 1) as *mut c_void) };
            return status::OK;
        }
    }
    status::OPERATING_SYSTEM
}

/// The descriptor a file pointer carries.
fn fd_of(file: *mut c_void) -> c_int {
    c_int::try_from((file as usize).wrapping_sub(1)).unwrap_or(-1)
}

/// `os_close_file`.
#[unsafe(no_mangle)]
pub extern "C" fn os_close_file(file: *mut c_void) {
    // SAFETY: the descriptor `os_open_temporary_file` opened.
    let _ = unsafe { libc::close(fd_of(file)) };
}

/// `os_write_file`.
///
/// # Safety
///
/// `buffer` holds `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_write_file(file: *mut c_void, buffer: *const u8, size: u64, offset: u64) -> NvStatus {
    let (Ok(size), Ok(offset)) = (usize::try_from(size), i64::try_from(offset)) else {
        return status::INVALID_ARGUMENT;
    };
    let mut done = 0_usize;
    while done < size {
        // SAFETY: the caller vouches for the buffer; `done` is within it.
        let wrote = unsafe {
            libc::pwrite(fd_of(file), buffer.add(done).cast(), size - done, offset + done as i64)
        };
        match usize::try_from(wrote) {
            Ok(wrote) if wrote > 0 => done += wrote,
            _ => return status::OPERATING_SYSTEM,
        }
    }
    status::OK
}

/// `os_read_file`.
///
/// # Safety
///
/// `buffer` has room for `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_read_file(file: *mut c_void, buffer: *mut u8, size: u64, offset: u64) -> NvStatus {
    let (Ok(size), Ok(offset)) = (usize::try_from(size), i64::try_from(offset)) else {
        return status::INVALID_ARGUMENT;
    };
    let mut done = 0_usize;
    while done < size {
        // SAFETY: the caller vouches for the room; `done` is within it.
        let read = unsafe {
            libc::pread(fd_of(file), buffer.add(done).cast(), size - done, offset + done as i64)
        };
        match usize::try_from(read) {
            Ok(read) if read > 0 => done += read,
            _ => return status::OPERATING_SYSTEM,
        }
    }
    status::OK
}

/// Read a whole file into a fresh block: firmware, from the volume (§4.2).
/// Returns the block, its size in `size`, or null with an `nvos:` line.
///
/// # Safety
///
/// `path` is a string; `size` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_read_whole_file(path: *const c_char, size: *mut u64) -> *mut c_void {
    // SAFETY: the caller vouches for the string.
    let shown = unsafe { c_str(path) };
    // SAFETY: as above.
    let fd = unsafe { libc::open(path, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        say!("{shown}: cannot open (errno {})", libc::errno());
        return ptr::null_mut();
    }
    // SAFETY: a descriptor this call opened.
    let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    let Ok(length) = usize::try_from(end) else {
        // SAFETY: as above.
        let _ = unsafe { libc::close(fd) };
        return ptr::null_mut();
    };
    // SAFETY: malloc takes any size.
    let block = unsafe { libc::malloc(length.max(1)) };
    let mut done = 0_usize;
    while !block.is_null() && done < length {
        // SAFETY: `block` has room for `length`; `done` is within it.
        let read = unsafe { libc::pread(fd, block.cast::<u8>().add(done).cast(), length - done, done as i64) };
        match usize::try_from(read) {
            Ok(read) if read > 0 => done += read,
            _ => break,
        }
    }
    // SAFETY: as above.
    let _ = unsafe { libc::close(fd) };
    if block.is_null() || done != length {
        say!("{shown}: read {done} of {length} bytes");
        // SAFETY: free takes null, and the block is ours.
        unsafe { libc::free(block) };
        return ptr::null_mut();
    }
    // SAFETY: the caller vouches for it.
    unsafe { size.write(length as u64) };
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtoul_reads_as_linux_does() {
        assert_eq!(strtoul(b"0x1f", 0), (0x1f, 4));
        assert_eq!(strtoul(b"017", 0), (0o17, 3));
        assert_eq!(strtoul(b"42z", 0), (42, 2));
        assert_eq!(strtoul(b"0x1f", 16), (0x1f, 4));
        assert_eq!(strtoul(b"ff", 16), (0xff, 2));
        assert_eq!(strtoul(b"0", 0), (0, 1));
        assert_eq!(strtoul(b"-1", 10), (0, 0));
    }

    #[test]
    fn mem_cmp_orders_as_memcmp() {
        let (a, b) = (*b"abc", *b"abd");
        // SAFETY: both are three live bytes.
        assert!(unsafe { os_mem_cmp(a.as_ptr(), b.as_ptr(), 3) } < 0);
        // SAFETY: as above.
        assert_eq!(unsafe { os_mem_cmp(a.as_ptr(), a.as_ptr(), 3) }, 0);
    }
}
