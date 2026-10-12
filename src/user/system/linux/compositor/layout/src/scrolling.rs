//! The scrolling layout: Hyprland's `ScrollingAlgorithm.cpp` and its
//! `ScrollTapeController`.
//!
//! A workspace is a *tape* of columns wider than the screen, and the screen
//! is a window onto it. Each column is a share of the screen's width --
//! `scrolling:column_width`, a fraction -- and holds one or more windows
//! stacked down it. Moving the focus sideways scrolls the tape so the
//! column comes into view; moving it up and down walks the windows of the
//! column it is already on.
//!
//! What makes it worth having over dwindle: a column's width is its own, so
//! a person can keep a wide editor and a narrow terminal side by side and
//! scroll a third column in beside them without either of the first two
//! changing shape. Nothing in the dwindle or master layouts can do that.
//!
//! # The tape and the camera
//!
//! A column's *width* is a fraction of the work area's, which is what makes
//! it mean the same thing on any monitor; every other length is pixels, as
//! Hyprland's are. `offset` is where the screen's left edge is on the tape,
//! and `CScrollTapeController::calculateCameraOffset` is the one rule that
//! keeps it honest:
//!
//! * a tape narrower than the screen is *centred*, so two columns that
//!   together take half the screen sit in the middle of it rather than
//!   against the left edge;
//! * a tape wider than the screen never scrolls past its own start, unless
//!   `scrolling:focus_fit_method` is `center`, which is allowed to put
//!   empty space before the first column so that the first column can be
//!   centred.
//!
//! Bringing a column into view is one of two things, and the option says
//! which. `center` puts the column's middle in the screen's middle;
//! `fit` moves the tape the least it can to get the whole column on the
//! screen, and leaves it alone when the column is already there.
//!
//! # Where it departs from Hyprland
//!
//! `scrolling:direction` is read and only `right` is laid out: the other
//! three flip the tape's axis or its sense, which is the same arithmetic
//! with two signs changed and no test in this tree that could tell it was
//! wrong. `scrolling:follow_min_visible` is read and not used -- it decides
//! whether the focus may *stay* on a column that is only partly on the
//! screen, which needs a pointer. The per-window heights inside a column
//! are equal shares rather than a person's own, because nothing here
//! resizes a window with the mouse yet.

use crate::WindowId;
use crate::geometry::Area;
use crate::settings::{FitMethod, Settings};

/// The narrowest and widest a column may be, as fractions of the work
/// area: `MIN_COLUMN_WIDTH` and `MAX_COLUMN_WIDTH`.
const MIN_WIDTH: f64 = 0.1;
/// The same.
const MAX_WIDTH: f64 = 1.0;

/// One column of the tape.
#[derive(Debug, Clone, PartialEq)]
struct Column {
    /// Its windows, top to bottom.
    windows: Vec<WindowId>,
    /// Its width, as a fraction of the work area's.
    width: f64,
}

/// One workspace's scrolling layout.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Scrolling {
    columns: Vec<Column>,
    /// Where the screen's left edge is on the tape, in pixels.
    offset: f64,
    /// Whether `layoutmsg inhibit_scroll` has stopped the tape moving.
    inhibited: bool,
}

impl Scrolling {
    /// Add `new`.
    ///
    /// A new window makes a column of its own, beside the one the focused
    /// window is on, and the tape moves to bring it into view -- which is
    /// `newTarget` with nothing being dragged. A window with no focused
    /// neighbour goes at the end.
    pub(crate) fn insert(
        &mut self,
        new: WindowId,
        focused: Option<WindowId>,
        settings: &Settings,
        across: f64,
    ) {
        if self.contains(new) {
            return;
        }
        let width = settings.scrolling.column_width.clamp(MIN_WIDTH, MAX_WIDTH);
        let at = focused
            .and_then(|window| self.column_of(window))
            .map_or(self.columns.len(), |at| at.saturating_add(1));
        self.columns.insert(
            at.min(self.columns.len()),
            Column {
                windows: vec![new],
                width,
            },
        );
        self.bring_into_view(
            at.min(self.columns.len().saturating_sub(1)),
            settings,
            across,
        );
    }

    /// Remove `window`, and the column it was alone in.
    pub(crate) fn remove(&mut self, window: WindowId) {
        for column in &mut self.columns {
            column.windows.retain(|id| *id != window);
        }
        self.columns.retain(|column| !column.windows.is_empty());
    }

    /// Exchange two windows' places, or rename one to the other.
    pub(crate) fn swap(&mut self, a: WindowId, b: WindowId) {
        for column in &mut self.columns {
            for id in &mut column.windows {
                if *id == a {
                    *id = b;
                } else if *id == b {
                    *id = a;
                }
            }
        }
    }

    /// Whether `window` is on the tape.
    pub(crate) fn contains(&self, window: WindowId) -> bool {
        self.columns
            .iter()
            .any(|column| column.windows.contains(&window))
    }

    /// Every window, column by column and top to bottom within each.
    pub(crate) fn windows(&self) -> Vec<WindowId> {
        self.columns
            .iter()
            .flat_map(|column| column.windows.iter().copied())
            .collect()
    }

    /// Which column `window` is on.
    fn column_of(&self, window: WindowId) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.windows.contains(&window))
    }

    /// The width of column `at` in pixels, given a work area `across`
    /// pixels wide.
    ///
    /// One column takes the whole screen when
    /// `scrolling:fullscreen_on_one_column` says so, which is what stops a
    /// lone window sitting in a narrow strip with the desktop either side.
    fn width_of(&self, at: usize, across: f64, settings: &Settings) -> f64 {
        if settings.scrolling.fullscreen_on_one_column && self.columns.len() == 1 {
            return across;
        }
        self.columns
            .get(at)
            .map_or(0.0, |column| across * column.width)
    }

    /// Where column `at` starts along the tape, in pixels.
    fn start_of(&self, at: usize, across: f64, settings: &Settings) -> f64 {
        (0..at.min(self.columns.len()))
            .map(|before| self.width_of(before, across, settings))
            .sum()
    }

    /// How long the whole tape is, in pixels.
    fn extent(&self, across: f64, settings: &Settings) -> f64 {
        (0..self.columns.len())
            .map(|at| self.width_of(at, across, settings))
            .sum()
    }

    /// The offset the camera actually uses, which is
    /// `calculateCameraOffset`: a tape narrower than the screen is centred,
    /// and a tape wider than it never scrolls past its own start unless the
    /// fit method is `center`.
    fn camera(&self, across: f64, settings: &Settings) -> f64 {
        let extent = self.extent(across, settings);
        if extent < across {
            return ((extent - across) / 2.0).round();
        }
        if self.offset < 0.0 && settings.scrolling.focus_fit_method != FitMethod::Center {
            return 0.0;
        }
        self.offset
    }

    /// Put column `at` in the middle of the screen: `centerStrip`.
    fn centre(&mut self, at: usize, across: f64, settings: &Settings) {
        let start = self.start_of(at, across, settings);
        let width = self.width_of(at, across, settings);
        self.offset = start - (across - width) / 2.0;
    }

    /// Move the tape the least it can to get column `at` wholly on the
    /// screen, and leave it alone when the column is already there:
    /// `fitStrip`.
    fn fit(&mut self, at: usize, across: f64, settings: &Settings) {
        let start = self.start_of(at, across, settings);
        let width = self.width_of(at, across, settings);
        let low = start - across + width;
        let high = start;
        if low > high {
            // A column wider than the screen cannot be fitted, so it is
            // centred, which is what Hyprland does rather than clamping an
            // empty range.
            self.offset = start - (across - width) / 2.0;
            return;
        }
        self.offset = self.offset.clamp(low, high);
    }

    /// Bring column `at` into view the way `scrolling:focus_fit_method`
    /// says: `centerOrFitCol`.
    fn bring_into_view(&mut self, at: usize, settings: &Settings, across: f64) {
        match settings.scrolling.focus_fit_method {
            FitMethod::Center => self.centre(at, across, settings),
            FitMethod::Fit => self.fit(at, across, settings),
        }
    }

    /// Whether column `at` is on the screen at all, or wholly on it when
    /// `whole`: `isStripVisible`.
    fn visible(&self, at: usize, across: f64, settings: &Settings, whole: bool) -> bool {
        let start = self.start_of(at, across, settings);
        let end = start + self.width_of(at, across, settings);
        let (view_start, view_end) = (
            self.camera(across, settings),
            self.camera(across, settings) + across,
        );
        if whole {
            start >= view_start && end <= view_end
        } else {
            start < view_end && view_start < end
        }
    }

    /// Each window's box, when the workspace's work area is `area`.
    ///
    /// The tape runs along the work area's width and each column fills its
    /// height, sharing it between its windows. A column off the screen is
    /// left out: its rectangle would be outside the monitor and there is
    /// nothing to draw there.
    pub(crate) fn slots(
        &self,
        area: Area,
        settings: &Settings,
        shown: Option<WindowId>,
    ) -> Vec<(WindowId, Area)> {
        self.slots_culled(area, settings, shown, true)
    }

    /// Every window's box, the columns scrolled off the screen included,
    /// at the places the tape has them: what a listing such as `hyprctl
    /// clients` reports, which names a window the screen does not show.
    pub(crate) fn every_slot(&self, area: Area, settings: &Settings) -> Vec<(WindowId, Area)> {
        self.slots_culled(area, settings, None, false)
    }

    fn slots_culled(
        &self,
        area: Area,
        settings: &Settings,
        shown: Option<WindowId>,
        cull: bool,
    ) -> Vec<(WindowId, Area)> {
        let camera = self.camera(area.w, settings);
        let mut out = Vec::with_capacity(self.windows().len());
        for (at, column) in self.columns.iter().enumerate() {
            let width = self.width_of(at, area.w, settings);
            let x = area.x + self.start_of(at, area.w, settings) - camera;
            // Wholly off the screen: nothing to draw, and a rectangle
            // outside the monitor would confuse every direction search.
            if cull && (x + width <= area.x || x >= area.x + area.w) {
                continue;
            }
            let mut y = area.y;
            let count = column.windows.len();
            for (down, &window) in column.windows.iter().enumerate() {
                // The last window takes what is left, so a column of three
                // covers its height to the pixel.
                let height = if down + 1 == count {
                    area.y + area.h - y
                } else {
                    area.h / count as f64
                };
                out.push((
                    window,
                    Area {
                        x,
                        y,
                        w: width,
                        h: height,
                    },
                ));
                y += height;
            }
        }
        // A window the tape has scrolled past is still the focused one, and
        // a workspace that drew nothing would be a black screen; the
        // focused window's column is always brought into view before the
        // slots are asked for, so this only catches a caller that did not.
        if out.is_empty()
            && let Some(window) = shown.filter(|window| self.contains(*window))
        {
            out.push((window, area));
        }
        out
    }

    /// `layoutmsg` for this layout, which is the largest of the four's.
    ///
    /// Gives whether anything changed. `focused` is the focused window,
    /// which nearly every message acts on.
    pub(crate) fn message(
        &mut self,
        words: &[&str],
        focused: Option<WindowId>,
        settings: &Settings,
        across: f64,
    ) -> bool {
        let Some(&word) = words.first() else {
            return false;
        };
        let argument = words.get(1).copied().unwrap_or("");
        let at = focused.and_then(|window| self.column_of(window));
        match word {
            "move" => self.move_tape(argument, at, settings, across),
            "colresize" => self.resize_column(words, at, settings, across),
            "center" => at.is_some_and(|at| {
                self.centre(at, across, settings);
                true
            }),
            "fit_into_view" => at.is_some_and(|at| {
                self.bring_into_view(at, settings, across);
                true
            }),
            "fit" => self.fit_column(argument, at, settings, across),
            "swapcol" => self.swap_column(argument, at, settings, across),
            "promote" | "expel" | "consume" | "consume_or_expel" => {
                self.regroup(word, argument, focused, at, settings, across)
            }
            "inhibit_scroll" => {
                self.inhibited = match argument {
                    "" => !self.inhibited,
                    "0" | "false" => false,
                    _ => true,
                };
                true
            }
            // `focus` moves the focus, which is the compositor's and not
            // this layout's; `State` answers it with the same walk
            // `movefocus` uses.
            _ => false,
        }
    }

    /// `layoutmsg move +col`, `-col`, or a number of screens.
    fn move_tape(
        &mut self,
        argument: &str,
        at: Option<usize>,
        settings: &Settings,
        across: f64,
    ) -> bool {
        if self.inhibited {
            return false;
        }
        match argument {
            "+col" | "col" => {
                let next = at.map_or(0, |at| at.saturating_add(1));
                if next >= self.columns.len() {
                    // Past the end: the tape goes as far as it can, which
                    // is what `setOffset(maxWidth())` does.
                    self.offset = self.extent(across, settings);
                    return true;
                }
                self.bring_into_view(next, settings, across);
                true
            }
            "-col" => {
                let Some(previous) = at.and_then(|at| at.checked_sub(1)) else {
                    return false;
                };
                self.bring_into_view(previous, settings, across);
                true
            }
            other => {
                let Ok(by) = other.trim_start_matches('+').parse::<f64>() else {
                    return false;
                };
                // Hyprland's own sign: `adjustOffset(-value)`, so a
                // positive number moves the tape's content left, which
                // looks like moving *along* it.
                self.offset -= by;
                true
            }
        }
    }

    /// `layoutmsg colresize`: the focused column's width.
    fn resize_column(
        &mut self,
        words: &[&str],
        at: Option<usize>,
        settings: &Settings,
        across: f64,
    ) -> bool {
        let argument = words.get(1).copied().unwrap_or("");
        if argument == "all" {
            let Ok(width) = words.get(2).copied().unwrap_or("").parse::<f64>() else {
                return false;
            };
            for column in &mut self.columns {
                column.width = width.clamp(MIN_WIDTH, MAX_WIDTH);
            }
            return true;
        }
        let Some(at) = at else {
            return false;
        };
        let held = self.columns.get(at).map_or(0.0, |column| column.width);
        let wanted = match argument {
            // `+conf` and `-conf` step through the widths
            // `scrolling:explicit_column_widths` lists, wrapping around.
            "+conf" => next_configured(settings.scrolling.column_widths.as_slice(), held, false),
            "-conf" => next_configured(settings.scrolling.column_widths.as_slice(), held, true),
            text if text.starts_with(['+', '-']) => {
                let Ok(by) = text.parse::<f64>() else {
                    return false;
                };
                held + by
            }
            text => {
                let Ok(width) = text.parse::<f64>() else {
                    return false;
                };
                width
            }
        };
        if let Some(column) = self.columns.get_mut(at) {
            column.width = wanted.clamp(MIN_WIDTH, MAX_WIDTH);
        }
        self.bring_into_view(at, settings, across);
        true
    }

    /// `layoutmsg fit active` and `fit expand`.
    fn fit_column(
        &mut self,
        argument: &str,
        at: Option<usize>,
        settings: &Settings,
        across: f64,
    ) -> bool {
        let Some(at) = at else {
            return false;
        };
        match argument {
            "active" => {
                if let Some(column) = self.columns.get_mut(at) {
                    column.width = MAX_WIDTH;
                }
                self.offset = self.start_of(at, across, settings);
                true
            }
            "expand" => {
                // As much width as is left over from the other columns that
                // are on the screen, which is what `fit expand` means.
                let taken: f64 = self
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| {
                        *other != at && self.visible(*other, across, settings, false)
                    })
                    .map(|(_, column)| column.width)
                    .sum();
                if let Some(column) = self.columns.get_mut(at) {
                    column.width = (1.0 - taken).clamp(MIN_WIDTH, MAX_WIDTH);
                }
                self.bring_into_view(at, settings, across);
                true
            }
            _ => false,
        }
    }

    /// `layoutmsg swapcol l` and `r`.
    fn swap_column(
        &mut self,
        argument: &str,
        at: Option<usize>,
        settings: &Settings,
        across: f64,
    ) -> bool {
        let Some(at) = at else {
            return false;
        };
        let count = self.columns.len();
        if count < 2 {
            return false;
        }
        let wrap = settings.scrolling.wrap_swapcol;
        let to = match argument {
            "l" if at == 0 => {
                if wrap {
                    count - 1
                } else {
                    return false;
                }
            }
            "l" => at - 1,
            "r" if at + 1 == count => {
                if wrap {
                    0
                } else {
                    return false;
                }
            }
            "r" => at + 1,
            _ => return false,
        };
        self.columns.swap(at, to);
        self.bring_into_view(to, settings, across);
        true
    }

    /// `layoutmsg promote`, `expel`, `consume` and `consume_or_expel`:
    /// moving a window between columns.
    fn regroup(
        &mut self,
        word: &str,
        argument: &str,
        focused: Option<WindowId>,
        at: Option<usize>,
        settings: &Settings,
        across: f64,
    ) -> bool {
        let (Some(window), Some(at)) = (focused, at) else {
            return false;
        };
        let width = settings.scrolling.column_width.clamp(MIN_WIDTH, MAX_WIDTH);
        match word {
            // The focused window leaves its column for one of its own,
            // just before it.
            "promote" => self.expel(window, at, at, width, settings, across),
            // The *last* window of the column leaves it for one just
            // after.
            "expel" => {
                let Some(&last) = self
                    .columns
                    .get(at)
                    .filter(|column| column.windows.len() > 1)
                    .and_then(|column| column.windows.last())
                else {
                    return false;
                };
                self.expel(last, at, at.saturating_add(1), width, settings, across)
            }
            // The first window of the next column joins this one.
            "consume" => {
                let Some(&taken) = self
                    .columns
                    .get(at.saturating_add(1))
                    .and_then(|column| column.windows.first())
                else {
                    return false;
                };
                self.remove(taken);
                let at = self.column_of(window).unwrap_or(at);
                if let Some(column) = self.columns.get_mut(at) {
                    column.windows.push(taken);
                }
                self.bring_into_view(at, settings, across);
                true
            }
            // Out of a column that holds more than one window, into the
            // neighbour otherwise.
            _ => {
                let prev = argument == "prev";
                if !prev && argument != "next" {
                    return false;
                }
                let alone = self
                    .columns
                    .get(at)
                    .is_none_or(|column| column.windows.len() < 2);
                if !alone {
                    let to = if prev { at } else { at.saturating_add(1) };
                    return self.expel(window, at, to, width, settings, across);
                }
                let Some(neighbour) = (if prev {
                    at.checked_sub(1)
                } else {
                    Some(at.saturating_add(1)).filter(|to| *to < self.columns.len())
                }) else {
                    return false;
                };
                self.remove(window);
                let neighbour = if prev { neighbour } else { neighbour - 1 };
                if let Some(column) = self.columns.get_mut(neighbour) {
                    column.windows.push(window);
                }
                self.bring_into_view(neighbour, settings, across);
                true
            }
        }
    }

    /// Take `window` out of column `from` and give it a column of its own
    /// at `to`.
    fn expel(
        &mut self,
        window: WindowId,
        from: usize,
        to: usize,
        width: f64,
        settings: &Settings,
        across: f64,
    ) -> bool {
        if self
            .columns
            .get(from)
            .is_none_or(|column| column.windows.len() < 2)
        {
            return false;
        }
        self.remove(window);
        let to = to.min(self.columns.len());
        self.columns.insert(
            to,
            Column {
                windows: vec![window],
                width,
            },
        );
        self.bring_into_view(to, settings, across);
        true
    }

    /// The window beside `window` in `direction`, for `movefocus` and for
    /// `layoutmsg focus`.
    ///
    /// `l` and `r` step between columns and `u` and `d` within one, which
    /// is what `layoutMsg`'s `focus` does for a tape that runs sideways.
    /// `None` at either end, which the caller answers with
    /// `general:no_focus_fallback` or `scrolling:wrap_focus`.
    pub(crate) fn beside(
        &self,
        window: WindowId,
        right: bool,
        along: bool,
        settings: &Settings,
    ) -> Option<WindowId> {
        let at = self.column_of(window)?;
        if !along {
            // Within the column.
            let column = self.columns.get(at)?;
            let down = column.windows.iter().position(|id| *id == window)?;
            let wanted = if right {
                down.checked_add(1).filter(|to| *to < column.windows.len())
            } else {
                down.checked_sub(1)
            };
            return match wanted {
                Some(to) => column.windows.get(to).copied(),
                None if settings.no_focus_fallback => None,
                // Hyprland wraps within a column unless the fallback is
                // off, whatever `wrap_focus` says: that option is about
                // the *columns*.
                None if right => column.windows.first().copied(),
                None => column.windows.last().copied(),
            };
        }
        let wanted = if right {
            at.checked_add(1).filter(|to| *to < self.columns.len())
        } else {
            at.checked_sub(1)
        };
        let to = match wanted {
            Some(to) => to,
            None if settings.no_focus_fallback => return None,
            None if !settings.scrolling.wrap_focus => return None,
            None if right => 0,
            None => self.columns.len().checked_sub(1)?,
        };
        // The window of that column nearest the one being left, by its
        // place down the column: `findBestNeighbor`.
        let down = self
            .columns
            .get(at)?
            .windows
            .iter()
            .position(|id| *id == window)?;
        let column = self.columns.get(to)?;
        column
            .windows
            .get(down)
            .or_else(|| column.windows.last())
            .copied()
    }

    /// Bring the column `window` is on into view, which the caller does
    /// after the focus moves: `focusOnInput`.
    pub(crate) fn follow(&mut self, window: WindowId, settings: &Settings, across: f64) {
        if self.inhibited || !settings.scrolling.follow_focus {
            return;
        }
        let Some(at) = self.column_of(window) else {
            return;
        };
        // Only when the column is not already wholly on the screen, which
        // is `recalculate`'s own test: otherwise every click would jog the
        // tape.
        if !self.visible(at, across, settings, true) {
            self.bring_into_view(at, settings, across);
        }
    }
}

/// The next width in `widths` above `held`, or below it when `down`,
/// wrapping around: `colresize +conf` and `-conf`.
fn next_configured(widths: &[f64], held: f64, down: bool) -> f64 {
    if widths.is_empty() {
        return held;
    }
    if down {
        for &width in widths.iter().rev() {
            if width < held {
                return width;
            }
        }
        return widths.last().copied().unwrap_or(held);
    }
    for &width in widths {
        if width > held {
            return width;
        }
    }
    widths.first().copied().unwrap_or(held)
}
