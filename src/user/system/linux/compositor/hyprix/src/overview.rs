//! The activities overview (`docs/DESKTOP.md` §1.2, step 1d).
//!
//! One key (`overview toggle`, which Ferrix light binds to `SUPER`) shows
//! every window of each monitor's workspace side by side, scaled to fit a
//! grid, with the monitor's other workspaces as a strip of thumbnails above
//! it. A click on a window focuses it and closes the overview, a click on a
//! thumbnail goes to that workspace and window, a click on nothing or
//! `Escape` closes it.
//!
//! It is a way of drawing and of reading the pointer, not a layout: the
//! windows keep their places and their clients are told nothing. The frame
//! is drawn from [`outputs`] in place of the layout's own, so each window
//! is the same surface stretched into its tile, the animations carry it
//! there and back, and the damage follows as it follows any move.

use compositor_layout::{MonitorId, MonitorLayout, Placed, Rect, State, WindowId, WorkspaceId};

use crate::seat::Action;

/// Whether the overview is up, and which window the pointer is over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Overview {
    /// Whether it is shown.
    pub on: bool,
    /// The window under the pointer, drawn with a highlight.
    pub hovered: Option<WindowId>,
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
fn fit(cell: Rect, (width, height): (i64, i64)) -> Rect {
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
                    });
                }
                x = x.saturating_add(box_width).saturating_add(GAP);
            }
        }

        // The grid: the shown workspace's windows, in the order they were
        // opened, below the strip.
        if let Some(active) = active {
            let shown: Vec<_> = listing
                .iter()
                .filter(|listed| listed.workspace == active)
                .collect();
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
/// picks, `Escape` closes, and nothing reaches a client. Binds still run,
/// so the key that opened it closes it. Gives the actions left over and
/// whether the overview or the layout changed.
pub fn take_input(
    actions: Vec<Action>,
    overview: &mut Overview,
    state: &mut State,
    pointer: (f64, f64),
) -> (Vec<Action>, bool) {
    if !overview.on {
        return (actions, false);
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
                code: KEY_ESC,
                pressed: true,
            } => changed |= dispatch("off", overview),
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
