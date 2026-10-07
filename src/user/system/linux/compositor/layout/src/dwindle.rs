//! The dwindle layout: Hyprland's `DwindleLayout.cpp`, which Hyprland 0.54
//! moved to `CDwindleAlgorithm` in `layout/algorithm/tiled/dwindle`.
//!
//! A workspace's tiled windows are the leaves of a binary tree. The root's
//! box is the workspace's work area, the monitor less its reserved strips
//! and `general:gaps_out`; each split divides its box in two, side by side
//! or stacked, the first child taking `ratio` times half. A new window
//! splits the leaf of the window it opens beside, and a closed window's
//! sibling takes its parent's place, keeping whatever subtree it has.
//!
//! A split's direction is chosen from its box when it is made: side by side
//! when the box is wider than its height times
//! `dwindle:split_width_multiplier`, stacked otherwise. Unless
//! `dwindle:preserve_split` is set, it is chosen again from the box whenever
//! the tree is laid out, as Hyprland's `recalcSizePosRecursive` does, which
//! uses the opposite comparison (stacked when the height times the
//! multiplier exceeds the width), so a square box ends up side by side.
//!
//! `movewindow` does what Hyprland's `moveTargetInDirection` does: it takes
//! the window out and puts it back beside the window whose box is nearest a
//! point one pixel past its edge in the direction of the move, on the half
//! of that box the point is in, which may be on another monitor's
//! workspace. When the window's sibling is a single window in that
//! direction, it lands on that sibling's far side. Either way the new split
//! gets `dwindle:default_split_ratio`.
//!
//! Three options are about where the pointer is when a window opens, and
//! the tree is told (`State::set_pointer`) rather than asking a device:
//! `use_active_for_splits` splits the focused window's box or, with it off,
//! the box the pointer is over; `force_split = 0` puts the new window on
//! the half of that box the pointer is in; and `smart_split` on the quarter
//! it is in, which picks the split's direction as well as its side. A
//! compositor that has seen no pointer gets the behaviour these had before
//! there was one -- the focused window's box, and the second half.
//!
//! A tiled window dragged with the mouse is dropped back in beside the box
//! under the pointer ([`Dwindle::insert_dropped`]), on the half of it the
//! pointer is in whatever `force_split` says, or with
//! `dwindle:precise_mouse_move` on the quarter, as `smart_split` places a
//! new window.
//!
//! Where it departs from Hyprland: `split_bias`
//! and pseudotiling are not implemented. `dwindle:smart_resizing` is, and is the only behaviour: it
//! is Hyprland's default and the setting is not read, so turning it off
//! changes nothing.

use crate::dispatch::Direction;
use crate::geometry::Area;
use crate::geometry::sticks_fine;
use crate::settings::{ForceSplit, Settings};
use crate::{Corner, WindowId};

/// Which side of a new split a new window takes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Place {
    /// As `dwindle:force_split` says.
    Configured,
    /// The half of the box this point is in.
    Point(f64, f64),
    /// `dwindle:smart_split`: the *quarter* of the box this point is in,
    /// which decides the split's direction as well as its order -- a point
    /// left of centre and near the middle height splits side by side with
    /// the new window on the left, one near the top splits stacked with the
    /// new window on top.
    Quadrant(f64, f64),
    /// First for a move left or up, second for right or down, and the split
    /// along that direction's axis.
    Toward(Direction),
}

/// A node of the tree.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// A window.
    Leaf(WindowId),
    /// A box divided in two.
    Split(Box<Split>),
}

/// A divided box.
#[derive(Debug, Clone, PartialEq)]
struct Split {
    /// Whether the children are one above the other, rather than side by
    /// side: Hyprland's `splitTop`.
    stacked: bool,
    /// The first child's share, as a multiple of half the box.
    ratio: f64,
    /// The left or top child.
    first: Node,
    /// The right or bottom child.
    second: Node,
}

impl Split {
    /// The direction this split lays out with in `area`.
    ///
    /// Worked out afresh from the box, unless something chose it on purpose
    /// and would have its choice thrown away: `dwindle:preserve_split`, and
    /// `dwindle:smart_split`, whose whole point is that the pointer picked
    /// the direction. Hyprland's `recalcSizePosRecursive` asks the same
    /// question -- it recomputes `splitTop` only when all three of
    /// `preserve_split`, `smart_split` and `precise_mouse_move` are off.
    fn stacked_in(&self, area: Area, settings: &Settings) -> bool {
        if settings.dwindle.preserve_split
            || settings.dwindle.smart_split
            || settings.dwindle.precise_mouse_move
        {
            self.stacked
        } else {
            area.h * settings.dwindle.split_width_multiplier > area.w
        }
    }

    /// The children's boxes within `area`.
    fn children(&self, area: Area, settings: &Settings) -> (Area, Area) {
        if self.stacked_in(area, settings) {
            let first = (area.h / 2.0 * self.ratio).clamp(0.0, area.h.max(0.0));
            (
                Area { h: first, ..area },
                Area {
                    y: area.y + first,
                    h: (area.h - first).max(0.0),
                    ..area
                },
            )
        } else {
            let first = (area.w / 2.0 * self.ratio).clamp(0.0, area.w.max(0.0));
            (
                Area { w: first, ..area },
                Area {
                    x: area.x + first,
                    w: (area.w - first).max(0.0),
                    ..area
                },
            )
        }
    }
}

/// One split on the way down to a window, as [`Node::ancestors_of`] writes
/// it out.
#[derive(Debug, Clone, PartialEq)]
struct Ancestor {
    /// The way to this split from the root, for [`Node::split_at`].
    path: Vec<bool>,
    /// The box it divides.
    area: Area,
    /// The box it gives the child the way down takes, which is Hyprland's
    /// `PHINNER->box` where the split itself is `PHINNER->pParent->box`.
    below: Area,
    /// Whether it lays that box out stacked.
    stacked: bool,
    /// Which of its two children holds the window: `false` for the left or
    /// top one.
    side: bool,
}

impl Node {
    /// Whether `window` is a leaf of this subtree.
    fn contains(&self, window: WindowId) -> bool {
        match self {
            Self::Leaf(id) => *id == window,
            Self::Split(split) => split.first.contains(window) || split.second.contains(window),
        }
    }

    /// The leaves, left to right and top to bottom.
    fn leaves(&self, out: &mut Vec<WindowId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split(split) => {
                split.first.leaves(out);
                split.second.leaves(out);
            }
        }
    }

    /// The way down to the split that holds `window` as one of its own two
    /// children: `false` for a first child and `true` for a second. An
    /// empty path is the root itself.
    ///
    /// A path rather than a borrow of the split, because a tree walked with
    /// `&mut` cannot hand back a reference into one branch and then look in
    /// the other; the path is taken once, and the walk down it is short.
    fn path_to_parent(&self, window: WindowId) -> Option<Vec<bool>> {
        let Self::Split(split) = self else {
            return None;
        };
        if split.first == Self::Leaf(window) || split.second == Self::Leaf(window) {
            return Some(Vec::new());
        }
        for (side, child) in [(false, &split.first), (true, &split.second)] {
            if child.contains(window)
                && let Some(mut rest) = child.path_to_parent(window)
            {
                rest.insert(0, side);
                return Some(rest);
            }
        }
        None
    }

    /// Every split from this node down to `window`'s leaf, root first, with
    /// the box each one divides and the child the way down takes, and the
    /// leaf's own box last.
    ///
    /// A resize moves a split, and which split it moves depends on the
    /// direction each one lays out with *and* on the boxes, which this tree
    /// works out on the way down rather than storing. So the way down is
    /// walked once and written out, and the walk back up is over this.
    ///
    /// `None` when the window is not a leaf of this subtree, and when it is
    /// the only one: a lone window has no split above it to move.
    fn ancestors_of(
        &self,
        window: WindowId,
        area: Area,
        settings: &Settings,
    ) -> Option<(Vec<Ancestor>, Area)> {
        let mut out: Vec<Ancestor> = Vec::new();
        let mut node = self;
        let mut area = area;
        loop {
            match node {
                Self::Leaf(id) if *id == window && !out.is_empty() => return Some((out, area)),
                Self::Leaf(_) => return None,
                Self::Split(split) => {
                    let side = if split.first.contains(window) {
                        false
                    } else if split.second.contains(window) {
                        true
                    } else {
                        return None;
                    };
                    let (first, second) = split.children(area, settings);
                    let path = out.iter().map(|above| above.side).collect();
                    let below = if side { second } else { first };
                    out.push(Ancestor {
                        path,
                        area,
                        below,
                        stacked: split.stacked_in(area, settings),
                        side,
                    });
                    node = if side { &split.second } else { &split.first };
                    area = below;
                }
            }
        }
    }

    /// The node a path leads to.
    fn at(&mut self, path: &[bool]) -> Option<&mut Self> {
        let Some((side, rest)) = path.split_first() else {
            return Some(self);
        };
        let Self::Split(split) = self else {
            return None;
        };
        if *side {
            split.second.at(rest)
        } else {
            split.first.at(rest)
        }
    }

    /// The split a path leads to.
    fn split_at(&mut self, path: &[bool]) -> Option<&mut Split> {
        match self.at(path) {
            Some(Self::Split(split)) => Some(split),
            _ => None,
        }
    }

    /// Exchange the two leaves holding `a` and `b`.
    ///
    /// The tree keeps its shape and the two windows change places in it,
    /// which is what Hyprland's dwindle `switchWindows` does: it exchanges
    /// the two nodes' windows rather than moving nodes around, so every
    /// split's ratio and direction survives the swap.
    ///
    /// Each window is a leaf exactly once, so rewriting a leaf as it is
    /// reached cannot make the walk swap the same pair twice.
    fn swap(&mut self, a: WindowId, b: WindowId) {
        match self {
            Self::Leaf(id) if *id == a => *id = b,
            Self::Leaf(id) if *id == b => *id = a,
            Self::Leaf(_) => {}
            Self::Split(split) => {
                split.first.swap(a, b);
                split.second.swap(a, b);
            }
        }
    }

    /// Each leaf with its box, when this subtree's box is `area`.
    fn slots(&self, area: Area, settings: &Settings, out: &mut Vec<(WindowId, Area)>) {
        match self {
            Self::Leaf(id) => out.push((*id, area)),
            Self::Split(split) => {
                let (first, second) = split.children(area, settings);
                split.first.slots(first, settings, out);
                split.second.slots(second, settings, out);
            }
        }
    }

    /// Record the direction each split lays out with, so that turning
    /// `preserve_split` on keeps the directions last seen.
    fn settle(&mut self, area: Area, settings: &Settings) {
        if let Self::Split(split) = self {
            split.stacked = split.stacked_in(area, settings);
            let (first, second) = split.children(area, settings);
            split.first.settle(first, settings);
            split.second.settle(second, settings);
        }
    }

    /// Split the leaf of `target` into it and `new`, `new` first when
    /// `place` says so. Returns whether the leaf was found.
    fn split_leaf(
        &mut self,
        target: WindowId,
        new: WindowId,
        area: Area,
        settings: &Settings,
        place: Place,
    ) -> bool {
        match self {
            Self::Leaf(id) => {
                if *id != target {
                    return false;
                }
                let old = *id;
                let side_by_side = area.w > area.h * settings.dwindle.split_width_multiplier;
                // `dwindle:smart_split`: which quarter of the box the point
                // is in. Hyprland compares the slope of the line from the
                // box's centre to the point against the box's own
                // proportions, which is the same as asking which of the
                // four triangles the box's diagonals cut it into the point
                // landed in -- so a wide box is split side by side over
                // most of its area and stacked only near the top and
                // bottom edges.
                let quarter = |x: f64, y: f64| {
                    let (dx, dy) = (x - (area.x + area.w / 2.0), y - (area.y + area.h / 2.0));
                    if (dy / dx).abs() < area.h / area.w {
                        (false, dx < 0.0)
                    } else {
                        (true, dy < 0.0)
                    }
                };
                let new_first = match place {
                    Place::Configured => settings.dwindle.force_split == ForceSplit::First,
                    // Hyprland's `force_split` 0 branch, which a moved window
                    // always takes: the half of the box the point is in.
                    Place::Point(x, y) => {
                        if side_by_side {
                            x < area.x + area.w / 2.0
                        } else {
                            y < area.y + area.h / 2.0
                        }
                    }
                    Place::Quadrant(x, y) => quarter(x, y).1,
                    Place::Toward(direction) => {
                        matches!(direction, Direction::Left | Direction::Up)
                    }
                };
                let stacked = match place {
                    Place::Toward(direction) => {
                        matches!(direction, Direction::Up | Direction::Down)
                    }
                    Place::Quadrant(x, y) => quarter(x, y).0,
                    Place::Configured | Place::Point(..) => !side_by_side,
                };
                let (first, second) = if new_first { (new, old) } else { (old, new) };
                // `dwindle:split_bias`: which of the two the split ratio
                // favours. `0` is directional and gives it to whichever
                // ends up first; `1` is `current` and gives it to the
                // window that was already there, by turning the ratio over
                // when the new one took first place
                // (`CDwindleAlgorithm::onWindowCreatedTiling`).
                let mut ratio = settings.dwindle.default_split_ratio;
                if settings.dwindle.split_bias_current && new_first {
                    ratio = 2.0 - ratio;
                }
                *self = Self::Split(Box::new(Split {
                    stacked,
                    ratio,
                    first: Self::Leaf(first),
                    second: Self::Leaf(second),
                }));
                true
            }
            Self::Split(split) => {
                let (first, second) = split.children(area, settings);
                split.first.split_leaf(target, new, first, settings, place)
                    || split
                        .second
                        .split_leaf(target, new, second, settings, place)
            }
        }
    }

    /// Whether `window`'s sibling is a single window lying in `direction`
    /// from it, across a split in that direction's axis.
    fn faces_lone_sibling(&self, window: WindowId, direction: Direction) -> bool {
        let Self::Split(split) = self else {
            return false;
        };
        let leaf = |node: &Self| matches!(node, Self::Leaf(id) if *id == window);
        let lone = |node: &Self| matches!(node, Self::Leaf(_));
        let across = match direction {
            Direction::Up | Direction::Down => split.stacked,
            Direction::Left | Direction::Right => !split.stacked,
        };
        let faces = match direction {
            Direction::Up | Direction::Left => leaf(&split.second) && lone(&split.first),
            Direction::Down | Direction::Right => leaf(&split.first) && lone(&split.second),
        };
        (across && faces)
            || split.first.faces_lone_sibling(window, direction)
            || split.second.faces_lone_sibling(window, direction)
    }

    /// This subtree without the leaf of `window`: a split that loses a
    /// child becomes its other child.
    fn without(self, window: WindowId) -> Option<Self> {
        match self {
            Self::Leaf(id) if id == window => None,
            Self::Leaf(_) => Some(self),
            Self::Split(split) => {
                let Split {
                    stacked,
                    ratio,
                    first,
                    second,
                } = *split;
                match (first.without(window), second.without(window)) {
                    (Some(first), Some(second)) => Some(Self::Split(Box::new(Split {
                        stacked,
                        ratio,
                        first,
                        second,
                    }))),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }
}

/// One workspace's dwindle tree.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Dwindle {
    root: Option<Node>,
    /// Where `layoutmsg preselect` said the next window goes, taken by the
    /// next window that opens.
    preselect: Option<Direction>,
}

impl Dwindle {
    /// Add `new` beside `target`, or beside the last window if `target` is
    /// not in the tree, when the workspace's work area is `area`.
    ///
    /// `cursor` is where the pointer is, which Hyprland's
    /// `CDwindleAlgorithm::addTarget` reads from the input manager and three
    /// options here read from it: `dwindle:use_active_for_splits = 0` opens
    /// beside the window it is over rather than beside the focused one, and
    /// `dwindle:force_split = 0` and `dwindle:smart_split` put the new
    /// window on the half or the quarter of that window's box it is in. A
    /// compositor that has not seen a pointer gives `None`, and each of the
    /// three falls back to what it did before there was one.
    pub(crate) fn insert(
        &mut self,
        new: WindowId,
        target: Option<WindowId>,
        cursor: Option<(f64, f64)>,
        area: Area,
        settings: &Settings,
    ) {
        let Some(mut root) = self.root.take() else {
            self.root = Some(Node::Leaf(new));
            return;
        };
        // Whose box to split. Hyprland's default is the focused window
        // (`use_active_for_splits`); with it off, the window the pointer is
        // over, and the nearest one when the pointer is over none -- a bar's
        // strip, or a gap.
        let under = cursor.and_then(|at| nearest(&root, at, area, settings));
        let target = if settings.dwindle.use_active_for_splits {
            target
                .filter(|target| root.contains(*target))
                .or(under)
                .or_else(|| leaves(&root).last().copied())
        } else {
            under
                .or_else(|| target.filter(|target| root.contains(*target)))
                .or_else(|| leaves(&root).last().copied())
        };
        if let Some(target) = target {
            // `layoutmsg preselect` names the side for one window, or --
            // with `dwindle:permanent_direction_override` -- for every
            // window until it is cleared. Hyprland resets its
            // `m_overrideDirection` after one window unless the option is
            // on, and `preselect none` clears it either way.
            let chosen = if settings.dwindle.permanent_direction_override {
                self.preselect
            } else {
                self.preselect.take()
            };
            let place = match (chosen, cursor) {
                (Some(direction), _) => Place::Toward(direction),
                (None, Some((x, y))) if settings.dwindle.smart_split => Place::Quadrant(x, y),
                (None, Some((x, y))) if settings.dwindle.force_split == ForceSplit::Auto => {
                    Place::Point(x, y)
                }
                (None, _) => Place::Configured,
            };
            let _found = root.split_leaf(target, new, area, settings, place);
        }
        self.root = Some(root);
    }

    /// Add `new` beside the window whose box is nearest the point `(x, y)`,
    /// on the half of that box the point is in, or, with `toward`, on the
    /// side of it a move in that direction heads for: how Hyprland puts back
    /// a window `movewindow` took out (`CDwindleAlgorithm::movedTarget`).
    pub(crate) fn insert_at(
        &mut self,
        new: WindowId,
        (x, y): (f64, f64),
        toward: Option<Direction>,
        area: Area,
        settings: &Settings,
    ) {
        let Some(mut root) = self.root.take() else {
            self.root = Some(Node::Leaf(new));
            return;
        };
        let place = toward.map_or(Place::Point(x, y), Place::Toward);
        if let Some(target) = nearest(&root, (x, y), area, settings) {
            let _found = root.split_leaf(target, new, area, settings, place);
        }
        self.root = Some(root);
    }

    /// Add `new`, a window the pointer dragged and dropped at `(x, y)`,
    /// beside the window whose box is nearest the point.
    ///
    /// Hyprland's `CDwindleAlgorithm::addTarget` with the drag controller's
    /// `wasDraggingWindow()` set: the box is the one under the pointer --
    /// the dragged window is not in the tree, so it cannot be its own --
    /// and the side is a `layoutmsg preselect`'s if one is waiting, then
    /// the *quarter* of the box the pointer is in with
    /// `dwindle:precise_mouse_move`, and otherwise the half of it, whatever
    /// `force_split` says: a drop is aimed, a new window is not.
    pub(crate) fn insert_dropped(
        &mut self,
        new: WindowId,
        (x, y): (f64, f64),
        area: Area,
        settings: &Settings,
    ) {
        let Some(mut root) = self.root.take() else {
            self.root = Some(Node::Leaf(new));
            return;
        };
        let chosen = if settings.dwindle.permanent_direction_override {
            self.preselect
        } else {
            self.preselect.take()
        };
        let place = match chosen {
            Some(direction) => Place::Toward(direction),
            None if settings.dwindle.precise_mouse_move => Place::Quadrant(x, y),
            None => Place::Point(x, y),
        };
        if let Some(target) = nearest(&root, (x, y), area, settings) {
            let _found = root.split_leaf(target, new, area, settings, place);
        }
        self.root = Some(root);
    }

    /// Whether `window`'s sibling is a single window in `direction`, the
    /// case in which Hyprland's `moveTargetInDirection` overrides where the
    /// moved window lands so that it ends up on the far side of it.
    pub(crate) fn faces_lone_sibling(&self, window: WindowId, direction: Direction) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| root.faces_lone_sibling(window, direction))
    }

    /// Remove `window`, promoting its sibling.
    pub(crate) fn remove(&mut self, window: WindowId) {
        self.root = self.root.take().and_then(|root| root.without(window));
    }

    /// Exchange two windows' places, leaving every split alone.
    pub(crate) fn swap(&mut self, a: WindowId, b: WindowId) {
        if let Some(root) = &mut self.root
            && root.contains(a)
            && root.contains(b)
        {
            root.swap(a, b);
        }
    }

    /// `layoutmsg togglesplit`: turn the split holding `window` the other
    /// way.
    ///
    /// As in Hyprland, this lasts only while `dwindle:preserve_split` is on:
    /// with it off, both compositors work a split's direction out from the
    /// shape of its box every time they lay the tree out, so the flip is
    /// undone by the next frame. Hyprland's `recalcSizePosRecursive` does
    /// exactly that, and so does `Split::stacked_in`.
    pub(crate) fn toggle_split(&mut self, window: WindowId) -> bool {
        let Some(root) = &mut self.root else {
            return false;
        };
        let Some(path) = root.path_to_parent(window) else {
            return false;
        };
        let Some(split) = root.split_at(&path) else {
            return false;
        };
        split.stacked = !split.stacked;
        true
    }

    /// `layoutmsg swapsplit`: exchange the two halves of the split holding
    /// `window`, so the window changes sides without changing size.
    pub(crate) fn swap_split(&mut self, window: WindowId) -> bool {
        let Some(root) = &mut self.root else {
            return false;
        };
        let Some(path) = root.path_to_parent(window) else {
            return false;
        };
        let Some(split) = root.split_at(&path) else {
            return false;
        };
        core::mem::swap(&mut split.first, &mut split.second);
        true
    }

    /// `layoutmsg movetoroot`: exchange `window` with the whole of the
    /// other half of the tree, so it takes half the screen.
    ///
    /// `stable` then exchanges the root's two halves as well, which is what
    /// Hyprland's own flag is for: the window ends up on the side of the
    /// screen it was already on rather than jumping across.
    pub(crate) fn move_to_root(&mut self, window: WindowId, stable: bool) -> bool {
        let Some(root) = &mut self.root else {
            return false;
        };
        let Some(path) = root.path_to_parent(window) else {
            return false;
        };
        // Its parent is the root, so it is already one of the two halves.
        let Some((&side, _)) = path.split_first() else {
            return false;
        };
        let Some(split) = root.split_at(&path) else {
            return false;
        };
        let mut deep = path.clone();
        deep.push(split.second == Node::Leaf(window));
        // The other half of the root, which is never inside `deep`.
        let shallow = [!side];
        let Some(here) = root.at(&deep) else {
            return false;
        };
        let leaf = core::mem::replace(here, Node::Leaf(window));
        let Some(there) = root.at(&shallow) else {
            return false;
        };
        let other = core::mem::replace(there, leaf);
        if let Some(here) = root.at(&deep) {
            *here = other;
        }
        if stable && let Node::Split(split) = root {
            core::mem::swap(&mut split.first, &mut split.second);
        }
        true
    }

    /// `layoutmsg preselect`: where the next window opened on this
    /// workspace goes, whatever the box's shape would otherwise say.
    pub(crate) fn preselect(&mut self, direction: Option<Direction>) {
        self.preselect = direction;
    }

    /// Whether `window` is in the tree.
    pub(crate) fn contains(&self, window: WindowId) -> bool {
        self.root.as_ref().is_some_and(|root| root.contains(window))
    }

    /// The windows, in order.
    pub(crate) fn windows(&self) -> Vec<WindowId> {
        self.root.as_ref().map(leaves).unwrap_or_default()
    }

    /// Each window's box, when the workspace's work area is `area`.
    pub(crate) fn slots(&self, area: Area, settings: &Settings) -> Vec<(WindowId, Area)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.slots(area, settings, &mut out);
        }
        out
    }

    /// Resize a tiled window by moving the splits it sits under, when the
    /// workspace's work area is `area`. Gives whether anything moved.
    ///
    /// Hyprland's `CDwindleAlgorithm::resizeTarget`. A tiled window has no
    /// rectangle of its own -- its box is whatever the tree gives it -- so
    /// making it wider is moving the nearest split that runs down the
    /// screen, and making it taller is moving the nearest that runs across.
    /// Each ratio changes by the distance as a share of half the box that
    /// split divides, and is held to Hyprland's own 0.1 to 1.9.
    ///
    /// Two things are Hyprland's that are worth knowing. A window against
    /// *both* of the work area's sides cannot be made wider by moving
    /// anything, so the sideways part of the distance is dropped rather
    /// than applied to some split further up (`STICKS`, within two pixels);
    /// the same downward. And a split is only moved if there is one in that
    /// direction -- a row of windows side by side has no split across it,
    /// and asking for one taller does nothing rather than something
    /// arbitrary.
    ///
    /// `corner` is the edge the pull is on, which decides *which* of the
    /// splits above a window moves when several run the same way. A
    /// dispatcher pulls on no edge ([`Corner::NONE`]) and the nearest of
    /// each direction is taken; a drag that grabbed a border names one, and
    /// the split taken is the one on that side. A window against the work
    /// area's far edge counts as having grabbed the near one, since that is
    /// the only edge it can move.
    ///
    /// `dwindle:smart_resizing` -- Hyprland's default -- is the second half:
    /// once the split on the grabbed side has moved, the *next* one the
    /// other way is moved back by as much, so the window beyond it keeps the
    /// size it had and only the two either side of the grabbed edge change.
    /// At [`Corner::NONE`] there is never an inner split to move, so both
    /// settings agree and a dispatcher cannot tell them apart.
    pub(crate) fn resize(
        &mut self,
        window: WindowId,
        by: (f64, f64),
        corner: Corner,
        area: Area,
        settings: &Settings,
    ) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        let Some((chain, box_of)) = root.ancestors_of(window, area, settings) else {
            return false;
        };
        // Hyprland's `STICKS`: an edge is against the work area's when it
        // is within two pixels of it, which is what a box divided by ratios
        // leaves.
        let (against_left, against_right) = (
            sticks_fine(box_of.x, area.x),
            sticks_fine(box_of.x + box_of.w, area.x + area.w),
        );
        let (against_top, against_bottom) = (
            sticks_fine(box_of.y, area.y),
            sticks_fine(box_of.y + box_of.h, area.y + area.h),
        );
        let allowed = (
            if against_left && against_right {
                0.0
            } else {
                by.0
            },
            if against_top && against_bottom {
                0.0
            } else {
                by.1
            },
        );
        // Hyprland's `LEFT`, `TOP`, `RIGHT`, `BOTTOM`: the edge that was
        // grabbed, or -- for a window with its back to one side of the work
        // area -- the only edge it has.
        let none = corner.is_none();
        let (left, right) = (corner.left || against_right, corner.right || against_left);
        let (top, bottom) = (corner.top || against_bottom, corner.bottom || against_top);
        // Walking up from the window, as Hyprland does: the first split of
        // each direction that the pull is *towards* is the outer one, and the
        // first that is not is the inner one. `side` is which child the way
        // down took, so a `true` is Hyprland's `children[1] == PCURRENT`.
        let (mut across, mut across_in) = (None::<&Ancestor>, None::<&Ancestor>);
        let (mut down, mut down_in) = (None::<&Ancestor>, None::<&Ancestor>);
        for above in chain.iter().rev() {
            if down.is_none()
                && above.stacked
                && (none || (top && above.side) || (bottom && !above.side))
            {
                down = Some(above);
            } else if down.is_none() && down_in.is_none() && above.stacked {
                down_in = Some(above);
            } else if across.is_none()
                && !above.stacked
                && (none || (left && above.side) || (right && !above.side))
            {
                across = Some(above);
            } else if across.is_none() && across_in.is_none() && !above.stacked {
                across_in = Some(above);
            }
            if down.is_some() && across.is_some() {
                break;
            }
        }
        let mut moved = false;
        for (outer, inner, sideways) in [(across, across_in, true), (down, down_in, false)] {
            let Some(outer) = outer else {
                continue;
            };
            let distance = if sideways { allowed.0 } else { allowed.1 };
            // What the inner split's own box measures now, which is what it
            // has to go on measuring: read before the outer one moves, as
            // Hyprland's `ORIGINAL` is.
            let was = inner.map(|inner| {
                if sideways {
                    inner.below.w
                } else {
                    inner.below.h
                }
            });
            let whole = if sideways { outer.area.w } else { outer.area.h };
            moved |= self.turn(&outer.path, distance * 2.0 / whole, None);
            let (Some(inner), Some(was)) = (inner, was) else {
                continue;
            };
            // The inner split's box has just changed, so it is measured
            // again -- the way down is walked afresh, and the split is the
            // one at the same path, since moving a ratio does not reshape
            // the tree. The ratio that leaves the node beyond it where it
            // was is Hyprland's, and which way round depends on the side.
            let Some(now) = self
                .root
                .as_ref()
                .and_then(|root| root.ancestors_of(window, area, settings))
                .and_then(|(chain, _)| chain.into_iter().find(|above| above.path == inner.path))
            else {
                continue;
            };
            let whole = if sideways { now.area.w } else { now.area.h };
            let ratio = if now.side {
                2.0 - (was + distance) / whole * 2.0
            } else {
                (was - distance) / whole * 2.0
            };
            moved |= self.turn(&now.path, 0.0, Some(ratio));
        }
        moved
    }

    /// Move the split a path leads to, by `change` or to `exactly`, held to
    /// Hyprland's 0.1 to 1.9. Gives whether it moved.
    fn turn(&mut self, path: &[bool], change: f64, exactly: Option<f64>) -> bool {
        let wanted = exactly.unwrap_or(f64::NAN);
        let Some(root) = &mut self.root else {
            return false;
        };
        let Some(split) = root.split_at(path) else {
            return false;
        };
        let now = if exactly.is_some() {
            wanted
        } else {
            split.ratio + change
        };
        if !now.is_finite() {
            return false;
        }
        let was = split.ratio;
        split.ratio = now.clamp(0.1, 1.9);
        split.ratio != was
    }

    /// Record the directions the splits lay out with in `area`.
    pub(crate) fn settle(&mut self, area: Area, settings: &Settings) {
        if let Some(root) = &mut self.root {
            root.settle(area, settings);
        }
    }
}

/// The window whose box is nearest `at`, which is the one the point is in
/// when it is in any of them: Hyprland's `getClosestNode`.
///
/// The distance is to the box rather than to its centre -- zero inside it --
/// so a point in a gap or on a bar's strip picks the window it is beside.
fn nearest(root: &Node, (x, y): (f64, f64), area: Area, settings: &Settings) -> Option<WindowId> {
    let mut slots = Vec::new();
    root.slots(area, settings, &mut slots);
    let mut nearest: Option<(f64, WindowId)> = None;
    for (window, slot) in slots {
        let dx = (slot.x - x).max(x - (slot.x + slot.w)).max(0.0);
        let dy = (slot.y - y).max(y - (slot.y + slot.h)).max(0.0);
        let distance = dx * dx + dy * dy;
        if nearest.is_none_or(|(best, _)| distance < best) {
            nearest = Some((distance, window));
        }
    }
    nearest.map(|(_, window)| window)
}

/// The leaves of `root`, in order.
fn leaves(root: &Node) -> Vec<WindowId> {
    let mut out = Vec::new();
    root.leaves(&mut out);
    out
}
