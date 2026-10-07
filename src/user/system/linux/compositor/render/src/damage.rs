//! Damage: the part of an output a frame redraws, as disjoint rectangles.

use crate::Rect;

/// How many rectangles a [`Damage`] holds before it becomes their bounding
/// box. Past this, working out the exact region costs more than drawing the
/// few pixels the box adds, and drawing them is harmless: a frame redraws
/// whatever it is told to from the same state.
pub(crate) const MAX_RECTS: usize = 32;

/// A region of an output, as disjoint rectangles that are not empty.
///
/// Rectangles added to it are cut against the ones it holds, so no pixel is
/// in two of them and a translucent draw clipped to each rectangle in turn
/// blends every pixel once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    rects: Vec<Rect>,
}

impl Damage {
    /// No damage.
    #[must_use]
    pub const fn new() -> Self {
        Self { rects: Vec::new() }
    }

    /// The whole of a `width` × `height` output.
    #[must_use]
    pub fn full(width: u32, height: u32) -> Self {
        Self::from(Rect::new(0, 0, i64::from(width), i64::from(height)))
    }

    /// Add `rect` to the region: the parts of it not already covered.
    pub fn add(&mut self, rect: Rect) {
        if is_empty(rect) {
            return;
        }
        let mut pieces = vec![rect];
        for &held in &self.rects {
            pieces = pieces
                .into_iter()
                .flat_map(|piece| subtract(piece, held))
                .collect();
            if pieces.is_empty() {
                return;
            }
        }
        self.rects.extend(pieces);
        if self.rects.len() > MAX_RECTS
            && let Some(bounds) = self.bounds()
        {
            self.rects = vec![bounds];
        }
    }

    /// Add every rectangle of `other`.
    pub fn extend(&mut self, other: &Self) {
        for &rect in &other.rects {
            self.add(rect);
        }
    }

    /// The rectangles, disjoint and not empty, in no particular order.
    #[must_use]
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    /// Whether the region is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// How many pixels the region covers.
    #[must_use]
    pub fn area(&self) -> i64 {
        self.rects
            .iter()
            .map(|rect| rect.width.saturating_mul(rect.height))
            .fold(0, i64::saturating_add)
    }

    /// Whether pixel (`x`, `y`) is in the region.
    #[must_use]
    pub fn contains(&self, x: i64, y: i64) -> bool {
        self.rects
            .iter()
            .any(|rect| (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y))
    }

    /// The smallest rectangle holding the region, if it is not empty.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        let first = *self.rects.first()?;
        let (left, top, right, bottom) = self.rects.iter().fold(
            (first.x, first.y, first.right(), first.bottom()),
            |(left, top, right, bottom), rect| {
                (
                    left.min(rect.x),
                    top.min(rect.y),
                    right.max(rect.right()),
                    bottom.max(rect.bottom()),
                )
            },
        );
        Some(Rect::new(
            left,
            top,
            right.saturating_sub(left),
            bottom.saturating_sub(top),
        ))
    }

    /// The part of the region inside `bounds`.
    #[must_use]
    pub fn clipped(&self, bounds: Rect) -> Self {
        Self {
            rects: self
                .rects
                .iter()
                .filter_map(|&rect| intersect(rect, bounds))
                .collect(),
        }
    }

    /// The region less `hole`.
    #[must_use]
    pub fn without(&self, hole: Rect) -> Self {
        Self {
            rects: self
                .rects
                .iter()
                .flat_map(|&rect| subtract(rect, hole))
                .collect(),
        }
    }

    /// Empty the region.
    pub fn clear(&mut self) {
        self.rects.clear();
    }
}

impl From<Rect> for Damage {
    fn from(rect: Rect) -> Self {
        let mut damage = Self::new();
        damage.add(rect);
        damage
    }
}

impl FromIterator<Rect> for Damage {
    fn from_iter<I: IntoIterator<Item = Rect>>(rects: I) -> Self {
        let mut damage = Self::new();
        for rect in rects {
            damage.add(rect);
        }
        damage
    }
}

/// Whether `rect` covers no pixel.
pub(crate) const fn is_empty(rect: Rect) -> bool {
    rect.width <= 0 || rect.height <= 0
}

/// The pixels `a` and `b` share, if any.
pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    (left < right && top < bottom).then(|| {
        Rect::new(
            left,
            top,
            right.saturating_sub(left),
            bottom.saturating_sub(top),
        )
    })
}

/// The smallest rectangle holding every one of `rects`, or `None` for none.
///
/// What a run of clips comes to: the blur reads a region around what it
/// writes, and what it writes is these.
pub(crate) fn bounding(rects: &[Rect]) -> Option<Rect> {
    let mut held: Option<Rect> = None;
    for &rect in rects {
        if is_empty(rect) {
            continue;
        }
        held = Some(match held {
            None => rect,
            Some(so_far) => {
                let (left, top) = (so_far.x.min(rect.x), so_far.y.min(rect.y));
                Rect::new(
                    left,
                    top,
                    so_far.right().max(rect.right()).saturating_sub(left),
                    so_far.bottom().max(rect.bottom()).saturating_sub(top),
                )
            }
        });
    }
    held
}

/// `a` less `b`, as up to four disjoint rectangles: the rows above and below
/// `b`, then the columns left and right of it within `b`'s rows.
fn subtract(a: Rect, b: Rect) -> Vec<Rect> {
    let Some(shared) = intersect(a, b) else {
        return vec![a];
    };
    [
        Rect::new(a.x, a.y, a.width, shared.y.saturating_sub(a.y)),
        Rect::new(
            a.x,
            shared.bottom(),
            a.width,
            a.bottom().saturating_sub(shared.bottom()),
        ),
        Rect::new(a.x, shared.y, shared.x.saturating_sub(a.x), shared.height),
        Rect::new(
            shared.right(),
            shared.y,
            a.right().saturating_sub(shared.right()),
            shared.height,
        ),
    ]
    .into_iter()
    .filter(|&piece| !is_empty(piece))
    .collect()
}
