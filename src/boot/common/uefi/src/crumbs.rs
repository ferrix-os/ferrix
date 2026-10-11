//! The loader's boot breadcrumbs (`ferrix_bootinfo::breadcrumbs`,
//! `docs/BAREMETAL.md`): blocks drawn straight into the framebuffer at its
//! physical address, which firmware's page tables map one to one and which
//! stay installed until the jump.
//!
//! The one thing the loader can show after `exit_boot_services`, when its
//! console is gone. Off unless the command line asks.

use core::cell::UnsafeCell;

use ferrix_bootinfo::Framebuffer;
use ferrix_bootinfo::breadcrumbs::{self, Step};

/// The framebuffer to draw on, once [`arm`] found the flag.
struct Armed(UnsafeCell<Option<Framebuffer>>);

// SAFETY: the loader is single-threaded (see `console`), so there is never a
// second accessor.
unsafe impl Sync for Armed {}

static ARMED: Armed = Armed(UnsafeCell::new(None));

/// Draw from here on if `cmdline` carries the flag and there is a framebuffer
/// the loader can reach. Answers whether it will.
pub(crate) fn arm(framebuffer: Option<Framebuffer>, cmdline: &str) -> bool {
    if !ferrix_bootinfo::flag_in(cmdline, breadcrumbs::FLAG) {
        return false;
    }
    let Some(framebuffer) = framebuffer.filter(Framebuffer::is_present) else {
        return false;
    };
    // SAFETY: single-threaded, as on the `Sync` impl.
    unsafe { *ARMED.0.get() = Some(framebuffer) };
    true
}

/// Whether [`arm`] found the flag.
pub(crate) fn armed() -> bool {
    // SAFETY: single-threaded, as on the `Sync` impl.
    unsafe { (*ARMED.0.get()).is_some() }
}

/// Draw `step`'s block in its own colour.
pub(crate) fn mark(step: Step) {
    paint(step as u8, step.colour());
}

/// Draw the block at `position` in `colour`.
pub(crate) fn paint(position: u8, colour: (u8, u8, u8)) {
    // SAFETY: single-threaded, as on the `Sync` impl.
    let Some(framebuffer) = (unsafe { *ARMED.0.get() }) else {
        return;
    };
    let Some(block) = breadcrumbs::block_at(&framebuffer, position) else {
        return;
    };
    let word = breadcrumbs::pixel(framebuffer.format, colour);
    let base = framebuffer.phys as *mut u32;
    for index in block.pixels() {
        // SAFETY: firmware's page tables, installed until the jump, map
        // physical memory one to one, the framebuffer's aperture included;
        // `block_at` keeps every pixel inside `framebuffer.size`, and nothing
        // else draws on the screen once the loader's console is shut down.
        unsafe { base.wrapping_add(index as usize).write_volatile(word) };
    }
}
