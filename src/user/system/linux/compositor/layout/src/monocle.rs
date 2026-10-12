//! The monocle layout: Hyprland's `MonocleAlgorithm.cpp`.
//!
//! Every window fills the workspace, and one of them is shown. The others
//! are behind it, the same size, and are not drawn at all -- Hyprland sets
//! their layout alpha to zero and blocks their input, and this leaves them
//! out of the frame, which comes to the same picture.
//!
//! What makes it a layout rather than a fullscreen window: the windows are
//! still tiled, so `cyclenext` walks them, closing one shows the next, and
//! nothing about the workspace's gaps, border or rounding changes. A person
//! who wants one window at a time on a small screen writes
//! `general:layout = monocle` and gets a tabbed workspace with no tabs.
//!
//! # Which one is shown
//!
//! The focused one. Hyprland hooks the focus event and brings the focused
//! window to the front (`focusTargetUpdate`), so a window focused by a
//! click, by `cyclenext`, or by a `windowrule` is the one on top. This
//! keeps the same rule by asking who is focused when the slots are worked
//! out, rather than by holding an index -- the focus is already the
//! compositor's and an index beside it is a second answer to the same
//! question.
//!
//! A new window is shown at once, which is what `newTarget` does by
//! pointing the index at it: here that falls out of the compositor
//! focusing a window that has just opened.

use crate::WindowId;
use crate::geometry::Area;

/// One workspace's monocle layout.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Monocle {
    /// Every tiled window, in the order they arrived, which is the order
    /// `cyclenext` walks.
    order: Vec<WindowId>,
}

impl Monocle {
    /// Add `new`, which goes to the end of the list.
    pub(crate) fn insert(&mut self, new: WindowId) {
        if !self.order.contains(&new) {
            self.order.push(new);
        }
    }

    /// Remove `window`.
    pub(crate) fn remove(&mut self, window: WindowId) {
        self.order.retain(|id| *id != window);
    }

    /// Exchange two windows' places, or rename one to the other.
    pub(crate) fn swap(&mut self, a: WindowId, b: WindowId) {
        for id in &mut self.order {
            if *id == a {
                *id = b;
            } else if *id == b {
                *id = a;
            }
        }
    }

    /// Whether `window` is in the list.
    pub(crate) fn contains(&self, window: WindowId) -> bool {
        self.order.contains(&window)
    }

    /// Every window, in the order they arrived.
    pub(crate) fn windows(&self) -> Vec<WindowId> {
        self.order.clone()
    }

    /// Each window's box: the one that is shown, and nothing else.
    ///
    /// The others are the same size and behind it, and Hyprland draws them
    /// with an alpha of zero; leaving them out of the slots is the same
    /// picture and is what the renderer here is given. `shown` is the
    /// workspace's focused window, or `None` for a workspace whose focus is
    /// elsewhere -- and then the first window stands in, because a
    /// workspace with windows on it must show one.
    pub(crate) fn slots(&self, area: Area, shown: Option<WindowId>) -> Vec<(WindowId, Area)> {
        let wanted = shown
            .filter(|window| self.contains(*window))
            .or_else(|| self.order.first().copied());
        wanted
            .map(|window| vec![(window, area)])
            .unwrap_or_default()
    }

    /// Every window's box, the ones behind the shown one included: each
    /// is the whole area, which is where Hyprland keeps them too.
    pub(crate) fn every_slot(&self, area: Area) -> Vec<(WindowId, Area)> {
        self.order.iter().map(|&window| (window, area)).collect()
    }
}
