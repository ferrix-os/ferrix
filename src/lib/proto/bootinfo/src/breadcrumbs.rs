//! Boot breadcrumbs: solid blocks the loader and the kernel draw on the
//! framebuffer firmware left, one per early step, for a machine whose serial
//! port is out of reach and whose screen shows nothing between the loader's
//! last line and the boot console (`docs/BAREMETAL.md`).
//!
//! Asked for with [`FLAG`] on the command line; a boot without it draws
//! nothing. This module is only the geometry and the colours, shared so that
//! the loader and the kernel draw one row the table in the document
//! describes; each side writes the pixels through the mapping it has.

use crate::{Framebuffer, PixelFormat};

/// The command-line word that turns the breadcrumbs on.
pub const FLAG: &str = "ferrix.breadcrumbs";

/// Side of one block, in pixels.
pub const BLOCK: u32 = 40;

/// From one block's left edge to the next one's.
pub const PITCH: u32 = 48;

/// The row's distance from the top and the left edge, in pixels: inside the
/// strip the boot console keeps clear at the top (`console::screen`).
pub const ORIGIN: u32 = 8;

/// One early step, in the order a boot passes them. The discriminant is the
/// block's position in the row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Step {
    /// The loader has the framebuffer and the command line, before it leaves
    /// boot services.
    LoaderReady = 0,
    /// `ExitBootServices` succeeded (drawn in [`RETRY`] instead when it took
    /// a second memory map).
    LoaderLeft = 1,
    /// The loader is about to load `CR3` and jump: the last loader step.
    LoaderJump = 2,
    /// The kernel validated the boot info and mapped the framebuffer itself.
    KernelEntered = 3,
    /// The early console is configured.
    KernelConsole = 4,
    /// The loader's hand-off has been printed.
    KernelReported = 5,
    /// Stage 1's self-check passed: from here a panic draws its screen.
    KernelStage1 = 6,
    /// The kernel's own trap vectors are installed.
    KernelTraps = 7,
    /// The frame allocator and heap are up (stage 2).
    KernelMemory = 8,
    /// The vmap arena is up.
    KernelVmap = 9,
    /// Interrupt controllers and clocks are up (stage 3).
    KernelInterrupts = 10,
    /// The timer interrupt has been seen with interrupts on.
    KernelTimer = 11,
    /// The other processors are up (stage 4).
    KernelProcessors = 12,
    /// The scheduler runs (stage 5): the last breadcrumb.
    KernelScheduler = 13,
}

/// How many positions the row has.
pub const STEPS: u8 = Step::KernelScheduler as u8 + 1;

/// The colour of a block drawn where a step failed rather than passed: the
/// position after the last step reached.
pub const FAILED: (u8, u8, u8) = (0xFF, 0x00, 0x00);

/// The colour [`Step::LoaderLeft`] is drawn in when `ExitBootServices` failed
/// once and succeeded on the retry.
pub const RETRY: (u8, u8, u8) = (0xFF, 0x80, 0x00);

impl Step {
    /// The colour the step's block is drawn in, red, green, blue. Loader
    /// steps are blue to white, kernel steps alternate through greens,
    /// yellows and magentas, so two neighbours never look alike; none is
    /// [`FAILED`]'s pure red or [`RETRY`]'s orange.
    #[must_use]
    pub const fn colour(self) -> (u8, u8, u8) {
        match self {
            Step::LoaderReady => (0x20, 0x40, 0xFF),
            Step::LoaderLeft => (0x00, 0xC0, 0xFF),
            Step::LoaderJump => (0xFF, 0xFF, 0xFF),
            Step::KernelEntered => (0x00, 0xFF, 0x00),
            Step::KernelConsole => (0xFF, 0xFF, 0x00),
            Step::KernelReported => (0xFF, 0x00, 0xFF),
            Step::KernelStage1 => (0x00, 0x80, 0x00),
            Step::KernelTraps => (0x80, 0x80, 0x00),
            Step::KernelMemory => (0x80, 0x00, 0x80),
            Step::KernelVmap => (0x00, 0xFF, 0x80),
            Step::KernelInterrupts => (0xFF, 0xFF, 0x80),
            Step::KernelTimer => (0xFF, 0x80, 0xFF),
            Step::KernelProcessors => (0x80, 0xFF, 0x00),
            Step::KernelScheduler => (0x80, 0x80, 0xFF),
        }
    }

    /// The step at `position`, if there is one.
    #[must_use]
    pub const fn at(position: u8) -> Option<Step> {
        Some(match position {
            0 => Step::LoaderReady,
            1 => Step::LoaderLeft,
            2 => Step::LoaderJump,
            3 => Step::KernelEntered,
            4 => Step::KernelConsole,
            5 => Step::KernelReported,
            6 => Step::KernelStage1,
            7 => Step::KernelTraps,
            8 => Step::KernelMemory,
            9 => Step::KernelVmap,
            10 => Step::KernelInterrupts,
            11 => Step::KernelTimer,
            12 => Step::KernelProcessors,
            13 => Step::KernelScheduler,
            _ => return None,
        })
    }
}

/// Where the block at `position` goes, as the pixel offset of its top-left
/// corner from the framebuffer's start and its side: `None` when the screen
/// is too small for it or the format is not one this draws.
#[must_use]
pub fn block_at(framebuffer: &Framebuffer, position: u8) -> Option<Block> {
    if framebuffer.format == PixelFormat::Unknown || !framebuffer.is_present() {
        return None;
    }
    let left = ORIGIN.checked_add(PITCH.checked_mul(u32::from(position))?)?;
    if left.checked_add(BLOCK)? > framebuffer.width || ORIGIN + BLOCK > framebuffer.height {
        return None;
    }
    let first = u64::from(ORIGIN) * u64::from(framebuffer.stride) + u64::from(left);
    let last_line = u64::from(ORIGIN + BLOCK - 1) * u64::from(framebuffer.stride);
    let end_bytes = (last_line + u64::from(left) + u64::from(BLOCK)) * 4;
    (end_bytes <= framebuffer.size).then_some(Block {
        first_pixel: first,
        stride: u64::from(framebuffer.stride),
    })
}

/// One block's place on the screen, in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Block {
    /// Pixels from the framebuffer's start to the block's top-left corner.
    pub first_pixel: u64,
    /// Pixels from one line to the next.
    pub stride: u64,
}

impl Block {
    /// The pixel index of each of the block's pixels, a line at a time.
    pub fn pixels(self) -> impl Iterator<Item = u64> {
        (0..u64::from(BLOCK)).flat_map(move |line| {
            let start = self.first_pixel + line * self.stride;
            start..start + u64::from(BLOCK)
        })
    }
}

/// `colour` as the 32-bit word a pixel of `format` stores.
#[must_use]
pub const fn pixel(format: PixelFormat, (red, green, blue): (u8, u8, u8)) -> u32 {
    match format {
        PixelFormat::Rgbx8888 => (red as u32) | ((green as u32) << 8) | ((blue as u32) << 16),
        _ => (blue as u32) | ((green as u32) << 8) | ((red as u32) << 16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(width: u32, height: u32) -> Framebuffer {
        Framebuffer {
            phys: 0x00FC_1000_0000,
            size: u64::from(width) * u64::from(height) * 4,
            width,
            height,
            stride: width,
            format: PixelFormat::Bgrx8888,
            reclaimable: 0,
            reserved: 0,
        }
    }

    #[test]
    fn every_step_fits_on_the_smallest_screen_seen() {
        let fb = screen(1024, 768);
        for position in 0..=STEPS {
            assert!(block_at(&fb, position).is_some(), "position {position}");
        }
    }

    #[test]
    fn steps_round_trip_and_colours_differ() {
        for position in 0..STEPS {
            let step = Step::at(position).unwrap();
            assert_eq!(step as u8, position);
            assert_ne!(step.colour(), FAILED);
            assert_ne!(step.colour(), RETRY);
            if let Some(next) = Step::at(position + 1) {
                assert_ne!(step.colour(), next.colour());
            }
        }
        assert_eq!(Step::at(STEPS), None);
    }

    #[test]
    fn a_tiny_or_unknown_screen_draws_nothing() {
        assert_eq!(block_at(&screen(40, 40), 0), None);
        let mut fb = screen(1024, 768);
        fb.format = PixelFormat::Unknown;
        assert_eq!(block_at(&fb, 0), None);
    }

    #[test]
    fn block_pixels_stay_inside() {
        let fb = screen(1024, 768);
        let block = block_at(&fb, STEPS).unwrap();
        let last = block.pixels().last().unwrap();
        assert!((last + 1) * 4 <= fb.size);
        assert_eq!(block.pixels().count(), (BLOCK * BLOCK) as usize);
    }

    #[test]
    fn formats_put_red_where_they_say() {
        assert_eq!(pixel(PixelFormat::Bgrx8888, (0xFF, 0, 0)), 0x00FF_0000);
        assert_eq!(pixel(PixelFormat::Rgbx8888, (0xFF, 0, 0)), 0x0000_00FF);
    }
}
