//! The program the boot's K1 and K11 checks run (`docs/microkernel/K1.md` §10,
//! `docs/microkernel/K11.md` §8).
//!
//! It runs in a shared process's normal mode, at the region's window, as
//! `linuxd` will, and drives small hand-written images in restricted mode. It
//! exits 0 when every case held, and otherwise with the case's number, which
//! the kernel's check turns into the panic's words: so the status is the
//! diagnosis.
//!
//! Its bootstrap handle is a channel, on which the kernel's check gives it the
//! region, its job and the restricted space, and answers two asks: a sweep of
//! the restricted root, and a count of barriers.

#![no_std]
#![no_main]
#![allow(
    unused_results,
    reason = "a call's value is its handle or a count, and most cases want only its status"
)]

mod arch;

ferrix_rt::entry!(main);

/// Run every case.
fn main(bootstrap: ferrix_rt::Bootstrap) -> i32 {
    arch::run(bootstrap)
}
