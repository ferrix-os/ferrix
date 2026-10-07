//! The options the layouts read, taken out of the configuration once so the
//! arithmetic does not look them up by name for every window.
//!
//! Each field falls back to Hyprland's default when the configuration does
//! not hold the option with the expected type, and each is clamped the way
//! Hyprland clamps it where it reads it.

use compositor_config::{Config, Gaps};

/// Which tiling layout workspaces use: `general:layout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// `dwindle`, the default, and what an unknown name falls back to.
    #[default]
    Dwindle,
    /// `master`.
    Master,
    /// `monocle`: every window fills the workspace and one is shown.
    Monocle,
    /// `scrolling`: a tape of columns wider than the screen.
    Scrolling,
}

/// How a column is brought into view: `scrolling:focus_fit_method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FitMethod {
    /// `0`, `center`: the column's middle goes to the screen's middle.
    Center,
    /// `1`, `fit`, the default: the tape moves the least it can to get the
    /// whole column on the screen, and does not move at all when it is
    /// already there.
    #[default]
    Fit,
}

/// The widths `colresize +conf` steps through, in a form [`Settings`] can
/// stay `Copy` with.
///
/// `Settings` is copied for every workspace whose `workspace =` line
/// changes an option, on every frame; a `Vec` here would be an allocation
/// each time for a list that is four numbers long in every configuration
/// anybody writes. Past [`Widths::MOST`] the rest are dropped, and the
/// parser says so.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Widths {
    held: [f64; Self::MOST],
    count: usize,
}

impl Widths {
    /// How many widths are kept.
    pub const MOST: usize = 16;

    /// The widths, in the order they were written.
    #[must_use]
    pub fn as_slice(&self) -> &[f64] {
        self.held.get(..self.count).unwrap_or(&[])
    }

    /// Read a comma-separated list, each width clamped the way Hyprland's
    /// own parser clamps it. Never empty: `+conf` on an empty list has
    /// nothing to step to, so it falls back to one whole screen.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut held = [0.0; Self::MOST];
        let mut count = 0;
        for width in text
            .split(',')
            .filter_map(|part| part.trim().parse::<f64>().ok())
            .map(|width| width.clamp(0.1, 1.0))
        {
            if let Some(slot) = held.get_mut(count) {
                *slot = width;
                count += 1;
            }
        }
        if count == 0 {
            if let Some(slot) = held.first_mut() {
                *slot = 1.0;
            }
            count = 1;
        }
        Self { held, count }
    }
}

/// The scrolling layout's options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollingSettings {
    /// `scrolling:column_width`: how much of the screen a new column takes.
    pub column_width: f64,
    /// `scrolling:focus_fit_method`.
    pub focus_fit_method: FitMethod,
    /// `scrolling:follow_focus`: the tape moves to bring the focused
    /// window's column into view.
    pub follow_focus: bool,
    /// `scrolling:fullscreen_on_one_column`: one column alone spans the
    /// whole screen, whatever its width says.
    pub fullscreen_on_one_column: bool,
    /// `scrolling:wrap_focus`: moving the focus past either end of the tape
    /// goes to the other end.
    pub wrap_focus: bool,
    /// `scrolling:wrap_swapcol`: the same for `layoutmsg swapcol`.
    pub wrap_swapcol: bool,
    /// `scrolling:explicit_column_widths`: the widths `colresize +conf` and
    /// `-conf` step through, in the order they were written.
    pub column_widths: Widths,
}

/// Which side of a split a new dwindle window takes: `dwindle:force_split`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForceSplit {
    /// `0`: Hyprland follows the cursor, putting the new window on the half
    /// of the split the cursor is over. The layouts have no cursor, so this
    /// behaves as [`ForceSplit::Second`].
    #[default]
    Auto,
    /// `1`: always left or top.
    First,
    /// `2`: always right or bottom.
    Second,
}

/// Where the master layout puts its master: `master:orientation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    /// `left`, the default, and what an unknown name falls back to.
    #[default]
    Left,
    /// `right`.
    Right,
    /// `top`.
    Top,
    /// `bottom`.
    Bottom,
    /// `center`: the master in the middle of the screen with the stack in
    /// two columns beside it, once there are
    /// [`MasterSettings::slave_count_for_center`] of them. Below that many
    /// it is [`MasterSettings::center_fallback`] instead, which is what
    /// keeps one window from being a narrow strip in the middle of an empty
    /// screen.
    Center,
}

/// What a new window becomes in the master layout: `master:new_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NewStatus {
    /// `slave`, the default: it joins the stack.
    #[default]
    Slave,
    /// `master`: it becomes the master and the old master joins the stack.
    Master,
    /// `inherit`: master if the focused window is the master.
    Inherit,
}

/// `general:snap:*`: how near a dragged floating window has to come to
/// another window's edge, or the screen's, to land flush against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapSettings {
    /// `general:snap:enabled`. Off, as Hyprland has it.
    pub enabled: bool,
    /// `general:snap:window_gap`: how near another window's edge counts.
    /// Zero turns window snapping off and leaves the monitor's edges on.
    pub window_gap: i64,
    /// `general:snap:monitor_gap`: the same for the screen's own edges.
    pub monitor_gap: i64,
    /// `general:snap:respect_gaps`: leave `general:gaps_in` between two
    /// windows that snapped together, and `general:gaps_out` between a
    /// window and the screen's edge, rather than putting them flush.
    pub respect_gaps: bool,
    /// `general:snap:border_overlap`, which is read and does nothing: it
    /// decides whether a window's shadow and border may hang over the
    /// screen's edge, and a rectangle in this layout has nothing outside
    /// it. `crate::snap` says so at more length.
    pub border_overlap: bool,
}

/// The dwindle layout's options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DwindleSettings {
    /// `dwindle:preserve_split`: keep each split's direction when its box
    /// changes shape, rather than choosing it again from the box.
    pub preserve_split: bool,
    /// `dwindle:force_split`.
    pub force_split: ForceSplit,
    /// `dwindle:split_width_multiplier`: a box splits side by side when its
    /// width exceeds its height times this.
    pub split_width_multiplier: f64,
    /// `dwindle:default_split_ratio`, clamped to 0.1 to 1.9 as Hyprland
    /// does: the first child of a split gets this times half the box.
    pub default_split_ratio: f64,
    /// `dwindle:split_bias = 1`, which Hyprland calls `current`: the split
    /// favours the window that was already there rather than whichever of
    /// the two ends up first.
    pub split_bias_current: bool,
    /// `dwindle:use_active_for_splits`, on by default: a new window splits
    /// the *focused* window's box. With it off it splits the box the
    /// pointer is over, which is what a person pointing at a window and
    /// starting a terminal means.
    pub use_active_for_splits: bool,
    /// `dwindle:smart_split`: the pointer picks the quarter of the box the
    /// new window takes, rather than the half, so it decides the split's
    /// direction as well as its side.
    pub smart_split: bool,
    /// `dwindle:permanent_direction_override`: `layoutmsg preselect` keeps
    /// naming the side for every window until it is cleared, rather than
    /// for the next one only.
    pub permanent_direction_override: bool,
    /// `dwindle:precise_mouse_move`: a dragged window dropped back into the
    /// tiling goes on the *quarter* of the box under the pointer it was
    /// dropped on, as `smart_split` does for a new window, rather than on
    /// the half.
    pub precise_mouse_move: bool,
}

/// The master layout's options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasterSettings {
    /// `master:mfact`, clamped to 0.05 to 0.95 as Hyprland does: the part of
    /// the workspace the master takes when there is a stack.
    pub mfact: f64,
    /// `master:new_status`.
    pub new_status: NewStatus,
    /// `master:new_on_top`: new windows go to the top of the stack.
    pub new_on_top: bool,
    /// `master:orientation`.
    pub orientation: Orientation,
    /// `master:slave_count_for_center_master`: how many windows there have
    /// to be in the stack before `center` centres the master.
    pub slave_count_for_center: usize,
    /// `master:center_master_fallback`: which orientation `center` is until
    /// there are that many. `right` also decides which column takes the odd
    /// window when the stack is an odd number.
    pub center_fallback: Orientation,
    /// `master:always_keep_position`: one window alone keeps the master's
    /// share of the screen rather than filling it.
    pub always_keep_position: bool,
    /// `master:new_on_active`: where a new window goes relative to the
    /// focused one, rather than at one end of the stack.
    pub new_on_active: NewOnActive,
    /// `master:focus_master_on_close`: closing a window focuses the master
    /// rather than whatever is nearest.
    pub focus_master_on_close: bool,
    /// `master:allow_small_split`: `layoutmsg addmaster` is allowed even
    /// when it would leave fewer than two windows in the stack. Hyprland
    /// refuses without it, because a stack of one beside two masters is
    /// not what the message is for.
    pub allow_small_split: bool,
}

/// `master:new_on_active`: where a new window goes relative to the focused
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NewOnActive {
    /// `none`, the default: at one end of the stack, which end being
    /// `master:new_on_top`.
    #[default]
    End,
    /// `before`: just before the focused window.
    Before,
    /// `after`: just after it.
    After,
}

/// Everything the layouts read from the configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// `general:layout`.
    pub layout: Layout,
    /// `general:gaps_in`: the gap on each window edge that faces another
    /// window, so two neighbours are twice this apart.
    pub gaps_in: Gaps,
    /// `general:gaps_out`: the gap on each window edge that faces the
    /// monitor's edge.
    pub gaps_out: Gaps,
    /// `general:float_gaps`: the same for a *floating* window, which
    /// Hyprland keeps apart -- a floating window is put where a person
    /// wants it and often wants no margin at all, which is why the default
    /// is zero rather than `gaps_out`. A side written negative means "use
    /// `gaps_out`", which is what `CSpace::recheckWorkArea` does with one.
    pub float_gaps: Gaps,
    /// `general:border_size`, not negative: reserved inside the gap on every
    /// edge of a window that is not fullscreen.
    pub border_size: i64,
    /// `general:resize_on_border`: a press on a window's border, or just
    /// outside it, grabs that edge and resizes rather than reaching the
    /// client.
    pub resize_on_border: bool,
    /// `general:extend_border_grab_area`: how far outside the border that
    /// press still counts, which is the difference between a border a
    /// person can hit and one they cannot. Only read when
    /// `resize_on_border` is on, as Hyprland's own description says.
    pub border_grab_extend: i64,
    /// `general:snap:*`: where a dragged floating window lands.
    pub snap: SnapSettings,
    /// `general:no_focus_fallback`: when `movefocus` finds no window and no
    /// monitor in its direction, do nothing rather than wrap around to the
    /// far edge of the monitor.
    pub no_focus_fallback: bool,
    /// The dwindle layout's options.
    pub dwindle: DwindleSettings,
    /// The master layout's options.
    pub master: MasterSettings,
    /// The scrolling layout's options.
    pub scrolling: ScrollingSettings,
    /// `binds:workspace_back_and_forth`: asking for the workspace that is
    /// already shown goes to the one before it instead, which is what makes
    /// one key both there and back.
    pub workspace_back_and_forth: bool,
    /// `binds:hide_special_on_workspace_change`: the scratchpad goes away
    /// when the workspace under it changes.
    pub hide_special_on_workspace_change: bool,
    /// `misc:close_special_on_empty`: a special workspace whose last window
    /// has gone stops being shown.
    pub close_special_on_empty: bool,
    /// `dwindle:special_scale_factor` and `master:special_scale_factor`: a
    /// window on a special workspace is drawn this much of the size the
    /// layout gave it, centred in that slot, so the scratchpad looks like
    /// something over the screen rather than another workspace.
    ///
    /// One number rather than two, because only the layout in force is ever
    /// asked and a person who sets one sets the other.
    pub special_scale_factor: f64,
    /// `binds:allow_pin_fullscreen`: a fullscreen window can be pinned.
    /// Without it `pin` takes only on a floating window, which is what
    /// Hyprland's own handler does with one.
    pub allow_pin_fullscreen: bool,
    /// `binds:movefocus_cycles_fullscreen`: `movefocus` on a fullscreen
    /// window walks to the next window on the workspace rather than looking
    /// for one in that direction -- there is nothing beside a window that
    /// covers the screen, so without this the key does nothing.
    pub movefocus_cycles_fullscreen: bool,
    /// `binds:window_direction_monitor_fallback`: `movefocus` with no window
    /// in that direction moves to the monitor there.
    pub window_direction_monitor_fallback: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

/// `value` if it is a finite number, else `default`.
fn finite(value: Option<f64>, default: f64) -> f64 {
    value.filter(|value| value.is_finite()).unwrap_or(default)
}

impl Settings {
    /// The orientation `name` names, `left` for anything else.
    ///
    /// Hyprland's `defaultOrientation`: the four sides and `center`, and
    /// every other word is `left`.
    #[must_use]
    pub fn orientation_of(name: &str) -> Orientation {
        match name.trim() {
            "right" => Orientation::Right,
            "top" => Orientation::Top,
            "bottom" => Orientation::Bottom,
            "center" => Orientation::Center,
            _ => Orientation::Left,
        }
    }

    /// Read the options from `config`.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let layout = match config.str("general:layout") {
            Some(name) if name.eq_ignore_ascii_case("master") => Layout::Master,
            Some(name) if name.eq_ignore_ascii_case("monocle") => Layout::Monocle,
            Some(name) if name.eq_ignore_ascii_case("scrolling") => Layout::Scrolling,
            _ => Layout::Dwindle,
        };
        let force_split = match config.int("dwindle:force_split") {
            Some(1) => ForceSplit::First,
            Some(2) => ForceSplit::Second,
            _ => ForceSplit::Auto,
        };
        let named = |name: Option<&str>| name.map_or(Orientation::Left, Self::orientation_of);
        let orientation = named(config.str("master:orientation"));
        // `center` is not a fallback for itself: Hyprland's own list of
        // fallbacks is the four sides, and anything else is `left`.
        let center_fallback = match config.str("master:center_master_fallback") {
            Some("center") | None => Orientation::Left,
            other => named(other),
        };
        let new_status = match config.str("master:new_status") {
            Some("master") => NewStatus::Master,
            Some("inherit") => NewStatus::Inherit,
            _ => NewStatus::Slave,
        };
        Self {
            layout,
            gaps_in: config.gaps("general:gaps_in").unwrap_or(Gaps::all(5)),
            gaps_out: config.gaps("general:gaps_out").unwrap_or(Gaps::all(20)),
            float_gaps: {
                let written = config.gaps("general:float_gaps").unwrap_or(Gaps::all(0));
                let out = config.gaps("general:gaps_out").unwrap_or(Gaps::all(20));
                if written.top < 0 || written.right < 0 || written.bottom < 0 || written.left < 0 {
                    out
                } else {
                    written
                }
            },
            border_size: config.int("general:border_size").unwrap_or(1).max(0),
            resize_on_border: config.bool("general:resize_on_border").unwrap_or(false),
            border_grab_extend: config
                .int("general:extend_border_grab_area")
                .unwrap_or(15)
                .max(0),
            snap: SnapSettings {
                enabled: config.bool("general:snap:enabled").unwrap_or(false),
                window_gap: config.int("general:snap:window_gap").unwrap_or(10).max(0),
                monitor_gap: config.int("general:snap:monitor_gap").unwrap_or(10).max(0),
                respect_gaps: config.bool("general:snap:respect_gaps").unwrap_or(false),
                border_overlap: config.bool("general:snap:border_overlap").unwrap_or(false),
            },
            no_focus_fallback: config.bool("general:no_focus_fallback").unwrap_or(false),
            workspace_back_and_forth: config
                .bool("binds:workspace_back_and_forth")
                .unwrap_or(false),
            hide_special_on_workspace_change: config
                .bool("binds:hide_special_on_workspace_change")
                .unwrap_or(false),
            close_special_on_empty: config.bool("misc:close_special_on_empty").unwrap_or(true),
            allow_pin_fullscreen: config.bool("binds:allow_pin_fullscreen").unwrap_or(false),
            movefocus_cycles_fullscreen: config
                .bool("binds:movefocus_cycles_fullscreen")
                .unwrap_or(false),
            window_direction_monitor_fallback: config
                .bool("binds:window_direction_monitor_fallback")
                .unwrap_or(true),
            special_scale_factor: finite(
                config.float(match layout {
                    Layout::Master => "master:special_scale_factor",
                    // Monocle has no scale factor of its own, and
                    // `dwindle`'s is the one Hyprland reads for a window
                    // whose layout does not offer one.
                    Layout::Dwindle | Layout::Monocle | Layout::Scrolling => {
                        "dwindle:special_scale_factor"
                    }
                }),
                1.0,
            )
            .clamp(0.0, 1.0),
            scrolling: ScrollingSettings {
                column_width: finite(config.float("scrolling:column_width"), 0.5).clamp(0.1, 1.0),
                focus_fit_method: match config.int("scrolling:focus_fit_method") {
                    Some(0) => FitMethod::Center,
                    _ => FitMethod::Fit,
                },
                follow_focus: config.bool("scrolling:follow_focus").unwrap_or(true),
                fullscreen_on_one_column: config
                    .bool("scrolling:fullscreen_on_one_column")
                    .unwrap_or(true),
                wrap_focus: config.bool("scrolling:wrap_focus").unwrap_or(true),
                wrap_swapcol: config.bool("scrolling:wrap_swapcol").unwrap_or(true),
                column_widths: Widths::parse(
                    config
                        .str("scrolling:explicit_column_widths")
                        .unwrap_or_default(),
                ),
            },
            dwindle: DwindleSettings {
                preserve_split: config.bool("dwindle:preserve_split").unwrap_or(false),
                force_split,
                split_width_multiplier: finite(config.float("dwindle:split_width_multiplier"), 1.0),
                split_bias_current: config.int("dwindle:split_bias").unwrap_or(0) == 1,
                default_split_ratio: finite(config.float("dwindle:default_split_ratio"), 1.0)
                    .clamp(0.1, 1.9),
                use_active_for_splits: config.bool("dwindle:use_active_for_splits").unwrap_or(true),
                smart_split: config.bool("dwindle:smart_split").unwrap_or(false),
                permanent_direction_override: config
                    .bool("dwindle:permanent_direction_override")
                    .unwrap_or(false),
                precise_mouse_move: config.bool("dwindle:precise_mouse_move").unwrap_or(false),
            },
            master: MasterSettings {
                mfact: finite(config.float("master:mfact"), 0.55).clamp(0.05, 0.95),
                new_status,
                new_on_top: config.bool("master:new_on_top").unwrap_or(false),
                orientation,
                slave_count_for_center: usize::try_from(
                    config
                        .int("master:slave_count_for_center_master")
                        .unwrap_or(2),
                )
                .unwrap_or(2),
                center_fallback,
                always_keep_position: config.bool("master:always_keep_position").unwrap_or(false),
                new_on_active: match config.str("master:new_on_active") {
                    Some("before") => NewOnActive::Before,
                    Some("after") => NewOnActive::After,
                    _ => NewOnActive::End,
                },
                focus_master_on_close: config.bool("master:focus_master_on_close").unwrap_or(false),
                allow_small_split: config.bool("master:allow_small_split").unwrap_or(false),
            },
        }
    }
}
