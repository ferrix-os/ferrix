//! `ferrix-nvos`: the OS layer NVIDIA's resource manager runs on inside
//! `nvrm`, the ring-3 program that hosts it (`docs/NVIDIA.md` §4.2, N1c).
//!
//! RM (`nv-kernel.o`, from open-gpu-kernel-modules 580.173.02) was written
//! to sit on Linux's glue. Of its 406 imports, this crate answers the
//! `os_*` calls whose arguments are plain values -- memory, locks,
//! semaphores, wait queues, time, work queues, PCI configuration, kernel
//! mappings, client copies and identity -- and gives NVIDIA's kept C
//! (`os/kept/`) the primitives it is rebuilt on: futex locks it can embed,
//! timers, work queues, pinned pages and the interrupt thread. What takes a
//! `va_list` is C (`os/glue/`), as are the loud stubs.
//!
//! It is a `no_std` static library linked into a C program beside
//! ferrousli, Ferrix's C library, and calls that library for the heap,
//! threads, clocks and files; futexes and Ferrix's native calls it makes by
//! the instruction (`futex`). The same library links against glibc for the
//! host's run of `nvrm-link-test`, where the native calls answer `ENOSYS`
//! and no device is attached.
//!
//! x86-64 only, as nvrm is.

#![cfg_attr(not(test), no_std)]

pub mod chardev;
pub mod client;
pub mod cpu;
pub mod device;
mod futex;
mod libc;
pub mod log;
pub mod os;
pub mod pages;
pub mod rmcore;
pub mod sha256;
pub mod status;
pub mod sync;
mod thread;
pub mod time;
pub mod timer;
pub mod workq;

pub use thread::nvos_isr_enter_leave;

/// A panic has nowhere to go: the frames above are RM's C. Say where, and
/// stop the process.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    log::say_line(format_args!("panic: {info}; nvrm stops"));
    // SAFETY: `abort` ends the process.
    unsafe { libc::abort() }
}

/// RM's entry point for a work item, which the tests have no RM to give.
#[cfg(test)]
#[unsafe(no_mangle)]
extern "C" fn rm_execute_work_item(_: *mut core::ffi::c_void, _: *mut core::ffi::c_void) {}
