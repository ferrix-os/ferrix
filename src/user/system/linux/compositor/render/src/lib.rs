//! The compositor's software renderer: frames drawn on the CPU with
//! tiny-skia, for the `XRGB8888` dumb buffers `/dev/dri/card0` scans out.
//!
//! Smithay's own CPU renderer is pixman, a C library, and the compositor
//! takes no C device or rendering stack (`src/user/system/linux/compositor/README.md`), so this is
//! the renderer whichever way the Smithay decision in `docs/BACKLOG.md` goes.
//! It depends on neither: nothing here is a Wayland object, a device or a
//! Smithay type. A frame is drawn from rectangles, colours and pixel buffers,
//! and tested on the host against committed images byte for byte.
//!
//! # The pieces
//!
//! * A [`Canvas`] is the frame being drawn: tiny-skia's premultiplied pixmap
//!   the size of the output, kept opaque. Its operations are the ones a
//!   compositor frame needs: [`Canvas::clear`], [`Canvas::fill`],
//!   [`Canvas::border`] and [`Canvas::composite`], each clipped to the
//!   [`Damage`] it is given and each recording what it wrote.
//! * [`Canvas::present`] copies the damaged part of the frame into a
//!   [`Target`], the mapping of a dumb buffer: width, height and the stride
//!   `MODE_CREATE_DUMB` returned. [`Canvas::present_transformed`] does the
//!   same for a monitor that is turned (`monitor = ..., transform, N`),
//!   putting each pixel where the [`transform`] sends it.
//! * A [`Surface`] is a client's pixels as `wl_shm` hands them over,
//!   [`Format::Argb8888`] (premultiplied, drawn with source-over) or
//!   [`Format::Xrgb8888`] (opaque, copied).
//! * [`render`] draws a monitor's whole frame from
//!   `compositor_layout::State::layout`'s answer: the background, then each
//!   window bottom to top, its border in the active or inactive colour of
//!   [`Style`] and its surface inside. [`damage_between`] says what part of
//!   the output changed between two layouts.
//! * [`Pattern`] is the two test clients' pixels, the checkerboard and the
//!   gradient, generated in code so a test and a client draw the same.
//!
//! # Byte order
//!
//! DRM's `XRGB8888` and `wl_shm`'s `ARGB8888` are 32-bit values stored
//! little-endian, so each pixel's bytes are blue, green, red, then X or
//! alpha. tiny-skia's pixmap holds premultiplied red, green, blue, alpha in
//! that byte order, and source-over treats the three colour channels alike,
//! so the canvas keeps *blue* in tiny-skia's red byte and red in its blue
//! byte: a colour is swapped once as it enters a paint, an `ARGB8888`
//! surface's bytes are already in canvas order, and presenting is a copy
//! with no conversion. Because the canvas is opaque, premultiplied colour is
//! the colour itself, and the X byte a frame presents is tiny-skia's alpha,
//! `0xFF`.
//!
//! # Exactness
//!
//! Every edge is on a whole pixel and anti-aliasing is off, so coverage is
//! all or nothing. Blends run on tiny-skia's `f32` pipeline, which rounds
//! `s + d × (255 − a) / 255` to the nearest byte, rather than the `u16`
//! pipeline's approximate division by 255. An opaque colour or an
//! `XRGB8888` surface is written exactly.
//!
//! # Wrapping it in a Smithay `Renderer`
//!
//! Smithay 0.7's `Renderer`, `Frame`, `Bind` and `ImportMem` map onto this
//! crate as follows, in a crate of the compositor's that depends on both:
//!
//! * `RendererSuper::Framebuffer<'buffer>` wraps a [`Target`] (a mapped dumb
//!   buffer, which `Bind<DumbBuffer>` maps), and the renderer owns one
//!   [`Canvas`] per output size.
//! * `Renderer::render` resizes the canvas if the output size changed and
//!   returns a frame borrowing canvas and target. `Frame::clear` is
//!   [`Canvas::clear`], `Frame::draw_solid` is [`Canvas::fill`], and
//!   Smithay's `&[Rectangle<i32, Physical>]` damage becomes a [`Damage`]
//!   with [`Damage::add`], which makes the rectangles disjoint so a
//!   translucent draw is never blended twice where they overlap.
//! * `ImportMem::import_memory` keeps the bytes, size and fourcc
//!   (`Argb8888` and `Xrgb8888`, [`Format::fourcc`]) as the `TextureId`;
//!   `Frame::render_texture_from_to` builds a [`Surface`] over them and calls
//!   [`Canvas::composite`]. The renderer draws a buffer 1:1, so a `src`
//!   whose size differs from `dst`, a transform other than `Normal`, or an
//!   `alpha` below 1 is refused with the renderer's error until scaling is
//!   written; Smithay's space elements at scale 1 do not ask for them.
//! * `Frame::finish` calls [`Canvas::present`] with the damage the frame
//!   accumulated ([`Canvas::take_damage`]) and returns a signalled
//!   `SyncPoint`, since the CPU is done when the call returns.
//! * `DebugFlags`, texture filters and `wait` have nothing to do.
//!
//! # Using it without Smithay
//!
//! A compositor written from scratch calls [`render`] with its layout and a
//! map from window to the buffer each client last committed, passing the
//! damage its own tracking found ([`damage_between`] for the layout, each
//! commit's `wl_surface.damage_buffer` offset to the window), and then
//! [`Canvas::present`] into the buffer it will page-flip. With two buffers
//! flipping, a buffer is behind by the frames drawn into the other, so it
//! presents the union of the damage of the frames since it was last shown.

#![forbid(unsafe_code)]

mod backdrop;
mod blur;
mod buffer;
mod canvas;
mod cores;
pub mod cursor;
mod damage;
mod frame;
pub mod gpu;
mod gradient;
mod paint;
mod patterns;
mod scratch;
pub mod timing;
pub mod transform;

// Public, and not only for this crate's own tests: the term app draws
// its expected image with its own font and its own grid, and the images live
// together whichever crate blesses one.
pub mod golden;
#[cfg(test)]
mod tests;

use core::fmt;

pub use backdrop::Backdrop;
pub use blur::Blur;
pub use buffer::{Format, Surface, Target};
pub use canvas::{Canvas, MAX_SIZE, Rounding, Shadow, rounded_spans};
pub use compositor_config::Color;
pub use compositor_layout::Rect;
pub use damage::{Damage, intersect};
pub use frame::{
    LayerFrame, Style, Styles, WindowStyle, damage_between, damage_between_parts,
    damage_between_styled, outer, reads_backdrop, render, render_onto, render_with_layers, scaled,
};
pub use gradient::Gradient;
pub use paint::Painter;
pub use patterns::Pattern;
pub use transform::Transform;

/// What the renderer could not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A canvas, surface or target size of zero or over [`MAX_SIZE`].
    Size {
        /// The width asked for.
        width: u32,
        /// The height asked for.
        height: u32,
    },
    /// A stride shorter than four bytes a pixel.
    Stride {
        /// The width in pixels.
        width: u32,
        /// The stride in bytes.
        stride: u32,
    },
    /// A buffer too short for its width, height and stride.
    Short {
        /// The bytes the buffer needs.
        needed: usize,
        /// The bytes it has.
        len: usize,
    },
    /// A target whose size is not the canvas's.
    Mismatch {
        /// The canvas's width and height.
        canvas: (u32, u32),
        /// The target's width and height.
        target: (u32, u32),
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Size { width, height } => {
                write!(f, "a {width}x{height} buffer is empty or too large")
            }
            Self::Stride { width, stride } => {
                write!(
                    f,
                    "a stride of {stride} bytes is too short for {width} pixels"
                )
            }
            Self::Short { needed, len } => {
                write!(f, "the buffer has {len} bytes and needs {needed}")
            }
            Self::Mismatch { canvas, target } => write!(
                f,
                "the target is {}x{} and the canvas {}x{}",
                target.0, target.1, canvas.0, canvas.1
            ),
        }
    }
}

impl core::error::Error for Error {}
