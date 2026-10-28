//! The blur: Hyprland's four passes, over a pyramid of textures.
//!
//! `crate::blur` says what the passes are and why each is where it is; it
//! is this crate's port of them to the CPU, and its shape is kept here so
//! that the two blurs are the same picture. Level 0 is the canvas's size and
//! each level is half the one above it. A region is prepared into level 0,
//! taken down the levels and back up them -- the way up writes over the way
//! down, which nothing reads again -- and finished into a texture of its
//! own, from which a window's shape is copied.
//!
//! # Where the pyramid is anchored
//!
//! To the canvas, not to the region. `crate::Canvas::blur_from` snaps what
//! it reads out to the lattice the pyramid halves on, so that a blur over a
//! damaged strip lands on the same source pixels at every level as a blur
//! over the whole window. Here the levels are whole textures, a level's
//! pixel is always the same four of the level above, and the region is only
//! which part of each is drawn: the same anchoring for nothing. The region
//! is still grown by the kernel's reach and snapped, because what is
//! *outside* it at each level is whatever an earlier blur left there, and
//! only pixels a reach inside the region are free of it. Those are the ones
//! copied out.

use super::{Backdrop, Backend, Blend, Filter, Image, Program, Renderer};
use crate::damage::{bounding, intersect};
use crate::{Blur, Damage, Painter, Rect, Rounding};

/// The most passes taken, which is Hyprland's own clamp.
const MAX_PASSES: u32 = 8;

/// `1 / size`, for a shader that works in pixels and samples in shares.
#[expect(
    clippy::cast_precision_loss,
    reason = "a texture's size, at most 16384, exact in f32"
)]
fn inverse(image: Image) -> (f32, f32) {
    (1.0 / image.width as f32, 1.0 / image.height as f32)
}

/// `rect` of level 0, as the pixels of level `level` that cover it.
fn at_level(rect: Rect, level: u32, image: Image) -> Option<Rect> {
    let step = 1_i64 << level.min(30);
    let (left, top) = (rect.x.div_euclid(step), rect.y.div_euclid(step));
    let right = rect.right().saturating_add(step - 1).div_euclid(step);
    let bottom = rect.bottom().saturating_add(step - 1).div_euclid(step);
    intersect(
        Rect::new(
            left,
            top,
            right.saturating_sub(left),
            bottom.saturating_sub(top),
        ),
        Rect::new(0, 0, i64::from(image.width), i64::from(image.height)),
    )
}

impl<B: Backend> Renderer<B> {
    /// Make the pyramid's levels and the finished blur's texture, the first
    /// time they are needed and again if there are more passes than before.
    fn pyramid(&mut self, passes: u32) -> Option<()> {
        while self.levels.len() <= passes as usize {
            let level = u32::try_from(self.levels.len()).unwrap_or(u32::MAX);
            let shift = level.min(30);
            let wide = self.width.div_ceil(1 << shift).max(1);
            let tall = self.height.div_ceil(1 << shift).max(1);
            match self.image(wide, tall, false, true) {
                Ok(image) => self.levels.push(image),
                Err(error) => {
                    self.fail(error);
                    return None;
                }
            }
        }
        if self.blurred.is_none() {
            match self.image(self.width, self.height, false, true) {
                Ok(image) => self.blurred = Some(image),
                Err(error) => {
                    self.fail(error);
                    return None;
                }
            }
        }
        Some(())
    }

    /// Blur `written` of `source` into `written` of `into`.
    pub(super) fn blurred_into(
        &mut self,
        source: Image,
        into: Image,
        written: Rect,
        blur: &Blur,
    ) -> Option<()> {
        let passes = blur.passes.min(MAX_PASSES);
        self.pyramid(passes)?;
        // What is read: what is written, a kernel's reach further on every
        // side, out to the lattice. `crate::Canvas::blur_from` says why.
        let lattice = 1_i64 << passes.min(6);
        let reach = blur.size.saturating_mul(lattice).saturating_mul(2);
        let snapped = |value: i64, up: bool| {
            let down = value.div_euclid(lattice).saturating_mul(lattice);
            if up && down < value {
                down.saturating_add(lattice)
            } else {
                down
            }
        };
        let (left, top) = (
            snapped(written.x.saturating_sub(reach), false),
            snapped(written.y.saturating_sub(reach), false),
        );
        let (right, bottom) = (
            snapped(written.right().saturating_add(reach), true),
            snapped(written.bottom().saturating_add(reach), true),
        );
        let read = intersect(
            Rect::new(
                left,
                top,
                right.saturating_sub(left),
                bottom.saturating_sub(top),
            ),
            Painter::bounds(self),
        )?;
        let levels: Vec<Image> = self
            .levels
            .iter()
            .copied()
            .take(passes as usize + 1)
            .collect();
        let first = *levels.first()?;
        let whole = |image: Image| Rect::new(0, 0, i64::from(image.width), i64::from(image.height));

        // blurprepare.glsl.
        let (wide, tall) = inverse(first);
        self.aim(first);
        self.sample(source, true, Filter::Nearest);
        self.program(
            Program::BlurPrepare,
            Blend::Replace,
            &[
                wide,
                tall,
                blur.contrast.clamp(0.0, 2.0),
                blur.brightness.clamp(0.0, 2.0).max(1.0),
            ],
        );
        self.draw(&[read], whole(first));

        // blur1.glsl, down the levels.
        #[expect(clippy::cast_precision_loss, reason = "a blur's size in pixels")]
        let radius = blur.size as f32;
        #[expect(clippy::cast_precision_loss, reason = "at most eight passes")]
        let boost = if passes == 0 {
            0.0
        } else {
            blur.vibrancy.clamp(0.0, 1.0) / passes as f32
        };
        let darkness = 1.0 - blur.vibrancy_darkness.clamp(0.0, 1.0);
        for (level, pair) in (1_u32..).zip(levels.windows(2)) {
            let [from, to] = pair else { continue };
            let Some(region) = at_level(read, level, *to) else {
                continue;
            };
            let (wide, tall) = inverse(*from);
            self.aim(*to);
            self.sample(*from, false, Filter::Linear);
            self.program(
                Program::BlurDown,
                Blend::Replace,
                &[wide, tall, radius, 0.0, boost, darkness, 0.0, 0.0],
            );
            self.draw(&[region], whole(*to));
        }

        // blur2.glsl, back up them.
        for (level, pair) in (0..passes).rev().zip(levels.windows(2).rev()) {
            let [to, from] = pair else { continue };
            let Some(region) = at_level(read, level, *to) else {
                continue;
            };
            let (wide, tall) = inverse(*from);
            self.aim(*to);
            self.sample(*from, false, Filter::Linear);
            self.program(
                Program::BlurUp,
                Blend::Replace,
                &[wide, tall, radius / 4.0, 0.0],
            );
            self.draw(&[region], whole(*to));
        }

        // blurFinish.glsl, into a texture of its own.
        let (wide, tall) = inverse(first);
        let bounds = Painter::bounds(self);
        self.aim(into);
        self.sample(first, false, Filter::Nearest);
        self.program(
            Program::BlurFinish,
            Blend::Replace,
            &[
                wide,
                tall,
                blur.noise.clamp(0.0, 1.0),
                blur.brightness.clamp(0.0, 2.0).min(1.0),
                0.0,
                0.0,
                super::float(bounds.width),
                super::float(bounds.height),
                0.0,
                2.0,
                0.0,
                0.0,
            ],
        );
        self.draw(&[written], bounds);
        Some(())
    }

    /// [`Painter::blur`] and [`Painter::blur_backdrop`]: replace what is
    /// inside `rect`, corners cut, with the blur of the frame so far or of
    /// `backdrop`.
    pub(super) fn blur_behind(
        &mut self,
        backdrop: Option<&mut Backdrop>,
        rect: Rect,
        rounding: Rounding,
        blur: &Blur,
        damage: &Damage,
    ) {
        if self.failed.is_some() || blur.size <= 0 || blur.passes == 0 {
            return;
        }
        let quads = Self::clips(rect, damage, Painter::bounds(self));
        let Some(written) = bounding(&quads) else {
            return;
        };
        let from = match backdrop.filter(|behind| behind.fits(self.width, self.height)) {
            Some(behind) => self.freshened(behind, &quads, blur),
            None => {
                let target = self.target;
                self.pyramid(blur.passes.min(MAX_PASSES))
                    .and(self.blurred)
                    .and_then(|into| {
                        self.blurred_into(target, into, written, blur)?;
                        Some(into)
                    })
            }
        };
        let Some(from) = from else {
            return;
        };
        // The finished blur is opaque, so drawing it over what is there
        // replaces it inside the shape and leaves it alone outside.
        let bounds = Painter::bounds(self);
        self.aim(self.target);
        self.textured(
            from,
            true,
            Filter::Nearest,
            bounds,
            (rect, rounding),
            1.0,
            Blend::Over,
            &quads,
        );
    }
}
