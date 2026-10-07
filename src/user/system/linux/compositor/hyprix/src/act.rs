//! The dispatchers that are the compositor's rather than the tiling's.
//!
//! `src/user/system/linux/compositor/layout` answers everything that moves a window: it holds the
//! monitors, the workspaces and the tree, and `movefocus` means nothing
//! without them. The rest of Hyprland's one dispatcher table reaches past
//! the layout -- it starts a program, signals one, turns a screen off, moves
//! the pointer, writes a line on the event socket or ends the session -- and
//! that is what this module is.
//!
//! [`Around`] is everything such a dispatcher may touch. It is a struct and
//! not a dozen arguments because Hyprland's table is one function taking one
//! string and the whole compositor behind it, and this is that compositor.
//!
//! # What is not here
//!
//! `forceidle` and `releaseinputcapture` are answered with a line saying
//! they do nothing: this compositor has neither an idle protocol nor input
//! capture, and a dispatcher that quietly did nothing would be worse than
//! one that says so. `toggleswallow` keeps its flag and says the same:
//! swallowing a terminal is not implemented.

use std::collections::BTreeMap;

use compositor_layout::{Corner, Move, State, WindowId};

use crate::frame::Source;
use crate::seat::{Action, Seat};
use crate::select::Selector;
use crate::state::Slot;

/// A drag with the mouse: `bindm = SUPER, mouse:272, movewindow`, a press
/// on a window's border with `general:resize_on_border` on, or a client's
/// own `xdg_toplevel.move` or `resize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drag {
    /// Which window is being dragged.
    pub window: WindowId,
    /// Whether it is being resized rather than moved.
    pub resizing: bool,
    /// Which edge of the window is being pulled, for a resize that grabbed
    /// one. A `bindm` grabs no edge and resizes about the window's origin,
    /// which is [`Corner::NONE`].
    pub corner: Corner,
    /// What started it, which is what says how it ends.
    pub began: Began,
    /// Where the pointer was when it started, and where it was last seen: a
    /// drag is carried on by the distance since the last look.
    pub from: (i64, i64),
    /// Whether the window was tiled when the drag began and was lifted out
    /// of the tiling for it: it is dropped back in where the drag ends
    /// (`compositor_layout::State::drop_window`).
    pub lifted: bool,
}

/// What started a [`Drag`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Began {
    /// A `bindm`, which ends when its own key goes up.
    Bind,
    /// A press on a border, which ends when the left button comes up. The
    /// client saw neither, so the release is swallowed too.
    Border,
    /// The client, from a press it was sent on its title bar or edge. It
    /// ends when the last button comes up, and the release goes on to the
    /// client: it saw the press, and an X client's server keeps the button
    /// grabbed until it sees the release.
    Client,
}

/// Everything a dispatcher may reach besides the layout.
pub struct Around<'a> {
    /// The connections.
    pub slots: &'a mut [Slot],
    /// Which client and surface each window's pixels come from.
    pub sources: &'a BTreeMap<WindowId, Source>,
    /// Where the Wayland socket is, for a program that is started.
    pub socket: &'a std::path::Path,
    /// What this compositor's instance is called, which is what a program
    /// it starts needs in `$HYPRLAND_INSTANCE_SIGNATURE` to find `hyprctl`'s
    /// socket.
    pub instance: Option<&'a str>,
    /// The keyboard and the pointer.
    pub seat: &'a mut Seat,
    /// The plugins, which are the table's last entry.
    pub plugins: &'a mut crate::plugins::Plugins,
    /// What a rule gave each window to be drawn with, which `setprop`
    /// changes by hand.
    pub rules: &'a mut crate::rules::Rules,
    /// The event socket, for `event`.
    pub events: &'a mut Option<crate::control::Events>,
    /// Whether each screen is turned off, by the name `dpms` names it.
    pub dpms: &'a mut BTreeMap<String, bool>,
    /// The screens' names and rectangles, for `dpms` and for
    /// `movecursortocorner`.
    pub screens: &'a [(String, compositor_layout::Rect)],
    /// Windows a client asked to have raised and that have not been looked
    /// at, which is what `focusurgentorlast` looks for.
    pub urgent: &'a mut Vec<WindowId>,
    /// A drag with the mouse, while one is going on.
    pub drag: &'a mut Option<Drag>,
    /// Actions a dispatcher caused, which the caller delivers.
    pub pending: &'a mut Vec<Action>,
    /// Set when a dispatcher asked the compositor to end.
    pub quit: &'a mut bool,
    /// Whether a terminal that opened a window is hidden: `toggleswallow`.
    pub swallow: &'a mut bool,
    /// How long `forceidle` is pretending the seat has gone unused, or
    /// `None` for the real clock.
    pub forced: &'a mut Option<std::time::Duration>,
    /// Which surface has the keyboard and which the pointer, so that a key
    /// sent to another window can be handed back afterwards.
    pub focus: &'a mut crate::deliver::Focus,
    /// The key that fired this dispatcher, when a key did: the evdev code
    /// and the modifiers held with it.
    pub trigger: Option<(u16, u32)>,
    /// Workspaces whose `on-created-empty:` command has already been run,
    /// so that going back to one does not start a second terminal.
    pub opened: &'a mut std::collections::BTreeSet<compositor_layout::WorkspaceId>,
    /// What to say.
    pub report: &'a mut dyn FnMut(&str),
}

impl core::fmt::Debug for Around<'_> {
    /// What a dispatcher changed, and not the parts of the compositor it
    /// borrowed to change them: a connection and a closure have nothing to
    /// print, and the loop's own report says what happened anyway.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Around")
            .field("screens", &self.screens)
            .field("dpms", &self.dpms)
            .field("urgent", &self.urgent)
            .field("drag", &self.drag)
            .field("quit", &self.quit)
            .field("swallow", &self.swallow)
            .finish_non_exhaustive()
    }
}

impl Around<'_> {
    /// Say something, the way the compositor says everything else.
    pub fn say(&mut self, what: &str) {
        (self.report)(what);
    }
}

/// Run `name` if it is one of the compositor's own dispatchers.
///
/// Gives `None` when the name belongs to the layout, so the caller hands it
/// on; `Some(changed)` when it was answered here, and whether the screen has
/// to be drawn again.
pub fn compositor(
    name: &str,
    argument: &str,
    state: &mut State,
    around: &mut Around<'_>,
) -> Option<bool> {
    match name {
        // Hyprland's `exec` goes through a shell and `execr` does not; this
        // compositor never went through one, so the two are the same call
        // and `execr` is the honest name for what both do.
        "exec" | "execr" => Some(start(argument, around)),
        "exit" => {
            *around.quit = true;
            around.say("hyprix: asked to end");
            Some(false)
        }
        "submap" => Some(submap(argument, around)),
        "forcerendererreload" => {
            // Nothing is cached between frames, so there is nothing to
            // throw away: drawing again is the whole of it.
            Some(true)
        }
        "event" => Some(event(argument, around)),
        "global" => Some(global(argument, around)),
        "dpms" => Some(dpms(argument, around)),
        "movecursor" => Some(move_cursor(argument, around)),
        "movecursortocorner" => Some(move_cursor_to_corner(argument, state, around)),
        "mouse" => Some(mouse(argument, state, around)),
        "forcekillactive" => Some(signal(argument_or(argument, "9"), None, state, around)),
        "signal" => Some(signal(argument, None, state, around)),
        "signalwindow" => {
            let (which, number) = argument.split_once(',')?;
            Some(signal(number.trim(), Some(which.trim()), state, around))
        }
        "setprop" => Some(set_prop(argument, state, around)),
        "pass" => Some(pass(argument, state, around)),
        "sendshortcut" => Some(send_shortcut(argument, None, state, around)),
        "sendkeystate" => Some(send_key_state(argument, state, around)),
        "focusurgentorlast" => Some(focus_urgent_or_last(state, around)),
        "toggleswallow" => {
            *around.swallow = !*around.swallow;
            around.say("hyprix: swallowing a terminal is not implemented");
            Some(false)
        }
        "forceidle" => Some(force_idle(argument, around)),
        "releaseinputcapture" => {
            around.say("hyprix: nothing has captured the input");
            Some(false)
        }
        _ => None,
    }
}

/// An empty argument means this instead, which is how `forcekillactive` is
/// `signal 9`.
fn argument_or<'a>(argument: &'a str, instead: &'a str) -> &'a str {
    if argument.trim().is_empty() {
        instead
    } else {
        argument
    }
}

/// `exec`: start a program with this compositor's socket in its
/// environment.
fn start(command: &str, around: &mut Around<'_>) -> bool {
    match crate::state::start(command, around.socket, around.instance) {
        Ok(pid) => around.say(&format!("hyprix: started {command} as {pid}")),
        Err(error) => {
            if let Some(line) = crate::state::not_started(command, &error) {
                around.say(&line);
            }
        }
    }
    false
}

/// `submap`: which set of binds is in force.
fn submap(name: &str, around: &mut Around<'_>) -> bool {
    match around.seat.enter_submap(name) {
        Ok(true) => {
            let name = around.seat.submap();
            if name.is_empty() {
                around.say("hyprix: the global keymap");
            } else {
                around.say(&format!("hyprix: submap {name}"));
            }
        }
        Ok(false) => {}
        Err(why) => around.say(&format!("hyprix: {why}")),
    }
    false
}

/// `event`: a line of a program's own on the event socket, which Hyprland
/// writes as `custom>>`.
fn event(data: &str, around: &mut Around<'_>) -> bool {
    match around.events.as_mut() {
        Some(events) => events.say(&format!("custom>>{data}\n")),
        None => around.say("hyprix: no event socket to write on"),
    }
    false
}

/// `global <app_id>:<id>`: a shortcut a program registered.
///
/// `hyprland-global-shortcuts-v1` is how a screen recorder or a
/// push-to-talk program has a key without reading the keyboard: it
/// registers a *name*, the person binds a key to `dispatch global <name>`,
/// and the program hears `pressed`. A plugin may have registered the same
/// name, and hears it too.
fn global(name: &str, around: &mut Around<'_>) -> bool {
    let at = now_monotonic();
    let mut heard = false;
    for slot in around.slots.iter_mut() {
        if slot.client_mut().fire_shortcut(name, at) {
            heard = true;
            let _ = slot.flush();
        }
    }
    heard |= around.plugins.dispatch("global", name);
    if !heard {
        around.say(&format!(
            "hyprix: nothing has registered the shortcut {name}"
        ));
    }
    false
}

/// The compositor's clock as these protocols carry it: seconds and
/// nanoseconds.
fn now_monotonic() -> (u64, u32) {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for clock_gettime to write.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    (
        u64::try_from(now.tv_sec).unwrap_or(0),
        u32::try_from(now.tv_nsec).unwrap_or(0),
    )
}

/// `dpms on|off|toggle [monitor]`: turn a screen off, or every screen.
fn dpms(argument: &str, around: &mut Around<'_>) -> bool {
    let mut words = argument.split_whitespace();
    let what = words.next().unwrap_or("");
    let named = words.next();
    let mut changed = false;
    for (name, _) in around.screens.iter() {
        if named.is_some_and(|wanted| wanted != name) {
            continue;
        }
        let now = around.dpms.get(name).copied().unwrap_or(false);
        // Hyprland reads anything that is not `on` or `toggle` as `off`.
        let off = match what {
            "on" => false,
            "toggle" => !now,
            _ => true,
        };
        if off != now {
            let _ = around.dpms.insert(name.clone(), off);
            changed = true;
        }
    }
    changed
}

/// `movecursor <x> <y>`: put the pointer somewhere.
fn move_cursor(argument: &str, around: &mut Around<'_>) -> bool {
    let mut numbers = argument.split_whitespace();
    let read = |text: Option<&str>| text.and_then(|text| text.parse::<f64>().ok());
    let (Some(x), Some(y)) = (read(numbers.next()), read(numbers.next())) else {
        around.say("hyprix: movecursor takes two numbers");
        return false;
    };
    around.pending.extend(around.seat.warp(x, y));
    true
}

/// `movecursortocorner <0-3>`: to a corner of the focused window, counting
/// anticlockwise from the bottom left, as Hyprland counts them.
fn move_cursor_to_corner(argument: &str, state: &State, around: &mut Around<'_>) -> bool {
    let Ok(corner) = argument.trim().parse::<u8>() else {
        around.say("hyprix: movecursortocorner takes a number from 0 to 3");
        return false;
    };
    // The focused window, or the focused screen when nothing is focused.
    let rect = state
        .focused_window()
        .and_then(|window| {
            state
                .layout()
                .iter()
                .flat_map(|output| output.windows.iter())
                .find(|placed| placed.window == window)
                .map(|placed| placed.rect)
        })
        .or_else(|| around.screens.first().map(|(_, rect)| *rect));
    let Some(rect) = rect else {
        return false;
    };
    // The last column and row inside the window, so the pointer lands on it
    // rather than one pixel past it.
    let (left, top) = (rect.x, rect.y);
    let (right, bottom) = (
        rect.right().saturating_sub(1),
        rect.bottom().saturating_sub(1),
    );
    let (x, y) = match corner {
        0 => (left, bottom),
        1 => (right, bottom),
        2 => (right, top),
        3 => (left, top),
        _ => {
            around.say("hyprix: movecursortocorner takes a number from 0 to 3");
            return false;
        }
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "a screen's pixels are far inside f64's exact range"
    )]
    around.pending.extend(around.seat.warp(x as f64, y as f64));
    true
}

/// `mouse +action` and `-action`: a drag begins and ends.
fn mouse(argument: &str, state: &mut State, around: &mut Around<'_>) -> bool {
    let (starting, action) = match argument.split_at_checked(1) {
        Some(("+", rest)) => (true, rest),
        Some(("-", rest)) => (false, rest),
        _ => {
            around.say("hyprix: mouse takes +action or -action");
            return false;
        }
    };
    if !starting {
        // A window lifted out of the tiling goes back in where the pointer
        // let go of it, which is Hyprland's `dragEnd`.
        let Some(held) = around.drag.take() else {
            return false;
        };
        if held.lifted {
            return state
                .drop_window(held.window, around.seat.pointer())
                .is_ok_and(|changes| !changes.is_empty());
        }
        return false;
    }
    let resizing = match action {
        "movewindow" => false,
        "resizewindow" => true,
        _ => {
            around.say(&format!("hyprix: there is no mouse action {action}"));
            return false;
        }
    };
    let Some(window) = state.focused_window() else {
        return false;
    };
    // A tiled window has no rectangle of its own to drag. Hyprland's drag
    // controller lifts a tiled one out of the tiling, floating at its own
    // size round the pointer, and drops it back in when the drag ends; a
    // resize floats it for good, as `togglefloating` would.
    let lifted = !resizing
        && !state.is_floating(window)
        && state
            .lift_window(window, around.seat.pointer())
            .is_ok_and(|_| state.is_floating(window));
    if !state.is_floating(window) {
        let _ = state.dispatch_str("togglefloating", "");
    }
    let (x, y) = around.seat.pointer();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the pointer is held inside the screen, which is far inside i64"
    )]
    let from = (x as i64, y as i64);
    *around.drag = Some(Drag {
        window,
        resizing,
        corner: Corner::NONE,
        began: Began::Bind,
        from,
        lifted,
    });
    true
}

/// The left button, which is what a border is grabbed with: `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;

/// Start or end a border drag, and give back the actions the clients should
/// still be told about.
///
/// `general:resize_on_border`: Hyprland's `processMouseDownNormal` hit-tests
/// the press against every window's border -- the ring around it, as wide as
/// `general:border_size + general:extend_border_grab_area` -- and resizes
/// instead of passing the press on. The hit test is
/// [`State::border_at`], because it is geometry and the layout holds the
/// rectangles; what is here is the part that is about buttons.
///
/// The press is swallowed only when it grabbed something, so a press on a
/// window's own pixels reaches the client as it always did. The release is
/// swallowed only when it ends a border drag: a `bindm` drag ends when its
/// bind's key goes up and its button is the client's business.
pub fn grab_border(
    actions: Vec<Action>,
    state: &State,
    seat: &Seat,
    drag: &mut Option<Drag>,
) -> Vec<Action> {
    let mut kept = Vec::with_capacity(actions.len());
    for action in actions {
        // A client's drag ends with the last button up, and the client is
        // told about the release.
        if let Action::Button { pressed: false, .. } = action
            && drag.is_some_and(|held| held.began == Began::Client)
            && !seat.buttons_held()
        {
            *drag = None;
            kept.push(action);
            continue;
        }
        let Action::Button {
            button: BTN_LEFT,
            pressed,
        } = action
        else {
            kept.push(action);
            continue;
        };
        if pressed {
            if drag.is_some() {
                kept.push(action);
                continue;
            }
            let (x, y) = seat.pointer();
            let Some((window, corner)) = state.border_at((x, y)) else {
                kept.push(action);
                continue;
            };
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the pointer is held inside the screen, which is far inside i64"
            )]
            let from = (x as i64, y as i64);
            *drag = Some(Drag {
                window,
                resizing: true,
                corner,
                began: Began::Border,
                from,
                lifted: false,
            });
        } else if drag.is_some_and(|held| held.began == Began::Border) {
            *drag = None;
        } else {
            kept.push(action);
        }
    }
    kept
}

/// Start the drag a client asked for with `xdg_toplevel.move` (`edges` 0)
/// or `resize`, and give whether one started.
///
/// Only a floating window is dragged: a tiled one has no rectangle of its
/// own, and floating it because its title bar was pressed would take it
/// out of the tiling on every click. Nor does one start without a button
/// held, as nothing would end it, nor while another drag is on --
/// Hyprland's `onXDGMoveRequest` asks the same of its drag controller.
pub fn client_drag(
    window: WindowId,
    edges: u32,
    state: &State,
    seat: &Seat,
    drag: &mut Option<Drag>,
) -> bool {
    if drag.is_some() || !seat.buttons_held() || !state.is_floating(window) {
        return false;
    }
    let fullscreen = state
        .workspace_of(window)
        .and_then(|workspace| state.fullscreen(workspace))
        .is_some_and(|(id, _)| id == window);
    if fullscreen {
        return false;
    }
    // `xdg_toplevel.resize_edge`'s bits: top 1, bottom 2, left 4, right 8.
    let corner = Corner {
        top: edges & 1 != 0,
        bottom: edges & 2 != 0,
        left: edges & 4 != 0,
        right: edges & 8 != 0,
    };
    let (x, y) = seat.pointer();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the pointer is held inside the screen, which is far inside i64"
    )]
    let from = (x as i64, y as i64);
    *drag = Some(Drag {
        window,
        resizing: edges != 0,
        corner,
        began: Began::Client,
        from,
        lifted: false,
    });
    true
}

/// Carry a drag on: the pointer has moved to `(x, y)`.
///
/// Gives whether the window moved, and is called from the loop rather than
/// from a dispatcher: a drag is one bind and then every pointer movement
/// until the button comes back up.
pub fn dragged(drag: &mut Drag, state: &mut State, (x, y): (i64, i64)) -> bool {
    let by = Move {
        x: x.saturating_sub(drag.from.0),
        y: y.saturating_sub(drag.from.1),
        exact: false,
    };
    if by.x == 0 && by.y == 0 {
        return false;
    }
    drag.from = (x, y);
    // The drag entry points, not the dispatchers': a hand dragging a window
    // is helped to an edge by `general:snap:*` and a dispatcher asked for a
    // number of pixels and means it.
    // A window lifted out of the tiling is not snapped: it is going back
    // into the tiling, and Hyprland's `mouseMove` snaps only when it is not
    // `m_draggingTiled`.
    let moved = if drag.resizing {
        state.drag_resize_window_pixel(drag.window, &by, drag.corner)
    } else if drag.lifted {
        state.move_window_pixel(drag.window, &by)
    } else {
        state.drag_window_pixel(drag.window, &by)
    };
    moved.is_ok_and(|changes| !changes.is_empty())
}

/// `pass <window>`: send the key that fired this bind on to another window.
///
/// The one use is a bind that both does something and lets the key through
/// to a particular window -- `bind = SUPER, P, pass, class:^(mpv)$` is how
/// a media key reaches a player that is not focused. There is nothing to
/// pass when no key fired the bind, which is what `hyprctl dispatch pass`
/// is.
fn pass(which: &str, state: &State, around: &mut Around<'_>) -> bool {
    let Some((code, mods)) = around.trigger else {
        around.say("hyprix: pass has no key to send: nothing fired it");
        return false;
    };
    let Some(window) = pick(Some(which), state, around) else {
        return false;
    };
    press(window, code, mods, Press::Tap, around);
    false
}

/// How `sendkeystate` asks for a key to be sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Press {
    /// Down and then up, which is what `pass` and `sendshortcut` send.
    Tap,
    /// Down and held.
    Down,
    /// Down twice, which is what a repeat looks like on the wire.
    Repeat,
    /// Up.
    Up,
}

/// `sendshortcut <mods>,<key>,<window>`: make up a key and send it.
///
/// The window may be empty, which is the focused one. The key is a name
/// XKB knows, `code:<n>` for a raw keycode, or a bare number above nine,
/// which is Hyprland's own reading.
fn send_shortcut(
    argument: &str,
    how: Option<Press>,
    state: &State,
    around: &mut Around<'_>,
) -> bool {
    let fields: Vec<&str> = argument.splitn(3, ',').collect();
    let [mods, key, which] = fields.as_slice() else {
        around.say("hyprix: sendshortcut takes modifiers, a key and a window");
        return false;
    };
    let Some(code) = keycode(around.seat.layout(), key.trim()) else {
        around.say(&format!("hyprix: sendshortcut: {key} is not a key"));
        return false;
    };
    let mods = compositor_config::Mods::parse(mods.trim()).0;
    let Some(window) = pick(Some(which.trim()), state, around) else {
        return false;
    };
    press(window, code, mods, how.unwrap_or(Press::Tap), around);
    false
}

/// `sendkeystate <mods>,<key>,<state>,<window>`: the same, with the half of
/// the press the caller chose.
fn send_key_state(argument: &str, state: &State, around: &mut Around<'_>) -> bool {
    let fields: Vec<&str> = argument.splitn(4, ',').collect();
    let [mods, key, wanted, which] = fields.as_slice() else {
        around.say("hyprix: sendkeystate takes modifiers, a key, a state and a window");
        return false;
    };
    let how = match wanted.trim() {
        "down" => Press::Down,
        "repeat" => Press::Repeat,
        "up" => Press::Up,
        _ => {
            around.say("hyprix: sendkeystate's state is down, repeat or up");
            return false;
        }
    };
    let rejoined = format!("{mods},{key},{which}");
    send_shortcut(&rejoined, Some(how), state, around)
}

/// A key by the name Hyprland lets a bind write it.
fn keycode(layout: &'static compositor_xkb::generated::Layout, text: &str) -> Option<u16> {
    if let Some(number) = text.strip_prefix("code:") {
        // `code:NN` is XKB's numbering, which is eight above evdev's.
        return u16::try_from(number.trim().parse::<u32>().ok()?.checked_sub(8)?).ok();
    }
    if let Ok(number) = text.parse::<u32>()
        && number > 9
    {
        return u16::try_from(number).ok();
    }
    layout.code_of(text)
}

/// Send one key to one window, whatever has the keyboard.
///
/// The window is handed the keyboard for the length of the key and then it
/// is handed back, which is what Hyprland's `Actions::pass` does: a
/// `wl_keyboard` only ever has one surface, so there is no other way to
/// send a key to a window that is not focused.
fn press(window: WindowId, code: u16, mods: u32, how: Press, around: &mut Around<'_>) {
    let Some(source) = around.sources.get(&window).copied() else {
        return;
    };
    let focused = around.focus.keyboard();
    let borrowed = focused != Some((source.client, source.surface));
    let held = around.seat.modifiers();
    let Some(slot) = around.slots.get_mut(source.client) else {
        return;
    };
    let with = compositor_xkb::Modifiers {
        depressed: mods,
        ..held
    };
    if borrowed {
        let _ = slot.client_mut().keyboard_enter(source.surface, &[], with);
    } else {
        let _ = slot.client_mut().keyboard_modifiers(with);
    }
    // The clock a key carries is the compositor's own milliseconds, which
    // is what every other key it sends carries.
    let time = u32::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            & u128::from(u32::MAX),
    )
    .unwrap_or(0);
    let halves: &[bool] = match how {
        Press::Tap => &[true, false],
        Press::Down => &[true],
        Press::Repeat => &[true, true],
        Press::Up => &[false],
    };
    for pressed in halves {
        let _ = slot.client_mut().keyboard_key(time, code, *pressed);
    }
    if borrowed {
        let _ = slot.client_mut().keyboard_leave(source.surface);
        // The focus has not moved, so the loop would see nothing to do;
        // forgetting it makes the next pass give the real window its
        // keyboard back.
        around.focus.forget_keyboard();
    } else {
        let _ = slot.client_mut().keyboard_modifiers(held);
    }
    let _ = slot.flush();
}

/// `forceidle <seconds>`: pretend the seat has gone that long unused.
///
/// Every `ext_idle_notification_v1` whose timeout is shorter than that is
/// told the seat went idle, which is how a person tests a screen locker
/// without waiting ten minutes for it. Any real input undoes it, and so
/// does a duration of nothing.
fn force_idle(argument: &str, around: &mut Around<'_>) -> bool {
    let Ok(seconds) = argument.trim().parse::<f64>() else {
        around.say("hyprix: forceidle takes a number of seconds");
        return false;
    };
    *around.forced = (seconds > 0.0).then(|| std::time::Duration::from_secs_f64(seconds));
    match *around.forced {
        Some(held) => around.say(&format!("hyprix: idle for {} s", held.as_secs_f64())),
        None => around.say("hyprix: no longer idle"),
    }
    false
}

/// `signal`, `signalwindow` and `forcekillactive`: send a signal to the
/// process on the other end of a window's connection.
fn signal(number: &str, which: Option<&str>, state: &State, around: &mut Around<'_>) -> bool {
    let Ok(number) = number.trim().parse::<i32>() else {
        around.say("hyprix: a signal is a number");
        return false;
    };
    let Some(window) = pick(which, state, around) else {
        return false;
    };
    let Some(pid) = pid_of(window, around) else {
        around.say("hyprix: the window's connection has no process");
        return false;
    };
    #[expect(
        unsafe_code,
        reason = "AUDIT: kill is not in std; the pid came from SO_PEERCRED on this compositor's own socket"
    )]
    // SAFETY: `kill` takes two integers and touches no memory.
    let sent = unsafe { libc::kill(pid, number) };
    if sent < 0 {
        around.say(&format!(
            "hyprix: signal {number} to {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    false
}

/// `setprop <window> <property> <value>`: change what one window is drawn
/// with, the way a `windowrule` would have.
fn set_prop(argument: &str, state: &State, around: &mut Around<'_>) -> bool {
    let mut words = argument.split_whitespace();
    let (Some(which), Some(property)) = (words.next(), words.next()) else {
        around.say("hyprix: setprop takes a window, a property and a value");
        return false;
    };
    let value: String = words.collect::<Vec<&str>>().join(" ");
    let Some(window) = pick(Some(which), state, around) else {
        return false;
    };
    if around.rules.set_property(window, property, &value) {
        return true;
    }
    around.say(&format!("hyprix: there is no window property {property}"));
    false
}

/// `focusurgentorlast`: the window that asked to be raised, or else the one
/// focused before this.
fn focus_urgent_or_last(state: &mut State, around: &mut Around<'_>) -> bool {
    while let Some(window) = around.urgent.pop() {
        // A window that has gone is not one to focus.
        if state.focus_window(window).is_ok() {
            return true;
        }
    }
    state
        .dispatch_str("focuscurrentorlast", "")
        .is_ok_and(|changes| !changes.is_empty())
}

/// The window an expression names, or the focused one when there is no
/// expression.
pub fn pick(which: Option<&str>, state: &State, around: &mut Around<'_>) -> Option<WindowId> {
    let Some(which) = which.filter(|text| !text.is_empty()) else {
        return state.focused_window();
    };
    let seen = crate::state::as_seen(state, around.slots, around.sources);
    let window = Selector::parse(which)?.pick(&seen, state.focused_window());
    if window.is_none() {
        around.say(&format!("hyprix: no window answers {which:?}"));
    }
    window
}

/// The process on the other end of a window's connection.
fn pid_of(window: WindowId, around: &Around<'_>) -> Option<i32> {
    let source = around.sources.get(&window)?;
    let slot = around.slots.get(source.client)?;
    Some(slot.pid()).filter(|pid| *pid > 0)
}

#[cfg(test)]
mod tests {
    //! What a press on a border does, what a client's own drag does, and
    //! what each leaves for the client.

    use compositor_config::NoSources;
    use compositor_layout::{Corner, Monitor, MonitorId, Rect, State, WindowId};

    use super::{Action, Began, Drag, Seat, client_drag, dragged, grab_border};
    use crate::seat::Input;

    /// `BTN_LEFT`, as the seat reports it.
    const LEFT: u32 = 0x110;

    /// A compositor with `resize_on_border` on, one 1024x768 screen and two
    /// tiled windows: 0..512 and 512..1024, both the full height.
    fn ready(text: &str) -> (State, Seat) {
        let config = compositor_config::parse("test", text, &mut NoSources).config;
        let mut state = State::from_config(&config);
        let _changes = state
            .add_monitor(Monitor {
                scale: 1.0,
                transform: Default::default(),
                name: "Virtual-1".to_owned(),
                id: MonitorId(1),
                rect: Rect::new(0, 0, 1024, 768),
                reserved: compositor_layout::Gaps::all(0),
                description: String::new(),
                made: <(String, String, String)>::default(),
            })
            .unwrap();
        for id in [1, 2] {
            let _changes = state.open_window(WindowId(id)).unwrap();
        }
        (state, Seat::new(&config, 1024, 768))
    }

    const ON: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
                      general:resize_on_border = true\ngeneral:extend_border_grab_area = 10\n";

    fn pressed(down: bool) -> Action {
        Action::Button {
            button: LEFT,
            pressed: down,
        }
    }

    /// A press on the ring around a window starts a resize and is swallowed;
    /// the release that ends it is swallowed too, so the client is never
    /// told about half a click.
    #[test]
    fn a_press_on_a_border_grabs_it_and_never_reaches_the_client() {
        let (state, mut seat) = ready(ON);
        // Just inside window 2's left edge's grab ring, and outside window 2.
        let _moved = seat.warp(508.0, 400.0);
        let mut drag = None;

        let kept = grab_border(vec![pressed(true)], &state, &seat, &mut drag);
        assert_eq!(kept, [], "the press was swallowed");
        assert_eq!(
            drag,
            Some(Drag {
                window: WindowId(2),
                resizing: true,
                corner: Corner {
                    left: true,
                    ..Corner::NONE
                },
                began: Began::Border,
                from: (508, 400),
                lifted: false,
            })
        );

        let kept = grab_border(vec![pressed(false)], &state, &seat, &mut drag);
        assert_eq!(kept, [], "and so was the release that ended it");
        assert_eq!(drag, None);
    }

    /// A press on a window's own pixels is the client's, and leaves no drag
    /// behind.
    #[test]
    fn a_press_inside_a_window_reaches_the_client() {
        let (state, mut seat) = ready(ON);
        let _moved = seat.warp(200.0, 400.0);
        let mut drag = None;

        let kept = grab_border(vec![pressed(true)], &state, &seat, &mut drag);
        assert_eq!(kept, [pressed(true)]);
        assert_eq!(drag, None);

        // And the release, with no border drag to end, goes through as well.
        let kept = grab_border(vec![pressed(false)], &state, &seat, &mut drag);
        assert_eq!(kept, [pressed(false)]);
    }

    /// With the option off there is no ring at all, which is Hyprland's
    /// default and what every other boot of this compositor has had.
    #[test]
    fn a_border_is_not_grabbed_when_the_option_is_off() {
        let (state, mut seat) = ready("general:gaps_in = 0\ngeneral:gaps_out = 0\n");
        let _moved = seat.warp(508.0, 400.0);
        let mut drag = None;
        let kept = grab_border(vec![pressed(true)], &state, &seat, &mut drag);
        assert_eq!(kept, [pressed(true)]);
        assert_eq!(drag, None);
    }

    /// The seat with the left button down, as the client's press left it.
    fn press(seat: &mut Seat, down: bool) -> Vec<Action> {
        seat.input(Input::Button {
            button: LEFT,
            pressed: down,
        })
    }

    /// A floating window's own `move` follows the pointer until the button
    /// comes up, and the release reaches the client, which saw the press.
    #[test]
    fn a_floating_window_is_moved_by_its_own_request_until_the_release() {
        let (mut state, mut seat) = ready(ON);
        let _ = state.focus_window(WindowId(2)).unwrap();
        let _ = state.dispatch_str("setfloating", "").unwrap();
        let _moved = seat.warp(700.0, 300.0);
        let _ = press(&mut seat, true);
        let mut drag = None;

        assert!(client_drag(WindowId(2), 0, &state, &seat, &mut drag));
        assert_eq!(
            drag.map(|held| (held.began, held.resizing, held.from)),
            Some((Began::Client, false, (700, 300)))
        );
        assert!(dragged(drag.as_mut().unwrap(), &mut state, (740, 320)));

        let up = press(&mut seat, false);
        let kept = grab_border(up.clone(), &state, &seat, &mut drag);
        assert_eq!(kept, up, "the release went on to the client");
        assert_eq!(drag, None);
    }

    /// A `resize` pulls the edges it names.
    #[test]
    fn a_resize_request_pulls_its_edges() {
        let (mut state, mut seat) = ready(ON);
        let _ = state.focus_window(WindowId(2)).unwrap();
        let _ = state.dispatch_str("setfloating", "").unwrap();
        let _ = press(&mut seat, true);
        let mut drag = None;
        // `bottom_right`.
        assert!(client_drag(WindowId(2), 10, &state, &seat, &mut drag));
        let held = drag.unwrap();
        assert!(held.resizing);
        assert_eq!(
            held.corner,
            Corner {
                right: true,
                bottom: true,
                ..Corner::NONE
            }
        );
    }

    /// A tiled window, or a request with no button held, starts nothing.
    #[test]
    fn a_tiled_window_or_a_released_button_starts_no_drag() {
        let (state, mut seat) = ready(ON);
        let mut drag = None;
        let _ = press(&mut seat, true);
        assert!(!client_drag(WindowId(1), 0, &state, &seat, &mut drag));
        let _ = press(&mut seat, false);
        assert!(!client_drag(WindowId(2), 0, &state, &seat, &mut drag));
        assert_eq!(drag, None);
    }
}
