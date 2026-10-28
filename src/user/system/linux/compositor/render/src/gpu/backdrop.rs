//! What is behind the windows, kept on the GPU, and the blur of it.
//!
//! [`crate::Backdrop`]'s job and its shape: a sharp copy of everything
//! behind the windows, brought up to date inside each frame's damage, and
//! the blur of it in tiles, a tile blurred again only after what it was
//! blurred from has changed. Two things differ from the software one, and
//! both because of where the pixels are.
//!
//! It cannot *look*. The software backdrop compares what it is given with
//! what it holds, so a pointer crossing the wallpaper -- which damages it
//! and changes nothing -- blurs nothing. Pixels on a GPU cannot be compared
//! without fetching them, which costs more than the blur. So a tile is stale
//! when the damage reached it, changed or not. A GPU's blur of a few tiles
//! is a fraction of a millisecond, which is what makes that the right trade.
//!
//! And its textures are made by the canvas that first keeps it, since a
//! backdrop has no device of its own.

use super::{Backend, Blend, Filter, Image, Renderer};
use crate::backdrop::{TILE, gathered};
use crate::damage::intersect;
use crate::{Blur, Damage, Painter, Rect, Rounding};

/// What is behind the windows of one screen, and the blur of it.
#[derive(Debug, Clone)]
pub struct Backdrop {
    width: u32,
    height: u32,
    /// The sharp copy and the blurred one, once a canvas has made them.
    images: Option<(Image, Image)>,
    /// One flag a tile, a row of tiles after another: whether the blurred
    /// copy holds the blur of what the sharp one holds now.
    fresh: Vec<bool>,
    /// What the blurred copy was blurred with, or `None` before anything
    /// was.
    with: Option<Blur>,
    blurs: u64,
}

/// How many tiles cover `size` pixels.
fn tiles(size: u32) -> usize {
    usize::try_from(i64::from(size).saturating_add(TILE - 1) / TILE).unwrap_or(0)
}

impl Backdrop {
    /// A backdrop for a `width` × `height` canvas, with nothing kept yet.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            images: None,
            fresh: vec![false; tiles(width).saturating_mul(tiles(height))],
            with: None,
            blurs: 0,
        }
    }

    /// How many regions have been blurred, which is what a test counts to
    /// see that a frame that changed nothing behind the windows blurred
    /// nothing.
    #[must_use]
    pub const fn blurs(&self) -> u64 {
        self.blurs
    }

    /// Whether this is a backdrop for a canvas of that size.
    pub(super) const fn fits(&self, width: u32, height: u32) -> bool {
        self.width == width && self.height == height
    }

    /// The tiles `rect` touches, as a range of columns and one of rows.
    fn span(&self, rect: Rect) -> Option<(core::ops::Range<usize>, core::ops::Range<usize>)> {
        let rect = intersect(
            rect,
            Rect::new(0, 0, i64::from(self.width), i64::from(self.height)),
        )?;
        let index = |value: i64| usize::try_from(value.max(0)).unwrap_or(0);
        Some((
            index(rect.x.div_euclid(TILE))..index((rect.right() - 1).div_euclid(TILE) + 1),
            index(rect.y.div_euclid(TILE))..index((rect.bottom() - 1).div_euclid(TILE) + 1),
        ))
    }

    /// Set every tile `rect` touches to `fresh`.
    fn mark(&mut self, rect: Rect, fresh: bool) {
        let Some((columns, rows)) = self.span(rect) else {
            return;
        };
        let across = tiles(self.width);
        for row in rows {
            for column in columns.clone() {
                if let Some(flag) = self.fresh.get_mut(row * across + column) {
                    *flag = fresh;
                }
            }
        }
    }

    /// The tiles `rect` touches that are not fresh.
    fn stale_in(&self, rect: Rect) -> Vec<Rect> {
        let Some((columns, rows)) = self.span(rect) else {
            return Vec::new();
        };
        let across = tiles(self.width);
        let place = |at: usize| i64::try_from(at).unwrap_or(0).saturating_mul(TILE);
        rows.flat_map(|row| columns.clone().map(move |column| (column, row)))
            .filter(|(column, row)| self.fresh.get(row * across + column) == Some(&false))
            .map(|(column, row)| Rect::new(place(column), place(row), TILE, TILE))
            .collect()
    }
}

impl<B: Backend> Renderer<B> {
    /// The backdrop's two textures, made the first time it is kept.
    fn backdrop_images(&mut self, backdrop: &mut Backdrop) -> Option<(Image, Image)> {
        if let Some(images) = backdrop.images {
            return Some(images);
        }
        let made = self
            .image(self.width, self.height, false, true)
            .and_then(|sharp| Ok((sharp, self.image(self.width, self.height, false, true)?)));
        match made {
            Ok(images) => {
                backdrop.images = Some(images);
                Some(images)
            }
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    /// [`Painter::keep_backdrop`]: copy the frame so far into the sharp
    /// copy, inside `damage`, and call every tile the damage could show in
    /// stale.
    pub(super) fn keep(&mut self, backdrop: &mut Backdrop, damage: &Damage) {
        if self.failed.is_some() || !backdrop.fits(self.width, self.height) {
            return;
        }
        let bounds = Painter::bounds(self);
        let quads = Self::clips(bounds, damage, bounds);
        if quads.is_empty() {
            return;
        }
        let Some((sharp, _)) = self.backdrop_images(backdrop) else {
            return;
        };
        let target = self.target;
        self.aim(sharp);
        self.textured(
            target,
            true,
            Filter::Nearest,
            bounds,
            (bounds, Rounding::none()),
            1.0,
            Blend::Replace,
            &quads,
        );
        // A changed pixel shows in the blur as far away as the kernel
        // reaches. Before anything has been blurred every tile is stale
        // already.
        if let Some(reach) = backdrop.with.map(|blur| blur.reach()) {
            for quad in quads {
                backdrop.mark(
                    Rect::new(
                        quad.x.saturating_sub(reach),
                        quad.y.saturating_sub(reach),
                        quad.width.saturating_add(reach.saturating_mul(2)),
                        quad.height.saturating_add(reach.saturating_mul(2)),
                    ),
                    false,
                );
            }
        }
    }

    /// Blur every tile under `clips` that is not the blur of what is behind
    /// it now, and answer the texture the blur is in.
    pub(super) fn freshened(
        &mut self,
        backdrop: &mut Backdrop,
        clips: &[Rect],
        blur: &Blur,
    ) -> Option<Image> {
        let (sharp, blurred) = self.backdrop_images(backdrop)?;
        if backdrop.with != Some(*blur) {
            backdrop.fresh.fill(false);
            backdrop.with = Some(*blur);
        }
        let stale: Vec<Rect> = clips
            .iter()
            .flat_map(|&clip| backdrop.stale_in(clip))
            .collect();
        let bounds = Painter::bounds(self);
        for region in gathered(stale, blur.reach()) {
            let Some(region) = intersect(region, bounds) else {
                continue;
            };
            self.blurred_into(sharp, blurred, region, blur)?;
            backdrop.mark(region, true);
            backdrop.blurs = backdrop.blurs.saturating_add(1);
        }
        Some(blurred)
    }
}
