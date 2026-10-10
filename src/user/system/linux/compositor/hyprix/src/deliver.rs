//! Who gets the key, and who gets the click.
//!
//! The seat says what happened; this says who to tell. Two focuses, because
//! Wayland has two:
//!
//! * **The keyboard's** is the layout's focused window, whatever the pointer
//!   is over. That is what a tiling compositor means by focus, and what
//!   `movefocus` changes.
//! * **The pointer's** is the window under the pointer. A client is told
//!   `enter` when the pointer comes onto its surface and `leave` when it
//!   goes, and the coordinates it is given are its own, not the screen's.
//!
//! A window that is not on the screen -- another workspace, or a client that
//! has not committed a buffer -- has neither.
//!
//! # `follow_mouse`
//!
//! Hyprland's `input:follow_mouse` decides whether moving the pointer onto a
//! window focuses it. 1, the default, is "yes"; 0 is "no". That is the only
//! value read here, since 2 and 3 are about which of the keyboard and the
//! pointer follows the other and both need a click to be acted on.

use std::collections::BTreeMap;

use compositor_layout::{State, WindowId};
use compositor_wire::{Fixed, ObjectId};

use crate::frame::Source;
use crate::seat::Action;
use crate::state::Slot;

/// Where a window is on the screen, and which client's surface it is.
#[derive(Clone, Copy, Debug)]
struct Placement {
    /// The layout's window, or `None` for a popup, which is no window.
    window: Option<WindowId>,
    client: usize,
    surface: ObjectId,
    rect: (i64, i64, i64, i64),
}

impl Placement {
    /// Whether the point `(x, y)` is on the window.
    fn contains(&self, x: f64, y: f64) -> bool {
        let (left, top, width, height) = self.rect;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's edge is at most a screen's width; the loss is beyond any pixel"
        )]
        let (left, top, right, bottom) = (
            left as f64,
            top as f64,
            (left + width) as f64,
            (top + height) as f64,
        );
        x >= left && x < right && y >= top && y < bottom
    }
}

/// What has the keyboard and what has the pointer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Focus {
    keyboard: Option<(usize, ObjectId)>,
    pointer: Option<(usize, ObjectId)>,
}

impl Focus {
    /// Nothing focused, which is a compositor with no windows.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            keyboard: None,
            pointer: None,
        }
    }

    /// The surface the pointer is on, if any.
    ///
    /// Which client that is decides what the cursor looks like: a client
    /// says with `wl_pointer.set_cursor`, and the one it is saying about is
    /// the one the pointer is over.
    #[must_use]
    pub const fn pointer_on(&self) -> Option<(usize, ObjectId)> {
        self.pointer
    }

    /// Forget which surface has the keyboard, so that the next pass sends
    /// it a fresh `enter`.
    ///
    /// `pass` and `sendshortcut` hand the keyboard to another window for
    /// the length of one key and hand it back; the focus has not changed,
    /// so without this the loop would see nothing to do and the real window
    /// would be left without an `enter`.
    pub const fn forget_keyboard(&mut self) {
        self.keyboard = None;
    }

    /// The window the keyboard is on, if any.
    #[must_use]
    pub const fn keyboard(&self) -> Option<(usize, ObjectId)> {
        self.keyboard
    }

    /// A connection went, so every slot after it moved: `places[old]` is
    /// where the client that was at `old` is now, or `None` if it is the one
    /// that went.
    ///
    /// The focus holds a client by its place in the list, and a place that
    /// is no longer that client is a `wl_keyboard.leave` sent to a stranger.
    pub fn renumber(&mut self, places: &[Option<usize>]) {
        moved(&mut self.keyboard, places);
        moved(&mut self.pointer, places);
    }

    /// Forget a focus whose surface its client has destroyed, telling
    /// nobody.
    ///
    /// Called every pass once the clients have been read. The client has
    /// had `delete_id` for the surface, so a `leave` naming it is an unknown
    /// object to libwayland, which ends the connection: Chrome closed a menu
    /// under the pointer, the pointer moved, and the `leave` for the menu
    /// took the whole browser down. Forgetting it in the same pass also
    /// keeps a `leave` from reaching whatever the client gives the id to
    /// next, which it may do as soon as it reads the `delete_id`.
    pub fn prune(&mut self, slots: &[Slot]) {
        let alive = |held: Option<(usize, ObjectId)>| {
            held.filter(|(client, surface)| {
                slots
                    .get(*client)
                    .is_some_and(|slot| slot.client().surface(*surface).is_some())
            })
        };
        self.keyboard = alive(self.keyboard);
        self.pointer = alive(self.pointer);
    }

    /// Give the keyboard to whatever the layout says is focused.
    ///
    /// Called every time round the loop. A focus that has not moved sends
    /// nothing: `wl_keyboard.leave` and `enter` to the same surface would
    /// make a toolkit drop what it was typing.
    ///
    /// The new focus is remembered only if the `enter` reached a
    /// `wl_keyboard`. A window is usually mapped in the same burst of
    /// requests that asks the seat for its keyboard, and whichever the server
    /// reads first, the client has to end up with the focus; remembering an
    /// `enter` that went nowhere would leave that window unable to be typed
    /// into for as long as it lived.
    pub fn follow_layout(
        &mut self,
        state: &State,
        slots: &mut [Slot],
        sources: &BTreeMap<WindowId, Source>,
        held: &[u16],
        modifiers: compositor_xkb::Modifiers,
    ) {
        let wanted = state
            .focused_window()
            .and_then(|window| sources.get(&window))
            .map(|source| (source.client, source.surface));
        self.follow(wanted, slots, held, modifiers);
    }

    /// The layer surface that asks for the keyboard, if one does: the
    /// topmost mapped surface on the top or overlay layer whose
    /// `keyboard_interactivity` is not `none`, and of those on one layer the
    /// one made last.
    ///
    /// wlr-layer-shell gives an `exclusive` surface above the windows every
    /// key while it is mapped -- that is how a launcher such as fuzzel is
    /// typed into at all -- and Hyprland gives an `on_demand` one the
    /// keyboard when it maps, as a click on it would. Taking it back from an
    /// `on_demand` surface when another window is clicked is not done here:
    /// the keyboard stays with the surface until it goes.
    #[must_use]
    pub fn interactive_layer(slots: &[Slot]) -> Option<(usize, ObjectId)> {
        let mut best: Option<(compositor_server::Layer, usize, ObjectId)> = None;
        for (index, slot) in slots.iter().enumerate() {
            for (_, layer) in slot.client().layer_surfaces() {
                let wants = layer.keyboard_interactivity != 0
                    && layer.committed
                    && layer.layer.above_windows();
                if wants && best.as_ref().is_none_or(|(on, ..)| layer.layer >= *on) {
                    best = Some((layer.layer, index, layer.surface));
                }
            }
        }
        best.map(|(_, client, surface)| (client, surface))
    }

    /// Give the keyboard to `wanted`, or to nothing.
    ///
    /// The layout says what that is in the ordinary case; a locked session
    /// says the lock's own surface, and a locked session whose program has
    /// gone says nothing at all -- which is a screen where no key reaches
    /// any client, and is what a lock is for.
    pub fn follow(
        &mut self,
        wanted: Option<(usize, ObjectId)>,
        slots: &mut [Slot],
        held: &[u16],
        modifiers: compositor_xkb::Modifiers,
    ) {
        if wanted == self.keyboard {
            return;
        }
        if let Some((client, surface)) = self.keyboard
            && let Some(slot) = slots.get_mut(client)
        {
            let _ = slot.client_mut().keyboard_leave(surface);
        }
        self.keyboard = None;
        let Some((client, surface)) = wanted else {
            return;
        };
        if let Some(slot) = slots.get_mut(client)
            && slot
                .client_mut()
                .keyboard_enter(surface, held, modifiers)
                .is_some()
        {
            self.keyboard = wanted;
        }
    }

    /// Everything the pointer left or arrived on, and the pointer's position
    /// inside whatever it is on now.
    fn move_pointer(
        &mut self,
        placements: &[Placement],
        slots: &mut [Slot],
        at: (f64, f64),
    ) -> Option<(f64, f64)> {
        let over = placements.iter().find(|placed| {
            let (x, y, width, height) = placed.rect;
            #[expect(
                clippy::cast_precision_loss,
                reason = "a window's edge is at most a screen's width; the loss is beyond any pixel"
            )]
            let (left, top, right, bottom) =
                (x as f64, y as f64, (x + width) as f64, (y + height) as f64);
            at.0 >= left && at.0 < right && at.1 >= top && at.1 < bottom
        });
        let wanted = over.map(|placed| (placed.client, placed.surface));
        // Where in the surface the pointer is. The frame stretches a
        // window's surface to its rectangle, so the pointer's place in the
        // rectangle is scaled back by the same ratio: a surface bigger than
        // its tile -- Chrome's, with its shadows around the window -- is
        // drawn smaller than it is, and a pointer taken at the rectangle's
        // own scale drifted from what was under it, by nothing at the
        // top-left corner and by the whole difference at the bottom-right.
        let local = over.map(|placed| {
            let window = slots
                .get(placed.client)
                .and_then(|slot| window_part(slot, placed.surface));
            surface_point(at, placed.rect, window)
        });

        if wanted != self.pointer {
            if let Some((client, surface)) = self.pointer
                && let Some(slot) = slots.get_mut(client)
            {
                let _ = slot.client_mut().pointer_leave(surface);
                slot.client_mut().pointer_frame();
            }
            // As in `follow_layout`: remembered only if it arrived.
            self.pointer = None;
            if let Some(((client, surface), (x, y))) = wanted.zip(local)
                && let Some(slot) = slots.get_mut(client)
                && slot
                    .client_mut()
                    .pointer_enter(surface, fixed(x), fixed(y))
                    .is_some()
            {
                slot.client_mut().pointer_frame();
                self.pointer = wanted;
            }
        }
        local
    }
}

/// One dispatcher a bind asked for: its name, its argument, and the key
/// that fired it when a key did.
///
/// The key is here because of one dispatcher: `pass` sends the key that
/// fired the bind on to another window, and has nothing to send without it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The dispatcher's name, lower-cased.
    pub name: String,
    /// Its argument, as the bind wrote it.
    pub argument: String,
    /// The evdev code and the modifiers held with it.
    pub trigger: Option<(u16, u32)>,
}

/// Which client and surface the pointer is over, and where inside it.
///
/// A drag needs this without moving the pointer's own focus: while a drag is
/// on, a window must not be told the pointer entered it -- that is the
/// protocol's rule, and a practical one, since a window that got a
/// `wl_pointer.enter` mid-drag would think the person had clicked it.
#[must_use]
pub fn under_pointer(
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    at: (f64, f64),
) -> Option<(usize, ObjectId, (f64, f64))> {
    let placements = placements(state, sources);
    let over = placements.iter().find(|placed| {
        let (x, y, width, height) = placed.rect;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's edge is at most a screen's width; the loss is beyond any pixel"
        )]
        let (left, top, right, bottom) =
            (x as f64, y as f64, (x + width) as f64, (y + height) as f64);
        at.0 >= left && at.0 < right && at.1 >= top && at.1 < bottom
    })?;
    let (x, y, _, _) = over.rect;
    #[expect(
        clippy::cast_precision_loss,
        reason = "as above: the origin is a screen coordinate"
    )]
    let local = (at.0 - x as f64, at.1 - y as f64);
    Some((over.client, over.surface, local))
}

/// What delivering the seat's actions did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Done {
    /// The dispatchers to run, in order, which the caller hands the layout.
    pub dispatch: Vec<Asked>,
    /// The window `follow_mouse` asks to be focused, if the pointer moved
    /// onto one that is not.
    ///
    /// Not a dispatcher, because Hyprland's `focuswindow` takes a window
    /// *rule* -- `class:foot`, `title:...` -- and not an id, so there is no
    /// dispatcher that says "this one". The layout has `focus_window`, and
    /// the caller calls it.
    pub focus: Option<WindowId>,
}

/// Send the seat's actions to whoever they belong to.
///
/// The dispatchers are not run here: they change the layout, and the layout
/// is the caller's. Everything else is sent as it goes.
#[expect(
    clippy::too_many_arguments,
    reason = "an action reaches the focus, the layout, every connection, the clock, one option and the drag"
)]
pub fn deliver(
    actions: &[Action],
    focus: &mut Focus,
    state: &State,
    slots: &mut [Slot],
    sources: &BTreeMap<WindowId, Source>,
    time: u32,
    follow_mouse: bool,
    dragging: bool,
) -> Done {
    let placements = with_popups(placements(state, sources), slots, state, sources);
    let mut done = Done::default();
    for action in actions {
        match action {
            Action::Dispatch {
                name,
                argument,
                trigger,
            } => {
                done.dispatch.push(Asked {
                    name: name.clone(),
                    argument: argument.clone(),
                    trigger: *trigger,
                });
            }
            Action::Key { code, pressed } => {
                if let Some((client, _)) = focus.keyboard
                    && let Some(slot) = slots.get_mut(client)
                {
                    let _ = slot.client_mut().keyboard_key(time, *code, *pressed);
                }
            }
            Action::Modifiers(modifiers) => {
                if let Some((client, _)) = focus.keyboard
                    && let Some(slot) = slots.get_mut(client)
                {
                    let _ = slot.client_mut().keyboard_modifiers(*modifiers);
                }
            }
            Action::Relative { dx, dy } => {
                // The distance, for every `zwp_relative_pointer_v1` of the
                // client the pointer is over. It is the device's own and is
                // sent whether or not the pointer moved: the protocol says
                // the events are "not limited by the surface", and a client
                // that locked the pointer has no other way to know it moved.
                if let Some((client, _)) = focus.pointer
                    && let Some(slot) = slots.get_mut(client)
                {
                    slot.client_mut().relative_motion(
                        u64::from(time).saturating_mul(1_000),
                        *dx,
                        *dy,
                    );
                }
            }
            Action::Pointer { x, y } => {
                // While a drag is on the pointer enters and leaves
                // nothing: the drag's own `enter` and `leave` are what a
                // window is told, and a `wl_pointer.enter` mid-drag would
                // look to it like a click.
                if dragging {
                    continue;
                }
                if let Some((local_x, local_y)) = focus.move_pointer(&placements, slots, (*x, *y))
                    && let Some((client, _)) = focus.pointer
                    && let Some(slot) = slots.get_mut(client)
                {
                    slot.client_mut()
                        .pointer_motion(time, fixed(local_x), fixed(local_y));
                    slot.client_mut().pointer_frame();
                }
                // Moving onto a window focuses it, which is what
                // `follow_mouse` turns off. The window under the pointer,
                // whether or not its client took the pointer: a terminal
                // that binds no `wl_pointer` is still focused by moving
                // onto it, as in Hyprland.
                if follow_mouse
                    && let Some(window) = placements
                        .iter()
                        .find(|placed| placed.contains(*x, *y))
                        .and_then(|placed| placed.window)
                    && state.focused_window() != Some(window)
                {
                    done.focus = Some(window);
                }
            }
            Action::Button { button, pressed } => {
                // A button while a drag is on is the drop, which the
                // compositor makes: the window under the pointer must not
                // be told the person clicked it.
                if dragging {
                    continue;
                }
                if let Some((client, _)) = focus.pointer
                    && let Some(slot) = slots.get_mut(client)
                {
                    let _ = slot.client_mut().pointer_button(time, *button, *pressed);
                    slot.client_mut().pointer_frame();
                }
            }
            Action::Axis { axis, value } => {
                if let Some((client, _)) = focus.pointer
                    && let Some(slot) = slots.get_mut(client)
                {
                    slot.client_mut().pointer_axis(time, *axis, fixed(*value));
                    slot.client_mut().pointer_frame();
                }
            }
        }
    }
    done
}

/// Every window on a screen now, with where it is and whose it is.
fn placements(state: &State, sources: &BTreeMap<WindowId, Source>) -> Vec<Placement> {
    let mut out = Vec::new();
    for output in state.layout() {
        for placed in &output.windows {
            let Some(source) = sources.get(&placed.window) else {
                continue;
            };
            out.push(Placement {
                window: Some(placed.window),
                client: source.client,
                surface: source.surface,
                rect: (
                    placed.rect.x,
                    placed.rect.y,
                    placed.rect.width,
                    placed.rect.height,
                ),
            });
        }
    }
    // The last window drawn is the one on top, so the pointer finds it first.
    out.reverse();
    out
}

/// The point `at` on the screen as a place in a surface whose `window` --
/// its part, in surface coordinates -- the frame draws stretched to `rect`:
/// the window's corner plus the point's place in the rectangle, scaled by
/// the window over the rectangle, which is the inverse of the drawing. A
/// window of the tile's size and no shadows is reached one to one; one of no
/// known size, or a rectangle of none, unscaled from the rectangle's corner.
fn surface_point(
    at: (f64, f64),
    rect: (i64, i64, i64, i64),
    window: Option<(f64, f64, f64, f64)>,
) -> (f64, f64) {
    let (x, y, width, height) = rect;
    #[expect(
        clippy::cast_precision_loss,
        reason = "a window's corner and size are screen coordinates, far inside f64"
    )]
    let (x, y, width, height) = (x as f64, y as f64, width as f64, height as f64);
    let (left, top, along, down) = window
        .filter(|_| width > 0.0 && height > 0.0)
        .map_or((0.0, 0.0, 1.0, 1.0), |(left, top, w, h)| {
            (left, top, w / width, h / height)
        });
    (left + (at.0 - x) * along, top + (at.1 - y) * down)
}

/// The part of a surface the frame stretches to the window's rectangle, in
/// surface coordinates: the window geometry [`crate::frame::window_crop`]
/// draws, or else the whole surface -- the viewport's destination if it has
/// one, else its buffer's size over the buffer's scale. `None` for a surface
/// with no buffer, which the pointer then reaches unscaled.
fn window_part(slot: &Slot, surface: ObjectId) -> Option<(f64, f64, f64, f64)> {
    let client = slot.client();
    if let Some(crop) = crate::frame::window_crop(client, surface) {
        return Some(crop.in_surface());
    }
    let state = &client.surface(surface)?.current;
    if let Some((width, height)) = state.viewport_size {
        return Some((0.0, 0.0, f64::from(width), f64::from(height)));
    }
    let buffer = client.buffer(state.buffer?)?;
    let scale = f64::from(state.scale.max(1));
    Some((
        0.0,
        0.0,
        f64::from(buffer.width) / scale,
        f64::from(buffer.height) / scale,
    ))
}

/// The popups -- menus, dropdowns, tooltips -- in front of everything, the
/// newest first, since that is the order they are drawn in from the top;
/// then the layer surfaces above the windows (`overlay`, then `top`: a
/// bar), the windows, and the layer surfaces under them (`bottom`, then
/// `background`), as Hyprland stacks them for input.
///
/// A popup is a surface of its own over its window, and a click on a menu
/// item belongs to the menu: sent to the window under it, at the window's
/// coordinates, it was a click outside the menu, which closed it and did
/// nothing else, or clicked whatever was beneath. Chrome's menus, its
/// address bar's suggestions and its dropdowns are all popups. A bar is a
/// layer surface, and one the pointer never reached was a bar nobody could
/// click or hover.
fn with_popups(
    windows: Vec<Placement>,
    slots: &[Slot],
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
) -> Vec<Placement> {
    let mut out: Vec<Placement> = crate::state::placed_popups(slots, state, sources)
        .into_iter()
        .rev()
        .map(|popup| Placement {
            window: None,
            client: popup.client,
            surface: popup.surface,
            rect: (
                popup.rect.x,
                popup.rect.y,
                popup.rect.width,
                popup.rect.height,
            ),
        })
        .collect();
    let layer = |wanted: compositor_server::Layer| -> Vec<Placement> {
        let mut found: Vec<Placement> = slots
            .iter()
            .enumerate()
            .flat_map(|(client, slot)| {
                slot.placed_layers()
                    .filter(|(_, _, layer)| *layer == wanted)
                    .map(move |(surface, rect, _)| Placement {
                        window: None,
                        client,
                        surface,
                        rect: (rect.x, rect.y, rect.width, rect.height),
                    })
            })
            .collect();
        // The last made is drawn on top, so the pointer finds it first.
        found.reverse();
        found
    };
    out.extend(layer(compositor_server::Layer::Overlay));
    out.extend(layer(compositor_server::Layer::Top));
    out.extend(windows);
    out.extend(layer(compositor_server::Layer::Bottom));
    out.extend(layer(compositor_server::Layer::Background));
    out
}

/// A pixel position as Wayland's 24.8 fixed point.
fn fixed(value: f64) -> Fixed {
    Fixed::from_f64(value)
}

/// One held `(client, surface)` pair, moved to where its client is now or
/// forgotten if that client has gone.
fn moved(held: &mut Option<(usize, ObjectId)>, places: &[Option<usize>]) {
    if let Some((client, surface)) = *held {
        *held = places
            .get(client)
            .copied()
            .flatten()
            .map(|at| (at, surface));
    }
}

#[cfg(test)]
mod tests {
    use super::{ObjectId, Placement, WindowId, surface_point};

    /// Chrome's surface is its window and 10 pixels of shadow all round;
    /// the frame draws only the window into the tile, so the pointer at the
    /// tile's corner is on the window's corner, 10 pixels into the surface,
    /// and one pixel across the tile is one across the window.
    #[test]
    fn the_pointer_is_on_the_window_its_client_drew_inside_its_shadows() {
        let tile = (21, 21, 982, 726);
        let chrome = Some((10.0, 10.0, 982.0, 726.0));
        assert_eq!(surface_point((21.0, 21.0), tile, chrome), (10.0, 10.0));
        assert_eq!(surface_point((1002.0, 746.0), tile, chrome), (991.0, 735.0));
        assert_eq!(
            surface_point((512.5, 384.25), tile, chrome),
            (501.5, 373.25)
        );
    }

    /// A window part-way through an animation is stretched to a tile that is
    /// not its size, and the pointer is scaled back through the stretch.
    #[test]
    fn the_pointer_is_scaled_back_through_a_stretch() {
        let tile = (0, 0, 500, 400);
        let (x, y) = surface_point((250.0, 100.0), tile, Some((0.0, 0.0, 1000.0, 800.0)));
        assert!(
            (x - 500.0).abs() < 1e-9 && (y - 200.0).abs() < 1e-9,
            "({x}, {y})"
        );
        // No buffer yet, or a rectangle of nothing: unscaled, not a NaN.
        assert_eq!(surface_point((100.0, 50.0), tile, None), (100.0, 50.0));
        assert_eq!(
            surface_point((100.0, 50.0), (0, 0, 0, 0), Some((0.0, 0.0, 10.0, 10.0))),
            (100.0, 50.0)
        );
    }

    #[test]
    fn a_point_is_on_a_window_from_its_corner_to_before_its_far_edges() {
        // The DK1's portrait terminal, as `hyprctl clients` reported it.
        let terminal = Placement {
            window: Some(WindowId(1)),
            client: 0,
            surface: ObjectId(3),
            rect: (21, 21, 678, 613),
        };
        assert!(terminal.contains(470.0, 413.0), "where the pointer was");
        assert!(terminal.contains(21.0, 21.0), "the corner");
        assert!(terminal.contains(698.5, 633.5), "just inside the far edges");
        assert!(
            !terminal.contains(699.0, 400.0),
            "the right edge is outside"
        );
        assert!(
            !terminal.contains(400.0, 634.0),
            "the bottom edge is outside"
        );
        assert!(!terminal.contains(20.9, 400.0), "left of it");
    }
}
