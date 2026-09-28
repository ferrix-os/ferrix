//! What a surface is asked to be: a layer surface's placement, a popup's
//! position, a pointer's shape.

/// A surface the runtime made, by its own number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SurfaceId(pub u32);

/// An idle notification the runtime made ([`crate::Client::idle_notification`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct IdleId(pub u32);

/// `zwlr_layer_shell_v1.layer`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Layer {
    /// Under everything: a wallpaper.
    Background,
    /// Under the windows.
    Bottom,
    /// Over the windows: a bar.
    #[default]
    Top,
    /// Over everything, fullscreen windows too: a launcher.
    Overlay,
}

impl Layer {
    /// The protocol's number.
    #[must_use]
    pub const fn wire(self) -> u32 {
        match self {
            Self::Background => 0,
            Self::Bottom => 1,
            Self::Top => 2,
            Self::Overlay => 3,
        }
    }

    /// From a configuration's word, as waybar's `"layer"` and fuzzel's
    /// `layer=` write it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "background" => Self::Background,
            "bottom" => Self::Bottom,
            "top" => Self::Top,
            "overlay" => Self::Overlay,
            _ => return None,
        })
    }
}

/// `zwlr_layer_surface_v1.anchor`: which edges the surface is held against,
/// as bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Anchor(pub u32);

impl Anchor {
    /// Held against no edge: centred.
    pub const NONE: Self = Self(0);
    /// The top edge.
    pub const TOP: Self = Self(1);
    /// The bottom edge.
    pub const BOTTOM: Self = Self(2);
    /// The left edge.
    pub const LEFT: Self = Self(4);
    /// The right edge.
    pub const RIGHT: Self = Self(8);

    /// Both anchors' edges.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every edge of `other` is in this one.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// `zwlr_layer_surface_v1.keyboard_interactivity`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum KeyboardInteractivity {
    /// Never focused: a bar.
    #[default]
    None,
    /// Takes the keyboard while it is up: a launcher.
    Exclusive,
    /// Focused when clicked, as a window is (version 4).
    OnDemand,
}

impl KeyboardInteractivity {
    /// The protocol's number.
    #[must_use]
    pub const fn wire(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Exclusive => 1,
            Self::OnDemand => 2,
        }
    }
}

/// Space kept between a layer surface and the edges it is anchored to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Margin {
    /// Above.
    pub top: i32,
    /// To the right.
    pub right: i32,
    /// Below.
    pub bottom: i32,
    /// To the left.
    pub left: i32,
}

/// A layer surface's whole placement, sent before its first commit and again
/// whenever [`crate::Client::set_layer_options`] changes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerOptions {
    /// The screen, or `None` to let the compositor choose (the focused one).
    pub output: Option<crate::OutputId>,
    /// Which layer.
    pub layer: Layer,
    /// `namespace`: what `layerrule`s match, `"waybar"`, `"launcher"`.
    pub namespace: String,
    /// The size asked for in logical pixels; a zero is "as wide (tall) as
    /// the anchors let it be", which needs both opposite edges anchored.
    pub size: (u32, u32),
    /// The edges it is held against.
    pub anchor: Anchor,
    /// The exclusive zone: positive keeps windows off that much of the
    /// anchored edge, `0` moves for others' zones, `-1` ignores them.
    pub exclusive_zone: i32,
    /// Space kept from the anchored edges.
    pub margin: Margin,
    /// Whether it takes the keyboard.
    pub keyboard: KeyboardInteractivity,
}

/// A window: an `xdg_toplevel`, which the compositor places, sizes and
/// decorates as it does every other program's window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToplevelOptions {
    /// `xdg_toplevel.set_title`: what a task bar or a window list shows.
    pub title: String,
    /// `xdg_toplevel.set_app_id`: what `windowrule`s match, and the name of
    /// the program's `.desktop` file without its suffix.
    pub app_id: String,
    /// The size asked for, in logical pixels, used for as long as the
    /// compositor's configures say `0x0` ("choose yourself"). A tiling
    /// compositor gives its own size instead.
    pub size: (u32, u32),
    /// `xdg_toplevel.set_parent`, sent before the first commit: the window
    /// this one is a dialog of, which a compositor may float it over.
    pub parent: Option<SurfaceId>,
}

/// A rectangle in a surface's logical pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: i32,
    /// Height.
    pub height: i32,
}

/// Where an `xdg_popup` goes, as `xdg_positioner` says it: a tooltip under
/// the module the pointer is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PopupOptions {
    /// The popup's size in logical pixels.
    pub size: (i32, i32),
    /// The rectangle on the parent it is placed against.
    pub anchor_rect: Rect,
    /// `xdg_positioner.anchor`: the point of `anchor_rect` it hangs from,
    /// the protocol's number (`0` none, `1` top, `2` bottom, `3` left, `4`
    /// right, `5`..`8` the corners).
    pub anchor: u32,
    /// `xdg_positioner.gravity`: which way it grows from that point, same
    /// numbering.
    pub gravity: u32,
    /// `xdg_positioner.constraint_adjustment` bits: how the compositor may
    /// move it to keep it on the screen.
    pub constraint_adjustment: u32,
    /// Moved by this much from where the rest puts it.
    pub offset: (i32, i32),
    /// Whether it takes a grab (a menu) or not (a tooltip, which must not).
    pub grab: bool,
}

impl Default for PopupOptions {
    fn default() -> Self {
        Self {
            size: (1, 1),
            anchor_rect: Rect::default(),
            // Hanging from the bottom of the rectangle and growing down: a
            // tooltip under a bar module.
            anchor: 2,
            gravity: 2,
            // slide_x | slide_y | flip_y: kept on the screen sideways, and
            // above the bar rather than off the bottom of a bottom bar.
            constraint_adjustment: 1 | 2 | 8,
            offset: (0, 0),
            grab: false,
        }
    }
}

/// `wp_cursor_shape_device_v1.shape`: a pointer drawn by the compositor, by
/// name. The numbers are the protocol's.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
    /// The arrow.
    #[default]
    Default,
    /// A hand, over something that can be clicked.
    Pointer,
    /// The I-beam, over text that can be typed into.
    Text,
    /// Nothing may be done here.
    NotAllowed,
    /// Busy.
    Wait,
    /// Any other the protocol names, by its number.
    Other(u32),
}

impl CursorShape {
    /// The protocol's number.
    #[must_use]
    pub const fn wire(self) -> u32 {
        match self {
            Self::Default => 1,
            Self::Pointer => 4,
            Self::Text => 9,
            Self::NotAllowed => 17,
            Self::Wait => 6,
            Self::Other(value) => value,
        }
    }
}
