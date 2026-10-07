//! Monitors, workspaces and windows, and what the dispatchers do to them.
//!
//! Workspaces are created on demand, as Hyprland's are: switching to a
//! number that does not exist makes it on the focused monitor, and a
//! workspace that is neither shown nor holding a window is removed. Each
//! workspace belongs to one monitor and keeps its tiled windows in the
//! configured layout, its floating windows in stacking order with their own
//! rectangles, and at most one fullscreen window.
//!
//! Focus follows Hyprland's focus history: one list of windows, most
//! recently focused last. The focused window of a workspace is the most
//! recent one on it, and the focused window overall is that of the
//! workspace the focused monitor shows. Closing a window therefore hands
//! focus back to the one focused before it, and `movefocus` breaks ties
//! between several neighbours by recency, as `binds:focus_preferred_method`
//! 0 does.
//!
//! A fullscreen window, in either mode, hides every other window on its
//! workspace, as Hyprland's `setFullscreenFadeAnimation` fades them to
//! nothing. Hyprland leaves visible the floating windows opened or raised
//! over it (`m_createdOverFullscreen`); here they stay hidden, and a new
//! window opens behind the fullscreen one. The direction searches skip the
//! hidden windows, where Hyprland's find them on other monitors.
//!
//! Every public method that changes anything returns the [`Change`]s it
//! caused. The ones only a caller can act on (a close request, a window
//! moved, floated or made fullscreen) are reported as they happen; which
//! monitor has focus, what each monitor shows, whose geometry changed and
//! which window has focus are found by comparing the state before and
//! after, so none can be forgotten.

use std::collections::{BTreeMap, BTreeSet};
use std::f64::consts::{FRAC_PI_2, PI};

use compositor_config::{Bind, Config, Gaps, WorkspaceRule};

use crate::dispatch::{
    Direction, Dispatcher, FullscreenMode, GroupMember, Locking, MonitorTarget, Move,
    WorkspaceOption, WorkspaceTarget,
};
use crate::dwindle::Dwindle;
use crate::geometry::{self, Area, overlap, sticks};
use crate::master::Master;
use crate::monocle::Monocle;
use crate::scrolling::Scrolling;
use crate::settings::{Layout, Orientation, Settings};
use crate::{Corner, Error, Limits, Monitor, MonitorId, Rect, WindowId, WorkspaceId};

/// Something a change to the state did that the caller acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The window should be asked to close. It stays until the caller
    /// reports it gone with [`State::window_gone`].
    Close(WindowId),
    /// The window moved to another workspace.
    MoveToWorkspace {
        /// The window.
        window: WindowId,
        /// The workspace it is on now.
        workspace: WorkspaceId,
    },
    /// The window started or stopped floating.
    Floating {
        /// The window.
        window: WindowId,
        /// Whether it floats now.
        floating: bool,
    },
    /// The window became fullscreen, changed mode, or stopped being
    /// fullscreen.
    Fullscreen {
        /// The window.
        window: WindowId,
        /// Its mode now, `None` if it is not fullscreen.
        mode: Option<FullscreenMode>,
    },
    /// Another monitor has focus.
    FocusMonitor(MonitorId),
    /// A monitor shows another workspace.
    Workspace {
        /// The monitor.
        monitor: MonitorId,
        /// The workspace it shows now.
        workspace: WorkspaceId,
    },
    /// What a monitor shows changed geometry: a window appeared, went, moved
    /// or resized.
    Layout(MonitorId),
    /// Another window has focus, or none.
    Focus(Option<WindowId>),
    /// A floating window was pinned to every workspace of its monitor, or
    /// unpinned.
    Pinned {
        /// The window.
        window: WindowId,
        /// Whether it is pinned now.
        pinned: bool,
    },
    /// A tiled window was made pseudotiled, or made ordinary again.
    Pseudo {
        /// The window.
        window: WindowId,
        /// Whether it is pseudotiled now.
        pseudo: bool,
    },
    /// A workspace was given a name, or had one taken away.
    Renamed {
        /// The workspace.
        workspace: WorkspaceId,
        /// Its name now; empty puts the number back.
        name: String,
    },
}

/// A visible window's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    /// The window.
    pub window: WindowId,
    /// Its client area: what the client is configured to, with the border
    /// drawn outside it.
    pub rect: Rect,
    /// The border's width on every side, zero when fullscreen.
    pub border: i64,
    /// Whether it is the focused window, which draws the active border.
    pub focused: bool,
    /// Whether it floats.
    pub floating: bool,
    /// Whether it is fullscreen or maximized.
    pub fullscreen: bool,
}

/// What one monitor shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLayout {
    /// The monitor.
    pub monitor: MonitorId,
    /// The workspace it shows.
    pub workspace: WorkspaceId,
    /// The visible windows, bottom to top: tiled windows, then floating ones
    /// in stacking order; only the fullscreen window when there is one.
    pub windows: Vec<Placed>,
}

/// A workspace's tiled windows, in the layout it was made with.
#[derive(Debug, Clone, PartialEq)]
enum Tiling {
    Dwindle(Dwindle),
    Master(Master),
    Monocle(Monocle),
    Scrolling(Scrolling),
}

impl Tiling {
    fn new(layout: Layout) -> Self {
        match layout {
            Layout::Dwindle => Self::Dwindle(Dwindle::default()),
            Layout::Master => Self::Master(Master::default()),
            Layout::Monocle => Self::Monocle(Monocle::default()),
            Layout::Scrolling => Self::Scrolling(Scrolling::default()),
        }
    }

    /// Add `new`, beside or after `focused`, the workspace's most recently
    /// focused tiled window.
    fn insert(
        &mut self,
        new: WindowId,
        focused: Option<WindowId>,
        cursor: Option<(f64, f64)>,
        area: Area,
        settings: &Settings,
    ) {
        match self {
            Self::Dwindle(dwindle) => dwindle.insert(new, focused, cursor, area, settings),
            Self::Master(master) => master.insert(new, focused, settings),
            Self::Monocle(monocle) => monocle.insert(new),
            Self::Scrolling(scrolling) => scrolling.insert(new, focused, settings, area.w),
        }
    }

    /// Add `new` at a point, as the dwindle layout puts back a window
    /// `movewindow` took out; the master layout adds it as it would a new
    /// one.
    fn insert_at(
        &mut self,
        new: WindowId,
        point: (f64, f64),
        toward: Option<Direction>,
        area: Area,
        settings: &Settings,
    ) {
        match self {
            Self::Dwindle(dwindle) => dwindle.insert_at(new, point, toward, area, settings),
            Self::Master(master) => master.insert(new, None, settings),
            Self::Monocle(monocle) => monocle.insert(new),
            Self::Scrolling(scrolling) => scrolling.insert(new, None, settings, area.w),
        }
    }

    /// Add `new`, dropped at a point by a drag: the dwindle layout puts it
    /// beside the box under the pointer ([`Dwindle::insert_dropped`]), and
    /// the others add it as they would a new one.
    fn insert_dropped(
        &mut self,
        new: WindowId,
        point: (f64, f64),
        area: Area,
        settings: &Settings,
    ) {
        match self {
            Self::Dwindle(dwindle) => dwindle.insert_dropped(new, point, area, settings),
            Self::Master(master) => master.insert(new, None, settings),
            Self::Monocle(monocle) => monocle.insert(new),
            Self::Scrolling(scrolling) => scrolling.insert(new, None, settings, area.w),
        }
    }

    fn remove(&mut self, window: WindowId) {
        match self {
            Self::Dwindle(dwindle) => dwindle.remove(window),
            Self::Master(master) => master.remove(window),
            Self::Monocle(monocle) => monocle.remove(window),
            Self::Scrolling(scrolling) => scrolling.remove(window),
        }
    }

    /// Exchange two windows' places, whichever layout holds them.
    fn swap(&mut self, a: WindowId, b: WindowId) {
        match self {
            Self::Dwindle(dwindle) => dwindle.swap(a, b),
            Self::Master(master) => master.swap(a, b),
            Self::Monocle(monocle) => monocle.swap(a, b),
            Self::Scrolling(scrolling) => scrolling.swap(a, b),
        }
    }

    /// The dwindle tree, when that is the layout: `layoutmsg` speaks to
    /// one layout and says nothing to the other, as in Hyprland.
    const fn dwindle(&mut self) -> Option<&mut Dwindle> {
        match self {
            Self::Dwindle(dwindle) => Some(dwindle),
            _ => None,
        }
    }

    /// Which windows the master layout holds as masters, if that is the
    /// layout.
    fn master_windows(&self) -> &[WindowId] {
        match self {
            Self::Master(master) => master.masters(),
            _ => &[],
        }
    }

    /// The window the master layout holds as its master, if that is the
    /// layout and it has one.
    fn master_window(&self) -> Option<WindowId> {
        match self {
            Self::Master(master) => master.master(),
            _ => None,
        }
    }

    /// The tape, when that is the layout.
    const fn scrolling(&mut self) -> Option<&mut Scrolling> {
        match self {
            Self::Scrolling(scrolling) => Some(scrolling),
            _ => None,
        }
    }

    /// The master list, when that is the layout.
    const fn master(&mut self) -> Option<&mut Master> {
        match self {
            Self::Master(master) => Some(master),
            _ => None,
        }
    }

    fn contains(&self, window: WindowId) -> bool {
        match self {
            Self::Dwindle(dwindle) => dwindle.contains(window),
            Self::Master(master) => master.contains(window),
            Self::Monocle(monocle) => monocle.contains(window),
            Self::Scrolling(scrolling) => scrolling.contains(window),
        }
    }

    fn windows(&self) -> Vec<WindowId> {
        match self {
            Self::Dwindle(dwindle) => dwindle.windows(),
            Self::Master(master) => master.windows(),
            Self::Monocle(monocle) => monocle.windows(),
            Self::Scrolling(scrolling) => scrolling.windows(),
        }
    }

    /// Each window's box. `shown` is the workspace's focused window, which
    /// only the monocle layout reads: it draws one window and that is the
    /// one.
    fn slots(
        &self,
        area: Area,
        settings: &Settings,
        shown: Option<WindowId>,
    ) -> Vec<(WindowId, Area)> {
        match self {
            Self::Dwindle(dwindle) => dwindle.slots(area, settings),
            Self::Master(master) => master.slots(area, settings),
            Self::Monocle(monocle) => monocle.slots(area, shown),
            Self::Scrolling(scrolling) => scrolling.slots(area, settings, shown),
        }
    }

    /// Resize a tiled window, which each layout does by moving whatever
    /// decides its size. Gives whether anything moved.
    ///
    /// Only the dwindle layout so far, whose splits are what a tiled window's
    /// box comes from. The master layout's `mfact` and the scrolling
    /// layout's column widths are the same idea and are not done here; the
    /// monocle layout has nothing to move, since its window is the
    /// workspace.
    fn resize(
        &mut self,
        window: WindowId,
        by: (f64, f64),
        corner: Corner,
        area: Area,
        settings: &Settings,
    ) -> bool {
        match self {
            Self::Dwindle(dwindle) => dwindle.resize(window, by, corner, area, settings),
            Self::Master(_) | Self::Monocle(_) | Self::Scrolling(_) => false,
        }
    }

    fn settle(&mut self, area: Area, settings: &Settings) {
        if let Self::Dwindle(dwindle) = self {
            dwindle.settle(area, settings);
        }
    }
}

/// A workspace.
#[derive(Debug, Clone, PartialEq)]
struct Workspace {
    /// The monitor it belongs to, which may have been unplugged.
    monitor: MonitorId,
    tiling: Tiling,
    /// Floating windows, bottom to top.
    floating: Vec<WindowId>,
    fullscreen: Option<(WindowId, FullscreenMode)>,
}

impl Workspace {
    fn is_empty(&self) -> bool {
        self.floating.is_empty() && self.tiling.windows().is_empty()
    }
}

/// A monitor, the workspace it shows, and the special workspace over it.
///
/// A special workspace is not shown *instead of* the normal one: it is drawn
/// over it, which is what makes Hyprland's scratchpad a scratchpad. A monitor
/// shows at most one at a time.
#[derive(Debug, Clone, PartialEq)]
struct Output {
    monitor: Monitor,
    active: WorkspaceId,
    /// What it showed before `active`, which is what `workspace previous`
    /// and `binds:workspace_back_and_forth` go to.
    previous: Option<WorkspaceId>,
    special: Option<WorkspaceId>,
}

/// The part of the state [`Change`]s are derived from.
struct Snapshot {
    focus: Option<WindowId>,
    monitor: Option<MonitorId>,
    layout: Vec<MonitorLayout>,
}

/// A window a direction search can find.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    window: WindowId,
    workspace: WorkspaceId,
    /// Where the layout put it: its slot if tiled, its rectangle if
    /// floating, the monitor if fullscreen. Hyprland's `m_position` and
    /// `m_size`.
    placed: Rect,
    /// `placed` as the searches compare it; see [`ideal_box`].
    ideal: Rect,
    floating: bool,
    fullscreen: bool,
}

/// Monitors, workspaces, windows and focus.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    settings: Settings,
    /// The `workspace =` lines, which say what one workspace is unlike the
    /// others: its gaps, its border, its layout, which monitor it lives on
    /// and whether it exists with nothing on it.
    rules: Vec<WorkspaceRule>,
    /// In the order they were added.
    outputs: Vec<Output>,
    focused_monitor: Option<MonitorId>,
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    /// Which workspace each window is on.
    windows: BTreeMap<WindowId, WorkspaceId>,
    /// Each window's floating rectangle, relative to its monitor's origin
    /// so it moves with the window between monitors, kept while the window
    /// is tiled so floating it again puts it back.
    floating_rects: BTreeMap<WindowId, Rect>,
    /// Tiled windows a drag has lifted out of the tiling, with the floating
    /// rectangle each had before -- `None` for one that never floated --
    /// which is given back when it is dropped in again: lifting one is not
    /// floating it, and must not change where `togglefloating` puts it.
    lifted: BTreeMap<WindowId, Option<Rect>>,
    /// Where the pointer is, in the global space the monitors share, or
    /// `None` on a compositor that has not seen one move.
    ///
    /// The layouts are arithmetic and have no devices, but three of
    /// Hyprland's dwindle options are about where the pointer *is* when a
    /// window opens -- it reads `getMouseCoordsInternal()` from the input
    /// manager at that moment -- so the compositor keeps this up to date
    /// and the tree reads it. Nothing else in the crate looks at it.
    pointer: Option<(f64, f64)>,
    /// Every window, most recently focused last.
    history: Vec<WindowId>,
    /// Each window the focus went to since [`State::take_focus_trail`] was
    /// last asked, in order: `None` where nothing was focused.
    focus_trail: Vec<Option<WindowId>>,
    /// The name of each workspace that has one, which is the special ones:
    /// a numbered workspace's name is its number.
    names: BTreeMap<WorkspaceId, String>,
    /// Every group, by its head -- the member the tiling tree holds.
    groups: BTreeMap<WindowId, Group>,
    /// `lockgroups`: whether a window may be added to a group. Nothing adds
    /// one on its own here, so this is recorded and reported and changes
    /// nothing; `moveintogroup` is a person asking, and Hyprland's lock does
    /// not stop that either.
    groups_locked: bool,
    /// `pin`: floating windows that stay on every workspace of their
    /// monitor.
    pinned: BTreeSet<WindowId>,
    /// `pseudo`: tiled windows drawn at the size they asked for, in the
    /// middle of the slot the tiling gave them.
    pseudo: BTreeSet<WindowId>,
    /// `min_size`, `max_size` and `keep_aspect_ratio`: how big a window
    /// may be, by window.
    ///
    /// A `windowrule`'s, kept here because every size a window is given
    /// has to respect it -- the rule that opened it, a manual resize, and
    /// the layout putting it back where it floated.
    limits: BTreeMap<WindowId, Limits>,
    /// `tagwindow`: the tags each window carries, which a `windowrule` can
    /// match on.
    tags: BTreeMap<WindowId, Vec<String>>,
    /// `denywindowfromgroup`: whether a window opened by the focused one
    /// joins its group. Recorded and reported, as `lockgroups` is.
    deny_from_group: bool,
}

/// A group: windows sharing one tiling slot, of which one is shown.
///
/// Hyprland's `togglegroup` makes a window a group of one; another window
/// moved into it becomes a tab. Only the active member is drawn, in the slot
/// the group's head holds in the tiling tree -- the head stays put while the
/// active tab changes, which is what keeps the layout still as a person
/// cycles through them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// The members, in tab order. The first is the head, and it is the one
    /// the tiling tree knows about.
    pub members: Vec<WindowId>,
    /// Which member is shown, an index into `members`.
    pub active: usize,
    /// `lockactivegroup`: whether this group takes any more windows.
    /// Recorded and reported, as `lockgroups` is: nothing here adds a
    /// window to a group on its own, and `moveintogroup` is a person
    /// asking.
    pub locked: bool,
}

impl Group {
    /// The member that is shown.
    #[must_use]
    pub fn showing(&self) -> Option<WindowId> {
        self.members.get(self.active).copied()
    }

    /// The head, which holds the group's place in the tiling.
    #[must_use]
    pub fn head(&self) -> Option<WindowId> {
        self.members.first().copied()
    }
}

/// The lowest id a special workspace has, `SPECIAL_WORKSPACE_START` in
/// Hyprland's `macros.hpp`. Every special workspace's id is between it and
/// −2.
const SPECIAL_START: i64 = -99;

impl State {
    /// No monitors and no windows.
    #[must_use]
    pub const fn new(settings: Settings) -> Self {
        Self {
            settings,
            rules: Vec::new(),
            outputs: Vec::new(),
            focused_monitor: None,
            workspaces: BTreeMap::new(),
            windows: BTreeMap::new(),
            floating_rects: BTreeMap::new(),
            lifted: BTreeMap::new(),
            history: Vec::new(),
            focus_trail: Vec::new(),
            names: BTreeMap::new(),
            groups: BTreeMap::new(),
            groups_locked: false,
            pinned: BTreeSet::new(),
            pseudo: BTreeSet::new(),
            limits: BTreeMap::new(),
            tags: BTreeMap::new(),
            deny_from_group: false,
            pointer: None,
        }
    }

    /// Tell the layouts where the pointer is.
    ///
    /// `dwindle:use_active_for_splits`, `dwindle:force_split = 0` and
    /// `dwindle:smart_split` are all about the window the pointer is over
    /// when a new one opens, and this is how they know. A compositor that
    /// never calls it leaves every layout behaving as if it had no pointer,
    /// which is what a headless test wants.
    pub const fn set_pointer(&mut self, at: (f64, f64)) {
        self.pointer = Some(at);
    }

    /// No monitors and no windows, with the options `config` sets.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::new(Settings::from_config(config))
    }

    // -- Queries --------------------------------------------------------------

    /// The options in force.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The options in force *on one workspace*: the general ones with
    /// whatever its `workspace =` lines changed.
    ///
    /// Hyprland reads `gapsin`, `gapsout`, `bordersize` and `layout` out of
    /// the workspace's rule wherever it would have read the option, which
    /// is what lets a person keep one workspace edge to edge -- a browser,
    /// a video -- while every other one has its gaps.
    #[must_use]
    pub fn settings_at(&self, workspace: WorkspaceId) -> Settings {
        let mut settings = self.settings;
        if self.rules.is_empty() {
            return settings;
        }
        let name = self.workspace_name(workspace);
        let rule = compositor_config::rules_for(&self.rules, workspace.0, &name);
        if let Some(gaps) = rule.gaps_in {
            settings.gaps_in = gaps;
        }
        if let Some(gaps) = rule.gaps_out {
            settings.gaps_out = gaps;
        }
        // `border:false` is not `bordersize:0` in Hyprland either; both end
        // at the same place here, because a border of no pixels is a
        // window with none.
        if rule.no_border == Some(true) {
            settings.border_size = 0;
        } else if let Some(size) = rule.border_size {
            settings.border_size = size.max(0);
        }
        if let Some(layout) = &rule.layout {
            settings.layout = if layout.eq_ignore_ascii_case("master") {
                Layout::Master
            } else {
                Layout::Dwindle
            };
        }
        // `layoutopt:<name>:<value>`: one of the layout's own options for
        // this workspace alone. Hyprland's `defaultOrientation` reads the
        // workspace rule's before it reads `master:orientation`, which is
        // how a person keeps one workspace's master on top.
        for (name, value) in &rule.layout_options {
            match name.as_str() {
                "orientation" => settings.master.orientation = Settings::orientation_of(value),
                "mfact" => {
                    if let Ok(mfact) = value.parse::<f64>() {
                        settings.master.mfact = mfact.clamp(0.05, 0.95);
                    }
                }
                _ => {}
            }
        }
        settings
    }

    /// Take the `workspace =` lines, and make the workspaces they say must
    /// exist.
    ///
    /// `persistent:true` is the one that has to be acted on at once: the
    /// workspace exists with nothing on it, which is what puts it in a
    /// bar's list before anything has been opened there. `defaultName:`
    /// names it, and `monitor:` says which screen it belongs to.
    pub fn set_workspace_rules(&mut self, rules: Vec<WorkspaceRule>) -> Vec<Change> {
        self.rules = rules;
        let mut changes = Vec::new();
        let persistent: Vec<WorkspaceRule> = self
            .rules
            .iter()
            .filter(|rule| rule.is_persistent == Some(true))
            .cloned()
            .collect();
        for rule in persistent {
            let compositor_config::Which::Id(id) = rule.which else {
                // A persistent workspace has to have a number to be made;
                // Hyprland's own `WORKSPACE_INVALID` path skips the rest.
                continue;
            };
            let workspace = WorkspaceId(id);
            let monitor = self.monitor_for(rule.monitor.as_deref());
            let Some(monitor) = monitor else { continue };
            if self.workspaces.contains_key(&workspace) {
                continue;
            }
            self.ensure_workspace(workspace, monitor);
            if let Some(name) = &rule.default_name {
                let _previous = self.names.insert(workspace, name.clone());
            }
            changes.push(Change::Layout(monitor));
        }
        changes
    }

    /// Which monitor a `monitor:` field names, or the focused one.
    fn monitor_for(&self, name: Option<&str>) -> Option<MonitorId> {
        let Some(name) = name else {
            return self.focused_monitor.or_else(|| self.first_monitor());
        };
        self.outputs
            .iter()
            .find(|output| output.monitor.name == name)
            .map(|output| output.monitor.id)
            .or_else(|| self.focused_monitor.or_else(|| self.first_monitor()))
    }

    /// The first monitor there is, for a rule that named none.
    fn first_monitor(&self) -> Option<MonitorId> {
        self.outputs.first().map(|output| output.monitor.id)
    }

    /// Which monitor a workspace's rule says it belongs to, if one does.
    ///
    /// `workspace = 3, monitor:DP-1` is how a person nails a workspace to a
    /// screen, and it is read when the workspace is first made.
    #[must_use]
    pub fn workspace_monitor_rule(&self, workspace: WorkspaceId) -> Option<MonitorId> {
        if self.rules.is_empty() {
            return None;
        }
        let name = self.workspace_name(workspace);
        let rule = compositor_config::rules_for(&self.rules, workspace.0, &name);
        self.outputs
            .iter()
            .find(|output| output.monitor.name == rule.monitor.clone().unwrap_or_default())
            .map(|output| output.monitor.id)
    }

    /// The command a `workspace = …, on-created-empty:` line names for a
    /// workspace that has just been made with nothing on it.
    #[must_use]
    pub fn on_created_empty(&self, workspace: WorkspaceId) -> Option<String> {
        if self.rules.is_empty() {
            return None;
        }
        let name = self.workspace_name(workspace);
        compositor_config::rules_for(&self.rules, workspace.0, &name).on_created_empty
    }

    /// The monitors, in the order they were added.
    pub fn monitors(&self) -> impl Iterator<Item = &Monitor> {
        self.outputs.iter().map(|output| &output.monitor)
    }

    /// The focused monitor.
    #[must_use]
    pub const fn focused_monitor(&self) -> Option<MonitorId> {
        self.focused_monitor
    }

    /// The workspace `monitor` shows.
    #[must_use]
    pub fn active_workspace(&self, monitor: MonitorId) -> Option<WorkspaceId> {
        self.output(monitor).map(|output| output.active)
    }

    /// The workspace the focused monitor shows.
    #[must_use]
    pub fn current_workspace(&self) -> Option<WorkspaceId> {
        self.focused_monitor
            .and_then(|monitor| self.active_workspace(monitor))
    }

    /// The focused window.
    #[must_use]
    pub fn focused_window(&self) -> Option<WindowId> {
        // A monitor showing a scratchpad has two workspaces on it, and the
        // focus may be on either: the most recently focused window of the
        // two is the focused one.
        let monitor = self.focused_monitor?;
        let special = self.special_on(monitor).and_then(|id| self.focused_on(id));
        let own = self
            .active_workspace(monitor)
            .and_then(|id| self.focused_on(id));
        match (special, own) {
            (Some(one), Some(other)) => {
                let at = |window: WindowId| self.history.iter().rposition(|id| *id == window);
                if at(one) >= at(other) {
                    Some(one)
                } else {
                    Some(other)
                }
            }
            (found, None) | (None, found) => found,
        }
    }

    /// The workspaces that exist, in number order.
    pub fn workspaces(&self) -> impl Iterator<Item = WorkspaceId> + '_ {
        self.workspaces.keys().copied()
    }

    /// The monitor a workspace belongs to.
    #[must_use]
    pub fn workspace_monitor(&self, workspace: WorkspaceId) -> Option<MonitorId> {
        self.workspaces.get(&workspace).map(|ws| ws.monitor)
    }

    /// The windows on a workspace: tiled ones in layout order, then floating
    /// ones bottom to top.
    #[must_use]
    pub fn windows(&self, workspace: WorkspaceId) -> Vec<WindowId> {
        self.workspaces
            .get(&workspace)
            .map(|ws| {
                let mut windows = ws.tiling.windows();
                windows.extend(ws.floating.iter().copied());
                windows
            })
            .unwrap_or_default()
    }

    /// The workspace a window is on.
    #[must_use]
    pub fn workspace_of(&self, window: WindowId) -> Option<WorkspaceId> {
        self.windows.get(&window).copied()
    }

    /// Whether a window floats.
    #[must_use]
    pub fn is_floating(&self, window: WindowId) -> bool {
        self.workspace_of(window)
            .and_then(|workspace| self.workspaces.get(&workspace))
            .is_some_and(|ws| ws.floating.contains(&window))
    }

    /// A workspace's fullscreen window and its mode.
    #[must_use]
    pub fn fullscreen(&self, workspace: WorkspaceId) -> Option<(WindowId, FullscreenMode)> {
        self.workspaces.get(&workspace).and_then(|ws| ws.fullscreen)
    }

    /// For the workspace each monitor shows, in monitor order, every visible
    /// window's place.
    #[must_use]
    pub fn layout(&self) -> Vec<MonitorLayout> {
        let focus = self.focused_window();
        self.outputs
            .iter()
            .map(|output| MonitorLayout {
                monitor: output.monitor.id,
                workspace: output.active,
                windows: self.with_special(output, focus),
            })
            .collect()
    }

    /// A monitor's windows: its workspace's, with its special workspace's
    /// over them.
    fn with_special(&self, output: &Output, focus: Option<WindowId>) -> Vec<Placed> {
        let mut windows = self.placements(output, focus);
        if let Some(special) = output.special {
            let over = Output {
                monitor: output.monitor.clone(),
                active: special,
                previous: None,
                special: None,
            };
            // `special_scale_factor`: a window on the scratchpad is drawn
            // that much of the size the layout gave it, centred in its
            // slot, so that the scratchpad looks like something *over* the
            // screen rather than another workspace
            // (`CWindowTarget::applyToWindow`).
            let scale = self.settings_at(special).special_scale_factor;
            windows.extend(self.placements(&over, focus).into_iter().map(|mut placed| {
                if !placed.fullscreen && scale < 1.0 {
                    placed.rect = shrunk(placed.rect, scale);
                }
                placed
            }));
        }
        windows
    }

    // -- Monitors -------------------------------------------------------------

    /// Say what a monitor has reserved, and re-tile it.
    ///
    /// The strips a layer surface's exclusive zone takes off the monitor:
    /// a bar across the top means the windows start below it. Nothing else
    /// sets them, and `src/user/system/linux/compositor/layout` works none of them out -- where a
    /// layer surface goes is [`crate::layers`]' and what it reserves is that
    /// module's answer, because it is the protocol's rule and not the
    /// tiling's.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownMonitor`] for a monitor that is not there.
    pub fn set_reserved(
        &mut self,
        monitor: MonitorId,
        reserved: Gaps,
    ) -> Result<Vec<Change>, Error> {
        if self.output(monitor).is_none() {
            return Err(Error::UnknownMonitor(monitor));
        }
        Ok(self.run(|state| {
            if let Some(output) = state
                .outputs
                .iter_mut()
                .find(|output| output.monitor.id == monitor)
            {
                output.monitor.reserved = reserved;
            }
            Vec::new()
        }))
    }

    /// Move a monitor, or change how much of it a logical pixel is.
    ///
    /// The workspaces stay where they are: moving a screen in the space all
    /// screens share is not unplugging it, and a window on it is still on
    /// it. That is what `wlr-randr --output DP-1 --pos 1920,0` means, and
    /// what a settings panel sends through
    /// `zwlr_output_configuration_v1.apply`.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownMonitor`] for a monitor that is not there.
    pub fn move_monitor(
        &mut self,
        monitor: MonitorId,
        rect: Rect,
        scale: f64,
    ) -> Result<Vec<Change>, Error> {
        if self.output(monitor).is_none() {
            return Err(Error::UnknownMonitor(monitor));
        }
        Ok(self.run(|state| {
            if let Some(output) = state
                .outputs
                .iter_mut()
                .find(|output| output.monitor.id == monitor)
            {
                output.monitor.rect = rect;
                output.monitor.scale = scale;
            }
            Vec::new()
        }))
    }

    /// Add a monitor. It shows the lowest-numbered workspace left without a
    /// monitor by an unplug, taking all of those, or else the lowest number
    /// not in use. The first monitor gets focus.
    pub fn add_monitor(&mut self, monitor: Monitor) -> Result<Vec<Change>, Error> {
        if self.output(monitor.id).is_some() {
            return Err(Error::DuplicateMonitor(monitor.id));
        }
        Ok(self.run(|state| {
            let orphans: Vec<WorkspaceId> = state
                .workspaces
                .iter()
                .filter(|(_, ws)| state.output(ws.monitor).is_none())
                .map(|(id, _)| *id)
                .collect();
            for id in &orphans {
                if let Some(ws) = state.workspaces.get_mut(id) {
                    ws.monitor = monitor.id;
                }
            }
            let active = orphans
                .first()
                .copied()
                .unwrap_or_else(|| state.first_free_workspace());
            let id = monitor.id;
            state.ensure_workspace(active, id);
            state.outputs.push(Output {
                monitor,
                active,
                previous: None,
                special: None,
            });
            if state.focused_monitor.is_none() {
                state.focused_monitor = Some(id);
            }
            Vec::new()
        }))
    }

    /// Remove a monitor. Its workspaces go to the first monitor left, or
    /// wait for the next one added if none is.
    pub fn remove_monitor(&mut self, id: MonitorId) -> Result<Vec<Change>, Error> {
        if self.output(id).is_none() {
            return Err(Error::UnknownMonitor(id));
        }
        Ok(self.run(|state| {
            state.outputs.retain(|output| output.monitor.id != id);
            let fallback = state.outputs.first().map(|output| output.monitor.id);
            if let Some(fallback) = fallback {
                state
                    .workspaces
                    .values_mut()
                    .filter(|ws| ws.monitor == id)
                    .for_each(|ws| ws.monitor = fallback);
            }
            if state.focused_monitor == Some(id) {
                state.focused_monitor = fallback;
            }
            Vec::new()
        }))
    }

    // -- Windows --------------------------------------------------------------

    /// A new window, tiled on the workspace the focused monitor shows, and
    /// focused unless a fullscreen window is there: Hyprland's default
    /// `misc:new_window_takes_over_fullscreen` of 0 opens it behind.
    pub fn open_window(&mut self, window: WindowId) -> Result<Vec<Change>, Error> {
        self.open(window, None)
    }

    /// A new window, floating at `rect` on the workspace the focused monitor
    /// shows, and focused as [`State::open_window`] would.
    pub fn open_floating(&mut self, window: WindowId, rect: Rect) -> Result<Vec<Change>, Error> {
        self.open(window, Some(rect))
    }

    /// Float a window that is already open, at `rect`.
    ///
    /// A window that floats already is moved and resized; one that is tiled
    /// comes out of the tiling and out of any group it is in, as
    /// `togglefloating` takes it. This is what a `windowrule` asking for a
    /// size or a place does, and what a dispatcher that moves a floating
    /// window by pixels will.
    ///
    /// # Errors
    ///
    /// A window this state does not have.
    pub fn float_window(&mut self, window: WindowId, rect: Rect) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        Ok(self.run(|state| {
            let Some(workspace) = state.workspace_of(window) else {
                return Vec::new();
            };
            // A floating window's rectangle is kept in the workspace's own
            // coordinates, so that a workspace moved to another monitor
            // takes its windows with it.
            let (x, y) = state.origin(workspace);
            state.hold_floating(
                window,
                rect.translate(x.saturating_neg(), y.saturating_neg()),
            );
            if state.is_floating(window) {
                return Vec::new();
            }
            if state.group_of(window).is_some() {
                let was = state.focused_window();
                state.focus(window);
                let _ = state.move_out_of_group();
                if let Some(was) = was {
                    state.focus(was);
                }
            }
            let mut changes = Vec::new();
            if let Some(ws) = state.workspaces.get_mut(&workspace) {
                if ws.fullscreen.is_some_and(|(id, _)| id == window) {
                    ws.fullscreen = None;
                    changes.push(Change::Fullscreen { window, mode: None });
                }
                ws.tiling.remove(window);
                ws.floating.push(window);
            }
            changes.push(Change::Floating {
                window,
                floating: true,
            });
            changes
        }))
    }

    /// Every window, most recently focused first.
    ///
    /// The focus history, which is what a rule that gives the focus back
    /// needs and what `hyprctl clients` numbers its `focusHistoryID` from.
    #[must_use]
    pub fn windows_in_focus_order(&self) -> Vec<WindowId> {
        let mut order: Vec<WindowId> = self.history.iter().rev().copied().collect();
        order.dedup();
        order
    }

    fn open(&mut self, window: WindowId, floating: Option<Rect>) -> Result<Vec<Change>, Error> {
        if self.windows.contains_key(&window) {
            return Err(Error::DuplicateWindow(window));
        }
        let Some(workspace) = self.current_workspace() else {
            return Err(Error::NoMonitor);
        };
        Ok(self.run(|state| {
            if let Some(rect) = floating {
                let (x, y) = state.origin(workspace);
                state.hold_floating(
                    window,
                    rect.translate(x.saturating_neg(), y.saturating_neg()),
                );
            }
            state.attach(window, workspace, floating.is_some());
            match state.fullscreen(workspace) {
                Some((fullscreen, _)) => {
                    let at = state
                        .history
                        .iter()
                        .position(|id| *id == fullscreen)
                        .unwrap_or(state.history.len());
                    state.history.insert(at, window);
                }
                None => state.history.push(window),
            }
            Vec::new()
        }))
    }

    /// A window is gone: its client closed it or died. Focus falls back to
    /// the window focused before it.
    pub fn window_gone(&mut self, window: WindowId) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        Ok(self.run(|state| {
            let was_on = state.windows.get(&window).copied();
            let was_focused = state.focused_window() == Some(window);
            state.ungroup(window);
            state.detach(window);
            state.history.retain(|id| *id != window);
            let _rect = state.floating_rects.remove(&window);
            let _lifted = state.lifted.remove(&window);
            // `master:focus_master_on_close`: the focus goes to the master
            // rather than to whatever the history has next, which is what
            // `getNextCandidate` does with the option on.
            if was_focused
                && state.settings.master.focus_master_on_close
                && let Some(workspace) = was_on
                && let Some(master) = state
                    .workspaces
                    .get(&workspace)
                    .and_then(|ws| ws.tiling.master_window())
            {
                state.focus(master);
            }
            // `misc:close_special_on_empty`: a scratchpad whose last window
            // has gone stops being shown, rather than leaving an empty
            // overlay over the screen for a person to dismiss by hand.
            if state.settings.close_special_on_empty
                && let Some(workspace) = was_on
                && Self::is_special(workspace)
                && state.windows(workspace).is_empty()
            {
                state
                    .outputs
                    .iter_mut()
                    .filter(|output| output.special == Some(workspace))
                    .for_each(|output| output.special = None);
            }
            Vec::new()
        }))
    }

    /// Focus a window, as a click or `focuswindow` does: its monitor gets
    /// focus and shows its workspace, and a floating window is raised.
    pub fn focus_window(&mut self, window: WindowId) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        Ok(self.run(|state| {
            state.focus(window);
            Vec::new()
        }))
    }

    /// Change the options. A different `general:layout` lays every
    /// workspace's tiled windows out again in the new layout, in their old
    /// order.
    pub fn set_settings(&mut self, settings: Settings) -> Vec<Change> {
        self.run(|state| {
            let old = state.settings.layout;
            state.settings = settings;
            if old != settings.layout {
                state.retile();
            }
            Vec::new()
        })
    }

    /// Lay every workspace's tiled windows out again in the configured
    /// layout, in their old order.
    fn retile(&mut self) {
        let outputs = &self.outputs;
        for ws in self.workspaces.values_mut() {
            let area = Area::of(work_area(outputs, ws.monitor, &self.settings));
            let windows = ws.tiling.windows();
            ws.tiling = Tiling::new(self.settings.layout);
            let mut previous = None;
            for window in windows {
                // No cursor: this is the same windows in the same order,
                // laid out again because an option changed, and where the
                // pointer happens to be is not part of that.
                ws.tiling
                    .insert(window, previous, None, area, &self.settings);
                previous = Some(window);
            }
        }
    }

    // -- Dispatchers ----------------------------------------------------------

    /// Run a dispatcher.
    pub fn dispatch(&mut self, dispatcher: &Dispatcher) -> Vec<Change> {
        let dispatcher = dispatcher.clone();
        self.run(|state| match dispatcher {
            Dispatcher::MoveFocus(direction) => state.move_focus(direction),
            Dispatcher::MoveWindow(direction) => state.move_window(direction),
            Dispatcher::Workspace(target) => state.switch_workspace(target),
            Dispatcher::MoveToWorkspace(target) => state.move_to_workspace(target, true),
            Dispatcher::MoveToWorkspaceSilent(target) => state.move_to_workspace(target, false),
            Dispatcher::KillActive => state
                .focused_window()
                .map(Change::Close)
                .into_iter()
                .collect(),
            Dispatcher::SetFloating => state.set_floating(true),
            Dispatcher::SetTiled => state.set_floating(false),
            Dispatcher::CenterWindow { whole } => state.center_window(whole),
            Dispatcher::Pin => state.pin(),
            Dispatcher::Pseudo => state.toggle_pseudo(),
            Dispatcher::ResizeActive(by) => state.resize_active(&by),
            Dispatcher::MoveActive(by) => state.move_active(&by),
            Dispatcher::SwapWindow(direction) => state.swap_window(&direction),
            Dispatcher::SwapNext { back } => state.swap_next(&back),
            Dispatcher::CycleNext { back, tiled_only } => state.cycle_next(&back, &tiled_only),
            Dispatcher::BringActiveToTop => state.alter_z_order(&true, None),
            Dispatcher::AlterZOrder { top } => state.alter_z_order(&top, None),
            Dispatcher::FocusWindow(_) | Dispatcher::CloseWindow(_) => {
                // Both name a window with a `windowrule`-shaped expression,
                // which is matched on a window's title and class -- and the
                // layout holds neither. The compositor answers these, and
                // reaches back in with `focus_window` and `Change::Close`.
                Vec::new()
            }
            Dispatcher::FocusCurrentOrLast => state.focus_current_or_last(),
            Dispatcher::FullscreenState { internal, client } => {
                state.fullscreen_state(&internal, &client)
            }
            Dispatcher::RenameWorkspace { id, name } => state.rename_workspace(&id, &name),
            Dispatcher::WorkspaceOpt(option) => state.workspace_option(&option),
            Dispatcher::MoveGroupWindow { back } => state.move_group_window(&back),
            Dispatcher::LockActiveGroup(locking) => state.lock_active_group(&locking),
            Dispatcher::DenyWindowFromGroup(locking) => {
                state.deny_from_group = match locking {
                    Locking::Lock => true,
                    Locking::Unlock => false,
                    Locking::Toggle => !state.deny_from_group,
                };
                Vec::new()
            }
            Dispatcher::TagWindow(tag) => state.tag_window(&tag),
            Dispatcher::FocusWorkspaceOnCurrentMonitor(target) => {
                state.focus_workspace_here(target)
            }
            Dispatcher::MoveIntoOrCreateGroup(direction) => {
                state.move_into_or_create_group(direction)
            }
            Dispatcher::MoveWindowOrGroup(direction) => state.move_window_or_group(direction),
            // Hyprland's own is an empty function: the group lock is
            // ignored per bind now, not per session.
            Dispatcher::SetIgnoreGroupLock => Vec::new(),
            Dispatcher::LayoutMessage(message) => state.layout_message(&message),
            // Both name a window with an expression the layout cannot
            // read; the compositor picks it out and reaches back in with
            // `move_window_pixel` and `resize_window_pixel`.
            Dispatcher::MoveWindowPixel { .. } | Dispatcher::ResizeWindowPixel { .. } => Vec::new(),
            Dispatcher::ToggleFloating => state.toggle_floating(),
            Dispatcher::Fullscreen(mode) => state.toggle_fullscreen(mode),
            Dispatcher::ToggleSpecialWorkspace(name) => state.toggle_special(&name),
            Dispatcher::ToggleGroup => state.toggle_group(),
            Dispatcher::ChangeGroupActive(which) => state.change_group_active(which),
            Dispatcher::MoveIntoGroup(direction) => state.move_into_group(direction),
            Dispatcher::MoveOutOfGroup => state.move_out_of_group(),
            Dispatcher::FocusMonitor(target) => state.focus_monitor(&target),
            Dispatcher::MoveWindowToMonitor { monitor, silent } => {
                state.move_window_to_monitor(&monitor, silent)
            }
            Dispatcher::MoveCurrentWorkspaceToMonitor(target) => {
                state.move_current_workspace_to_monitor(&target)
            }
            Dispatcher::MoveWorkspaceToMonitor { workspace, monitor } => {
                state.move_workspace_to_monitor(workspace, &monitor)
            }
            Dispatcher::SwapActiveWorkspaces { one, other } => {
                state.swap_active_workspaces(&one, &other)
            }
            Dispatcher::LockGroups(lock) => {
                state.groups_locked = match lock {
                    Locking::Lock => true,
                    Locking::Unlock => false,
                    Locking::Toggle => !state.groups_locked,
                };
                Vec::new()
            }
        })
    }

    /// Parse and run a dispatcher, as `hyprctl dispatch` does.
    pub fn dispatch_str(&mut self, name: &str, arg: &str) -> Result<Vec<Change>, Error> {
        let dispatcher = Dispatcher::parse(name, arg)?;
        Ok(self.dispatch(&dispatcher))
    }

    /// Run the dispatcher a binding names.
    pub fn dispatch_bind(&mut self, bind: &Bind) -> Result<Vec<Change>, Error> {
        self.dispatch_str(&bind.dispatcher, &bind.arg)
    }

    /// Hyprland's `moveFocusTo`.
    fn move_focus(&mut self, direction: Direction) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            if let Some(monitor) = self.monitor_towards(direction) {
                self.focused_monitor = Some(monitor);
            }
            return Vec::new();
        };
        // `binds:movefocus_cycles_fullscreen`: there is nothing beside a
        // window that covers the screen, so a direction key on one does
        // nothing unless it is read as "the next window" instead.
        let fullscreen = self
            .workspace_of(window)
            .and_then(|workspace| self.fullscreen(workspace))
            .is_some_and(|(id, _)| id == window);
        if self.settings.movefocus_cycles_fullscreen && fullscreen {
            let back = matches!(direction, Direction::Up | Direction::Left);
            return self.cycle_next(&back, &false);
        }
        // The scrolling layout's directions are the tape's, not the
        // screen's: a column off the side of the screen has no rectangle
        // for a geometric search to find, so the tape is asked instead.
        if matches!(self.settings.layout, Layout::Scrolling) {
            let (right, along) = match direction {
                Direction::Left => (false, true),
                Direction::Right => (true, true),
                Direction::Up => (false, false),
                Direction::Down => (true, false),
            };
            let along_tape = self
                .workspace_of(window)
                .and_then(|workspace| self.workspaces.get(&workspace))
                .and_then(|ws| match &ws.tiling {
                    Tiling::Scrolling(scrolling) => {
                        scrolling.beside(window, right, along, &self.settings)
                    }
                    _ => None,
                });
            if let Some(next) = along_tape {
                let settings = self.settings;
                self.focus(next);
                let across = self
                    .workspace_of(next)
                    .map_or(0.0, |workspace| self.work_area_of(workspace).width as f64);
                if let Some(tape) = self
                    .workspace_of(next)
                    .and_then(|workspace| self.workspaces.get_mut(&workspace))
                    .and_then(|ws| ws.tiling.scrolling())
                {
                    tape.follow(next, &settings, across);
                }
                return Vec::new();
            }
        }
        if let Some(target) = self.window_in_direction(window, direction) {
            self.focus(target);
        } else if self.settings.window_direction_monitor_fallback
            && let Some(monitor) = self.monitor_towards(direction)
        {
            self.focused_monitor = Some(monitor);
        } else if !self.settings.no_focus_fallback
            && let Some(target) = self.wrapped(window, direction)
        {
            self.focus(target);
        }
        Vec::new()
    }

    /// Hyprland's `moveActiveTo`, for a tiled window.
    fn move_window(&mut self, direction: Direction) -> Vec<Change> {
        let Some(focused) = self.focused_window() else {
            return Vec::new();
        };
        // A grouped window moves with its group: the head is what the
        // tiling can move, and the focus stays where it was.
        let window = self.in_tiling(focused);
        let Some(from) = self.workspace_of(window) else {
            return Vec::new();
        };
        if self.is_floating(window) || self.fullscreen(from).is_some() {
            return Vec::new();
        }
        if let Some(other) = self.window_in_direction(focused, direction) {
            return self.move_past(window, other, direction);
        }
        let Some(to) = self
            .monitor_towards(direction)
            .and_then(|monitor| self.active_workspace(monitor))
        else {
            return Vec::new();
        };
        self.move_window_to(window, to);
        self.focus(focused);
        vec![Change::MoveToWorkspace {
            window,
            workspace: to,
        }]
    }

    /// Move the tiled `window` in `direction`, where the neighbour search
    /// found `other`: the layouts' `moveWindowTo`, `moveTargetInDirection`
    /// from Hyprland 0.54.
    ///
    /// The master layout exchanges the two on one workspace and sends the
    /// window to the other's workspace across two. The dwindle layout takes
    /// the window out and puts it back in at a point one pixel past its
    /// edge, on the active workspace of whichever monitor that point is on.
    fn move_past(
        &mut self,
        window: WindowId,
        other: WindowId,
        direction: Direction,
    ) -> Vec<Change> {
        // The search found what is drawn; the tiling holds the group's head.
        let (window, other) = (self.in_tiling(window), self.in_tiling(other));
        let (Some(from), Some(beyond)) = (self.workspace_of(window), self.workspace_of(other))
        else {
            return Vec::new();
        };
        let lone_sibling = match self.workspaces.get(&from).map(|ws| &ws.tiling) {
            Some(Tiling::Dwindle(dwindle)) => dwindle.faces_lone_sibling(window, direction),
            _ => {
                if self.fullscreen(beyond).is_some() {
                    return Vec::new();
                }
                if from == beyond {
                    if let Some(ws) = self.workspaces.get_mut(&from) {
                        ws.tiling.swap(window, other);
                    }
                    return Vec::new();
                }
                self.move_window_to(window, beyond);
                self.focus(window);
                return vec![Change::MoveToWorkspace {
                    window,
                    workspace: beyond,
                }];
            }
        };
        let Some(ideal) = self
            .candidates()
            .into_iter()
            .find(|candidate| candidate.window == self.shown(window))
            .map(|candidate| candidate.ideal)
        else {
            return Vec::new();
        };
        let point = focal_point(ideal, direction);
        let Some(to) = self
            .monitor_at(point)
            .and_then(|monitor| self.active_workspace(monitor))
        else {
            return Vec::new();
        };
        if self.fullscreen(to).is_some() {
            return Vec::new();
        }
        let toward = (from == to && lone_sibling).then_some(direction);
        let area = Area::of(self.work_area_of(to));
        self.detach(window);
        let settings = self.settings;
        if let Some(ws) = self.workspaces.get_mut(&to) {
            ws.tiling.insert_at(window, point, toward, area, &settings);
        }
        let _previous = self.windows.insert(window, to);
        self.focus(window);
        if from == to {
            Vec::new()
        } else {
            vec![Change::MoveToWorkspace {
                window,
                workspace: to,
            }]
        }
    }

    fn switch_workspace(&mut self, target: WorkspaceTarget) -> Vec<Change> {
        if let Some(workspace) = self.resolve(target) {
            self.show(workspace);
        }
        Vec::new()
    }

    fn move_to_workspace(&mut self, target: WorkspaceTarget, follow: bool) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some(to) = self.resolve(target) else {
            return Vec::new();
        };
        if self.workspace_of(window) == Some(to) {
            return Vec::new();
        }
        if let Some(monitor) = self.focused_monitor {
            self.ensure_workspace(to, monitor);
            // A window sent to a scratchpad goes to a workspace that is
            // shown over this one, not to one this monitor switches to.
            if Self::is_special(to)
                && follow
                && let Some(output) = self
                    .outputs
                    .iter_mut()
                    .find(|output| output.monitor.id == monitor)
            {
                output.special = Some(to);
            }
        }
        self.move_window_to(window, to);
        if follow {
            self.focus(window);
        }
        vec![Change::MoveToWorkspace {
            window,
            workspace: to,
        }]
    }

    /// `setfloating` and `settiled`: float or tile the focused window,
    /// whether or not it already is.
    ///
    /// `togglefloating` is the one that turns it over; these two are what a
    /// configuration binds when it wants a key that always does the same
    /// thing.
    fn set_floating(&mut self, floating: bool) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        if self.is_floating(window) == floating {
            return Vec::new();
        }
        self.toggle_floating()
    }

    /// `centerwindow`: put a floating window in the middle of its monitor.
    ///
    /// A tiled window is where the tiling put it and this does nothing to
    /// it, which is what Hyprland's does. `whole` centres it on the whole
    /// monitor rather than on what the bars left.
    fn center_window(&mut self, whole: bool) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        if !self.is_floating(window) {
            return Vec::new();
        }
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let area = if whole {
            self.workspace_monitor(workspace)
                .and_then(|monitor| self.output(monitor))
                .map(|output| output.monitor.rect)
                .unwrap_or_default()
        } else {
            self.float_work_area_of(workspace)
        };
        let Some(rect) = self.floating_rects.get(&window).copied() else {
            return Vec::new();
        };
        let at = Rect::new(
            area.x
                .saturating_add(area.width.saturating_sub(rect.width) / 2),
            area.y
                .saturating_add(area.height.saturating_sub(rect.height) / 2),
            rect.width,
            rect.height,
        );
        self.place_floating(window, at)
    }

    /// Put a floating window at `rect`, in the space every window's
    /// rectangle is in.
    fn place_floating(&mut self, window: WindowId, rect: Rect) -> Vec<Change> {
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let (x, y) = self.origin(workspace);
        self.hold_floating(
            window,
            rect.translate(x.saturating_neg(), y.saturating_neg()),
        );
        self.workspace_monitor(workspace)
            .map(Change::Layout)
            .into_iter()
            .collect()
    }

    /// `pin`: keep a floating window on every workspace of its monitor.
    ///
    /// A tiled window cannot be pinned -- it has a slot on one workspace and
    /// nowhere else -- which is what Hyprland's refuses too.
    fn pin(&mut self) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        // Hyprland pins a floating window; `binds:allow_pin_fullscreen`
        // lets a fullscreen one be pinned as well, because a fullscreen
        // window is already drawn over everything and a person who asked
        // for it to follow them meant it.
        if !self.is_floating(window)
            && !(self.settings.allow_pin_fullscreen
                && self
                    .workspace_of(window)
                    .and_then(|workspace| self.fullscreen(workspace))
                    .is_some_and(|(id, _)| id == window))
        {
            return Vec::new();
        }
        if !self.pinned.insert(window) {
            let _ = self.pinned.remove(&window);
        }
        vec![Change::Pinned {
            window,
            pinned: self.pinned.contains(&window),
        }]
    }

    /// `pseudo`: draw a tiled window at the size it asked for, in the middle
    /// of the slot the tiling gave it.
    fn toggle_pseudo(&mut self) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        if !self.pseudo.insert(window) {
            let _ = self.pseudo.remove(&window);
        }
        vec![Change::Pseudo {
            window,
            pseudo: self.pseudo.contains(&window),
        }]
    }

    /// `resizeactive`: make the focused window larger or smaller.
    ///
    /// A floating window is resized where it is; a tiled one has no
    /// rectangle of its own, so its size is the tiling's to decide and the
    /// tiling is asked instead -- which for the dwindle layout is moving
    /// the split it sits under, as Hyprland's does.
    fn resize_active(&mut self, by: &Move) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        self.move_floating(window, by, true, Corner::NONE, false)
    }

    /// `moveactive`: move a floating window, by a distance or to a place.
    fn move_active(&mut self, by: &Move) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        self.move_floating(window, by, false, Corner::NONE, false)
    }

    /// Move or resize one window, which is what all four of `moveactive`,
    /// `resizeactive`, `movewindowpixel` and `resizewindowpixel` come down
    /// to.
    ///
    /// A tiled window cannot be *moved* -- where it is belongs to the
    /// tiling, and Hyprland's own dispatchers do nothing to one either --
    /// but it can be resized, by moving whatever decides its size.
    fn move_floating(
        &mut self,
        window: WindowId,
        by: &Move,
        resizing: bool,
        corner: Corner,
        snapping: bool,
    ) -> Vec<Change> {
        if !self.is_floating(window) {
            // A tiled window has no rectangle of its own to move, but its
            // size is still something the tiling can be asked for.
            return if resizing {
                self.resize_tiled(window, by, corner)
            } else {
                Vec::new()
            };
        }
        let Some(rect) = self.rect_of(window) else {
            return Vec::new();
        };
        let at = |now: i64, change: i64| {
            if by.exact {
                change
            } else {
                now.saturating_add(change)
            }
        };
        let rect = if resizing {
            // Which edge moves is the corner's to say, as Hyprland's drag
            // does it: the right edge takes the distance and the left edge
            // takes it the other way round, moving the window's origin by
            // as much so that the edge nobody grabbed stays where it is. A
            // dispatcher grabs nothing and the origin is left alone, which
            // is the corner-less resize this has always done.
            let width = at(rect.width, if corner.left { -by.x } else { by.x }).max(1);
            let height = at(rect.height, if corner.top { -by.y } else { by.y }).max(1);
            // A window is never resized out of existence, and an edge that
            // ran into that limit stops rather than carrying the origin on.
            Rect::new(
                rect.x + if corner.left { rect.width - width } else { 0 },
                rect.y + if corner.top { rect.height - height } else { 0 },
                width,
                height,
            )
        } else {
            Rect::new(at(rect.x, by.x), at(rect.y, by.y), rect.width, rect.height)
        };
        let rect = if snapping {
            self.snapped(window, rect, !resizing, corner)
        } else {
            rect
        };
        self.place_floating(window, rect)
    }

    /// `general:snap:*`: a dragged floating window that came near another
    /// window's edge, or the screen's, lands flush against it.
    ///
    /// Only a drag, which is why this is not in `move_floating` itself:
    /// Hyprland's drag controller calls `performSnap` and its dispatchers do
    /// not, so `moveactive 10 0` moves by ten pixels and means it.
    fn snapped(&self, window: WindowId, rect: Rect, moving: bool, corner: Corner) -> Rect {
        let Some(workspace) = self.workspace_of(window) else {
            return rect;
        };
        let Some(monitor) = self
            .workspace_monitor(workspace)
            .and_then(|id| self.monitors().find(|monitor| monitor.id == id))
        else {
            return rect;
        };
        let others: Vec<Rect> = self
            .layout()
            .iter()
            .flat_map(|output| output.windows.iter())
            .filter(|placed| placed.window != window)
            .map(|placed| placed.rect)
            .collect();
        crate::snap::perform(
            rect,
            moving,
            corner,
            &others,
            monitor.rect,
            monitor.reserved,
            &self.settings,
        )
    }

    /// Resize a tiled window by asking its tiling to give it more or less
    /// room, which the dwindle layout does by moving the splits above it.
    ///
    /// `exact` is a size rather than a distance, and the tiling is told the
    /// difference from the size the window has now: a tiling holds
    /// proportions, so "600 wide" only means anything beside what it is.
    fn resize_tiled(&mut self, window: WindowId, by: &Move, corner: Corner) -> Vec<Change> {
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let Some(rect) = self.rect_of(window) else {
            return Vec::new();
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a screen's pixels are far inside f64's exact integers"
        )]
        let by = if by.exact {
            ((by.x - rect.width) as f64, (by.y - rect.height) as f64)
        } else {
            (by.x as f64, by.y as f64)
        };
        let area = Area::of(self.work_area_of(workspace));
        let settings = self.settings;
        let moved = self
            .workspaces
            .get_mut(&workspace)
            .is_some_and(|ws| ws.tiling.resize(window, by, corner, area, &settings));
        if !moved {
            return Vec::new();
        }
        self.workspace_monitor(workspace)
            .map(Change::Layout)
            .into_iter()
            .collect()
    }

    /// Where a window is now: its floating rectangle, or the slot the
    /// tiling gave it.
    fn rect_of(&self, window: WindowId) -> Option<Rect> {
        self.layout()
            .iter()
            .flat_map(|output| output.windows.iter())
            .find(|placed| placed.window == window)
            .map(|placed| placed.rect)
    }

    /// `swapwindow`: exchange the focused window with its neighbour.
    ///
    /// The focus follows the window that moved, which is what Hyprland's
    /// does and what makes a run of them walk a window across the screen.
    fn swap_window(&mut self, direction: &Direction) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some(other) = self.window_in_direction(window, *direction) else {
            return Vec::new();
        };
        self.swap(window, other)
    }

    /// `swapnext`: exchange the focused window with the next in the tiling.
    fn swap_next(&mut self, back: &bool) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some(other) = self.along(window, *back, false) else {
            return Vec::new();
        };
        self.swap(window, other)
    }

    /// `cyclenext`: focus the next window on the workspace.
    fn cycle_next(&mut self, back: &bool, tiled_only: &bool) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some(other) = self.along(window, *back, *tiled_only) else {
            return Vec::new();
        };
        self.focus(other);
        vec![Change::Focus(Some(other))]
    }

    /// The window before or after `window` on its workspace, wrapping.
    ///
    /// The order is the one the layout draws in -- tiled windows then
    /// floating ones -- which is the order Hyprland's `cyclenext` walks.
    fn along(&self, window: WindowId, back: bool, tiled_only: bool) -> Option<WindowId> {
        let workspace = self.workspace_of(window)?;
        let held = self.workspaces.get(&workspace)?;
        let mut order: Vec<WindowId> = held.tiling.windows();
        if !tiled_only {
            order.extend(held.floating.iter().copied());
        }
        if order.len() < 2 {
            return None;
        }
        let at = order.iter().position(|held| *held == window)?;
        let next = if back {
            at.checked_sub(1).unwrap_or(order.len() - 1)
        } else {
            (at + 1) % order.len()
        };
        order.get(next).copied()
    }

    /// Exchange two windows' places, leaving the focus on the first.
    fn swap(&mut self, window: WindowId, other: WindowId) -> Vec<Change> {
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        if self.workspace_of(other) != Some(workspace) {
            return Vec::new();
        }
        let floating = (self.is_floating(window), self.is_floating(other));
        match floating {
            // Two tiled windows exchange their places in the tree.
            (false, false) => {
                if let Some(held) = self.workspaces.get_mut(&workspace) {
                    held.tiling.swap(window, other);
                }
            }
            // Two floating windows exchange their rectangles.
            (true, true) => {
                let (one, two) = (
                    self.floating_rects.get(&window).copied(),
                    self.floating_rects.get(&other).copied(),
                );
                if let (Some(one), Some(two)) = (one, two) {
                    self.hold_floating(window, two);
                    self.hold_floating(other, one);
                }
            }
            // One of each is not a swap Hyprland makes either.
            _ => return Vec::new(),
        }
        self.workspace_monitor(workspace)
            .map(Change::Layout)
            .into_iter()
            .collect()
    }

    /// `bringactivetotop` and `alterzorder`: where a floating window sits
    /// in the stack.
    fn alter_z_order(&mut self, top: &bool, which: Option<WindowId>) -> Vec<Change> {
        let Some(window) = which.or_else(|| self.focused_window()) else {
            return Vec::new();
        };
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let Some(held) = self.workspaces.get_mut(&workspace) else {
            return Vec::new();
        };
        let Some(at) = held.floating.iter().position(|held| *held == window) else {
            return Vec::new();
        };
        let _ = held.floating.remove(at);
        if *top {
            held.floating.push(window);
        } else {
            held.floating.insert(0, window);
        }
        self.workspace_monitor(workspace)
            .map(Change::Layout)
            .into_iter()
            .collect()
    }

    /// `focuscurrentorlast`: swap between the focused window and the one
    /// before it.
    fn focus_current_or_last(&mut self) -> Vec<Change> {
        let order = self.windows_in_focus_order();
        let Some(previous) = order.get(1).copied() else {
            return Vec::new();
        };
        self.focus(previous);
        vec![Change::Focus(Some(previous))]
    }

    /// `fullscreenstate`: the compositor's fullscreen and the client's, set
    /// separately.
    ///
    /// -1 leaves one alone. This layout keeps one state rather than two --
    /// what the compositor does *is* what the client is told -- so the
    /// internal one decides and the client's is read and reported.
    fn fullscreen_state(&mut self, internal: &i64, _client: &i64) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let now = self.fullscreen(workspace).map(|(_, mode)| mode);
        let wanted = match internal {
            -1 => return Vec::new(),
            0 => None,
            1 => Some(FullscreenMode::Maximized),
            _ => Some(FullscreenMode::Fullscreen),
        };
        if now == wanted {
            return Vec::new();
        }
        // The one path both go through, so that a state set here and one
        // toggled by `fullscreen` cannot drift.
        match wanted {
            None => self.dispatch(&Dispatcher::Fullscreen(
                now.unwrap_or(FullscreenMode::Fullscreen),
            )),
            Some(mode) => {
                let mut changes = Vec::new();
                if now.is_some() {
                    changes.extend(self.dispatch(&Dispatcher::Fullscreen(
                        now.unwrap_or(FullscreenMode::Fullscreen),
                    )));
                }
                changes.extend(self.dispatch(&Dispatcher::Fullscreen(mode)));
                changes
            }
        }
    }

    /// `renameworkspace`: give a workspace a name, or take one away.
    fn rename_workspace(&mut self, id: &i64, name: &str) -> Vec<Change> {
        let workspace = WorkspaceId(*id);
        if !self.workspaces.contains_key(&workspace) {
            return Vec::new();
        }
        if name.is_empty() {
            let _ = self.names.remove(&workspace);
        } else {
            let _ = self.names.insert(workspace, name.to_owned());
        }
        vec![Change::Renamed {
            workspace,
            name: name.to_owned(),
        }]
    }

    /// `workspaceopt`: float or pseudotile every window on the focused
    /// workspace.
    fn workspace_option(&mut self, option: &WorkspaceOption) -> Vec<Change> {
        let Some(workspace) = self
            .focused_monitor()
            .and_then(|monitor| self.active_workspace(monitor))
        else {
            return Vec::new();
        };
        let windows = self.windows_on(workspace);
        let was = self.focused_window();
        let mut changes = Vec::new();
        for window in windows {
            match option {
                WorkspaceOption::AllFloat => {
                    if !self.is_floating(window) {
                        self.focus(window);
                        changes.extend(self.toggle_floating());
                    }
                }
                WorkspaceOption::AllPseudo => {
                    let _ = self.pseudo.insert(window);
                    changes.push(Change::Pseudo {
                        window,
                        pseudo: true,
                    });
                }
            }
        }
        if let Some(was) = was {
            self.focus(was);
        }
        changes
    }

    /// Every window on a workspace, tiled then floating.
    fn windows_on(&self, workspace: WorkspaceId) -> Vec<WindowId> {
        let Some(held) = self.workspaces.get(&workspace) else {
            return Vec::new();
        };
        let mut windows = held.tiling.windows();
        windows.extend(held.floating.iter().copied());
        windows
    }

    /// `movegroupwindow`: move the focused window inside its group.
    fn move_group_window(&mut self, back: &bool) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some((head, _)) = self.group_of(window) else {
            return Vec::new();
        };
        let Some(group) = self.groups.get_mut(&head) else {
            return Vec::new();
        };
        let Some(at) = group.members.iter().position(|held| *held == window) else {
            return Vec::new();
        };
        let to = if *back {
            at.checked_sub(1)
                .unwrap_or(group.members.len().saturating_sub(1))
        } else {
            (at + 1) % group.members.len()
        };
        // The head is the member the tiling tree holds, and it stays the
        // head: Hyprland's `movegroupwindow` changes the tab order and not
        // which slot the group is in.
        if at != 0 && to != 0 {
            group.members.swap(at, to);
        }
        self.workspace_of(window)
            .and_then(|workspace| self.workspace_monitor(workspace))
            .map(Change::Layout)
            .into_iter()
            .collect()
    }

    /// `lockactivegroup`: whether the focused window's group takes more
    /// windows.
    ///
    /// Recorded and reported, as `lockgroups` is: nothing here adds a window
    /// to a group on its own, and `moveintogroup` is a person asking.
    fn lock_active_group(&mut self, locking: &Locking) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some((head, _)) = self.group_of(window) else {
            return Vec::new();
        };
        if let Some(group) = self.groups.get_mut(&head) {
            group.locked = match locking {
                Locking::Lock => true,
                Locking::Unlock => false,
                Locking::Toggle => !group.locked,
            };
        }
        Vec::new()
    }

    /// `tagwindow`: add a tag, take one away with `-`, or turn one over
    /// with `+`, as Hyprland's own reads them.
    fn tag_window(&mut self, tag: &str) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let (tag, add) = match tag.trim().strip_prefix('-') {
            Some(rest) => (rest.trim(), Some(false)),
            None => match tag.trim().strip_prefix('+') {
                Some(rest) => (rest.trim(), Some(true)),
                None => (tag.trim(), None),
            },
        };
        if tag.is_empty() {
            return Vec::new();
        }
        let held = self.tags.entry(window).or_default();
        let there = held.iter().any(|name| name == tag);
        let wanted = add.unwrap_or(!there);
        if wanted && !there {
            held.push(tag.to_owned());
        } else if !wanted {
            held.retain(|name| name != tag);
        }
        Vec::new()
    }

    /// The tags a window carries, which a `windowrule` can match on.
    #[must_use]
    pub fn tags_of(&self, window: WindowId) -> &[String] {
        self.tags.get(&window).map_or(&[], Vec::as_slice)
    }

    /// Whether a window is pinned, drawn pseudotiled, or both.
    #[must_use]
    pub fn is_pinned(&self, window: WindowId) -> bool {
        self.pinned.contains(&window)
    }

    /// The same for `pseudo`.
    #[must_use]
    pub fn is_pseudo(&self, window: WindowId) -> bool {
        self.pseudo.contains(&window)
    }

    fn toggle_floating(&mut self) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        // A floating window has no slot to share, so one that floats leaves
        // its group on the way out, as Hyprland's `setWindowFullscreen`
        // path and its group code both do.
        if self.group_of(window).is_some() {
            let _ = self.move_out_of_group();
        }
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let floating = self.is_floating(window);
        let monitor_rect = self
            .workspace_monitor(workspace)
            .and_then(|monitor| self.output(monitor))
            .map(|output| output.monitor.rect)
            .unwrap_or_default();
        let area = Area::of(self.work_area_of(workspace));
        let beside = self.recent_tiled(workspace);
        let cursor = self.pointer;
        if !floating && !self.floating_rects.contains_key(&window) {
            // A window floated for the first time is centred at half the
            // monitor's size.
            let width = monitor_rect.width / 2;
            let height = monitor_rect.height / 2;
            let rect = Rect::new(
                monitor_rect.width.saturating_sub(width) / 2,
                monitor_rect.height.saturating_sub(height) / 2,
                width,
                height,
            );
            self.hold_floating(window, rect);
        }
        let Some(ws) = self.workspaces.get_mut(&workspace) else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        if ws.fullscreen.is_some_and(|(id, _)| id == window) {
            ws.fullscreen = None;
            changes.push(Change::Fullscreen { window, mode: None });
        }
        if floating {
            ws.floating.retain(|id| *id != window);
            ws.tiling
                .insert(window, beside, cursor, area, &self.settings);
        } else {
            ws.tiling.remove(window);
            ws.floating.push(window);
        }
        changes.push(Change::Floating {
            window,
            floating: !floating,
        });
        changes
    }

    fn toggle_fullscreen(&mut self, mode: FullscreenMode) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some(ws) = self
            .workspace_of(window)
            .and_then(|workspace| self.workspaces.get_mut(&workspace))
        else {
            return Vec::new();
        };
        let old = ws.fullscreen;
        ws.fullscreen = if old == Some((window, mode)) {
            None
        } else {
            Some((window, mode))
        };
        let mut changes = Vec::new();
        // One fullscreen window to a workspace: another one stops being.
        if let Some((previous, _)) = old.filter(|(id, _)| *id != window) {
            changes.push(Change::Fullscreen {
                window: previous,
                mode: None,
            });
        }
        changes.push(Change::Fullscreen {
            window,
            mode: ws.fullscreen.map(|(_, mode)| mode),
        });
        changes
    }

    // -- Neighbours -----------------------------------------------------------

    /// The windows the direction searches look at: those on the workspaces
    /// the monitors show, less the ones a fullscreen window hides, in
    /// monitor order, tiled before floating.
    fn candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for output in &self.outputs {
            let Some(ws) = self.workspaces.get(&output.active) else {
                continue;
            };
            let monitor = &output.monitor;
            if let Some((window, _)) = ws.fullscreen {
                out.push(Candidate {
                    window,
                    workspace: output.active,
                    placed: monitor.rect,
                    ideal: monitor.rect,
                    floating: ws.floating.contains(&window),
                    fullscreen: true,
                });
                continue;
            }
            let work = geometry::work_area(monitor, &self.settings);
            let tiled = ws
                .tiling
                .slots(Area::of(work), &self.settings, self.focused_window())
                .into_iter()
                // A group's slot draws its active member, and that is the
                // window a direction search has to find: it is the one with
                // the box, and the one the focus goes to.
                .map(|(window, slot)| (self.shown(window), slot.round(), false));
            let floating = ws.floating.iter().filter_map(|&window| {
                self.floating_rects
                    .get(&window)
                    .map(|rect| (window, rect.translate(monitor.rect.x, monitor.rect.y), true))
            });
            out.extend(
                tiled
                    .chain(floating)
                    .map(|(window, placed, floating)| Candidate {
                        window,
                        workspace: output.active,
                        placed,
                        ideal: ideal_box(placed, monitor.rect, work),
                        floating,
                        fullscreen: false,
                    }),
            );
        }
        out
    }

    /// Hyprland's `CCompositor::getWindowInDirection` from a window: the
    /// search from its box, by edges for a tiled window and by angles for a
    /// floating one.
    fn window_in_direction(&self, window: WindowId, direction: Direction) -> Option<WindowId> {
        let candidates = self.candidates();
        let from = candidates
            .iter()
            .find(|candidate| candidate.window == window)?;
        self.search(
            &candidates,
            from.ideal,
            from.workspace,
            direction,
            window,
            from.floating,
        )
    }

    /// What `movefocus` wraps around to when it finds neither a window nor
    /// a monitor: the search again, from a line one pixel outside the
    /// focused monitor's opposite edge. A window that already spans the
    /// monitor along the direction has nothing to wrap to.
    fn wrapped(&self, window: WindowId, direction: Direction) -> Option<WindowId> {
        let candidates = self.candidates();
        let from = candidates
            .iter()
            .find(|candidate| candidate.window == window)?;
        let output = self.output(self.workspace_monitor(from.workspace)?)?;
        let (monitor, placed) = (output.monitor.rect, from.placed);
        let spans = match direction {
            Direction::Left | Direction::Right => {
                sticks(placed.x, monitor.x) && sticks(placed.width, monitor.width)
            }
            Direction::Up | Direction::Down => {
                sticks(placed.y, monitor.y) && sticks(placed.height, monitor.height)
            }
        };
        if spans {
            return None;
        }
        let line = match direction {
            Direction::Left => Rect::new(monitor.right(), monitor.y, 1, monitor.height),
            Direction::Right => {
                Rect::new(monitor.x.saturating_sub(1), monitor.y, 1, monitor.height)
            }
            Direction::Up => Rect::new(monitor.x, monitor.bottom(), monitor.width, 1),
            Direction::Down => Rect::new(monitor.x, monitor.y.saturating_sub(1), monitor.width, 1),
        };
        self.search(
            &candidates,
            line,
            output.active,
            direction,
            window,
            from.floating,
        )
    }

    /// Hyprland's `getWindowInDirection` from a box on `workspace`, never
    /// finding `ignore`. From a workspace with a fullscreen window only
    /// other fullscreen windows are found.
    ///
    /// By edges: a tiled or fullscreen window whose box's opposite edge
    /// touches this one's and overlaps it along that edge, the most recently
    /// focused of several (`binds:focus_preferred_method` 0).
    ///
    /// By angles: among floating and fullscreen windows whose centre lies
    /// within a right angle of the direction, the nearest of those within
    /// 0.3 pi of it if there are any, else the one at the smallest angle.
    /// As in Hyprland, distances are compared in whole pixels.
    fn search(
        &self,
        candidates: &[Candidate],
        from: Rect,
        workspace: WorkspaceId,
        direction: Direction,
        ignore: WindowId,
        by_angle: bool,
    ) -> Option<WindowId> {
        let fullscreen = self.fullscreen(workspace);
        let eligible = candidates.iter().filter(|candidate| {
            candidate.window != ignore && (fullscreen.is_none() || candidate.fullscreen)
        });
        if !by_angle {
            let mut leader: Option<(usize, WindowId)> = None;
            for candidate in
                eligible.filter(|candidate| !candidate.floating || candidate.fullscreen)
            {
                let to = candidate.ideal;
                let touches = match direction {
                    Direction::Left => sticks(from.x, to.right()),
                    Direction::Right => sticks(from.right(), to.x),
                    Direction::Up => sticks(from.y, to.bottom()),
                    Direction::Down => sticks(from.bottom(), to.y),
                };
                let length = match direction {
                    Direction::Left | Direction::Right => {
                        overlap(from.y, from.bottom(), to.y, to.bottom())
                    }
                    Direction::Up | Direction::Down => {
                        overlap(from.x, from.right(), to.x, to.right())
                    }
                };
                if !touches || length <= 0 {
                    continue;
                }
                // A window never focused is not in the history, and
                // Hyprland's index of -1 for it never leads.
                let Some(recency) = self.history.iter().position(|id| *id == candidate.window)
                else {
                    continue;
                };
                if leader.is_none_or(|(best, _)| recency > best) {
                    leader = Some((recency, candidate.window));
                }
            }
            return leader.map(|(_, window)| window);
        }
        let threshold = 0.3 * PI;
        let (from_x, from_y) = geometry::center(from);
        let (dx, dy) = match direction {
            Direction::Left => (-1.0, 0.0),
            Direction::Right => (1.0, 0.0),
            Direction::Up => (0.0, -1.0),
            Direction::Down => (0.0, 1.0),
        };
        let mut leader: Option<(f64, WindowId)> = None;
        let mut best_angle = 2.0 * PI;
        for candidate in eligible.filter(|candidate| candidate.floating || candidate.fullscreen) {
            let (x, y) = geometry::center(candidate.placed);
            let (vx, vy) = (x - from_x, y - from_y);
            let distance = vx.hypot(vy);
            let angle = ((vx * dx + vy * dy) / distance).clamp(-1.0, 1.0).acos();
            if angle > FRAC_PI_2 {
                continue;
            }
            let nearer = leader.is_some_and(|(nearest, _)| distance < nearest);
            if (best_angle < threshold && nearer && angle < threshold)
                || (angle < best_angle && best_angle > threshold)
                || leader.is_none()
            {
                leader = Some((distance.trunc(), candidate.window));
                best_angle = angle;
            }
        }
        leader
            .map(|(_, window)| window)
            .or_else(|| fullscreen.map(|(window, _)| window))
    }

    /// The monitor whose edge touches the focused monitor's in `direction`,
    /// the one sharing the longest stretch of it if several do. One that
    /// meets it only at a corner counts, as in Hyprland's
    /// `getMonitorInDirection`.
    fn monitor_towards(&self, direction: Direction) -> Option<MonitorId> {
        let from = self.output(self.focused_monitor?)?.monitor.rect;
        let mut best: Option<(i64, MonitorId)> = None;
        for output in &self.outputs {
            let to = output.monitor.rect;
            if Some(output.monitor.id) == self.focused_monitor {
                continue;
            }
            let (touches, length) = match direction {
                Direction::Left => (
                    sticks(from.x, to.right()),
                    overlap(from.y, from.bottom(), to.y, to.bottom()),
                ),
                Direction::Right => (
                    sticks(from.right(), to.x),
                    overlap(from.y, from.bottom(), to.y, to.bottom()),
                ),
                Direction::Up => (
                    sticks(from.y, to.bottom()),
                    overlap(from.x, from.right(), to.x, to.right()),
                ),
                Direction::Down => (
                    sticks(from.bottom(), to.y),
                    overlap(from.x, from.right(), to.x, to.right()),
                ),
            };
            if touches && best.is_none_or(|(longest, _)| length > longest) {
                best = Some((length, output.monitor.id));
            }
        }
        best.map(|(_, monitor)| monitor)
    }

    /// The monitor a point is on, or else the nearest one: Hyprland's
    /// `getMonitorFromVector`.
    fn monitor_at(&self, (x, y): (f64, f64)) -> Option<MonitorId> {
        let mut nearest: Option<(f64, MonitorId)> = None;
        for output in &self.outputs {
            let rect = Area::of(output.monitor.rect);
            if x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h {
                return Some(output.monitor.id);
            }
            let dx = (rect.x - x).max(x - (rect.x + rect.w)).max(0.0);
            let dy = (rect.y - y).max(y - (rect.y + rect.h)).max(0.0);
            let distance = dx * dx + dy * dy;
            if nearest.is_none_or(|(best, _)| distance < best) {
                nearest = Some((distance, output.monitor.id));
            }
        }
        nearest.map(|(_, monitor)| monitor)
    }

    // -- Internals ------------------------------------------------------------

    /// Make a change, then settle the dwindle trees, drop workspaces nobody
    /// needs and work out what changed.
    fn run(&mut self, change: impl FnOnce(&mut Self) -> Vec<Change>) -> Vec<Change> {
        let before = self.snapshot();
        let mut changes = change(self);
        self.settle();
        self.prune();
        let after = self.snapshot();
        if after.monitor != before.monitor
            && let Some(monitor) = after.monitor
        {
            changes.push(Change::FocusMonitor(monitor));
        }
        for layout in &after.layout {
            let old = before
                .layout
                .iter()
                .find(|old| old.monitor == layout.monitor);
            if old.map(|old| old.workspace) != Some(layout.workspace) {
                changes.push(Change::Workspace {
                    monitor: layout.monitor,
                    workspace: layout.workspace,
                });
            }
            if !old.is_some_and(|old| same_geometry(&old.windows, &layout.windows)) {
                changes.push(Change::Layout(layout.monitor));
            }
        }
        if after.focus != before.focus {
            changes.push(Change::Focus(after.focus));
            self.focus_trail.push(after.focus);
        }
        changes
    }

    /// Every window the focus went to since the last time this was asked,
    /// in order, and forget them.
    ///
    /// Hyprland posts `activewindow` at the moment the focus moves, so two
    /// windows mapped one after the other are two events even when nothing
    /// happens between them. A compositor that compares its state once a
    /// pass would say only where the focus ended; this is what lets it say
    /// where it went on the way.
    pub fn take_focus_trail(&mut self) -> Vec<Option<WindowId>> {
        std::mem::take(&mut self.focus_trail)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            focus: self.focused_window(),
            monitor: self.focused_monitor,
            layout: self.layout(),
        }
    }

    fn output(&self, monitor: MonitorId) -> Option<&Output> {
        self.outputs
            .iter()
            .find(|output| output.monitor.id == monitor)
    }

    /// The most recently focused window on a workspace.
    fn focused_on(&self, workspace: WorkspaceId) -> Option<WindowId> {
        self.history
            .iter()
            .rev()
            .find(|id| self.windows.get(id) == Some(&workspace))
            .copied()
    }

    /// The most recently focused tiled window on a workspace: where a new
    /// tiled window opens, as `dwindle:use_active_for_splits` has it.
    fn recent_tiled(&self, workspace: WorkspaceId) -> Option<WindowId> {
        let ws = self.workspaces.get(&workspace)?;
        self.history
            .iter()
            .rev()
            .find(|id| ws.tiling.contains(**id))
            .copied()
    }

    /// The origin of the monitor a workspace belongs to, `(0, 0)` if it has
    /// none.
    fn origin(&self, workspace: WorkspaceId) -> (i64, i64) {
        self.workspace_monitor(workspace)
            .and_then(|monitor| self.output(monitor))
            .map_or((0, 0), |output| {
                (output.monitor.rect.x, output.monitor.rect.y)
            })
    }

    /// A workspace's work area: its monitor less the reserved strips and
    /// `gaps_out`.
    /// The same for a *floating* window, which Hyprland keeps apart:
    /// `general:float_gaps` rather than `general:gaps_out`.
    fn float_work_area_of(&self, workspace: WorkspaceId) -> Rect {
        let mut settings = self.settings_at(workspace);
        settings.gaps_out = settings.float_gaps;
        self.workspace_monitor(workspace)
            .map(|monitor| work_area(&self.outputs, monitor, &settings))
            .unwrap_or_default()
    }

    fn work_area_of(&self, workspace: WorkspaceId) -> Rect {
        self.workspace_monitor(workspace)
            .map(|monitor| work_area(&self.outputs, monitor, &self.settings_at(workspace)))
            .unwrap_or_default()
    }

    fn first_free_workspace(&self) -> WorkspaceId {
        let mut id = 1_i64;
        for existing in self.workspaces.keys() {
            if existing.0 == id {
                id = id.saturating_add(1);
            } else if existing.0 > id {
                break;
            }
        }
        WorkspaceId(id)
    }

    fn ensure_workspace(&mut self, workspace: WorkspaceId, monitor: MonitorId) {
        // `workspace = …, layout:master` on a workspace that has no tree
        // yet, which is the only moment the tree's kind is decided.
        let layout = self.settings_at(workspace).layout;
        let _ws = self
            .workspaces
            .entry(workspace)
            .or_insert_with(|| Workspace {
                monitor,
                tiling: Tiling::new(layout),
                floating: Vec::new(),
                fullscreen: None,
            });
    }

    /// Put a window that is on no workspace onto one, tiled beside that
    /// workspace's most recently focused tiled window, or floating at the
    /// rectangle it has.
    fn attach(&mut self, window: WindowId, workspace: WorkspaceId, floating: bool) {
        let area = Area::of(self.work_area_of(workspace));
        let beside = self.recent_tiled(workspace);
        let settings = self.settings_at(workspace);
        let cursor = self.pointer;
        let Some(ws) = self.workspaces.get_mut(&workspace) else {
            return;
        };
        if floating {
            ws.floating.push(window);
        } else {
            ws.tiling.insert(window, beside, cursor, area, &settings);
        }
        let _previous = self.windows.insert(window, workspace);
    }

    /// Take a window off its workspace; it stops being fullscreen.
    fn detach(&mut self, window: WindowId) {
        let Some(workspace) = self.windows.remove(&window) else {
            return;
        };
        if let Some(ws) = self.workspaces.get_mut(&workspace) {
            ws.tiling.remove(window);
            ws.floating.retain(|id| *id != window);
            if ws.fullscreen.is_some_and(|(id, _)| id == window) {
                ws.fullscreen = None;
            }
        }
    }

    /// Move a window to another existing workspace, floating or tiled as it
    /// was.
    fn move_window_to(&mut self, window: WindowId, workspace: WorkspaceId) {
        let floating = self.is_floating(window);
        // Only the head is in the tiling, but the whole group goes with it.
        let members = self
            .groups
            .get(&window)
            .map(|group| group.members.clone())
            .unwrap_or_default();
        self.detach(window);
        self.attach(window, workspace, floating);
        for member in members.into_iter().skip(1) {
            let _ = self.windows.insert(member, workspace);
        }
    }

    /// Focus a window: its monitor gets focus and shows its workspace, it
    /// becomes the most recent in the history, and it is raised if it
    /// floats.
    fn focus(&mut self, window: WindowId) {
        let Some(workspace) = self.workspace_of(window) else {
            return;
        };
        if let Some(monitor) = self.workspace_monitor(workspace)
            && let Some(output) = self
                .outputs
                .iter_mut()
                .find(|output| output.monitor.id == monitor)
        {
            // Focusing a window on a special workspace shows that workspace
            // over the monitor's own; it does not switch to it, or hiding
            // the scratchpad again would leave the monitor showing it.
            if Self::is_special(workspace) {
                output.special = Some(workspace);
            } else {
                output.active = workspace;
            }
            self.focused_monitor = Some(monitor);
        }
        self.history.retain(|id| *id != window);
        self.history.push(window);
        if let Some(ws) = self.workspaces.get_mut(&workspace)
            && ws.floating.contains(&window)
        {
            ws.floating.retain(|id| *id != window);
            ws.floating.push(window);
        }
    }

    /// Show a workspace on the monitor it belongs to, creating it on the
    /// focused monitor if it does not exist, and focus that monitor.
    fn show(&mut self, workspace: WorkspaceId) {
        let Some(focused) = self.focused_monitor else {
            return;
        };
        // A special workspace is never shown *instead of* the monitor's own:
        // `workspace special:name` puts the scratchpad over what is there,
        // as `togglespecialworkspace` does.
        if Self::is_special(workspace) {
            self.ensure_workspace(workspace, focused);
            if let Some(output) = self
                .outputs
                .iter_mut()
                .find(|output| output.monitor.id == focused)
            {
                output.special = Some(workspace);
            }
            return;
        }
        self.ensure_workspace(workspace, focused);
        let monitor = match self.workspace_monitor(workspace) {
            Some(monitor) if self.output(monitor).is_some() => monitor,
            _ => {
                if let Some(ws) = self.workspaces.get_mut(&workspace) {
                    ws.monitor = focused;
                }
                focused
            }
        };
        if let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| output.monitor.id == monitor)
        {
            if output.active != workspace {
                output.previous = Some(output.active);
            }
            output.active = workspace;
            // `binds:hide_special_on_workspace_change`: the scratchpad goes
            // away when the workspace under it changes, which is what
            // `Actions::changeWorkspace` does with it.
            if self.settings.hide_special_on_workspace_change {
                output.special = None;
            }
        }
        self.focused_monitor = Some(monitor);
    }

    /// The workspace a target names, from the one the focused monitor
    /// shows.
    fn resolve(&self, target: WorkspaceTarget) -> Option<WorkspaceId> {
        let current = self.current_workspace()?;
        match target {
            WorkspaceTarget::Special(name) => Some(self.special_id(&name)),
            // `binds:workspace_back_and_forth`: asking for the workspace
            // that is already shown goes to the one before it instead,
            // which is what makes one key both there and back.
            WorkspaceTarget::Id(id) if id == current && self.settings.workspace_back_and_forth => {
                self.previous_workspace().or(Some(id))
            }
            WorkspaceTarget::Id(id) => Some(id),
            WorkspaceTarget::Previous => self.previous_workspace(),
            // `name:NAME`: the workspace that has that name, or the first
            // free number, which is what `getWorkspaceIDNameFromString`
            // does with a name nothing carries yet.
            WorkspaceTarget::Named(name) => Some(
                self.names
                    .iter()
                    .find(|(_, held)| **held == name)
                    .map(|(id, _)| *id)
                    .unwrap_or_else(|| self.first_free_workspace()),
            ),
            WorkspaceTarget::Next => Some(WorkspaceId(current.0.saturating_add(1).max(1))),
            WorkspaceTarget::Empty { after_current } => {
                let from = if after_current { current.0 } else { 0 };
                let mut id = from.saturating_add(1).max(1);
                // The lowest-numbered workspace with nothing on it, which
                // is either one that does not exist or one that is empty.
                while self
                    .workspaces
                    .get(&WorkspaceId(id))
                    .is_some_and(|ws| !ws.is_empty())
                {
                    id = id.saturating_add(1);
                }
                Some(WorkspaceId(id))
            }
            WorkspaceTarget::Relative(offset) => {
                Some(WorkspaceId(current.0.saturating_add(offset).max(1)))
            }
            WorkspaceTarget::Open(offset) => {
                let ids: Vec<WorkspaceId> = self.workspaces.keys().copied().collect();
                let count = i64::try_from(ids.len()).ok()?;
                let index = i64::try_from(ids.iter().position(|id| *id == current)?).ok()?;
                let next = index.checked_add(offset)?.checked_rem_euclid(count)?;
                ids.get(usize::try_from(next).ok()?).copied()
            }
        }
    }

    // -- Monitors -------------------------------------------------------------

    /// Give `window` a floating rectangle, held down to the size its rules
    /// allow. The rectangle is the workspace's own, as the map holds it.
    ///
    /// Every floating rectangle goes through here, so `min_size`,
    /// `max_size` and `keep_aspect_ratio` hold for the rule that opened
    /// the window, for a manual resize, and for the layout putting it back
    /// where it floated -- which is what Hyprland's own `setSizeLimits`
    /// does by clamping at each of those points.
    fn hold_floating(&mut self, window: WindowId, rect: Rect) {
        let held = self
            .limits
            .get(&window)
            .copied()
            .unwrap_or_default()
            .hold(rect);
        let _previous = self.floating_rects.insert(window, held);
    }

    /// `min_size`, `max_size`, `no_max_size` and `keep_aspect_ratio` for
    /// one window, as its `windowrule`s gave them.
    pub fn set_limits(&mut self, window: WindowId, limits: Limits) -> Vec<Change> {
        let _previous = self.limits.insert(window, limits);
        let Some(rect) = self.floating_rects.get(&window).copied() else {
            return Vec::new();
        };
        self.run(|state| {
            state.hold_floating(window, rect);
            Vec::new()
        })
    }

    /// The workspace the focused monitor showed before this one.
    fn previous_workspace(&self) -> Option<WorkspaceId> {
        self.outputs
            .iter()
            .find(|output| Some(output.monitor.id) == self.focused_monitor)?
            .previous
    }

    /// The monitor a dispatcher's argument names, by
    /// `CMonitorQueryCore::fromConfigString`'s rules.
    fn monitor_target(&self, target: &MonitorTarget) -> Option<MonitorId> {
        match target {
            MonitorTarget::Current => self.focused_monitor,
            MonitorTarget::Direction(direction) => self.monitor_towards(*direction),
            MonitorTarget::Relative(offset) => self.monitor_along(*offset),
            // Hyprland's monitor ids count from zero, and this tree's from
            // one: the first monitor is `0` to a person and `MonitorId(1)`
            // here, as `hyprctl monitors` prints it.
            MonitorTarget::Id(id) => u32::try_from(id.saturating_add(1))
                .ok()
                .map(MonitorId)
                .filter(|id| self.output(*id).is_some()),
            MonitorTarget::Named(name) => self.monitor_named(name),
        }
    }

    /// The monitor called `name`.
    #[must_use]
    pub fn monitor_named(&self, name: &str) -> Option<MonitorId> {
        self.outputs
            .iter()
            .find(|output| output.monitor.name == name)
            .map(|output| output.monitor.id)
    }

    /// The monitor `offset` places along from the focused one, wrapping
    /// around: `focusmonitor +1`.
    fn monitor_along(&self, offset: i64) -> Option<MonitorId> {
        let count = i64::try_from(self.outputs.len()).ok()?;
        if count == 0 {
            return None;
        }
        let at = self
            .outputs
            .iter()
            .position(|output| Some(output.monitor.id) == self.focused_monitor)
            .and_then(|at| i64::try_from(at).ok())
            .unwrap_or(0);
        let wrapped = (at + offset % count + count) % count;
        let index = usize::try_from(wrapped).ok()?;
        self.outputs.get(index).map(|output| output.monitor.id)
    }

    /// `focusmonitor`: focus a monitor, and whatever window was last
    /// focused on the workspace it shows.
    fn focus_monitor(&mut self, target: &MonitorTarget) -> Vec<Change> {
        let Some(monitor) = self.monitor_target(target) else {
            return Vec::new();
        };
        self.focused_monitor = Some(monitor);
        if let Some(window) = self
            .active_workspace(monitor)
            .and_then(|workspace| self.focused_on(workspace))
        {
            self.focus(window);
        }
        Vec::new()
    }

    /// `movewindow mon:<monitor>`: send the focused window to a monitor's
    /// active workspace.
    fn move_window_to_monitor(&mut self, target: &MonitorTarget, silent: bool) -> Vec<Change> {
        let (Some(window), Some(monitor)) = (self.focused_window(), self.monitor_target(target))
        else {
            return Vec::new();
        };
        let Some(to) = self.active_workspace(monitor) else {
            return Vec::new();
        };
        if self.workspace_of(window) == Some(to) {
            return Vec::new();
        }
        self.move_window_to(window, to);
        if silent {
            // The window went; the focus stays where it was, which is what
            // `silent` means.
            if let Some(stay) = self.focused_window() {
                self.focus(stay);
            }
        } else {
            self.focused_monitor = Some(monitor);
            self.focus(window);
        }
        vec![Change::MoveToWorkspace {
            window,
            workspace: to,
        }]
    }

    /// `movecurrentworkspacetomonitor`: the focused monitor's workspace goes
    /// to another monitor, which then shows it.
    fn move_current_workspace_to_monitor(&mut self, target: &MonitorTarget) -> Vec<Change> {
        let Some(workspace) = self.current_workspace() else {
            return Vec::new();
        };
        self.workspace_onto(workspace, target)
    }

    /// `moveworkspacetomonitor`: a workspace goes to a monitor.
    fn move_workspace_to_monitor(
        &mut self,
        workspace: WorkspaceTarget,
        target: &MonitorTarget,
    ) -> Vec<Change> {
        let Some(workspace) = self.resolve(workspace) else {
            return Vec::new();
        };
        self.workspace_onto(workspace, target)
    }

    /// Put `workspace` on the monitor `target` names, and show it there.
    ///
    /// The monitor it came from is left showing something: the workspace it
    /// showed before, or a new one, since a monitor showing nothing at all
    /// is not a state Hyprland leaves a screen in.
    fn workspace_onto(&mut self, workspace: WorkspaceId, target: &MonitorTarget) -> Vec<Change> {
        let Some(monitor) = self.monitor_target(target) else {
            return Vec::new();
        };
        let from = self.workspace_monitor(workspace);
        if from == Some(monitor) {
            return Vec::new();
        }
        self.ensure_workspace(workspace, monitor);
        if let Some(ws) = self.workspaces.get_mut(&workspace) {
            ws.monitor = monitor;
        }
        if let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| output.monitor.id == monitor)
        {
            output.active = workspace;
        }
        if let Some(from) = from {
            let replacement = self
                .workspaces
                .iter()
                .find(|(id, ws)| ws.monitor == from && **id != workspace)
                .map(|(id, _)| *id)
                .unwrap_or_else(|| self.first_free_workspace());
            self.ensure_workspace(replacement, from);
            if let Some(output) = self
                .outputs
                .iter_mut()
                .find(|output| output.monitor.id == from)
            {
                output.active = replacement;
            }
        }
        Vec::new()
    }

    /// `swapactiveworkspaces`: two monitors exchange what they are showing.
    fn swap_active_workspaces(
        &mut self,
        one: &MonitorTarget,
        other: &MonitorTarget,
    ) -> Vec<Change> {
        let (Some(first), Some(second)) = (self.monitor_target(one), self.monitor_target(other))
        else {
            return Vec::new();
        };
        if first == second {
            return Vec::new();
        }
        let (Some(here), Some(there)) =
            (self.active_workspace(first), self.active_workspace(second))
        else {
            return Vec::new();
        };
        for (workspace, monitor) in [(here, second), (there, first)] {
            if let Some(ws) = self.workspaces.get_mut(&workspace) {
                ws.monitor = monitor;
            }
        }
        for (monitor, workspace) in [(first, there), (second, here)] {
            if let Some(output) = self
                .outputs
                .iter_mut()
                .find(|output| output.monitor.id == monitor)
            {
                output.active = workspace;
            }
        }
        Vec::new()
    }

    // -- Groups ---------------------------------------------------------------

    /// The group `window` belongs to, and its head.
    fn group_of(&self, window: WindowId) -> Option<(WindowId, &Group)> {
        self.groups
            .iter()
            .find(|(_, group)| group.members.contains(&window))
            .map(|(head, group)| (*head, group))
    }

    /// The members of the group `window` is in, or nothing if it is in none.
    #[must_use]
    pub fn group(&self, window: WindowId) -> Option<&Group> {
        self.group_of(window).map(|(_, group)| group)
    }

    /// What a slot draws: the active member if `window` heads a group, and
    /// `window` itself if it heads none.
    fn shown(&self, window: WindowId) -> WindowId {
        self.groups
            .get(&window)
            .and_then(Group::showing)
            .unwrap_or(window)
    }

    /// What holds `window`'s place in the tiling: its group's head if it is
    /// in a group, since only the head is in the tree.
    fn in_tiling(&self, window: WindowId) -> WindowId {
        self.group_of(window).map_or(window, |(head, _)| head)
    }

    /// Whether `lockgroups` has locked them.
    #[must_use]
    pub const fn groups_locked(&self) -> bool {
        self.groups_locked
    }

    /// `togglegroup`: make the focused window a group, or dissolve the one
    /// it is in.
    ///
    /// Dissolving puts every member back in the tiling beside the head,
    /// which is where they would have been had they never been grouped.
    fn toggle_group(&mut self) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        if let Some((head, _)) = self.group_of(window) {
            let Some(group) = self.groups.remove(&head) else {
                return Vec::new();
            };
            let Some(workspace) = self.workspace_of(head) else {
                return Vec::new();
            };
            let area = Area::of(self.work_area_of(workspace));
            let settings = self.settings;
            if let Some(ws) = self.workspaces.get_mut(&workspace) {
                for member in group.members.iter().skip(1) {
                    // No cursor: a group breaking up puts its members back
                    // beside their head, and that is not a window opening.
                    ws.tiling.insert(*member, Some(head), None, area, &settings);
                }
            }
            self.focus(window);
            return Vec::new();
        }
        // A floating window is not in the tiling, so it has no slot to
        // share; Hyprland's `togglegroup` does nothing for one either.
        if self.is_floating(window) {
            return Vec::new();
        }
        let _ = self.groups.insert(
            window,
            Group {
                members: vec![window],
                active: 0,
                locked: false,
            },
        );
        Vec::new()
    }

    /// `changegroupactive`: show another member of the focused window's
    /// group, and focus it.
    fn change_group_active(&mut self, which: GroupMember) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some((head, group)) = self.group_of(window) else {
            return Vec::new();
        };
        let count = group.members.len();
        if count == 0 {
            return Vec::new();
        }
        let active = match which {
            // Hyprland wraps both ways.
            GroupMember::Forward => (group.active + 1) % count,
            GroupMember::Back => (group.active + count - 1) % count,
            // Its index is one-based, and out of range does nothing.
            GroupMember::Index(index) => match usize::try_from(index) {
                Ok(index) if (1..=count).contains(&index) => index - 1,
                _ => return Vec::new(),
            },
        };
        let Some(showing) = self.groups.get_mut(&head).map(|group| {
            group.active = active;
            group.members.get(active).copied()
        }) else {
            return Vec::new();
        };
        if let Some(showing) = showing {
            self.focus(showing);
        }
        Vec::new()
    }

    /// `moveintogroup`: put the focused window into the group in `direction`.
    fn move_into_group(&mut self, direction: Direction) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        if self.group_of(window).is_some() || self.is_floating(window) {
            return Vec::new();
        }
        let Some(target) = self.window_in_direction(window, direction) else {
            return Vec::new();
        };
        let Some((head, _)) = self.group_of(target) else {
            return Vec::new();
        };
        // Out of the tiling: from here on the group's head holds its place.
        if let Some(workspace) = self.workspace_of(window)
            && let Some(ws) = self.workspaces.get_mut(&workspace)
        {
            ws.tiling.remove(window);
        }
        if let Some(group) = self.groups.get_mut(&head) {
            group.members.push(window);
            group.active = group.members.len().saturating_sub(1);
        }
        self.focus(window);
        Vec::new()
    }

    /// `moveintoorcreategroup`: the same, making the window in that
    /// direction into a group of one first if it is not already in a group.
    ///
    /// This is what people bind rather than `moveintogroup`: the first
    /// press makes the group and the second joins it, so a group never has
    /// to be made by hand.
    fn move_into_or_create_group(&mut self, direction: Direction) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        if self.group_of(window).is_some() || self.is_floating(window) {
            return Vec::new();
        }
        let Some(target) = self.window_in_direction(window, direction) else {
            return Vec::new();
        };
        if self.group_of(target).is_none() {
            if self.is_floating(target) {
                return Vec::new();
            }
            let _ = self.groups.insert(
                target,
                Group {
                    members: vec![target],
                    active: 0,
                    locked: false,
                },
            );
        }
        self.move_into_group(direction)
    }

    /// `movewindoworgroup`: into the group in a direction if there is one
    /// there, and past the window there otherwise.
    ///
    /// The one dispatcher people bind to each arrow: it fills a group when
    /// there is one to fill and moves the window when there is not.
    fn move_window_or_group(&mut self, direction: Direction) -> Vec<Change> {
        let joins = self
            .focused_window()
            .filter(|window| !self.is_floating(*window) && self.group_of(*window).is_none())
            .and_then(|window| self.window_in_direction(window, direction))
            .is_some_and(|target| self.group_of(target).is_some());
        if joins {
            return self.move_into_group(direction);
        }
        self.move_window(direction)
    }

    /// `focusworkspaceoncurrentmonitor`: show a workspace here rather than
    /// going to the monitor it is on.
    ///
    /// `workspace` follows a workspace to wherever it lives; this brings it
    /// over instead, which is what a person with two screens binds when
    /// they want the numbers to mean "here".
    fn focus_workspace_here(&mut self, target: WorkspaceTarget) -> Vec<Change> {
        let Some(workspace) = self.resolve(target) else {
            return Vec::new();
        };
        let changes = self.workspace_onto(workspace, &MonitorTarget::Current);
        self.show(workspace);
        if let Some(window) = self.focused_on(workspace) {
            self.focus(window);
        }
        changes
    }

    /// `layoutmsg`: a message to the layout itself.
    ///
    /// Each of Hyprland's two layouts reads its own set of messages and
    /// ignores the other's, and so does this: `layoutmsg togglesplit` says
    /// nothing to the master layout and `layoutmsg swapwithmaster` says
    /// nothing to dwindle. A message neither knows is ignored, as
    /// Hyprland ignores one.
    fn layout_message(&mut self, message: &str) -> Vec<Change> {
        let mut words = message.split_whitespace();
        let Some(word) = words.next() else {
            return Vec::new();
        };
        let rest: Vec<&str> = words.collect();
        match self.settings.layout {
            Layout::Dwindle => self.dwindle_message(word, &rest),
            Layout::Master => self.master_message(word, &rest),
            // Monocle's own messages are `cyclenext` and `cycleprev`,
            // which are the dispatcher of the same name and belong to no
            // layout; every other word is not one of its messages.
            Layout::Monocle => match word {
                "cyclenext" => self.cycle_next(&false, &true),
                "cycleprev" => self.cycle_next(&true, &true),
                _ => Vec::new(),
            },
            Layout::Scrolling => self.scrolling_message(word, &rest),
        }
    }

    /// The scrolling layout's own messages, which are the largest of the
    /// four layouts'.
    ///
    /// `focus` moves the focus, which is the compositor's and not the
    /// tape's, so it is answered here with the same walk `movefocus` uses;
    /// everything else is the tape's own.
    fn scrolling_message(&mut self, word: &str, rest: &[&str]) -> Vec<Change> {
        let focused = self.focused_window();
        if word == "focus" {
            return self.scrolling_focus(rest.first().copied().unwrap_or(""));
        }
        let settings = self.settings;
        let Some(workspace) = focused.and_then(|window| self.workspace_of(window)) else {
            return Vec::new();
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a monitor's pixels are far inside f64's exact range"
        )]
        let across = self.work_area_of(workspace).width as f64;
        let mut words = vec![word];
        words.extend_from_slice(rest);
        let Some(tape) = self
            .workspaces
            .get_mut(&workspace)
            .and_then(|ws| ws.tiling.scrolling())
        else {
            return Vec::new();
        };
        if !tape.message(&words, focused, &settings, across) {
            return Vec::new();
        }
        self.run(|_| Vec::new())
    }

    /// `layoutmsg focus <direction>`: the tape's own focus walk.
    ///
    /// `l` and `r` step between columns and `u` and `d` within one, which
    /// is what a tape that runs sideways means by a direction.
    fn scrolling_focus(&mut self, direction: &str) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let settings = self.settings;
        let (right, along) = match direction.chars().next() {
            Some('l') => (false, true),
            Some('r') => (true, true),
            Some('u' | 't') => (false, false),
            Some('b' | 'd') => (true, false),
            _ => return Vec::new(),
        };
        let Some(next) = self
            .workspaces
            .get(&workspace)
            .and_then(|ws| match &ws.tiling {
                Tiling::Scrolling(scrolling) => scrolling.beside(window, right, along, &settings),
                _ => None,
            })
        else {
            return Vec::new();
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a monitor's pixels are far inside f64's exact range"
        )]
        let across = self.work_area_of(workspace).width as f64;
        self.run(|state| {
            state.focus(next);
            if let Some(tape) = state
                .workspaces
                .get_mut(&workspace)
                .and_then(|ws| ws.tiling.scrolling())
            {
                tape.follow(next, &settings, across);
            }
            Vec::new()
        })
    }

    /// The dwindle layout's own messages, which all act on the focused
    /// window's place in the tree.
    fn dwindle_message(&mut self, word: &str, rest: &[&str]) -> Vec<Change> {
        // `preselect` is the one that acts on no window: it says where the
        // *next* window goes.
        if word == "preselect" {
            let direction = rest.first().and_then(|text| Direction::parse(text));
            let Some(workspace) = self.current_workspace() else {
                return Vec::new();
            };
            if let Some(ws) = self.workspaces.get_mut(&workspace)
                && let Some(dwindle) = ws.tiling.dwindle()
            {
                dwindle.preselect(direction);
            }
            return Vec::new();
        }
        let Some(window) = self.focused_window().map(|window| self.in_tiling(window)) else {
            return Vec::new();
        };
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        // `movetoroot` takes an optional `unstable`; anything else is the
        // stable form, as Hyprland reads it.
        let stable = rest.first() != Some(&"unstable");
        let Some(ws) = self.workspaces.get_mut(&workspace) else {
            return Vec::new();
        };
        let Some(dwindle) = ws.tiling.dwindle() else {
            return Vec::new();
        };
        // The changes are `run`'s to work out: every one of these moves a
        // window, and the caller is told by the layout it sees afterwards.
        let _acted = match word {
            "togglesplit" => dwindle.toggle_split(window),
            "swapsplit" => dwindle.swap_split(window),
            "movetoroot" => dwindle.move_to_root(window, stable),
            _ => false,
        };
        Vec::new()
    }

    /// The master layout's own messages.
    fn master_message(&mut self, word: &str, rest: &[&str]) -> Vec<Change> {
        // The two that change a setting rather than the list.
        match word {
            "mfact" => return self.set_mfact(rest),
            other if other.starts_with("orientation") => return self.set_orientation(other, rest),

            _ => {}
        }
        let Some(window) = self.focused_window().map(|window| self.in_tiling(window)) else {
            return Vec::new();
        };
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        // `focusmaster` moves the focus rather than the windows.
        if word == "focusmaster" {
            let master = self
                .workspaces
                .get_mut(&workspace)
                .and_then(|ws| ws.tiling.master())
                .and_then(|master| master.master());
            let Some(master) = master else {
                return Vec::new();
            };
            self.focus(master);
            return Vec::new();
        }
        let Some(ws) = self.workspaces.get_mut(&workspace) else {
            return Vec::new();
        };
        let Some(master) = ws.tiling.master() else {
            return Vec::new();
        };
        let small = self.settings.master.allow_small_split;
        let _acted = match word {
            "addmaster" => master.add_master(window, small),
            "removemaster" => master.remove_master(window),
            "swapwithmaster" => master.swap_with_master(window),
            "swapnext" => master.swap_along(window, false),
            "swapprev" => master.swap_along(window, true),
            "rollnext" => {
                master.roll(false);
                true
            }
            "rollprev" => {
                master.roll(true);
                true
            }
            _ => false,
        };
        // `cyclenext` and `cycleprev` are the dispatcher of the same name,
        // which walks the focus and belongs to neither layout.
        match word {
            "cyclenext" => self.cycle_next(&false, &true),
            "cycleprev" => self.cycle_next(&true, &true),
            _ => Vec::new(),
        }
    }

    /// `layoutmsg mfact [exact] <value>`: how much of the screen the master
    /// takes, as a share between nothing and all of it.
    fn set_mfact(&mut self, rest: &[&str]) -> Vec<Change> {
        let (exact, text) = match rest {
            ["exact", value, ..] => (true, *value),
            [value, ..] => (false, *value),
            [] => return Vec::new(),
        };
        let Ok(value) = text.parse::<f64>() else {
            return Vec::new();
        };
        let mfact = if exact {
            value
        } else {
            self.settings.master.mfact + value
        };
        // Hyprland clamps to the same bounds its configuration does.
        let mfact = mfact.clamp(0.05, 0.95);
        if (mfact - self.settings.master.mfact).abs() < f64::EPSILON {
            return Vec::new();
        }
        self.settings.master.mfact = mfact;
        self.run(|_| Vec::new())
    }

    /// `layoutmsg orientation<side>`, `orientationnext`, `orientationprev`
    /// and `orientationcycle`: which side of the screen the master is on.
    fn set_orientation(&mut self, word: &str, rest: &[&str]) -> Vec<Change> {
        // `orientation left` and `orientationleft` are both written.
        let named = word.strip_prefix("orientation").unwrap_or("");
        let named = if named.is_empty() {
            rest.first().copied().unwrap_or("")
        } else {
            named
        };
        // The order `buildOrientationCycleVectorFromEOperation` walks,
        // which is the enum's own: left, top, right, bottom, centre.
        let round = [
            Orientation::Left,
            Orientation::Top,
            Orientation::Right,
            Orientation::Bottom,
            Orientation::Center,
        ];
        let at = round
            .iter()
            .position(|side| *side == self.settings.master.orientation)
            .unwrap_or(0);
        let along = |step: usize| round.get((at + step) % round.len()).copied();
        let orientation = match named {
            "left" => Some(Orientation::Left),
            "right" => Some(Orientation::Right),
            "top" | "up" => Some(Orientation::Top),
            "bottom" | "down" => Some(Orientation::Bottom),
            "center" => Some(Orientation::Center),
            "next" | "cycle" => along(1),
            "prev" => along(round.len() - 1),
            _ => None,
        };
        let Some(orientation) = orientation else {
            return Vec::new();
        };
        if orientation == self.settings.master.orientation {
            return Vec::new();
        }
        self.settings.master.orientation = orientation;
        self.run(|_| Vec::new())
    }

    /// `movewindowpixel`: move a window the compositor picked out.
    ///
    /// # Errors
    ///
    /// The window is not one this layout holds.
    pub fn move_window_pixel(&mut self, window: WindowId, by: &Move) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        Ok(self.run(|state| state.move_floating(window, by, false, Corner::NONE, false)))
    }

    /// `resizewindowpixel`: resize a window the compositor picked out,
    /// pulling on no particular edge.
    ///
    /// # Errors
    ///
    /// The window is not one this layout holds.
    pub fn resize_window_pixel(
        &mut self,
        window: WindowId,
        by: &Move,
    ) -> Result<Vec<Change>, Error> {
        self.resize_window_pixel_at(window, by, Corner::NONE)
    }

    /// Resize a window, pulling on the edge a drag grabbed.
    ///
    /// The same as [`State::resize_window_pixel`] but for the corner, which
    /// says which edge moves and -- for a tiled window -- which of the
    /// splits above it. A drag that grabbed a border has one; a dispatcher
    /// has not.
    ///
    /// # Errors
    ///
    /// The window is not one this layout holds.
    pub fn resize_window_pixel_at(
        &mut self,
        window: WindowId,
        by: &Move,
        corner: Corner,
    ) -> Result<Vec<Change>, Error> {
        self.resize_window(window, by, corner, false)
    }

    /// Move a window because the pointer is dragging it, which is
    /// [`State::move_window_pixel`] with `general:snap:*` on top.
    ///
    /// A dispatcher and a drag are different callers on purpose: Hyprland's
    /// drag controller is the only thing that calls `performSnap`, so
    /// `movewindowpixel 10 0` moves by ten pixels and means it while a hand
    /// dragging a window is helped to the edge.
    ///
    /// # Errors
    ///
    /// The window is not one this layout holds.
    pub fn drag_window_pixel(&mut self, window: WindowId, by: &Move) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        Ok(self.run(|state| state.move_floating(window, by, false, Corner::NONE, true)))
    }

    /// Lift the focused window, tiled, out of the tiling because the pointer
    /// has started to drag it: it floats at the size it had, centred on the
    /// pointer at `at`, until [`State::drop_window`] puts it back.
    ///
    /// Hyprland's `CDragStateController::updateDragWindow`: a tiled target
    /// dragged with `movewindow` is set floating at its own size with its
    /// middle at the mouse, and the drag remembers it was tiled. A floating
    /// window, or one that is not focused, is not lifted -- the drag moves
    /// the first as it is, and the compositor drags only the focused one.
    ///
    /// # Errors
    ///
    /// The window is not one this layout holds.
    pub fn lift_window(&mut self, window: WindowId, at: (f64, f64)) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        if self.is_floating(window) || self.focused_window() != Some(window) {
            return Ok(Vec::new());
        }
        let Some(rect) = self.rect_of(window) else {
            return Ok(Vec::new());
        };
        let kept = self.floating_rects.get(&window).copied();
        Ok(self.run(|state| {
            let mut changes = state.toggle_floating();
            if !state.is_floating(window) {
                return changes;
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the pointer is on a screen, far inside i64"
            )]
            let (x, y) = (at.0.round() as i64, at.1.round() as i64);
            let lifted = Rect::new(
                x.saturating_sub(rect.width / 2),
                y.saturating_sub(rect.height / 2),
                rect.width,
                rect.height,
            );
            changes.extend(state.place_floating(window, lifted));
            let _previous = state.lifted.insert(window, kept);
            changes
        }))
    }

    /// Drop a window [`State::lift_window`] lifted back into the tiling, at
    /// the point `at` the pointer let go of it.
    ///
    /// Hyprland's `CDragStateController::dragEnd`: a target that was
    /// dragged tiled is set tiled again, which adds it to the layout with
    /// the pointer where it was let go and `wasDraggingWindow()` set -- the
    /// one moment `dwindle:precise_mouse_move` is read. Its floating size
    /// is given back as it was before the drag. A window that was not
    /// lifted, or that something floated for good while it was held, is
    /// left alone.
    ///
    /// # Errors
    ///
    /// The window is not one this layout holds.
    pub fn drop_window(&mut self, window: WindowId, at: (f64, f64)) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        let Some(kept) = self.lifted.remove(&window) else {
            return Ok(Vec::new());
        };
        if !self.is_floating(window) {
            return Ok(Vec::new());
        }
        Ok(self.run(|state| {
            let Some(workspace) = state.workspace_of(window) else {
                return Vec::new();
            };
            let area = Area::of(state.work_area_of(workspace));
            let settings = state.settings;
            let Some(ws) = state.workspaces.get_mut(&workspace) else {
                return Vec::new();
            };
            ws.floating.retain(|id| *id != window);
            ws.tiling.insert_dropped(window, at, area, &settings);
            match kept {
                Some(rect) => {
                    let _previous = state.floating_rects.insert(window, rect);
                }
                None => {
                    let _gone = state.floating_rects.remove(&window);
                }
            }
            vec![Change::Floating {
                window,
                floating: false,
            }]
        }))
    }

    /// Resize a window because the pointer is dragging an edge of it:
    /// [`State::resize_window_pixel_at`] with `general:snap:*` on top.
    ///
    /// # Errors
    ///
    /// The window is not one this layout holds.
    pub fn drag_resize_window_pixel(
        &mut self,
        window: WindowId,
        by: &Move,
        corner: Corner,
    ) -> Result<Vec<Change>, Error> {
        self.resize_window(window, by, corner, true)
    }

    /// What both resize entry points come down to.
    fn resize_window(
        &mut self,
        window: WindowId,
        by: &Move,
        corner: Corner,
        snapping: bool,
    ) -> Result<Vec<Change>, Error> {
        if !self.windows.contains_key(&window) {
            return Err(Error::UnknownWindow(window));
        }
        Ok(self.run(|state| state.move_floating(window, by, true, corner, snapping)))
    }

    /// The window whose border is under `at`, and which of its edges that
    /// is, when `general:resize_on_border` says a border may be grabbed.
    ///
    /// Hyprland's `processMouseDownNormal`: the window's box grown by
    /// `general:border_size + general:extend_border_grab_area` on every
    /// side is what counts as its border, *less the window itself* -- a
    /// press inside a window belongs to the client, and only the ring
    /// around it resizes. The grab area is why a one-pixel border can be
    /// hit at all.
    ///
    /// The topmost window wins, which is the order [`State::layout`] draws
    /// in read backwards, so a floating window's border is grabbed rather
    /// than that of the tiled window beneath it. A fullscreen window has no
    /// border to grab.
    ///
    /// The corner is the edge or edges `at` is beyond; a press off the end
    /// of one side, in the ring's corner, names both.
    #[must_use]
    pub fn border_at(&self, at: (f64, f64)) -> Option<(WindowId, Corner)> {
        if !self.settings.resize_on_border {
            return None;
        }
        let reach = self.settings.border_size + self.settings.border_grab_extend;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's edge is a screen coordinate, far inside f64"
        )]
        let edges = |rect: Rect| {
            (
                rect.x as f64,
                rect.y as f64,
                (rect.x + rect.width) as f64,
                (rect.y + rect.height) as f64,
            )
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "as above: a configured reach is a handful of pixels"
        )]
        let reach = reach as f64;
        self.layout()
            .iter()
            .flat_map(|output| output.windows.iter())
            .rev()
            .find_map(|placed| {
                if self
                    .workspace_of(placed.window)
                    .and_then(|workspace| self.fullscreen(workspace))
                    .is_some_and(|(id, _)| id == placed.window)
                {
                    return None;
                }
                let (left, top, right, bottom) = edges(placed.rect);
                let inside = at.0 >= left && at.0 < right && at.1 >= top && at.1 < bottom;
                let within = at.0 >= left - reach
                    && at.0 < right + reach
                    && at.1 >= top - reach
                    && at.1 < bottom + reach;
                if inside || !within {
                    return None;
                }
                Some((
                    placed.window,
                    Corner {
                        left: at.0 < left,
                        right: at.0 >= right,
                        top: at.1 < top,
                        bottom: at.1 >= bottom,
                    },
                ))
            })
    }

    /// `moveoutofgroup`: take the focused window out of its group and put it
    /// back in the tiling.
    ///
    /// Taking the head out moves the group's place to the next member, which
    /// is what Hyprland does: a group with a member left is still a group.
    fn move_out_of_group(&mut self) -> Vec<Change> {
        let Some(window) = self.focused_window() else {
            return Vec::new();
        };
        let Some((head, _)) = self.group_of(window) else {
            return Vec::new();
        };
        let Some(workspace) = self.workspace_of(window) else {
            return Vec::new();
        };
        let Some(mut group) = self.groups.remove(&head) else {
            return Vec::new();
        };
        group.members.retain(|member| *member != window);
        group.active = group.active.min(group.members.len().saturating_sub(1));

        let area = Area::of(self.work_area_of(workspace));
        let settings = self.settings;
        if let Some(ws) = self.workspaces.get_mut(&workspace) {
            if window == head {
                // The head left: whoever is first now takes its place in the
                // tree, and the window that left goes beside it.
                ws.tiling.remove(head);
                if let Some(next) = group.members.first().copied() {
                    ws.tiling.insert(next, None, None, area, &settings);
                    ws.tiling.insert(window, Some(next), None, area, &settings);
                } else {
                    ws.tiling.insert(window, None, None, area, &settings);
                }
            } else {
                ws.tiling.insert(window, Some(head), None, area, &settings);
            }
        }
        // A group of one is no group, which is what Hyprland's own
        // `moveoutofgroup` leaves behind.
        if group.members.len() > 1
            && let Some(next) = group.members.first().copied()
        {
            let _ = self.groups.insert(next, group);
        }
        self.focus(window);
        Vec::new()
    }

    /// Forget a window that has gone, from whatever group held it.
    fn ungroup(&mut self, window: WindowId) {
        let Some((head, _)) = self.group_of(window) else {
            return;
        };
        let Some(mut group) = self.groups.remove(&head) else {
            return;
        };
        group.members.retain(|member| *member != window);
        group.active = group.active.min(group.members.len().saturating_sub(1));
        let Some(next) = group.members.first().copied() else {
            return;
        };
        if window == head {
            // The head's slot in the tree goes with it; the next member
            // takes its place.
            if let Some(workspace) = self.workspace_of(next) {
                let area = Area::of(self.work_area_of(workspace));
                let settings = self.settings;
                if let Some(ws) = self.workspaces.get_mut(&workspace) {
                    ws.tiling.insert(next, None, None, area, &settings);
                }
            }
        }
        if group.members.len() > 1 {
            let _ = self.groups.insert(next, group);
        }
    }

    /// The id of the special workspace called `name`, making one if there is
    /// none.
    ///
    /// Hyprland's special workspaces have negative ids: `special:special` is
    /// `SPECIAL_WORKSPACE_START`, −99, and every other counts up from there
    /// towards −2 (`State::workspaceState()->newSpecialID()`). The name is
    /// `special:` and the name, which is what `hyprctl` prints and what a
    /// `workspace` rule matches.
    fn special_id(&self, name: &str) -> WorkspaceId {
        let full = format!("special:{name}");
        if let Some((id, _)) = self.names.iter().find(|(_, known)| **known == full) {
            return *id;
        }
        if name == "special" {
            return WorkspaceId(SPECIAL_START);
        }
        // The first free id from −99 upwards, as `newSpecialID` takes the
        // highest in use and adds one.
        let taken = |id: i64| {
            self.workspaces.contains_key(&WorkspaceId(id))
                || self.names.contains_key(&WorkspaceId(id))
        };
        let mut id = SPECIAL_START;
        while taken(id) && id < -2 {
            id = id.saturating_add(1);
        }
        WorkspaceId(id)
    }

    /// `togglespecialworkspace`: show the special workspace over the focused
    /// monitor, or hide it if it is the one already showing.
    ///
    /// The workspace is made if it does not exist, as Hyprland makes one; an
    /// empty scratchpad is a scratchpad you can put something in.
    fn toggle_special(&mut self, name: &str) -> Vec<Change> {
        let Some(monitor) = self
            .focused_monitor
            .or_else(|| self.outputs.first().map(|output| output.monitor.id))
        else {
            return Vec::new();
        };
        let id = self.special_id(name);
        let showing = self
            .outputs
            .iter()
            .find(|output| output.monitor.id == monitor)
            .and_then(|output| output.special);
        if showing == Some(id) {
            if let Some(output) = self
                .outputs
                .iter_mut()
                .find(|output| output.monitor.id == monitor)
            {
                output.special = None;
            }
            // The focus goes back to the monitor's own workspace.
            if let Some(window) = self.recent_on_monitor(monitor) {
                self.focus(window);
            }
            return Vec::new();
        }
        self.ensure_workspace(id, monitor);
        let _ = self.names.insert(id, format!("special:{name}"));
        if let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| output.monitor.id == monitor)
        {
            output.special = Some(id);
        }
        self.focused_monitor = Some(monitor);
        // A special workspace with something on it takes the focus, as
        // Hyprland's does; an empty one leaves it where it was.
        if let Some(window) = self.recent_tiled(id).or_else(|| {
            self.workspaces
                .get(&id)
                .and_then(|ws| ws.floating.last().copied())
        }) {
            self.focus(window);
        }
        Vec::new()
    }

    /// The most recently focused window on the monitor's own workspace.
    fn recent_on_monitor(&self, monitor: MonitorId) -> Option<WindowId> {
        let active = self
            .outputs
            .iter()
            .find(|output| output.monitor.id == monitor)?
            .active;
        self.history
            .iter()
            .rev()
            .find(|window| self.windows.get(window) == Some(&active))
            .copied()
    }

    /// The name a workspace has, which for a numbered one is its number.
    #[must_use]
    pub fn workspace_name(&self, workspace: WorkspaceId) -> String {
        self.names
            .get(&workspace)
            .cloned()
            .unwrap_or_else(|| workspace.0.to_string())
    }

    /// The special workspace a monitor is showing, if any.
    #[must_use]
    pub fn special_on(&self, monitor: MonitorId) -> Option<WorkspaceId> {
        self.outputs
            .iter()
            .find(|output| output.monitor.id == monitor)?
            .special
    }

    /// Which windows the master layout holds as masters on `workspace`.
    ///
    /// Empty for a dwindle workspace, which has no master at all. What
    /// `layoutmsg addmaster` and `removemaster` change: two masters look
    /// like two windows in one column, and a rectangle alone cannot tell
    /// that from a stack.
    #[must_use]
    pub fn masters_on(&self, workspace: WorkspaceId) -> Vec<WindowId> {
        self.workspaces
            .get(&workspace)
            .map(|ws| ws.tiling.master_windows().to_vec())
            .unwrap_or_default()
    }

    /// Whether `workspace` is a special one, by Hyprland's own range.
    #[must_use]
    pub const fn is_special(workspace: WorkspaceId) -> bool {
        workspace.0 >= SPECIAL_START && workspace.0 <= -2
    }

    /// Record each dwindle split's direction for its current box.
    fn settle(&mut self) {
        let at: Vec<(WorkspaceId, Settings)> = self
            .workspaces
            .keys()
            .map(|&workspace| (workspace, self.settings_at(workspace)))
            .collect();
        let outputs = &self.outputs;
        for (workspace, settings) in at {
            let Some(ws) = self.workspaces.get_mut(&workspace) else {
                continue;
            };
            let area = Area::of(work_area(outputs, ws.monitor, &settings));
            ws.tiling.settle(area, &settings);
        }
    }

    /// Remove workspaces that are empty and not shown.
    fn prune(&mut self) {
        let outputs = &self.outputs;
        self.workspaces.retain(|id, ws| {
            !ws.is_empty()
                || outputs
                    .iter()
                    .any(|output| output.active == *id || output.special == Some(*id))
        });
    }

    /// The visible windows of what `output` shows.
    fn placements(&self, output: &Output, focus: Option<WindowId>) -> Vec<Placed> {
        let Some(ws) = self.workspaces.get(&output.active) else {
            return Vec::new();
        };
        // A workspace's own `gapsin`, `gapsout` and `bordersize`, where a
        // `workspace =` line set them; the general options otherwise.
        let settings = &self.settings_at(output.active);
        let area = geometry::work_area(&output.monitor, settings);
        let border = settings.border_size;
        let place =
            |window: WindowId, rect: Rect, border: i64, floating: bool, fullscreen: bool| Placed {
                window,
                rect,
                border,
                focused: focus == Some(window),
                floating,
                fullscreen,
            };
        if let Some((window, mode)) = ws.fullscreen {
            let floating = ws.floating.contains(&window);
            return vec![match mode {
                FullscreenMode::Fullscreen => place(window, output.monitor.rect, 0, floating, true),
                FullscreenMode::Maximized => place(
                    window,
                    geometry::client(area, area, settings),
                    border,
                    floating,
                    true,
                ),
            }];
        }
        // The monocle layout draws the workspace's focused window, so the
        // focus is part of what the slots are: `recalculate` reads the
        // index `focusTargetUpdate` keeps, and the focus is that index.
        let shown = self.recent_tiled(output.active);
        let mut windows: Vec<Placed> = ws
            .tiling
            .slots(Area::of(area), settings, shown)
            .into_iter()
            .map(|(window, slot)| {
                let rect = geometry::client(slot.round(), area, settings);
                // A group's slot shows whichever member is active; the head
                // is what the tree holds and may not be what is drawn.
                place(self.shown(window), rect, border, false, false)
            })
            .collect();
        let (x, y) = (output.monitor.rect.x, output.monitor.rect.y);
        windows.extend(ws.floating.iter().filter_map(|&window| {
            self.floating_rects
                .get(&window)
                .map(|rect| place(window, rect.translate(x, y), border, true, false))
        }));
        windows
    }
}

/// The work area of `monitor` among `outputs`, empty if it is not there.
/// `rect` scaled about its own middle, rounded to whole pixels.
///
/// `CWindowTarget::applyToWindow`'s `calcPos + (calcSize - calcSize *
/// factor) / 2`, which is what puts a scratchpad's window inside the slot
/// the layout gave it with a margin all round.
fn shrunk(rect: Rect, factor: f64) -> Rect {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a window's pixels are far inside f64's exact range"
    )]
    let scaled = |value: i64| (value as f64 * factor).round() as i64;
    let (width, height) = (scaled(rect.width), scaled(rect.height));
    Rect::new(
        rect.x.saturating_add(rect.width.saturating_sub(width) / 2),
        rect.y
            .saturating_add(rect.height.saturating_sub(height) / 2),
        width.max(1),
        height.max(1),
    )
}

fn work_area(outputs: &[Output], monitor: MonitorId, settings: &Settings) -> Rect {
    outputs
        .iter()
        .find(|output| output.monitor.id == monitor)
        .map(|output| geometry::work_area(&output.monitor, settings))
        .unwrap_or_default()
}

/// A window's box as Hyprland's direction searches see it,
/// `getWindowIdealBoundingBoxIgnoreReserved`: where the layout placed it,
/// grown out to the monitor's edge on each side where it meets the edge of
/// the work area, so that windows on two monitors side by side touch across
/// the gaps and reserved strips between them.
fn ideal_box(placed: Rect, monitor: Rect, work: Rect) -> Rect {
    let mut ideal = placed;
    if placed.y == work.y {
        ideal.y = monitor.y;
        ideal.height = ideal
            .height
            .saturating_add(work.y.saturating_sub(monitor.y));
    }
    if placed.x == work.x {
        ideal.x = monitor.x;
        ideal.width = ideal.width.saturating_add(work.x.saturating_sub(monitor.x));
    }
    if placed.right() == work.right() {
        ideal.width = ideal
            .width
            .saturating_add(monitor.right().saturating_sub(work.right()));
    }
    if placed.bottom() == work.bottom() {
        ideal.height = ideal
            .height
            .saturating_add(monitor.bottom().saturating_sub(work.bottom()));
    }
    ideal
}

/// The point one pixel past the middle of a box's edge in `direction`,
/// which Hyprland's `focalPointForDir` aims a moved window at.
fn focal_point(ideal: Rect, direction: Direction) -> (f64, f64) {
    let (x, y) = (ideal.x as f64, ideal.y as f64);
    let (width, height) = (ideal.width as f64, ideal.height as f64);
    match direction {
        Direction::Up => (x + width / 2.0, y - 1.0),
        Direction::Down => (x + width / 2.0, y + height + 1.0),
        Direction::Left => (x - 1.0, y + height / 2.0),
        Direction::Right => (x + width + 1.0, y + height / 2.0),
    }
}

/// Whether two lists of placements put the same windows in the same places,
/// ignoring focus, which [`Change::Focus`] reports.
fn same_geometry(a: &[Placed], b: &[Placed]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.window == b.window
                && a.rect == b.rect
                && a.border == b.border
                && a.floating == b.floating
                && a.fullscreen == b.fullscreen
        })
}
