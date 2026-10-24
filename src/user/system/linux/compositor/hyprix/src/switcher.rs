//! The window switcher: `ALT+TAB` as GNOME has it (`docs/DESKTOP.md` §1.2).
//!
//! `switcher next` (Ferrix light binds it to `ALT+Tab` and `SUPER+Tab`)
//! shows every window -- minimized ones too, dimmed -- in a row across the
//! focused monitor, most recently focused first, with the one before the
//! focused window picked. Each further `switcher next` or `switcher prev`
//! picks the one after or before it; letting go of the modifiers the bind
//! was pressed with, `Return` or a click focuses the picked window -- a
//! minimized one comes back -- and closes the row; `Escape` closes it and
//! changes nothing.
//!
//! Hyprland's `cyclenext` walks one workspace's windows and never a
//! minimized one, since minimized windows wait on a special workspace, so a
//! window minimized by mistake had no key back. This is the overview's
//! drawing put to that use: the windows keep their places, the clients are
//! told nothing, and only the frame is drawn from [`outputs`].

use compositor_layout::{MonitorLayout, Placed, Rect, State, WindowId, WorkspaceId};

use crate::seat::Action;

/// Whether the switcher is up, what it lists and which one is picked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Switcher {
    /// Whether it is shown.
    pub on: bool,
    /// The windows it lists, most recently focused first.
    pub order: Vec<WindowId>,
    /// The picked one's place in `order`.
    pub at: usize,
    /// The modifiers held with the key that opened it; letting all of them
    /// go picks. Zero when it was opened otherwise -- `hyprctl dispatch` --
    /// and then only `Return`, a click or `Escape` ends it.
    pub held: u32,
    /// Keys whose press it took, so that their release is taken too: the
    /// `Return` that picked a window would otherwise reach that window as a
    /// release it never saw pressed.
    pub eaten: Vec<u16>,
}

impl Switcher {
    /// The picked window.
    #[must_use]
    pub fn picked(&self) -> Option<WindowId> {
        self.order.get(self.at).copied()
    }
}

/// The space between tiles, and round the row, in logical pixels.
const GAP: i64 = 24;

/// How much of the usable height the row may take.
const ROW: f64 = 0.3;

/// How wide the picked tile's outline is.
const HIGHLIGHT: i64 = 3;

/// `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;

/// `KEY_ESC`.
const KEY_ESC: u16 = 1;

/// `KEY_ENTER` and `KEY_KPENTER`.
const PICK: [u16; 2] = [28, 96];

/// `KEY_UP` and `KEY_LEFT`: the one before.
const BACK: [u16; 2] = [103, 105];

/// `KEY_TAB`, `KEY_RIGHT` and `KEY_DOWN`: the one after.
const ON: [u16; 3] = [15, 106, 108];

/// Every window a person could switch to, most recently focused first:
/// those on an ordinary workspace and the minimized ones, but not a
/// scratchpad's, which `togglespecialworkspace` shows and hides.
#[must_use]
pub fn windows(state: &State) -> Vec<WindowId> {
    let minimized: Vec<WindowId> = state
        .minimized_windows()
        .into_iter()
        .map(|(window, _)| window)
        .collect();
    let wanted = |window: &WindowId| {
        minimized.contains(window)
            || state
                .workspace_of(*window)
                .is_some_and(|workspace| workspace.0 > 0)
    };
    let mut order: Vec<WindowId> = state
        .windows_in_focus_order()
        .into_iter()
        .filter(|window| state.workspace_of(*window).is_some())
        .filter(wanted)
        .collect();
    // A window never focused is in no history: after the rest, in the order
    // the layout lists it.
    for listed in state.listing() {
        let window = listed.placed.window;
        if !order.contains(&window) && wanted(&window) {
            order.push(window);
        }
    }
    order
}

/// `switcher next|prev|pick|cancel`, with the modifiers held with the key
/// that ran it and those held now. Gives whether anything changed.
///
/// Modifiers let go of by now -- a quick `ALT+Tab`, its release read in the
/// same pass as the `Tab` -- pick at once, as the release would have.
pub fn dispatch(
    argument: &str,
    (held, now): (u32, u32),
    switcher: &mut Switcher,
    state: &mut State,
) -> bool {
    let changed = run(argument, held, switcher, state);
    if switcher.on && switcher.held != 0 && now & switcher.held == 0 {
        return pick(switcher, state) || changed;
    }
    changed
}

fn run(argument: &str, held: u32, switcher: &mut Switcher, state: &mut State) -> bool {
    match argument.trim() {
        "pick" | "commit" => pick(switcher, state),
        "cancel" | "off" | "close" => close(switcher),
        which => {
            let back = matches!(which, "prev" | "previous" | "back" | "b");
            if switcher.on {
                step(switcher, back);
                return true;
            }
            let order = windows(state);
            if order.is_empty() {
                return false;
            }
            let count = order.len();
            // The one before the focused window, as GNOME starts: one
            // `ALT+Tab` goes back to where a person just was.
            let first_is_focused = order.first().copied() == state.focused_window();
            let at = if back {
                count - 1
            } else if first_is_focused && count > 1 {
                1
            } else {
                0
            };
            *switcher = Switcher {
                on: true,
                order,
                at,
                held,
                eaten: std::mem::take(&mut switcher.eaten),
            };
            true
        }
    }
}

/// Pick the window after the picked one, or before it, wrapping.
fn step(switcher: &mut Switcher, back: bool) {
    let count = switcher.order.len();
    if count == 0 {
        return;
    }
    switcher.at = if back {
        switcher.at.checked_sub(1).unwrap_or(count - 1)
    } else {
        (switcher.at + 1) % count
    };
}

/// Focus the picked window, bringing a minimized one back, and close.
fn pick(switcher: &mut Switcher, state: &mut State) -> bool {
    if !switcher.on {
        return false;
    }
    if let Some(window) = switcher.picked() {
        let _ = state.focus_window(window);
    }
    close(switcher)
}

/// Close it and change nothing else.
fn close(switcher: &mut Switcher) -> bool {
    let was = switcher.on;
    switcher.on = false;
    switcher.order.clear();
    switcher.at = 0;
    switcher.held = 0;
    was
}

/// Where each listed window is drawn: a row across the middle of the
/// focused monitor's usable area, each window scaled to fit its cell
/// keeping its shape and never enlarged.
#[must_use]
pub fn tiles(state: &State, switcher: &Switcher) -> Vec<(WindowId, Rect)> {
    let Some(monitor) = state
        .focused_monitor()
        .and_then(|id| state.monitors().find(|monitor| monitor.id == id))
        .or_else(|| state.monitors().next())
    else {
        return Vec::new();
    };
    let whole = monitor.rect;
    let reserved = monitor.reserved;
    let usable = Rect::new(
        whole.x.saturating_add(reserved.left),
        whole.y.saturating_add(reserved.top),
        whole
            .width
            .saturating_sub(reserved.left)
            .saturating_sub(reserved.right),
        whole
            .height
            .saturating_sub(reserved.top)
            .saturating_sub(reserved.bottom),
    );
    let listing = state.listing();
    let sizes: Vec<(WindowId, (i64, i64))> = switcher
        .order
        .iter()
        .filter_map(|window| {
            listing
                .iter()
                .find(|listed| listed.placed.window == *window)
                .map(|listed| {
                    (
                        *window,
                        (listed.placed.rect.width, listed.placed.rect.height),
                    )
                })
        })
        .collect();
    let count = i64::try_from(sizes.len()).unwrap_or(i64::MAX);
    if count == 0 {
        return Vec::new();
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "a row is a fraction of a screen's height"
    )]
    let height = (usable.height as f64 * ROW).round() as i64;
    let inner = usable.width.saturating_sub(GAP.saturating_mul(4));
    let cell_width = inner.saturating_sub(GAP.saturating_mul(count.saturating_sub(1))) / count;
    // No wider than the screen's own shape at the row's height, so that two
    // windows are not each half the screen wide.
    let cell_width = cell_width
        .min(height.saturating_mul(whole.width) / whole.height.max(1))
        .max(1);
    let row = count
        .saturating_mul(cell_width)
        .saturating_add(GAP.saturating_mul(count.saturating_sub(1)));
    let left = usable
        .x
        .saturating_add(usable.width.saturating_sub(row) / 2);
    let top = usable
        .y
        .saturating_add(usable.height.saturating_sub(height) / 2);
    sizes
        .into_iter()
        .zip(0_i64..)
        .map(|((window, size), index)| {
            let cell = Rect::new(
                left.saturating_add(index.saturating_mul(cell_width.saturating_add(GAP))),
                top,
                cell_width,
                height,
            );
            (window, crate::overview::fit(cell, size))
        })
        .collect()
}

/// What each monitor draws while the switcher is up: the row on the
/// focused monitor, the picked window outlined; the others as the layout
/// has them, less the listed windows, which a monitor draws only once.
#[must_use]
pub fn outputs(state: &State, switcher: &Switcher) -> Vec<MonitorLayout> {
    let tiles = tiles(state, switcher);
    let shown_on = state
        .focused_monitor()
        .or_else(|| state.monitors().next().map(|monitor| monitor.id));
    let picked = switcher.picked();
    state
        .layout()
        .into_iter()
        .map(|output| {
            let windows = if Some(output.monitor) == shown_on {
                tiles
                    .iter()
                    .map(|&(window, rect)| {
                        let lit = picked == Some(window);
                        Placed {
                            window,
                            rect,
                            border: if lit { HIGHLIGHT } else { 0 },
                            focused: lit,
                            floating: true,
                            fullscreen: false,
                        }
                    })
                    .collect()
            } else {
                output
                    .windows
                    .iter()
                    .filter(|placed| !switcher.order.contains(&placed.window))
                    .copied()
                    .collect()
            };
            MonitorLayout { windows, ..output }
        })
        .collect()
}

/// The windows drawn dimmed while it is up: the minimized ones.
#[must_use]
pub fn dimmed(state: &State, switcher: &Switcher) -> Vec<WindowId> {
    let minimized: Vec<(WindowId, WorkspaceId)> = state.minimized_windows();
    switcher
        .order
        .iter()
        .copied()
        .filter(|window| minimized.iter().any(|(held, _)| held == window))
        .collect()
}

/// Whether `code` is a modifier's key, which goes on to the client whatever
/// the switcher does: its press went there before the switcher opened.
const fn is_modifier(code: u16) -> bool {
    matches!(code, 29 | 97 | 42 | 54 | 56 | 100 | 125 | 126)
}

/// Take the input while the switcher is up. Letting go of the modifiers it
/// was opened with picks, as does `Return` or a click on a tile; `Escape`
/// closes; the arrow keys and `Tab` move the pick; the pointer over a tile
/// picks it. Binds still run, so the `Tab` of `ALT+Tab` moves it on.
/// Gives the actions left over and whether the switcher or the layout
/// changed.
pub fn take_input(
    actions: Vec<Action>,
    switcher: &mut Switcher,
    state: &mut State,
    pointer: (f64, f64),
) -> (Vec<Action>, bool) {
    if !switcher.on {
        if switcher.eaten.is_empty() {
            return (actions, false);
        }
        let kept = actions
            .into_iter()
            .filter(|action| match action {
                Action::Key {
                    code,
                    pressed: false,
                } => {
                    let at = switcher.eaten.iter().position(|held| held == code);
                    at.map(|at| switcher.eaten.remove(at)).is_none()
                }
                _ => true,
            })
            .collect();
        return (kept, false);
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the pointer is held inside the screen, which is far inside i64"
    )]
    let at = (pointer.0 as i64, pointer.1 as i64);
    let under = |state: &State, switcher: &Switcher| {
        tiles(state, switcher)
            .into_iter()
            .find(|(_, rect)| {
                at.0 >= rect.x && at.0 < rect.right() && at.1 >= rect.y && at.1 < rect.bottom()
            })
            .and_then(|(window, _)| switcher.order.iter().position(|held| *held == window))
    };
    let mut changed = false;
    let mut kept = Vec::new();
    for action in actions {
        if !switcher.on {
            kept.push(action);
            continue;
        }
        match action {
            Action::Modifiers(modifiers) => {
                let let_go = switcher.held != 0 && modifiers.depressed & switcher.held == 0;
                kept.push(action);
                if let_go {
                    changed |= pick(switcher, state);
                }
            }
            Action::Pointer { .. } => {
                if let Some(index) = under(state, switcher)
                    && index != switcher.at
                {
                    switcher.at = index;
                    changed = true;
                }
            }
            Action::Button {
                button: BTN_LEFT,
                pressed: true,
            } => {
                if let Some(index) = under(state, switcher) {
                    switcher.at = index;
                    changed |= pick(switcher, state);
                } else {
                    changed |= close(switcher);
                }
            }
            Action::Key { code, pressed } if is_modifier(code) => {
                kept.push(Action::Key { code, pressed });
            }
            Action::Key {
                code,
                pressed: true,
            } => {
                let acts = code == KEY_ESC
                    || PICK.contains(&code)
                    || BACK.contains(&code)
                    || ON.contains(&code);
                if acts && !switcher.eaten.contains(&code) {
                    switcher.eaten.push(code);
                }
                if code == KEY_ESC {
                    changed |= close(switcher);
                } else if PICK.contains(&code) {
                    changed |= pick(switcher, state);
                } else if BACK.contains(&code) || ON.contains(&code) {
                    step(switcher, BACK.contains(&code));
                    changed = true;
                }
            }
            Action::Key {
                code,
                pressed: false,
            } => {
                if let Some(at) = switcher.eaten.iter().position(|held| *held == code) {
                    let _ = switcher.eaten.remove(at);
                }
            }
            Action::Dispatch { .. } => kept.push(action),
            _ => {}
        }
    }
    (kept, changed)
}

#[cfg(test)]
mod tests {
    use compositor_config::NoSources;
    use compositor_layout::{Monitor, MonitorId, Rect, State, WindowId};
    use compositor_xkb::Modifiers;

    use super::{Switcher, dimmed, dispatch, outputs, take_input, tiles, windows};
    use crate::seat::Action;

    /// `MOD1`, which is Alt.
    const ALT: u32 = 8;

    /// Three floating windows on one 1280x800 monitor, 3 focused last,
    /// then 2 minimized.
    fn desktop() -> State {
        let text = "general:layout = floating\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n";
        let config = compositor_config::parse("test", text, &mut NoSources).config;
        let mut state = State::from_config(&config);
        let _ = state
            .add_monitor(Monitor {
                scale: 1.0,
                transform: Default::default(),
                name: "Virtual-1".to_owned(),
                id: MonitorId(1),
                rect: Rect::new(0, 0, 1280, 800),
                reserved: compositor_layout::Gaps::all(0),
                description: String::new(),
                made: <(String, String, String)>::default(),
            })
            .unwrap();
        for (id, rect) in [
            (1, Rect::new(100, 100, 640, 480)),
            (2, Rect::new(300, 200, 640, 480)),
            (3, Rect::new(0, 0, 640, 400)),
        ] {
            let _ = state.open_window(WindowId(id)).unwrap();
            let _ = state.float_window(WindowId(id), rect).unwrap();
            let _ = state.focus_window(WindowId(id)).unwrap();
        }
        let _ = state.minimize(WindowId(2)).unwrap();
        state
    }

    fn released() -> Action {
        Action::Modifiers(Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        })
    }

    /// The minimized window is listed with the others, most recent first,
    /// and drawn dimmed in the row.
    #[test]
    fn minimized_windows_are_listed_and_dimmed() {
        let state = desktop();
        assert_eq!(state.focused_window(), Some(WindowId(3)));
        assert_eq!(windows(&state), [WindowId(3), WindowId(2), WindowId(1)]);
        let mut state = state;
        let mut switcher = Switcher::default();
        assert!(dispatch("next", (ALT, ALT), &mut switcher, &mut state));
        assert_eq!(switcher.picked(), Some(WindowId(2)), "the one before");
        assert_eq!(dimmed(&state, &switcher), [WindowId(2)]);
        let row = tiles(&state, &switcher);
        assert_eq!(row.len(), 3);
        assert!(
            row.windows(2).all(|pair| pair[0].1.right() <= pair[1].1.x),
            "side by side: {row:?}"
        );
        let shown = outputs(&state, &switcher);
        let lit: Vec<_> = shown[0]
            .windows
            .iter()
            .filter(|placed| placed.focused)
            .collect();
        assert_eq!(lit.len(), 1);
        assert_eq!(lit[0].window, WindowId(2));
    }

    /// `ALT+Tab` and letting go of `ALT` brings the minimized window back
    /// and focuses it, as GNOME's switcher does.
    #[test]
    fn letting_go_of_alt_restores_the_picked_minimized_window() {
        let mut state = desktop();
        let mut switcher = Switcher::default();
        assert!(dispatch("next", (ALT, ALT), &mut switcher, &mut state));
        let (kept, changed) = take_input(vec![released()], &mut switcher, &mut state, (0.0, 0.0));
        assert!(changed && !switcher.on);
        assert_eq!(kept, [released()], "the client is still told");
        assert!(!state.is_minimized(WindowId(2)));
        assert_eq!(state.focused_window(), Some(WindowId(2)));
    }

    /// Another `Tab` moves the pick on, wrapping, and `prev` goes back;
    /// `Escape` closes and changes nothing.
    #[test]
    fn tab_moves_on_and_escape_changes_nothing() {
        let mut state = desktop();
        let mut switcher = Switcher::default();
        let _ = dispatch("next", (ALT, ALT), &mut switcher, &mut state);
        let _ = dispatch("next", (ALT, ALT), &mut switcher, &mut state);
        assert_eq!(switcher.picked(), Some(WindowId(1)));
        let _ = dispatch("next", (ALT, ALT), &mut switcher, &mut state);
        assert_eq!(switcher.picked(), Some(WindowId(3)), "wraps");
        let _ = dispatch("prev", (ALT, ALT), &mut switcher, &mut state);
        assert_eq!(switcher.picked(), Some(WindowId(1)));
        let (_, changed) = take_input(
            vec![Action::Key {
                code: 1,
                pressed: true,
            }],
            &mut switcher,
            &mut state,
            (0.0, 0.0),
        );
        assert!(changed && !switcher.on);
        assert_eq!(state.focused_window(), Some(WindowId(3)));
        assert!(state.is_minimized(WindowId(2)));
        // And the release of the Escape it took does not reach a client.
        let (kept, _) = take_input(
            vec![Action::Key {
                code: 1,
                pressed: false,
            }],
            &mut switcher,
            &mut state,
            (0.0, 0.0),
        );
        assert!(kept.is_empty());
    }

    /// Opened with no modifier held -- `hyprctl dispatch switcher` -- it
    /// stays up until `Return` picks.
    #[test]
    fn opened_without_a_modifier_return_picks() {
        let mut state = desktop();
        let mut switcher = Switcher::default();
        let _ = dispatch("next", (0, 0), &mut switcher, &mut state);
        let (_, changed) = take_input(vec![released()], &mut switcher, &mut state, (0.0, 0.0));
        assert!(!changed && switcher.on, "nothing was held to let go of");
        let (_, changed) = take_input(
            vec![Action::Key {
                code: 28,
                pressed: true,
            }],
            &mut switcher,
            &mut state,
            (0.0, 0.0),
        );
        assert!(changed && !switcher.on);
        assert_eq!(state.focused_window(), Some(WindowId(2)));
        assert!(!state.is_minimized(WindowId(2)));
    }

    /// A quick `ALT+Tab`, `ALT` let go of before the bind's dispatcher ran,
    /// goes straight back to the window before.
    #[test]
    fn a_quick_alt_tab_goes_back_at_once() {
        let mut state = desktop();
        let mut switcher = Switcher::default();
        assert!(dispatch("next", (ALT, 0), &mut switcher, &mut state));
        assert!(!switcher.on);
        assert_eq!(state.focused_window(), Some(WindowId(2)));
        assert!(!state.is_minimized(WindowId(2)));
    }

    /// A click on a tile picks that window.
    #[test]
    fn a_click_picks_the_tile_under_it() {
        let mut state = desktop();
        let mut switcher = Switcher::default();
        let _ = dispatch("next", (ALT, ALT), &mut switcher, &mut state);
        let (window, rect) = tiles(&state, &switcher)[2];
        assert_eq!(window, WindowId(1));
        #[expect(clippy::cast_precision_loss, reason = "a test's screen")]
        let at = (
            (rect.x + rect.width / 2) as f64,
            (rect.y + rect.height / 2) as f64,
        );
        let (_, changed) = take_input(
            vec![Action::Button {
                button: 0x110,
                pressed: true,
            }],
            &mut switcher,
            &mut state,
            at,
        );
        assert!(changed && !switcher.on);
        assert_eq!(state.focused_window(), Some(WindowId(1)));
    }
}
