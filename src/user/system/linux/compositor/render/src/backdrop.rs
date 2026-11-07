//! What is behind the windows, kept from frame to frame, and the blur of it.
//!
//! Hyprland's `decoration:blur:new_optimizations`, which is on by default. A
//! tiled window does not blur the frame as it stands when the window is
//! drawn: the monitor keeps a framebuffer of its own, `m_blurFB`, holding the
//! background and the layer surfaces under the windows *already blurred*,
//! blurs it again only when one of those changes (`m_blurFBDirty`,
//! `CHyprOpenGLImpl::preRender` in `src/render/OpenGL.cpp`), and a window
//! samples it. `IHyprRenderer::shouldUseNewBlurOptimizations` in
//! `src/render/Renderer.cpp` says which windows do; [`crate::reads_backdrop`]
//! is that rule here.
//!
//! On a GPU that saves a few passes. Here it is the difference between a
//! desktop that can be used and one that cannot: a software blur behind a
//! full-screen window is most of a tenth of a second, and without this every
//! frame that touched a translucent window paid it -- one for each pointer
//! motion over a terminal, one for each letter typed into it.
//!
//! So a [`Backdrop`] holds two canvases. `sharp` is what is behind the
//! windows, brought up to date within each frame's damage. `blurred` is the
//! blur of it, in tiles: a tile is blurred the first time a window needs it
//! and again only after the pixels it was blurred *from* have changed. Which
//! they have is found by looking -- [`Backdrop::take`] compares what it is
//! given with what it holds -- because a frame's damage says what was drawn
//! again, not what came out different, and a pointer crossing a window
//! damages the wallpaper under it without changing one pixel of it.
//!
//! The blur of a tile is the same pixels a blur of the whole window would
//! have put there: [`Canvas::blur_from`] snaps what it reads to the lattice
//! the pyramid halves on, and a tile's edges are on it.

use crate::blur::Blur;
use crate::canvas::{Canvas, Rounding};
use crate::damage::{bounding, intersect};
use crate::{Damage, Error, Rect};

/// A tile's side in pixels.
///
/// A multiple of every lattice [`Canvas::blur_from`] snaps to, the coarsest
/// of which is `2^6`; small enough that a bar's blur is a row of them and
/// large enough that a screen's worth of flags is a few hundred.
pub(crate) const TILE: i64 = 64;

/// What is behind the windows of one screen, and the blur of it.
#[derive(Debug, Clone)]
pub struct Backdrop {
    /// The background and the layer surfaces under the windows, as the last
    /// frames drew them.
    sharp: Canvas,
    /// The blur of `sharp`, wherever `fresh` says so.
    blurred: Canvas,
    /// One flag a tile, a row of tiles after another: whether `blurred`
    /// holds the blur of what `sharp` holds now.
    fresh: Vec<bool>,
    /// What `blurred` was blurred with, or `None` before anything was.
    with: Option<Blur>,
    /// How many times anything has been blurred.
    blurs: u64,
}

/// `value`, known to be inside a canvas, as an index.
fn index(value: i64) -> usize {
    usize::try_from(value).unwrap_or(0)
}

/// Whether two runs of pixels hold the same bytes.
///
/// A word at a time rather than through `memcmp`, which musl -- the C
/// library every Ferrix program links -- implements a byte at a time: on a
/// Cortex-A7 that comparison was most of what keeping the backdrop cost.
fn same(one: &[u8], other: &[u8]) -> bool {
    const WORD: usize = 8;
    if one.len() != other.len() {
        return false;
    }
    let ((ones, one_tail), (others, other_tail)) =
        (one.as_chunks::<WORD>(), other.as_chunks::<WORD>());
    one_tail == other_tail
        && ones
            .iter()
            .zip(others)
            .all(|(a, b)| u64::from_ne_bytes(*a) == u64::from_ne_bytes(*b))
}

/// Where the tile at `at` along one direction of the grid begins, in pixels.
fn place(at: usize) -> i64 {
    i64::try_from(at).unwrap_or(0).saturating_mul(TILE)
}

/// What blurring `region` costs: the pixels read, which is the region and
/// a kernel's `reach` on every side of it.
fn cost(region: Rect, reach: i64) -> i64 {
    let around = reach.saturating_mul(2);
    region
        .width
        .saturating_add(around)
        .saturating_mul(region.height.saturating_add(around))
}

/// The smallest rectangle holding both.
fn both(one: Rect, other: Rect) -> Rect {
    bounding(&[one, other]).unwrap_or(one)
}

/// The first two of `regions` that are cheaper blurred as one than apart.
fn cheaper_together(regions: &[Rect], reach: i64) -> Option<(usize, usize)> {
    regions.iter().enumerate().find_map(|(at, &one)| {
        let other = regions
            .iter()
            .skip(at.saturating_add(1))
            .position(|&other| {
                cost(both(one, other), reach) <= cost(one, reach).saturating_add(cost(other, reach))
            })?;
        Some((at, at.saturating_add(1).saturating_add(other)))
    })
}

/// `tiles` gathered into the regions it is cheapest to blur them as.
///
/// A blur reads a kernel's reach around what it writes, so a tile blurred
/// alone costs twenty-five times its own pixels at the usual size and
/// passes, and two tiles side by side cost barely more than one: tiles near
/// each other belong in one blur. But not every tile in one. A wallpaper
/// that moves, moves here and there -- an eye, a strand of hair, a
/// particle -- and one box round all of it is the whole window, blurred
/// every frame for a few tiles' worth of change. So two regions are joined
/// whenever reading their one box costs no more than reading each, until no
/// two are; whatever order they are joined in, each blur writes the pixels
/// a blur of the whole window would.
pub(crate) fn gathered(tiles: Vec<Rect>, reach: i64) -> Vec<Rect> {
    let mut regions = tiles;
    regions.sort_unstable_by_key(|tile| (tile.y, tile.x));
    regions.dedup();
    while let Some((one, other)) = cheaper_together(&regions, reach) {
        let joined = regions.swap_remove(other);
        if let Some(region) = regions.get_mut(one) {
            *region = both(*region, joined);
        }
    }
    regions
}

impl Backdrop {
    /// A backdrop for a `width` × `height` canvas, with nothing blurred yet.
    ///
    /// # Errors
    ///
    /// [`Error::Size`] for a size of zero or over [`crate::MAX_SIZE`].
    pub fn new(width: u32, height: u32) -> Result<Self, Error> {
        let sharp = Canvas::new(width, height)?;
        let blurred = sharp.clone();
        let (columns, rows) = Self::tiles_of(&sharp);
        Ok(Self {
            sharp,
            blurred,
            fresh: vec![false; columns.saturating_mul(rows)],
            with: None,
            blurs: 0,
        })
    }

    /// How many times a blur has been run since this was made.
    ///
    /// The number that says the backdrop is doing what it is for: it goes
    /// up when what is behind the windows changes under one of them, and
    /// does not when a pointer crosses a window or a client draws in one.
    #[must_use]
    pub const fn blurs(&self) -> u64 {
        self.blurs
    }

    /// Whether this is `canvas`'s backdrop: one of its size, which is the
    /// one [`Backdrop::blur_onto`] copies out of rather than blurring the
    /// frame as it stands.
    pub(crate) fn fits(&self, canvas: &Canvas) -> bool {
        (canvas.width(), canvas.height()) == (self.sharp.width(), self.sharp.height())
    }

    /// How many tiles across and down a canvas is.
    fn tiles_of(canvas: &Canvas) -> (usize, usize) {
        let across = |pixels: u32| index((i64::from(pixels) + TILE - 1) / TILE);
        (across(canvas.width()), across(canvas.height()))
    }

    /// The tiles `rect` touches, as the first and one past the last in each
    /// direction, held inside the grid.
    fn tiles_in(&self, rect: Rect) -> ((usize, usize), (usize, usize)) {
        let (columns, rows) = Self::tiles_of(&self.sharp);
        let first = |value: i64| index(value.max(0) / TILE);
        let past = |value: i64, most: usize| index((value.max(0) + TILE - 1) / TILE).min(most);
        (
            (first(rect.x), past(rect.right(), columns)),
            (first(rect.y), past(rect.bottom(), rows)),
        )
    }

    /// Bring the backdrop up to date: `canvas` holds everything behind the
    /// windows within `damage`, and nothing over it yet.
    ///
    /// Only what came out *different* counts as a change. A tile whose
    /// pixels moved is no longer the blur of anything, and neither is any
    /// tile within a kernel's reach of it.
    pub(crate) fn take(&mut self, canvas: &Canvas, damage: &Damage) {
        if (canvas.width(), canvas.height()) != (self.sharp.width(), self.sharp.height()) {
            return;
        }
        let (columns, _) = Self::tiles_of(&self.sharp);
        let mut moved = vec![false; self.fresh.len()];
        for clip in damage.clipped(canvas.bounds()).rects() {
            for y in clip.y..clip.bottom() {
                self.take_row(canvas, (clip.x, clip.right()), y, &mut moved);
            }
        }
        // Before anything was blurred nothing is fresh, and there is no
        // kernel to measure a reach with.
        let Some(reach) = self.with.map(|blur| blur.reach()) else {
            return;
        };
        for at in (0..moved.len()).filter(|at| moved.get(*at) == Some(&true)) {
            let around = Rect::new(
                place(at % columns).saturating_sub(reach),
                place(at / columns).saturating_sub(reach),
                TILE.saturating_add(reach.saturating_mul(2)),
                TILE.saturating_add(reach.saturating_mul(2)),
            );
            let ((from_x, to_x), (from_y, to_y)) = self.tiles_in(around);
            for y in from_y..to_y {
                if let Some(flags) = self.fresh.get_mut(y * columns + from_x..y * columns + to_x) {
                    flags.fill(false);
                }
            }
        }
    }

    /// One row of [`Backdrop::take`], from `span.0` to `span.1`: copy what
    /// differs and flag the tiles it differs in.
    ///
    /// A tile's worth of the row at a time, so that a change is known to
    /// the tile, and a long row of a wallpaper that did not change costs a
    /// comparison and no more.
    fn take_row(&mut self, canvas: &Canvas, span: (i64, i64), y: i64, moved: &mut [bool]) {
        let width = index(i64::from(canvas.width()));
        let (columns, _) = Self::tiles_of(&self.sharp);
        let mut x = span.0;
        while x < span.1 {
            let end = (x / TILE + 1).saturating_mul(TILE).min(span.1);
            let from = (index(y) * width + index(x)) * 4;
            let to = (index(y) * width + index(end)) * 4;
            let tile = index(y / TILE) * columns + index(x / TILE);
            x = end;
            let (Some(source), Some(target)) = (
                canvas.data().get(from..to),
                self.sharp.data_mut().get_mut(from..to),
            ) else {
                continue;
            };
            if same(source, target) {
                continue;
            }
            target.copy_from_slice(source);
            if let Some(flag) = moved.get_mut(tile) {
                *flag = true;
            }
        }
    }

    /// The tiles `rect` touches that are not the blur of what is behind
    /// them now, each as its own rectangle.
    fn stale_in(&self, rect: Rect) -> impl Iterator<Item = Rect> + '_ {
        let (columns, _) = Self::tiles_of(&self.sharp);
        let ((from_x, to_x), (from_y, to_y)) = self.tiles_in(rect);
        (from_y..to_y)
            .flat_map(move |y| (from_x..to_x).map(move |x| (x, y)))
            .filter(move |(x, y)| self.fresh.get(y * columns + x) == Some(&false))
            .map(|(x, y)| Rect::new(place(x), place(y), TILE, TILE))
    }

    /// Draw the blur of the backdrop into `rect` of `canvas`, with its
    /// corners cut to `rounding` and within `damage`: what
    /// [`Canvas::blur`] draws for a window with nothing but the backdrop
    /// behind it, without blurring anything that has been blurred before.
    pub(crate) fn blur_onto(
        &mut self,
        canvas: &mut Canvas,
        rect: Rect,
        rounding: Rounding,
        blur: &Blur,
        damage: &Damage,
    ) {
        // A backdrop of another size is not this screen's, and the blur of
        // the frame so far is what a window gets without one.
        if (canvas.width(), canvas.height()) != (self.sharp.width(), self.sharp.height()) {
            canvas.blur(rect, rounding, blur, damage);
            return;
        }
        if blur.size <= 0 || blur.passes == 0 {
            return;
        }
        let clips = canvas.rounded_clips(rect, rounding, damage);
        if clips.is_empty() {
            return;
        }
        self.freshen(&clips, blur);
        canvas.copy_from(&self.blurred, &clips);
    }

    /// Blur every tile under `clips` that is not the blur of what is behind
    /// it now.
    fn freshen(&mut self, clips: &[Rect], blur: &Blur) {
        if self.with != Some(*blur) {
            self.fresh.fill(false);
            self.with = Some(*blur);
        }
        let (columns, _) = Self::tiles_of(&self.sharp);
        let stale: Vec<Rect> = clips.iter().flat_map(|&clip| self.stale_in(clip)).collect();
        for region in gathered(stale, blur.reach()) {
            let Some(region) = intersect(region, self.sharp.bounds()) else {
                continue;
            };
            self.blurred.blur_from(
                Some(&self.sharp),
                region,
                Rounding::none(),
                blur,
                &Damage::from(region),
            );
            self.blurs = self.blurs.saturating_add(1);
            let ((from_x, to_x), (from_y, to_y)) = self.tiles_in(region);
            for y in from_y..to_y {
                if let Some(flags) = self.fresh.get_mut(y * columns + from_x..y * columns + to_x) {
                    flags.fill(true);
                }
            }
        }
        // What a canvas records of what was drawn on it is for a frame to
        // present, and this one is never presented.
        let _ = self.blurred.take_damage();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile, by its place in the grid.
    fn tile(x: i64, y: i64) -> Rect {
        Rect::new(x * TILE, y * TILE, TILE, TILE)
    }

    /// Tiles side by side are one blur, tiles a screen apart are two, and a
    /// screen full of them is one again: each blur reads a reach around what
    /// it writes, and that is what decides.
    #[test]
    fn tiles_are_gathered_into_the_blurs_that_read_least() {
        let reach = Blur::new(8, 3).reach();

        let beside = gathered(vec![tile(3, 3), tile(4, 3), tile(3, 4), tile(3, 3)], reach);
        assert_eq!(beside, [Rect::new(3 * TILE, 3 * TILE, 2 * TILE, 2 * TILE)]);

        let mut apart = gathered(vec![tile(0, 0), tile(28, 15)], reach);
        apart.sort_unstable_by_key(|region| region.x);
        assert_eq!(apart, [tile(0, 0), tile(28, 15)]);

        let everywhere: Vec<Rect> = (0..17)
            .flat_map(|y| (0..30).map(move |x| tile(x, y)))
            .collect();
        assert_eq!(
            gathered(everywhere, reach),
            [Rect::new(0, 0, 30 * TILE, 17 * TILE)]
        );
    }
}
