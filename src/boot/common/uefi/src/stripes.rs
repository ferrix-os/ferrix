//! Progress stripes the loader paints on the framebuffer while it leaves
//! firmware, for a machine whose screen is its only output.
//!
//! Firmware's console is gone from `console::shutdown` on, and the kernel's
//! own stripes (`console::screen::milestone`) start only once the kernel runs.
//! Between the two a hang left the loader's last line on the screen and said
//! nothing about where it stopped. So each step of the hand-over paints a bar
//! of its own straight into the framebuffer by its physical address, which
//! firmware's page tables identity map until the kernel's are installed, and
//! which the loader may write after `exit_boot_services` as before.
//!
//! The bars are on the right half of the screen, near the bottom, one row per
//! step counted upwards, 12 pixels high: unlike the kernel's, which run the
//! whole width at the top. A step that failed is painted red and the loader
//! stops there rather than return to firmware that is no longer there.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::{Framebuffer, PixelFormat};

/// Physical address of the framebuffer, or 0 when there is none to paint.
static PHYS: AtomicU64 = AtomicU64::new(0);
/// Visible width in pixels.
static WIDTH: AtomicU32 = AtomicU32::new(0);
/// Visible height in pixels.
static HEIGHT: AtomicU32 = AtomicU32::new(0);
/// Pixels from one line to the next.
static STRIDE: AtomicU32 = AtomicU32::new(0);
/// Whether red is the low byte of a pixel (`Rgbx8888`).
static RGB: AtomicU32 = AtomicU32::new(0);

/// The height of one bar, in pixels.
const BAR: u32 = 12;
/// From one bar's top to the next one's.
const PITCH: u32 = 16;
/// Space under the lowest bar.
const BOTTOM: u32 = 24;

/// The steps of the hand-over, as the row each paints.
#[derive(Clone, Copy)]
pub(crate) enum Step {
    /// The last line printed; firmware's console is about to go.
    Printed = 0,
    /// `exit_boot_services` succeeded.
    LeftFirmware = 1,
    /// The memory map is in the boot info.
    MapRecorded = 2,
    /// The caches are cleaned.
    Cleaned = 3,
    /// The next instruction is the switch to the kernel's tables.
    Entering = 4,
}

/// The colour of each step's bar, as 0xRRGGBB: none of them red.
const COLOURS: [u32; 5] = [0xFF_FF_FF, 0x00_E0_E0, 0xFF_A0_00, 0x80_FF_00, 0xFF_00_FF];
/// A step that failed.
const RED: u32 = 0xFF_00_00;

/// Remember the framebuffer to paint on, when it is in a format with four
/// bytes a pixel.
pub(crate) fn record(framebuffer: Option<&Framebuffer>) {
    let Some(framebuffer) = framebuffer else {
        return;
    };
    let rgb = match framebuffer.format {
        PixelFormat::Bgrx8888 => 0,
        PixelFormat::Rgbx8888 => 1,
        _ => return,
    };
    WIDTH.store(framebuffer.width, Ordering::Relaxed);
    HEIGHT.store(framebuffer.height, Ordering::Relaxed);
    STRIDE.store(framebuffer.stride, Ordering::Relaxed);
    RGB.store(rgb, Ordering::Relaxed);
    PHYS.store(framebuffer.phys, Ordering::Release);
}

/// Paint `step`'s bar.
pub(crate) fn reached(step: Step) {
    let row = step as usize;
    paint(row as u32, COLOURS.get(row).copied().unwrap_or(RED));
}

/// Paint `step`'s bar red and stop: what failed there left nothing to return
/// to, and a photo of the screen is the only report there can be.
pub(crate) fn failed(step: Step) -> ! {
    paint(step as u32, RED);
    loop {
        core::hint::spin_loop();
    }
}

/// The calls between [`Step::Printed`] and [`Step::LeftFirmware`], each
/// marked by a short block in the latter's row once it returned: the last
/// block on the screen names the call that did not.
#[derive(Clone, Copy)]
pub(crate) enum Tick {
    /// Firmware's console is forgotten.
    ConsoleGone = 0,
    /// The first `get_memory_map` returned.
    FirstMap = 1,
    /// The first `exit_boot_services` returned success.
    FirstExitOk = 2,
    /// The first `exit_boot_services` returned failure.
    FirstExitFailed = 3,
    /// The second `get_memory_map` returned.
    SecondMap = 4,
}

/// The width of a tick's block, and from one block's left edge to the next.
const TICK: u32 = 40;
/// From one tick's left edge to the next one's.
const TICK_PITCH: u32 = 56;
/// Each tick's colour: white, blue, green, yellow, blue.
const TICK_COLOURS: [u32; 5] = [0xFF_FF_FF, 0x40_80_FF, 0x00_FF_00, 0xFF_FF_00, 0x40_80_FF];

/// Paint `tick`'s block in [`Step::LeftFirmware`]'s row.
pub(crate) fn tick(tick: Tick) {
    let n = tick as u32;
    let colour = TICK_COLOURS.get(n as usize).copied().unwrap_or(RED);
    let from = WIDTH.load(Ordering::Relaxed) / 2 + n * TICK_PITCH;
    paint_span(Step::LeftFirmware as u32, colour, from, from + TICK);
}

/// Paint row `row` from the bottom, on the right half, in `rgb`.
fn paint(row: u32, rgb: u32) {
    let width = WIDTH.load(Ordering::Relaxed);
    paint_span(row, rgb, width / 2, width);
}

/// Paint row `row` from the bottom, from column `from` to before `to`, in
/// `rgb`.
fn paint_span(row: u32, rgb: u32, from: u32, to: u32) {
    let phys = PHYS.load(Ordering::Acquire);
    if phys == 0 {
        return;
    }
    let width = WIDTH.load(Ordering::Relaxed);
    let height = HEIGHT.load(Ordering::Relaxed);
    let stride = u64::from(STRIDE.load(Ordering::Relaxed));
    let Some(top) = height
        .checked_sub(BOTTOM)
        .and_then(|bottom| bottom.checked_sub((row + 1) * PITCH))
    else {
        return;
    };
    let pixel = if RGB.load(Ordering::Relaxed) == 1 {
        ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)
    } else {
        rgb
    };
    for y in top..top + BAR {
        let line = phys + u64::from(y) * stride * 4;
        for x in from..to.min(width) {
            // (x, y) is inside the visible area firmware reported, so
            // the address is inside its framebuffer, which firmware's tables
            // identity map until the kernel's are installed; nothing else
            // draws on it once firmware's console is gone, and before that a
            // pixel of firmware's text is all that can be overwritten.
            let Ok(address) = usize::try_from(line + u64::from(x) * 4) else {
                return;
            };
            // SAFETY: as above.
            unsafe { core::ptr::write_volatile(address as *mut u32, pixel) };
        }
    }
}
