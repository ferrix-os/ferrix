//! The scene: the tree turned into drawing operations, and the trait a
//! renderer plays them through (`docs/UI-FRAMEWORK.md` §4.6).
//!
//! A frame is made in two halves. [`Ui::scene`] walks the tree with the
//! properties settled, shapes the text and answers a list of [`DrawOp`]s
//! in logical pixels; [`play`] hands them to a [`Renderer`]. The CPU
//! renderer ([`crate::cpu`]) is tiny-skia; a GPU one later implements the
//! same trait.

use std::rc::Rc;

use compositor_text::Run;

use crate::item::{ItemId, Kind};
use crate::ui::Ui;
use crate::value::{Color, Elide, HAlign, VAlign};

/// A rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// A rectangle.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }

    /// The right edge.
    #[must_use]
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    /// The bottom edge.
    #[must_use]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Whether the point is inside.
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// The part both share; empty ones have no width or height.
    #[must_use]
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Rect::new(
            x,
            y,
            (self.right().min(other.right()) - x).max(0.0),
            (self.bottom().min(other.bottom()) - y).max(0.0),
        )
    }
}

/// One drawing operation.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawOp {
    /// A rectangle filled, its corners rounded.
    Fill {
        /// Where.
        rect: Rect,
        /// The corners' radius.
        radius: f32,
        /// The colour.
        color: Color,
    },
    /// A rectangle's outline, `width` wide inside its edge.
    Stroke {
        /// Where.
        rect: Rect,
        /// The corners' radius.
        radius: f32,
        /// The line's width.
        width: f32,
        /// The colour.
        color: Color,
    },
    /// A shaped run, its pen at (`x`, `baseline`) in buffer pixels, as
    /// the run is.
    Text {
        /// The glyphs.
        run: Rc<Run>,
        /// The pen's start.
        x: f32,
        /// The baseline.
        baseline: f32,
        /// The colour.
        color: Color,
    },
    /// Cut what follows to a rectangle, inside any cut already made.
    PushClip(Rect),
    /// End the last cut.
    PopClip,
    /// Draw what follows together, then at this opacity.
    PushOpacity(f32),
    /// End the last group.
    PopOpacity,
}

/// What a scene is drawn through.
pub trait Renderer {
    /// A rectangle filled, its corners rounded by `radius`.
    fn fill(&mut self, rect: Rect, radius: f32, color: Color);
    /// A rectangle's outline, `width` wide inside its edge.
    fn stroke(&mut self, rect: Rect, radius: f32, width: f32, color: Color);
    /// A run, its pen at (`x`, `baseline`) in buffer pixels.
    fn text(&mut self, run: &Run, x: f32, baseline: f32, color: Color);
    /// Cut what follows to `rect`.
    fn push_clip(&mut self, rect: Rect);
    /// End the last cut.
    fn pop_clip(&mut self);
    /// Draw what follows together, then at `opacity`.
    fn push_opacity(&mut self, opacity: f32);
    /// End the last group.
    fn pop_opacity(&mut self);
}

/// Hand `ops` to `renderer`.
pub fn play(ops: &[DrawOp], renderer: &mut dyn Renderer) {
    for op in ops {
        match op {
            DrawOp::Fill {
                rect,
                radius,
                color,
            } => renderer.fill(*rect, *radius, *color),
            DrawOp::Stroke {
                rect,
                radius,
                width,
                color,
            } => renderer.stroke(*rect, *radius, *width, *color),
            DrawOp::Text {
                run,
                x,
                baseline,
                color,
            } => renderer.text(run, *x, *baseline, *color),
            DrawOp::PushClip(rect) => renderer.push_clip(*rect),
            DrawOp::PopClip => renderer.pop_clip(),
            DrawOp::PushOpacity(opacity) => renderer.push_opacity(*opacity),
            DrawOp::PopOpacity => renderer.pop_opacity(),
        }
    }
}

impl Ui {
    /// The whole tree as drawing operations, roots in the order made.
    pub fn scene(&mut self) -> Vec<DrawOp> {
        let mut ops = Vec::new();
        for root in self.roots.clone() {
            self.scene_of(root, (0.0, 0.0), &mut ops);
        }
        ops
    }

    /// `item` and its children, its parent's origin at `origin`.
    fn scene_of(&mut self, item: ItemId, origin: (f32, f32), ops: &mut Vec<DrawOp>) {
        let Some(common) = self.common(item) else {
            return;
        };
        let opacity = self.get(common.opacity);
        if !self.get(common.visible) || opacity <= 0.0 {
            return;
        }
        let place = Rect::new(
            origin.0 + self.get(common.x),
            origin.1 + self.get(common.y),
            self.get(common.width),
            self.get(common.height),
        );
        let grouped = opacity < 1.0;
        if grouped {
            ops.push(DrawOp::PushOpacity(opacity));
        }
        match self.kind(item) {
            Some(Kind::Rectangle(props)) => {
                let radius = self.get(props.radius);
                let color = self.get(props.color);
                if !color.is_transparent() {
                    ops.push(DrawOp::Fill {
                        rect: place,
                        radius,
                        color,
                    });
                }
                let width = self.get(props.border_width);
                let border = self.get(props.border_color);
                if width > 0.0 && !border.is_transparent() {
                    ops.push(DrawOp::Stroke {
                        rect: place,
                        radius,
                        width,
                        color: border,
                    });
                }
            }
            Some(Kind::Text(props)) => self.text_op(props, place, ops),
            _ => {}
        }
        let clipped = self.get(common.clip);
        if clipped {
            ops.push(DrawOp::PushClip(place));
        }
        for child in self.children(item).to_vec() {
            self.scene_of(child, (place.x, place.y), ops);
        }
        if clipped {
            ops.push(DrawOp::PopClip);
        }
        if grouped {
            ops.push(DrawOp::PopOpacity);
        }
    }

    /// A `Text`'s run, placed in `place` as the lode viewer places its
    /// labels: the pen and the baseline on whole buffer pixels.
    fn text_op(&mut self, props: crate::item::TextProps, place: Rect, ops: &mut Vec<DrawOp>) {
        let text = self.get(props.text);
        let color = self.get(props.color);
        if text.is_empty() || color.is_transparent() {
            return;
        }
        let font = self.get(props.font);
        let scale = self.scale();
        let run = match self.get(props.elide) {
            Elide::Right if place.w > 0.0 => {
                self.text.run_within(&font, scale, &text, place.w * scale)
            }
            _ => self.text.run(&font, scale, &text),
        };
        let width = run.width / scale;
        let x = match self.get(props.horizontal_alignment) {
            HAlign::Left => place.x,
            HAlign::Center => place.x + (place.w - width) / 2.0,
            HAlign::Right => place.right() - width,
        };
        let (top, height) = (place.y * scale, place.h * scale);
        let baseline = match self.get(props.vertical_alignment) {
            VAlign::Top => top + run.ascent,
            VAlign::Center => top + (height - (run.ascent + run.descent)) / 2.0 + run.ascent,
            VAlign::Bottom => top + height - run.descent,
        };
        ops.push(DrawOp::Text {
            run,
            x: (x * scale).round(),
            baseline: baseline.round(),
            color,
        });
    }
}
