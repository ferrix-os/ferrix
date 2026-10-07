//! Hyprland's window management as a pure state machine: monitors,
//! workspaces, all four of its tiling layouts, and the dispatchers a key
//! binding runs.
//!
//! Nothing here is a Wayland object, a socket or a device. A window is a
//! [`WindowId`] the protocol server chose, a monitor is a rectangle in
//! logical pixels, and the answer to "where does everything go" is
//! [`State::layout`]: for the workspace each monitor shows, every visible
//! window's rectangle and whether it has focus. The protocol server turns
//! those into configure events and the renderer into borders, so this crate
//! is tested with arithmetic alone and carries over whichever way the Smithay
//! decision in `docs/BACKLOG.md` goes.
//!
//! Hyprland is the reference: the dwindle tree follows
//! `DwindleAlgorithm.cpp`, the master layout `MasterAlgorithm.cpp`, the
//! monocle layout `MonocleAlgorithm.cpp`, the scrolling layout
//! `ScrollingAlgorithm.cpp` and its `ScrollTapeController`, the neighbour
//! search
//! `CCompositor::getWindowInDirection`, and the gap and border arithmetic
//! `applyNodeDataToWindow`. Each module says where it departs.
//!
//! Every change goes through [`State`]'s methods, and each returns the
//! [`Change`]s it caused, so the caller redraws, reconfigures or emits IPC
//! events for exactly those. A dispatcher that cannot do anything, such as
//! `movefocus` with nowhere to go, returns no changes; a dispatcher that is
//! not understood is an [`Error`], never a panic.
//!
//! What is not handled yet: `dwindle:pseudotile` (the option is left unread
//! and every tiled window fills its slot), `scrolling:direction` other than
//! `right`, resizing in the master and scrolling
//! layouts, window selectors as dispatcher arguments, and `movewindow` on a
//! floating window, which in Hyprland pushes it against the monitor's edge
//! and here does nothing.

#![forbid(unsafe_code)]

mod dispatch;
mod dwindle;
mod geometry;
pub mod layers;
mod master;
mod monocle;
pub mod popup;
mod scrolling;
mod settings;
mod snap;
mod state;

#[cfg(test)]
mod tests;

use core::fmt;

pub use compositor_config::Gaps;
pub use dispatch::{
    Direction, Dispatcher, FullscreenMode, GroupMember, Locking, Move, WorkspaceTarget,
};
pub use settings::{
    DwindleSettings, ForceSplit, Layout, MasterSettings, NewStatus, Orientation, Settings,
    SnapSettings,
};
pub use state::{Change, Group, MonitorLayout, Placed, State};

/// A window, by the id the protocol server gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u64);

/// A workspace, by number. Hyprland's workspace ids are signed, with the
/// negative ones for special workspaces; the ones this crate creates count
/// from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(pub i64);

/// A monitor, by the id the backend gave its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonitorId(pub u32);

/// A rectangle in logical pixels, in the global space all monitors share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    /// The left edge.
    pub x: i64,
    /// The top edge.
    pub y: i64,
    /// The width, not negative.
    pub width: i64,
    /// The height, not negative.
    pub height: i64,
}

impl Rect {
    /// A rectangle from its position and size.
    #[must_use]
    pub const fn new(x: i64, y: i64, width: i64, height: i64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The first column right of the rectangle.
    #[must_use]
    pub const fn right(self) -> i64 {
        self.x.saturating_add(self.width)
    }

    /// The first row below the rectangle.
    #[must_use]
    pub const fn bottom(self) -> i64 {
        self.y.saturating_add(self.height)
    }

    /// The rectangle moved by `dx` and `dy`.
    #[must_use]
    pub const fn translate(self, dx: i64, dy: i64) -> Self {
        Self {
            x: self.x.saturating_add(dx),
            y: self.y.saturating_add(dy),
            width: self.width,
            height: self.height,
        }
    }
}

/// A monitor as the layouts see it.
///
/// Not `Eq`: its scale is a float, as Hyprland's is.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// Its id.
    pub id: MonitorId,
    /// What it is called: the connector's name, as Hyprland names a monitor
    /// and as a `monitor =` line, a `workspace` rule and `focusmonitor`
    /// name one. Empty for a monitor nothing has named.
    pub name: String,
    /// Where it is and how big, in logical pixels.
    pub rect: Rect,
    /// The strips along its edges that layer-shell surfaces such as bars
    /// reserve, which tiled windows stay out of: Hyprland's
    /// `vecReservedTopLeft` and `vecReservedBottomRight`.
    pub reserved: Gaps,
    /// How many buffer pixels one logical pixel is on it: `monitor =
    /// name, res, pos, 2`.
    ///
    /// Nothing in the layouts reads it -- `rect` is already in the logical
    /// pixels they work in -- but it is part of what a monitor is, and
    /// `hyprctl monitors` prints it.
    pub scale: f64,
    /// How it is turned: `monitor = name, res, pos, scale, transform, 1`.
    ///
    /// Nothing in the layouts reads this either: `rect` is the monitor as
    /// it is read, turned already, so a monitor stood on its edge is simply
    /// a tall one. It is here for `hyprctl monitors`, which prints it.
    pub transform: compositor_config::Transform,
    /// What the monitor says it is: its make, model and serial with spaces
    /// between them, out of its `EDID`.
    ///
    /// Nothing in the layouts reads this either, and it is part of what a
    /// monitor is for the same reason `scale` is: a `monitor = desc:` line
    /// and a bar's own `"output"` setting name a monitor by it, because a
    /// connector's name moves when a cable does and a description does not.
    pub description: String,
    /// The three parts of it -- the make, the model and the serial -- which
    /// `hyprctl monitors` prints apart as well as together.
    pub made: (String, String, String),
}

/// Which edges of a window a resize pulls on: Hyprland's `eRectCorner`.
///
/// A dispatcher pulls on none of them ([`Corner::NONE`], Hyprland's
/// `CORNER_NONE`), and the layout has to guess which way to go. A drag that
/// grabbed a border knows, and the guess is not needed -- which is the
/// whole reason the type exists.
///
/// Both edges of an axis are never set at once: a grab is at one corner or
/// along one edge, and `left` with `right` would be asking for two
/// directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Corner {
    /// The left edge is being pulled.
    pub left: bool,
    /// The right edge is being pulled.
    pub right: bool,
    /// The top edge is being pulled.
    pub top: bool,
    /// The bottom edge is being pulled.
    pub bottom: bool,
}

impl Corner {
    /// No edge: what a dispatcher pulls on.
    pub const NONE: Self = Self {
        left: false,
        right: false,
        top: false,
        bottom: false,
    };

    /// Whether no edge is named.
    #[must_use]
    pub const fn is_none(self) -> bool {
        !self.left && !self.right && !self.top && !self.bottom
    }
}

/// How big a window may be: `min_size`, `max_size`, `no_max_size` and
/// `keep_aspect_ratio` from its `windowrule`s.
///
/// Hyprland clamps a window's size at every point it could change --
/// `setSizeLimits` -- so the limits belong with the window rather than with
/// the rule that set them: the rule fires once and the window is resized
/// many times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    /// `min_size <w> <h>`: never smaller than this.
    pub smallest: Option<(i64, i64)>,
    /// `max_size <w> <h>`: never larger. `no_max_size` clears it, which is
    /// what a person writes for a window whose own protocol maximum is
    /// wrong.
    pub largest: Option<(i64, i64)>,
    /// `keep_aspect_ratio`: the shape it was first given is the shape it
    /// keeps, so a resize changes one side and the other follows.
    pub keep_aspect: bool,
}

impl Limits {
    /// `rect` held down to what these limits allow, about its top-left
    /// corner.
    #[must_use]
    pub fn hold(self, rect: Rect) -> Rect {
        let mut width = rect.width.max(1);
        let mut height = rect.height.max(1);
        if let Some((least_wide, least_tall)) = self.smallest {
            width = width.max(least_wide.max(1));
            height = height.max(least_tall.max(1));
        }
        if let Some((most_wide, most_tall)) = self.largest {
            width = width.min(most_wide.max(1));
            height = height.min(most_tall.max(1));
        }
        if self.keep_aspect && rect.width > 0 && rect.height > 0 {
            // The shape the rectangle came in with, kept by taking the
            // smaller of the two scales -- which is what fits inside what
            // was asked for rather than spilling out of it.
            let (asked_wide, asked_tall) = (rect.width, rect.height);
            let by_width = width.saturating_mul(asked_tall) / asked_wide.max(1);
            if by_width <= height {
                height = by_width.max(1);
            } else {
                width = (height.saturating_mul(asked_wide) / asked_tall.max(1)).max(1);
            }
        }
        Rect::new(rect.x, rect.y, width, height)
    }
}

/// What a request to the layouts could not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A dispatcher name this crate does not know.
    UnknownDispatcher(String),
    /// A dispatcher it knows, with an argument it cannot take.
    BadArgument {
        /// The dispatcher's name.
        dispatcher: String,
        /// The argument as given.
        arg: String,
    },
    /// A window was to open, and there is no monitor to put it on.
    NoMonitor,
    /// A monitor was added with an id already in use.
    DuplicateMonitor(MonitorId),
    /// A monitor id that is not present.
    UnknownMonitor(MonitorId),
    /// A window was opened with an id already in use.
    DuplicateWindow(WindowId),
    /// A window id that is not present.
    UnknownWindow(WindowId),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDispatcher(name) => write!(f, "Invalid dispatcher {name}"),
            Self::BadArgument { dispatcher, arg } => {
                write!(f, "Invalid argument for {dispatcher}: {arg}")
            }
            Self::NoMonitor => f.write_str("No monitor"),
            Self::DuplicateMonitor(id) => write!(f, "Monitor {} already exists", id.0),
            Self::UnknownMonitor(id) => write!(f, "No monitor {}", id.0),
            Self::DuplicateWindow(id) => write!(f, "Window {} already exists", id.0),
            Self::UnknownWindow(id) => write!(f, "No window {}", id.0),
        }
    }
}

impl core::error::Error for Error {}
