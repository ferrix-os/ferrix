//! Surfaces and regions: what a client draws on, and what it says about it.
//!
//! # Nothing takes effect until a commit
//!
//! Every `wl_surface` request but `destroy` and `frame` changes *pending*
//! state; `commit` makes the pending state current, all of it at once. That
//! is the whole reason a window never tears or shows half an update: a client
//! attaches a new buffer, says which part changed, sets its scale and commits,
//! and the compositor sees one consistent surface rather than four halfway
//! ones.
//!
//! What the protocol calls double-buffered is spelled out here as two structs
//! with the same shape, because the alternative -- flags saying which fields
//! are dirty -- is where the bugs are.
//!
//! # A commit's damage is not its buffer's damage
//!
//! `wl_surface.damage` is in surface coordinates and `damage_buffer` in the
//! buffer's, which differ when the scale or transform is not the identity.
//! Both accumulate until a commit and both are cleared by it, as
//! `wl_surface`'s description says. They are kept apart here and joined only
//! when the frame is drawn, which is where the scale that applies to them is
//! known.

use compositor_wire::ObjectId;

/// A rectangle, in whatever coordinates its holder is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width; never negative in anything this module keeps.
    pub width: i32,
    /// Height; never negative in anything this module keeps.
    pub height: i32,
}

impl Rect {
    /// A rectangle of `width` by `height` at (`x`, `y`), or `None` when it
    /// covers no pixel.
    ///
    /// `wl_surface.damage` takes an arbitrary rectangle and the protocol puts
    /// no bound on it; one that is empty or negative is dropped rather than
    /// refused, since the protocol gives no error for it and libwayland
    /// passes it through to a compositor that clips it away.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        Some(Self {
            x,
            y,
            width,
            height,
        })
    }
}

/// A `wl_region`: rectangles added and subtracted, in surface coordinates.
///
/// Kept as the client's own list of operations rather than reduced to a set,
/// because the only things done with a region are "is this point inside it"
/// and "hand it to the renderer", and a client's `add`/`subtract` order is
/// what decides both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    operations: Vec<(bool, Rect)>,
}

impl Region {
    /// An empty region.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            operations: Vec::new(),
        }
    }

    /// Add a rectangle.
    pub fn add(&mut self, rect: Rect) {
        self.operations.push((true, rect));
    }

    /// Take a rectangle away.
    pub fn subtract(&mut self, rect: Rect) {
        self.operations.push((false, rect));
    }

    /// Whether (`x`, `y`) is in the region: the last operation covering it
    /// wins, which is what `pixman_region32_union` and `_subtract` applied in
    /// order come to.
    #[must_use]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        let mut inside = false;
        for (adding, rect) in &self.operations {
            if x >= rect.x && y >= rect.y && x - rect.x < rect.width && y - rect.y < rect.height {
                inside = *adding;
            }
        }
        inside
    }

    /// Whether nothing has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// The rectangles and whether each was added, in the client's order.
    #[must_use]
    pub fn operations(&self) -> &[(bool, Rect)] {
        &self.operations
    }
}

/// What a surface shows, or is about to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// The buffer, or `None` where the client attached the null buffer --
    /// which is how a surface is unmapped.
    pub buffer: Option<ObjectId>,
    /// Where the buffer's top-left sits relative to the surface's, from
    /// `attach`'s `x` and `y` before version 5 and from `offset` after it.
    pub offset: (i32, i32),
    /// Damage in surface coordinates, from `wl_surface.damage`.
    pub damage: Vec<Rect>,
    /// Damage in buffer coordinates, from `wl_surface.damage_buffer`.
    pub buffer_damage: Vec<Rect>,
    /// The opaque region, or `None` for "none of it is".
    pub opaque: Option<Region>,
    /// The input region, or `None` for "all of it".
    pub input: Option<Region>,
    /// `wl_output.transform`, applied to the buffer.
    pub transform: u32,
    /// Buffer pixels per surface pixel; at least 1.
    pub scale: i32,
    /// `wp_viewport.set_source`: the part of the buffer to draw, in buffer
    /// coordinates, or `None` for all of it.
    pub viewport_source: Option<(f64, f64, f64, f64)>,
    /// `wp_viewport.set_destination`: the size the surface is to be, in
    /// surface coordinates, or `None` for the buffer's own.
    pub viewport_size: Option<(i32, i32)>,
    /// `wp_alpha_modifier_surface_v1.set_multiplier`: how much of the
    /// surface shows, as a fraction, or `None` for all of it.
    pub alpha: Option<f32>,
    /// `wp_content_type_v1.set_content_type`: what the surface is showing,
    /// as the protocol numbers it.
    pub content: u32,
    /// `ext_background_effect_surface_v1.set_blur_region`: whether what is
    /// behind the surface is blurred.
    pub blur: bool,
    /// `wp_tearing_control_v1.set_presentation_hint`: whether the client
    /// would rather have its frame at once than wait for a whole one.
    pub tearing: bool,
}

impl State {
    /// A surface as it starts: nothing attached, no damage, the identity
    /// transform and a scale of one, as `wl_surface`'s description says.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: None,
            offset: (0, 0),
            damage: Vec::new(),
            buffer_damage: Vec::new(),
            opaque: None,
            input: None,
            transform: compositor_protocol::core::wl_output::transform::NORMAL,
            scale: 1,
            viewport_source: None,
            viewport_size: None,
            alpha: None,
            content: compositor_protocol::content_type::wp_content_type_v1::r#type::NONE,
            blur: false,
            tearing: false,
        }
    }
}

/// A `wl_surface`.
#[derive(Clone, Debug, PartialEq)]
pub struct Surface {
    /// What it shows now.
    pub current: State,
    /// What the next commit will make current.
    pub pending: State,
    /// `wl_callback`s from `wl_surface.frame` that this commit owes, in the
    /// order they were asked for.
    pub frame_callbacks: Vec<ObjectId>,
    /// Callbacks a commit has taken on and that the next frame must fire.
    pub committed_callbacks: Vec<ObjectId>,
    /// How many commits it has had, which is how a test says a commit
    /// happened without reading the state.
    pub commits: u64,
    /// Whether `wl_surface.attach` has been sent since the last commit: the
    /// pending buffer carries the current one forward, so it alone cannot
    /// tell a new frame from a commit of state.
    pub attached: bool,
}

impl Default for Surface {
    fn default() -> Self {
        Self::new()
    }
}

/// What a commit changed, for the compositor above.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Committed {
    /// The buffer now shown, if any.
    pub buffer: Option<ObjectId>,
    /// The buffer the commit replaced, which the compositor must release
    /// once it has finished reading it.
    pub released: Option<ObjectId>,
    /// Whether the surface went from showing nothing to showing something.
    pub mapped: bool,
    /// Whether it went from showing something to showing nothing.
    pub unmapped: bool,
    /// Whether the commit shows a buffer attached for it: a new frame of
    /// the client's, the same `wl_buffer` drawn into again included, and
    /// not a commit of state alone -- a frame callback, an input region,
    /// damage with nothing new under it.
    pub attached: bool,
}

impl Surface {
    /// A surface as a fresh `wl_compositor.create_surface` makes it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: State::new(),
            pending: State::new(),
            frame_callbacks: Vec::new(),
            committed_callbacks: Vec::new(),
            commits: 0,
            attached: false,
        }
    }

    /// Whether the surface has a buffer, which is what "mapped" means for
    /// everything but `xdg_shell`, whose own rules come with it.
    #[must_use]
    pub const fn is_mapped(&self) -> bool {
        self.current.buffer.is_some()
    }

    /// Make the pending state current.
    ///
    /// The damage lists and the frame callbacks are taken by the commit and
    /// start empty again, as the protocol says; everything else is carried
    /// over, because a commit that did not mention the scale leaves the scale
    /// alone.
    pub fn commit(&mut self) -> Committed {
        let was = self.current.buffer;
        let now = self.pending.buffer;
        let attached = std::mem::take(&mut self.attached) && now.is_some();
        // A buffer the surface is no longer showing is the client's again.
        // The same buffer attached twice is not released: the client never
        // got it back.
        let released = match (was, now) {
            (Some(old), Some(new)) if old != new => Some(old),
            (Some(old), None) => Some(old),
            _ => None,
        };
        self.current = std::mem::replace(&mut self.pending, State::new());
        // The next commit starts from what is current, less the parts a
        // commit consumes.
        self.pending = State {
            buffer: self.current.buffer,
            offset: self.current.offset,
            damage: Vec::new(),
            buffer_damage: Vec::new(),
            opaque: self.current.opaque.clone(),
            input: self.current.input.clone(),
            transform: self.current.transform,
            scale: self.current.scale,
            viewport_source: self.current.viewport_source,
            viewport_size: self.current.viewport_size,
            alpha: self.current.alpha,
            content: self.current.content,
            blur: self.current.blur,
            tearing: self.current.tearing,
        };
        self.committed_callbacks
            .append(&mut std::mem::take(&mut self.frame_callbacks));
        self.commits += 1;
        Committed {
            buffer: now,
            released,
            mapped: was.is_none() && now.is_some(),
            unmapped: was.is_some() && now.is_none(),
            attached,
        }
    }

    /// Take the frame callbacks a drawn frame must fire.
    #[must_use]
    pub fn take_frame_callbacks(&mut self) -> Vec<ObjectId> {
        std::mem::take(&mut self.committed_callbacks)
    }
}

/// A `wl_subsurface`: a surface placed relative to another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Subsurface {
    /// The surface that was given the role.
    pub surface: ObjectId,
    /// The surface it is placed against.
    pub parent: ObjectId,
    /// Where its top-left sits in the parent's coordinates.
    pub position: (i32, i32),
    /// Whether its commits wait for the parent's, which is the state a
    /// subsurface starts in.
    pub synchronised: bool,
}

/// What `wl_output` tells a client the screen is.
///
/// A client with no mode has no size to scale against, so every field here
/// is one some toolkit reads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Output {
    /// Where the screen is in the space all screens share.
    pub x: i32,
    /// The same.
    pub y: i32,
    /// In pixels.
    pub width: i32,
    /// In pixels.
    pub height: i32,
    /// Refresh in millihertz, as the protocol counts it.
    pub refresh: i32,
    /// Buffer pixels per logical pixel.
    pub scale: i32,
    /// How the monitor is turned, as a `wl_output.transform`: what
    /// `wl_output.geometry` tells a client. `width` and `height` are the
    /// mode's all the same, which is what `wl_output.mode` carries; the
    /// logical size `zxdg_output_v1` gives is the one turned.
    pub transform: i32,
    /// The name a person sees, as `hyprctl monitors` prints it, and as the
    /// connector is called: `DP-1`, `Virtual-1`.
    pub name: String,
    /// What the monitor says it is, as `wl_output.description` and
    /// `zxdg_output_v1.description` carry it: the make, the model and the
    /// serial, with the connector's name in brackets after them.
    ///
    /// A bar that is told to be on one screen matches on this -- waybar's
    /// `"output"` is a description, not a connector -- so a compositor that
    /// sent a fixed sentence here is one whose person's bar never appears.
    pub description: String,
    /// Every mode the screen can run, `(width, height, refresh in
    /// millihertz)`, which output management lists so that a settings
    /// program can choose one; empty for the running mode alone.
    pub modes: Vec<(i32, i32, i32)>,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            refresh: 60_000,
            scale: 1,
            transform: 0,
            name: "HEADLESS-1".to_owned(),
            description: "Headless output 1 (HEADLESS-1)".to_owned(),
            modes: Vec::new(),
        }
    }
}
