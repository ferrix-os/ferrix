//! The counter's arithmetic against Hyprland's, and its picture on a real
//! canvas.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use compositor_render::{Canvas, Damage};

use super::{
    BAD, BOX_MARGIN, FAIR, GOOD, MARGIN_LEFT, MARGIN_TOP, Mark, Metric, Overlay, REFRESH, Size,
    WARNING, bar_color, fps_color, text, wrap,
};

/// Sixty hertz, as `wl_output` says it.
const SIXTY: i32 = 60_000;

/// `frames` frames of the screen `name` at sixty a second, each taking
/// `took` to draw, from `start`. The instant after the last.
fn frames(
    overlay: &mut Overlay,
    name: &str,
    start: Instant,
    frames: u32,
    took: Duration,
) -> Instant {
    let period = Duration::from_micros(16_667);
    let mut now = start;
    for _ in 0..frames {
        overlay.frame(name, SIXTY, true, now);
        overlay.rendered(name, took, took / 4);
        now += period;
    }
    now
}

/// The text of every line of the picture, in drawing order: which lines
/// were written, read back from what `rewrite` put in each monitor.
fn lines(overlay: &Overlay) -> Vec<String> {
    overlay
        .monitors
        .iter()
        .flat_map(|monitor| monitor.lines.iter().map(|line| line.text.clone()))
        .collect()
}

#[test]
fn metrics_are_hyprlands_average_and_spread() {
    let samples: VecDeque<f32> = [10.0, 20.0, 30.0].into_iter().collect();
    let metric = Metric::of(&samples);
    assert!((metric.avg - 20.0).abs() < 1e-4, "{metric:?}");
    assert!((metric.var - 20.0).abs() < 1e-4, "{metric:?}");
    assert_eq!(Metric::of(&VecDeque::new()), Metric::default());
}

/// Green above 95% of the refresh rate, yellow above 80%, red below, as
/// `rebuildCache` colours the line.
#[test]
fn the_rate_is_coloured_by_how_near_the_refresh_it_is() {
    assert_eq!(fps_color(58.0, 60.0), GOOD);
    assert_eq!(fps_color(57.0, 60.0), FAIR, "95% is not above 95%");
    assert_eq!(fps_color(49.0, 60.0), FAIR);
    assert_eq!(fps_color(48.0, 60.0), BAD, "80% is not above 80%");
    assert_eq!(fps_color(0.0, 60.0), BAD);
}

/// A second of frames at sixty a second reads as sixty, in green, with the
/// frame time and the render times Hyprland's lines give them.
#[test]
fn a_second_at_sixty_reads_sixty() {
    let mut overlay = Overlay::default();
    let start = Instant::now();
    let now = frames(
        &mut overlay,
        "Virtual-1",
        start,
        61,
        Duration::from_millis(4),
    );
    assert!(overlay.ask(now));
    let _ = overlay.picture(now, 1.0);
    assert_eq!(
        lines(&overlay),
        [
            "Virtual-1",
            "60 FPS",
            "Avg Frametime: 16.67ms (var 0.00ms)",
            "Avg Rendertime: 4.00ms (var 0.00ms)",
            "Avg Rendertime (No Overlay): 3.00ms (var 0.00ms)",
            "Avg Anim Tick: 0.00ms (var 0.00ms) (0.00 TPS)",
        ]
    );
    let monitor = overlay.monitors.first().expect("the screen");
    assert_eq!(monitor.lines.get(1).map(|line| line.color), Some(GOOD));
    // A second's worth of samples, and no more.
    assert_eq!(monitor.frametimes.len(), 60);
    // One whole second counted for the graph, at the refresh rate.
    assert_eq!(monitor.fps_per_second.len(), 1);
    let first = monitor.fps_per_second.front().copied().unwrap_or(0.0);
    assert!((first - 60.0).abs() < 1.0, "{first}");
}

/// Ticks are the time between them, and so a rate.
#[test]
fn animation_ticks_are_the_time_between_them() {
    let mut overlay = Overlay::default();
    let start = Instant::now();
    overlay.tick(start);
    overlay.tick(start + Duration::from_millis(10));
    let now = frames(
        &mut overlay,
        "Virtual-1",
        start,
        3,
        Duration::from_millis(1),
    );
    assert!(overlay.ask(now));
    let _ = overlay.picture(now, 1.0);
    assert_eq!(
        lines(&overlay).last().map(String::as_str),
        Some("Avg Anim Tick: 10.00ms (var 0.00ms) (100.00 TPS)")
    );
}

/// A frame is owed every 200 ms and not between, and the lines are written
/// once for each: the numbers move and do not flicker.
#[test]
fn a_frame_is_asked_for_every_refresh_and_the_lines_follow_it() {
    let mut overlay = Overlay::default();
    let start = Instant::now();
    let _ = frames(
        &mut overlay,
        "Virtual-1",
        start,
        2,
        Duration::from_millis(1),
    );
    assert!(overlay.ask(start), "the first is owed at once");
    assert!(!overlay.ask(start + Duration::from_millis(100)));
    assert_eq!(
        overlay.wait(start + Duration::from_millis(150)),
        Duration::from_millis(50)
    );
    let one = overlay.picture(start + Duration::from_millis(5), 1.0);
    // A frame drawn for another reason before the next asking shows the
    // same numbers.
    let again = overlay.picture(start + Duration::from_millis(100), 1.0);
    assert_eq!(one.stamp, again.stamp);
    assert!(overlay.ask(start + REFRESH));
    // Drawn a little sooner after its asking than the first was: rewritten
    // all the same.
    let next = overlay.picture(start + REFRESH + Duration::from_millis(1), 1.0);
    assert_eq!(next.stamp.generation, one.stamp.generation + 1);
}

/// The box is where Hyprland puts it: its first line at the margins, the
/// box behind at the top left, and all of it doubled on a screen at scale
/// two.
#[test]
fn the_box_is_at_the_top_left_and_scales_whole() {
    for (scale, times) in [(1.0, 1), (2.0, 2)] {
        let mut overlay = Overlay::default();
        let start = Instant::now();
        let now = frames(
            &mut overlay,
            "Virtual-1",
            start,
            3,
            Duration::from_millis(1),
        );
        assert!(overlay.ask(now));
        let picture = overlay.picture(now, scale);
        let Some(Mark::Panel { rect, .. }) = picture.marks.first() else {
            panic!("the box comes first: {:?}", picture.marks.first());
        };
        assert_eq!((rect.x, rect.y), (MARGIN_LEFT * times, MARGIN_TOP * times));
        let Some(Mark::Text { rect: name, .. }) = picture.marks.get(1) else {
            panic!("the name comes next");
        };
        let at = (MARGIN_LEFT + BOX_MARGIN) * times;
        assert_eq!((name.x, name.y), (at, (MARGIN_TOP + BOX_MARGIN) * times));
        assert_eq!(name.height, 16 * times, "Spleen's 16 rows, scaled");
        // What the damage compares covers everything drawn.
        for mark in &picture.marks {
            let (Mark::Panel { rect, .. } | Mark::Fill { rect, .. } | Mark::Text { rect, .. }) =
                mark;
            let stamp = picture.stamp.rect;
            assert!(
                rect.x >= stamp.x
                    && rect.y >= stamp.y
                    && rect.right() <= stamp.right()
                    && rect.bottom() <= stamp.bottom(),
                "{rect:?} outside {stamp:?}"
            );
        }
    }
}

/// Two screens are two sections of one box, in the screens' order.
#[test]
fn every_screen_has_its_section_in_order() {
    let mut overlay = Overlay::default();
    let start = Instant::now();
    let _ = frames(&mut overlay, "HDMI-A-1", start, 3, Duration::from_millis(1));
    let now = frames(
        &mut overlay,
        "Virtual-1",
        start,
        3,
        Duration::from_millis(1),
    );
    overlay.screens(["Virtual-1", "HDMI-A-1"].into_iter());
    assert!(overlay.ask(now));
    let _ = overlay.picture(now, 1.0);
    let names: Vec<String> = overlay
        .monitors
        .iter()
        .map(|monitor| monitor.name.clone())
        .collect();
    assert_eq!(names, ["Virtual-1", "HDMI-A-1"]);
    // A screen that went is forgotten.
    overlay.screens(["HDMI-A-1"].into_iter());
    assert_eq!(overlay.monitors.len(), 1);
}

#[test]
fn text_is_spleen_at_one_size_or_two() {
    let (pixels, width, height) = text("A", GOOD, Size::Normal, 1);
    assert_eq!((width, height), (8, 16));
    let lit: Vec<&[u8; 4]> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] == 0xff)
        .collect();
    assert!(!lit.is_empty(), "an A covers something");
    assert!(
        lit.iter().all(|pixel| **pixel == [0x33, 0xff, 0x33, 0xff]),
        "every lit pixel is the colour, as B G R A"
    );
    let (large, width, height) = text("A", GOOD, Size::Large, 1);
    assert_eq!((width, height), (16, 32));
    let count = |bytes: &[u8]| {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[3] == 0xff)
            .count()
    };
    assert_eq!(
        count(&large),
        count(&pixels) * 4,
        "every pixel doubled both ways"
    );
}

#[test]
fn the_warning_wraps_at_spaces_to_the_box() {
    let lines = wrap(WARNING, 30);
    assert!(lines.len() > 1);
    assert!(lines.iter().all(|line| line.len() <= 30), "{lines:?}");
    assert_eq!(lines.join(" "), WARNING);
}

/// The graph's ends are Hyprland's two colours.
#[test]
fn a_bar_runs_from_red_to_green() {
    let near = |one: compositor_render::Color, other: compositor_render::Color| {
        [
            (one.red(), other.red()),
            (one.green(), other.green()),
            (one.blue(), other.blue()),
        ]
        .iter()
        .all(|(a, b)| a.abs_diff(*b) <= 1)
    };
    assert!(near(bar_color(0.0), BAD), "{:?}", bar_color(0.0));
    assert!(near(bar_color(1.0), GOOD), "{:?}", bar_color(1.0));
    let middle = bar_color(0.5);
    assert!(middle.red() > 0x33 && middle.green() > 0x33, "{middle:?}");
}

/// Painted on a canvas, the box darkens what is under it and the rate is
/// written in green.
#[test]
fn painted_it_darkens_the_corner_and_writes_in_green() {
    let mut canvas = Canvas::new(640, 200).expect("a canvas");
    let everything = Damage::full(640, 200);
    compositor_render::Painter::clear(
        &mut canvas,
        compositor_render::Color(0xffff_ffff),
        &everything,
    );
    let mut overlay = Overlay::default();
    let start = Instant::now();
    let now = frames(
        &mut overlay,
        "Virtual-1",
        start,
        61,
        Duration::from_millis(1),
    );
    assert!(overlay.ask(now));
    let picture = overlay.picture(now, 1.0);
    picture.paint(&mut canvas, None, &everything);
    // Inside the box, left of the text: white under a 60% dark grey.
    let inside = canvas.pixel(6, 30).expect("on the canvas");
    assert!(inside & 0xff < 0x80, "{inside:#x}");
    // Outside it, untouched.
    assert_eq!(canvas.pixel(630, 190), Some(0xffff_ffff));
    // Somewhere on the rate's line, a green pixel.
    let Some(Mark::Text { rect, .. }) = picture.marks.get(2) else {
        panic!("the rate is the second line");
    };
    let green = (rect.y..rect.bottom()).any(|y| {
        (rect.x..rect.right()).any(|x| {
            let x = u32::try_from(x).unwrap_or(0);
            let y = u32::try_from(y).unwrap_or(0);
            canvas.pixel(x, y) == Some(0xff33_ff33)
        })
    });
    assert!(green, "the rate is drawn in green");
}
