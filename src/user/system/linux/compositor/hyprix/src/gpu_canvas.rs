//! A screen's GPU painter, whichever it is: virgl's streams on the render
//! node or the test server, or hyprix-gpu's ops (`crate::helper`).
//!
//! The frame is `compositor_render::render_onto` either way; this only
//! passes each `Painter` call to the painter there is, so the code that
//! draws, shows and gives up a GPU frame has one type to hold.

use std::io;
use std::os::fd::OwnedFd;

use compositor_render::{Blur, Color, Damage, Gradient, Painter, Rect, Rounding, Shadow, Surface};

/// The GPU painter a screen has.
#[derive(Debug)]
pub enum GpuCanvas {
    /// virgl streams.
    Virgl(compositor_render::gpu::Canvas<Box<dyn compositor_virgl::Device>>),
    /// hyprix-gpu's ops.
    Helper(crate::helper::Canvas),
}

/// What the painter keeps behind the windows.
#[derive(Debug)]
pub enum GpuBackdrop {
    /// virgl's.
    Virgl(compositor_render::gpu::Backdrop),
    /// hyprix-gpu's, which is in the helper.
    Helper(crate::helper::Backdrop),
}

impl GpuCanvas {
    /// The backdrop this painter keeps, for a `width` × `height` screen.
    #[must_use]
    pub fn backdrop(&self, width: u32, height: u32) -> GpuBackdrop {
        match self {
            Self::Virgl(_) => {
                GpuBackdrop::Virgl(compositor_render::gpu::Backdrop::new(width, height))
            }
            Self::Helper(_) => GpuBackdrop::Helper(crate::helper::Backdrop::new(width, height)),
        }
    }

    /// Submit the frame and wait for it.
    ///
    /// # Errors
    ///
    /// Whatever the device or the helper said.
    pub fn finish(&mut self) -> io::Result<()> {
        match self {
            Self::Virgl(canvas) => canvas.finish(),
            Self::Helper(canvas) => canvas.finish(),
        }
    }

    /// The pixels of `rect`, rows packed.
    ///
    /// # Errors
    ///
    /// As [`GpuCanvas::finish`]'s.
    pub fn read(&mut self, rect: Rect) -> io::Result<Vec<u8>> {
        match self {
            Self::Virgl(canvas) => canvas.read(rect),
            Self::Helper(canvas) => canvas.read(rect),
        }
    }

    /// The pixels of `rect`, into `buffer` at the same place.
    ///
    /// # Errors
    ///
    /// As [`GpuCanvas::finish`]'s.
    pub fn read_into(&mut self, rect: Rect, buffer: &mut [u8], stride: u32) -> io::Result<()> {
        match self {
            Self::Virgl(canvas) => canvas.read_into(rect, buffer, stride),
            Self::Helper(canvas) => canvas.read_into(rect, buffer, stride),
        }
    }

    /// The buffer the frame is drawn into, for the card to show.
    ///
    /// # Errors
    ///
    /// As [`GpuCanvas::finish`]'s.
    pub fn export(&mut self) -> io::Result<Option<OwnedFd>> {
        match self {
            Self::Virgl(canvas) => canvas.export(),
            Self::Helper(canvas) => canvas.export(),
        }
    }
}

impl Painter for GpuCanvas {
    type Backdrop = GpuBackdrop;

    fn bounds(&self) -> Rect {
        match self {
            Self::Virgl(canvas) => canvas.bounds(),
            Self::Helper(canvas) => canvas.bounds(),
        }
    }

    fn clear(&mut self, color: Color, damage: &Damage) {
        match self {
            Self::Virgl(canvas) => canvas.clear(color, damage),
            Self::Helper(canvas) => canvas.clear(color, damage),
        }
    }

    fn fill(&mut self, rect: Rect, color: Color, damage: &Damage) {
        match self {
            Self::Virgl(canvas) => canvas.fill(rect, color, damage),
            Self::Helper(canvas) => canvas.fill(rect, color, damage),
        }
    }

    fn fill_rounded(&mut self, rect: Rect, rounding: Rounding, color: Color, damage: &Damage) {
        match self {
            Self::Virgl(canvas) => canvas.fill_rounded(rect, rounding, color, damage),
            Self::Helper(canvas) => canvas.fill_rounded(rect, rounding, color, damage),
        }
    }

    fn fill_rounded_gradient(
        &mut self,
        rect: Rect,
        rounding: Rounding,
        gradient: &Gradient,
        damage: &Damage,
    ) {
        match self {
            Self::Virgl(canvas) => canvas.fill_rounded_gradient(rect, rounding, gradient, damage),
            Self::Helper(canvas) => canvas.fill_rounded_gradient(rect, rounding, gradient, damage),
        }
    }

    fn border_gradient(&mut self, rect: Rect, width: i64, gradient: &Gradient, damage: &Damage) {
        match self {
            Self::Virgl(canvas) => canvas.border_gradient(rect, width, gradient, damage),
            Self::Helper(canvas) => canvas.border_gradient(rect, width, gradient, damage),
        }
    }

    fn shadow(&mut self, rect: Rect, shadow: &Shadow, damage: &Damage) {
        match self {
            Self::Virgl(canvas) => canvas.shadow(rect, shadow, damage),
            Self::Helper(canvas) => canvas.shadow(rect, shadow, damage),
        }
    }

    fn blur(&mut self, rect: Rect, rounding: Rounding, blur: &Blur, damage: &Damage) {
        match self {
            Self::Virgl(canvas) => canvas.blur(rect, rounding, blur, damage),
            Self::Helper(canvas) => canvas.blur(rect, rounding, blur, damage),
        }
    }

    fn composite(&mut self, surface: &Surface<'_>, rect: Rect, damage: &Damage) {
        match self {
            Self::Virgl(canvas) => canvas.composite(surface, rect, damage),
            Self::Helper(canvas) => canvas.composite(surface, rect, damage),
        }
    }

    fn composite_scaled(
        &mut self,
        surface: &Surface<'_>,
        rect: Rect,
        rounding: Rounding,
        opacity: f32,
        nearest: bool,
        damage: &Damage,
    ) {
        match self {
            Self::Virgl(canvas) => {
                canvas.composite_scaled(surface, rect, rounding, opacity, nearest, damage);
            }
            Self::Helper(canvas) => {
                canvas.composite_scaled(surface, rect, rounding, opacity, nearest, damage);
            }
        }
    }

    fn backdrop_fits(&self, backdrop: &Self::Backdrop) -> bool {
        match (self, backdrop) {
            (Self::Virgl(canvas), GpuBackdrop::Virgl(backdrop)) => canvas.backdrop_fits(backdrop),
            (Self::Helper(canvas), GpuBackdrop::Helper(backdrop)) => canvas.backdrop_fits(backdrop),
            _ => false,
        }
    }

    fn keep_backdrop(&mut self, backdrop: &mut Self::Backdrop, damage: &Damage) {
        match (self, backdrop) {
            (Self::Virgl(canvas), GpuBackdrop::Virgl(backdrop)) => {
                canvas.keep_backdrop(backdrop, damage);
            }
            (Self::Helper(canvas), GpuBackdrop::Helper(backdrop)) => {
                canvas.keep_backdrop(backdrop, damage);
            }
            _ => {}
        }
    }

    fn blur_backdrop(
        &mut self,
        backdrop: &mut Self::Backdrop,
        rect: Rect,
        rounding: Rounding,
        blur: &Blur,
        damage: &Damage,
    ) {
        match (self, backdrop) {
            (Self::Virgl(canvas), GpuBackdrop::Virgl(backdrop)) => {
                canvas.blur_backdrop(backdrop, rect, rounding, blur, damage);
            }
            (Self::Helper(canvas), GpuBackdrop::Helper(backdrop)) => {
                canvas.blur_backdrop(backdrop, rect, rounding, blur, damage);
            }
            _ => {}
        }
    }
}
