//! Positioners and anchors, as bindings (`docs/UI-FRAMEWORK.md` §4.3).
//!
//! A `Row` binds each child's `x` to the one before it -- its `x`, plus
//! its width and the spacing when it is visible -- and its own implicit
//! size to its children's; a `Column` does the same down. Anchors bind an
//! item's `x`, `y`, `width` and `height` to lines of its parent or a
//! sibling. Neither is a pass of its own: a change anywhere re-evaluates
//! just the places it reaches.

use crate::item::{Common, ItemId, Kind};
use crate::ui::{Prop, Ui};

/// Which way a positioner runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    /// Across: a `Row`.
    Across,
    /// Down: a `Column`.
    Down,
}

/// The properties along and across an axis.
fn along(common: Common, axis: Axis) -> (Prop<f32>, Prop<f32>, Prop<f32>) {
    match axis {
        Axis::Across => (common.x, common.width, common.height),
        Axis::Down => (common.y, common.height, common.width),
    }
}

impl Ui {
    /// Bind the places of `parent`'s children when it is a positioner.
    pub(crate) fn position_children(&mut self, parent: ItemId) {
        let (props, axis) = match self.kind(parent) {
            Some(Kind::Row(props)) => (props, Axis::Across),
            Some(Kind::Column(props)) => (props, Axis::Down),
            _ => return,
        };
        let Some(own) = self.common(parent) else {
            return;
        };
        let children: Vec<Common> = self
            .children(parent)
            .iter()
            .filter_map(|&child| self.common(child))
            .collect();
        let mut before: Option<Common> = None;
        for &child in &children {
            let (place, _, _) = along(child, axis);
            match before {
                None => place.bind(self, move |ui| ui.get(props.padding)),
                Some(prev) => place.bind(self, move |ui| {
                    let (prev_place, prev_size, _) = along(prev, axis);
                    let at = ui.get(prev_place);
                    if ui.get(prev.visible) {
                        at + ui.get(prev_size) + ui.get(props.spacing)
                    } else {
                        at
                    }
                }),
            }
            before = Some(child);
        }
        let (_, own_size, own_cross) = match axis {
            Axis::Across => (own.x, own.implicit_width, own.implicit_height),
            Axis::Down => (own.y, own.implicit_height, own.implicit_width),
        };
        let lengthwise = children.clone();
        own_size.bind(self, move |ui| {
            let mut total = 0.0;
            let mut shown = 0_u32;
            for &child in &lengthwise {
                if ui.get(child.visible) {
                    let (_, size, _) = along(child, axis);
                    total += ui.get(size);
                    shown += 1;
                }
            }
            let gaps = shown.saturating_sub(1) as f32 * ui.get(props.spacing);
            total + gaps + 2.0 * ui.get(props.padding)
        });
        own_cross.bind(self, move |ui| {
            let mut most = 0.0_f32;
            for &child in &children {
                if ui.get(child.visible) {
                    let (_, _, cross) = along(child, axis);
                    most = most.max(ui.get(cross));
                }
            }
            most + 2.0 * ui.get(props.padding)
        });
    }
}

/// A line of an item an anchor can attach to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    /// The left edge.
    Left,
    /// The right edge.
    Right,
    /// The middle across.
    HorizontalCenter,
    /// The top edge.
    Top,
    /// The bottom edge.
    Bottom,
    /// The middle down.
    VerticalCenter,
}

/// An item's anchors, as `anchors { ... }` writes them. `fill` and
/// `center_in` stand for the four edges and the two middles.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Anchors {
    /// Every edge to this item's.
    pub fill: Option<ItemId>,
    /// Both middles to this item's.
    pub center_in: Option<ItemId>,
    /// The left edge to a line.
    pub left: Option<(ItemId, Line)>,
    /// The right edge to a line.
    pub right: Option<(ItemId, Line)>,
    /// The middle across to a line.
    pub horizontal_center: Option<(ItemId, Line)>,
    /// The top edge to a line.
    pub top: Option<(ItemId, Line)>,
    /// The bottom edge to a line.
    pub bottom: Option<(ItemId, Line)>,
    /// The middle down to a line.
    pub vertical_center: Option<(ItemId, Line)>,
    /// Every margin not given on its own.
    pub margins: f32,
    /// The left margin.
    pub left_margin: Option<f32>,
    /// The right margin.
    pub right_margin: Option<f32>,
    /// The top margin.
    pub top_margin: Option<f32>,
    /// The bottom margin.
    pub bottom_margin: Option<f32>,
}

/// A line of a target, as an anchor reads it from the anchored item's
/// parent: a parent's lines are from its own origin, a sibling's from its
/// place.
#[derive(Clone, Copy, Debug)]
struct Edge {
    /// The target's properties.
    target: Common,
    /// Whether the target is the anchored item's parent.
    is_parent: bool,
    /// Which line.
    line: Line,
}

impl Edge {
    /// Where the line is.
    fn at(self, ui: &mut Ui) -> f32 {
        let t = self.target;
        let (start, size) = match self.line {
            Line::Left | Line::Right | Line::HorizontalCenter => (t.x, t.width),
            Line::Top | Line::Bottom | Line::VerticalCenter => (t.y, t.height),
        };
        let origin = if self.is_parent { 0.0 } else { ui.get(start) };
        match self.line {
            Line::Left | Line::Top => origin,
            Line::Right | Line::Bottom => origin + ui.get(size),
            Line::HorizontalCenter | Line::VerticalCenter => origin + ui.get(size) / 2.0,
        }
    }
}

impl Ui {
    /// Anchor `item` as `anchors` says, binding its place and size.
    /// Answers `false`, binding nothing, when `item` or a target is gone
    /// or a target is neither its parent nor a sibling.
    pub fn anchor(&mut self, item: ItemId, anchors: &Anchors) -> bool {
        let Some(own) = self.common(item) else {
            return false;
        };
        let parent = self.parent(item);
        let mut a = *anchors;
        if let Some(target) = a.fill {
            a.left = Some((target, Line::Left));
            a.right = Some((target, Line::Right));
            a.top = Some((target, Line::Top));
            a.bottom = Some((target, Line::Bottom));
        }
        if let Some(target) = a.center_in {
            a.horizontal_center = Some((target, Line::HorizontalCenter));
            a.vertical_center = Some((target, Line::VerticalCenter));
        }
        let edge = |target: Option<(ItemId, Line)>| -> Result<Option<Edge>, ()> {
            let Some((target, line)) = target else {
                return Ok(None);
            };
            let is_parent = Some(target) == parent;
            let sibling = !is_parent && self.parent(target) == parent && target != item;
            let common = self.common(target).ok_or(())?;
            if is_parent || sibling {
                Ok(Some(Edge {
                    target: common,
                    is_parent,
                    line,
                }))
            } else {
                Err(())
            }
        };
        let (Ok(left), Ok(right), Ok(hcenter), Ok(top), Ok(bottom), Ok(vcenter)) = (
            edge(a.left),
            edge(a.right),
            edge(a.horizontal_center),
            edge(a.top),
            edge(a.bottom),
            edge(a.vertical_center),
        ) else {
            return false;
        };
        let margin = |own: Option<f32>| own.unwrap_or(a.margins);
        self.anchor_axis(
            (own.x, own.width),
            (left, right, hcenter),
            (margin(a.left_margin), margin(a.right_margin)),
        );
        self.anchor_axis(
            (own.y, own.height),
            (top, bottom, vcenter),
            (margin(a.top_margin), margin(a.bottom_margin)),
        );
        true
    }

    /// One axis of [`Ui::anchor`]: the start and size given the start,
    /// end and middle lines and the two margins.
    fn anchor_axis(
        &mut self,
        (place, size): (Prop<f32>, Prop<f32>),
        (start, end, middle): (Option<Edge>, Option<Edge>, Option<Edge>),
        (start_margin, end_margin): (f32, f32),
    ) {
        match (start, end, middle) {
            (Some(start), Some(end), _) => {
                place.bind(self, move |ui| start.at(ui) + start_margin);
                size.bind(self, move |ui| {
                    (end.at(ui) - end_margin - start.at(ui) - start_margin).max(0.0)
                });
            }
            (Some(start), None, _) => place.bind(self, move |ui| start.at(ui) + start_margin),
            (None, Some(end), _) => {
                place.bind(self, move |ui| end.at(ui) - end_margin - ui.get(size));
            }
            (None, None, Some(middle)) => {
                place.bind(self, move |ui| middle.at(ui) - ui.get(size) / 2.0);
            }
            (None, None, None) => {}
        }
    }
}
