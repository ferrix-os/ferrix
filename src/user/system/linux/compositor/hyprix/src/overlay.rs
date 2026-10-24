//! `debug:overlay`: the frames-per-second counter Hyprland draws over the
//! top left corner of the first monitor.
//!
//! A port of Hyprland 0.56.2's `src/debug/Overlay.cpp`, which is what a
//! person turns on with `debug { overlay = true }` or `hyprctl keyword
//! debug:overlay 1` to see how fast the compositor is drawing. For every
//! monitor that has drawn a frame it shows, in one blurred, rounded box:
//!
//! * the monitor's name;
//! * its frames per second over the last second's worth of frames, green
//!   above 95% of the refresh rate, yellow above 80% and red below;
//! * a graph of the last thirty seconds, one bar a second, each as tall and
//!   as green as that second came near the refresh rate;
//! * the average time between frames, and the time drawing one took with
//!   and without the overlay, each with how far apart the shortest and the
//!   longest were;
//! * the average time between animation ticks, and so ticks per second.
//!
//! and under the box, the warning Hyprland gives: a compositor that draws
//! only when something changes draws few frames over a still desktop, and
//! the counter says so rather than letting a person think it slow.
//!
//! The numbers are rewritten every 200 ms and a frame is asked for that
//! often, as Hyprland's `COverlay::draw` does, so the counter moves over a
//! desktop where nothing else does.
//!
//! What differs is the text. Hyprland lays its lines out with Pango in
//! `misc:font_family`; this compositor has no font renderer, and draws them
//! in Spleen 8x16, the bitmap face the kernel's panic screen uses
//! (`src/lib/kernel/fbtext`), at twice its size for the frames-per-second line as
//! Hyprland draws that line at 16 points against the others' 10. The
//! layout -- margins, gaps, the graph's bars, the colours -- is Hyprland's
//! own numbers.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use compositor_layout::Rect;
use compositor_render::{Blur, Color, Damage, Format, Painter, Rounding, Surface};

/// How often the numbers are rewritten, and a frame asked for:
/// `OVERLAY_REFRESH_INTERVAL_MS`.
pub(crate) const REFRESH: Duration = Duration::from_millis(200);

/// `OVERLAY_MARGIN_TOP`.
const MARGIN_TOP: i64 = 4;
/// `OVERLAY_MARGIN_LEFT`.
const MARGIN_LEFT: i64 = 4;
/// `OVERLAY_LINE_GAP`.
const LINE_GAP: i64 = 1;
/// `OVERLAY_MONITOR_GAP`.
const MONITOR_GAP: i64 = 5;
/// `OVERLAY_BOX_MARGIN`.
const BOX_MARGIN: i64 = 5;

/// `OVERLAY_FPS_GRAPH_HISTORY_SEC`: one bar a second, this many seconds.
const GRAPH_SECONDS: usize = 30;
/// `OVERLAY_FPS_GRAPH_BAR_WIDTH`.
const BAR_WIDTH: i64 = 3;
/// `OVERLAY_FPS_GRAPH_BAR_GAP`.
const BAR_GAP: i64 = 1;
/// `OVERLAY_FPS_GRAPH_HEIGHT`.
const GRAPH_HEIGHT: i64 = 22;
/// `OVERLAY_FPS_GRAPH_PADDING`.
const GRAPH_PADDING: i64 = 2;
/// `OVERLAY_FPS_GRAPH_GAP_TOP`.
const GRAPH_GAP_TOP: i64 = 2;

/// The box behind everything: `CHyprColor{0.1, 0.1, 0.1, 0.6}`, rounded 10
/// and blurred.
const BACKGROUND: Color = Color(0x991a_1a1a);
/// Its corners' radius.
const BACKGROUND_ROUNDING: i64 = 10;
/// The graph's own backdrop: black at `OVERLAY_FPS_GRAPH_BG_ALPHA`, 0.35,
/// rounded 2.
const GRAPH_BACKGROUND: Color = Color(0x5900_0000);
/// Its corners' radius.
const GRAPH_ROUNDING: i64 = 2;

/// White, for the name and the timings.
const WHITE: Color = Color(0xffff_ffff);
/// `CHyprColor{0.2, 1, 0.2}`: at least 95% of the refresh rate.
const GOOD: Color = Color(0xff33_ff33);
/// `CHyprColor{1, 1, 0.2}`: at least 80%.
const FAIR: Color = Color(0xffff_ff33);
/// `CHyprColor{1, 0.2, 0.2}`: below that.
const BAD: Color = Color(0xffff_3333);
/// `Colors::YELLOW`, the warning's colour.
const YELLOW: Color = Color(0xffff_ff00);

/// What Hyprland says under the box.
const WARNING: &str =
    "[!] FPS might be below your monitor's refresh rate if there are no content updates";

/// How big a line's letters are: Hyprland's font sizes, 10 and 16 points,
/// and the warning's 8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Size {
    /// Hyprland's 8 and 10: Spleen as it is, 8x16.
    Normal,
    /// Hyprland's 16: Spleen doubled, 16x32.
    Large,
}

impl Size {
    /// How many pixels one of Spleen's is, before the screen's scale.
    const fn times(self) -> i64 {
        match self {
            Self::Normal => 1,
            Self::Large => 2,
        }
    }
}

/// One line of text as it was last written.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    text: String,
    color: Color,
    size: Size,
}

/// The four figures `metricsFromSamples` gives of a list of samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Metric {
    avg: f32,
    min: f32,
    max: f32,
    /// How far the longest is from the shortest, which Hyprland calls the
    /// variance.
    var: f32,
}

impl Metric {
    /// `metricsFromSamples`: all zero for no samples.
    fn of(samples: &VecDeque<f32>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut metric = Self {
            min: f32::MAX,
            max: f32::MIN,
            ..Self::default()
        };
        for &sample in samples {
            metric.avg += sample;
            metric.min = metric.min.min(sample);
            metric.max = metric.max.max(sample);
        }
        metric.avg /= count(samples.len());
        metric.var = metric.max - metric.min;
        metric
    }
}

/// `n` as a float, for averages of a few hundred samples at most.
#[expect(
    clippy::cast_precision_loss,
    reason = "a count of samples is a few hundred at most, far inside f32's exact integers"
)]
const fn count(n: usize) -> f32 {
    n as f32
}

/// A duration in milliseconds, as Hyprland keeps its samples.
fn millis(duration: Duration) -> f32 {
    // Microseconds first, as Hyprland counts them, so that the float is
    // made from a whole number.
    let micros = u32::try_from(duration.as_micros()).unwrap_or(u32::MAX);
    #[expect(
        clippy::cast_precision_loss,
        reason = "a frame's microseconds are far below f32's 2^24 exact integers but for a stall, \
                  which is rounded"
    )]
    let micros = micros as f32;
    micros / 1000.0
}

/// Push `sample` onto `samples`, keeping the newest `limit`.
fn keep(samples: &mut VecDeque<f32>, sample: f32, limit: usize) {
    samples.push_back(sample);
    while samples.len() > limit {
        let _ = samples.pop_front();
    }
}

/// One monitor's samples and lines: `CMonitorOverlay`.
#[derive(Clone, Debug)]
struct Monitor {
    /// The connector's name.
    name: String,
    /// Its refresh rate, in hertz.
    refresh: f32,
    /// Milliseconds between one frame and the next.
    frametimes: VecDeque<f32>,
    /// Frames drawn in each of the last thirty whole seconds.
    fps_per_second: VecDeque<f32>,
    /// Milliseconds a frame took to draw.
    render_times: VecDeque<f32>,
    /// The same without the time the overlay itself took.
    render_times_no_overlay: VecDeque<f32>,
    /// Milliseconds between animation ticks, for the monitor ticks are
    /// counted on.
    animation_ticks: VecDeque<f32>,
    /// When the last frame began.
    last_frame: Option<Instant>,
    /// When the second being counted began, and how many frames it has had.
    second_start: Option<Instant>,
    frames_in_second: u32,
    /// The lines as they were last written.
    lines: Vec<Line>,
}

impl Monitor {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            refresh: 60.0,
            frametimes: VecDeque::new(),
            fps_per_second: VecDeque::new(),
            render_times: VecDeque::new(),
            render_times_no_overlay: VecDeque::new(),
            animation_ticks: VecDeque::new(),
            last_frame: None,
            second_start: None,
            frames_in_second: 0,
            lines: Vec::new(),
        }
    }

    /// The refresh rate, never below one.
    fn ideal(&self) -> f32 {
        self.refresh.max(1.0)
    }

    /// How many samples a list keeps: a second's worth at the refresh
    /// rate, `SAMPLELIMIT`.
    fn limit(&self) -> usize {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a refresh rate is a positive number of hertz in the hundreds at most"
        )]
        let limit = self.ideal().ceil() as usize;
        limit.max(1)
    }

    /// `CMonitorOverlay::frameData`: a frame of this monitor began at `now`.
    fn frame(&mut self, now: Instant) {
        let limit = self.limit();
        if let Some(last) = self.last_frame {
            keep(
                &mut self.frametimes,
                millis(now.saturating_duration_since(last)),
                limit,
            );
        }
        self.last_frame = Some(now);
        let start = *self.second_start.get_or_insert(now);
        self.frames_in_second = self.frames_in_second.saturating_add(1);
        let window = now.saturating_duration_since(start);
        if window >= Duration::from_secs(1) {
            let seconds = window.as_secs_f32();
            let frames = f32::from(u16::try_from(self.frames_in_second).unwrap_or(u16::MAX));
            let fps = if seconds > 0.0 { frames / seconds } else { 0.0 };
            let ideal = self.ideal();
            keep(
                &mut self.fps_per_second,
                fps.clamp(0.0, ideal),
                GRAPH_SECONDS,
            );
            self.frames_in_second = 0;
            self.second_start = Some(now);
        }
    }

    /// `CMonitorOverlay::rebuildCache`: write the lines from the samples.
    fn rewrite(&mut self) {
        let frames = Metric::of(&self.frametimes);
        let render = Metric::of(&self.render_times);
        let no_overlay = Metric::of(&self.render_times_no_overlay);
        let ticks = Metric::of(&self.animation_ticks);
        let fps = if frames.avg <= 0.0 {
            0.0
        } else {
            1000.0 / frames.avg
        };
        let tps = if ticks.avg <= 0.0 {
            0.0
        } else {
            1000.0 / ticks.avg
        };
        let line = |text: String, color: Color, size: Size| Line { text, color, size };
        self.lines = vec![
            line(self.name.clone(), WHITE, Size::Normal),
            line(
                format!("{} FPS", fps.round()),
                fps_color(fps, self.ideal()),
                Size::Large,
            ),
            line(
                format!(
                    "Avg Frametime: {:.2}ms (var {:.2}ms)",
                    frames.avg, frames.var
                ),
                WHITE,
                Size::Normal,
            ),
            line(
                format!(
                    "Avg Rendertime: {:.2}ms (var {:.2}ms)",
                    render.avg, render.var
                ),
                WHITE,
                Size::Normal,
            ),
            line(
                format!(
                    "Avg Rendertime (No Overlay): {:.2}ms (var {:.2}ms)",
                    no_overlay.avg, no_overlay.var
                ),
                WHITE,
                Size::Normal,
            ),
            line(
                format!(
                    "Avg Anim Tick: {:.2}ms (var {:.2}ms) ({tps:.2} TPS)",
                    ticks.avg, ticks.var
                ),
                WHITE,
                Size::Normal,
            ),
        ];
    }
}

/// The frames-per-second line's colour: green from 95% of the refresh rate,
/// yellow from 80%, red below.
fn fps_color(fps: f32, ideal: f32) -> Color {
    if fps > ideal * 0.95 {
        GOOD
    } else if fps > ideal * 0.8 {
        FAIR
    } else {
        BAD
    }
}

/// Every monitor's samples, and when the overlay was last rewritten:
/// `COverlay`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Overlay {
    /// In the order of the screens, which is Hyprland's order of monitors.
    monitors: Vec<Monitor>,
    /// When the lines were last written.
    rewritten: Option<Instant>,
    /// When a frame was last asked for, which is apart from the writing: a
    /// first screen that is off draws no counter, and the loop must not
    /// spin asking it to.
    asked: Option<Instant>,
    /// Counts the rewrites, so that the damage sees a box whose text has
    /// changed though its size has not.
    generation: u64,
    /// When the animations last ticked, and how long before that the tick
    /// before was: `m_lastTickTimeMs`.
    last_tick: Option<Instant>,
    last_tick_ms: f32,
}

impl Overlay {
    /// The monitor called `name`, made the first time it is asked for.
    fn monitor(&mut self, name: &str) -> Option<&mut Monitor> {
        if !self.monitors.iter().any(|one| one.name == name) {
            self.monitors.push(Monitor::new(name));
        }
        self.monitors.iter_mut().find(|one| one.name == name)
    }

    /// Put the monitors in the screens' order and forget any that is gone:
    /// the box lists them as the compositor has them.
    pub(crate) fn screens<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        let mut ordered = Vec::with_capacity(self.monitors.len());
        for name in names {
            if let Some(at) = self.monitors.iter().position(|one| one.name == name) {
                ordered.push(self.monitors.swap_remove(at));
            }
        }
        self.monitors = ordered;
    }

    /// The animations ticked at `now`: `CHyprAnimationManager::tick`, which
    /// keeps how long it was since the last.
    pub(crate) fn tick(&mut self, now: Instant) {
        if let Some(last) = self.last_tick {
            self.last_tick_ms = millis(now.saturating_duration_since(last));
        }
        self.last_tick = Some(now);
    }

    /// A frame of the screen called `name`, `first` of them all or not,
    /// whose refresh is `millihertz`, began at `now`: `frameData`. The
    /// first screen also keeps the animations' last tick, as Hyprland's
    /// monitor for ticks does.
    pub(crate) fn frame(&mut self, name: &str, millihertz: i32, first: bool, now: Instant) {
        let tick = self.last_tick_ms;
        let Some(monitor) = self.monitor(name) else {
            return;
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a refresh rate in millihertz is far inside f32's exact integers"
        )]
        let hertz = millihertz.max(1) as f32 / 1000.0;
        monitor.refresh = hertz;
        monitor.frame(now);
        if first {
            let limit = monitor.limit();
            keep(&mut monitor.animation_ticks, tick, limit);
        }
    }

    /// The frame of the screen called `name` took `took` to draw, of which
    /// `overlay` was the overlay's own: `renderData` and
    /// `renderDataNoOverlay`.
    pub(crate) fn rendered(&mut self, name: &str, took: Duration, overlay: Duration) {
        let Some(monitor) = self.monitor(name) else {
            return;
        };
        let limit = monitor.limit();
        keep(&mut monitor.render_times, millis(took), limit);
        keep(
            &mut monitor.render_times_no_overlay,
            millis(took.saturating_sub(overlay)),
            limit,
        );
    }

    /// Whether a frame is owed at `now` so that the numbers move, and if
    /// so, that it has been asked for: every [`REFRESH`], as
    /// `COverlay::draw` schedules one.
    pub(crate) fn ask(&mut self, now: Instant) -> bool {
        let due = self
            .asked
            .is_none_or(|at| now.saturating_duration_since(at) >= REFRESH);
        if due {
            self.asked = Some(now);
        }
        due
    }

    /// How long until [`Overlay::ask`] owes the next frame, for the loop's
    /// wait.
    pub(crate) fn wait(&self, now: Instant) -> Duration {
        self.asked.map_or(Duration::ZERO, |at| {
            REFRESH.saturating_sub(now.saturating_duration_since(at))
        })
    }

    /// The overlay as the first screen draws it at `now`, at `scale` buffer
    /// pixels a logical one: `COverlay::draw`, with the lines rewritten if
    /// a frame has been asked for since they last were.
    pub(crate) fn picture(&mut self, now: Instant, scale: f64) -> Picture {
        // Rewritten once for each frame [`Overlay::ask`] asked for, rather
        // than by its own clock: a frame drawn a little sooner after its
        // asking than the last was would otherwise find the lines a hair
        // under [`REFRESH`] old and leave them for another.
        let asked = self.asked;
        let stale = self
            .rewritten
            .is_none_or(|at| asked.is_some_and(|asked| asked > at));
        if stale {
            for monitor in &mut self.monitors {
                monitor.rewrite();
            }
            self.rewritten = Some(now);
            self.generation = self.generation.wrapping_add(1);
        }
        lay_out(&self.monitors, self.generation, scale)
    }
}

/// Something the overlay draws, in the first screen's own pixels; the
/// trusted prompt draws with the same marks (`crate::prompt`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// A box blurred behind and filled, corners cut.
    Panel {
        rect: Rect,
        radius: i64,
        color: Color,
        blurred: bool,
    },
    /// A rectangle of one colour, square.
    Fill { rect: Rect, color: Color },
    /// A line of text: its pixels, premultiplied ARGB, and where they go.
    Text { rect: Rect, pixels: Vec<u8> },
}

/// Everything the overlay draws in one frame, and what the damage compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    marks: Vec<Mark>,
    /// What a frame's damage compares with the last frame's.
    pub stamp: Stamp,
}

/// Where the overlay is and which rewrite of it: a frame owes its old place
/// and its new one when either changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Everything it covers.
    pub rect: Rect,
    /// Which rewrite of the lines it shows.
    pub generation: u64,
}

impl Picture {
    /// A picture of `marks`, which cover `stamp.rect`.
    #[cfg(feature = "settings-prompt")]
    pub(crate) const fn of(marks: Vec<Mark>, stamp: Stamp) -> Picture {
        Picture { marks, stamp }
    }

    /// The boxes whose blur reads what is behind them, which the damage
    /// redraws whole when it touches them.
    pub(crate) fn blurred(&self) -> impl Iterator<Item = Rect> + '_ {
        self.marks.iter().filter_map(|mark| match mark {
            Mark::Panel {
                rect,
                blurred: true,
                ..
            } => Some(*rect),
            _ => None,
        })
    }

    /// Draw it with `painter` inside `damage`, blurring the boxes with
    /// `blur` when blur is on.
    pub(crate) fn paint<P: Painter>(&self, painter: &mut P, blur: Option<&Blur>, damage: &Damage) {
        for mark in &self.marks {
            match mark {
                Mark::Panel {
                    rect,
                    radius,
                    color,
                    blurred,
                } => {
                    let rounding = Rounding {
                        radius: *radius,
                        power: Rounding::POWER,
                    };
                    if *blurred && let Some(blur) = blur {
                        painter.blur(*rect, rounding, blur, damage);
                    }
                    painter.fill_rounded(*rect, rounding, *color, damage);
                }
                Mark::Fill { rect, color } => painter.fill(*rect, *color, damage),
                Mark::Text { rect, pixels } => {
                    let (Ok(width), Ok(height)) =
                        (u32::try_from(rect.width), u32::try_from(rect.height))
                    else {
                        continue;
                    };
                    if let Ok(surface) = Surface::new(
                        pixels,
                        width,
                        height,
                        width.saturating_mul(4),
                        Format::Argb8888,
                    ) {
                        painter.composite(&surface, *rect, damage);
                    }
                }
            }
        }
    }
}

/// Lay the overlay out: `COverlay::draw` and `CMonitorOverlay::draw`, in
/// Hyprland's logical pixels, then multiplied up to the screen's.
fn lay_out(monitors: &[Monitor], generation: u64, scale: f64) -> Picture {
    // The whole numbers of the layout become buffer pixels by one factor, so
    // Spleen's pixels stay square and whole at any scale.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a monitor's scale is a small positive number"
    )]
    let times = (scale.round() as i64).max(1);
    let px = |value: i64| value.saturating_mul(times);
    let mut marks = Vec::new();
    let mut monitors_drawn: Vec<Vec<Mark>> = Vec::new();
    // Each monitor's box, `lastDrawnBox`, and how far down the next begins.
    let mut sizes: Vec<(i64, i64)> = Vec::new();
    let mut offset = 0i64;
    for monitor in monitors.iter().filter(|monitor| !monitor.lines.is_empty()) {
        let mut own = Vec::new();
        let left = MARGIN_LEFT + BOX_MARGIN;
        let mut y = offset + MARGIN_TOP + BOX_MARGIN;
        let mut widest = 0i64;
        for (index, line) in monitor.lines.iter().enumerate() {
            let (text, width, height) = text(&line.text, line.color, line.size, times);
            own.push(Mark::Text {
                rect: Rect::new(px(left), px(y), width, height),
                pixels: text,
            });
            widest = widest.max(width / times);
            y += height / times + LINE_GAP;
            if index == 1 {
                let graph = graph(
                    (left, y + GRAPH_GAP_TOP),
                    monitor.ideal(),
                    &monitor.fps_per_second,
                    times,
                );
                widest = widest.max(graph.0);
                y = graph.1 + LINE_GAP;
                own.extend(graph.2);
            }
        }
        let height = y - offset - MARGIN_TOP - BOX_MARGIN;
        if widest > 0 && height > 0 {
            sizes.push((widest + 2, height + 2));
            monitors_drawn.push(own);
        }
        offset += (y - offset) + MONITOR_GAP;
    }
    offset -= MONITOR_GAP;
    // The box behind them all, sized to hold every monitor's.
    let mut full = (0i64, 0i64);
    for &(width, height) in &sizes {
        full.0 = full.0.max(width);
        full.1 += height + MONITOR_GAP;
    }
    if let Some(stacked) = i64::try_from(sizes.len()).ok().filter(|&n| n > 0) {
        full.1 -= MONITOR_GAP;
        full.1 += (stacked - 1) * (MARGIN_TOP + BOX_MARGIN - 2);
    }
    let mut covered: Option<Rect> = None;
    let mut cover = |rect: Rect| {
        covered = Some(covered.map_or(rect, |so_far| union(so_far, rect)));
    };
    if full.0 > 1 && full.1 > 1 {
        let rect = Rect::new(
            px(MARGIN_LEFT),
            px(MARGIN_TOP),
            px(full.0 + BOX_MARGIN * 2),
            px(full.1 + BOX_MARGIN * 2),
        );
        cover(rect);
        marks.push(Mark::Panel {
            rect,
            radius: px(BACKGROUND_ROUNDING),
            color: BACKGROUND,
            blurred: true,
        });
        for own in monitors_drawn {
            marks.extend(own);
        }
        // The warning, wrapped to the box's width, in a box of its own
        // under it.
        let lines = wrap(WARNING, usize::try_from(full.0 / 8).unwrap_or(1).max(1));
        let high = i64::try_from(lines.len()).unwrap_or(0) * 16;
        let wide = lines
            .iter()
            .map(|line| i64::try_from(line.len()).unwrap_or(0) * 8)
            .max()
            .unwrap_or(0);
        let top = offset + MARGIN_TOP * 2 + BOX_MARGIN;
        let rect = Rect::new(
            px(MARGIN_LEFT),
            px(top),
            px(full.0 + BOX_MARGIN * 2),
            px(high + BOX_MARGIN * 2),
        );
        cover(rect);
        marks.push(Mark::Panel {
            rect,
            radius: px(BACKGROUND_ROUNDING),
            color: BACKGROUND,
            blurred: true,
        });
        let left = MARGIN_LEFT + (full.0 - wide) / 2;
        for (row, line) in lines.iter().enumerate() {
            let (pixels, width, height) = text(line, YELLOW, Size::Normal, times);
            let down = i64::try_from(row).unwrap_or(0) * 16;
            marks.push(Mark::Text {
                rect: Rect::new(px(left), px(top + BOX_MARGIN + down), width, height),
                pixels,
            });
        }
    }
    Picture {
        marks,
        stamp: Stamp {
            rect: covered.unwrap_or_else(|| Rect::new(0, 0, 0, 0)),
            generation,
        },
    }
}

/// The smallest rectangle holding both.
fn union(one: Rect, other: Rect) -> Rect {
    let left = one.x.min(other.x);
    let top = one.y.min(other.y);
    let right = one.right().max(other.right());
    let bottom = one.bottom().max(other.bottom());
    Rect::new(left, top, right - left, bottom - top)
}

/// Break `text` at spaces into lines of at most `columns` characters, as
/// Pango wraps the warning to the box's width. A word longer than a line is
/// a line of its own.
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        if !current.is_empty() && current.len() + 1 + word.len() > columns {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// `drawFPSGraph`: the graph's backdrop and one bar a second, the newest on
/// the right, at `(x, y)` in logical pixels. Its width, the bottom's `y`,
/// and the marks.
fn graph(at: (i64, i64), ideal: f32, history: &VecDeque<f32>, times: i64) -> (i64, i64, Vec<Mark>) {
    let px = |value: i64| value.saturating_mul(times);
    let seconds = i64::try_from(GRAPH_SECONDS).unwrap_or(30);
    let inner_width = seconds * BAR_WIDTH + (seconds - 1) * BAR_GAP;
    let width = inner_width + GRAPH_PADDING * 2;
    let height = GRAPH_HEIGHT + GRAPH_PADDING * 2;
    let mut marks = vec![Mark::Panel {
        rect: Rect::new(px(at.0), px(at.1), px(width), px(height)),
        radius: px(GRAPH_ROUNDING),
        color: GRAPH_BACKGROUND,
        blurred: false,
    }];
    let bars = history.len().min(GRAPH_SECONDS);
    let blank = GRAPH_SECONDS - bars;
    for (bar, &fps) in history.iter().skip(history.len() - bars).enumerate() {
        let normalized = (fps / ideal).clamp(0.0, 1.0);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a bar's height is at most the graph's 22 pixels"
        )]
        let bar_height = ((normalized * 22.0).round() as i64).max(1);
        let slot = i64::try_from(blank + bar).unwrap_or(0);
        let x = at.0 + GRAPH_PADDING + slot * (BAR_WIDTH + BAR_GAP);
        let y = at.1 + GRAPH_PADDING + (GRAPH_HEIGHT - bar_height);
        marks.push(Mark::Fill {
            rect: Rect::new(px(x), px(y), px(BAR_WIDTH), px(bar_height)),
            color: bar_color(normalized),
        });
    }
    (width, at.1 + height, marks)
}

/// `fpsBarColor`: from red to green through Oklab, as far as `normalized`.
fn bar_color(normalized: f32) -> Color {
    let bad = oklab(BAD);
    let good = oklab(GOOD);
    let mix = |from: f32, to: f32| from + (to - from) * normalized;
    srgb([
        mix(bad[0], good[0]),
        mix(bad[1], good[1]),
        mix(bad[2], good[2]),
    ])
}

/// A colour's channels as sRGB fractions.
fn fractions(color: Color) -> [f32; 3] {
    [color.red(), color.green(), color.blue()].map(|channel| f32::from(channel) / 255.0)
}

/// sRGB's transfer function, undone.
fn linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// And done.
fn gamma(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// An sRGB colour in Oklab, Björn Ottosson's matrices.
fn oklab(color: Color) -> [f32; 3] {
    let [r, g, b] = fractions(color).map(linear);
    let l = 0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b;
    let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
    let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;
    let [l, m, s] = [l.cbrt(), m.cbrt(), s.cbrt()];
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// An Oklab colour back in opaque sRGB.
fn srgb([big_l, a, b]: [f32; 3]) -> Color {
    let l = big_l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m = big_l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s = big_l - 0.089_484_18 * a - 1.291_485_5 * b;
    let [l, m, s] = [l * l * l, m * m * m, s * s * s];
    let rgb = [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ];
    let [r, g, b] = rgb.map(|value| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 0..=255 first"
        )]
        let byte = (gamma(value.clamp(0.0, 1.0)) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u32;
        byte
    });
    Color(0xff00_0000 | (r << 16) | (g << 8) | b)
}

/// `text` in Spleen, `size` and `times` over, in `color`: the pixels,
/// premultiplied ARGB, and their width and height.
fn text(text: &str, color: Color, size: Size, times: i64) -> (Vec<u8>, i64, i64) {
    let scale = usize::try_from(size.times().saturating_mul(times)).unwrap_or(1);
    let glyph_width = ferrix_fbtext::GLYPH_WIDTH * scale;
    let glyph_height = ferrix_fbtext::GLYPH_HEIGHT * scale;
    let width = text.chars().count() * glyph_width;
    let mut pixels = vec![0u8; width * glyph_height * 4];
    // Opaque, so premultiplied is the colour itself; ARGB8888 is B, G, R, A
    // in memory.
    let bytes = [color.blue(), color.green(), color.red(), 0xff];
    for (column, character) in text.chars().enumerate() {
        for (row, bits) in ferrix_fbtext::glyph(character).iter().enumerate() {
            let lit = (0..ferrix_fbtext::GLYPH_WIDTH).filter(|bit| bits & (0x80 >> bit) != 0);
            for bit in lit {
                // One of Spleen's pixels is a `scale`-sided square.
                let x = column * glyph_width + bit * scale;
                for y in row * scale..(row + 1) * scale {
                    let at = (y * width + x) * 4;
                    let run = pixels.get_mut(at..at + scale * 4).unwrap_or_default();
                    run.chunks_exact_mut(4)
                        .for_each(|pixel| pixel.copy_from_slice(&bytes));
                }
            }
        }
    }
    (
        pixels,
        i64::try_from(width).unwrap_or(0),
        i64::try_from(glyph_height).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests;
