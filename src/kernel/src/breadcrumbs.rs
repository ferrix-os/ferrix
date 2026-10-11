//! Boot breadcrumbs: a block drawn on the framebuffer at each early step,
//! for a machine whose serial port is out of reach (`docs/BAREMETAL.md`,
//! `ferrix_bootinfo::breadcrumbs`).
//!
//! Off unless the command line carries `ferrix.breadcrumbs`. When on, the
//! framebuffer is mapped from the kernel's first steps -- at the window stage
//! 1 would map it at for the panic screen, which then takes this mapping
//! rather than making its own -- so a boot that stops before the boot console
//! or a panic screen exists still leaves a row of blocks saying how far it
//! got, and a red one after them when it panicked.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use ferrix_bootinfo::breadcrumbs::{self, Step};
use ferrix_bootinfo::{BootView, Framebuffer};

use crate::early::EarlyMemory;
use crate::vmap;

/// Where the framebuffer is mapped while the breadcrumbs are on, or 0.
static VIRT: AtomicU64 = AtomicU64::new(0);
/// How many bytes of it are mapped there.
static MAPPED: AtomicU64 = AtomicU64::new(0);
/// One past the furthest step drawn: where a failure's block goes.
static REACHED: AtomicU8 = AtomicU8::new(0);

/// The framebuffer the loader described, written once by [`start`] before
/// [`VIRT`] is published.
struct Screen(UnsafeCell<Option<Framebuffer>>);

// SAFETY: (SHARED) written once by `start`, on the boot processor before any
// other runs and before `VIRT` is stored with release ordering; read only
// after `VIRT` is loaded non-zero with acquire ordering.
unsafe impl Sync for Screen {}

static SCREEN: Screen = Screen(UnsafeCell::new(None));

/// Map the framebuffer and draw the first kernel block, if the command line
/// asks for breadcrumbs and there is a framebuffer the panic screen could
/// draw on too: not one in memory the allocator will own (`docs/DISPLAY.md`
/// §2.4).
pub(crate) fn start(view: &BootView<'_>, memory: &mut EarlyMemory) {
    if !view.flag(breadcrumbs::FLAG) {
        return;
    }
    let framebuffer = view.raw().framebuffer;
    if !framebuffer.is_present() || framebuffer.is_reclaimable() {
        return;
    }
    let at = vmap::FRAMEBUFFER_WINDOW;
    let len = crate::framebuffer_bytes(&framebuffer).min(vmap::FRAMEBUFFER_WINDOW_SIZE);
    if memory.map_framebuffer(at, framebuffer.phys, len).is_err() {
        return;
    }
    // SAFETY: (SHARED) the one write, as the `Sync` impl argues.
    unsafe { *SCREEN.0.get() = Some(framebuffer) };
    MAPPED.store(len, Ordering::Relaxed);
    VIRT.store(at, Ordering::Release);
    mark(Step::KernelEntered);
}

/// The window [`start`] mapped the framebuffer at, and its length, for stage
/// 1 to hand to the panic screen instead of mapping it again.
pub(crate) fn window() -> Option<(u64, u64)> {
    let at = VIRT.load(Ordering::Acquire);
    (at != 0).then(|| (at, MAPPED.load(Ordering::Relaxed)))
}

/// Draw `step`'s block, and every block before it: the boot console clears
/// the screen when it starts, and a row with a gap would read as a step
/// skipped.
pub(crate) fn mark(step: Step) {
    let reached = step as u8 + 1;
    let _ = REACHED.fetch_max(reached, Ordering::Relaxed);
    for position in 0..reached {
        if let Some(earlier) = Step::at(position) {
            paint(position, earlier.colour());
        }
    }
}

/// Draw the red block after the furthest step reached: the kernel panicked.
pub(crate) fn failed() {
    paint(REACHED.load(Ordering::Relaxed), breadcrumbs::FAILED);
}

/// Draw the block at `position` in `colour`, if the breadcrumbs are on.
fn paint(position: u8, colour: (u8, u8, u8)) {
    let at = VIRT.load(Ordering::Acquire);
    if at == 0 {
        return;
    }
    // SAFETY: (SHARED) `VIRT` is non-zero, so `start` wrote this before
    // publishing it, and nothing writes it again.
    let Some(framebuffer) = (unsafe { *SCREEN.0.get() }) else {
        return;
    };
    let Some(block) = breadcrumbs::block_at(&framebuffer, position) else {
        return;
    };
    let mapped = MAPPED.load(Ordering::Relaxed);
    let word = breadcrumbs::pixel(framebuffer.format, colour);
    let base = at as *mut u32;
    for index in block.pixels() {
        if (index + 1) * 4 > mapped {
            return;
        }
        // SAFETY: (DEVICE) `start` mapped `mapped` bytes of the framebuffer at
        // `at`, and the index is inside them; a block is a few pixels in the
        // strip the boot console keeps clear, so a draw racing it changes
        // only what is shown.
        unsafe { base.wrapping_add(index as usize).write_volatile(word) };
    }
}
