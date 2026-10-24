//! The activities overview (`docs/DESKTOP.md` §1.2, step 1d).
//!
//! One key (`overview toggle`, which Ferrix light binds to `SUPER`) shows
//! every window of each monitor's workspace side by side, scaled to fit a
//! grid, with the monitor's other workspaces as a strip of thumbnails above
//! it. A minimized window is in its workspace's grid too, after the others
//! and dimmed, since the overview is where a person looks for a window
//! that is gone from the screen. A click on a window, or `Return` on the
//! one the arrow keys or `Tab` picked, focuses it -- bringing a minimized
//! one back -- and closes the overview; a click on a thumbnail goes to that
//! workspace and window, a click on nothing or `Escape` closes it.
//!
//! It is a way of drawing and of reading the pointer, not a layout: the
//! windows keep their places and their clients are told nothing. The frame
//! is drawn from [`outputs`] in place of the layout's own, so each window
//! is the same surface stretched into its tile, the animations carry it
//! there and back, and the damage follows as it follows any move.

use compositor_layout::{MonitorId, MonitorLayout, Placed, Rect, State, WindowId, WorkspaceId};

use crate::seat::Action;

/// Whether the overview is up, and which window the pointer is over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overview {
    /// Whether it is shown.
    pub on: bool,
    /// The window under the pointer, or the one the keys picked, drawn
    /// with a highlight.
    pub hovered: Option<WindowId>,
    /// Keys whose press the overview took, so that their release is taken
    /// too, even once it has closed: a `Return` that picked a window would
    /// otherwise reach that window as a release it never saw pressed.
    pub eaten: Vec<u16>,
}

/// One window's place in the overview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    /// The window.
    pub window: WindowId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Where it is drawn, in the space every monitor shares.
    pub rect: Rect,
    /// Whether it is in the strip of other workspaces rather than the grid.
    pub thumbnail: bool,
    /// Whether it is minimized: in its workspace's grid, drawn dimmed.
    pub minimized: bool,
}

/// The space between tiles, and round the grid, in logical pixels.
const GAP: i64 = 24;

/// How much of the monitor's height the strip of workspaces takes.
const STRIP: f64 = 0.12;

/// How wide a highlighted tile's outline is.
const HIGHLIGHT: i64 = 3;

/// `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;

/// `KEY_ESC`.
const KEY_ESC: u16 = 1;

/// The keys that pick: `KEY_ENTER` and `KEY_KPENTER`.
const PICK: [u16; 2] = [28, 96];

/// The keys that pick the window before the picked one: `KEY_UP` and
/// `KEY_LEFT`.
const BACK: [u16; 2] = [103, 105];

/// The keys that pick the window after it: `KEY_TAB`, `KEY_RIGHT` and
/// `KEY_DOWN`.
const ON: [u16; 3] = [15, 106, 108];

/// How much of a minimized window shows in the overview: enough to know it
/// by, faint enough to see it is not on the screen.
pub const MINIMIZED_OPACITY: f32 = 0.5;

/// Rectangles for windows of `sizes` laid out in a grid inside `area`, in
/// the order given: as many columns as the square root of the count rounded
/// up, the rows that takes, each window scaled to fit its cell keeping its
/// shape -- never above its own size -- and centred in it, the last row
/// centred when it is short.
#[must_use]
pub fn grid(area: Rect, sizes: &[(i64, i64)], gap: i64) -> Vec<Rect> {
    let count = sizes.len();
    if count == 0 || area.width <= 0 || area.height <= 0 {
        return Vec::new();
    }
    let mut columns = 1_usize;
    while columns.saturating_mul(columns) < count {
        columns = columns.saturating_add(1);
    }
    let rows = count.div_ceil(columns);
    let as_i64 = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    let cell_width = area
        .width
        .saturating_sub(gap.saturating_mul(as_i64(columns).saturating_sub(1)))
        / as_i64(columns);
    let cell_height = area
        .height
        .saturating_sub(gap.saturating_mul(as_i64(rows).saturating_sub(1)))
        / as_i64(rows);
    let mut rects = Vec::with_capacity(count);
    for (index, &(width, height)) in sizes.iter().enumerate() {
        let (row, column) = (index / columns, index % columns);
        // A short last row is centred.
        let in_row = if row + 1 == rows {
            count - row * columns
        } else {
            columns
        };
        let row_width = as_i64(in_row)
            .saturating_mul(cell_width)
            .saturating_add(gap.saturating_mul(as_i64(in_row).saturating_sub(1)));
        let left = area
            .x
            .saturating_add(area.width.saturating_sub(row_width) / 2);
        let cell = Rect::new(
            left.saturating_add(as_i64(column).saturating_mul(cell_width.saturating_add(gap))),
            area.y
                .saturating_add(as_i64(row).saturating_mul(cell_height.saturating_add(gap))),
            cell_width,
            cell_height,
        );
        rects.push(fit(cell, (width, height)));
    }
    rects
}

/// A window of `size` scaled to fit `cell` keeping its shape, never larger
/// than it is, centred in it.
pub(crate) fn fit(cell: Rect, (width, height): (i64, i64)) -> Rect {
    let (width, height) = (width.max(1), height.max(1));
    #[expect(
        clippy::cast_precision_loss,
        reason = "window and screen sizes are far inside f64's exact range"
    )]
    let scale = (cell.width as f64 / width as f64)
        .min(cell.height as f64 / height as f64)
        .min(1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "a scaled size is no larger than the cell"
    )]
    let (w, h) = (
        ((width as f64 * scale).round() as i64).max(1),
        ((height as f64 * scale).round() as i64).max(1),
    );
    Rect::new(
        cell.x.saturating_add(cell.width.saturating_sub(w) / 2),
        cell.y.saturating_add(cell.height.saturating_sub(h) / 2),
        w,
        h,
    )
}

/// `value`, measured from `from`, in a miniature `to` high of something
/// `size` high.
fn shrink(value: i64, from: i64, to: i64, size: i64) -> i64 {
    value.saturating_sub(from).saturating_mul(to) / size.max(1)
}

/// Every monitor's tiles: the strip of its other workspaces' windows, then
/// the grid of the one it shows, bottom to top.
#[must_use]
pub fn tiles(state: &State) -> Vec<(MonitorId, Vec<Tile>)> {
    let listing = state.listing();
    let mut all = Vec::new();
    for monitor in state.monitors() {
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
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "a strip is a fraction of a screen's height"
        )]
        let strip_height = (usable.height as f64 * STRIP).round() as i64;
        let active = state.active_workspace(monitor.id);
        let mut tiles = Vec::new();

        // The strip: each other workspace of this monitor as a miniature
        // of the monitor, its windows where they are on it.
        let others: Vec<WorkspaceId> = state
            .workspaces()
            .filter(|workspace| workspace.0 > 0 && Some(*workspace) != active)
            .filter(|workspace| state.workspace_monitor(*workspace) == Some(monitor.id))
            .collect();
        if !others.is_empty() && whole.height > 0 {
            let box_width = strip_height.saturating_mul(whole.width) / whole.height;
            let count = i64::try_from(others.len()).unwrap_or(i64::MAX);
            let row = count
                .saturating_mul(box_width)
                .saturating_add(GAP.saturating_mul(count.saturating_sub(1)));
            let mut x = usable
                .x
                .saturating_add(usable.width.saturating_sub(row) / 2);
            let y = usable.y.saturating_add(GAP / 2);
            for workspace in others {
                for listed in listing
                    .iter()
                    .filter(|listed| listed.workspace == workspace)
                {
                    let at = listed.placed.rect;
                    let scaled =
                        |value: i64, from: i64, size: i64| shrink(value, from, strip_height, size);
                    let rect = Rect::new(
                        x.saturating_add(scaled(at.x, whole.x, whole.height)),
                        y.saturating_add(scaled(at.y, whole.y, whole.height)),
                        scaled(at.width, 0, whole.height).max(1),
                        scaled(at.height, 0, whole.height).max(1),
                    );
                    tiles.push(Tile {
                        window: listed.placed.window,
                        workspace,
                        rect,
                        thumbnail: true,
                        minimized: false,
                    });
                }
                x = x.saturating_add(box_width).saturating_add(GAP);
            }
        }

        // The grid: the shown workspace's windows, in the order they were
        // opened, below the strip; then the ones minimized from it, which
        // wait on a workspace nothing shows and have no other place to be
        // found.
        if let Some(active) = active {
            let minimized: Vec<WindowId> = state
                .minimized_windows()
                .into_iter()
                .filter(|(_, back)| *back == active)
                .map(|(window, _)| window)
                .collect();
            let mut shown: Vec<_> = listing
                .iter()
                .filter(|listed| listed.workspace == active)
                .filter(|listed| !minimized.contains(&listed.placed.window))
                .collect();
            shown.extend(
                listing
                    .iter()
                    .filter(|listed| minimized.contains(&listed.placed.window)),
            );
            let top = usable
                .y
                .saturating_add(strip_height)
                .saturating_add(GAP.saturating_mul(2));
            let area = Rect::new(
                usable.x.saturating_add(GAP * 2),
                top,
                usable.width.saturating_sub(GAP * 4),
                usable.bottom().saturating_sub(top).saturating_sub(GAP * 2),
            );
            let sizes: Vec<(i64, i64)> = shown
                .iter()
                .map(|listed| (listed.placed.rect.width, listed.placed.rect.height))
                .collect();
            for (listed, rect) in shown.iter().zip(grid(area, &sizes, GAP)) {
                tiles.push(Tile {
                    window: listed.placed.window,
                    workspace: active,
                    rect,
                    thumbnail: false,
                    minimized: minimized.contains(&listed.placed.window),
                });
            }
        }
        all.push((monitor.id, tiles));
    }
    all
}

/// What each monitor draws while the overview is up: [`tiles`] as the
/// windows, the hovered one outlined.
#[must_use]
pub fn outputs(state: &State, overview: &Overview) -> Vec<MonitorLayout> {
    let tiles = tiles(state);
    state
        .layout()
        .into_iter()
        .map(|output| {
            let windows = tiles
                .iter()
                .find(|(monitor, _)| *monitor == output.monitor)
                .map(|(_, tiles)| {
                    tiles
                        .iter()
                        .map(|tile| {
                            let lit = overview.hovered == Some(tile.window) && !tile.thumbnail;
                            Placed {
                                window: tile.window,
                                rect: tile.rect,
                                border: if lit { HIGHLIGHT } else { 0 },
                                focused: lit,
                                floating: true,
                                fullscreen: false,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            MonitorLayout { windows, ..output }
        })
        .collect()
}

/// The windows the overview draws dimmed, at [`MINIMIZED_OPACITY`]: the
/// minimized ones in a grid.
#[must_use]
pub fn dimmed(state: &State) -> Vec<WindowId> {
    tiles(state)
        .into_iter()
        .flat_map(|(_, tiles)| tiles)
        .filter(|tile| tile.minimized)
        .map(|tile| tile.window)
        .collect()
}

/// The window the keys pick after `from` -- or before it, `back` -- among
/// the grids' tiles, wrapping: the first or the last when nothing is
/// picked yet.
fn step(state: &State, from: Option<WindowId>, back: bool) -> Option<WindowId> {
    let order: Vec<WindowId> = tiles(state)
        .into_iter()
        .flat_map(|(_, tiles)| tiles)
        .filter(|tile| !tile.thumbnail)
        .map(|tile| tile.window)
        .collect();
    let count = order.len();
    if count == 0 {
        return None;
    }
    let at = from.and_then(|window| order.iter().position(|held| *held == window));
    let next = match (at, back) {
        (None, false) => 0,
        (None, true) => count - 1,
        (Some(at), false) => (at + 1) % count,
        (Some(at), true) => at.checked_sub(1).unwrap_or(count - 1),
    };
    order.get(next).copied()
}

/// The tile at `point`, the topmost where two overlap.
#[must_use]
pub fn hit(state: &State, point: (i64, i64)) -> Option<Tile> {
    let (x, y) = point;
    tiles(state)
        .into_iter()
        .flat_map(|(_, tiles)| tiles)
        .rfind(|tile| {
            x >= tile.rect.x && x < tile.rect.right() && y >= tile.rect.y && y < tile.rect.bottom()
        })
}

/// `overview toggle|on|off|next|prev|pick`: the last three are the keys'
/// `Tab`, `Left` and `Return`, for a script or a bind. Gives whether
/// anything changed.
pub fn dispatch_in(argument: &str, overview: &mut Overview, state: &mut State) -> bool {
    match argument.trim() {
        "next" | "prev" if overview.on => {
            let picked = step(state, overview.hovered, argument.trim() == "prev");
            let changed = picked != overview.hovered;
            overview.hovered = picked;
            changed
        }
        "pick" if overview.on => match overview.hovered {
            Some(window) => {
                let _ = state.focus_window(window);
                dispatch("off", overview)
            }
            None => false,
        },
        "next" | "prev" | "pick" => false,
        _ => dispatch(argument, overview),
    }
}

/// `overview toggle|on|off`. Gives whether anything changed.
pub fn dispatch(argument: &str, overview: &mut Overview) -> bool {
    let on = match argument.trim() {
        "on" | "open" => true,
        "off" | "close" => false,
        _ => !overview.on,
    };
    let changed = overview.on != on;
    overview.on = on;
    if !on {
        overview.hovered = None;
    }
    changed
}

/// Take the input while the overview is up: the pointer highlights and
/// picks, the arrow keys and `Tab` highlight, `Return` picks, `Escape`
/// closes, and nothing reaches a client. Binds still run, so the key that
/// opened it closes it. Gives the actions left over and whether the
/// overview or the layout changed.
pub fn take_input(
    actions: Vec<Action>,
    overview: &mut Overview,
    state: &mut State,
    pointer: (f64, f64),
) -> (Vec<Action>, bool) {
    if !overview.on {
        // The release of a key the overview took, after it closed.
        if overview.eaten.is_empty() {
            return (actions, false);
        }
        let kept = actions
            .into_iter()
            .filter(|action| match action {
                Action::Key {
                    code,
                    pressed: false,
                } => {
                    let at = overview.eaten.iter().position(|held| held == code);
                    at.map(|at| overview.eaten.remove(at)).is_none()
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
    let mut changed = false;
    let mut kept = Vec::new();
    for action in actions {
        match action {
            Action::Pointer { .. } => {
                let hovered = hit(state, at)
                    .filter(|tile| !tile.thumbnail)
                    .map(|tile| tile.window);
                if hovered != overview.hovered {
                    overview.hovered = hovered;
                    changed = true;
                }
            }
            Action::Button {
                button: BTN_LEFT,
                pressed: true,
            } => {
                if let Some(tile) = hit(state, at) {
                    let _ = state.focus_window(tile.window);
                }
                changed |= dispatch("off", overview);
            }
            Action::Key {
                code,
                pressed: true,
            } => {
                // The keys it acts on, whose release would otherwise reach
                // the window they focused. Every other key was never the
                // overview's to keep, and is dropped as it always was.
                let acts = code == KEY_ESC
                    || PICK.contains(&code)
                    || BACK.contains(&code)
                    || ON.contains(&code);
                if acts && !overview.eaten.contains(&code) {
                    overview.eaten.push(code);
                }
                if code == KEY_ESC {
                    changed |= dispatch("off", overview);
                } else if PICK.contains(&code) {
                    changed |= dispatch_in("pick", overview, state);
                } else if BACK.contains(&code) {
                    changed |= dispatch_in("prev", overview, state);
                } else if ON.contains(&code) {
                    changed |= dispatch_in("next", overview, state);
                }
            }
            Action::Key {
                code,
                pressed: false,
            } => {
                if let Some(at) = overview.eaten.iter().position(|held| *held == code) {
                    let _ = overview.eaten.remove(at);
                }
            }
            Action::Dispatch { .. } | Action::Modifiers(_) => kept.push(action),
            _ => {}
        }
    }
    (kept, changed)
}

#[cfg(test)]
mod tests {
    use compositor_config::NoSources;
    use compositor_layout::{Monitor, MonitorId, Rect, State, WindowId};

    use super::{Overview, dispatch, grid, take_input, tiles};
    use crate::seat::Action;

    /// A floating desktop on one 1280x800 monitor with two windows on
    /// workspace 1 and one on workspace 2, workspace 1 shown.
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
        }
        let _ = state.focus_window(WindowId(3)).unwrap();
        let _ = state.dispatch_str("movetoworkspacesilent", "2").unwrap();
        state
    }

    /// The shown workspace's windows are side by side in the grid, below a
    /// strip holding the other workspace's window in miniature.
    #[test]
    fn the_shown_workspace_is_a_grid_and_the_others_a_strip() {
        let state = desktop();
        let all = tiles(&state);
        let [(_, tiles)] = all.as_slice() else {
            panic!("one monitor: {all:?}");
        };
        let thumbnails: Vec<_> = tiles.iter().filter(|tile| tile.thumbnail).collect();
        let grid: Vec<_> = tiles.iter().filter(|tile| !tile.thumbnail).collect();
        assert_eq!(thumbnails.len(), 1);
        assert_eq!(thumbnails[0].window, WindowId(3));
        assert!(thumbnails[0].rect.height < 100, "{:?}", thumbnails[0]);
        assert_eq!(grid.len(), 2);
        assert!(
            grid[0].rect.right() <= grid[1].rect.x,
            "side by side: {grid:?}"
        );
        assert!(
            grid.iter()
                .all(|tile| tile.rect.y > thumbnails[0].rect.bottom())
        );
    }

    /// A click on a tile focuses its window and closes the overview, and
    /// nothing reaches a client; Escape closes it too.
    #[test]
    fn a_click_picks_a_window_and_escape_closes() {
        let mut state = desktop();
        let mut overview = Overview::default();
        assert!(dispatch("toggle", &mut overview));
        let all = tiles(&state);
        let first = all[0]
            .1
            .iter()
            .find(|tile| !tile.thumbnail)
            .copied()
            .unwrap();
        #[expect(clippy::cast_precision_loss, reason = "a test's screen")]
        let at = (
            (first.rect.x + first.rect.width / 2) as f64,
            (first.rect.y + first.rect.height / 2) as f64,
        );
        let (kept, changed) = take_input(
            vec![Action::Pointer { x: at.0, y: at.1 }],
            &mut overview,
            &mut state,
            at,
        );
        assert!(kept.is_empty() && changed);
        assert_eq!(overview.hovered, Some(first.window));
        let (kept, _) = take_input(
            vec![Action::Button {
                button: 0x110,
                pressed: true,
            }],
            &mut overview,
            &mut state,
            at,
        );
        assert!(kept.is_empty());
        assert!(!overview.on);
        assert_eq!(state.focused_window(), Some(first.window));

        assert!(dispatch("on", &mut overview));
        let (kept, changed) = take_input(
            vec![Action::Key {
                code: 1,
                pressed: true,
            }],
            &mut overview,
            &mut state,
            (0.0, 0.0),
        );
        assert!(kept.is_empty() && changed && !overview.on);
    }

    /// A minimized window is in its workspace's grid, after the others and
    /// marked; the strip does not have it, since it is on no workspace a
    /// monitor shows.
    #[test]
    fn a_minimized_window_is_in_its_workspaces_grid_and_dimmed() {
        let mut state = desktop();
        let _ = state.focus_window(WindowId(1)).unwrap();
        let _ = state.minimize(WindowId(1)).unwrap();
        let all = tiles(&state);
        let grid: Vec<_> = all[0].1.iter().filter(|tile| !tile.thumbnail).collect();
        assert_eq!(
            grid.iter()
                .map(|tile| (tile.window, tile.minimized))
                .collect::<Vec<_>>(),
            [(WindowId(2), false), (WindowId(1), true)]
        );
        assert_eq!(super::dimmed(&state), [WindowId(1)]);
        assert!(
            all[0]
                .1
                .iter()
                .filter(|tile| tile.thumbnail)
                .all(|tile| tile.window == WindowId(3))
        );
    }

    /// A click on a minimized window's tile brings it back and focuses it.
    #[test]
    fn a_click_on_a_minimized_tile_restores_it() {
        let mut state = desktop();
        let _ = state.focus_window(WindowId(1)).unwrap();
        let _ = state.minimize(WindowId(1)).unwrap();
        let mut overview = Overview::default();
        let _ = dispatch("on", &mut overview);
        let tile = tiles(&state)[0]
            .1
            .iter()
            .find(|tile| tile.minimized)
            .copied()
            .unwrap();
        #[expect(clippy::cast_precision_loss, reason = "a test's screen")]
        let at = (
            (tile.rect.x + tile.rect.width / 2) as f64,
            (tile.rect.y + tile.rect.height / 2) as f64,
        );
        let (_, changed) = take_input(
            vec![Action::Button {
                button: 0x110,
                pressed: true,
            }],
            &mut overview,
            &mut state,
            at,
        );
        assert!(changed && !overview.on);
        assert!(!state.is_minimized(WindowId(1)));
        assert_eq!(state.focused_window(), Some(WindowId(1)));
    }

    /// The keys: `Left` with nothing picked picks the last tile -- the
    /// minimized one -- `Tab` the next, wrapping, and `Return` restores the
    /// picked window; its release is taken too, after the overview closed.
    #[test]
    fn the_arrow_keys_pick_and_return_restores() {
        let mut state = desktop();
        let _ = state.focus_window(WindowId(1)).unwrap();
        let _ = state.minimize(WindowId(1)).unwrap();
        let mut overview = Overview::default();
        let _ = dispatch("on", &mut overview);
        let key = |code: u16, pressed: bool| vec![Action::Key { code, pressed }];
        let (_, changed) = take_input(key(105, true), &mut overview, &mut state, (0.0, 0.0));
        assert!(changed);
        assert_eq!(overview.hovered, Some(WindowId(1)), "Left picks the last");
        let _ = take_input(key(105, false), &mut overview, &mut state, (0.0, 0.0));
        let _ = take_input(key(15, true), &mut overview, &mut state, (0.0, 0.0));
        assert_eq!(overview.hovered, Some(WindowId(2)), "Tab wraps");
        let _ = take_input(key(15, true), &mut overview, &mut state, (0.0, 0.0));
        assert_eq!(overview.hovered, Some(WindowId(1)));
        let (_, changed) = take_input(key(28, true), &mut overview, &mut state, (0.0, 0.0));
        assert!(changed && !overview.on);
        assert!(!state.is_minimized(WindowId(1)));
        assert_eq!(state.focused_window(), Some(WindowId(1)));
        let (kept, _) = take_input(key(28, false), &mut overview, &mut state, (0.0, 0.0));
        assert!(kept.is_empty(), "the Return's release reached a client");
        // And a key it never took still reaches one.
        let (kept, _) = take_input(key(30, false), &mut overview, &mut state, (0.0, 0.0));
        assert_eq!(kept, key(30, false));
    }

    /// One window is centred at its own size; one too large is scaled down
    /// keeping its shape.
    #[test]
    fn one_window_is_centred_and_never_enlarged() {
        let area = Rect::new(0, 0, 1000, 600);
        assert_eq!(
            grid(area, &[(400, 300)], 20),
            [Rect::new(300, 150, 400, 300)]
        );
        assert_eq!(
            grid(area, &[(2000, 600)], 20),
            [Rect::new(0, 150, 1000, 300)]
        );
    }

    /// Three windows: two columns, two rows, the short last row centred.
    #[test]
    fn three_windows_take_two_rows_and_the_last_is_centred() {
        let area = Rect::new(0, 0, 1020, 620);
        let rects = grid(area, &[(500, 300), (500, 300), (500, 300)], 20);
        assert_eq!(
            rects,
            [
                Rect::new(0, 0, 500, 300),
                Rect::new(520, 0, 500, 300),
                Rect::new(260, 320, 500, 300),
            ]
        );
    }

    /// Nothing to lay out is nothing.
    #[test]
    fn no_windows_no_tiles() {
        assert!(grid(Rect::new(0, 0, 100, 100), &[], 10).is_empty());
    }
}
