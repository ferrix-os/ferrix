use compositor_config::{Config, Gaps, NoSources, parse};

use crate::Group;
use crate::{
    Change, Corner, Direction, Dispatcher, Error, ForceSplit, FullscreenMode, Layout, Limits,
    Monitor, MonitorId, Move, NewStatus, Orientation, Rect, Settings, State, WindowId, WorkspaceId,
    WorkspaceTarget,
};

/// No gaps and no border, so slots and client rectangles coincide.
const BARE: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n";

const M1: MonitorId = MonitorId(1);
const M2: MonitorId = MonitorId(2);

fn config(text: &str) -> Config {
    let parsed = parse("t.conf", text, &mut NoSources);
    assert_eq!(parsed.diagnostics, [], "the test configuration parses");
    parsed.config
}

fn monitor(id: MonitorId, x: i64, y: i64, width: i64, height: i64) -> Monitor {
    Monitor {
        scale: 1.0,
        transform: Default::default(),
        // What a virtio-gpu connector is called, which is what Ferrix's
        // monitors are named after.
        name: format!("Virtual-{}", id.0),
        id,
        rect: Rect::new(x, y, width, height),
        reserved: Gaps::all(0),
        description: String::new(),
        made: <(String, String, String)>::default(),
    }
}

/// A state from `text` with one 1920x1080 monitor.
fn setup(text: &str) -> State {
    state_on(text, monitor(M1, 0, 0, 1920, 1080))
}

fn state_on(text: &str, monitor: Monitor) -> State {
    let mut state = State::from_config(&config(text));
    let _changes = state.add_monitor(monitor).unwrap();
    state
}

fn open(state: &mut State, ids: &[u64]) {
    for &id in ids {
        let _changes = state.open_window(WindowId(id)).unwrap();
    }
}

fn dispatch(state: &mut State, name: &str, arg: &str) -> Vec<Change> {
    state.dispatch_str(name, arg).unwrap()
}

fn focus(state: &mut State, id: u64) {
    let _changes = state.focus_window(WindowId(id)).unwrap();
}

/// Three tiled windows with *two* splits running down the screen above the
/// second, which is what a corner has to choose between.
///
/// 3840 wide, because a split's direction comes from the box it divides: on
/// a 1920 screen the right half is taller than it is wide and divides the
/// other way, and there is then only one split of each direction to pick.
fn wide_three() -> State {
    let mut state = state_on(BARE, monitor(M1, 0, 0, 3840, 1080));
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    focus(&mut state, 2);
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 1920, 1080)),
            (2, Rect::new(1920, 0, 960, 1080)),
            (3, Rect::new(2880, 0, 960, 1080)),
        ],
        "the right half divides side by side too"
    );
    state
}

/// A resize of `pixels` across and none downwards.
const fn distance(pixels: i64) -> Move {
    Move {
        x: pixels,
        y: 0,
        exact: false,
    }
}

/// The windows the given monitor shows, with their client rectangles.
fn rects_on(state: &State, monitor: MonitorId) -> Vec<(u64, Rect)> {
    state
        .layout()
        .into_iter()
        .find(|layout| layout.monitor == monitor)
        .unwrap()
        .windows
        .into_iter()
        .map(|placed| (placed.window.0, placed.rect))
        .collect()
}

fn rects(state: &State) -> Vec<(u64, Rect)> {
    let mut rects = rects_on(state, M1);
    rects.sort_by_key(|(id, _)| *id);
    rects
}

fn r(x: i64, y: i64, width: i64, height: i64) -> Rect {
    Rect::new(x, y, width, height)
}

fn focused(state: &State) -> Option<u64> {
    state.focused_window().map(|window| window.0)
}

// -- Settings -----------------------------------------------------------------

#[test]
fn settings_default_to_hyprlands_defaults() {
    let settings = Settings::default();
    assert_eq!(settings.layout, Layout::Dwindle);
    assert_eq!(settings.gaps_in, Gaps::all(5));
    assert_eq!(settings.gaps_out, Gaps::all(20));
    assert_eq!(settings.border_size, 1);
    assert!(!settings.no_focus_fallback);
    assert!(!settings.dwindle.preserve_split);
    assert_eq!(settings.dwindle.force_split, ForceSplit::Auto);
    assert!((settings.dwindle.split_width_multiplier - 1.0).abs() < f64::EPSILON);
    assert!((settings.dwindle.default_split_ratio - 1.0).abs() < f64::EPSILON);
    assert!((settings.master.mfact - 0.55).abs() < f64::EPSILON);
    assert_eq!(settings.master.new_status, NewStatus::Slave);
    assert!(!settings.master.new_on_top);
    assert_eq!(settings.master.orientation, Orientation::Left);
}

#[test]
fn settings_are_read_from_the_configuration_and_clamped() {
    let settings = Settings::from_config(&config(
        "general {\n  layout = master\n  gaps_in = 1 2 3 4\n  gaps_out = 7\n  border_size = 3\n  no_focus_fallback = 1\n}\n\
         dwindle {\n  preserve_split = true\n  force_split = 2\n  split_width_multiplier = 1.5\n  default_split_ratio = 5\n}\n\
         master {\n  mfact = 0.01\n  new_status = master\n  new_on_top = 1\n  orientation = bottom\n}\n",
    ));
    assert_eq!(settings.layout, Layout::Master);
    assert_eq!(
        settings.gaps_in,
        Gaps {
            top: 1,
            right: 2,
            bottom: 3,
            left: 4
        }
    );
    assert_eq!(settings.gaps_out, Gaps::all(7));
    assert_eq!(settings.border_size, 3);
    assert!(settings.no_focus_fallback);
    assert!(settings.dwindle.preserve_split);
    assert_eq!(settings.dwindle.force_split, ForceSplit::Second);
    assert!((settings.dwindle.split_width_multiplier - 1.5).abs() < f64::EPSILON);
    assert!((settings.dwindle.default_split_ratio - 1.9).abs() < f64::EPSILON);
    assert!((settings.master.mfact - 0.05).abs() < f64::EPSILON);
    assert_eq!(settings.master.new_status, NewStatus::Master);
    assert!(settings.master.new_on_top);
    assert_eq!(settings.master.orientation, Orientation::Bottom);

    let fallback = Settings::from_config(&config(
        "general:layout = spiral\ngeneral:border_size = -4\nmaster:orientation = sideways\n",
    ));
    assert_eq!(fallback.layout, Layout::Dwindle);
    assert_eq!(fallback.border_size, 0);
    assert_eq!(fallback.master.orientation, Orientation::Left);

    // `center` is its own orientation, with the two settings that decide
    // when it takes and which way it leans until it does.
    let centred = Settings::from_config(&config(
        "master:orientation = center\n\
         master:slave_count_for_center_master = 3\n\
         master:center_master_fallback = right\n",
    ));
    assert_eq!(centred.master.orientation, Orientation::Center);
    assert_eq!(centred.master.slave_count_for_center, 3);
    assert_eq!(centred.master.center_fallback, Orientation::Right);
    // And `center` is not a fallback for itself; Hyprland's own list is the
    // four sides, and anything else is `left`.
    let circular = Settings::from_config(&config("master:center_master_fallback = center\n"));
    assert_eq!(circular.master.center_fallback, Orientation::Left);
}

// -- Dwindle ------------------------------------------------------------------

#[test]
fn a_lone_window_fills_the_monitor() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
}

#[test]
fn dwindle_splits_a_wide_box_side_by_side() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );
    assert_eq!(focused(&state), Some(2));
}

#[test]
fn dwindle_stacks_on_a_tall_monitor() {
    let mut state = state_on(BARE, monitor(M1, 0, 0, 1080, 1920));
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1080, 960)), (2, r(0, 960, 1080, 960))]
    );
}

#[test]
fn dwindle_splits_the_focused_window_and_dwindles() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    // 3 opened beside the focused 2, whose 960x1080 box is tall.
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 0, 960, 1080)),
            (2, r(960, 0, 960, 540)),
            (3, r(960, 540, 960, 540))
        ]
    );
    // Beside 1 instead, once it has focus.
    focus(&mut state, 1);
    open(&mut state, &[4]);
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 0, 960, 540)),
            (2, r(960, 0, 960, 540)),
            (3, r(960, 540, 960, 540)),
            (4, r(0, 540, 960, 540))
        ]
    );
}

#[test]
fn dwindle_split_width_multiplier_decides_the_direction() {
    let mut state = setup(&format!("{BARE}dwindle:split_width_multiplier = 2.0\n"));
    open(&mut state, &[1, 2]);
    // 1920 is not more than 1080 * 2, so the split stacks.
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1920, 540)), (2, r(0, 540, 1920, 540))]
    );
}

#[test]
fn dwindle_default_split_ratio_sizes_the_first_child() {
    let mut state = setup(&format!("{BARE}dwindle:default_split_ratio = 1.5\n"));
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1440, 1080)), (2, r(1440, 0, 480, 1080))]
    );
}

#[test]
fn dwindle_force_split_puts_the_new_window_first() {
    let mut state = setup(&format!("{BARE}dwindle:force_split = 1\n"));
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [(1, r(960, 0, 960, 1080)), (2, r(0, 0, 960, 1080))]
    );
    let mut state = setup(&format!("{BARE}dwindle:force_split = 2\n"));
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );
}

/// `dwindle:split_bias = 1`: the split's ratio favours the window that was
/// already there rather than whichever of the two ends up first.
///
/// Only visible together with a ratio that is not one and a `force_split`
/// that puts the new window first: with `split_bias = 0` the first child
/// takes the larger share whichever window it is, and with `1` the share
/// follows the old window across.
#[test]
fn dwindle_split_bias_favours_the_window_that_was_there() {
    let settings = "dwindle:default_split_ratio = 1.5\ndwindle:force_split = 1\n";
    // `0`, the default: the new window is first and takes 1.5 halves.
    let mut directional = setup(&format!("{BARE}{settings}"));
    open(&mut directional, &[1, 2]);
    assert_eq!(
        rects(&directional),
        [(1, r(1440, 0, 480, 1080)), (2, r(0, 0, 1440, 1080))]
    );

    // `1`: the ratio is turned over, so the share stays with the window
    // that was already on the screen.
    let mut current = setup(&format!("{BARE}{settings}dwindle:split_bias = 1\n"));
    open(&mut current, &[1, 2]);
    assert_eq!(
        rects(&current),
        [(1, r(480, 0, 1440, 1080)), (2, r(0, 0, 480, 1080))]
    );
}

#[test]
fn dwindle_odd_sizes_round_edges_not_widths() {
    let mut state = state_on(BARE, monitor(M1, 0, 0, 1001, 500));
    open(&mut state, &[1, 2]);
    // 500.5 rounds to an edge at 501 that both windows share.
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 501, 500)), (2, r(501, 0, 500, 500))]
    );
}

#[test]
fn removing_a_window_promotes_its_sibling() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    let _changes = state.window_gone(WindowId(2)).unwrap();
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 960, 1080)), (3, r(960, 0, 960, 1080))]
    );
    assert_eq!(state.windows(WorkspaceId(1)), [WindowId(1), WindowId(3)]);

    // A promoted subtree keeps its windows; without preserve_split its split
    // direction is chosen again for its new, wide box.
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    let _changes = state.window_gone(WindowId(1)).unwrap();
    assert_eq!(
        rects(&state),
        [(2, r(0, 0, 960, 1080)), (3, r(960, 0, 960, 1080))]
    );
    assert_eq!(focused(&state), Some(3));
}

#[test]
fn preserve_split_keeps_a_promoted_splits_direction() {
    let mut state = setup(&format!("{BARE}dwindle:preserve_split = 1\n"));
    open(&mut state, &[1, 2, 3]);
    let _changes = state.window_gone(WindowId(1)).unwrap();
    assert_eq!(
        rects(&state),
        [(2, r(0, 0, 1920, 540)), (3, r(0, 540, 1920, 540))]
    );
}

#[test]
fn closing_the_last_window_empties_the_workspace() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let changes = state.window_gone(WindowId(1)).unwrap();
    assert_eq!(changes, [Change::Layout(M1), Change::Focus(None)]);
    assert_eq!(rects(&state), []);
    assert_eq!(
        state.window_gone(WindowId(1)),
        Err(Error::UnknownWindow(WindowId(1)))
    );
}

// -- Gaps and borders ---------------------------------------------------------

#[test]
fn gaps_out_face_the_monitor_and_gaps_in_face_neighbours() {
    // Hyprland's defaults: gaps_in 5, gaps_out 20, border 1.
    let mut state = setup("");
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(21, 21, 1878, 1038))]);
    open(&mut state, &[2]);
    // Left window: 20 + 1 on the monitor edges, 5 + 1 on the inner edge.
    // Right window: the mirror image. The borders are 10 pixels apart.
    assert_eq!(
        rects(&state),
        [(1, r(21, 21, 933, 1038)), (2, r(966, 21, 933, 1038))]
    );
    let layout = state.layout();
    let windows = &layout[0].windows;
    assert!(windows.iter().all(|placed| placed.border == 1));
    assert_eq!(
        windows
            .iter()
            .map(|placed| (placed.window.0, placed.focused))
            .collect::<Vec<_>>(),
        [(1, false), (2, true)]
    );
}

#[test]
fn gaps_follow_css_order_per_side() {
    let mut state = setup(
        "general:gaps_out = 10 20 30 40\ngeneral:gaps_in = 1 2 3 4\ngeneral:border_size = 2\n",
    );
    open(&mut state, &[1]);
    // top 10+2, right 20+2, bottom 30+2, left 40+2.
    assert_eq!(
        rects(&state),
        [(1, r(42, 12, 1920 - 42 - 22, 1080 - 12 - 32))]
    );
    open(&mut state, &[2]);
    // The work area, 40 to 1900, splits at 970. The left window's right
    // edge takes gaps_in's right (2), the right window's left edge gaps_in's
    // left (4).
    assert_eq!(
        rects(&state),
        [
            (1, r(42, 12, 970 - 2 - 2 - 42, 1036)),
            (2, r(970 + 4 + 2, 12, 1900 - 2 - 976, 1036))
        ]
    );
}

#[test]
fn splits_divide_the_area_inside_gaps_out() {
    // As Hyprland's work area does: with a ratio of 1.5 the first child
    // gets 1.5 times half of 1880, not of 1920, and the outer edges of both
    // windows are gaps_out from the monitor's.
    let mut state = setup("dwindle:default_split_ratio = 1.5\n");
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [
            (1, r(21, 21, 20 + 1410 - 6 - 21, 1038)),
            (2, r(20 + 1410 + 6, 21, 1899 - 1436, 1038))
        ]
    );
}

#[test]
fn reserved_strips_are_outside_the_tiled_area() {
    let mut state = State::from_config(&config("general:gaps_out = 20\ngeneral:border_size = 0\n"));
    let _changes = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: String::new(),
            id: M1,
            rect: r(0, 0, 1920, 1080),
            reserved: Gaps {
                top: 30,
                right: 0,
                bottom: 0,
                left: 0,
            },
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(20, 50, 1880, 1010))]);
}

#[test]
fn gaps_apply_on_a_monitor_away_from_the_origin() {
    let mut state = state_on("", monitor(M1, 1920, 100, 1280, 720));
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [
            (1, r(1920 + 21, 121, 640 - 27, 720 - 42)),
            (2, r(1920 + 640 + 6, 121, 640 - 27, 720 - 42))
        ]
    );
}

// -- Master -------------------------------------------------------------------

fn master(orientation: &str) -> State {
    setup(&format!(
        "{BARE}general:layout = master\nmaster:orientation = {orientation}\n"
    ))
}

#[test]
fn master_left() {
    let mut state = master("left");
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1056, 1080)), (2, r(1056, 0, 864, 1080))]
    );
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 0, 1056, 1080)),
            (2, r(1056, 0, 864, 540)),
            (3, r(1056, 540, 864, 540))
        ]
    );
}

/// `master:orientation = center`: the master in the middle, the stack in two
/// columns beside it, once there are enough of them.
///
/// Below `slave_count_for_center_master` it is the fallback orientation
/// instead, which is what keeps one window from being a narrow strip in the
/// middle of an empty screen
/// (`CMasterAlgorithm::recalculateSpace`).
#[test]
fn master_center() {
    let mut state = master("center");
    // One window fills the screen, as it does in every orientation.
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);

    // One window in the stack is below `slave_count_for_center_master`,
    // which defaults to two, so this is the fallback: `left`.
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1056, 1080)), (2, r(1056, 0, 864, 1080))]
    );

    // Two in the stack, and the master goes to the middle with one column
    // either side. `mfact` is 0.55, so the master is 1056 wide and the
    // columns 432 each.
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state),
        [
            (1, r(432, 0, 1056, 1080)),
            (2, r(0, 0, 432, 1080)),
            (3, r(1488, 0, 432, 1080))
        ]
    );

    // The odd window goes to the column the fallback does not name, so
    // with the default `left` fallback the left column takes two of three.
    open(&mut state, &[4]);
    assert_eq!(
        rects(&state),
        [
            (1, r(432, 0, 1056, 1080)),
            (2, r(0, 0, 432, 540)),
            (3, r(1488, 0, 432, 1080)),
            (4, r(0, 540, 432, 540))
        ]
    );
}

/// `master:center_master_fallback = right` leans the other way: it is the
/// orientation below the threshold, and the right column takes the odd
/// window above it.
#[test]
fn master_center_leaning_right() {
    let mut state = setup(&format!(
        "{BARE}general:layout = master\n\
         master:orientation = center\n\
         master:center_master_fallback = right\n\
         master:slave_count_for_center_master = 3\n"
    ));
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [(1, r(864, 0, 1056, 1080)), (2, r(0, 0, 864, 1080))],
        "one in the stack: the fallback, which is `right`"
    );

    // Three in the stack reaches the threshold, and the right column takes
    // two of the three.
    open(&mut state, &[3, 4]);
    assert_eq!(
        rects(&state),
        [
            (1, r(432, 0, 1056, 1080)),
            (2, r(1488, 0, 432, 540)),
            (3, r(0, 0, 432, 1080)),
            (4, r(1488, 540, 432, 540))
        ]
    );
}

/// `master:always_keep_position`: one window alone keeps the master's share
/// of the screen rather than filling it, so that opening a second does not
/// move the first.
#[test]
fn master_keeps_its_position_when_it_is_alone() {
    let mut state = setup(&format!(
        "{BARE}general:layout = master\nmaster:always_keep_position = true\n"
    ));
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(0, 0, 1056, 1080))]);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1056, 1080)), (2, r(1056, 0, 864, 1080))],
        "the first window did not move"
    );
}

#[test]
fn master_right() {
    let mut state = master("right");
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [(1, r(864, 0, 1056, 1080)), (2, r(0, 0, 864, 1080))]
    );
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state),
        [
            (1, r(864, 0, 1056, 1080)),
            (2, r(0, 0, 864, 540)),
            (3, r(0, 540, 864, 540))
        ]
    );
}

#[test]
fn master_top() {
    let mut state = master("top");
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1920, 594)), (2, r(0, 594, 1920, 486))]
    );
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 0, 1920, 594)),
            (2, r(0, 594, 960, 486)),
            (3, r(960, 594, 960, 486))
        ]
    );
}

#[test]
fn master_bottom() {
    let mut state = master("bottom");
    open(&mut state, &[1]);
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 486, 1920, 594)), (2, r(0, 0, 1920, 486))]
    );
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 486, 1920, 594)),
            (2, r(0, 0, 960, 486)),
            (3, r(960, 0, 960, 486))
        ]
    );
}

#[test]
fn master_stack_shares_an_uneven_height_without_losing_a_pixel() {
    let mut state = state_on(
        &format!("{BARE}general:layout = master\nmaster:mfact = 0.5\n"),
        monitor(M1, 0, 0, 1000, 1000),
    );
    open(&mut state, &[1, 2, 3, 4]);
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 0, 500, 1000)),
            (2, r(500, 0, 500, 333)),
            (3, r(500, 333, 500, 334)),
            (4, r(500, 667, 500, 333))
        ]
    );
}

#[test]
fn master_new_status_master_takes_over_and_the_old_master_joins_the_stack() {
    let mut state = setup(&format!(
        "{BARE}general:layout = master\nmaster:new_status = master\n"
    ));
    open(&mut state, &[1, 2, 3]);
    assert_eq!(
        state.windows(WorkspaceId(1)),
        [WindowId(3), WindowId(1), WindowId(2)]
    );
    assert_eq!(
        rects(&state),
        [
            (1, r(1056, 0, 864, 540)),
            (2, r(1056, 540, 864, 540)),
            (3, r(0, 0, 1056, 1080))
        ]
    );
}

#[test]
fn master_new_status_inherit_follows_the_focused_window() {
    let mut state = setup(&format!(
        "{BARE}general:layout = master\nmaster:new_status = inherit\n"
    ));
    // 1 is the master and focused when 2 opens, so 2 takes over.
    open(&mut state, &[1, 2]);
    assert_eq!(state.windows(WorkspaceId(1)), [WindowId(2), WindowId(1)]);
    // 1 is focused and in the stack, so 3 joins the stack.
    focus(&mut state, 1);
    open(&mut state, &[3]);
    assert_eq!(state.windows(WorkspaceId(1))[0], WindowId(2));
    focus(&mut state, 2);
    open(&mut state, &[4]);
    assert_eq!(state.windows(WorkspaceId(1))[0], WindowId(4));
}

#[test]
fn master_new_on_top_puts_new_windows_at_the_top_of_the_stack() {
    let mut state = setup(&format!(
        "{BARE}general:layout = master\nmaster:new_on_top = true\n"
    ));
    open(&mut state, &[1, 2, 3]);
    assert_eq!(
        state.windows(WorkspaceId(1)),
        [WindowId(1), WindowId(3), WindowId(2)]
    );
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 0, 1056, 1080)),
            (2, r(1056, 540, 864, 540)),
            (3, r(1056, 0, 864, 540))
        ]
    );
}

#[test]
fn master_closing_the_master_promotes_the_first_stack_window() {
    let mut state = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut state, &[1, 2, 3]);
    let _changes = state.window_gone(WindowId(1)).unwrap();
    assert_eq!(
        rects(&state),
        [(2, r(0, 0, 1056, 1080)), (3, r(1056, 0, 864, 1080))]
    );
}

#[test]
fn master_gaps_and_borders() {
    let mut state = setup("general:layout = master\n");
    open(&mut state, &[1, 2, 3]);
    // The master takes 0.55 of the 1880 wide work area: 1034.
    assert_eq!(
        rects(&state),
        [
            (1, r(21, 21, 1054 - 6 - 21, 1038)),
            (2, r(1054 + 6, 21, 1899 - 1060, 540 - 6 - 21)),
            (3, r(1054 + 6, 540 + 6, 1899 - 1060, 1059 - 546))
        ]
    );
}

#[test]
fn changing_the_layout_retiles_in_the_old_order() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    let mut settings = *state.settings();
    settings.layout = Layout::Master;
    let changes = state.set_settings(settings);
    assert_eq!(changes, [Change::Layout(M1)]);
    assert_eq!(
        rects(&state),
        [
            (1, r(0, 0, 1056, 1080)),
            (2, r(1056, 0, 864, 540)),
            (3, r(1056, 540, 864, 540))
        ]
    );
}

// -- movefocus and movewindow -------------------------------------------------

/// Four windows in a 2x2 grid: 1 top left, 2 top right, 3 bottom right,
/// 4 bottom left, with 1 focused.
fn grid() -> State {
    grid_with("")
}

fn grid_with(text: &str) -> State {
    let mut state = setup(text);
    open(&mut state, &[1, 2, 3]);
    focus(&mut state, 1);
    open(&mut state, &[4]);
    assert_eq!(
        rects(&state),
        [
            (1, r(21, 21, 933, 513)),
            (2, r(966, 21, 933, 513)),
            (3, r(966, 546, 933, 513)),
            (4, r(21, 546, 933, 513))
        ]
    );
    focus(&mut state, 1);
    state
}

#[test]
fn movefocus_walks_the_grid() {
    let mut state = grid();
    let changes = dispatch(&mut state, "movefocus", "r");
    assert_eq!(changes, [Change::Focus(Some(WindowId(2)))]);
    let _changes = dispatch(&mut state, "movefocus", "d");
    assert_eq!(focused(&state), Some(3));
    let _changes = dispatch(&mut state, "movefocus", "l");
    assert_eq!(focused(&state), Some(4));
    let _changes = dispatch(&mut state, "movefocus", "u");
    assert_eq!(focused(&state), Some(1));
    let _changes = dispatch(&mut state, "movefocus", "b");
    assert_eq!(focused(&state), Some(4));
    let _changes = dispatch(&mut state, "movefocus", "t");
    assert_eq!(focused(&state), Some(1));
}

#[test]
fn movefocus_wraps_around_the_monitor() {
    // Nothing left of 1 and no monitor there: the search starts again from
    // the monitor's right edge, where 2 and 3 are, and 3 was focused more
    // recently.
    let mut state = grid();
    let changes = dispatch(&mut state, "movefocus", "l");
    assert_eq!(changes, [Change::Focus(Some(WindowId(3)))]);
    // Down from 3 wraps to the top edge, where 1 was focused more recently
    // than 2.
    let _changes = dispatch(&mut state, "movefocus", "d");
    assert_eq!(focused(&state), Some(1));

    // A window as wide as the monitor has nothing to wrap to.
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let _changes = dispatch(&mut state, "togglefloating", "");
    focus(&mut state, 1);
    assert_eq!(dispatch(&mut state, "movefocus", "r"), []);
}

#[test]
fn movefocus_with_nowhere_to_go_and_no_focus_fallback_changes_nothing() {
    let mut state = grid_with("general:no_focus_fallback = true\n");
    assert_eq!(dispatch(&mut state, "movefocus", "l"), []);
    assert_eq!(dispatch(&mut state, "movefocus", "u"), []);
    assert_eq!(focused(&state), Some(1));
    let mut empty = setup("");
    assert_eq!(dispatch(&mut empty, "movefocus", "r"), []);
}

#[test]
fn movefocus_prefers_the_most_recently_focused_neighbour() {
    let mut state = setup(BARE);
    // 1 on the left, 2 above 3 on the right.
    open(&mut state, &[1, 2, 3]);
    focus(&mut state, 1);
    let _changes = dispatch(&mut state, "movefocus", "r");
    assert_eq!(focused(&state), Some(3));
    focus(&mut state, 2);
    focus(&mut state, 1);
    let _changes = dispatch(&mut state, "movefocus", "r");
    assert_eq!(focused(&state), Some(2));
}

#[test]
fn movewindow_puts_the_window_back_past_its_neighbour() {
    let mut state = grid();
    // The point one pixel right of 1 is in 2's slot, on its left half: 1
    // comes out, 4 takes its place, and 1 splits 2's slot on the left.
    let changes = dispatch(&mut state, "movewindow", "r");
    assert_eq!(changes, [Change::Layout(M1)]);
    assert_eq!(
        rects(&state),
        [
            (1, r(966, 21, 1424 - 966, 513)),
            (2, r(1436, 21, 1899 - 1436, 513)),
            (3, r(966, 546, 933, 513)),
            (4, r(21, 21, 933, 1038))
        ]
    );
    assert_eq!(focused(&state), Some(1));

    // Down, into the left half of 3's slot.
    let _changes = dispatch(&mut state, "movewindow", "d");
    assert_eq!(
        rects(&state),
        [
            (1, r(966, 546, 1424 - 966, 513)),
            (2, r(966, 21, 933, 513)),
            (3, r(1436, 546, 1899 - 1436, 513)),
            (4, r(21, 21, 933, 1038))
        ]
    );

    // Right again: 3 is 1's sibling and a lone window that way, so 1 lands
    // on its far side, which is an exchange.
    let _changes = dispatch(&mut state, "movewindow", "r");
    assert_eq!(
        rects(&state),
        [
            (1, r(1436, 546, 1899 - 1436, 513)),
            (2, r(966, 21, 933, 513)),
            (3, r(966, 546, 1424 - 966, 513)),
            (4, r(21, 21, 933, 1038))
        ]
    );
    assert_eq!(dispatch(&mut state, "movewindow", "r"), []);
}

#[test]
fn movewindow_to_a_lone_sibling_exchanges_the_two() {
    let mut state = setup(&format!("{BARE}dwindle:default_split_ratio = 1.5\n"));
    open(&mut state, &[1, 2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1440, 1080)), (2, r(1440, 0, 480, 1080))]
    );
    let _changes = dispatch(&mut state, "movewindow", "l");
    assert_eq!(
        rects(&state),
        [(1, r(1440, 0, 480, 1080)), (2, r(0, 0, 1440, 1080))]
    );
    assert_eq!(focused(&state), Some(2));
}

#[test]
fn movewindow_swaps_in_the_master_layout() {
    let mut state = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut state, &[1, 2]);
    let _changes = dispatch(&mut state, "movewindow", "l");
    assert_eq!(
        rects(&state),
        [(1, r(1056, 0, 864, 1080)), (2, r(0, 0, 1056, 1080))]
    );
    assert_eq!(state.windows(WorkspaceId(1))[0], WindowId(2));
}

// -- Monitors -----------------------------------------------------------------

fn two_monitors() -> State {
    let mut state = setup(BARE);
    let changes = state.add_monitor(monitor(M2, 1920, 0, 1280, 1024)).unwrap();
    assert_eq!(
        changes,
        [
            Change::Workspace {
                monitor: M2,
                workspace: WorkspaceId(2)
            },
            Change::Layout(M2)
        ]
    );
    state
}

#[test]
fn a_second_monitor_gets_the_next_free_workspace() {
    let state = two_monitors();
    assert_eq!(state.active_workspace(M1), Some(WorkspaceId(1)));
    assert_eq!(state.active_workspace(M2), Some(WorkspaceId(2)));
    assert_eq!(state.focused_monitor(), Some(M1));
    let mut state = state;
    assert_eq!(
        state.add_monitor(monitor(M2, 0, 0, 1, 1)),
        Err(Error::DuplicateMonitor(M2))
    );
}

#[test]
fn movefocus_and_movewindow_cross_monitors() {
    let mut state = two_monitors();
    open(&mut state, &[1]);
    // No window to the right, so the monitor there gets focus.
    let changes = dispatch(&mut state, "movefocus", "r");
    assert_eq!(changes, [Change::FocusMonitor(M2), Change::Focus(None)]);
    open(&mut state, &[2]);
    assert_eq!(rects_on(&state, M2), [(2, r(1920, 0, 1280, 1024))]);
    let _changes = dispatch(&mut state, "movefocus", "l");
    assert_eq!(focused(&state), Some(1));
    assert_eq!(state.focused_monitor(), Some(M1));

    // Window 1's right edge touches window 2's left edge. The point one
    // pixel past it is on the left half of 2's slot on the other monitor, so
    // 1 moves there and splits it; 2 stays.
    let changes = dispatch(&mut state, "movewindow", "r");
    assert_eq!(
        changes[0],
        Change::MoveToWorkspace {
            window: WindowId(1),
            workspace: WorkspaceId(2)
        }
    );
    assert_eq!(rects_on(&state, M1), []);
    assert_eq!(
        rects_on(&state, M2),
        [(1, r(1920, 0, 640, 1024)), (2, r(2560, 0, 640, 1024))]
    );
    assert_eq!(state.workspace_of(WindowId(1)), Some(WorkspaceId(2)));
    assert_eq!(focused(&state), Some(1));
    assert_eq!(state.focused_monitor(), Some(M2));

    // With the other monitor empty, movewindow moves the window there.
    let _changes = state.window_gone(WindowId(2)).unwrap();
    let changes = dispatch(&mut state, "movewindow", "l");
    assert_eq!(
        changes[0],
        Change::MoveToWorkspace {
            window: WindowId(1),
            workspace: WorkspaceId(1)
        }
    );
    assert_eq!(rects_on(&state, M1), [(1, r(0, 0, 1920, 1080))]);
    assert_eq!(state.focused_monitor(), Some(M1));
}

#[test]
fn movewindow_in_the_master_layout_sends_the_window_across_monitors() {
    let mut state = setup(&format!("{BARE}general:layout = master\n"));
    let _changes = state.add_monitor(monitor(M2, 1920, 0, 1280, 1024)).unwrap();
    open(&mut state, &[1]);
    let _changes = dispatch(&mut state, "workspace", "2");
    open(&mut state, &[2]);
    focus(&mut state, 1);
    let _changes = dispatch(&mut state, "movewindow", "r");
    assert_eq!(rects_on(&state, M1), []);
    // 1 joins 2's workspace as a new window would: in the stack.
    assert_eq!(
        rects_on(&state, M2),
        [(2, r(1920, 0, 704, 1024)), (1, r(2624, 0, 576, 1024))]
    );
    assert_eq!(focused(&state), Some(1));
}

#[test]
fn the_neighbour_search_reaches_across_gaps_and_reserved_strips() {
    // Windows are compared by their slots grown out to the monitor's edge
    // wherever they meet the work area's, as Hyprland's
    // getWindowIdealBoundingBoxIgnoreReserved does, so the 20 pixel gaps
    // and a 30 pixel bar between the two do not hide 2 from 1.
    let mut state = setup("");
    let _changes = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: String::new(),
            id: M2,
            rect: r(1920, 0, 1280, 1024),
            reserved: Gaps {
                top: 0,
                right: 0,
                bottom: 0,
                left: 30,
            },
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    open(&mut state, &[1]);
    let _changes = dispatch(&mut state, "workspace", "2");
    open(&mut state, &[2]);
    assert_eq!(
        rects_on(&state, M2),
        [(2, r(1920 + 51, 21, 1280 - 72, 982))]
    );
    focus(&mut state, 1);
    let _changes = dispatch(&mut state, "movefocus", "r");
    assert_eq!(focused(&state), Some(2));
    let _changes = dispatch(&mut state, "movefocus", "l");
    assert_eq!(focused(&state), Some(1));
}

#[test]
fn workspace_on_another_monitor_focuses_that_monitor() {
    let mut state = two_monitors();
    let changes = dispatch(&mut state, "workspace", "2");
    assert_eq!(changes, [Change::FocusMonitor(M2)]);
    assert_eq!(state.active_workspace(M1), Some(WorkspaceId(1)));
}

#[test]
fn removing_a_monitor_moves_its_workspaces() {
    let mut state = two_monitors();
    let _changes = dispatch(&mut state, "workspace", "2");
    open(&mut state, &[7]);
    let _changes = state.remove_monitor(M2).unwrap();
    assert_eq!(state.focused_monitor(), Some(M1));
    assert_eq!(state.workspace_monitor(WorkspaceId(2)), Some(M1));
    let _changes = dispatch(&mut state, "workspace", "2");
    assert_eq!(rects_on(&state, M1), [(7, r(0, 0, 1920, 1080))]);
    assert_eq!(state.remove_monitor(M2), Err(Error::UnknownMonitor(M2)));
}

#[test]
fn workspaces_wait_for_a_monitor_when_the_last_goes() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let _changes = state.remove_monitor(M1).unwrap();
    assert_eq!(state.focused_monitor(), None);
    assert_eq!(state.open_window(WindowId(2)), Err(Error::NoMonitor));
    assert_eq!(dispatch(&mut state, "movefocus", "l"), []);
    let _changes = state.add_monitor(monitor(M2, 0, 0, 800, 600)).unwrap();
    assert_eq!(state.active_workspace(M2), Some(WorkspaceId(1)));
    assert_eq!(rects_on(&state, M2), [(1, r(0, 0, 800, 600))]);
}

// -- Workspaces ---------------------------------------------------------------

#[test]
fn workspace_switches_and_creates_on_demand() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let changes = dispatch(&mut state, "workspace", "2");
    assert_eq!(
        changes,
        [
            Change::Workspace {
                monitor: M1,
                workspace: WorkspaceId(2)
            },
            Change::Layout(M1),
            Change::Focus(None)
        ]
    );
    assert_eq!(rects(&state), []);
    open(&mut state, &[2]);
    let _changes = dispatch(&mut state, "workspace", "1");
    assert_eq!(focused(&state), Some(1));
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    // Switching to the workspace already shown changes nothing.
    assert_eq!(dispatch(&mut state, "workspace", "1"), []);
    // An empty workspace left behind is removed.
    let _changes = dispatch(&mut state, "workspace", "5");
    let _changes = dispatch(&mut state, "workspace", "1");
    assert_eq!(
        state.workspaces().collect::<Vec<_>>(),
        [WorkspaceId(1), WorkspaceId(2)]
    );
}

#[test]
fn workspace_relative_and_open_targets() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let _changes = dispatch(&mut state, "workspace", "3");
    open(&mut state, &[3]);
    let _changes = dispatch(&mut state, "workspace", "1");

    let _changes = dispatch(&mut state, "workspace", "e+1");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(3)));
    let _changes = dispatch(&mut state, "workspace", "e+1");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(1)));
    let _changes = dispatch(&mut state, "workspace", "e-1");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(3)));
    let _changes = dispatch(&mut state, "workspace", "-1");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(2)));
    let _changes = dispatch(&mut state, "workspace", "-5");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(1)));
    let _changes = dispatch(&mut state, "workspace", "+9223372036854775807");
    assert_eq!(
        state.current_workspace(),
        Some(WorkspaceId(9_223_372_036_854_775_807))
    );
    assert_eq!(
        dispatch(&mut state, "workspace", "e+9223372036854775807"),
        []
    );
}

#[test]
fn movetoworkspace_follows_the_window() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let changes = dispatch(&mut state, "movetoworkspace", "3");
    assert_eq!(
        changes,
        [
            Change::MoveToWorkspace {
                window: WindowId(2),
                workspace: WorkspaceId(3)
            },
            Change::Workspace {
                monitor: M1,
                workspace: WorkspaceId(3)
            },
            Change::Layout(M1)
        ]
    );
    assert_eq!(state.current_workspace(), Some(WorkspaceId(3)));
    assert_eq!(focused(&state), Some(2));
    assert_eq!(rects(&state), [(2, r(0, 0, 1920, 1080))]);
    let _changes = dispatch(&mut state, "workspace", "1");
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    // Moving to the workspace it is on changes nothing.
    assert_eq!(dispatch(&mut state, "movetoworkspace", "1"), []);
}

#[test]
fn movetoworkspacesilent_stays() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let changes = dispatch(&mut state, "movetoworkspacesilent", "+1");
    assert_eq!(
        changes,
        [
            Change::MoveToWorkspace {
                window: WindowId(2),
                workspace: WorkspaceId(2)
            },
            Change::Layout(M1),
            Change::Focus(Some(WindowId(1)))
        ]
    );
    assert_eq!(state.current_workspace(), Some(WorkspaceId(1)));
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    assert_eq!(state.windows(WorkspaceId(2)), [WindowId(2)]);
    let _changes = dispatch(&mut state, "workspace", "2");
    assert_eq!(focused(&state), Some(2));
}

// -- killactive ---------------------------------------------------------------

#[test]
fn killactive_asks_and_the_window_stays_until_it_is_gone() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    assert_eq!(
        dispatch(&mut state, "killactive", ""),
        [Change::Close(WindowId(2))]
    );
    assert_eq!(focused(&state), Some(2));
    assert_eq!(rects(&state).len(), 2);
    let changes = state.window_gone(WindowId(2)).unwrap();
    assert_eq!(
        changes,
        [Change::Layout(M1), Change::Focus(Some(WindowId(1)))]
    );
    let mut empty = setup(BARE);
    assert_eq!(dispatch(&mut empty, "killactive", ""), []);
}

#[test]
fn focus_returns_to_the_previously_focused_window() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    focus(&mut state, 1);
    focus(&mut state, 3);
    let _changes = state.window_gone(WindowId(3)).unwrap();
    assert_eq!(focused(&state), Some(1));
}

// -- togglefloating -----------------------------------------------------------

#[test]
fn togglefloating_centres_at_half_size_and_remembers() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let changes = dispatch(&mut state, "togglefloating", "");
    assert_eq!(
        changes,
        [
            Change::Floating {
                window: WindowId(2),
                floating: true
            },
            Change::Layout(M1)
        ]
    );
    assert!(state.is_floating(WindowId(2)));
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1920, 1080)), (2, r(480, 270, 960, 540))]
    );
    let layout = state.layout();
    assert!(layout[0].windows[1].floating);
    assert!(layout[0].windows[1].focused);

    let _changes = dispatch(&mut state, "togglefloating", "active");
    assert!(!state.is_floating(WindowId(2)));
    // Tiled again beside 1, side by side.
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );
    let _changes = dispatch(&mut state, "togglefloating", "");
    assert_eq!(rects(&state)[1], (2, r(480, 270, 960, 540)));
}

#[test]
fn floating_windows_keep_their_rect_and_sit_above_tiled_ones() {
    let mut state = setup("");
    open(&mut state, &[1]);
    let _changes = state
        .open_floating(WindowId(2), r(100, 100, 300, 200))
        .unwrap();
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(21, 21, 1878, 1038)), (2, r(100, 100, 300, 200))]
    );
    // Floating windows are not in the tiled neighbour search.
    focus(&mut state, 1);
    assert_eq!(dispatch(&mut state, "movefocus", "r"), []);
    assert_eq!(
        state.open_floating(WindowId(2), r(0, 0, 1, 1)),
        Err(Error::DuplicateWindow(WindowId(2)))
    );
}

#[test]
fn focusing_a_floating_window_raises_it() {
    let mut state = setup(BARE);
    let _changes = state.open_floating(WindowId(1), r(0, 0, 100, 100)).unwrap();
    let _changes = state
        .open_floating(WindowId(2), r(50, 50, 100, 100))
        .unwrap();
    assert_eq!(
        rects_on(&state, M1)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    focus(&mut state, 1);
    assert_eq!(
        rects_on(&state, M1)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [2, 1]
    );
}

#[test]
fn movefocus_between_floating_windows_goes_by_angle_and_distance() {
    let mut state = setup(BARE);
    let _changes = state
        .open_floating(WindowId(1), r(0, 400, 100, 100))
        .unwrap();
    let _changes = state
        .open_floating(WindowId(2), r(800, 400, 100, 100))
        .unwrap();
    let _changes = state
        .open_floating(WindowId(3), r(400, 450, 100, 100))
        .unwrap();
    // Up and to the right, but within 0.3 pi of straight up.
    let _changes = state
        .open_floating(WindowId(4), r(900, 0, 100, 100))
        .unwrap();
    focus(&mut state, 1);
    let _changes = dispatch(&mut state, "movefocus", "r");
    assert_eq!(focused(&state), Some(3));
    let _changes = dispatch(&mut state, "movefocus", "r");
    assert_eq!(focused(&state), Some(2));
    // Nothing lies within 0.3 pi of straight down from 2, so the window at
    // the smallest angle within a right angle of it wins: 3, below and far
    // to the left, over 1, which is exactly left.
    let _changes = dispatch(&mut state, "movefocus", "d");
    assert_eq!(focused(&state), Some(3));
    let _changes = dispatch(&mut state, "movefocus", "u");
    assert_eq!(focused(&state), Some(4));
}

#[test]
fn floating_windows_move_with_their_workspace_between_monitors() {
    let mut state = two_monitors();
    let _changes = state
        .open_floating(WindowId(1), r(100, 100, 300, 200))
        .unwrap();
    let _changes = dispatch(&mut state, "movetoworkspace", "2");
    assert_eq!(rects_on(&state, M2), [(1, r(1920 + 100, 100, 300, 200))]);
}

// -- fullscreen ---------------------------------------------------------------

#[test]
fn fullscreen_covers_the_monitor_without_gaps_or_border() {
    let mut state = setup("");
    open(&mut state, &[1, 2]);
    let changes = dispatch(&mut state, "fullscreen", "");
    assert_eq!(
        changes,
        [
            Change::Fullscreen {
                window: WindowId(2),
                mode: Some(FullscreenMode::Fullscreen)
            },
            Change::Layout(M1)
        ]
    );
    let layout = state.layout();
    assert_eq!(layout[0].windows.len(), 1);
    let placed = layout[0].windows[0];
    assert_eq!(placed.window, WindowId(2));
    assert_eq!(placed.rect, r(0, 0, 1920, 1080));
    assert_eq!(placed.border, 0);
    assert!(placed.fullscreen && placed.focused);
    assert_eq!(
        state.fullscreen(WorkspaceId(1)),
        Some((WindowId(2), FullscreenMode::Fullscreen))
    );

    // Nothing to move focus to past a fullscreen window.
    assert_eq!(dispatch(&mut state, "movefocus", "l"), []);
    assert_eq!(dispatch(&mut state, "movewindow", "l"), []);

    let _changes = dispatch(&mut state, "fullscreen", "0");
    assert_eq!(state.fullscreen(WorkspaceId(1)), None);
    assert_eq!(
        rects(&state),
        [(1, r(21, 21, 933, 1038)), (2, r(966, 21, 933, 1038))]
    );
}

#[test]
fn maximize_keeps_gaps_and_border() {
    let mut state = setup("");
    open(&mut state, &[1, 2]);
    let _changes = dispatch(&mut state, "fullscreen", "1");
    let layout = state.layout();
    assert_eq!(layout[0].windows.len(), 1);
    assert_eq!(layout[0].windows[0].rect, r(21, 21, 1878, 1038));
    assert_eq!(layout[0].windows[0].border, 1);
    // Another mode switches, the same mode again ends it.
    let changes = dispatch(&mut state, "fullscreen", "0");
    assert_eq!(
        changes[0],
        Change::Fullscreen {
            window: WindowId(2),
            mode: Some(FullscreenMode::Fullscreen)
        }
    );
    let _changes = dispatch(&mut state, "fullscreen", "0");
    assert_eq!(state.fullscreen(WorkspaceId(1)), None);
}

#[test]
fn a_new_window_opens_behind_a_fullscreen_one() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let _changes = dispatch(&mut state, "fullscreen", "");
    let changes = state.open_window(WindowId(2)).unwrap();
    assert_eq!(changes, []);
    assert_eq!(focused(&state), Some(1));
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    let _changes = dispatch(&mut state, "fullscreen", "");
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );
}

#[test]
fn closing_or_floating_a_fullscreen_window_ends_fullscreen() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let _changes = dispatch(&mut state, "fullscreen", "");
    let changes = dispatch(&mut state, "togglefloating", "");
    assert_eq!(
        &changes[..2],
        [
            Change::Fullscreen {
                window: WindowId(2),
                mode: None
            },
            Change::Floating {
                window: WindowId(2),
                floating: true
            }
        ]
    );
    let _changes = dispatch(&mut state, "togglefloating", "");
    let _changes = dispatch(&mut state, "fullscreen", "");
    let _changes = state.window_gone(WindowId(2)).unwrap();
    assert_eq!(state.fullscreen(WorkspaceId(1)), None);
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
}

// -- Parsing dispatchers ------------------------------------------------------

#[test]
fn dispatchers_parse_hyprlands_forms() {
    let parse = |name, arg| Dispatcher::parse(name, arg);
    assert_eq!(
        parse("movefocus", "l"),
        Ok(Dispatcher::MoveFocus(Direction::Left))
    );
    assert_eq!(
        parse("MoveWindow", " u "),
        Ok(Dispatcher::MoveWindow(Direction::Up))
    );
    assert_eq!(
        parse("workspace", "4"),
        Ok(Dispatcher::Workspace(WorkspaceTarget::Id(WorkspaceId(4))))
    );
    assert_eq!(
        parse("workspace", "+2"),
        Ok(Dispatcher::Workspace(WorkspaceTarget::Relative(2)))
    );
    assert_eq!(
        parse("movetoworkspace", "e-1"),
        Ok(Dispatcher::MoveToWorkspace(WorkspaceTarget::Open(-1)))
    );
    assert_eq!(parse("killactive", "whatever"), Ok(Dispatcher::KillActive));
    assert_eq!(
        parse("fullscreen", "1"),
        Ok(Dispatcher::Fullscreen(FullscreenMode::Maximized))
    );
    // Hyprland clamps a workspace number to 1.
    assert_eq!(
        parse("workspace", "0"),
        Ok(Dispatcher::Workspace(WorkspaceTarget::Id(WorkspaceId(1))))
    );
}

#[test]
fn bad_dispatchers_and_arguments_are_errors_not_panics() {
    assert_eq!(
        Dispatcher::parse("exec", "kitty"),
        Err(Error::UnknownDispatcher("exec".to_owned()))
    );
    for (name, arg) in [
        ("movefocus", ""),
        ("movefocus", "left"),
        ("movewindow", "x"),
        ("workspace", ""),
        ("workspace", "-0x1"),
        ("workspace", "name:"),
        ("workspace", "special:"),
        ("workspace", "e"),
        ("workspace", "e1"),
        ("workspace", "99999999999999999999"),
        ("movetoworkspace", "2,class:kitty"),
        ("togglefloating", "class:kitty"),
        ("fullscreen", "3"),
    ] {
        assert_eq!(
            Dispatcher::parse(name, arg),
            Err(Error::BadArgument {
                dispatcher: name.to_owned(),
                arg: arg.to_owned()
            }),
            "{name}, {arg}"
        );
    }
    let mut state = setup(BARE);
    let error = state.dispatch_str("movefocus", "q").unwrap_err();
    assert_eq!(error.to_string(), "Invalid argument for movefocus: q");
}

#[test]
fn every_dispatcher_on_an_empty_state_is_harmless() {
    let mut state = State::new(Settings::default());
    for (name, arg) in [
        ("movefocus", "l"),
        ("movewindow", "r"),
        ("workspace", "3"),
        ("workspace", "e+1"),
        ("movetoworkspace", "2"),
        ("movetoworkspacesilent", "-1"),
        ("killactive", ""),
        ("togglefloating", ""),
        ("fullscreen", "1"),
    ] {
        assert_eq!(dispatch(&mut state, name, arg), [], "{name}, {arg}");
    }
}

#[test]
fn binds_from_the_configuration_run() {
    let config = config(
        "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
         bind = SUPER, H, movefocus, l\nbind = SUPER, 2, workspace, 2\nbind = SUPER, Q, killactive,\n",
    );
    let mut state = State::from_config(&config);
    let _changes = state.add_monitor(monitor(M1, 0, 0, 1920, 1080)).unwrap();
    open(&mut state, &[1, 2]);
    let [left, workspace, kill] = &config.binds[..] else {
        panic!("three binds");
    };
    let _changes = state.dispatch_bind(left).unwrap();
    assert_eq!(focused(&state), Some(1));
    assert_eq!(
        state.dispatch_bind(kill).unwrap(),
        [Change::Close(WindowId(1))]
    );
    let _changes = state.dispatch_bind(workspace).unwrap();
    assert_eq!(state.current_workspace(), Some(WorkspaceId(2)));
}

#[test]
fn opening_reports_layout_and_focus() {
    let mut state = setup(BARE);
    let changes = state.open_window(WindowId(1)).unwrap();
    assert_eq!(
        changes,
        [Change::Layout(M1), Change::Focus(Some(WindowId(1)))]
    );
    assert_eq!(
        state.open_window(WindowId(1)),
        Err(Error::DuplicateWindow(WindowId(1)))
    );
    assert_eq!(
        state.focus_window(WindowId(9)),
        Err(Error::UnknownWindow(WindowId(9)))
    );
}

#[test]
fn extreme_geometry_does_not_panic() {
    let mut state = State::from_config(&config(
        "general:gaps_in = 9223372036854775807\ngeneral:gaps_out = -9223372036854775808\n\
         general:border_size = 9223372036854775807\ndwindle:split_width_multiplier = -1\n",
    ));
    let _changes = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: String::new(),
            id: M1,
            rect: r(i64::MAX, i64::MIN, i64::MAX, 0),
            reserved: Gaps::all(i64::MIN),
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    open(&mut state, &[1, 2, 3]);
    let _changes = dispatch(&mut state, "togglefloating", "");
    let _changes = dispatch(&mut state, "movefocus", "l");
    let _changes = dispatch(&mut state, "movewindow", "u");
    let _changes = dispatch(&mut state, "fullscreen", "1");
    let _layout = state.layout();
}

// ---------------------------------------------------------------------------
// Special workspaces
//
// Hyprland's scratchpad: a workspace shown *over* the monitor's own rather
// than instead of it, with a negative id and a `special:` name. A compositor
// that switched to it instead of showing it over would be a compositor where
// the scratchpad hid your work.
// ---------------------------------------------------------------------------

#[test]
fn a_special_workspace_is_shown_over_the_monitors_own() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    assert_eq!(state.layout()[0].windows.len(), 2);

    // Nothing on it yet: it is shown, and the two windows are still there.
    let _ = state.dispatch_str("togglespecialworkspace", "").unwrap();
    assert_eq!(
        state.special_on(M1),
        Some(WorkspaceId(-99)),
        "bare `special` is Hyprland's -99"
    );
    assert_eq!(state.workspace_name(WorkspaceId(-99)), "special:special");
    assert_eq!(
        state.layout()[0].windows.len(),
        2,
        "an empty scratchpad hid the windows"
    );
    assert_eq!(
        state.layout()[0].workspace,
        WorkspaceId(1),
        "the monitor still shows its own workspace"
    );

    // A window moved onto it is drawn over them.
    let _ = state.dispatch_str("movetoworkspace", "special").unwrap();
    let windows = state.layout()[0].windows.clone();
    assert_eq!(windows.len(), 2, "one of the two went to the scratchpad");
    assert_eq!(
        windows.last().map(|placed| placed.window),
        Some(WindowId(2)),
        "the scratchpad's window is on top: {windows:?}"
    );

    // And toggling it again hides it.
    let _ = state.dispatch_str("togglespecialworkspace", "").unwrap();
    assert_eq!(state.special_on(M1), None);
    assert_eq!(state.layout()[0].windows.len(), 1);
}

#[test]
fn a_named_special_workspace_gets_an_id_of_its_own() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);

    let _ = state
        .dispatch_str("togglespecialworkspace", "term")
        .unwrap();
    let term = state.special_on(M1).expect("a special workspace");
    assert_eq!(state.workspace_name(term), "special:term");
    assert!(State::is_special(term), "{term:?} is not in the range");

    // Another name is another workspace, and toggling it swaps which is
    // shown rather than showing both.
    let _ = state
        .dispatch_str("togglespecialworkspace", "notes")
        .unwrap();
    let notes = state.special_on(M1).expect("a special workspace");
    assert_ne!(notes, term);
    assert_eq!(state.workspace_name(notes), "special:notes");

    // And the first one is still there, with its id remembered.
    let _ = state
        .dispatch_str("togglespecialworkspace", "term")
        .unwrap();
    assert_eq!(state.special_on(M1), Some(term));
}

/// Hyprland's range is −99 to −2; a numbered workspace is never special and
/// a special one never shows in the normal rotation.
#[test]
fn the_special_range_is_hyprlands() {
    assert!(State::is_special(WorkspaceId(-99)));
    assert!(State::is_special(WorkspaceId(-2)));
    assert!(!State::is_special(WorkspaceId(-1)));
    assert!(!State::is_special(WorkspaceId(0)));
    assert!(!State::is_special(WorkspaceId(1)));
    assert!(!State::is_special(WorkspaceId(-100)));
}

/// A numbered workspace's name is its number, which is what `hyprctl
/// workspaces` prints for one.
#[test]
fn a_numbered_workspace_is_named_by_its_number() {
    let state = setup(BARE);
    assert_eq!(state.workspace_name(WorkspaceId(1)), "1");
    assert_eq!(state.workspace_name(WorkspaceId(7)), "7");
}

/// The scratchpad takes the focus when it has something on it, and gives it
/// back when it is hidden.
#[test]
fn showing_a_scratchpad_with_a_window_focuses_it() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let _ = state.dispatch_str("togglespecialworkspace", "").unwrap();
    let _ = state.dispatch_str("movetoworkspace", "special").unwrap();
    assert_eq!(state.focused_window(), Some(WindowId(2)));

    let _ = state.dispatch_str("togglespecialworkspace", "").unwrap();
    assert_eq!(
        state.focused_window(),
        Some(WindowId(1)),
        "hiding it left the focus on a window nobody can see"
    );

    let _ = state.dispatch_str("togglespecialworkspace", "").unwrap();
    assert_eq!(
        state.focused_window(),
        Some(WindowId(2)),
        "showing it again did not take the focus back"
    );
}

// ---------------------------------------------------------------------------
// Groups
//
// Hyprland's tabs: windows sharing one tiling slot, of which one is shown.
// The slot stays where it is while the tabs are cycled, which is the whole
// point of them -- a group that moved the layout each time would be a
// workspace switch with extra steps.
// ---------------------------------------------------------------------------

/// The windows the layout draws, in order.
fn drawn(state: &State) -> Vec<WindowId> {
    state.layout()[0]
        .windows
        .iter()
        .map(|placed| placed.window)
        .collect()
}

#[test]
fn a_group_shows_one_member_in_one_slot() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    let before = state.layout()[0].windows.clone();
    assert_eq!(before.len(), 3);

    // Window 3 has the focus; make it a group and move 2 into it.
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    assert_eq!(state.group(WindowId(3)).map(|g| g.members.len()), Some(1));

    let _ = state.dispatch_str("movefocus", "l").unwrap();
    let focused = state.focused_window().expect("a window");
    let _ = state.dispatch_str("moveintogroup", "r").unwrap();
    let group = state.group(WindowId(3)).expect("the group");
    assert_eq!(group.members, [WindowId(3), focused]);
    assert_eq!(group.showing(), Some(focused), "the one moved in is shown");

    // Two slots now, not three, and one of them is the group's.
    let after = state.layout()[0].windows.clone();
    assert_eq!(after.len(), 2, "{after:?}");
    assert!(drawn(&state).contains(&focused));
}

#[test]
fn cycling_a_group_changes_what_is_drawn_and_not_where() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    let _ = state.dispatch_str("movefocus", "l").unwrap();
    let _ = state.dispatch_str("moveintogroup", "r").unwrap();

    let places = |state: &State| -> Vec<Rect> {
        state.layout()[0]
            .windows
            .iter()
            .map(|placed| placed.rect)
            .collect()
    };
    let before = places(&state);
    let showing = drawn(&state);

    let _ = state.dispatch_str("changegroupactive", "f").unwrap();
    assert_eq!(places(&state), before, "the layout moved");
    assert_ne!(drawn(&state), showing, "the same member is still shown");

    // Forward again comes back round: a group of two wraps.
    let _ = state.dispatch_str("changegroupactive", "f").unwrap();
    assert_eq!(drawn(&state), showing);
    // And back goes the other way.
    let _ = state.dispatch_str("changegroupactive", "b").unwrap();
    assert_ne!(drawn(&state), showing);

    // An index counts from one, as Hyprland's does: two is the one moved
    // in, since the head was there first. Out of range does nothing rather
    // than showing a member that is not there.
    let _ = state.dispatch_str("changegroupactive", "2").unwrap();
    assert_eq!(drawn(&state), showing);
    let _ = state.dispatch_str("changegroupactive", "9").unwrap();
    assert_eq!(drawn(&state), showing);
}

#[test]
fn the_active_member_is_the_focused_one() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    let _ = state.dispatch_str("movefocus", "l").unwrap();
    let moved = state.focused_window().expect("a window");
    let _ = state.dispatch_str("moveintogroup", "r").unwrap();
    assert_eq!(state.focused_window(), Some(moved));

    let _ = state.dispatch_str("changegroupactive", "f").unwrap();
    let showing = state.group(moved).and_then(Group::showing);
    assert_eq!(state.focused_window(), showing);
}

#[test]
fn taking_a_window_out_puts_it_back_in_the_tiling() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    let _ = state.dispatch_str("movefocus", "l").unwrap();
    let _ = state.dispatch_str("moveintogroup", "r").unwrap();
    assert_eq!(state.layout()[0].windows.len(), 1);

    let _ = state.dispatch_str("moveoutofgroup", "").unwrap();
    assert_eq!(state.layout()[0].windows.len(), 2, "it did not come back");
    // A group of one is no group, which is what Hyprland leaves behind.
    assert_eq!(state.group(WindowId(1)), None);
    assert_eq!(state.group(WindowId(2)), None);
}

#[test]
fn dissolving_a_group_puts_every_member_back() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    for _ in 0..2 {
        let _ = state.dispatch_str("movefocus", "l").unwrap();
        let _ = state.dispatch_str("moveintogroup", "r").unwrap();
    }
    assert_eq!(state.group(WindowId(3)).map(|g| g.members.len()), Some(3));
    assert_eq!(state.layout()[0].windows.len(), 1);

    let _ = state.dispatch_str("togglegroup", "").unwrap();
    assert_eq!(state.group(WindowId(3)), None);
    let mut back = drawn(&state);
    back.sort_unstable();
    assert_eq!(back, [WindowId(1), WindowId(2), WindowId(3)]);
}

/// A window that goes while it is grouped takes itself out of the group, and
/// a group whose head went keeps its place in the tiling.
#[test]
fn a_window_that_goes_leaves_its_group_whole() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    for _ in 0..2 {
        let _ = state.dispatch_str("movefocus", "l").unwrap();
        let _ = state.dispatch_str("moveintogroup", "r").unwrap();
    }
    // The head goes.
    let _ = state.window_gone(WindowId(3)).unwrap();
    let group = state.group(WindowId(1)).expect("the group survived");
    assert_eq!(group.members.len(), 2);
    assert!(!group.members.contains(&WindowId(3)));
    assert_eq!(state.layout()[0].windows.len(), 1, "the slot went with it");

    // And down to one member, it stops being a group.
    let showing = state.group(WindowId(1)).and_then(Group::showing).unwrap();
    let _ = state.window_gone(showing).unwrap();
    assert_eq!(state.group(WindowId(1)), None);
    assert_eq!(state.group(WindowId(2)), None);
    assert_eq!(state.layout()[0].windows.len(), 1);
}

#[test]
fn lockgroups_is_read_and_reported() {
    let mut state = setup(BARE);
    assert!(!state.groups_locked());
    let _ = state.dispatch_str("lockgroups", "lock").unwrap();
    assert!(state.groups_locked());
    let _ = state.dispatch_str("lockgroups", "toggle").unwrap();
    assert!(!state.groups_locked());
    let _ = state.dispatch_str("lockgroups", "unlock").unwrap();
    assert!(!state.groups_locked());
    assert!(state.dispatch_str("lockgroups", "sideways").is_err());
}

/// A floating window has no slot to share, so `togglegroup` does nothing for
/// one -- which is what Hyprland does too.
#[test]
fn a_floating_window_cannot_be_a_group() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let _ = state.dispatch_str("togglefloating", "").unwrap();
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    assert_eq!(state.group(WindowId(1)), None);
}

// ---------------------------------------------------------------------------
// The monitor dispatchers
//
// Which monitor an argument names is `CMonitorQueryCore::fromConfigString`'s:
// `current`, a direction, `+N`/`-N` along the list, an id counting from zero,
// or a name.
// ---------------------------------------------------------------------------

#[test]
fn focusmonitor_takes_every_form_hyprland_takes() {
    let mut state = two_monitors();
    assert_eq!(state.focused_monitor(), Some(M1));

    // A direction.
    let _ = state.dispatch_str("focusmonitor", "r").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2));
    let _ = state.dispatch_str("focusmonitor", "l").unwrap();
    assert_eq!(state.focused_monitor(), Some(M1));

    // Along the list, wrapping both ways.
    let _ = state.dispatch_str("focusmonitor", "+1").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2));
    let _ = state.dispatch_str("focusmonitor", "+1").unwrap();
    assert_eq!(state.focused_monitor(), Some(M1), "it did not wrap");
    let _ = state.dispatch_str("focusmonitor", "-1").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2));

    // An id, which Hyprland counts from zero.
    let _ = state.dispatch_str("focusmonitor", "0").unwrap();
    assert_eq!(state.focused_monitor(), Some(M1));
    let _ = state.dispatch_str("focusmonitor", "1").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2));
    // One that is not there changes nothing.
    let _ = state.dispatch_str("focusmonitor", "7").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2));

    // A name, which is the connector's.
    let _ = state.dispatch_str("focusmonitor", "Virtual-1").unwrap();
    assert_eq!(state.focused_monitor(), Some(M1));
    let _ = state.dispatch_str("focusmonitor", "Virtual-2").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2));
    let _ = state.dispatch_str("focusmonitor", "DP-9").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2), "a name nothing has");

    // `current` stays, and nothing at all is an error.
    let _ = state.dispatch_str("focusmonitor", "current").unwrap();
    assert_eq!(state.focused_monitor(), Some(M2));
    assert!(state.dispatch_str("focusmonitor", "").is_err());
}

/// Focusing a monitor focuses the window last focused on it, which is what
/// makes `focusmonitor` a focus change rather than a cursor move.
#[test]
fn focusmonitor_takes_the_window_with_it() {
    let mut state = two_monitors();
    open(&mut state, &[1]);
    let _ = state.dispatch_str("focusmonitor", "r").unwrap();
    assert_eq!(state.focused_window(), None, "the second monitor is empty");
    let _ = state.dispatch_str("focusmonitor", "l").unwrap();
    assert_eq!(state.focused_window(), Some(WindowId(1)));
}

#[test]
fn movewindow_to_a_monitor_sends_the_window_and_the_focus() {
    let mut state = two_monitors();
    open(&mut state, &[1]);
    let changes = state.dispatch_str("movewindow", "mon:Virtual-2").unwrap();
    assert!(
        changes.contains(&Change::MoveToWorkspace {
            window: WindowId(1),
            workspace: WorkspaceId(2),
        }),
        "{changes:?}"
    );
    assert_eq!(state.workspace_of(WindowId(1)), Some(WorkspaceId(2)));
    assert_eq!(state.focused_monitor(), Some(M2));
    assert_eq!(state.focused_window(), Some(WindowId(1)));

    // `silent` sends the window and leaves the focus where it was.
    let _ = state.dispatch_str("movewindow", "mon:0").unwrap();
    open(&mut state, &[2]);
    let _ = state.dispatch_str("movewindow", "mon:1 silent").unwrap();
    assert_eq!(state.workspace_of(WindowId(2)), Some(WorkspaceId(2)));
    assert_eq!(state.focused_monitor(), Some(M1));
}

#[test]
fn a_workspace_can_be_moved_to_another_monitor() {
    let mut state = two_monitors();
    open(&mut state, &[1]);
    assert_eq!(state.workspace_monitor(WorkspaceId(1)), Some(M1));

    let _ = state
        .dispatch_str("movecurrentworkspacetomonitor", "Virtual-2")
        .unwrap();
    assert_eq!(state.workspace_monitor(WorkspaceId(1)), Some(M2));
    assert_eq!(state.active_workspace(M2), Some(WorkspaceId(1)));
    // The monitor it left is showing something rather than nothing.
    let left = state.active_workspace(M1).expect("a workspace");
    assert_ne!(left, WorkspaceId(1));

    // And by name, the other way.
    let _ = state
        .dispatch_str("moveworkspacetomonitor", "1 Virtual-1")
        .unwrap();
    assert_eq!(state.workspace_monitor(WorkspaceId(1)), Some(M1));
    assert_eq!(state.active_workspace(M1), Some(WorkspaceId(1)));
}

#[test]
fn two_monitors_can_swap_what_they_show() {
    let mut state = two_monitors();
    let (one, two) = (
        state.active_workspace(M1).expect("a workspace"),
        state.active_workspace(M2).expect("a workspace"),
    );
    assert_ne!(one, two);

    let _ = state.dispatch_str("swapactiveworkspaces", "0 1").unwrap();
    assert_eq!(state.active_workspace(M1), Some(two));
    assert_eq!(state.active_workspace(M2), Some(one));
    assert_eq!(state.workspace_monitor(one), Some(M2));
    assert_eq!(state.workspace_monitor(two), Some(M1));

    // The same monitor twice does nothing, and one argument is an error.
    let _ = state.dispatch_str("swapactiveworkspaces", "0 0").unwrap();
    assert_eq!(state.active_workspace(M1), Some(two));
    assert!(state.dispatch_str("swapactiveworkspaces", "0").is_err());
}

/// A window told to float at a rectangle floats there, whether it was tiled,
/// floating already or in a group.
#[test]
fn a_window_can_be_told_to_float_somewhere() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let where_it_goes = Rect::new(100, 80, 400, 300);

    let changes = state.float_window(WindowId(1), where_it_goes).unwrap();
    assert!(
        changes.contains(&Change::Floating {
            window: WindowId(1),
            floating: true
        }),
        "{changes:?}"
    );
    assert!(state.is_floating(WindowId(1)));
    let placed = |state: &State, window: WindowId| -> Rect {
        state.layout()[0]
            .windows
            .iter()
            .find(|placed| placed.window == window)
            .expect("the window")
            .rect
    };
    assert_eq!(placed(&state, WindowId(1)), where_it_goes);
    // The other window has the whole tiling to itself now.
    assert!(!state.is_floating(WindowId(2)));

    // Again, with another rectangle: a window that floats already is moved.
    let moved = Rect::new(10, 20, 200, 150);
    let _ = state.float_window(WindowId(1), moved).unwrap();
    assert_eq!(placed(&state, WindowId(1)), moved);

    // A window this state does not have is an error, not a new window.
    assert_eq!(
        state.float_window(WindowId(9), moved),
        Err(Error::UnknownWindow(WindowId(9)))
    );
}

/// Floating a grouped window takes it out of its group, since a floating
/// window has no slot to share.
#[test]
fn floating_a_grouped_window_takes_it_out_of_the_group() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let _ = state.dispatch_str("togglegroup", "").unwrap();
    let _ = state.dispatch_str("movefocus", "l").unwrap();
    let _ = state.dispatch_str("moveintogroup", "r").unwrap();
    let head = state.group(WindowId(2)).expect("a group").members[0];
    let member = state
        .group(head)
        .expect("a group")
        .members
        .get(1)
        .copied()
        .expect("a second member");

    let _ = state
        .float_window(member, Rect::new(0, 0, 200, 200))
        .unwrap();
    assert!(state.is_floating(member));
    assert_eq!(state.group(member), None, "it is still in a group");
}

/// The focus history, most recent first, which is what a `no_focus` rule
/// gives the focus back with.
#[test]
fn the_focus_order_is_the_history_backwards() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    assert_eq!(
        state.windows_in_focus_order().first().copied(),
        Some(WindowId(3))
    );
    let _ = state.focus_window(WindowId(1)).unwrap();
    let order = state.windows_in_focus_order();
    assert_eq!(order.first().copied(), Some(WindowId(1)));
    assert_eq!(order.len(), 3, "{order:?}");
}

// ---------------------------------------------------------------------------
// The dispatchers a person's configuration binds that the layout had not
// answered: `setfloating` and its relatives, the ones that move and resize a
// floating window, the ones that walk the tiling, and the ones that name a
// workspace or tag a window.
//
// Each is Hyprland's by name and by what its argument means, and each is
// checked here rather than on a screen: a dispatcher is rectangles and ids.
// ---------------------------------------------------------------------------

/// `setfloating` and `settiled` always do the same thing; `togglefloating`
/// is the one that turns the window over.
#[test]
fn setfloating_and_settiled_do_not_toggle() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 1);
    assert!(!state.is_floating(WindowId(1)));

    let _ = dispatch(&mut state, "setfloating", "");
    assert!(state.is_floating(WindowId(1)));
    // Twice is still floating, which is the whole difference from the
    // toggle.
    let changes = dispatch(&mut state, "setfloating", "");
    assert_eq!(changes, [], "a window that already floats is left alone");
    assert!(state.is_floating(WindowId(1)));

    let _ = dispatch(&mut state, "settiled", "");
    assert!(!state.is_floating(WindowId(1)));
    let changes = dispatch(&mut state, "settiled", "");
    assert_eq!(changes, []);
}

/// `centerwindow` puts a floating window in the middle of the work area,
/// and `centerwindow 1` in the middle of the whole monitor.
#[test]
fn centerwindow_centres_a_floating_window() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "setfloating", "");
    let _ = state
        .float_window(WindowId(1), Rect::new(0, 0, 400, 200))
        .unwrap();

    let _ = dispatch(&mut state, "centerwindow", "");
    let at = rects(&state)[0].1;
    assert_eq!(
        (at.x, at.y),
        ((1920 - 400) / 2, (1080 - 200) / 2),
        "the window is in the middle"
    );
    assert_eq!((at.width, at.height), (400, 200), "and is the size it was");

    // A tiled window is where the tiling put it, and this does nothing.
    let _ = dispatch(&mut state, "settiled", "");
    let before = rects(&state);
    let changes = dispatch(&mut state, "centerwindow", "");
    assert_eq!(changes, []);
    assert_eq!(rects(&state), before);
}

/// `moveactive` and `resizeactive` take a distance, or a position and a size
/// after `exact`.
#[test]
fn moveactive_and_resizeactive_move_and_size_a_floating_window() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "setfloating", "");
    let _ = state
        .float_window(WindowId(1), Rect::new(100, 100, 400, 200))
        .unwrap();

    let _ = dispatch(&mut state, "moveactive", "30 -20");
    assert_eq!(rects(&state)[0].1, Rect::new(130, 80, 400, 200));

    let _ = dispatch(&mut state, "resizeactive", "50 50");
    assert_eq!(rects(&state)[0].1, Rect::new(130, 80, 450, 250));

    // `exact` makes the two numbers a place and a size rather than a change.
    let _ = dispatch(&mut state, "moveactive", "exact 10 10");
    assert_eq!(rects(&state)[0].1, Rect::new(10, 10, 450, 250));
    let _ = dispatch(&mut state, "resizeactive", "exact 640 480");
    assert_eq!(rects(&state)[0].1, Rect::new(10, 10, 640, 480));

    // And a window is never resized out of existence.
    let _ = dispatch(&mut state, "resizeactive", "-10000 -10000");
    let at = rects(&state)[0].1;
    assert!(at.width >= 1 && at.height >= 1, "{at:?}");
}

/// `resizeactive` on a *tiled* window moves the split it sits under, which
/// is the only thing a tiled window's size can come from.
///
/// Hyprland's `CDwindleAlgorithm::resizeTarget` at `CORNER_NONE`: the
/// nearest split running down the screen takes the sideways distance as a
/// share of half the box it divides, and the nearest running across takes
/// the downward one.
#[test]
fn resizeactive_on_a_tiled_window_moves_the_split_under_it() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 1);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 960, 1080)),
            (2, Rect::new(960, 0, 960, 1080))
        ],
        "1920 wide splits side by side, in half"
    );

    // 100 pixels of a box 1920 wide is 100 * 2 / 1920 of the ratio, and the
    // first child is `box.w / 2 * ratio`: 960 * (1 + 100 * 2 / 1920) = 1060.
    let _ = dispatch(&mut state, "resizeactive", "100 0");
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 1060, 1080)),
            (2, Rect::new(1060, 0, 860, 1080))
        ],
        "the split moved, and the window beside it gave up what this one took"
    );

    // Both windows run the full height of the work area, so there is no
    // split across the screen to move and no edge of its own to move
    // either: Hyprland drops that part of the distance rather than
    // applying it somewhere arbitrary.
    let before = rects(&state);
    let _ = dispatch(&mut state, "resizeactive", "0 100");
    assert_eq!(rects(&state), before, "nothing to move downwards");
}

/// A tiled window against the work area's far edge moves the one edge it
/// has, which makes a positive distance *narrow* it.
///
/// This looks backwards and is Hyprland's: at `CORNER_NONE` the ratio only
/// ever grows with the distance, and a ratio that grows gives the room to
/// the left-hand window. The window on the right is against the screen's
/// right edge, so the only edge it can move is its left one, and dragging
/// that rightwards is asking for less. A drag that grabbed a border says
/// which corner it grabbed and does not need the guess.
#[test]
fn a_tiled_window_against_the_far_edge_moves_its_only_free_edge() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 2);

    let _ = dispatch(&mut state, "resizeactive", "100 0");
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 1060, 1080)),
            (2, Rect::new(1060, 0, 860, 1080))
        ]
    );
}

/// `exact` is a size, and a tiling is told the difference from the size the
/// window has now: a tree holds proportions, so a number of pixels only
/// means something beside what is there.
#[test]
fn resizeactive_exact_asks_a_tiling_for_the_difference() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 1);

    let _ = dispatch(&mut state, "resizeactive", "exact 1060 1080");
    assert_eq!(rects(&state)[0].1, Rect::new(0, 0, 1060, 1080));
}

/// The only window on a workspace has no split above it to move, and a
/// ratio is held to Hyprland's own 0.1 to 1.9 however far a resize asks.
#[test]
fn a_tiled_resize_needs_a_split_and_is_held_inside_its_bounds() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let alone = rects(&state);
    let changes = dispatch(&mut state, "resizeactive", "100 0");
    assert_eq!(changes, [], "nothing above it to move");
    assert_eq!(rects(&state), alone);

    open(&mut state, &[2]);
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "resizeactive", "100000 0");
    assert_eq!(
        rects(&state)[0].1,
        Rect::new(0, 0, 1824, 1080),
        "the ratio stops at 1.9, which is 960 * 1.9"
    );
}

/// A grabbed corner says which of the splits above a tiled window moves,
/// which is what the guess at `CORNER_NONE` was standing in for.
///
/// Three windows on a screen wide enough that both splits run down it: the
/// root divides 3840 in half, and the right half divides again, so window 2
/// has two such splits above it. Grabbing its right edge moves the inner
/// one -- the split that edge *is* -- and takes the room from window 3,
/// leaving window 1 alone.
#[test]
fn a_grabbed_corner_picks_which_split_moves() {
    let mut state = wide_three();
    let _ = state
        .resize_window_pixel_at(
            WindowId(2),
            &distance(100),
            Corner {
                right: true,
                ..Corner::NONE
            },
        )
        .unwrap();
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 1920, 1080)),
            (2, Rect::new(1920, 0, 1060, 1080)),
            (3, Rect::new(2980, 0, 860, 1080)),
        ]
    );
}

/// `dwindle:smart_resizing`, Hyprland's default: with the split on the
/// grabbed side moved, the next one the other way moves back by as much, so
/// only the two windows either side of the grabbed edge change size.
///
/// The same three windows, and window 2's *left* edge. That edge is the
/// root's split, so the root moves and window 1 takes the 100 pixels. What
/// the second half of the algorithm is for is window 3: the inner split
/// divides a box that just got 100 narrower, so without moving it back
/// windows 2 and 3 would come out 910 each and window 3 would have shrunk
/// for a drag on the other side of window 2. Moved back, window 2 pays the
/// whole 100 and window 3 keeps its 960.
#[test]
fn smart_resizing_gives_the_room_back_to_the_inner_split() {
    let mut state = wide_three();
    let _ = state
        .resize_window_pixel_at(
            WindowId(2),
            &distance(100),
            Corner {
                left: true,
                ..Corner::NONE
            },
        )
        .unwrap();
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 2020, 1080)),
            (2, Rect::new(2020, 0, 860, 1080)),
            (3, Rect::new(2880, 0, 960, 1080)),
        ]
    );
}

/// A floating window resized by a grabbed edge moves that edge and leaves
/// the opposite one where it is, which is what makes a border drag look
/// like one.
#[test]
fn a_grabbed_edge_moves_a_floating_window_by_that_edge() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "setfloating", "");
    let _ = state
        .float_window(WindowId(1), Rect::new(100, 100, 400, 200))
        .unwrap();

    // The left edge: the origin moves with it and the right edge, at 500,
    // stays where it is.
    let by = Move {
        x: 40,
        y: 0,
        exact: false,
    };
    let _ = state
        .resize_window_pixel_at(
            WindowId(1),
            &by,
            Corner {
                left: true,
                ..Corner::NONE
            },
        )
        .unwrap();
    assert_eq!(rects(&state)[0].1, Rect::new(140, 100, 360, 200));

    // The bottom edge: the origin is left alone.
    let by = Move {
        x: 0,
        y: 30,
        exact: false,
    };
    let _ = state
        .resize_window_pixel_at(
            WindowId(1),
            &by,
            Corner {
                bottom: true,
                ..Corner::NONE
            },
        )
        .unwrap();
    assert_eq!(rects(&state)[0].1, Rect::new(140, 100, 360, 230));
}

/// `general:resize_on_border`: the ring around a window, as wide as the
/// border plus `general:extend_border_grab_area`, grabs an edge; inside the
/// window belongs to the client and nothing is grabbed.
#[test]
fn a_border_is_grabbed_around_a_window_and_not_inside_it() {
    const ON: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
                      general:resize_on_border = true\ngeneral:extend_border_grab_area = 10\n";
    let mut state = setup(ON);
    open(&mut state, &[1, 2]);
    // Window 1 is 0..960, window 2 is 960..1920, both the full height.

    // Inside window 1, nowhere near an edge: the client's.
    assert_eq!(state.border_at((400.0, 500.0)), None);

    // Just past window 1's right edge, which is window 2's left edge: the
    // topmost window wins, and window 2 is the one drawn later.
    assert_eq!(
        state.border_at((955.0, 500.0)),
        Some((
            WindowId(2),
            Corner {
                left: true,
                ..Corner::NONE
            }
        )),
        "within 10 of window 2's left edge, and outside window 2"
    );

    // Ten pixels is the reach: eleven away from any edge is nobody's.
    assert_eq!(state.border_at((948.0, 500.0)), None);

    // The screen's own top-left corner names both edges of window 1.
    assert_eq!(
        state.border_at((-3.0, -3.0)),
        Some((
            WindowId(1),
            Corner {
                left: true,
                top: true,
                ..Corner::NONE
            }
        ))
    );
}

/// With `general:resize_on_border` off -- Hyprland's default -- no press is
/// ever a border grab, whatever the grab area says.
#[test]
fn no_border_is_grabbed_when_the_option_is_off() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    assert_eq!(state.border_at((955.0, 500.0)), None);
    assert_eq!(state.border_at((-3.0, -3.0)), None);
}

/// `general:snap:enabled`: a dragged floating window that comes near the
/// screen's edge lands flush against it, keeping its size.
#[test]
fn a_dragged_window_snaps_to_the_screens_edge() {
    const SNAP: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
                        general:snap:enabled = true\ngeneral:snap:monitor_gap = 10\n";
    let mut state = setup(SNAP);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "setfloating", "");
    let _ = state
        .float_window(WindowId(1), Rect::new(100, 100, 400, 200))
        .unwrap();

    // Dragged to within the gap of the left edge: it lands on it, and keeps
    // its 400 by 200.
    let _ = state
        .drag_window_pixel(
            WindowId(1),
            &Move {
                x: -95,
                y: 0,
                exact: false,
            },
        )
        .unwrap();
    assert_eq!(rects(&state)[0].1, Rect::new(0, 100, 400, 200));

    // A drag that stops further out than the gap is left where it was put.
    let _ = state
        .drag_window_pixel(
            WindowId(1),
            &Move {
                x: 300,
                y: 0,
                exact: false,
            },
        )
        .unwrap();
    assert_eq!(rects(&state)[0].1, Rect::new(300, 100, 400, 200));
}

/// A drag *resizing* a window moves only the grabbed edge to the snap, so
/// the window changes size rather than sliding: Hyprland's `snapResize`
/// against its `snapMove`.
#[test]
fn a_dragged_edge_snaps_without_taking_the_window_with_it() {
    const SNAP: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
                        general:snap:enabled = true\ngeneral:snap:monitor_gap = 10\n";
    let mut state = setup(SNAP);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "setfloating", "");
    let _ = state
        .float_window(WindowId(1), Rect::new(100, 100, 400, 200))
        .unwrap();

    // The left edge, dragged to within the gap of the screen's: it lands on
    // 0 and the right edge stays at 500, so the window is 500 wide.
    let _ = state
        .drag_resize_window_pixel(
            WindowId(1),
            &Move {
                x: -95,
                y: 0,
                exact: false,
            },
            Corner {
                left: true,
                ..Corner::NONE
            },
        )
        .unwrap();
    assert_eq!(rects(&state)[0].1, Rect::new(0, 100, 500, 200));
}

/// Two floating windows snap to each other, and a dispatcher never snaps:
/// Hyprland calls `performSnap` from its drag controller alone.
#[test]
fn windows_snap_to_each_other_and_dispatchers_do_not_snap() {
    const SNAP: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
                        general:snap:enabled = true\ngeneral:snap:window_gap = 10\n\
                        general:snap:monitor_gap = 0\n";
    let mut state = setup(SNAP);
    open(&mut state, &[1, 2]);
    for id in [1, 2] {
        focus(&mut state, id);
        let _ = dispatch(&mut state, "setfloating", "");
    }
    let _ = state
        .float_window(WindowId(1), Rect::new(100, 100, 400, 200))
        .unwrap();
    // Window 2 starts eight pixels right of window 1's right edge at 500.
    let _ = state
        .float_window(WindowId(2), Rect::new(508, 100, 300, 200))
        .unwrap();

    let nudge = Move {
        x: 0,
        y: 0,
        exact: false,
    };
    // A drag that does not move it at all still snaps it: the rectangle is
    // judged where it is, which is what makes a drag settle.
    let _ = state.drag_window_pixel(WindowId(2), &nudge).unwrap();
    assert_eq!(
        rects(&state)[1].1,
        Rect::new(500, 100, 300, 200),
        "flush against window 1's right edge"
    );

    // The dispatcher moves by what it was asked for and leaves it there,
    // eight pixels short of the edge it would have snapped to.
    let _ = state
        .float_window(WindowId(2), Rect::new(508, 100, 300, 200))
        .unwrap();
    let _ = state
        .move_window_pixel(
            WindowId(2),
            &Move {
                x: 0,
                y: 0,
                exact: false,
            },
        )
        .unwrap();
    assert_eq!(rects(&state)[1].1, Rect::new(508, 100, 300, 200));
}

/// With `general:snap:enabled` off -- Hyprland's default -- a drag puts a
/// window exactly where it was dragged.
#[test]
fn nothing_snaps_when_the_option_is_off() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "setfloating", "");
    let _ = state
        .float_window(WindowId(1), Rect::new(100, 100, 400, 200))
        .unwrap();
    let _ = state
        .drag_window_pixel(
            WindowId(1),
            &Move {
                x: -95,
                y: 0,
                exact: false,
            },
        )
        .unwrap();
    assert_eq!(rects(&state)[0].1, Rect::new(5, 100, 400, 200));
}

/// `dwindle:force_split = 0`, Hyprland's default: a new window takes the
/// half of the split the pointer is over, rather than always the second.
#[test]
fn a_new_window_takes_the_half_the_pointer_is_over() {
    // The pointer on the left of the screen: the new window goes first.
    let mut state = setup(BARE);
    state.set_pointer((100.0, 500.0));
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(960, 0, 960, 1080)),
            (2, Rect::new(0, 0, 960, 1080))
        ],
        "window 2 opened on the half the pointer was over"
    );

    // And on the right: the new window goes second, which is what this
    // layout used to do whatever the pointer said.
    let mut state = setup(BARE);
    state.set_pointer((1800.0, 500.0));
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 960, 1080)),
            (2, Rect::new(960, 0, 960, 1080))
        ]
    );
}

/// A compositor that has seen no pointer behaves as it did before there was
/// one, which is what every headless test relies on.
#[test]
fn a_layout_with_no_pointer_splits_as_it_always_did() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 960, 1080)),
            (2, Rect::new(960, 0, 960, 1080))
        ]
    );
}

/// `dwindle:force_split` names a side and the pointer is then not consulted.
#[test]
fn force_split_overrules_the_pointer() {
    const FIRST: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
                         dwindle:force_split = 1\n";
    let mut state = setup(FIRST);
    // The pointer on the right, which `force_split = 0` would have put the
    // new window on.
    state.set_pointer((1800.0, 500.0));
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(960, 0, 960, 1080)),
            (2, Rect::new(0, 0, 960, 1080))
        ],
        "first, as the option says, wherever the pointer is"
    );
}

/// `dwindle:use_active_for_splits = 0`: a new window splits the box the
/// pointer is over rather than the focused window's.
#[test]
fn use_active_for_splits_off_opens_beside_the_window_under_the_pointer() {
    const BY_POINTER: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\n\
                              general:border_size = 0\ndwindle:use_active_for_splits = 0\n\
                              dwindle:force_split = 2\n";
    let mut state = setup(BY_POINTER);
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    // Window 1 is 0..960 and window 2 is 960..1920. The focus is on window
    // 2 -- a new window is focused as it opens -- and the pointer is over
    // window 1, so window 1's box is the one that splits.
    state.set_pointer((100.0, 500.0));
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 0, 960, 540)),
            (2, Rect::new(960, 0, 960, 1080)),
            (3, Rect::new(0, 540, 960, 540)),
        ],
        "window 1's box split, stacked because it is taller than it is wide"
    );
}

/// `dwindle:smart_split`: the *quarter* of the box the pointer is in, which
/// picks the split's direction as well as its side.
#[test]
fn smart_split_takes_the_quarter_the_pointer_is_in() {
    const SMART: &str = "general:gaps_in = 0\ngeneral:gaps_out = 0\ngeneral:border_size = 0\n\
                         dwindle:smart_split = true\n";
    // Near the top edge of a 1920x1080 screen: above both diagonals, so the
    // split is stacked and the new window takes the top.
    let mut state = setup(SMART);
    state.set_pointer((960.0, 20.0));
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(0, 540, 1920, 540)),
            (2, Rect::new(0, 0, 1920, 540))
        ]
    );

    // Near the left edge: left of both diagonals, so side by side with the
    // new window on the left -- a direction the half-the-box rule could
    // never have chosen, since a 1920x1080 box splits side by side anyway
    // but the *stacked* case above could not be reached at all.
    let mut state = setup(SMART);
    state.set_pointer((20.0, 540.0));
    open(&mut state, &[1]);
    focus(&mut state, 1);
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [
            (1, Rect::new(960, 0, 960, 1080)),
            (2, Rect::new(0, 0, 960, 1080))
        ]
    );
}

/// `swapwindow` exchanges two tiled windows and leaves the focus on the one
/// that moved, so a run of them walks a window across the screen.
#[test]
fn swapwindow_exchanges_two_tiled_windows() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 1);
    let before = rects(&state);

    let _ = dispatch(&mut state, "swapwindow", "r");
    let after = rects(&state);
    assert_ne!(before, after, "the two changed places");
    assert_eq!(
        state.focused_window(),
        Some(WindowId(1)),
        "the focus follows the window that moved"
    );
    // The rectangles are the same two, with the ids exchanged.
    let places = |rows: &[(u64, Rect)]| {
        let mut out: Vec<Rect> = rows.iter().map(|(_, rect)| *rect).collect();
        out.sort_by_key(|rect| rect.x);
        out
    };
    assert_eq!(places(&before), places(&after));
}

/// `cyclenext` walks the windows on the workspace, wrapping both ways.
#[test]
fn cyclenext_walks_the_workspace() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    focus(&mut state, 1);
    let order = |state: &mut State, arg: &str| {
        let _ = dispatch(state, "cyclenext", arg);
        state.focused_window().map(|window| window.0)
    };
    assert_eq!(order(&mut state, ""), Some(2));
    assert_eq!(order(&mut state, ""), Some(3));
    assert_eq!(order(&mut state, ""), Some(1), "it wraps round");
    assert_eq!(order(&mut state, "prev"), Some(3), "and back the other way");
}

/// `pin` and `pseudo` are recorded on the window and said out loud, and
/// `pin` is refused for a tiled window, which has one slot on one workspace
/// and nowhere else.
#[test]
fn pin_and_pseudo_are_kept_on_the_window() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);

    assert_eq!(
        dispatch(&mut state, "pin", ""),
        [],
        "a tiled window is not pinned"
    );
    assert!(!state.is_pinned(WindowId(1)));

    let _ = dispatch(&mut state, "setfloating", "");
    assert_eq!(
        dispatch(&mut state, "pin", ""),
        [Change::Pinned {
            window: WindowId(1),
            pinned: true
        }]
    );
    assert!(state.is_pinned(WindowId(1)));
    let _ = dispatch(&mut state, "pin", "");
    assert!(!state.is_pinned(WindowId(1)), "it turns over");

    assert_eq!(
        dispatch(&mut state, "pseudo", ""),
        [Change::Pseudo {
            window: WindowId(1),
            pseudo: true
        }]
    );
    assert!(state.is_pseudo(WindowId(1)));
}

/// `tagwindow` adds a tag, `-name` takes it away, and a bare name turns it
/// over, which is how Hyprland's reads its argument.
#[test]
fn tagwindow_adds_takes_away_and_turns_over() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    focus(&mut state, 1);

    let _ = dispatch(&mut state, "tagwindow", "+music");
    assert_eq!(state.tags_of(WindowId(1)), ["music"]);
    // Adding it again changes nothing.
    let _ = dispatch(&mut state, "tagwindow", "+music");
    assert_eq!(state.tags_of(WindowId(1)), ["music"]);

    let _ = dispatch(&mut state, "tagwindow", "-music");
    assert_eq!(state.tags_of(WindowId(1)), [] as [String; 0]);

    // A bare name turns it over.
    let _ = dispatch(&mut state, "tagwindow", "music");
    assert_eq!(state.tags_of(WindowId(1)), ["music"]);
    let _ = dispatch(&mut state, "tagwindow", "music");
    assert_eq!(state.tags_of(WindowId(1)), [] as [String; 0]);
}

/// `renameworkspace` gives a workspace a name, and an empty one puts the
/// number back.
#[test]
fn renameworkspace_names_a_workspace() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    assert_eq!(state.workspace_name(WorkspaceId(1)), "1");

    let changes = dispatch(&mut state, "renameworkspace", "1 mail");
    assert_eq!(
        changes,
        [Change::Renamed {
            workspace: WorkspaceId(1),
            name: "mail".to_owned()
        }]
    );
    assert_eq!(state.workspace_name(WorkspaceId(1)), "mail");

    let _ = dispatch(&mut state, "renameworkspace", "1");
    assert_eq!(state.workspace_name(WorkspaceId(1)), "1");

    // A workspace that does not exist is left alone rather than made.
    assert_eq!(dispatch(&mut state, "renameworkspace", "7 nowhere"), []);
}

/// `workspaceopt allfloat` floats every window on the workspace and leaves
/// the focus where it was.
#[test]
fn workspaceopt_floats_every_window() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    focus(&mut state, 2);

    let _ = dispatch(&mut state, "workspaceopt", "allfloat");
    for id in [1, 2, 3] {
        assert!(state.is_floating(WindowId(id)), "{id} floats");
    }
    assert_eq!(state.focused_window(), Some(WindowId(2)));

    let _ = dispatch(&mut state, "workspaceopt", "allpseudo");
    for id in [1, 2, 3] {
        assert!(state.is_pseudo(WindowId(id)), "{id} is pseudotiled");
    }
}

/// `focuscurrentorlast` swaps between the focused window and the one before
/// it, which is what a person binds to Alt-Tab.
#[test]
fn focuscurrentorlast_swaps_with_the_one_before() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3]);
    focus(&mut state, 1);
    focus(&mut state, 3);

    let _ = dispatch(&mut state, "focuscurrentorlast", "");
    assert_eq!(state.focused_window(), Some(WindowId(1)));
    let _ = dispatch(&mut state, "focuscurrentorlast", "");
    assert_eq!(state.focused_window(), Some(WindowId(3)), "and back");
}

/// `fullscreenstate` sets the state rather than turning it over, and -1
/// leaves it alone.
#[test]
fn fullscreenstate_sets_rather_than_toggles() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 1);

    assert_eq!(dispatch(&mut state, "fullscreenstate", "-1 -1"), []);
    assert!(state.fullscreen(WorkspaceId(1)).is_none());

    let _ = dispatch(&mut state, "fullscreenstate", "2 -1");
    assert_eq!(
        state.fullscreen(WorkspaceId(1)),
        Some((WindowId(1), FullscreenMode::Fullscreen))
    );
    // Again is not a toggle.
    let changes = dispatch(&mut state, "fullscreenstate", "2 -1");
    assert_eq!(changes, []);
    assert!(state.fullscreen(WorkspaceId(1)).is_some());

    let _ = dispatch(&mut state, "fullscreenstate", "0 -1");
    assert!(state.fullscreen(WorkspaceId(1)).is_none());
}

/// `bringactivetotop` and `alterzorder` decide which floating window is
/// drawn over the others.
#[test]
fn bringactivetotop_puts_a_floating_window_over_the_rest() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    for id in [1, 2] {
        focus(&mut state, id);
        let _ = dispatch(&mut state, "setfloating", "");
    }
    // The layout draws floating windows bottom to top, so the last is on
    // top: window 2 floated last.
    // `rects` sorts by id; the drawing order is the one `layout` gives.
    let on_top = |state: &State| rects_on(state, M1).last().map(|(id, _)| *id);
    assert_eq!(on_top(&state), Some(2));

    focus(&mut state, 1);
    let _ = dispatch(&mut state, "bringactivetotop", "");
    assert_eq!(on_top(&state), Some(1));

    let _ = dispatch(&mut state, "alterzorder", "bottom");
    assert_eq!(on_top(&state), Some(2), "and back under");
}

/// Every new dispatcher is refused with Hyprland's own error when its
/// argument is not one, rather than doing something nobody asked for.
#[test]
fn a_bad_argument_to_a_new_dispatcher_is_refused() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    for (name, arg) in [
        ("resizeactive", "sideways"),
        ("moveactive", "10"),
        ("swapwindow", "sideways"),
        ("workspaceopt", "allsomething"),
        ("lockactivegroup", "maybe"),
        ("denywindowfromgroup", "maybe"),
        ("renameworkspace", "notanumber name"),
        ("fullscreenstate", "x y"),
    ] {
        assert!(
            matches!(
                state.dispatch_str(name, arg),
                Err(Error::BadArgument { .. })
            ),
            "{name} {arg} was accepted"
        );
    }
}

// -- The rest of Hyprland's dispatchers ---------------------------------------

/// `layoutmsg togglesplit` turns the split holding the focused window the
/// other way, which lasts as long as `dwindle:preserve_split` is on -- the
/// same condition Hyprland's own has.
#[test]
fn layoutmsg_togglesplit_turns_the_split() {
    let mut state = setup(&format!("{BARE}dwindle:preserve_split = true\n"));
    open(&mut state, &[1, 2]);
    // Side by side on a wide monitor.
    assert_eq!(rects(&state)[0].1, r(0, 0, 960, 1080));

    focus(&mut state, 2);
    assert_eq!(
        dispatch(&mut state, "layoutmsg", "togglesplit"),
        [Change::Layout(M1)]
    );
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1920, 540)), (2, r(0, 540, 1920, 540))],
        "one above the other"
    );

    // And back.
    let _ = dispatch(&mut state, "layoutmsg", "togglesplit");
    assert_eq!(rects(&state)[0].1, r(0, 0, 960, 1080));
}

/// `layoutmsg swapsplit` exchanges the two halves of the split, so the
/// window changes sides without changing size.
#[test]
fn layoutmsg_swapsplit_exchanges_the_halves() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    assert_eq!(rects(&state)[0].1, r(0, 0, 960, 1080));

    focus(&mut state, 1);
    assert_eq!(
        dispatch(&mut state, "layoutmsg", "swapsplit"),
        [Change::Layout(M1)]
    );
    assert_eq!(
        rects(&state),
        [(1, r(960, 0, 960, 1080)), (2, r(0, 0, 960, 1080))]
    );
}

/// `layoutmsg movetoroot` gives a window buried in the tree half the
/// screen, and `unstable` puts it on the other side.
#[test]
fn layoutmsg_movetoroot_lifts_a_window_to_half_the_screen() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2, 3, 4]);
    // The fourth window is two splits deep, so it has an eighth.
    focus(&mut state, 4);
    let buried = rects(&state)[3].1;
    assert!(buried.width * buried.height < 1920 * 1080 / 4, "{buried:?}");

    assert_eq!(
        dispatch(&mut state, "layoutmsg", "movetoroot"),
        [Change::Layout(M1)]
    );
    let lifted = rects(&state)[3].1;
    assert_eq!(lifted.width * lifted.height, 1920 * 1080 / 2);
    // Stable by default: it stays on the side it was on.
    assert_eq!(lifted.x, buried.x.min(960));
}

/// `layoutmsg preselect` says where the next window opened goes, whatever
/// the shape of the box would otherwise say.
#[test]
fn layoutmsg_preselect_places_the_next_window() {
    // As in Hyprland, the side only sticks while `preserve_split` is on:
    // with it off both compositors work every split out from its box again.
    let mut state = setup(&format!("{BARE}dwindle:preserve_split = true\n"));
    open(&mut state, &[1]);
    // A 1920x1080 box splits side by side on its own.
    let _ = dispatch(&mut state, "layoutmsg", "preselect u");
    open(&mut state, &[2]);
    assert_eq!(
        rects(&state),
        [(1, r(0, 540, 1920, 540)), (2, r(0, 0, 1920, 540))],
        "the new window went above"
    );

    // And only for the one window: the next splits the usual way.
    open(&mut state, &[3]);
    assert_eq!(rects(&state)[2].1.height, 540, "not a third of the screen");
}

/// `dwindle:permanent_direction_override`: `preselect` keeps naming the
/// side until it is cleared, rather than for the next window only.
#[test]
fn permanent_direction_override_keeps_a_preselected_side() {
    let mut state = setup(&format!(
        "{BARE}dwindle:preserve_split = true\ndwindle:permanent_direction_override = true\n"
    ));
    open(&mut state, &[1]);
    let _ = dispatch(&mut state, "layoutmsg", "preselect u");
    open(&mut state, &[2]);
    // The second window went above, and so does the third: without the
    // option the third would have split the usual way and been 540 tall.
    open(&mut state, &[3]);
    assert_eq!(
        rects(&state)[2].1.height,
        270,
        "the side stuck, so the third window split the second's half again"
    );

    // `preselect none` clears it, with the option on as without.
    let _ = dispatch(&mut state, "layoutmsg", "preselect none");
    open(&mut state, &[4]);
    assert_ne!(rects(&state)[3].1.height, 135);
}

/// The master layout's own messages: which window is the master, where it
/// is and how much it takes.
#[test]
fn layoutmsg_speaks_to_the_master_layout() {
    let mut state = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut state, &[1, 2, 3]);
    // The first is the master, on the left, taking `master:mfact` of the
    // screen -- Hyprland's default 0.55.
    assert_eq!(rects(&state)[0].1, r(0, 0, 1056, 1080));

    focus(&mut state, 3);
    let _ = dispatch(&mut state, "layoutmsg", "swapwithmaster");
    assert_eq!(rects(&state)[2].1, r(0, 0, 1056, 1080), "3 is the master");

    focus(&mut state, 1);
    let _ = dispatch(&mut state, "layoutmsg", "focusmaster");
    assert_eq!(focused(&state), Some(3));

    let _ = dispatch(&mut state, "layoutmsg", "orientationtop");
    assert_eq!(rects(&state)[2].1, r(0, 0, 1920, 594), "along the top");

    let _ = dispatch(&mut state, "layoutmsg", "mfact exact 0.25");
    assert_eq!(rects(&state)[2].1, r(0, 0, 1920, 270));
    let _ = dispatch(&mut state, "layoutmsg", "mfact 0.25");
    assert_eq!(
        rects(&state)[2].1,
        r(0, 0, 1920, 540),
        "a change, not a size"
    );
}

/// A message meant for the other layout, and one neither knows, are both
/// ignored rather than refused: Hyprland ignores them too.
#[test]
fn a_layoutmsg_the_layout_does_not_know_is_ignored() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let before = rects(&state);
    for message in ["swapwithmaster", "orientationtop", "nonsense", ""] {
        assert_eq!(dispatch(&mut state, "layoutmsg", message), []);
    }
    assert_eq!(rects(&state), before);
}

/// `moveintoorcreategroup` makes the window in that direction into a group
/// and joins it, so one bind is enough to gather windows.
#[test]
fn moveintoorcreategroup_makes_the_group_it_needs() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 2);
    let _ = dispatch(&mut state, "moveintoorcreategroup", "l");
    let group = state.group(WindowId(1)).expect("window 1 is now a group");
    assert_eq!(group.members, [WindowId(1), WindowId(2)]);
    // And one window fills the screen, since the group holds one slot.
    assert_eq!(rects(&state), [(2, r(0, 0, 1920, 1080))]);
}

/// `movewindoworgroup` joins a group when there is one in that direction
/// and moves the window when there is not.
#[test]
fn movewindoworgroup_joins_a_group_or_moves() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    // Nothing is a group yet, so this moves.
    focus(&mut state, 2);
    let _ = dispatch(&mut state, "movewindoworgroup", "l");
    assert_eq!(
        rects(&state),
        [(1, r(960, 0, 960, 1080)), (2, r(0, 0, 960, 1080))]
    );
    assert!(state.group(WindowId(1)).is_none());

    // Make one, and the same dispatcher joins it.
    focus(&mut state, 2);
    let _ = dispatch(&mut state, "togglegroup", "");
    focus(&mut state, 1);
    let _ = dispatch(&mut state, "movewindoworgroup", "l");
    let group = state.group(WindowId(2)).expect("window 2 is a group");
    assert_eq!(group.members, [WindowId(2), WindowId(1)]);
}

/// `focusworkspaceoncurrentmonitor` brings the workspace over rather than
/// following it to the monitor it is on.
#[test]
fn focusworkspaceoncurrentmonitor_brings_the_workspace_here() {
    let mut state = setup(BARE);
    let _changes = state.add_monitor(monitor(M2, 1920, 0, 1920, 1080)).unwrap();
    // Workspace 2 lives on the second monitor.
    let _ = dispatch(&mut state, "focusmonitor", "1");
    let _ = dispatch(&mut state, "workspace", "2");
    open(&mut state, &[1]);
    assert_eq!(state.workspace_monitor(WorkspaceId(2)), Some(M2));

    let _ = dispatch(&mut state, "focusmonitor", "0");
    let _ = dispatch(&mut state, "focusworkspaceoncurrentmonitor", "2");
    assert_eq!(state.workspace_monitor(WorkspaceId(2)), Some(M1));
    assert_eq!(state.active_workspace(M1), Some(WorkspaceId(2)));
    assert_eq!(focused(&state), Some(1), "and the window on it");
}

/// `movewindowpixel` and `resizewindowpixel` act on the window the
/// compositor picked out rather than on the focused one.
#[test]
fn movewindowpixel_and_resizewindowpixel_act_on_one_window() {
    let mut state = setup(BARE);
    let _changes = state
        .open_floating(WindowId(1), r(100, 100, 400, 300))
        .unwrap();
    let _changes = state
        .open_floating(WindowId(2), r(700, 100, 400, 300))
        .unwrap();
    focus(&mut state, 2);

    let by = Move {
        x: 30,
        y: -20,
        exact: false,
    };
    let _changes = state.move_window_pixel(WindowId(1), &by).unwrap();
    let _changes = state.resize_window_pixel(WindowId(1), &by).unwrap();
    assert_eq!(rects(&state)[0].1, r(130, 80, 430, 280));
    assert_eq!(rects(&state)[1].1, r(700, 100, 400, 300), "2 is untouched");
    assert!(state.move_window_pixel(WindowId(9), &by).is_err());
}

/// Both read the window off the end, after a comma, which is the one place
/// Hyprland puts it last.
#[test]
fn movewindowpixel_reads_the_window_after_the_comma() {
    assert_eq!(
        Dispatcher::parse("movewindowpixel", "10 20,class:foot").unwrap(),
        Dispatcher::MoveWindowPixel {
            by: Move {
                x: 10,
                y: 20,
                exact: false
            },
            window: "class:foot".to_owned(),
        }
    );
    assert!(Dispatcher::parse("resizewindowpixel", "10 20").is_err());
    assert!(Dispatcher::parse("movewindowpixel", "sideways,foo").is_err());
}

/// `setignoregrouplock` is deprecated in Hyprland, where it does nothing;
/// it is accepted here and does nothing too.
#[test]
fn setignoregrouplock_is_accepted_and_does_nothing() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    let before = rects(&state);
    assert_eq!(dispatch(&mut state, "setignoregrouplock", "toggle"), []);
    assert_eq!(rects(&state), before);
}

/// A `workspace =` line changes that workspace's gaps and border, and no
/// other workspace's.
///
/// This is the whole point of the keyword: one workspace edge to edge for a
/// browser or a video while everything else keeps its gaps. A compositor
/// that read the rule and then laid every workspace out with the general
/// options would look right in `hyprctl workspacerules` and wrong on the
/// screen.
#[test]
fn a_workspace_rule_changes_that_workspaces_gaps() {
    let mut state = setup("general:gaps_in = 10\ngeneral:gaps_out = 20\ngeneral:border_size = 2\n");
    let rules = ["1, gapsin:0, gapsout:0, bordersize:0"]
        .iter()
        .map(|line| compositor_config::WorkspaceRule::parse(line).expect("a rule"))
        .collect();
    let _changes = state.set_workspace_rules(rules);

    open(&mut state, &[1]);
    assert_eq!(
        rects_on(&state, M1),
        [(1, Rect::new(0, 0, 1920, 1080))],
        "workspace 1 has no gaps and no border"
    );

    // The second workspace has the general options, untouched.
    let _moved = dispatch(&mut state, "movetoworkspace", "2");
    let _shown = dispatch(&mut state, "workspace", "2");
    assert_eq!(
        rects_on(&state, M1),
        [(1, Rect::new(22, 22, 1876, 1036))],
        "workspace 2 keeps gaps_out 20 and border 2"
    );
}

/// `persistent:true` makes the workspace exist with nothing on it, and
/// `defaultName:` names it.
#[test]
fn a_persistent_workspace_exists_with_nothing_on_it() {
    let mut state = setup(BARE);
    assert_eq!(state.workspaces().count(), 1, "the one the monitor shows");

    let rules = ["5, persistent:true, defaultName:media"]
        .iter()
        .map(|line| compositor_config::WorkspaceRule::parse(line).expect("a rule"))
        .collect();
    let changes = state.set_workspace_rules(rules);
    assert_eq!(changes, [Change::Layout(M1)]);
    assert!(
        state.workspaces().any(|id| id == WorkspaceId(5)),
        "workspace 5 was made"
    );
    assert_eq!(state.workspace_name(WorkspaceId(5)), "media");
    assert!(state.windows(WorkspaceId(5)).is_empty());
}

/// `layout:master` on one workspace leaves every other one dwindle, and
/// `layoutopt:` sets that layout's own options there.
///
/// Hyprland's `defaultOrientation` reads the workspace rule's `layoutopt`
/// before it reads `master:orientation`, which is how a person keeps one
/// workspace's master on top and every other one's on the left.
#[test]
fn a_workspace_rule_chooses_that_workspaces_layout() {
    let mut state = setup(BARE);
    let rules = ["2, layout:master, layoutopt:orientation:top, layoutopt:mfact:0.7"]
        .iter()
        .map(|line| compositor_config::WorkspaceRule::parse(line).expect("a rule"))
        .collect();
    let _changes = state.set_workspace_rules(rules);
    assert_eq!(state.settings_at(WorkspaceId(1)).layout, Layout::Dwindle);
    let second = state.settings_at(WorkspaceId(2));
    assert_eq!(second.layout, Layout::Master);
    assert_eq!(second.master.orientation, Orientation::Top);
    assert!((second.master.mfact - 0.7).abs() < f64::EPSILON);
    // And the general options are untouched on every other workspace.
    assert_eq!(
        state.settings_at(WorkspaceId(1)).master.orientation,
        Orientation::Left
    );
}

/// The workspace names a dispatcher can be given beyond a number.
///
/// `previous`, `name:`, `empty` and `next` are all
/// `getWorkspaceIDNameFromString`'s, and each one is a line a person
/// writes: `bind = SUPER, Tab, workspace, previous` is the commonest
/// keybind in any Hyprland configuration after the numbers themselves.
#[test]
fn a_workspace_can_be_named_rather_than_numbered() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);

    // `previous`: where the monitor was before.
    let _second = dispatch(&mut state, "workspace", "2");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(2)));
    let _back = dispatch(&mut state, "workspace", "previous");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(1)));
    let _forth = dispatch(&mut state, "workspace", "prev");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(2)));

    // `next`: one above, whether or not it exists.
    let _next = dispatch(&mut state, "workspace", "next");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(3)));

    // `empty`: the lowest-numbered workspace with nothing on it, which is
    // 2 here because 1 has the window.
    let _empty = dispatch(&mut state, "workspace", "empty");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(2)));
    // `emptyn` counts up from the one shown now instead.
    let _after = dispatch(&mut state, "workspace", "emptyn");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(3)));

    // `name:`: a workspace by the name it was given, and the first free
    // number for a name nothing carries yet.
    let _renamed = dispatch(&mut state, "renameworkspace", "3 web");
    let _elsewhere = dispatch(&mut state, "workspace", "1");
    let _named = dispatch(&mut state, "workspace", "name:web");
    assert_eq!(state.current_workspace(), Some(WorkspaceId(3)));
}

/// `binds:workspace_back_and_forth`: asking for the workspace that is
/// already shown goes to the one before it.
///
/// One key both there and back, which is what the option is for. Without
/// it the same key twice is the same workspace twice.
#[test]
fn workspace_back_and_forth_returns_to_the_one_before() {
    let mut plain = setup(BARE);
    let _second = dispatch(&mut plain, "workspace", "2");
    let _again = dispatch(&mut plain, "workspace", "2");
    assert_eq!(plain.current_workspace(), Some(WorkspaceId(2)));

    let mut both = setup(&format!("{BARE}binds:workspace_back_and_forth = true\n"));
    let _second = dispatch(&mut both, "workspace", "2");
    let _again = dispatch(&mut both, "workspace", "2");
    assert_eq!(both.current_workspace(), Some(WorkspaceId(1)));
}

/// `binds:hide_special_on_workspace_change`: the scratchpad goes away when
/// the workspace under it changes.
#[test]
fn the_scratchpad_can_be_hidden_when_the_workspace_changes() {
    let mut kept = setup(BARE);
    let _over = dispatch(&mut kept, "togglespecialworkspace", "");
    assert!(kept.special_on(M1).is_some());
    let _second = dispatch(&mut kept, "workspace", "2");
    assert!(kept.special_on(M1).is_some(), "it stays by default");

    let mut hidden = setup(&format!(
        "{BARE}binds:hide_special_on_workspace_change = true\n"
    ));
    let _over = dispatch(&mut hidden, "togglespecialworkspace", "");
    assert!(hidden.special_on(M1).is_some());
    let _second = dispatch(&mut hidden, "workspace", "2");
    assert!(hidden.special_on(M1).is_none());
}

/// `special_scale_factor` draws a scratchpad's windows smaller than the
/// slot the layout gave them, centred in it.
///
/// What makes the scratchpad look like something *over* the screen rather
/// than another workspace: `CWindowTarget::applyToWindow` shrinks a
/// special workspace's window about its own middle.
#[test]
fn the_scratchpads_windows_are_drawn_smaller_than_their_slot() {
    let mut state = setup(&format!("{BARE}dwindle:special_scale_factor = 0.8\n"));
    open(&mut state, &[1, 2]);
    let _over = dispatch(&mut state, "togglespecialworkspace", "");
    focus(&mut state, 2);
    let _moved = dispatch(&mut state, "movetoworkspace", "special");
    assert_eq!(
        rects_on(&state, M1),
        [
            (1, r(0, 0, 1920, 1080)),
            // 80% of the monitor, centred: 1536x864 at 192,108.
            (2, r(192, 108, 1536, 864))
        ]
    );
}

/// `misc:close_special_on_empty`: a scratchpad whose last window has gone
/// stops being shown.
///
/// Otherwise an empty overlay is left over the screen for a person to
/// dismiss by hand, which is not what they asked for by closing the window.
#[test]
fn the_scratchpad_closes_when_its_last_window_goes() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let _over = dispatch(&mut state, "togglespecialworkspace", "");
    let _moved = dispatch(&mut state, "movetoworkspace", "special");
    assert!(state.special_on(M1).is_some());
    let _gone = state.window_gone(WindowId(1)).expect("the window");
    assert!(state.special_on(M1).is_none());

    // And with the option off it stays, which is a person asking for it to.
    let mut kept = setup(&format!("{BARE}misc:close_special_on_empty = false\n"));
    open(&mut kept, &[1]);
    let _over = dispatch(&mut kept, "togglespecialworkspace", "");
    let _moved = dispatch(&mut kept, "movetoworkspace", "special");
    assert!(kept.special_on(M1).is_some());
    let _gone = kept.window_gone(WindowId(1)).expect("the window");
    assert!(kept.special_on(M1).is_some());
}

/// `binds:movefocus_cycles_fullscreen`: a direction key on a fullscreen
/// window walks to the next window instead of looking for one beside it.
///
/// There is nothing beside a window that covers the screen, so without the
/// option the key does nothing at all -- which is what a person notices and
/// why the option exists.
#[test]
fn movefocus_can_cycle_when_a_window_is_fullscreen() {
    let mut plain = setup(BARE);
    open(&mut plain, &[1, 2]);
    focus(&mut plain, 1);
    let _full = dispatch(&mut plain, "fullscreen", "0");
    let _moved = dispatch(&mut plain, "movefocus", "r");
    assert_eq!(plain.focused_window(), Some(WindowId(1)), "nowhere to go");

    let mut cycling = setup(&format!("{BARE}binds:movefocus_cycles_fullscreen = true\n"));
    open(&mut cycling, &[1, 2]);
    focus(&mut cycling, 1);
    let _full = dispatch(&mut cycling, "fullscreen", "0");
    let _moved = dispatch(&mut cycling, "movefocus", "r");
    assert_eq!(cycling.focused_window(), Some(WindowId(2)));
}

/// `binds:allow_pin_fullscreen`: a fullscreen window can be pinned.
///
/// `pin` takes only on a floating window otherwise, which is Hyprland's own
/// rule and is what makes `pin` on a fullscreen window do nothing.
#[test]
fn a_fullscreen_window_can_be_pinned_when_the_option_says_so() {
    let mut plain = setup(BARE);
    open(&mut plain, &[1]);
    let _full = dispatch(&mut plain, "fullscreen", "0");
    let _pinned = dispatch(&mut plain, "pin", "");
    assert!(!plain.is_pinned(WindowId(1)));

    let mut allowed = setup(&format!("{BARE}binds:allow_pin_fullscreen = true\n"));
    open(&mut allowed, &[1]);
    let _full = dispatch(&mut allowed, "fullscreen", "0");
    let _pinned = dispatch(&mut allowed, "pin", "");
    assert!(allowed.is_pinned(WindowId(1)));
}

/// `binds:window_direction_monitor_fallback = false`: `movefocus` with no
/// window in that direction stays where it is rather than moving to the
/// monitor there.
#[test]
fn movefocus_can_be_kept_off_the_next_monitor() {
    let mut state = state_on(BARE, monitor(M1, 0, 0, 1920, 1080));
    let _second = state
        .add_monitor(monitor(M2, 1920, 0, 1280, 1024))
        .expect("a second monitor");
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _moved = dispatch(&mut state, "movefocus", "r");
    assert_eq!(state.focused_monitor(), Some(M2), "the default follows");

    let mut kept = state_on(
        &format!("{BARE}binds:window_direction_monitor_fallback = false\n"),
        monitor(M1, 0, 0, 1920, 1080),
    );
    let _second = kept
        .add_monitor(monitor(M2, 1920, 0, 1280, 1024))
        .expect("a second monitor");
    open(&mut kept, &[1]);
    focus(&mut kept, 1);
    let _moved = dispatch(&mut kept, "movefocus", "r");
    assert_eq!(kept.focused_monitor(), Some(M1));
}

/// `general:float_gaps`: a floating window is centred inside its own work
/// area, which Hyprland keeps apart from the tiled one.
///
/// A floating window is put where a person wants it and often wants no
/// margin at all, which is why the default is zero rather than `gaps_out`;
/// a side written negative means "use `gaps_out`".
#[test]
fn a_floating_window_is_centred_in_its_own_work_area() {
    let settings = "general:gaps_out = 20\ngeneral:border_size = 0\ngeneral:gaps_in = 0\n";
    // No float gaps by default, so the window is centred on the whole
    // monitor: 960x540 at 480,270.
    let mut plain = setup(settings);
    open(&mut plain, &[1]);
    let _floated = dispatch(&mut plain, "togglefloating", "");
    let _centred = dispatch(&mut plain, "centerwindow", "");
    assert_eq!(rects(&plain), [(1, r(480, 270, 960, 540))]);

    // With float gaps of 100 the area is 1720x880 at 100,100, so the same
    // window is centred at 480,270 again -- the middle does not move -- but
    // a side gap that is not even does.
    let mut uneven = setup(&format!("{settings}general:float_gaps = 0 200 0 0\n"));
    open(&mut uneven, &[1]);
    let _floated = dispatch(&mut uneven, "togglefloating", "");
    let _centred = dispatch(&mut uneven, "centerwindow", "");
    assert_eq!(rects(&uneven), [(1, r(380, 270, 960, 540))]);

    // A negative side means `gaps_out`, which is 20 here.
    let mut fallback = setup(&format!("{settings}general:float_gaps = -1\n"));
    open(&mut fallback, &[1]);
    let _floated = dispatch(&mut fallback, "togglefloating", "");
    let _centred = dispatch(&mut fallback, "centerwindow", "");
    assert_eq!(rects(&fallback), [(1, r(480, 270, 960, 540))]);
}

/// `master:new_on_active`: a new window goes beside the focused one rather
/// than at one end of the stack.
#[test]
fn master_new_on_active_puts_the_window_beside_the_focused_one() {
    let stack =
        |state: &State| -> Vec<u64> { rects_on(state, M1).into_iter().map(|(id, _)| id).collect() };

    let mut after = setup(&format!(
        "{BARE}general:layout = master\nmaster:new_on_active = after\n"
    ));
    open(&mut after, &[1, 2, 3]);
    focus(&mut after, 2);
    open(&mut after, &[4]);
    assert_eq!(stack(&after), [1, 2, 4, 3]);

    // `before` applies to every window as it opens, so the stack is
    // already reversed by the time the fourth arrives: 2 pushed behind the
    // master, then 3 went in front of 2, and 4 goes in front of 3.
    let mut before = setup(&format!(
        "{BARE}general:layout = master\nmaster:new_on_active = before\n"
    ));
    open(&mut before, &[1, 2, 3]);
    assert_eq!(stack(&before), [1, 3, 2]);
    focus(&mut before, 3);
    open(&mut before, &[4]);
    assert_eq!(stack(&before), [1, 4, 3, 2]);

    // `none`, the default: at the end of the stack whatever is focused.
    let mut plain = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut plain, &[1, 2, 3]);
    focus(&mut plain, 2);
    open(&mut plain, &[4]);
    assert_eq!(stack(&plain), [1, 2, 3, 4]);
}

/// `master:focus_master_on_close`: closing a window focuses the master
/// rather than whatever the history has next.
#[test]
fn master_can_take_the_focus_when_a_window_closes() {
    let mut plain = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut plain, &[1, 2, 3]);
    focus(&mut plain, 2);
    focus(&mut plain, 3);
    let _gone = plain.window_gone(WindowId(3)).expect("the window");
    assert_eq!(plain.focused_window(), Some(WindowId(2)), "the history's");

    let mut mastered = setup(&format!(
        "{BARE}general:layout = master\nmaster:focus_master_on_close = true\n"
    ));
    open(&mut mastered, &[1, 2, 3]);
    focus(&mut mastered, 2);
    focus(&mut mastered, 3);
    let _gone = mastered.window_gone(WindowId(3)).expect("the window");
    assert_eq!(mastered.focused_window(), Some(WindowId(1)));
}

/// `layoutmsg addmaster` and `removemaster` move a window between the
/// master column and the stack.
///
/// Hyprland's master layout has as many masters as a person asks for, and
/// they share the master column. This compositor had one and both messages
/// did nothing, so two masters side by side -- which is the whole reason
/// the messages exist -- could not be had at all.
#[test]
fn addmaster_and_removemaster_move_a_window_between_the_columns() {
    let mut state = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut state, &[1, 2, 3]);
    assert_eq!(state.masters_on(WorkspaceId(1)), [WindowId(1)]);
    assert_eq!(
        rects_on(&state, M1),
        [
            (1, r(0, 0, 1056, 1080)),
            (2, r(1056, 0, 864, 540)),
            (3, r(1056, 540, 864, 540))
        ]
    );

    // The focused window joins the masters and the column is shared.
    focus(&mut state, 2);
    let _added = dispatch(&mut state, "layoutmsg", "addmaster");
    assert_eq!(state.masters_on(WorkspaceId(1)), [WindowId(1), WindowId(2)]);
    assert_eq!(
        rects_on(&state, M1),
        [
            (1, r(0, 0, 1056, 540)),
            (2, r(0, 540, 1056, 540)),
            (3, r(1056, 0, 864, 1080))
        ]
    );

    // And back out again.
    let _removed = dispatch(&mut state, "layoutmsg", "removemaster");
    assert_eq!(state.masters_on(WorkspaceId(1)), [WindowId(1)]);
    assert_eq!(
        rects_on(&state, M1),
        [
            (1, r(0, 0, 1056, 1080)),
            (2, r(1056, 0, 864, 540)),
            (3, r(1056, 540, 864, 540))
        ]
    );
}

/// `addmaster` is refused when it would leave fewer than two windows in the
/// stack, unless `master:allow_small_split` says otherwise.
///
/// A stack of one beside two masters is not what the message is for, which
/// is why Hyprland answers `nothing to do`.
#[test]
fn addmaster_needs_a_stack_to_take_from() {
    let mut plain = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut plain, &[1, 2]);
    focus(&mut plain, 2);
    let _added = dispatch(&mut plain, "layoutmsg", "addmaster");
    assert_eq!(plain.masters_on(WorkspaceId(1)), [WindowId(1)]);

    let mut small = setup(&format!(
        "{BARE}general:layout = master\nmaster:allow_small_split = true\n"
    ));
    open(&mut small, &[1, 2]);
    focus(&mut small, 2);
    let _added = dispatch(&mut small, "layoutmsg", "addmaster");
    assert_eq!(
        small.masters_on(WorkspaceId(1)),
        [WindowId(1), WindowId(2)],
        "both are masters, so they share the whole workspace"
    );
    assert_eq!(
        rects_on(&small, M1),
        [(1, r(0, 0, 1920, 540)), (2, r(0, 540, 1920, 540))]
    );

    // `removemaster` is refused with fewer than two masters: a workspace
    // with none is not a state the layout has.
    let mut state = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut state, &[1, 2]);
    let _removed = dispatch(&mut state, "layoutmsg", "removemaster");
    assert_eq!(state.masters_on(WorkspaceId(1)), [WindowId(1)]);
}

/// The last master closing hands the master's slot to the first window
/// left, and a master closing with another master leaves that one.
#[test]
fn a_master_closing_leaves_the_masters_that_are_left() {
    let mut state = setup(&format!("{BARE}general:layout = master\n"));
    open(&mut state, &[1, 2, 3]);
    focus(&mut state, 2);
    let _added = dispatch(&mut state, "layoutmsg", "addmaster");
    assert_eq!(state.masters_on(WorkspaceId(1)), [WindowId(1), WindowId(2)]);

    let _gone = state.window_gone(WindowId(1)).expect("the window");
    assert_eq!(state.masters_on(WorkspaceId(1)), [WindowId(2)]);
    let _gone = state.window_gone(WindowId(2)).expect("the window");
    assert_eq!(
        state.masters_on(WorkspaceId(1)),
        [WindowId(3)],
        "the first window left takes the slot"
    );
}

// -- Monocle ------------------------------------------------------------------

/// `general:layout = monocle`: every window fills the workspace and the
/// focused one is shown.
///
/// Hyprland's third tiling layout, and one this compositor did not have at
/// all: a person who wrote it got dwindle. What makes it a layout rather
/// than a fullscreen window is that the windows are still tiled --
/// `cyclenext` walks them, closing one shows the next, and the gaps and
/// the border are the workspace's.
#[test]
fn monocle_shows_one_window_at_a_time() {
    let mut state = setup("general:layout = monocle\n");
    open(&mut state, &[1]);
    // The gaps and the border are the workspace's, which is what tells a
    // monocle window from a fullscreen one: `gaps_out` 20 and a border of
    // 1 leave 1878x1038 at 21,21.
    assert_eq!(rects(&state), [(1, r(21, 21, 1878, 1038))]);

    // A second window is focused as it opens, so it is the one shown.
    open(&mut state, &[2]);
    assert_eq!(rects(&state), [(2, r(21, 21, 1878, 1038))]);

    // Focusing the first shows it instead.
    focus(&mut state, 1);
    assert_eq!(rects(&state), [(1, r(21, 21, 1878, 1038))]);

    // And both are still on the workspace, which is what makes the layout
    // a layout.
    assert_eq!(state.windows(WorkspaceId(1)).len(), 2);
}

/// `cyclenext` walks the windows, and each one it lands on is the one
/// drawn.
#[test]
fn monocle_cyclenext_walks_the_windows() {
    let mut state = setup(&format!("{BARE}general:layout = monocle\n"));
    open(&mut state, &[1, 2, 3]);
    assert_eq!(rects(&state), [(3, r(0, 0, 1920, 1080))]);

    let _next = dispatch(&mut state, "cyclenext", "");
    assert_eq!(state.focused_window(), Some(WindowId(1)));
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);

    let _next = dispatch(&mut state, "cyclenext", "");
    assert_eq!(rects(&state), [(2, r(0, 0, 1920, 1080))]);

    // `layoutmsg cyclenext` is the monocle layout's own message and does
    // the same thing, which is what `CMonocleAlgorithm::layoutMsg` does
    // with it.
    let _next = dispatch(&mut state, "layoutmsg", "cyclenext");
    assert_eq!(rects(&state), [(3, r(0, 0, 1920, 1080))]);
}

/// Closing the shown window shows another, and closing the last empties
/// the workspace.
#[test]
fn monocle_closing_the_shown_window_shows_another() {
    let mut state = setup(&format!("{BARE}general:layout = monocle\n"));
    open(&mut state, &[1, 2]);
    assert_eq!(rects(&state), [(2, r(0, 0, 1920, 1080))]);
    let _gone = state.window_gone(WindowId(2)).expect("the window");
    assert_eq!(rects(&state), [(1, r(0, 0, 1920, 1080))]);
    let _gone = state.window_gone(WindowId(1)).expect("the window");
    assert_eq!(rects(&state), [] as [(u64, Rect); 0]);
}

/// A workspace whose focus is elsewhere still shows one of its windows.
///
/// A monitor showing a workspace nothing on it is focused on -- the other
/// monitor has the focus -- must still draw something, or the screen goes
/// blank when the focus moves away.
#[test]
fn monocle_shows_a_window_on_an_unfocused_workspace() {
    let mut state = state_on(
        &format!("{BARE}general:layout = monocle\n"),
        monitor(M1, 0, 0, 1920, 1080),
    );
    let _second = state
        .add_monitor(monitor(M2, 1920, 0, 1280, 1024))
        .expect("a second monitor");
    open(&mut state, &[1]);
    focus(&mut state, 1);
    let _moved = dispatch(&mut state, "focusmonitor", "1");
    open(&mut state, &[2]);
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(0, 0, 1920, 1080))],
        "the first monitor still draws its window"
    );
    assert_eq!(rects_on(&state, M2), [(2, r(1920, 0, 1280, 1024))]);
}

// -- Scrolling ----------------------------------------------------------------

/// A tape of columns, each a share of the screen's width.
fn tape(extra: &str) -> State {
    setup(&format!("{BARE}general:layout = scrolling\n{extra}"))
}

/// `general:layout = scrolling`: a tape of columns, each its own width, and
/// the screen is a window onto it.
///
/// Hyprland's fourth tiling layout and the one no other tiling compositor
/// has: a column's width is its own, so a wide editor and a narrow terminal
/// sit side by side and a third column scrolls in beside them without
/// either of the first two changing shape.
#[test]
fn scrolling_lays_a_tape_of_columns() {
    let mut state = tape("");
    // One column alone spans the screen, because
    // `fullscreen_on_one_column` is on by default.
    open(&mut state, &[1]);
    assert_eq!(rects_on(&state, M1), [(1, r(0, 0, 1920, 1080))]);

    // Two columns at the default `column_width` of 0.5 fill the screen.
    open(&mut state, &[2]);
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );

    // Three at 0.5 are a tape half again as wide as the screen, and the
    // third is brought into view -- `fit` moves the tape the least it can,
    // so the tape's right edge is the screen's and the first column is off
    // the left.
    open(&mut state, &[3]);
    assert_eq!(
        rects_on(&state, M1),
        [(2, r(0, 0, 960, 1080)), (3, r(960, 0, 960, 1080))],
        "the first column scrolled off the left"
    );
}

/// A tape narrower than the screen is centred, which is
/// `calculateCameraOffset`'s first rule.
#[test]
fn scrolling_centres_a_tape_narrower_than_the_screen() {
    let mut state =
        tape("scrolling:column_width = 0.25\nscrolling:fullscreen_on_one_column = false\n");
    open(&mut state, &[1]);
    // One column of a quarter of the screen sits in the middle of it.
    assert_eq!(rects_on(&state, M1), [(1, r(720, 0, 480, 1080))]);
    open(&mut state, &[2]);
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(480, 0, 480, 1080)), (2, r(960, 0, 480, 1080))]
    );
}

/// `movefocus l` and `r` step between columns and scroll the tape;
/// `movefocus u` and `d` walk the windows of the column.
#[test]
fn scrolling_movefocus_walks_the_tape_and_the_column() {
    let mut state = tape("");
    open(&mut state, &[1, 2, 3]);
    assert_eq!(state.focused_window(), Some(WindowId(3)));

    let _left = dispatch(&mut state, "movefocus", "l");
    assert_eq!(state.focused_window(), Some(WindowId(2)));
    let _left = dispatch(&mut state, "movefocus", "l");
    assert_eq!(state.focused_window(), Some(WindowId(1)));
    // And the tape followed, so the first column is on the screen again.
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );

    // `wrap_focus` is on by default, so going further wraps to the end.
    let _left = dispatch(&mut state, "movefocus", "l");
    assert_eq!(state.focused_window(), Some(WindowId(3)));
}

/// `layoutmsg expel` takes the last window of a column out into one of its
/// own, and `consume` takes the next column's first window in.
#[test]
fn scrolling_expel_and_consume_move_windows_between_columns() {
    let mut state = tape("");
    open(&mut state, &[1, 2]);
    // `consume` pulls the second column's window into the first.
    focus(&mut state, 1);
    let _consumed = dispatch(&mut state, "layoutmsg", "consume");
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(0, 0, 1920, 540)), (2, r(0, 540, 1920, 540))],
        "one column of two windows, spanning the screen"
    );

    // `expel` sends the last of them back out.
    let _expelled = dispatch(&mut state, "layoutmsg", "expel");
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );
}

/// `layoutmsg colresize` changes the focused column's width, and
/// `colresize +conf` steps through `scrolling:explicit_column_widths`.
#[test]
fn scrolling_colresize_changes_one_columns_width() {
    let mut state = tape("scrolling:explicit_column_widths = 0.25, 0.5, 1.0\n");
    open(&mut state, &[1, 2]);
    focus(&mut state, 1);

    let _resized = dispatch(&mut state, "layoutmsg", "colresize 0.25");
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(240, 0, 480, 1080)), (2, r(720, 0, 960, 1080))],
        "a tape of 0.75 of the screen, centred"
    );

    // `+conf` steps to the next width in the list above the one it has.
    let _stepped = dispatch(&mut state, "layoutmsg", "colresize +conf");
    assert_eq!(
        rects_on(&state, M1),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );
}

/// `layoutmsg swapcol` exchanges the focused column with its neighbour, and
/// `inhibit_scroll` stops the tape moving.
#[test]
fn scrolling_swapcol_and_inhibit_scroll() {
    let mut state = tape("");
    open(&mut state, &[1, 2]);
    focus(&mut state, 1);
    let _swapped = dispatch(&mut state, "layoutmsg", "swapcol r");
    assert_eq!(
        rects_on(&state, M1),
        [(2, r(0, 0, 960, 1080)), (1, r(960, 0, 960, 1080))]
    );

    // With the tape inhibited, `move +col` does not move it.
    let mut held = tape("");
    open(&mut held, &[1, 2, 3]);
    focus(&mut held, 1);
    let before = rects_on(&held, M1);
    let _inhibited = dispatch(&mut held, "layoutmsg", "inhibit_scroll");
    let _moved = dispatch(&mut held, "layoutmsg", "move +col");
    assert_eq!(rects_on(&held, M1), before);
}

/// `min_size`, `max_size` and `keep_aspect_ratio` hold a floating window's
/// size wherever it is set.
///
/// Hyprland clamps at every point a size could change -- `setSizeLimits` --
/// so the limits belong with the window and not with the rule that set
/// them: the rule fires once and the window is resized many times.
#[test]
fn a_windows_size_limits_hold_wherever_it_is_set() {
    let mut state = setup(BARE);
    open(&mut state, &[1]);
    let _floated = dispatch(&mut state, "togglefloating", "");

    let _limited = state.set_limits(
        WindowId(1),
        Limits {
            smallest: Some((600, 400)),
            largest: Some((1200, 800)),
            keep_aspect: false,
        },
    );
    // The window was floated at half the monitor, 960x540, which is inside
    // both limits and so did not move.
    assert_eq!(rects(&state), [(1, r(480, 270, 960, 540))]);

    // Too small, and it is held up to the minimum.
    let _resized = state
        .float_window(WindowId(1), r(0, 0, 100, 100))
        .expect("the window");
    assert_eq!(rects(&state), [(1, r(0, 0, 600, 400))]);

    // Too large, and it is held down to the maximum.
    let _resized = state
        .float_window(WindowId(1), r(0, 0, 1900, 1000))
        .expect("the window");
    assert_eq!(rects(&state), [(1, r(0, 0, 1200, 800))]);

    // `no_max_size` is the absence of a maximum, and then the same
    // rectangle is allowed.
    let _cleared = state.set_limits(
        WindowId(1),
        Limits {
            smallest: Some((600, 400)),
            largest: None,
            keep_aspect: false,
        },
    );
    let _resized = state
        .float_window(WindowId(1), r(0, 0, 1900, 1000))
        .expect("the window");
    assert_eq!(rects(&state), [(1, r(0, 0, 1900, 1000))]);
}

/// `keep_aspect_ratio`: the shape asked for is the shape kept, held inside
/// what the limits allow.
#[test]
fn keep_aspect_ratio_holds_the_shape_inside_the_limits() {
    // A 2:1 rectangle held to at most 1000 wide comes out 1000x500 rather
    // than 1000x800: the shape wins over the taller maximum.
    let held = Limits {
        smallest: None,
        largest: Some((1000, 800)),
        keep_aspect: true,
    }
    .hold(r(0, 0, 1600, 800));
    assert_eq!(held, r(0, 0, 1000, 500));

    // And a 1:2 rectangle held to at most 800 tall comes out 400x800.
    let tall = Limits {
        smallest: None,
        largest: Some((1000, 800)),
        keep_aspect: true,
    }
    .hold(r(0, 0, 800, 1600));
    assert_eq!(tall, r(0, 0, 400, 800));

    // With no limits at all the rectangle is its own shape already.
    assert_eq!(
        Limits {
            keep_aspect: true,
            ..Limits::default()
        }
        .hold(r(0, 0, 1600, 800)),
        r(0, 0, 1600, 800)
    );
}

/// Two windows opened one after the other each take the focus, and the
/// trail says so in order, once: a compositor that looks at its state once
/// a pass would otherwise see only the second.
#[test]
fn the_focus_trail_is_every_window_focused_in_order() {
    let mut state = state_on(BARE, monitor(M1, 0, 0, 1920, 1080));
    open(&mut state, &[1, 2]);
    assert_eq!(
        state.take_focus_trail(),
        [Some(WindowId(1)), Some(WindowId(2))]
    );
    assert!(state.take_focus_trail().is_empty(), "taken, and forgotten");
    focus(&mut state, 1);
    assert_eq!(state.take_focus_trail(), [Some(WindowId(1))]);
}

/// A tiled window dragged with the mouse is lifted out of the tiling at its
/// own size, centred on the pointer, and dropped back in beside the window
/// under the pointer, on the half of its box the pointer is in.
///
/// Hyprland's drag controller: `updateDragWindow` floats a tiled target at
/// the mouse, and `dragEnd` sets it tiled again, which re-adds it with the
/// mouse where it was let go. Until this, a drag floated a tiled window and
/// left it floating.
#[test]
fn a_dragged_tiled_window_is_dropped_back_into_the_tiling() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 2);
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))]
    );

    let _ = state.lift_window(WindowId(2), (1440.0, 540.0)).unwrap();
    assert!(state.is_floating(WindowId(2)), "lifted, it floats");
    assert_eq!(
        rects(&state),
        [(1, r(0, 0, 1920, 1080)), (2, r(960, 0, 960, 1080))],
        "at its own size round the pointer, and the other window takes the room"
    );

    // Carried to the left half of the window that is left, and let go.
    let _ = state
        .drag_window_pixel(
            WindowId(2),
            &Move {
                x: -1200,
                y: 0,
                exact: false,
            },
        )
        .unwrap();
    let _ = state.drop_window(WindowId(2), (240.0, 540.0)).unwrap();
    assert!(
        !state.is_floating(WindowId(2)),
        "dropped, it is tiled again"
    );
    assert_eq!(
        rects(&state),
        [(1, r(960, 0, 960, 1080)), (2, r(0, 0, 960, 1080))],
        "on the half of the box it was dropped on"
    );

    // Dropping again does nothing: the drag that lifted it is over.
    assert!(
        state
            .drop_window(WindowId(2), (1800.0, 540.0))
            .unwrap()
            .is_empty()
    );
}

/// `dwindle:precise_mouse_move`: a dropped window goes on the *quarter* of
/// the box under the pointer, so a drop near the top of a wide window
/// stacks the two. Without it the same drop is the half rule's, side by
/// side -- which is the control.
#[test]
fn precise_mouse_move_drops_a_window_on_the_quarter_the_pointer_is_in() {
    let dropped = |text: &str| {
        let mut state = setup(text);
        open(&mut state, &[1, 2]);
        focus(&mut state, 2);
        let _ = state.lift_window(WindowId(2), (1440.0, 540.0)).unwrap();
        let _ = state.drop_window(WindowId(2), (1200.0, 30.0)).unwrap();
        rects(&state)
    };
    assert_eq!(
        dropped(&format!("{BARE}dwindle:precise_mouse_move = true\n")),
        [(1, r(0, 540, 1920, 540)), (2, r(0, 0, 1920, 540))],
        "near the top edge: stacked, and on top"
    );
    assert_eq!(
        dropped(BARE),
        [(1, r(0, 0, 960, 1080)), (2, r(960, 0, 960, 1080))],
        "without the option, the right half"
    );
}

/// Lifting a window is not floating it: one that was floating before keeps
/// the rectangle `togglefloating` puts it back at, and a window that is not
/// focused, or is floating already, is not lifted at all.
#[test]
fn lifting_a_window_leaves_its_floating_rectangle_alone() {
    let mut state = setup(BARE);
    open(&mut state, &[1, 2]);
    focus(&mut state, 2);
    let _ = dispatch(&mut state, "togglefloating", "");
    let _ = state
        .float_window(WindowId(2), r(100, 100, 300, 200))
        .unwrap();
    let _ = dispatch(&mut state, "togglefloating", "");
    assert!(!state.is_floating(WindowId(2)));

    assert!(
        state
            .lift_window(WindowId(1), (100.0, 100.0))
            .unwrap()
            .is_empty(),
        "not focused"
    );
    let _ = state.lift_window(WindowId(2), (1440.0, 540.0)).unwrap();
    let _ = state.drop_window(WindowId(2), (1800.0, 540.0)).unwrap();
    let _ = dispatch(&mut state, "togglefloating", "");
    assert_eq!(rects(&state)[1], (2, r(100, 100, 300, 200)));
}
