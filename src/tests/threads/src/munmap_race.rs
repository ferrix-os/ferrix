//! **munmap-race**: a thread's first read of a page and another's `munmap` of it.
//!
//! A kernel that fills a page it has already been told is gone leaves a program
//! reading memory that may be someone else's by now. The read may come before the
//! unmap or after it, but once `munmap` has returned a further read must fault. In a
//! forked child, so that the fault ends only the child, and the parent reads how it
//! ended. `docs/microkernel/M5-LIFT.md` §10 (ledger 6012, R1) names its control, which
//! holds the fill back at this page, by this address.

#![expect(
    unsafe_code,
    reason = "a page mapped and unmapped by hand, and a read that must fault"
)]

use core::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

/// Fixed so a control can recognise it, and in a 32-bit program's reach.
const AT: usize = 0x5a5a_0000;
const PAGE: usize = 4096;

static STARTING: AtomicBool = AtomicBool::new(false);
static UNMAPPED: AtomicBool = AtomicBool::new(false);

fn child() -> ! {
    // SAFETY: nothing else in this child maps at `AT`.
    let page = unsafe {
        libc::mmap(
            ptr::without_provenance_mut(AT),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if page.cast::<u8>().addr() != AT {
        // SAFETY: ending the child.
        unsafe { libc::_exit(2) };
    }
    let reader = std::thread::spawn(move || {
        STARTING.store(true, Ordering::Release);
        let address = ptr::without_provenance::<u8>(AT);
        // SAFETY: mapped until the other thread unmaps it, and a fault after that is
        // what this step is looking for: it ends the child.
        let _ = unsafe { ptr::read_volatile(address) };
        while !UNMAPPED.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        // SAFETY: as above; this one must fault.
        let _ = unsafe { ptr::read_volatile(address) };
        // SAFETY: ending the child, with what it found.
        unsafe { libc::_exit(3) };
    });
    while !STARTING.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    // SAFETY: the reader alone touches this mapping, and a fault is its end.
    let unmapped = unsafe { libc::munmap(ptr::without_provenance_mut(AT), PAGE) };
    UNMAPPED.store(true, Ordering::Release);
    let _ = reader.join();
    // SAFETY: ending the child. The reader ended it first, unless munmap failed.
    unsafe { libc::_exit(if unmapped == 0 { 4 } else { 5 }) };
}

/// The step: a read after `munmap` returned faults, in each of `ROUNDS` children.
pub fn check_munmap_race(fail: fn(&str) -> !) {
    const ROUNDS: usize = 8;
    for _ in 0..ROUNDS {
        // SAFETY: the child calls only what a fork from a threaded program may use
        // at this step: this program's other threads have been joined.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            child();
        }
        if pid < 0 {
            fail("fork for the munmap race");
        }
        let mut status = 0_i32;
        // SAFETY: `status` is this frame's.
        if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
            fail("waitpid for the munmap race");
        }
        if status & 0x7f == libc::SIGSEGV {
            continue;
        }
        if status >> 8 & 0xff == 3 {
            fail("read after munmap: the page was still there");
        }
        fail("the munmap race's child ended some other way");
    }
}
