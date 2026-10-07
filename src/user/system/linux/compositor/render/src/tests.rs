use std::collections::BTreeMap;

use compositor_config::{Gaps, NoSources, parse};
use compositor_layout::{Monitor, MonitorId, MonitorLayout, Placed, Settings, State, WindowId};

use crate::golden::{self, Mismatch};
use crate::{
    Backdrop, Blur, Canvas, Color, Damage, Error, Format, Gradient, LayerFrame, Pattern, Rect,
    Rounding, Style, Styles, Surface, Target, Transform, cursor, damage_between,
    damage_between_styled, outer, reads_backdrop, render, render_onto, render_with_layers,
};

const BG: u32 = 0x0020_4060;

/// The style every expected image in this crate is blessed from:
/// [`Style::default`] with the dither off.
///
/// [`Style::undithered`] says why. The dither is drawn, and
/// `graded-blur-two-clients` is the image that holds it; every other
/// picture here is of the layout and the decorations, and a dither over
/// all of them costs eight megabytes of run-length encoding to say the
/// same thing eleven times.
pub(crate) fn plain_style() -> Style {
    Style::default().undithered()
}

/// `plain_style()` with the drop shadow off.
///
/// Hyprland has shadows on by default, so a default frame has them; a test
/// that asks what colour a gap or a border is has to say it wants the one
/// with nothing over it, or it is asking about the shadow.
fn unshadowed() -> Style {
    Style {
        shadow: None,
        ..plain_style()
    }
}

/// An `XRGB8888` value as the canvas presents it, X byte `0xFF`.
const fn shown(xrgb: u32) -> u32 {
    xrgb | 0xFF00_0000
}

fn canvas(width: u32, height: u32) -> Canvas {
    let mut canvas = Canvas::new(width, height).unwrap();
    canvas.clear(Color(BG), &Damage::full(width, height));
    let _ = canvas.take_damage();
    canvas
}

/// Every pixel of `canvas` in row order.
fn pixels(canvas: &Canvas) -> Vec<u32> {
    (0..canvas.height())
        .flat_map(|y| (0..canvas.width()).map(move |x| (x, y)))
        .map(|(x, y)| canvas.pixel(x, y).unwrap())
        .collect()
}

// -- The frame of two pattern clients ----------------------------------------

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;
const CHECKERBOARD: WindowId = WindowId(1);
const GRADIENT: WindowId = WindowId(2);

/// A 1024x768 monitor with the default gaps and border, the checkerboard
/// opened first and the gradient second, as dwindle tiles them.
fn two_clients() -> (State, MonitorLayout) {
    two_clients_tiled(Settings::default())
}

/// The same, tiled with `settings`: the gaps and the border width a
/// `general` block asked for.
fn two_clients_tiled(settings: Settings) -> (State, MonitorLayout) {
    two_clients_on((WIDTH, HEIGHT), settings)
}

/// The same two clients on a monitor of another size, for a picture that
/// wants to be smaller than the screen this crate's images are usually of.
pub(crate) fn two_clients_on(size: (u32, u32), settings: Settings) -> (State, MonitorLayout) {
    let mut state = State::new(settings);
    let _ = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: "Virtual-1".to_owned(),
            id: MonitorId(1),
            rect: Rect::new(0, 0, i64::from(size.0), i64::from(size.1)),
            reserved: Gaps::all(0),
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    let _ = state.open_window(CHECKERBOARD).unwrap();
    let _ = state.open_window(GRADIENT).unwrap();
    let layout = state.layout().remove(0);
    (state, layout)
}

/// Each window's pattern, drawn at its client rectangle's size.
pub(crate) fn client_buffers(
    layout: &MonitorLayout,
) -> BTreeMap<WindowId, (Vec<u8>, u32, u32, Format)> {
    layout
        .windows
        .iter()
        .map(|placed| {
            let pattern = if placed.window == CHECKERBOARD {
                Pattern::Checkerboard
            } else {
                Pattern::Gradient
            };
            let (width, height) = (placed.rect.width as u32, placed.rect.height as u32);
            (
                placed.window,
                (pattern.draw(width, height), width, height, pattern.format()),
            )
        })
        .collect()
}

pub(crate) fn surfaces(
    buffers: &BTreeMap<WindowId, (Vec<u8>, u32, u32, Format)>,
) -> BTreeMap<WindowId, Surface<'_>> {
    buffers
        .iter()
        .map(|(&window, (data, width, height, format))| {
            (
                window,
                Surface::new(data, *width, *height, width * 4, *format).unwrap(),
            )
        })
        .collect()
}

/// The two clients' frame after the dispatchers in `after` have run,
/// presented into a dumb-buffer-shaped target with no padding, as bytes.
///
/// The dispatchers are `src/user/system/linux/compositor/layout`'s own, by the names a keybind
/// writes, so the picture a keybind makes and the picture this blesses come
/// from one piece of code.
fn frame_after(after: &[(&str, &str)]) -> Vec<u8> {
    frame_with(&plain_style(), after)
}

/// The same, allowing a dispatcher that changes no picture: `togglegroup`
/// makes a group of one, which is drawn exactly as the window was.
fn frame_quiet(after: &[(&str, &str)]) -> Vec<u8> {
    frame_dispatching(&plain_style(), after, false)
}

/// The same, drawn with `style`.
fn frame_with(style: &Style, after: &[(&str, &str)]) -> Vec<u8> {
    frame_dispatching(style, after, true)
}

/// The frame after `after`, requiring each dispatcher to have changed
/// something when `must_change` says so.
fn frame_dispatching(style: &Style, after: &[(&str, &str)], must_change: bool) -> Vec<u8> {
    let (mut state, mut layout) = two_clients();
    for (name, argument) in after {
        let changes = state.dispatch_str(name, argument).unwrap();
        assert!(
            !must_change || !changes.is_empty(),
            "{name} {argument} changed nothing"
        );
        layout = state.layout().remove(0);
    }
    let buffers = client_buffers(&layout);
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let produced = render(
        &mut canvas,
        &layout,
        (0, 0),
        style,
        &surfaces(&buffers),
        &full,
    );
    assert_eq!(produced, full);
    let mut bytes = vec![0; WIDTH as usize * HEIGHT as usize * 4];
    let mut target = Target::new(&mut bytes, WIDTH, HEIGHT, WIDTH * 4).unwrap();
    canvas.present(&mut target, &full).unwrap();
    assert_eq!(bytes, canvas.data());
    bytes
}

/// A 1024x768 screen at `monitor = , preferred, auto, 2`: the windows tiled
/// in 512x384 logical pixels and drawn as 1024x768 buffer pixels, with the
/// clients' own buffers at the size a client that read `wl_output.scale`
/// sends.
fn scaled_frame() -> (Vec<u8>, Placed) {
    const SCALE: f64 = 2.0;
    let mut state = State::new(Settings::default());
    let _ = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: "Virtual-1".to_owned(),
            id: MonitorId(1),
            // The logical size: what the screen is, divided by the scale.
            rect: Rect::new(0, 0, i64::from(WIDTH) / 2, i64::from(HEIGHT) / 2),
            reserved: Gaps::all(0),
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    let _ = state.open_window(CHECKERBOARD).unwrap();
    let _ = state.open_window(GRADIENT).unwrap();
    let layout = crate::scaled(&state.layout().remove(0), (0, 0), SCALE);
    let buffers = client_buffers(&layout);
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let produced = render(
        &mut canvas,
        &layout,
        (0, 0),
        &plain_style().at_scale(SCALE),
        &surfaces(&buffers),
        &full,
    );
    assert_eq!(produced, full);
    let placed = *layout
        .windows
        .iter()
        .find(|placed| placed.window == CHECKERBOARD)
        .expect("the checkerboard");
    (canvas.data().to_vec(), placed)
}

/// A scaled monitor draws every logical pixel as two: the same tiling, the
/// same gaps and the same borders, each twice the size.
///
/// This is the picture `cargo xtask test-compositor` requires from a
/// screendump of a guest booted with `monitor = , preferred, auto, 2`.
#[test]
fn a_scaled_monitor_draws_each_logical_pixel_twice() {
    let (frame, placed) = scaled_frame();
    golden::check("scaled-two-clients", WIDTH, HEIGHT, &frame);

    // Not the unscaled picture: the same two windows tiled the same way,
    // but every gap and every border twice as wide.
    assert_ne!(frame, two_client_frame(), "the scale changed nothing");

    // The border is two buffer pixels at scale two, where it is one at
    // scale one: `general:border_size` is in logical pixels, as every
    // length a person writes is.
    assert_eq!(placed.border, 2, "the scaled border is not two pixels");
    let at = |x: i64, y: i64| -> u32 {
        let start = ((y * i64::from(WIDTH) + x) * 4) as usize;
        u32::from_le_bytes([
            frame[start],
            frame[start + 1],
            frame[start + 2],
            frame[start + 3],
        ])
    };
    // The window's left edge, and the two columns of border in front of it,
    // taken well down the edge so the corner is not in the way. The colour
    // is Hyprland's translucent `col.inactive_border` over the background,
    // so what is checked is that the two columns match one another and that
    // the third is something else: two pixels of border, not one and not
    // three.
    let left = placed.rect.x;
    let row = placed.rect.y + placed.rect.height / 2;
    assert_eq!(
        at(left - 1, row),
        at(left - 2, row),
        "the border is not two pixels wide"
    );
    assert_ne!(
        at(left - 3, row),
        at(left - 2, row),
        "the border is wider than two pixels"
    );
    assert_ne!(
        at(left, row),
        at(left - 1, row),
        "the window's own pixel is the border's colour"
    );
}

/// Two 1024x768 monitors side by side, with the gradient moved to the second
/// one: a frame each, drawn the way the compositor draws them -- one canvas
/// a monitor, each with its own origin in the space the windows' rectangles
/// are in.
fn two_monitor_frames() -> (Vec<u8>, Vec<u8>) {
    let mut state = State::new(Settings::default());
    for (index, x) in [(1u32, 0i64), (2, i64::from(WIDTH))] {
        let _ = state
            .add_monitor(Monitor {
                scale: 1.0,
                transform: Default::default(),
                name: format!("Virtual-{index}"),
                id: MonitorId(index),
                rect: Rect::new(x, 0, i64::from(WIDTH), i64::from(HEIGHT)),
                reserved: Gaps::all(0),
                description: String::new(),
                made: <(String, String, String)>::default(),
            })
            .unwrap();
    }
    let _ = state.open_window(CHECKERBOARD).unwrap();
    let _ = state.open_window(GRADIENT).unwrap();
    // The gradient has the focus, and goes to the second monitor with it;
    // the checkerboard is left alone on the first, unfocused.
    let changes = state.dispatch_str("movewindow", "mon:1").unwrap();
    assert!(!changes.is_empty(), "the window did not move");

    let outputs = state.layout();
    let mut frames = Vec::new();
    for (index, x) in [(1u32, 0i64), (2, i64::from(WIDTH))] {
        let layout = outputs
            .iter()
            .find(|output| output.monitor == MonitorId(index))
            .expect("a layout for the monitor")
            .clone();
        let buffers = client_buffers(&layout);
        let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
        let full = Damage::full(WIDTH, HEIGHT);
        let produced = render(
            &mut canvas,
            &layout,
            (x, 0),
            &plain_style(),
            &surfaces(&buffers),
            &full,
        );
        assert_eq!(produced, full);
        frames.push(canvas.data().to_vec());
    }
    let mut frames = frames.into_iter();
    let (left, right) = (
        frames.next().expect("the left frame"),
        frames.next().expect("the right frame"),
    );
    (left, right)
}

/// A window moved to the second monitor is drawn there and nowhere else,
/// each monitor drawing the workspace it shows.
///
/// These are the two pictures `cargo xtask test-compositor` requires from
/// screendumps of two virtio-gpu devices, one for each of the guest's cards.
#[test]
fn the_first_monitor_keeps_the_window_that_stayed() {
    let (left, _) = two_monitor_frames();
    golden::check("two-monitors-left", WIDTH, HEIGHT, &left);
}

/// The other monitor, and what both of them must be: two different pictures,
/// neither empty and neither the tiled pair.
///
/// A test of its own for each image because blessing one writes it and
/// stops, and two images want two runs.
#[test]
fn the_second_monitor_draws_the_window_moved_to_it() {
    let (left, right) = two_monitor_frames();
    golden::check("two-monitors-right", WIDTH, HEIGHT, &right);
    assert_ne!(left, right, "both monitors drew the same thing");

    // Each monitor has one window on it, so neither frame is the tiled pair
    // and neither is empty.
    let background = shown(plain_style().background.0 & 0x00FF_FFFF);
    for (what, frame) in [("left", &left), ("right", &right)] {
        let pixels: Vec<u32> = frame
            .chunks_exact(4)
            .filter_map(|pixel| pixel.try_into().ok().map(u32::from_le_bytes))
            .collect();
        assert!(
            pixels.iter().any(|pixel| *pixel != background),
            "the {what} monitor is empty"
        );
        assert!(
            pixels.contains(&background),
            "the {what} monitor has no background at all, so nothing is tiled"
        );
    }
}

/// What a `windowrule` that floats, sizes and moves a window makes: the
/// checkerboard with the tiling to itself, and the gradient floating over it
/// at the place and size the rule gave.
///
/// The rule is carried out by `src/user/system/linux/compositor/hyprix`'s `rules`, which calls
/// `State::float_window`; this calls the same thing, so the picture the
/// compositor draws on Ferrix and the picture blessed here are made by one
/// piece of code.
fn ruled_frame() -> Vec<u8> {
    use std::collections::BTreeMap;

    let (mut state, _) = two_clients();
    let _ = state
        .float_window(GRADIENT, Rect::new(RULED.0, RULED.1, RULED.2, RULED.3))
        .expect("the window floats");
    let layout = state.layout().remove(0);
    let buffers = client_buffers(&layout);
    // And what the rules give each window to be drawn with: the floating one
    // at `opacity 0.6`, the tiled one with its corners cut and no shadow.
    let mut windows = BTreeMap::new();
    let _ = windows.insert(
        GRADIENT,
        crate::WindowStyle {
            opacity: Some(0.6),
            ..crate::WindowStyle::default()
        },
    );
    let _ = windows.insert(
        CHECKERBOARD,
        crate::WindowStyle {
            rounding: Some(12),
            shadow: false,
            ..crate::WindowStyle::default()
        },
    );
    let style = plain_style();
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let produced = render_with_layers(
        &mut canvas,
        &layout,
        (0, 0),
        &Styles {
            base: &style,
            windows: &windows,
        },
        &surfaces(&buffers),
        &[],
        &full,
    );
    assert_eq!(produced, full);
    canvas.data().to_vec()
}

/// Where the rule puts the floating window: the same numbers
/// `cargo xtask test-compositor`'s configuration gives it.
const RULED: (i64, i64, i64, i64) = (200, 150, 400, 300);

/// A window a rule floats is drawn where the rule put it, over the one that
/// has the tiling to itself.
///
/// This is the picture `cargo xtask test-compositor` requires from a
/// screendump of a guest whose `hyprland.conf` carries those rules.
#[test]
fn a_ruled_window_floats_where_the_rule_put_it() {
    let frame = ruled_frame();
    golden::check("ruled-two-clients", WIDTH, HEIGHT, &frame);

    // Not the tiled picture: one window has the whole area and the other is
    // over it.
    assert_ne!(frame, two_client_frame(), "the rule changed nothing");

    // The floating window's own pixels are where the rule said. Inside its
    // client area, which is the rectangle less the border.
    let at = |x: i64, y: i64| -> u32 {
        let start = ((y * i64::from(WIDTH) + x) * 4) as usize;
        u32::from_le_bytes([
            frame[start],
            frame[start + 1],
            frame[start + 2],
            frame[start + 3],
        ])
    };
    let tiled = two_client_frame();
    let tiled_at = |x: i64, y: i64| -> u32 {
        let start = ((y * i64::from(WIDTH) + x) * 4) as usize;
        u32::from_le_bytes([
            tiled[start],
            tiled[start + 1],
            tiled[start + 2],
            tiled[start + 3],
        ])
    };
    let middle = (RULED.0 + RULED.2 / 2, RULED.1 + RULED.3 / 2);
    assert_ne!(
        at(middle.0, middle.1),
        tiled_at(middle.0, middle.1),
        "the floating window is not over the middle of where it was put"
    );
    // And outside it, at the far right, the checkerboard now reaches: the
    // tiling is one window's.
    let far = (i64::from(WIDTH) - 40, i64::from(HEIGHT) / 2);
    assert_ne!(
        at(far.0, far.1),
        tiled_at(far.0, far.1),
        "the tiled window did not take the space the floating one left"
    );
}

/// A window a rule gave a style of its own: drawn with that, where every
/// other window is drawn with the configuration's.
#[test]
fn a_window_a_rule_styled_is_drawn_with_it() {
    use std::collections::BTreeMap;

    use crate::{Styles, WindowStyle};

    let (_, layout) = two_clients();
    let buffers = client_buffers(&layout);
    let style = decorated_style();
    let frame = |windows: &BTreeMap<WindowId, WindowStyle>| -> Vec<u8> {
        let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
        let full = Damage::full(WIDTH, HEIGHT);
        let styles = Styles {
            base: &style,
            windows,
        };
        let _ = render_with_layers(
            &mut canvas,
            &layout,
            (0, 0),
            &styles,
            &surfaces(&buffers),
            &[],
            &full,
        );
        canvas.data().to_vec()
    };
    let plain = frame(&BTreeMap::new());

    // `no_shadow` on one window: the picture changes, and only where that
    // window's shadow was.
    let mut without = BTreeMap::new();
    let _ = without.insert(
        CHECKERBOARD,
        WindowStyle {
            shadow: false,
            ..WindowStyle::default()
        },
    );
    let unshadowed = frame(&without);
    assert_ne!(unshadowed, plain, "the shadow was drawn anyway");

    // `rounding 0` on one window: its corner is the border again.
    let mut square = BTreeMap::new();
    let _ = square.insert(
        CHECKERBOARD,
        WindowStyle {
            rounding: Some(0),
            ..WindowStyle::default()
        },
    );
    let squared = frame(&square);
    assert_ne!(squared, plain, "the corner was cut anyway");

    // `opacity 1` on the unfocused window, which the style draws at 0.6:
    // more of it shows.
    let mut opaque = BTreeMap::new();
    let _ = opaque.insert(
        CHECKERBOARD,
        WindowStyle {
            opacity: Some(1.0),
            ..WindowStyle::default()
        },
    );
    assert_ne!(frame(&opaque), plain, "the opacity was the style's anyway");

    // And a window with no rule is drawn as it always was: the gradient's
    // half of every picture above is the same.
    let half = (WIDTH as usize / 2 + 100) * 4;
    let row = (HEIGHT as usize / 2) * WIDTH as usize * 4;
    assert_eq!(
        plain.get(row + half..row + half + 4),
        unshadowed.get(row + half..row + half + 4),
        "the window with no rule changed"
    );
}

/// The frame with nothing dispatched: the second window opened is the
/// focused one, which dwindle puts on the right.
fn two_client_frame() -> Vec<u8> {
    frame_after(&[])
}

/// A bar across the top, and the two clients tiled in what is left.
///
/// The bar's height is 30 and its exclusive zone all of it, which is what
/// `src/user/system/linux/compositor/pattern --bar 30` asks for; where it goes is
/// `compositor_layout::layers`' answer and what it leaves is the monitor's
/// reserved strip, so the picture is made by the same two crates the
/// compositor uses.
fn bar_and_two_clients_frame() -> Vec<u8> {
    bar_frame_with_menu(None)
}

/// The bar and the two clients, with a menu hanging off the bar's top-left
/// corner when `menu` says whether its popup is blurred: what
/// `layerrule = blur_popups` does to a bar's tooltip.
fn bar_frame_with_menu(menu: Option<bool>) -> Vec<u8> {
    const BAR: u32 = 30;

    let monitor = Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT));
    let request = compositor_layout::layers::Request {
        top: true,
        left: true,
        right: true,
        size: (0, BAR),
        exclusive_zone: BAR.cast_signed(),
        ..compositor_layout::layers::Request::default()
    };
    let (placements, reserved) = compositor_layout::layers::place(monitor, &[request]);
    let bar = placements[0];

    let mut state = State::new(Settings::default());
    let _ = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: "Virtual-1".to_owned(),
            id: MonitorId(1),
            rect: monitor,
            reserved: Gaps::all(0),
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    let _ = state.set_reserved(MonitorId(1), reserved).unwrap();
    let _ = state.open_window(CHECKERBOARD).unwrap();
    let _ = state.open_window(GRADIENT).unwrap();
    let layout = state.layout().remove(0);

    let buffers = client_buffers(&layout);
    // The bar draws the checkerboard, which is what the test client does.
    let bar_pixels = Pattern::Checkerboard.draw(bar.rect.width as u32, bar.rect.height as u32);
    let bar_surface = Surface::new(
        &bar_pixels,
        bar.rect.width as u32,
        bar.rect.height as u32,
        bar.rect.width as u32 * 4,
        Pattern::Checkerboard.format(),
    )
    .unwrap();

    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let menu_pixels = Pattern::Gradient.draw(MENU as u32, MENU as u32);
    let mut layers = vec![LayerFrame {
        rect: bar.rect,
        above: true,
        surface: Some(bar_surface),
        dim_around: false,
        blur: false,
        xray: false,
    }];
    if let Some(blur) = menu {
        layers.push(menu_frame(&menu_pixels, bar.rect, blur));
    }
    let produced = render_with_layers(
        &mut canvas,
        &layout,
        (0, 0),
        &Styles::plain(&plain_style()),
        &surfaces(&buffers),
        &layers,
        &full,
    );
    assert_eq!(produced, full);
    canvas.data().to_vec()
}

/// A layer client the compositor has not told the screen's scale draws at
/// 1: on a screen at 2 its buffer is half its rectangle each way. It is
/// stretched over the rectangle, as a window is, and not drawn pixel for
/// pixel into its top-left quarter under a reserved zone of the whole.
#[test]
fn a_layer_drawn_at_half_its_rectangles_size_is_stretched_over_it() {
    let mut state = State::new(Settings::default());
    let _ = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: "Virtual-1".to_owned(),
            id: MonitorId(1),
            rect: Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT)),
            reserved: Gaps::all(0),
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    let layout = state.layout().remove(0);
    let pixels = [0x10_u8, 0x80, 0xf0, 0xff].repeat(100 * 30);
    let surface = Surface::new(&pixels, 100, 30, 400, Format::Xrgb8888).unwrap();
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let layers = [LayerFrame {
        rect: Rect::new(0, 0, 200, 60),
        above: true,
        surface: Some(surface),
        dim_around: false,
        blur: false,
        xray: false,
    }];
    let _ = render_with_layers(
        &mut canvas,
        &layout,
        (0, 0),
        &Styles::plain(&plain_style()),
        &BTreeMap::new(),
        &layers,
        &full,
    );
    let at = |x: usize, y: usize| {
        let i = (y * WIDTH as usize + x) * 4;
        canvas.data()[i..i + 4].to_vec()
    };
    assert_eq!(at(199, 59), at(0, 0), "the far corner is the layer's");
    assert_ne!(at(199, 59), at(300, 300), "and not the background");
}

#[test]
fn a_bar_takes_its_strip_and_the_windows_tile_under_it() {
    let frame = bar_and_two_clients_frame();
    golden::check("layer-bar-two-clients", WIDTH, HEIGHT, &frame);

    // The windows really moved down: the frame differs from the one with no
    // bar over far more than the bar's own strip.
    let plain = two_client_frame();
    let differing = frame
        .chunks(4)
        .zip(plain.chunks(4))
        .filter(|(one, other)| one != other)
        .count();
    assert!(
        differing > (WIDTH as usize) * 30,
        "only {differing} pixels changed; the bar drew and nothing moved"
    );
}

/// The same two clients with Hyprland's two decorations on: corners cut to
/// `decoration:rounding` and the unfocused window at
/// `decoration:inactive_opacity`.
pub(crate) fn decorated_style() -> Style {
    let config = parse(
        "test",
        "decoration:rounding = 12\n\
         decoration:inactive_opacity = 0.6\n\
         decoration:shadow:range = 12\n\
         decoration:shadow:render_power = 2\n\
         decoration:dim_inactive = 1\n\
         decoration:dim_strength = 0.4\n",
        &mut NoSources,
    )
    .config;
    let style = Style::from_config(&config).undithered();
    assert_eq!(style.rounding, Rounding::circle(12));
    assert!((style.inactive_opacity - 0.6).abs() < 0.001);
    assert!((style.active_opacity - 1.0).abs() < f32::EPSILON);
    assert!((style.dim - 0.4).abs() < 0.001);
    let shadow = style.shadow.expect("shadows are on by default");
    assert_eq!((shadow.range, shadow.power), (12, 2));
    assert_eq!(shadow.color, Color(0xEE1A_1A1A), "Hyprland's own default");
    assert_eq!(
        shadow.rounding,
        Rounding::circle(12),
        "the shadow follows the window's corners"
    );
    style
}

fn decorated_frame() -> Vec<u8> {
    frame_with(&decorated_style(), &[])
}

/// The shadow's shape, against the rules `shadow.glsl` has: nothing past the
/// range, and a falloff towards the window that never turns back.
#[test]
fn a_shadow_fades_out_over_its_range_and_no_further() {
    let (_, layout) = two_clients();
    let placed = layout
        .windows
        .iter()
        .find(|placed| placed.focused)
        .copied()
        .unwrap();
    let frame = decorated_frame();
    let at = |x: i64, y: i64| -> u32 {
        let start = ((y * i64::from(WIDTH) + x) * 4) as usize;
        u32::from_le_bytes([
            frame[start],
            frame[start + 1],
            frame[start + 2],
            frame[start + 3],
        ])
    };
    let background = shown(plain_style().background.0 & 0x00FF_FFFF);
    // Above the focused window, at its horizontal middle: the only shadow
    // that can reach there is its own, since the other window is beside it
    // and 20 pixels is not the distance between them.
    let range = decorated_style().shadow.expect("a shadow").range;
    let top = placed.rect.y - placed.border.max(0);
    let middle = placed.rect.x + placed.rect.width / 2;
    assert!(top - range > 0, "the window is too near the top edge");

    assert_eq!(
        at(middle, top - range - 1),
        background,
        "the shadow reached past its range"
    );
    // Near the window it is plainly there. One pixel inside the range it is
    // not: a power of two puts the alpha at `(1/12)² × 0.93`, which is less
    // than half a step of the background's grey and rounds away. That is
    // what a falloff is, and a test that asked for a visible pixel at the
    // far edge would be asking the shadow to have a hard edge.
    assert_ne!(at(middle, top - 2), background, "there is no shadow");

    // Each step towards the window is at least as far towards the shadow's
    // own colour as the one before. Towards, not darker: Hyprland's default
    // shadow is `0xee1a1a1a` and its background `0x111111`, so its shadow is
    // the lighter of the two and a test that asked for darker would be
    // asking about this tree's background rather than about the falloff.
    let greys: Vec<u32> = (1..range)
        .map(|step| at(middle, top - range + step) & 0xFF)
        .collect();
    assert!(
        greys.windows(2).all(|pair| pair[1] >= pair[0]),
        "the falloff is not monotonic: {greys:?}"
    );
    assert_eq!(
        greys.iter().min(),
        greys.first(),
        "it does not start at the background: {greys:?}"
    );
    assert!(
        greys.iter().min() < greys.iter().max(),
        "the shadow is flat: {greys:?}"
    );
}

/// `decoration:dim_inactive` lays black over a window that is not focused,
/// and over no other.
#[test]
fn dimming_darkens_the_unfocused_window_alone() {
    let (_, layout) = two_clients();
    let frame = decorated_frame();
    let plain = frame_with(
        &Style {
            dim: 0.0,
            ..decorated_style()
        },
        &[],
    );
    let at = |bytes: &[u8], x: i64, y: i64| -> u32 {
        let start = ((y * i64::from(WIDTH) + x) * 4) as usize;
        u32::from_le_bytes([
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ])
    };
    for placed in &layout.windows {
        let (x, y) = (
            placed.rect.x + placed.rect.width / 2,
            placed.rect.y + placed.rect.height / 2,
        );
        let (dimmed, undimmed) = (at(&frame, x, y), at(&plain, x, y));
        if placed.focused {
            assert_eq!(dimmed, undimmed, "the focused window was dimmed");
        } else {
            assert_ne!(dimmed, undimmed, "the unfocused window was not dimmed");
            assert!(
                (dimmed & 0xFF) <= (undimmed & 0xFF),
                "dimming made it lighter"
            );
        }
    }
}

#[test]
fn rounding_and_opacity_make_a_different_picture() {
    let frame = decorated_frame();
    golden::check("decorated-two-clients", WIDTH, HEIGHT, &frame);

    // The corner of the focused window: outside the rounding it is the
    // background, inside it the border. With no rounding both are border.
    let plain = two_client_frame();
    let differing = frame
        .chunks(4)
        .zip(plain.chunks(4))
        .filter(|(one, other)| one != other)
        .count();
    assert!(differing > 0, "the decorations changed nothing");
}

/// The decorated windows after `movefocus l` and `movewindow r`: the state a
/// window's slide ends in, with the corners cut, the shadows under them, the
/// blur behind the translucent one and the dimming on whichever is not
/// focused.
///
/// This is the picture `cargo xtask test-compositor` requires at the end of
/// the sequence of screendumps it takes while the window is moving, which is
/// what `docs/ROADMAP.md` stage 19's exit asks for.
#[test]
fn the_decorated_windows_swap_places_too() {
    let swapped = frame_with(
        &decorated_style(),
        &[("movefocus", "l"), ("movewindow", "r")],
    );
    golden::check("decorated-two-clients-swapped", WIDTH, HEIGHT, &swapped);
    assert_ne!(
        swapped,
        decorated_frame(),
        "the windows did not change places"
    );
}

/// A rounded window's very corner is not the window: the rounding cut it
/// away, and what shows there is whatever is behind. Along the same edge,
/// away from the corner, the border is drawn as it always was.
#[test]
fn a_rounded_corner_shows_what_is_behind_it() {
    let (_, layout) = two_clients();
    // The focused window, so the border on its edge is the active colour.
    let placed = layout
        .windows
        .iter()
        .find(|placed| placed.focused)
        .copied()
        .unwrap();
    let border = placed.border.max(0);
    let outer_x = (placed.rect.x - border) as u32;
    let outer_y = (placed.rect.y - border) as u32;
    let middle = outer_x + (placed.rect.width / 2) as u32;

    // With the shadow off: a corner that is cut shows the background, and a
    // shadow over it would be neither.
    let bytes = frame_with(
        &Style {
            shadow: None,
            ..decorated_style()
        },
        &[],
    );
    let at = |x: u32, y: u32| -> u32 {
        let start = ((y * WIDTH + x) * 4) as usize;
        u32::from_le_bytes([
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ])
    };
    let background = shown(plain_style().background.0 & 0x00FF_FFFF);
    assert_eq!(at(outer_x, outer_y), background, "the corner was not cut");
    assert_eq!(
        at(outer_x, outer_y + 1),
        background,
        "the row below the corner was not cut either"
    );
    // The same edge, away from the corner: the border, one pixel of it,
    // which is `general:border_size`'s default.
    assert_eq!(
        at(middle, outer_y),
        shown(0x00FF_FFFF),
        "the border is not on the edge"
    );

    // With no rounding the corner is the border, which is what makes the
    // check above worth having.
    let square = frame_with(&unshadowed(), &[]);
    let start = ((outer_y * WIDTH + outer_x) * 4) as usize;
    assert_eq!(
        u32::from_le_bytes([
            square[start],
            square[start + 1],
            square[start + 2],
            square[start + 3],
        ]),
        shown(0x00FF_FFFF)
    );
}

/// A group: the focused window made one with `togglegroup`, and the other
/// moved into it with `moveintogroup`. The two share one slot, which is the
/// whole work area now that only the head is in the tiling, and the slot
/// draws the member that was moved in -- Hyprland makes the window it adds
/// the active one.
///
/// This is the picture `cargo xtask test-compositor` requires from a
/// screendump after a keybind batches those dispatchers through the control
/// socket.
#[test]
fn a_group_draws_one_member_in_the_slot_they_share() {
    let grouped = frame_quiet(&GROUPING);
    golden::check("grouped-two-clients", WIDTH, HEIGHT, &grouped);

    // One window's worth of picture, and not the tiled one: both windows are
    // in one slot, so the gradient's half of the screen is gone.
    let tiled = two_client_frame();
    assert_ne!(grouped, tiled, "the group changed nothing");

    // Cycling the group draws the other member in the same place, and
    // cycling again comes back: a group of two wraps.
    let mut cycled = GROUPING.to_vec();
    cycled.push(("changegroupactive", "f"));
    let other = frame_quiet(&cycled);
    assert_ne!(other, grouped, "the same member is still drawn");
    cycled.push(("changegroupactive", "f"));
    assert_eq!(frame_quiet(&cycled), grouped, "forward twice did not wrap");
}

/// What makes the group: the dispatchers a keybind runs, here and on Ferrix.
const GROUPING: [(&str, &str); 3] = [
    ("togglegroup", ""),
    ("movefocus", "l"),
    ("moveintogroup", "r"),
];

#[test]
fn two_pattern_clients_tiled_by_dwindle_match_the_expected_image() {
    golden::check("dwindle-two-clients", WIDTH, HEIGHT, &two_client_frame());
}

/// The pointer, drawn over the two tiled windows.
///
/// This is the picture `cargo xtask test-compositor` requires after the
/// pointer has been moved: the compositor's own arrow, its tip at the point
/// the mouse is, over everything else -- not under a window and not under a
/// menu, because a pointer nobody can follow is worse than none.
#[test]
fn the_pointer_is_drawn_over_the_windows() {
    let (_, layout) = two_clients();
    let buffers = client_buffers(&layout);
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let produced = render(
        &mut canvas,
        &layout,
        (0, 0),
        &plain_style(),
        &surfaces(&buffers),
        &full,
    );
    assert_eq!(produced, full);

    let arrow = cursor::arrow();
    let surface = cursor::surface(&arrow).unwrap();
    canvas.composite(
        &surface,
        Rect::new(
            i64::from(POINTER.0 - cursor::HOTSPOT.0),
            i64::from(POINTER.1 - cursor::HOTSPOT.1),
            i64::from(cursor::SIDE),
            i64::from(cursor::SIDE),
        ),
        &full,
    );
    golden::check("pointer-on-two-clients", WIDTH, HEIGHT, canvas.data());
}

/// Where the pointer is put, which the boot's QMP movement must match.
///
/// Inside the *focused* window, which is the one opened second. Hyprland's
/// `input:follow_mouse` is on by default and this compositor's is too, so a
/// pointer moved into the other window would take the focus with it and
/// change which border is drawn active -- a different picture, and one this
/// test is not about.
const POINTER: (i32, i32) = (700, 300);

/// A window with a menu on it: the popup drawn over the window, at the
/// rectangle `xdg_positioner`'s rules put it.
///
/// This is the picture `cargo xtask test-compositor` requires of a popup.
/// The popup hangs off a one-pixel anchor rectangle at the window's
/// top-left, anchored and gravitated `bottom_right`, so its own top-left
/// lands one pixel in from the window's -- which is where a menu opened at a
/// point goes, and is what every toolkit asks for.
#[test]
fn a_window_with_a_menu_on_it_matches_the_expected_image() {
    golden::check("menu-on-a-window", WIDTH, HEIGHT, &window_menu_frame(false));
}

/// `decoration:blur:popups`: the same menu with what is behind it blurred,
/// as Hyprland's `renderWindow` blurs a window's popups when it is on.
///
/// The gradient's lower half can be seen through, so the blur shows there
/// and changes the picture; the control is the unblurred image above.
#[test]
fn a_blurred_menu_on_a_window_matches_the_expected_image() {
    let blurred = window_menu_frame(true);
    golden::check("blurred-menu-on-a-window", WIDTH, HEIGHT, &blurred);
    assert_ne!(
        blurred,
        window_menu_frame(false),
        "the blur behind the menu changed nothing"
    );
}

/// `layerrule = blur_popups`: a bar's menu with what is behind it blurred,
/// as Hyprland's `renderLayer` does for the popups of a surface whose rule
/// says so. The menu hangs off the bar's corner and over the windows.
#[test]
fn a_blurred_menu_on_a_bar_matches_the_expected_image() {
    let blurred = bar_frame_with_menu(Some(true));
    golden::check("blurred-menu-on-a-bar", WIDTH, HEIGHT, &blurred);
    assert_ne!(
        blurred,
        bar_frame_with_menu(Some(false)),
        "the blur behind the menu changed nothing"
    );
}

/// The two clients with a menu on the checkerboard, its popup blurred when
/// `blur` says so.
fn window_menu_frame(blur: bool) -> Vec<u8> {
    let (_, layout) = two_clients();
    let buffers = client_buffers(&layout);
    // Where the compositor puts it: the placement rules, on the window the
    // menu belongs to, so the picture and the compositor agree by
    // construction.
    let parent = layout
        .windows
        .iter()
        .find(|placed| placed.window == CHECKERBOARD)
        .map(|placed| placed.rect)
        .expect("the checkerboard is on screen");
    let menu = Pattern::Gradient.draw(MENU as u32, MENU as u32);
    let over = [menu_frame(&menu, parent, blur)];
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let _ = render_with_layers(
        &mut canvas,
        &layout,
        (0, 0),
        &Styles::plain(&plain_style()),
        &surfaces(&buffers),
        &over,
        &full,
    );
    canvas.data().to_vec()
}

/// The menu the pattern client opens on `parent`, drawn from `pixels`.
///
/// The popup hangs off a one-pixel anchor rectangle at the parent's
/// top-left, anchored and gravitated `bottom_right`, so its own top-left
/// lands one pixel in from the parent's -- which is where a menu opened at
/// a point goes, and is what every toolkit asks for.
fn menu_frame(pixels: &[u8], parent: Rect, blur: bool) -> LayerFrame<'_> {
    let positioner = compositor_layout::popup::Positioner {
        size: (MENU, MENU),
        anchor_rect: Rect::new(0, 0, 1, 1),
        anchor: compositor_layout::popup::Anchor::BottomRight,
        gravity: compositor_layout::popup::Anchor::BottomRight,
        adjust: compositor_layout::popup::Adjust(
            compositor_layout::popup::Adjust::SLIDE_X | compositor_layout::popup::Adjust::SLIDE_Y,
        ),
        ..compositor_layout::popup::Positioner::default()
    };
    let at = compositor_layout::popup::place(
        &positioner,
        parent,
        Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT)),
    );
    let surface = Surface::new(
        pixels,
        MENU as u32,
        MENU as u32,
        MENU as u32 * 4,
        Pattern::Gradient.format(),
    )
    .unwrap();
    LayerFrame {
        rect: Rect::new(parent.x + at.x, parent.y + at.y, at.width, at.height),
        above: true,
        surface: Some(surface),
        dim_around: false,
        blur,
        xray: false,
    }
}

/// The menu's side, which the boot's `--menu` argument must match.
const MENU: i64 = 200;

/// A locked screen: the lock's own surface over the whole of it, and
/// nothing of what was there before.
///
/// This is the picture `cargo xtask test-compositor` requires while
/// `ext-session-lock-v1` holds the screen. It is the checkerboard at the
/// screen's exact size and at its exact origin, because that is what the
/// protocol makes a lock surface: a compositor that drew it anywhere else,
/// or left a strip of a window showing, would not match.
#[test]
fn a_locked_screen_is_the_lock_surface_and_nothing_else() {
    // No windows at all: while the session is locked the compositor draws
    // the lock's surface and stops drawing the layout, which is what the
    // frame below is built from.
    let layout = MonitorLayout {
        monitor: MonitorId(1),
        workspace: compositor_layout::WorkspaceId(1),
        windows: Vec::new(),
    };
    let pixels = Pattern::Checkerboard.draw(WIDTH, HEIGHT);
    let surface = Surface::new(&pixels, WIDTH, HEIGHT, WIDTH * 4, Format::Xrgb8888).unwrap();
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let over = [LayerFrame {
        rect: Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT)),
        above: true,
        surface: Some(surface),
        dim_around: false,
        blur: false,
        xray: false,
    }];
    let _ = render_with_layers(
        &mut canvas,
        &layout,
        (0, 0),
        &Styles::plain(&plain_style()),
        &BTreeMap::new(),
        &over,
        &full,
    );
    golden::check("locked-screen", WIDTH, HEIGHT, canvas.data());
}

/// One window left after the other closed: it takes the whole workspace and
/// is drawn with the active border, since the focus goes to what is left.
///
/// This is the picture `cargo xtask test-compositor` requires after a
/// taskbar has asked a window it does not own to close, through
/// `zwlr_foreign_toplevel_handle_v1.close`. The layout is reached the way
/// the compositor reaches it -- the window is *gone*, not merely unfocused
/// -- so the two pictures are made by one piece of code.
#[test]
fn one_client_left_after_the_other_closed_matches_the_expected_image() {
    let (mut state, _) = two_clients();
    let _ = state.window_gone(CHECKERBOARD).unwrap();
    let layout = state.layout().remove(0);
    assert_eq!(layout.windows.len(), 1, "one window should be left");
    let buffers = client_buffers(&layout);
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let produced = render(
        &mut canvas,
        &layout,
        (0, 0),
        &plain_style(),
        &surfaces(&buffers),
        &full,
    );
    assert_eq!(produced, full);
    golden::check("one-client-alone", WIDTH, HEIGHT, canvas.data());
}

/// `movefocus l`: the same two windows in the same places, with the active
/// border on the other one.
///
/// This is the state `cargo xtask test-compositor` requires from a
/// screendump after sending the keybind through QEMU, so it is blessed here
/// -- by calling the renderer with rectangles from the layout -- and compared
/// there, where the picture came from two programs talking Wayland.
#[test]
fn moving_the_focus_moves_the_active_border_and_nothing_else() {
    let moved = frame_after(&[("movefocus", "l")]);
    golden::check("dwindle-two-clients-focus-left", WIDTH, HEIGHT, &moved);

    // Only the borders differ from the tiled frame: a `movefocus` that moved
    // a window would be a different picture and a different bug.
    let tiled = two_client_frame();
    let differing = moved
        .chunks(4)
        .zip(tiled.chunks(4))
        .filter(|(one, other)| one != other)
        .count();
    assert!(differing > 0, "the active border did not move");
    assert!(
        differing < (WIDTH as usize) * (HEIGHT as usize) / 4,
        "{differing} pixels changed for a border's worth of colour; a window moved"
    );
}

/// `movewindow r` with the focus on the left window: the two swap places,
/// which is the other keybind stage 18's exit criterion names.
#[test]
fn moving_a_window_swaps_the_two_of_them() {
    let swapped = frame_after(&[("movefocus", "l"), ("movewindow", "r")]);
    golden::check("dwindle-two-clients-swapped", WIDTH, HEIGHT, &swapped);

    // The patterns changed sides, so far more than a border differs.
    let tiled = two_client_frame();
    let differing = swapped
        .chunks(4)
        .zip(tiled.chunks(4))
        .filter(|(one, other)| one != other)
        .count();
    assert!(
        differing > (WIDTH as usize) * (HEIGHT as usize) / 4,
        "only {differing} pixels changed; the windows did not swap"
    );
}

#[test]
fn the_expected_image_check_fails_on_exactly_the_pixel_altered() {
    // The negative control: the frame the golden test passes, with one
    // pixel inside the gradient changed, must be reported as that one pixel.
    let mut frame = two_client_frame();
    let file = std::fs::read(golden::path("dwindle-two-clients")).unwrap();
    let (_, _, expected) = golden::decode(&file).unwrap();
    assert_eq!(golden::compare(&expected, &frame, WIDTH), []);

    let (x, y) = (700u32, 400u32);
    let offset = (y * WIDTH + x) as usize * 4;
    let before = u32::from_le_bytes(frame[offset..offset + 4].try_into().unwrap());
    frame[offset] ^= 0x01;
    let after = u32::from_le_bytes(frame[offset..offset + 4].try_into().unwrap());
    assert_eq!(
        golden::compare(&expected, &frame, WIDTH),
        [Mismatch {
            x,
            y,
            expected: before,
            actual: after,
        }]
    );
}

#[test]
fn the_expected_image_format_round_trips_and_is_strict() {
    let frame = two_client_frame();
    let file = golden::encode(WIDTH, HEIGHT, &frame);
    assert!(file.len() < 64 * 1024, "{} bytes", file.len());
    assert_eq!(golden::decode(&file), Ok((WIDTH, HEIGHT, frame)));

    let mut long = file.clone();
    long.push(0);
    assert!(golden::decode(&long).is_err());
    assert!(golden::decode(&file[..file.len() - 1]).is_err());
    // A first row may not repeat the row above it.
    let mut header = golden::encode(1, 1, &[1, 2, 3, 4]);
    let tag = header.len() - 7;
    header.truncate(tag);
    header.push(0);
    assert!(golden::decode(&header).is_err());
}

#[test]
fn the_tiled_frame_has_its_gaps_and_borders_on_exact_pixels() {
    let (_, layout) = two_clients();
    let rects: Vec<Rect> = layout.windows.iter().map(|placed| placed.rect).collect();
    // gaps_out 20 to the monitor, gaps_in 5 on each inner edge, border 1.
    assert_eq!(
        rects,
        [Rect::new(21, 21, 485, 726), Rect::new(518, 21, 485, 726)]
    );
    let frame = frame_with(&unshadowed(), &[]);
    let at = |x: u32, y: u32| {
        let offset = (y * WIDTH + x) as usize * 4;
        u32::from_le_bytes(frame[offset..offset + 4].try_into().unwrap())
    };
    let style = unshadowed();
    let bg = style.background.0 | 0xFF00_0000;
    let inactive = style.inactive_border.first().0;
    let active = style.active_border.first().0;
    assert_eq!((inactive, active), (0xFF44_4444, 0xFFFF_FFFF));

    // Along row 300: the outer gap, the checkerboard's border and pixels,
    // its right border, the inner gap of 10, the gradient's border.
    assert_eq!(at(19, 300), bg);
    assert_eq!(at(20, 300), inactive);
    assert_eq!(at(21, 300), shown(Pattern::Checkerboard.pixel(0, 279, 726)));
    assert_eq!(
        at(505, 300),
        shown(Pattern::Checkerboard.pixel(484, 279, 726))
    );
    assert_eq!(at(506, 300), inactive);
    for x in 507..517 {
        assert_eq!(at(x, 300), bg, "the gap at x {x}");
    }
    assert_eq!(at(517, 300), active);
    assert_eq!(at(518, 300), shown(Pattern::Gradient.pixel(0, 279, 726)));
    assert_eq!(at(1002, 300), shown(Pattern::Gradient.pixel(484, 279, 726)));
    assert_eq!(at(1003, 300), active);
    assert_eq!(at(1004, 300), bg);

    // Down column 100: the border rows and the gap to the monitor's edge.
    assert_eq!(at(100, 19), bg);
    assert_eq!(at(100, 20), inactive);
    assert_eq!(at(100, 21), shown(Pattern::Checkerboard.pixel(79, 0, 726)));
    assert_eq!(
        at(100, 746),
        shown(Pattern::Checkerboard.pixel(79, 725, 726))
    );
    assert_eq!(at(100, 747), inactive);
    assert_eq!(at(100, 748), bg);
    // The border's corners are drawn, and the pixel past them diagonally is
    // not.
    assert_eq!(at(20, 20), inactive);
    assert_eq!(at(1003, 747), active);
    assert_eq!(at(1004, 748), bg);
    assert_eq!(at(19, 19), bg);

    // Every pixel of the frame is opaque in its X byte.
    assert!(frame.chunks(4).all(|pixel| pixel[3] == 0xFF));
}

#[test]
fn the_style_follows_the_configuration() {
    let parsed = parse(
        "t.conf",
        "general:col.active_border = rgba(33ccffee) rgba(00ff99ee) 45deg\n\
         general:col.inactive_border = 0xff595959\n",
        &mut NoSources,
    );
    assert_eq!(parsed.diagnostics, []);
    let style = Style::from_config(&parsed.config);
    // The whole gradient, not its first colour: two colours and the angle
    // they run at are what that line says.
    assert_eq!(
        style.active_border.colors(),
        [Color(0xEE33_CCFF), Color(0xEE00_FF99)]
    );
    assert_eq!(style.active_border.angle_degrees(), 45);
    assert_eq!(
        style.inactive_border,
        Gradient::solid(Color(0xFF59_5959)),
        "one colour is a gradient of one"
    );
    assert!(style.inactive_border.is_solid());
    assert_eq!(style.background, Style::BACKGROUND);
}

// -- Operations, pixel by pixel ----------------------------------------------

#[test]
fn a_border_lies_outside_its_rectangle_on_exact_pixels() {
    let mut canvas = canvas(20, 16);
    let full = Damage::full(20, 16);
    let color = Color(0xFF12_3456);
    canvas.border(Rect::new(5, 5, 6, 4), 2, color, &full);
    for y in 0..16 {
        for x in 0..20 {
            let outer = (3..13).contains(&x) && (3..11).contains(&y);
            let inner = (5..11).contains(&x) && (5..9).contains(&y);
            let expected = if outer && !inner { 0x0012_3456 } else { BG };
            assert_eq!(canvas.pixel(x, y), Some(shown(expected)), "({x}, {y})");
        }
    }
    assert_eq!(canvas.take_damage().area(), 10 * 8 - 6 * 4);
}

#[test]
fn a_translucent_border_is_blended_once_at_its_corners() {
    let mut canvas = canvas(8, 8);
    canvas.border(
        Rect::new(2, 2, 4, 4),
        1,
        Color(0x80FF_0000),
        &Damage::full(8, 8),
    );
    // Straight red at alpha 128 over BG: 128 + 32 × 127 / 255, and the
    // other channels BG's × 127 / 255, each rounded.
    let blended = shown(0x0090_2030);
    assert_eq!(canvas.pixel(1, 1), Some(blended));
    assert_eq!(canvas.pixel(3, 1), Some(blended));
    assert_eq!(canvas.pixel(6, 6), Some(blended));
    assert_eq!(canvas.pixel(2, 2), Some(shown(BG)));
}

#[test]
fn source_over_matches_hand_computed_values() {
    let mut canvas = canvas(4, 2);
    let full = Damage::full(4, 2);
    // Premultiplied ARGB8888: half alpha, nothing, opaque, alpha 1.
    let row = [0x8040_2010u32, 0x0000_0000, 0xFF12_3456, 0x0101_0101];
    let data: Vec<u8> = row.iter().flat_map(|pixel| pixel.to_le_bytes()).collect();
    let surface = Surface::new(&data, 4, 1, 16, Format::Argb8888).unwrap();
    canvas.composite(&surface, Rect::new(0, 0, 4, 1), &full);
    // s + d × (255 − a) / 255 rounded, per channel, over 0x204060.
    //   a 128: R 64 + 15.94, G 32 + 31.87, B 16 + 47.81
    //   a 1:   R 1 + 31.87,  G 1 + 63.75,  B 1 + 95.62
    assert_eq!(canvas.pixel(0, 0), Some(shown(0x0050_4040)));
    assert_eq!(canvas.pixel(1, 0), Some(shown(BG)));
    assert_eq!(canvas.pixel(2, 0), Some(shown(0x0012_3456)));
    assert_eq!(canvas.pixel(3, 0), Some(shown(0x0021_4161)));

    // A straight colour: 0x40FF8000 is premultiplied to R 64, G 32.125.
    //   R 64 + 32 × 191 / 255 = 87.97, G 32.125 + 47.94 = 80.06,
    //   B 96 × 191 / 255 = 71.91.
    canvas.fill(Rect::new(0, 1, 1, 1), Color(0x40FF_8000), &full);
    assert_eq!(canvas.pixel(0, 1), Some(shown(0x0058_5048)));
    // No alpha draws nothing and damages nothing.
    let _ = canvas.take_damage();
    canvas.fill(Rect::new(1, 1, 1, 1), Color(0x00FF_FFFF), &full);
    assert_eq!(canvas.pixel(1, 1), Some(shown(BG)));
    assert!(canvas.damage().is_empty());

    // Exhaustively for one destination: every alpha, one colour value each.
    for alpha in 0..=255u32 {
        let mut canvas = canvas_of(1, 1);
        let value = alpha / 2;
        let pixel = (alpha << 24) | (value << 16) | (value << 8) | value;
        let data = pixel.to_le_bytes();
        let surface = Surface::new(&data, 1, 1, 4, Format::Argb8888).unwrap();
        canvas.composite(&surface, Rect::new(0, 0, 1, 1), &Damage::full(1, 1));
        let channel = |d: u32| value + (d * (255 - alpha) + 127) / 255;
        let expected = (channel(0x20) << 16) | (channel(0x40) << 8) | channel(0x60);
        assert_eq!(canvas.pixel(0, 0), Some(shown(expected)), "alpha {alpha}");
    }
}

fn canvas_of(width: u32, height: u32) -> Canvas {
    canvas(width, height)
}

#[test]
fn an_xrgb_surface_is_copied_opaque_whatever_its_x_byte() {
    let mut canvas = canvas(3, 1);
    let data: Vec<u8> = [0x0011_2233u32, 0x8044_5566, 0xFF77_8899]
        .iter()
        .flat_map(|pixel| pixel.to_le_bytes())
        .collect();
    let surface = Surface::new(&data, 3, 1, 12, Format::Xrgb8888).unwrap();
    canvas.composite(&surface, Rect::new(0, 0, 3, 1), &Damage::full(3, 1));
    assert_eq!(
        pixels(&canvas),
        [shown(0x0011_2233), shown(0x0044_5566), shown(0x0077_8899)]
    );
}

#[test]
fn a_surface_is_drawn_one_to_one_and_cropped_to_its_rectangle() {
    for format in [Format::Xrgb8888, Format::Argb8888] {
        // A 4x3 surface of distinct opaque pixels, into a 3x4 rectangle at
        // (1, 1) on a 6x6 canvas: column 3 is cropped, row 3 is background.
        let value = |x: u32, y: u32| 0xFF00_0000 | (x << 16) | (y << 8) | 0x7F;
        let data: Vec<u8> = (0..3)
            .flat_map(|y| (0..4).map(move |x| (x, y)))
            .flat_map(|(x, y)| value(x, y).to_le_bytes())
            .collect();
        let surface = Surface::new(&data, 4, 3, 16, format).unwrap();
        let mut canvas = canvas(6, 6);
        canvas.composite(&surface, Rect::new(1, 1, 3, 4), &Damage::full(6, 6));
        for y in 0..6 {
            for x in 0..6 {
                let expected = if (1..4).contains(&x) && (1..4).contains(&y) {
                    value(x - 1, y - 1)
                } else {
                    shown(BG)
                };
                assert_eq!(canvas.pixel(x, y), Some(expected), "{format:?} ({x}, {y})");
            }
        }
        assert_eq!(canvas.take_damage(), Damage::from(Rect::new(1, 1, 3, 3)));
    }
}

#[test]
fn a_padded_surface_draws_as_a_tight_one() {
    for format in [Format::Xrgb8888, Format::Argb8888] {
        let (width, height) = (37, 21);
        let tight = Pattern::Gradient.draw(width, height);
        let stride = width * 4 + 12;
        let mut padded = vec![0x5Au8; (stride * height) as usize];
        for (row, source) in padded
            .chunks_mut(stride as usize)
            .zip(tight.chunks(width as usize * 4))
        {
            row[..source.len()].copy_from_slice(source);
        }
        let rect = Rect::new(3, 2, i64::from(width), i64::from(height));
        let full = Damage::full(50, 30);
        let mut a = canvas(50, 30);
        let mut b = canvas(50, 30);
        a.composite(
            &Surface::new(&tight, width, height, width * 4, format).unwrap(),
            rect,
            &full,
        );
        b.composite(
            &Surface::new(&padded, width, height, stride, format).unwrap(),
            rect,
            &full,
        );
        assert_eq!(a.data(), b.data(), "{format:?}");

        // And a part of it, which is all of a padded buffer that is
        // gathered: two cells of a terminal, away from the surface's corner
        // and from each other, come out as the whole surface drew them, at
        // an opacity that takes the opaque one off the copying path too.
        let cells: Damage = [Rect::new(9, 5, 7, 4), Rect::new(30, 17, 6, 3)]
            .into_iter()
            .collect();
        for opacity in [1.0, 0.5] {
            let mut whole = canvas(50, 30);
            let mut part = canvas(50, 30);
            let surface = Surface::new(&padded, width, height, stride, format).unwrap();
            whole.composite_with(&surface, rect, Rounding::none(), opacity, &full);
            part.composite_with(&surface, rect, Rounding::none(), opacity, &cells);
            let untouched = canvas(50, 30);
            for (x, y) in (0..30).flat_map(|y| (0..50).map(move |x| (x, y))) {
                let from = if cells.contains(i64::from(x), i64::from(y)) {
                    &whole
                } else {
                    &untouched
                };
                assert_eq!(
                    part.pixel(x, y),
                    from.pixel(x, y),
                    "{format:?} {opacity} at {x},{y}"
                );
            }
        }
    }
}

#[test]
fn presenting_honours_a_stride_wider_than_the_pixels() {
    let (width, height) = (5u32, 4u32);
    let stride = width * 4 + 8;
    let mut canvas = canvas(width, height);
    canvas.fill(
        Rect::new(1, 1, 3, 2),
        Color(0xFFAB_CDEF),
        &Damage::full(width, height),
    );
    // The last row may stop at its last pixel.
    let mut bytes = vec![0xEEu8; (stride * (height - 1) + width * 4) as usize];
    let mut target = Target::new(&mut bytes, width, height, stride).unwrap();
    canvas
        .present(&mut target, &Damage::full(width, height))
        .unwrap();
    for y in 0..height {
        let row = &bytes[(y * stride) as usize..];
        for x in 0..width {
            let offset = x as usize * 4;
            let pixel = u32::from_le_bytes(row[offset..offset + 4].try_into().unwrap());
            assert_eq!(Some(pixel), canvas.pixel(x, y), "({x}, {y})");
        }
        if y + 1 < height {
            assert_eq!(
                row[width as usize * 4..stride as usize],
                [0xEE; 8],
                "padding"
            );
        }
    }
}

#[test]
fn presenting_writes_only_the_damage_and_needs_the_canvas_size() {
    let mut canvas = canvas(4, 4);
    canvas.clear(Color(0x0000_00FF), &Damage::full(4, 4));
    let mut bytes = vec![0u8; 64];
    let mut target = Target::new(&mut bytes, 4, 4, 16).unwrap();
    canvas
        .present(&mut target, &Damage::from(Rect::new(1, 2, 2, 1)))
        .unwrap();
    for (index, pixel) in bytes.chunks(4).enumerate() {
        let inside = index == 9 || index == 10;
        let expected: &[u8] = if inside { &[0xFF, 0, 0, 0xFF] } else { &[0; 4] };
        assert_eq!(pixel, expected, "pixel {index}");
    }

    let mut small = vec![0u8; 36];
    let mut target = Target::new(&mut small, 3, 3, 12).unwrap();
    assert_eq!(
        canvas.present(&mut target, &Damage::full(4, 4)),
        Err(Error::Mismatch {
            canvas: (4, 4),
            target: (3, 3)
        })
    );
}

#[test]
fn drawing_is_clipped_to_the_damage_and_reports_what_it_wrote() {
    let mut canvas = canvas(10, 10);
    let damage: Damage = [Rect::new(1, 1, 3, 3), Rect::new(6, 2, 10, 2)]
        .into_iter()
        .collect();
    canvas.fill(Rect::new(0, 0, 10, 10), Color(0xFF00_FF00), &damage);
    for y in 0..10 {
        for x in 0..10 {
            let expected = if damage.contains(i64::from(x), i64::from(y)) {
                0x0000_FF00
            } else {
                BG
            };
            assert_eq!(canvas.pixel(x, y), Some(shown(expected)), "({x}, {y})");
        }
    }
    // What was written is the damage on the canvas: the second rectangle
    // loses its columns past the edge.
    let written = canvas.take_damage();
    assert_eq!(written.area(), 9 + 8);
    assert_eq!(written, damage.clipped(canvas.bounds()));
    assert!(canvas.damage().is_empty());

    // Nothing is drawn outside the canvas or with no damage.
    canvas.fill(
        Rect::new(20, 20, 5, 5),
        Color(0xFFFF_FFFF),
        &Damage::full(10, 10),
    );
    canvas.fill(Rect::new(0, 0, 10, 10), Color(0xFFFF_FFFF), &Damage::new());
    assert!(canvas.take_damage().is_empty());
}

#[test]
fn overlapping_damage_blends_a_translucent_fill_once() {
    let damage: Damage = [Rect::new(0, 0, 3, 3), Rect::new(1, 1, 3, 3)]
        .into_iter()
        .collect();
    assert_eq!(damage.area(), 9 + 9 - 4);
    let mut canvas = canvas(4, 4);
    canvas.fill(Rect::new(0, 0, 4, 4), Color(0x80FF_0000), &damage);
    assert_eq!(canvas.pixel(0, 0), Some(shown(0x0090_2030)));
    assert_eq!(canvas.pixel(2, 2), Some(shown(0x0090_2030)), "the overlap");
    assert_eq!(canvas.pixel(3, 0), Some(shown(BG)));
}

#[test]
fn damage_is_a_region_of_disjoint_rectangles() {
    let mut damage = Damage::new();
    damage.add(Rect::new(0, 0, 10, 10));
    damage.add(Rect::new(2, 2, 3, 3));
    assert_eq!(damage.rects(), [Rect::new(0, 0, 10, 10)], "contained");
    damage.add(Rect::new(5, 5, 10, 10));
    damage.add(Rect::new(0, 0, 0, 5));
    assert_eq!(damage.area(), 100 + 100 - 25);
    for (i, a) in damage.rects().iter().enumerate() {
        for b in &damage.rects()[i + 1..] {
            assert_eq!(crate::damage::intersect(*a, *b), None, "{a:?} and {b:?}");
        }
    }
    assert!(damage.contains(14, 14));
    assert!(!damage.contains(15, 14));
    assert!(!damage.contains(12, 2));
    assert_eq!(damage.bounds(), Some(Rect::new(0, 0, 15, 15)));
    assert_eq!(damage.clipped(Rect::new(0, 0, 8, 8)).area(), 64);

    // Past the limit it becomes its bounding box, and fills up from there
    // again: never more rectangles than the limit and the box that replaced
    // them, and never a pixel dropped, since a collapse only ever adds.
    let mut many = Damage::new();
    for i in 0..40 {
        many.add(Rect::new(i * 2, 0, 1, 1));
        assert!(many.rects().len() <= crate::damage::MAX_RECTS + 1);
    }
    assert!(many.rects().len() < 40, "the rectangles were collapsed");
    for i in 0..40 {
        assert!(
            many.contains(i * 2, 0),
            "the column at {} was dropped",
            i * 2
        );
    }
    assert_eq!(many.bounds(), Some(Rect::new(0, 0, 79, 1)));
    // The gaps between the columns the first collapse swallowed are in the
    // region now; the ones added after it are still their own rectangles.
    assert!(many.contains(1, 0));
    assert!(!many.contains(67, 0));
}

#[test]
fn a_frame_with_partial_damage_leaves_the_rest_alone() {
    let (mut state, before) = two_clients();
    let buffers = client_buffers(&before);
    let surfaces = surfaces(&buffers);
    let style = plain_style();
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let _ = render(
        &mut canvas,
        &before,
        (0, 0),
        &style,
        &surfaces,
        &Damage::full(WIDTH, HEIGHT),
    );
    let first = canvas.data().to_vec();

    // Moving focus to the checkerboard changes both borders and nothing
    // else, so the damage is the two outer rectangles.
    let bind = parse("t.conf", "bind = SUPER, H, movefocus, l\n", &mut NoSources)
        .config
        .binds
        .remove(0);
    assert!(!state.dispatch_bind(&bind).unwrap().is_empty());
    let after = state.layout().remove(0);
    let damage = damage_between(&before, &after, (0, 0));
    let expected: Damage = before.windows.iter().map(outer).collect();
    assert_eq!(damage, expected);

    let _ = canvas.take_damage();
    let produced = render(&mut canvas, &after, (0, 0), &style, &surfaces, &damage);
    assert_eq!(produced, damage);
    assert_eq!(canvas.take_damage(), damage);

    // A full redraw of the new layout is the same frame.
    let mut fresh = Canvas::new(WIDTH, HEIGHT).unwrap();
    let _ = render(
        &mut fresh,
        &after,
        (0, 0),
        &style,
        &surfaces,
        &Damage::full(WIDTH, HEIGHT),
    );
    assert_eq!(canvas.data(), fresh.data());
    // And the damaged frame differs from the first only inside the damage.
    for (index, (old, new)) in first.chunks(4).zip(canvas.data().chunks(4)).enumerate() {
        let (x, y) = (
            index as i64 % i64::from(WIDTH),
            index as i64 / i64::from(WIDTH),
        );
        if old != new {
            assert!(
                damage.contains(x, y),
                "({x}, {y}) changed outside the damage"
            );
        }
    }
    assert_eq!(canvas.pixel(20, 300), Some(0xFFFF_FFFF));
    assert_eq!(canvas.pixel(517, 300), Some(0xFF44_4444));
}

#[test]
fn a_focus_change_redraws_the_two_borders_and_nothing_inside() {
    // Square and rounded: under a rounded corner the border's fill shows,
    // so the corner is part of the ring.
    for rounding in [Rounding::none(), Rounding::circle(12)] {
        let (mut state, before) = two_clients();
        let buffers = client_buffers(&before);
        let surfaces = surfaces(&buffers);
        let style = Style {
            rounding,
            ..plain_style()
        };
        let styles = Styles::plain(&style);
        let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
        let _ = render(
            &mut canvas,
            &before,
            (0, 0),
            &style,
            &surfaces,
            &Damage::full(WIDTH, HEIGHT),
        );

        let bind = parse("t.conf", "bind = SUPER, H, movefocus, l\n", &mut NoSources)
            .config
            .binds
            .remove(0);
        assert!(!state.dispatch_bind(&bind).unwrap().is_empty());
        let after = state.layout().remove(0);
        let damage = damage_between_styled(&before, &after, (0, 0), &styles);
        let whole: Damage = before.windows.iter().map(outer).collect();
        assert!(
            damage.area() < whole.area() / 10,
            "a focus change damaged {} pixels of the windows' {}",
            damage.area(),
            whole.area()
        );

        // Drawn through that damage alone, the frame is the one a full
        // redraw of the new layout makes.
        let _ = render(&mut canvas, &after, (0, 0), &style, &surfaces, &damage);
        let mut fresh = Canvas::new(WIDTH, HEIGHT).unwrap();
        let _ = render(
            &mut fresh,
            &after,
            (0, 0),
            &style,
            &surfaces,
            &Damage::full(WIDTH, HEIGHT),
        );
        assert!(
            canvas.data() == fresh.data(),
            "the ring left a pixel of the old focus behind (rounding {rounding:?})"
        );
    }
}

#[test]
fn a_focus_change_that_raises_a_tiled_window_redraws_only_the_borders() {
    // What a compositor does on a focus change: the focused window is drawn
    // last. Tiled windows do not overlap, so the order changes no pixel.
    let (_, before) = two_clients();
    let buffers = client_buffers(&before);
    let surfaces = surfaces(&buffers);
    let style = plain_style();
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let _ = render(
        &mut canvas,
        &before,
        (0, 0),
        &style,
        &surfaces,
        &Damage::full(WIDTH, HEIGHT),
    );
    let mut after = before.clone();
    after.windows.reverse();
    for placed in &mut after.windows {
        placed.focused = !placed.focused;
    }
    let damage = damage_between_styled(&before, &after, (0, 0), &Styles::plain(&style));
    let whole: Damage = before.windows.iter().map(outer).collect();
    assert!(
        damage.area() < whole.area() / 10,
        "a raise between tiled windows damaged {} pixels of the windows' {}",
        damage.area(),
        whole.area()
    );
    let _ = render(&mut canvas, &after, (0, 0), &style, &surfaces, &damage);
    let mut fresh = Canvas::new(WIDTH, HEIGHT).unwrap();
    let _ = render(
        &mut fresh,
        &after,
        (0, 0),
        &style,
        &surfaces,
        &Damage::full(WIDTH, HEIGHT),
    );
    assert!(
        canvas.data() == fresh.data(),
        "the raise left a pixel behind"
    );
}

#[test]
fn a_raise_between_overlapping_windows_redraws_where_they_overlap() {
    // Two windows moved to overlap, then their order swapped: the overlap
    // is owed, and a frame drawn through the damage is the whole frame.
    let (_, mut before) = two_clients();
    let shift = before.windows[1].rect.x - before.windows[0].rect.x - 40;
    before.windows[1].rect.x -= shift;
    before.windows[1].floating = true;
    let buffers = client_buffers(&before);
    let surfaces = surfaces(&buffers);
    // No blur: a translucent window blurring the frame as it stands is owed
    // all of itself when anything under it changes, which is the
    // compositor's rule (`hyprix`'s `blurs_whole`), not this function's.
    let style = Style {
        blur: None,
        ..plain_style()
    };
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let _ = render(
        &mut canvas,
        &before,
        (0, 0),
        &style,
        &surfaces,
        &Damage::full(WIDTH, HEIGHT),
    );
    let mut after = before.clone();
    after.windows.reverse();
    let damage = damage_between_styled(&before, &after, (0, 0), &Styles::plain(&style));
    assert!(
        !damage.is_empty(),
        "a restack of overlapping windows owes their overlap"
    );
    let _ = render(&mut canvas, &after, (0, 0), &style, &surfaces, &damage);
    let mut fresh = Canvas::new(WIDTH, HEIGHT).unwrap();
    let _ = render(
        &mut fresh,
        &after,
        (0, 0),
        &style,
        &surfaces,
        &Damage::full(WIDTH, HEIGHT),
    );
    assert!(
        canvas.data() == fresh.data(),
        "the restack left a pixel behind"
    );
}

#[test]
fn a_focus_change_that_fades_or_dims_a_window_redraws_all_of_it() {
    let (mut state, before) = two_clients();
    let bind = parse("t.conf", "bind = SUPER, H, movefocus, l\n", &mut NoSources)
        .config
        .binds
        .remove(0);
    assert!(!state.dispatch_bind(&bind).unwrap().is_empty());
    let after = state.layout().remove(0);
    let whole: Damage = before.windows.iter().map(outer).collect();
    for style in [
        Style {
            inactive_opacity: 0.8,
            ..plain_style()
        },
        Style {
            dim: 0.5,
            ..plain_style()
        },
    ] {
        let damage = damage_between_styled(&before, &after, (0, 0), &Styles::plain(&style));
        assert_eq!(damage, whole);
    }
}

#[test]
fn a_layout_on_a_monitor_away_from_the_origin_is_drawn_in_its_coordinates() {
    let mut state = State::new(Settings::default());
    let _ = state
        .add_monitor(Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: "Virtual-1".to_owned(),
            id: MonitorId(7),
            rect: Rect::new(1920, 100, 200, 100),
            reserved: Gaps::all(0),
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .unwrap();
    let _ = state.open_window(WindowId(3)).unwrap();
    let layout = state.layout().remove(0);
    assert_eq!(layout.windows[0].rect, Rect::new(1941, 121, 158, 58));
    let mut canvas = Canvas::new(200, 100).unwrap();
    let produced = render(
        &mut canvas,
        &layout,
        (1920, 100),
        &unshadowed(),
        &BTreeMap::new(),
        &Damage::full(200, 100),
    );
    assert_eq!(produced, Damage::full(200, 100));
    assert_eq!(
        canvas.pixel(20, 20),
        Some(0xFFFF_FFFF),
        "the focused border"
    );
    assert_eq!(canvas.pixel(21, 21), Some(0xFF11_1111), "no surface yet");
    assert_eq!(canvas.pixel(19, 20), Some(0xFF11_1111));
    assert_eq!(
        damage_between(
            &layout,
            &MonitorLayout {
                windows: Vec::new(),
                ..layout.clone()
            },
            (1920, 100)
        ),
        Damage::from(Rect::new(20, 20, 160, 60))
    );
}

#[test]
fn buffers_are_checked_when_described() {
    let data = [0u8; 16];
    assert!(Surface::new(&data, 2, 2, 8, Format::Argb8888).is_ok());
    assert_eq!(
        Surface::new(&data, 2, 2, 7, Format::Argb8888).unwrap_err(),
        Error::Stride {
            width: 2,
            stride: 7
        }
    );
    assert_eq!(
        Surface::new(&data, 2, 3, 8, Format::Xrgb8888).unwrap_err(),
        Error::Short {
            needed: 24,
            len: 16
        }
    );
    assert_eq!(
        Surface::new(&data, 0, 1, 0, Format::Xrgb8888).unwrap_err(),
        Error::Size {
            width: 0,
            height: 1
        }
    );
    let mut bytes = [0u8; 16];
    assert!(Target::new(&mut bytes, 1, 1, 4).is_ok());
    assert!(Target::new(&mut bytes, 17, 1, 68).is_err());
    assert!(Canvas::new(crate::MAX_SIZE + 1, 1).is_err());
    assert_eq!(
        Format::from_wl_shm(Format::Xrgb8888.wl_shm()),
        Some(Format::Xrgb8888)
    );
    assert_eq!(Format::from_wl_shm(2), None);
    // DRM_FORMAT_ARGB8888 and DRM_FORMAT_XRGB8888 from drm_fourcc.h.
    assert_eq!(Format::Argb8888.fourcc(), 0x3432_5241);
    assert_eq!(Format::Xrgb8888.fourcc(), 0x3432_5258);
}

#[test]
fn the_patterns_are_what_their_docs_say() {
    let board = Pattern::Checkerboard.draw(40, 20);
    assert_eq!(board.len(), 40 * 20 * 4);
    assert_eq!(board[..4], [0xE0, 0xE0, 0xE0, 0x00]);
    assert_eq!(Pattern::Checkerboard.pixel(16, 0, 20), Pattern::DARK);
    assert_eq!(Pattern::Checkerboard.pixel(16, 16, 20), Pattern::LIGHT);
    assert_eq!(Pattern::Gradient.pixel(0, 0, 20), 0xFF00_0080);
    assert_eq!(Pattern::Gradient.pixel(33, 9, 20), 0xFF10_0080);
    // Bottom half, alpha 0xC0. Cell 1 of the gradient is 1 × 8 = 8, so red
    // and green are 8 × 192 / 255 = 6.02, and blue 128 × 192 / 255 = 96.4.
    assert_eq!(Pattern::Gradient.pixel(16, 16, 20), 0xC006_0660);
    // Premultiplied: no channel above alpha.
    for y in 0..40 {
        let pixel = Pattern::Gradient.pixel(1000, y * 16, 640);
        let alpha = pixel >> 24;
        assert!(
            [16, 8, 0]
                .iter()
                .all(|shift| (pixel >> shift) & 0xFF <= alpha)
        );
    }
}

/// The blur is behind a window that can be seen through and nowhere else.
///
/// Every pixel it changes is inside the gradient's rectangle: the gradient
/// is the only surface with an alpha channel, and blurring behind an opaque
/// window costs a pyramid of passes and changes nothing anybody can see.
#[test]
fn only_a_translucent_window_has_its_background_blurred() {
    let (_, layout) = two_clients();
    let style = Style {
        blur: Some(Blur::new(16, 2)),
        shadow: None,
        dim: 0.0,
        ..plain_style()
    };
    let blurred = frame_with(&style, &[]);
    let plain = frame_with(
        &Style {
            blur: None,
            ..style
        },
        &[],
    );

    let inside = |placed: &Placed, x: i64, y: i64| {
        x >= placed.rect.x
            && x < placed.rect.x + placed.rect.width
            && y >= placed.rect.y
            && y < placed.rect.y + placed.rect.height
    };
    let gradient = layout
        .windows
        .iter()
        .find(|placed| placed.window == GRADIENT)
        .copied()
        .unwrap();
    let checkerboard = layout
        .windows
        .iter()
        .find(|placed| placed.window == CHECKERBOARD)
        .copied()
        .unwrap();

    let mut changed = 0_u32;
    let mut stray = 0_u32;
    let mut over_the_opaque = 0_u32;
    for y in 0..i64::from(HEIGHT) {
        for x in 0..i64::from(WIDTH) {
            let at = ((y * i64::from(WIDTH) + x) * 4) as usize;
            if blurred.get(at..at + 4) == plain.get(at..at + 4) {
                continue;
            }
            changed += 1;
            if inside(&checkerboard, x, y) {
                over_the_opaque += 1;
            } else if !inside(&gradient, x, y) {
                stray += 1;
            }
        }
    }
    assert!(changed > 1000, "the blur changed only {changed} pixels");
    assert_eq!(
        over_the_opaque, 0,
        "the opaque window's pixels were blurred"
    );
    assert_eq!(stray, 0, "{stray} pixels outside any window changed");
}

/// And over something that is not flat, it blurs: the canvas's own pixels,
/// which is where a window over another window would read from.
#[test]
fn the_canvas_blurs_what_is_drawn_on_it() {
    let mut canvas = Canvas::new(128, 128).unwrap();
    let full = Damage::full(128, 128);
    canvas.clear(Color(0xFF00_0000), &full);
    canvas.fill(Rect::new(32, 32, 64, 64), Color(0xFFFF_FFFF), &full);
    let before: Vec<u32> = (0..128).map(|x| canvas.pixel(x, 64).unwrap()).collect();

    canvas.blur(
        Rect::new(0, 0, 128, 128),
        Rounding::circle(0),
        &Blur::ungraded(8, 2),
        &full,
    );
    let after: Vec<u32> = (0..128).map(|x| canvas.pixel(x, 64).unwrap()).collect();
    assert_ne!(before, after, "nothing was blurred");

    // The hard edge at 32 became a gradient: the pixel just outside the
    // square is no longer the background it was.
    assert_eq!(before[30] & 0xFF, 0x00);
    assert!(after[30] & 0xFF > 0, "the edge did not spread outwards");
    // And the frame is still opaque, which presenting depends on.
    assert!(after.iter().all(|pixel| pixel >> 24 == 0xFF));
}

/// `blur:enabled = 0` turns it off, and then the frame is the one with no
/// blur at all -- to the byte, which is what a compositor that skipped the
/// pass rather than running it with no effect produces.
#[test]
fn the_blur_can_be_turned_off() {
    let config = parse("test", "decoration:blur:enabled = 0\n", &mut NoSources).config;
    let style = Style::from_config(&config);
    assert_eq!(style.blur, None);
    assert_eq!(
        frame_with(&style, &[]),
        frame_with(
            &Style {
                blur: None,
                ..plain_style()
            },
            &[]
        )
    );
}

/// What the software fallback costs, which `docs/ROADMAP.md` stage 19 asks
/// each effect to have stated: the worst frame this renderer draws, timed.
///
/// The worst frame is every decoration at once over the whole screen --
/// rounded corners, a shadow under each window, the dim over the unfocused
/// one, and a dual-Kawase blur behind both, since `inactive_opacity` makes
/// both translucent. The blur is nearly all of it: the same frame without it
/// is under a fifth of the time.
///
/// Only in release, and only as a ceiling: a test that asserted a debug
/// build's time would be asserting the optimiser's absence, and one that
/// asserted a tight number would fail on a slower machine for no reason
/// anybody could act on. What it catches is a change that makes a frame
/// several times more expensive.
#[test]
fn a_frame_with_every_effect_is_inside_the_stated_bound() {
    /// The bound, in milliseconds. About 110 ms on the machine this was
    /// written on, so the ceiling is a little over twice that: a slower
    /// machine passes, and an effect that doubled in cost does not.
    const BOUND: u128 = 250;

    if cfg!(debug_assertions) {
        // Not a failure: `cargo test` is a debug build, and the number a
        // debug build gives says nothing about the fallback's cost.
        return;
    }
    // Once to warm whatever the allocator and the caches need, then the
    // slowest of three, which is what a frame has to fit inside.
    let _ = decorated_frame();
    let mut slowest = 0;
    for _ in 0..3 {
        let began = std::time::Instant::now();
        let _ = decorated_frame();
        slowest = slowest.max(began.elapsed().as_millis());
    }
    assert!(
        slowest <= BOUND,
        "the worst frame took {slowest} ms, past the {BOUND} ms this renderer's software \
         fallback is stated to be inside"
    );
}

// -- The real configuration: a gradient border and a graded blur -------------

/// The `general` block of the configuration this compositor is meant to
/// draw faithfully, as `~/.config/hypr/hyprland.conf` writes it:
///
/// ```text
/// general {
///     border_size = 2
///     col.active_border = rgba(33ccffee) rgba(00ff99ee) 45deg
///     col.inactive_border = rgba(595959aa)
/// }
/// ```
fn gradient_config() -> compositor_config::Config {
    let parsed = parse(
        "hyprland.conf",
        "general:border_size = 2\n\
         general:col.active_border = rgba(33ccffee) rgba(00ff99ee) 45deg\n\
         general:col.inactive_border = rgba(595959aa)\n",
        &mut NoSources,
    );
    assert_eq!(parsed.diagnostics, []);
    parsed.config
}

/// That block's style, with the shadow and the blur off so that what a test
/// looks at is the border and not what is drawn over or under it.
fn gradient_style() -> Style {
    let style = Style::from_config(&gradient_config()).undithered();
    assert_eq!(
        style.active_border.colors(),
        [Color(0xEE33_CCFF), Color(0xEE00_FF99)],
        "both colours of the gradient reached the style"
    );
    assert_eq!(style.active_border.angle_degrees(), 45);
    assert_eq!(
        style.inactive_border,
        Gradient::solid(Color(0xAA59_5959)),
        "one colour is a gradient of one"
    );
    Style {
        shadow: None,
        blur: None,
        ..style
    }
}

/// The two clients drawn with `style`, tiled the way `settings` tiles them.
fn frame_tiled(style: &Style, settings: Settings) -> Vec<u8> {
    frame_on((WIDTH, HEIGHT), style, settings)
}

/// The same on a monitor of `size`.
fn frame_on(size: (u32, u32), style: &Style, settings: Settings) -> Vec<u8> {
    let (width, height) = size;
    let (_, layout) = two_clients_on(size, settings);
    let buffers = client_buffers(&layout);
    let mut canvas = Canvas::new(width, height).unwrap();
    let full = Damage::full(width, height);
    let produced = render(
        &mut canvas,
        &layout,
        (0, 0),
        style,
        &surfaces(&buffers),
        &full,
    );
    assert_eq!(produced, full);
    canvas.data().to_vec()
}

/// The frame that configuration's `general` block draws: a two-pixel border
/// running from blue to green across the focused window, and a flat one
/// round the other.
#[test]
fn the_gradient_border_matches_the_expected_image() {
    let frame = frame_tiled(&gradient_style(), Settings::from_config(&gradient_config()));
    golden::check("gradient-border-two-clients", WIDTH, HEIGHT, &frame);
}

/// The gradient is a gradient: two pixels along its angle are two colours,
/// and which colour is where says it runs the way Hyprland runs it -- at
/// `45deg`, from the window's top-left corner towards its bottom-right one.
///
/// A gradient of one colour is still flat, which is what
/// `col.inactive_border` asks for.
#[test]
fn a_gradient_border_runs_from_corner_to_corner() {
    let settings = Settings::from_config(&gradient_config());
    let (_, layout) = two_clients_tiled(settings);
    let frame = frame_tiled(&gradient_style(), settings);
    let at = |x: i64, y: i64| -> u32 {
        let start = ((y * i64::from(WIDTH) + x) * 4) as usize;
        u32::from_le_bytes([
            frame[start],
            frame[start + 1],
            frame[start + 2],
            frame[start + 3],
        ])
    };
    let blue = |pixel: u32| pixel & 0xFF;
    let green = |pixel: u32| (pixel >> 8) & 0xFF;

    let focused = layout
        .windows
        .iter()
        .find(|placed| placed.focused)
        .copied()
        .expect("a focused window");
    assert_eq!(focused.border, 2, "general:border_size = 2");
    let border = outer(&focused);
    // Two corners of the border's own box, which are the two ends of the
    // gradient at this angle.
    let (start, end) = (
        at(border.x, border.y),
        at(border.right() - 1, border.bottom() - 1),
    );
    assert_ne!(
        start, end,
        "the border is one colour: {start:08x} at both corners"
    );
    // `rgba(33ccffee)` is blue and `rgba(00ff99ee)` green, so the first
    // colour has more blue than green and the last the other way round.
    // Finding them at these two corners is what says 45 degrees turns the
    // bands clockwise from vertical rather than the other way.
    assert!(
        blue(start) > green(start),
        "the top-left corner is not the first colour: {start:08x}"
    );
    assert!(
        green(end) > blue(end),
        "the bottom-right corner is not the last colour: {end:08x}"
    );
    // Across the top edge it moves as well, and by less: the shader's
    // progress at 45 degrees is `0.707·y + 0.293·x`, so a step across is
    // under half a step down.
    let top_right = at(border.right() - 1, border.y);
    let bottom_left = at(border.x, border.bottom() - 1);
    assert_ne!(start, top_right, "the top edge is flat");
    let across = green(top_right).abs_diff(green(start));
    let down = green(bottom_left).abs_diff(green(start));
    assert!(
        across < down,
        "the gradient does not lean downwards: {across} across against {down} down"
    );

    // And the window that is not focused has `col.inactive_border`, one
    // colour: every edge of it is that one blend over the background.
    let quiet = layout
        .windows
        .iter()
        .find(|placed| !placed.focused)
        .copied()
        .expect("an unfocused window");
    let quiet_box = outer(&quiet);
    let middle = |from: i64, along: i64| from + along / 2;
    let flat = [
        at(middle(quiet_box.x, quiet_box.width), quiet_box.y),
        at(middle(quiet_box.x, quiet_box.width), quiet_box.bottom() - 1),
        at(quiet_box.x, middle(quiet_box.y, quiet_box.height)),
        at(quiet_box.right() - 1, middle(quiet_box.y, quiet_box.height)),
    ];
    assert!(
        flat.windows(2).all(|pair| pair[0] == pair[1]),
        "a gradient of one colour was not drawn flat: {flat:08x?}"
    );
}

/// Where a gradient's angle points, read off `Gradient::progress` rather
/// than off a window, so that what is checked is the shader's arithmetic
/// and not a blend over a background.
///
/// Against `gradient.glsl`'s `getOkColorForCoordArray1`: the progress is
/// `y·sin(angle) + x·(1 − sin(angle))`, with the coordinate folded into the
/// first quadrant first.
#[test]
fn the_gradient_angle_turns_the_bands_clockwise() {
    let two = |degrees| Gradient::new(&[Color(0xFF00_0000), Color(0xFFFF_FFFF)], degrees);
    let near = |value: f32, expected: f32| (value - expected).abs() < 0.001;

    // Zero: `sin(0)` is zero, so the progress is the coordinate's `x`. The
    // colour runs left to right and the bands are vertical.
    let flat = two(0);
    assert!(near(flat.progress(0.0, 0.0), 0.0));
    assert!(near(flat.progress(1.0, 0.0), 1.0));
    assert!(
        near(flat.progress(0.25, 0.9), 0.25),
        "0 degrees is not horizontal"
    );

    // A quarter turn: `sin` is one, so it is the coordinate's `y`. The
    // colour runs top to bottom and the bands are horizontal -- a vertical
    // line turned a quarter turn clockwise, on a screen whose `y` grows
    // downwards.
    let quarter = two(90);
    assert!(
        quarter.progress(0.9, 0.0) < 0.01,
        "90 degrees is not vertical"
    );
    assert!(quarter.progress(0.1, 1.0) > 0.99);

    // And 45 in between, blended rather than rotated: the first colour at
    // the top-left corner, the last at the bottom-right one, with the
    // shader's `sin` and `1 - sin` for weights rather than a rotation's
    // equal halves.
    let leaning = two(45);
    let sine = 45.0_f32.to_radians().sin();
    assert!(near(leaning.progress(1.0, 0.0), 1.0 - sine));
    assert!(near(leaning.progress(0.0, 1.0), sine));
    assert!(leaning.progress(0.0, 0.0) < 0.01 && leaning.progress(1.0, 1.0) > 0.99);

    // Half a turn folds the coordinate rather than turning the arithmetic
    // round, and the gradient runs right to left.
    let about = two(180);
    assert!(
        about.progress(0.0, 0.5) > about.progress(1.0, 0.5),
        "180 degrees did not turn the gradient round"
    );
}

/// The `decoration` block of the same configuration, blur and all:
///
/// ```text
/// decoration {
///     rounding = 8
///     shadow { enabled = true; range = 4; render_power = 3; color = rgba(1a1a1aee) }
///     blur {
///         enabled = true
///         size = 8
///         passes = 3
///         noise = 0.015
///         contrast = 1.1
///         brightness = 0.9
///         vibrancy = 0.1696
///     }
/// }
/// ```
///
/// The four grading values are put on the style rather than read out of the
/// configuration because `src/user/system/linux/compositor/config`'s option table does not carry
/// the five `blur:` grading options yet:
/// `the_grading_waits_on_the_options_table` below is the test that says so,
/// and the day it fails is the day these come out of the file.
fn graded_style() -> Style {
    let parsed = parse(
        "hyprland.conf",
        "decoration:rounding = 8\n\
         decoration:shadow:enabled = true\n\
         decoration:shadow:range = 4\n\
         decoration:shadow:render_power = 3\n\
         decoration:shadow:color = rgba(1a1a1aee)\n\
         decoration:blur:enabled = true\n\
         decoration:blur:size = 8\n\
         decoration:blur:passes = 3\n",
        &mut NoSources,
    );
    assert_eq!(parsed.diagnostics, []);
    let style = Style::from_config(&parsed.config);
    let blur = style.blur.expect("the blur is on");
    assert_eq!((blur.size, blur.passes), (8, 3));
    Style {
        blur: Some(Blur {
            noise: 0.015,
            contrast: 1.1,
            brightness: 0.9,
            vibrancy: 0.1696,
            ..blur
        }),
        ..style
    }
}

/// A monitor half as wide and half as tall as the rest of this file's,
/// which is what the graded blur's expected image is of.
///
/// Smaller because of the dither: `blur:noise` moves every pixel it touches
/// by a step or two, which is what a run-length encoded image cannot hold,
/// so the one committed image that carries a dither carries it over a
/// quarter of the pixels. The blur, the rounding and the shadow are the
/// same ones; only the screen is smaller.
const SMALL: (u32, u32) = (512, 384);

/// The two clients on the small monitor, over a checkerboard wallpaper,
/// drawn with `style`.
///
/// Over a wallpaper because a tiled window's blur is a blur of what is
/// behind the windows and nothing else, and behind these two with no
/// wallpaper is one colour: a blur of that is the colour, and a dither seen
/// through a window a quarter transparent rounds away to nothing.
fn graded_frame(style: &Style) -> Vec<u8> {
    let (width, height) = SMALL;
    let (_, layout) = two_clients_on(SMALL, Settings::default());
    let buffers = client_buffers(&layout);
    let wallpaper = Pattern::Checkerboard.draw(width, height);
    let layers = [LayerFrame {
        rect: Rect::new(0, 0, i64::from(width), i64::from(height)),
        above: false,
        surface: Some(
            Surface::new(
                &wallpaper,
                width,
                height,
                width * 4,
                Pattern::Checkerboard.format(),
            )
            .unwrap(),
        ),
        dim_around: false,
        blur: false,
        xray: false,
    }];
    let mut canvas = Canvas::new(width, height).unwrap();
    let full = Damage::full(width, height);
    let produced = render_with_layers(
        &mut canvas,
        &layout,
        (0, 0),
        &Styles::plain(style),
        &surfaces(&buffers),
        &layers,
        &full,
    );
    assert_eq!(produced, full);
    canvas.data().to_vec()
}

/// The frame that configuration's `decoration` block draws: the blur behind
/// the window that can be seen through, graded the way its five values ask
/// for, dither and all.
#[test]
fn the_graded_blur_matches_the_expected_image() {
    let style = graded_style();
    let frame = graded_frame(&style);
    // Dither and all: the image is the one place a dither is held, so it is
    // checked to be holding one.
    let blur = style.blur.expect("the blur is on");
    let undithered = graded_frame(&Style {
        blur: Some(Blur { noise: 0.0, ..blur }),
        ..style
    });
    assert_ne!(
        frame, undithered,
        "no dither can be seen in the graded frame"
    );
    golden::check("graded-blur-two-clients", SMALL.0, SMALL.1, &frame);
}

/// The grading changes the blur and nothing else: the same frame with the
/// five values at the ones that do nothing is a different picture, and it
/// differs only where the blur was drawn.
#[test]
fn the_grading_changes_the_blurred_pixels_and_no_others() {
    let style = graded_style();
    let blur = style.blur.expect("the blur is on");
    let graded = frame_with(&style, &[]);
    let ungraded = frame_with(
        &Style {
            blur: Some(Blur::ungraded(blur.size, blur.passes)),
            ..style
        },
        &[],
    );
    assert_ne!(graded, ungraded, "the grading changed nothing");

    let (_, layout) = two_clients();
    let translucent = layout
        .windows
        .iter()
        .find(|placed| placed.window == GRADIENT)
        .copied()
        .expect("the gradient client");
    let mut changed = 0_u32;
    let mut stray = 0_u32;
    for y in 0..i64::from(HEIGHT) {
        for x in 0..i64::from(WIDTH) {
            let at = ((y * i64::from(WIDTH) + x) * 4) as usize;
            if graded.get(at..at + 4) == ungraded.get(at..at + 4) {
                continue;
            }
            changed += 1;
            let inside = x >= translucent.rect.x
                && x < translucent.rect.right()
                && y >= translucent.rect.y
                && y < translucent.rect.bottom();
            if !inside {
                stray += 1;
            }
        }
    }
    assert!(
        changed > 10_000,
        "the grading changed only {changed} pixels of the blur"
    );
    assert_eq!(stray, 0, "{stray} pixels outside the blur changed");
}

/// `misc:background_color` is what shows where no window is.
///
/// Hyprland's default is a very dark grey and a person who sets it expects
/// the colour they wrote, not a constant compiled into the compositor.
#[test]
fn the_background_is_the_colour_the_configuration_names() {
    assert_eq!(Style::default().background, Style::BACKGROUND);
    let parsed = parse(
        "t.conf",
        "misc:background_color = rgb(203040)\n",
        &mut NoSources,
    );
    assert_eq!(parsed.diagnostics, []);
    assert_eq!(
        Style::from_config(&parsed.config).background,
        Color(0xFF20_3040)
    );
}

/// `decoration:rounding_power` cuts the corner by a superellipse rather
/// than a circle.
///
/// `rounding.glsl`'s `distanceWithRounding` is
/// `(|x|^p + |y|^p)^(1/p)`, and `p` is the option. Two is the circle every
/// other compositor draws; above two the corner is squarer, which is the
/// "squircle" a person sets the option for. A compositor that read the
/// option and drew a circle anyway would look right to a reader of the
/// configuration and wrong on the screen.
#[test]
fn the_rounding_power_changes_the_shape_of_the_corner() {
    let (width, height) = (200u32, 200u32);
    let damage = Damage::full(width, height);
    let corner = |power: f32| -> u64 {
        let mut canvas = Canvas::new(width, height).unwrap();
        canvas.clear(Color(0xFF00_0000), &damage);
        canvas.fill_rounded(
            Rect::new(0, 0, 200, 200),
            Rounding { radius: 60, power },
            Color(0xFFFF_FFFF),
            &damage,
        );
        // How many pixels of the top-left 60x60 corner were filled: a
        // squarer corner fills more of it.
        let mut filled = 0;
        for y in 0..60 {
            for x in 0..60 {
                if canvas.pixel(x, y) == Some(0xFFFF_FFFF) {
                    filled += 1;
                }
            }
        }
        filled
    };

    let circle = corner(2.0);
    let squarer = corner(4.0);
    let pinched = corner(1.0);
    assert!(
        squarer > circle,
        "a power of 4 fills {squarer} of the corner and a circle {circle}"
    );
    assert!(
        pinched < circle,
        "a power of 1 fills {pinched} of the corner and a circle {circle}"
    );
    // A circle's quarter is pi/4 of the square, which is what says the
    // default is a circle and not something else that grows with the power.
    let quarter = f64::from(60 * 60) * core::f64::consts::FRAC_PI_4;
    #[expect(
        clippy::cast_precision_loss,
        reason = "a corner is a few thousand pixels"
    )]
    let counted = circle as f64;
    assert!(
        (counted - quarter).abs() / quarter < 0.02,
        "a circle's corner is {counted} pixels and a quarter circle is {quarter}"
    );
    // And a power of 1 is a straight diagonal: half the corner square.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a corner is a few thousand pixels"
    )]
    let diagonal = pinched as f64;
    let half = f64::from(60 * 60) / 2.0;
    assert!(
        (diagonal - half).abs() / half < 0.05,
        "a power of 1 fills {diagonal} pixels and half the corner is {half}"
    );
}

/// `rounding_power`, `border_color`, `decorate` and `opaque` reach the
/// frame from a `windowrule`.
#[test]
fn a_rule_can_change_the_corner_the_border_and_the_alpha() {
    let parsed = parse(
        "t.conf",
        "decoration:rounding = 10\n\
         decoration:rounding_power = 3\n",
        &mut NoSources,
    );
    assert_eq!(parsed.diagnostics, []);
    let style = Style::from_config(&parsed.config);
    assert_eq!(style.rounding.radius, 10);
    assert!((style.rounding.power - 3.0).abs() < f32::EPSILON);

    // And the four fields a rule sets, which the frame reads instead of
    // the style's.
    let rule = crate::WindowStyle {
        rounding_power: Some(5.0),
        border_color: Some(Gradient::solid(Color(0xFF00_FF00))),
        decorate: false,
        opaque: true,
        ..crate::WindowStyle::default()
    };
    assert_eq!(rule.rounding_power, Some(5.0));
    assert_eq!(
        rule.border_color.as_ref().map(Gradient::first),
        Some(Color(0xFF00_FF00))
    );
    assert!(!rule.decorate);
    assert!(rule.opaque);
}

/// A configuration's five grading values reach the style, and a
/// configuration that says nothing gets Hyprland's own.
///
/// Hyprland's defaults are not nothing -- `contrast` is 0.8916, `vibrancy`
/// 0.1696 and `noise` 0.0117 out of the box -- so a compositor that fell
/// back to the values that do nothing would draw a flat grey blur where
/// Hyprland draws a graded one, on a configuration that said only
/// `blur { enabled = true }`.
#[test]
fn the_grading_comes_out_of_the_configuration() {
    let parsed = parse(
        "hyprland.conf",
        "decoration:blur:noise = 0.015\n\
         decoration:blur:contrast = 1.1\n\
         decoration:blur:brightness = 0.9\n\
         decoration:blur:vibrancy = 0.5\n\
         decoration:blur:vibrancy_darkness = 0.25\n",
        &mut NoSources,
    );
    assert_eq!(
        parsed.diagnostics,
        [],
        "every grading option is in the table"
    );
    let blur = Style::from_config(&parsed.config)
        .blur
        .expect("the blur is on by default");
    assert!((blur.noise - 0.015).abs() < 1e-6, "{}", blur.noise);
    assert!((blur.contrast - 1.1).abs() < 1e-6, "{}", blur.contrast);
    assert!((blur.brightness - 0.9).abs() < 1e-6, "{}", blur.brightness);
    assert!((blur.vibrancy - 0.5).abs() < 1e-6, "{}", blur.vibrancy);
    assert!(
        (blur.vibrancy_darkness - 0.25).abs() < 1e-6,
        "{}",
        blur.vibrancy_darkness
    );

    // And a configuration that says none of them gets Hyprland's.
    let plain = Style::default().blur.expect("the blur is on by default");
    assert_eq!(
        (
            plain.noise,
            plain.contrast,
            plain.brightness,
            plain.vibrancy,
            plain.vibrancy_darkness
        ),
        (
            Blur::NOISE,
            Blur::CONTRAST,
            Blur::BRIGHTNESS,
            Blur::VIBRANCY,
            Blur::VIBRANCY_DARKNESS
        )
    );
    // `Style::undithered` is the expected images' own style and nothing
    // else's: it takes the dither off and leaves the rest of the grading.
    let undithered = Style::default()
        .undithered()
        .blur
        .expect("the blur is on by default");
    assert_eq!(undithered.noise, 0.0);
    assert_eq!(undithered.contrast, Blur::CONTRAST);
}

/// A blur over a damaged strip writes the same pixels a blur over the whole
/// window writes, and costs what the strip costs rather than what the
/// window costs.
///
/// This is the difference between a blurred desktop that can be used and
/// one that cannot. A terminal's cursor blinking damages a few hundred
/// pixels; if the blur behind the window is recomputed for the whole window
/// anyway, that blink costs a tenth of a second and the compositor draws at
/// six frames a second with nothing happening. The pixels have to come out
/// identical for the shortcut to be allowed, and they do: a blurred pixel
/// depends on nothing further away than the blur's reach, which is exactly
/// what the region read is grown by.
#[test]
fn a_blur_reads_what_is_damaged_and_writes_the_same_pixels() {
    const BOUND: u128 = 25;
    let (width, height) = (1920u32, 1080u32);
    let rect = Rect::new(10, 10, i64::from(width) - 20, i64::from(height) - 20);
    let blur = Blur::new(8, 3);
    let strip = Rect::new(900, 500, 120, 40);

    let painted = |canvas: &mut Canvas, damage: &Damage| {
        canvas.clear(Color(BG), &Damage::full(width, height));
        canvas.fill(
            Rect::new(100, 100, 700, 500),
            Color(0xFFFF_FFFF),
            &Damage::full(width, height),
        );
        canvas.blur(rect, Rounding::circle(8), &blur, damage);
    };

    let mut whole = Canvas::new(width, height).unwrap();
    painted(&mut whole, &Damage::full(width, height));
    let mut part = Canvas::new(width, height).unwrap();
    painted(&mut part, &Damage::from(strip));

    // Inside the strip the two agree to the byte, and outside it the
    // partial one is the unblurred frame.
    let mut worst = 0_u32;
    for y in strip.y..strip.bottom() {
        for x in strip.x..strip.right() {
            let (x, y) = (u32::try_from(x).unwrap(), u32::try_from(y).unwrap());
            let (a, b) = (whole.pixel(x, y).unwrap(), part.pixel(x, y).unwrap());
            for shift in [0, 8, 16, 24] {
                let (one, two) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
                worst = worst.max(one.abs_diff(two));
            }
        }
    }
    assert_eq!(
        worst, 0,
        "the blur over the damaged strip differs from the blur over the whole window by {worst} \
         of a channel"
    );
    // And a pixel well outside the strip was left alone, which is what says
    // the damage was obeyed rather than ignored. Just outside the white
    // rectangle, where a blur lightens the background and doing nothing
    // does not.
    assert_ne!(
        whole.pixel(805, 300),
        part.pixel(805, 300),
        "a pixel outside the damage was blurred anyway"
    );

    if cfg!(debug_assertions) {
        return;
    }
    // Once to warm the caches, then the slowest of three.
    part.blur(rect, Rounding::circle(8), &blur, &Damage::from(strip));
    let mut slowest = 0;
    for _ in 0..3 {
        let began = std::time::Instant::now();
        part.blur(rect, Rounding::circle(8), &blur, &Damage::from(strip));
        slowest = slowest.max(began.elapsed().as_millis());
    }
    assert!(
        slowest <= BOUND,
        "a blur over a {}x{} strip took {slowest} ms, which is the whole window's cost: the \
         damage is being ignored",
        strip.width,
        strip.height
    );
}

/// What a blurred screen costs, which is the compositor's whole frame
/// budget: a full-screen blur at the size and passes the configuration at
/// the head of this file asks for, timed.
///
/// Hyprland blurs what is behind a translucent window, so a bar or a
/// terminal over a wallpaper puts this on the critical path of every frame
/// it is on. At 1920x1080 with `size = 8` and `passes = 3` it was 304 ms
/// when the pyramid was first written, which is three frames a second; the
/// row-hoisted taps, the shared vertical mix, the contrast table and the
/// cast that replaced `f32::floor` took it to 85 ms on the machine this was
/// written on, and the expected images are the same bytes as before, which
/// is what says the kernel did not change.
///
/// The ceiling is more than twice that measurement, for the reason
/// `a_frame_with_every_effect_is_inside_the_stated_bound` states: a slower
/// machine must pass, and a change that made the blur several times more
/// expensive must not.
#[test]
fn a_full_screen_blur_is_inside_the_stated_bound() {
    /// The bound, in milliseconds.
    const BOUND: u128 = 220;
    /// A screen, which is what a blur behind a full-screen window covers.
    const SCREEN: (u32, u32) = (1920, 1080);

    if cfg!(debug_assertions) {
        return;
    }
    let (width, height) = SCREEN;
    let mut canvas = Canvas::new(width, height).unwrap();
    let full = Damage::full(width, height);
    canvas.clear(Color(BG), &full);
    // Something to blur: a flat screen would be the same pixel everywhere
    // and would not touch the memory a real frame touches.
    canvas.fill(Rect::new(100, 100, 700, 500), Color(0xFFFF_FFFF), &full);
    let rect = Rect::new(10, 10, i64::from(width) - 20, i64::from(height) - 20);
    // Once to warm the caches and the allocator, then the slowest of three.
    canvas.blur(rect, Rounding::circle(8), &Blur::new(8, 3), &full);
    let mut slowest = 0;
    for _ in 0..3 {
        let began = std::time::Instant::now();
        canvas.blur(rect, Rounding::circle(8), &Blur::new(8, 3), &full);
        slowest = slowest.max(began.elapsed().as_millis());
    }
    assert!(
        slowest <= BOUND,
        "a full-screen blur took {slowest} ms, past the {BOUND} ms this renderer is stated to \
         be inside"
    );
}

/// `dim_around` darkens everything behind a window and nothing in front of
/// it.
///
/// What a launcher does to the desktop. Hyprland dims what is *behind* the
/// thing that asked for it; drawing in order means "behind" is "already
/// drawn", so one fill just before that thing is the whole of the effect --
/// no second pass and no second canvas.
#[test]
fn dim_around_darkens_what_is_behind_and_not_what_is_in_front() {
    let (width, height) = (400u32, 300u32);
    let damage = Damage::full(width, height);
    let mut style = plain_style();
    style.dim_around = 0.5;
    style.rounding = Rounding::none();

    // Two windows side by side; the second asks for `dim_around`, so the
    // first is darkened and the second is not.
    let bytes = vec![0xFFu8; (width * height * 4) as usize];
    let first = Surface::new(&bytes, 100, 100, 400, Format::Xrgb8888).expect("a surface");
    let surfaces: BTreeMap<WindowId, Surface<'_>> = [
        (WindowId(1), first),
        (
            WindowId(2),
            Surface::new(&bytes, 100, 100, 400, Format::Xrgb8888).expect("a surface"),
        ),
    ]
    .into_iter()
    .collect();
    let mut windows = BTreeMap::new();
    let _previous = windows.insert(
        WindowId(2),
        crate::WindowStyle {
            dim_around: true,
            ..crate::WindowStyle::default()
        },
    );
    let output = MonitorLayout {
        monitor: MonitorId(1),
        workspace: compositor_layout::WorkspaceId(1),
        windows: vec![
            Placed {
                window: WindowId(1),
                rect: Rect::new(0, 0, 100, 100),
                border: 0,
                focused: false,
                floating: false,
                fullscreen: false,
            },
            Placed {
                window: WindowId(2),
                rect: Rect::new(200, 0, 100, 100),
                border: 0,
                focused: true,
                floating: false,
                fullscreen: false,
            },
        ],
    };

    let mut canvas = Canvas::new(width, height).expect("a canvas");
    let _drawn = render_with_layers(
        &mut canvas,
        &output,
        (0, 0),
        &Styles {
            base: &style,
            windows: &windows,
        },
        &surfaces,
        &[],
        &damage,
    );
    // The first window is white under half-black, so its channels are
    // about 128; the second is drawn after the dim and is still white.
    let behind = canvas.pixel(50, 50).expect("a pixel") & 0xFF;
    let front = canvas.pixel(250, 50).expect("a pixel") & 0xFF;
    assert_eq!(front, 0xFF, "the window that asked for it is not dimmed");
    assert!(
        (100..=160).contains(&behind),
        "the window behind it is {behind} and half of white is about 128"
    );
}

// -- The kept backdrop -------------------------------------------------------

/// A wallpaper that is not flat, so that a blur of it is not the wallpaper:
/// the checkerboard over the whole screen, with a white square at `square`
/// when there is one -- which is a wallpaper that repainted part of itself.
fn wallpaper(square: Option<Rect>) -> Vec<u8> {
    let mut bytes = Pattern::Checkerboard.draw(WIDTH, HEIGHT);
    let Some(rect) = square else {
        return bytes;
    };
    for y in rect.y..rect.bottom() {
        let at = ((y * i64::from(WIDTH) + rect.x) * 4) as usize;
        bytes[at..at + rect.width as usize * 4].fill(0xFF);
    }
    bytes
}

/// One frame of the two clients over `wallpaper`, with a backdrop.
fn frame_over(
    canvas: &mut Canvas,
    backdrop: &mut Backdrop,
    wallpaper: &[u8],
    style: &Style,
    damage: &Damage,
) {
    let (_, layout) = two_clients();
    let buffers = client_buffers(&layout);
    let layers = [LayerFrame {
        rect: Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT)),
        above: false,
        surface: Some(
            Surface::new(
                wallpaper,
                WIDTH,
                HEIGHT,
                WIDTH * 4,
                Pattern::Checkerboard.format(),
            )
            .unwrap(),
        ),
        dim_around: false,
        blur: false,
        xray: false,
    }];
    let _ = render_onto(
        canvas,
        Some(backdrop),
        &layout,
        (0, 0),
        &Styles::plain(style),
        &surfaces(&buffers),
        &layers,
        damage,
    );
}

/// A tiled window's blur is kept, and is blurred again only when what is
/// behind the windows changes.
///
/// This is what makes a blurred desktop usable on a CPU. A pointer crossing
/// a translucent terminal damages a few hundred pixels of it, and a letter
/// typed into it a few hundred more; if either costs a blur, the pointer
/// moves six times a second and the letters arrive like a tty's over a bad
/// line. With the backdrop both cost a copy.
///
/// Three frames, each compared byte for byte with a whole frame drawn from
/// nothing, because a strip that comes out a step of a channel away from
/// the whole frame's is a seam a person can see:
///
/// 1. something was drawn over the window and the strip is redrawn -- the
///    same pixels, and *no blur was run*;
/// 2. the wallpaper repainted a square under the window, and the damage is
///    the square grown by [`Blur::reach`] -- the same pixels, one blur;
/// 3. the same with the damage *not* grown, which must come out wrong: that
///    is what says the reach is owed and the second frame did not pass by
///    drawing the same thing either way.
#[test]
fn a_tiled_window_keeps_the_blur_of_what_is_behind_it() {
    let blur = Blur::new(8, 2);
    let style = Style {
        blur: Some(blur),
        ..plain_style()
    };
    let full = Damage::full(WIDTH, HEIGHT);
    let (_, layout) = two_clients();
    let at = layout
        .windows
        .iter()
        .position(|placed| placed.window == GRADIENT)
        .unwrap();
    assert!(
        reads_backdrop(&layout.windows, at, &Styles::plain(&style)),
        "a tiled window with nothing under it takes its blur from the backdrop"
    );
    let window = layout.windows[at].rect;

    let whole = |wallpaper: &[u8]| {
        let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
        let mut backdrop = Backdrop::new(WIDTH, HEIGHT).unwrap();
        frame_over(&mut canvas, &mut backdrop, wallpaper, &style, &full);
        canvas.data().to_vec()
    };
    let plain = wallpaper(None);
    let expected = whole(&plain);

    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let mut backdrop = Backdrop::new(WIDTH, HEIGHT).unwrap();
    frame_over(&mut canvas, &mut backdrop, &plain, &style, &full);
    assert_eq!(canvas.data(), expected.as_slice());
    let first = backdrop.blurs();
    assert!(first > 0, "the first frame blurred nothing");

    // The gradient's bottom half is the half that can be seen through, so
    // that is where a blur shows and where all of this happens.
    let seen_through = window.y + window.height / 2;

    // 1. A pointer's worth of the window, drawn over and then drawn again.
    let strip = Rect::new(window.x + 40, seen_through + 60, 24, 24);
    canvas.fill(strip, Color(0xFFFF_FFFF), &full);
    assert_ne!(canvas.data(), expected.as_slice());
    frame_over(
        &mut canvas,
        &mut backdrop,
        &plain,
        &style,
        &Damage::from(strip),
    );
    assert!(
        canvas.data() == expected.as_slice(),
        "a strip of the window redrawn alone is not the whole frame's pixels"
    );
    assert_eq!(
        backdrop.blurs(),
        first,
        "redrawing a strip of the window ran a blur, and nothing behind it had changed"
    );

    // 2. The wallpaper repaints a square under the window.
    let square = Rect::new(window.x + 200, seen_through + 150, 40, 40);
    let repainted = wallpaper(Some(square));
    let expected = whole(&repainted);
    let reach = blur.reach();
    let owed = Rect::new(
        square.x - reach,
        square.y - reach,
        square.width + reach * 2,
        square.height + reach * 2,
    );
    let mut kept = (canvas.clone(), backdrop.clone());
    frame_over(
        &mut kept.0,
        &mut kept.1,
        &repainted,
        &style,
        &Damage::from(owed),
    );
    assert!(
        kept.0.data() == expected.as_slice(),
        "a change behind the window, redrawn a reach around, is not the whole frame's pixels"
    );
    assert_eq!(
        kept.1.blurs(),
        first + 1,
        "a change behind the window is one more blur"
    );

    // 3. And without the reach it is not: the blur of the square spreads
    // past the square.
    frame_over(
        &mut canvas,
        &mut backdrop,
        &repainted,
        &style,
        &Damage::from(square),
    );
    assert!(
        canvas.data() != expected.as_slice(),
        "the square alone came out as the whole frame, so nothing here tests the reach"
    );
}

/// Which windows take the backdrop's blur: Hyprland's
/// `shouldUseNewBlurOptimizations`, asked of a layout.
#[test]
fn only_a_window_over_the_desktop_alone_reads_the_backdrop() {
    let placed = |window: u64, rect: Rect, floating: bool| Placed {
        window: WindowId(window),
        rect,
        border: 2,
        focused: false,
        floating,
        fullscreen: false,
    };
    let style = plain_style();
    let windows = [
        placed(1, Rect::new(10, 10, 300, 300), false),
        placed(2, Rect::new(330, 10, 300, 300), false),
        // A scratchpad's window, tiled over the two of them.
        placed(3, Rect::new(100, 100, 400, 100), false),
        placed(4, Rect::new(700, 400, 100, 100), true),
    ];
    let reads = |at: usize| reads_backdrop(&windows, at, &Styles::plain(&style));
    assert!(reads(0) && reads(1), "tiled, and nothing under either");
    assert!(!reads(2), "a window over other windows blurs them");
    assert!(!reads(3), "a floating window blurs what it floats over");
    assert!(!reads(4), "and no window is no window");

    // A window `dim_around` darkened the desktop for is behind a fill the
    // backdrop does not hold, and so is everything drawn after it.
    let mut ruled = BTreeMap::new();
    let _previous = ruled.insert(
        WindowId(1),
        crate::WindowStyle {
            dim_around: true,
            ..crate::WindowStyle::default()
        },
    );
    let dimmed = Styles {
        base: &style,
        windows: &ruled,
    };
    assert!(!reads_backdrop(&windows, 0, &dimmed));
    assert!(!reads_backdrop(&windows, 1, &dimmed));
}

/// `windowrule = xray`, and `decoration:blur:xray` for every window: the
/// blur behind a window is of the wallpaper, whatever it floats over.
///
/// Hyprland's `shouldUseNewBlurOptimizations`: a window's own `xray 0`
/// never reads the kept `m_blurFB`, the option or the window's `xray 1`
/// always does, and otherwise a tiled window does. So the test is what the
/// rule promises, as the layer rule's is: a translucent floating window
/// with it is the same pixels whether or not a window is under it, and
/// without it -- the control -- it is not.
#[test]
fn an_xray_window_blurs_the_wallpaper_and_not_the_window_under_it() {
    let base = Style {
        blur: Some(Blur::new(8, 2)),
        ..plain_style()
    };
    let full = Damage::full(WIDTH, HEIGHT);
    let behind = wallpaper(None);
    // The gradient floated over the middle of the checkerboard.
    let over = Rect::new(120, 200, 400, 300);
    let draw = |style: &Style, rule: Option<bool>, under: bool| {
        let (_, mut layout) = two_clients();
        for placed in &mut layout.windows {
            if placed.window == GRADIENT {
                placed.floating = true;
                placed.rect = over;
            }
        }
        if !under {
            layout.windows.retain(|placed| placed.window == GRADIENT);
        }
        let buffers = client_buffers(&layout);
        let layers = [LayerFrame {
            rect: Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT)),
            above: false,
            surface: Some(
                Surface::new(
                    &behind,
                    WIDTH,
                    HEIGHT,
                    WIDTH * 4,
                    Pattern::Checkerboard.format(),
                )
                .unwrap(),
            ),
            dim_around: false,
            blur: false,
            xray: false,
        }];
        let mut ruled = BTreeMap::new();
        let _previous = ruled.insert(
            GRADIENT,
            crate::WindowStyle {
                xray: rule,
                ..crate::WindowStyle::default()
            },
        );
        let styles = Styles {
            base: style,
            windows: &ruled,
        };
        let at = layout
            .windows
            .iter()
            .position(|placed| placed.window == GRADIENT)
            .unwrap();
        let reads = reads_backdrop(&layout.windows, at, &styles);
        let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
        let mut backdrop = Backdrop::new(WIDTH, HEIGHT).unwrap();
        let _ = render_onto(
            &mut canvas,
            Some(&mut backdrop),
            &layout,
            (0, 0),
            &styles,
            &surfaces(&buffers),
            &layers,
            &full,
        );
        (reads, rows(canvas.data(), over))
    };

    let (reads, with) = draw(&base, Some(true), true);
    assert!(reads, "a floating window with `xray 1` reads the backdrop");
    assert!(
        with == draw(&base, Some(true), false).1,
        "an xray window is the same pixels whether or not a window is under it"
    );

    let xray = Style { xray: true, ..base };
    let (reads, with) = draw(&xray, None, true);
    assert!(reads, "`decoration:blur:xray` makes every window read it");
    assert!(
        with == draw(&xray, None, false).1,
        "with `decoration:blur:xray` the window under it is not blurred either"
    );

    // The control: the same two frames without the rule differ, so the
    // window under it is what an ordinary floating window blurs.
    let (reads, with) = draw(&base, None, true);
    assert!(!reads, "a floating window blurs what it floats over");
    assert!(
        with != draw(&base, None, false).1,
        "without xray the window under it shows through the blur"
    );

    // And `xray 0` beats the option, as it does in Hyprland.
    let (reads, with) = draw(&xray, Some(false), true);
    assert!(!reads, "`xray 0` never reads the backdrop");
    assert!(with != draw(&xray, Some(false), false).1);
}

/// `xray 0` on a *tiled* window: it blurs the frame as it stands, as a
/// floating one does, where the tiling alone would have read the backdrop.
#[test]
fn xray_off_takes_a_tiled_window_off_the_backdrop() {
    let style = plain_style();
    let (_, layout) = two_clients();
    let mut ruled = BTreeMap::new();
    let _previous = ruled.insert(
        GRADIENT,
        crate::WindowStyle {
            xray: Some(false),
            ..crate::WindowStyle::default()
        },
    );
    let styles = Styles {
        base: &style,
        windows: &ruled,
    };
    let at = |window: WindowId| {
        layout
            .windows
            .iter()
            .position(|placed| placed.window == window)
            .unwrap()
    };
    assert!(reads_backdrop(&layout.windows, at(CHECKERBOARD), &styles));
    assert!(!reads_backdrop(&layout.windows, at(GRADIENT), &styles));
    assert!(reads_backdrop(
        &layout.windows,
        at(GRADIENT),
        &Styles::plain(&style)
    ));
}

/// A frame is the same bytes on one thread as on seven.
///
/// The blur's passes, a surface blended onto the frame and one copied into
/// it are each cut into bands of rows and drawn a band a thread
/// (`crate::cores`). Every expected image in this file says the bands come
/// out right on whatever machine ran the tests; this says it of the two
/// numbers of threads a machine cannot be relied on to have, with every
/// effect on and a wallpaper behind, so that each of the three is large
/// enough to be cut up.
#[test]
fn a_frame_is_the_same_bytes_on_one_thread_as_on_seven() {
    let style = graded_style();
    let mut frames = [1, 7].map(|threads| {
        crate::cores::tests::force(threads);
        let frame = graded_frame(&style);
        crate::cores::tests::force(1);
        frame
    });
    let seven = frames.last_mut().map(std::mem::take).unwrap();
    assert!(
        frames[0] == seven,
        "seven threads drew another frame than one"
    );
}

/// A region less a hole is every pixel of it outside the hole and none
/// inside, which is what lets a shadow be drawn only where it shows.
#[test]
fn a_region_less_a_hole_is_what_is_left() {
    let region: Damage = [Rect::new(0, 0, 100, 60), Rect::new(200, 0, 10, 10)]
        .into_iter()
        .collect();
    let hole = Rect::new(20, 10, 60, 30);
    let left = region.without(hole);
    assert_eq!(left.area(), region.area() - 60 * 30);
    for (x, y, inside) in [
        (19, 10, true),
        (20, 10, false),
        (79, 39, false),
        (80, 39, true),
    ] {
        assert_eq!(left.contains(x, y), inside, "at {x},{y}");
    }
    assert!(
        left.contains(205, 5),
        "a rectangle the hole never touched went with it"
    );
    assert_eq!(region.without(Rect::new(500, 500, 5, 5)), region);
}

/// The two clients on a 1920x1080 screen, which is the picture a guest whose
/// `monitor =` line asked for that mode has to show.
///
/// `cargo xtask test-compositor`'s mode boot is what needs it: the card there
/// prefers 1024x768, the configuration asks for 1920x1080, and a compositor
/// that set the preferred mode anyway shows a picture of another size.
#[test]
fn two_pattern_clients_on_a_1920x1080_screen_match_the_expected_image() {
    let size = (1920, 1080);
    let frame = frame_on(size, &plain_style(), Settings::default());
    golden::check("dwindle-two-clients-1920x1080", size.0, size.1, &frame);
}

/// The two clients on a 1024x768 connector whose monitor is turned by
/// `transform`, as the connector's buffer holds them: tiled on the monitor
/// as it is read -- 768 wide and 1024 tall for a quarter turn -- and turned
/// into the buffer on the way out, as `hyprix` does.
fn turned_frame(transform: Transform) -> Vec<u8> {
    let (width, height) = transform.size((WIDTH, HEIGHT));
    let (_, layout) = two_clients_on((width, height), Settings::default());
    let buffers = client_buffers(&layout);
    let mut canvas = Canvas::new(width, height).unwrap();
    let full = Damage::full(width, height);
    let produced = render(
        &mut canvas,
        &layout,
        (0, 0),
        &plain_style(),
        &surfaces(&buffers),
        &full,
    );
    assert_eq!(produced, full);
    let mut bytes = vec![0; WIDTH as usize * HEIGHT as usize * 4];
    let mut target = Target::new(&mut bytes, WIDTH, HEIGHT, WIDTH * 4).unwrap();
    let written = canvas
        .present_transformed(&mut target, &full, transform)
        .unwrap();
    assert_eq!(
        written,
        Damage::full(WIDTH, HEIGHT),
        "a whole frame is the whole buffer"
    );
    bytes
}

/// `monitor = , preferred, auto, 1, transform, 1` and `transform, 3`: the
/// pictures `cargo xtask test-compositor --boot transform` requires from
/// QEMU's screendump, which reads the connector's buffer as it is.
///
/// The monitor is laid out tall, so dwindle stacks the two windows rather
/// than putting them side by side, and the buffer holds that picture turned
/// counter-clockwise for 1 -- the first window, the checkerboard, along the
/// buffer's left -- and clockwise for 3, along its right.
#[test]
fn two_pattern_clients_on_a_monitor_turned_once_match_the_expected_image() {
    let one = turned_frame(Transform::Rotated90);
    golden::check("dwindle-two-clients-transform-1", WIDTH, HEIGHT, &one);
}

/// The same for `transform, 3`: a test of its own, since blessing writes
/// one image a test.
#[test]
fn two_pattern_clients_on_a_monitor_turned_thrice_match_the_expected_image() {
    let three = turned_frame(Transform::Rotated270);
    golden::check("dwindle-two-clients-transform-3", WIDTH, HEIGHT, &three);
}

/// What the two turned pictures are to each other and to the upright ones,
/// which is what makes them the right pictures rather than merely the ones
/// that were blessed.
#[test]
fn the_turned_pictures_are_the_upright_one_turned() {
    let one = turned_frame(Transform::Rotated90);
    let three = turned_frame(Transform::Rotated270);

    // Each pixel of the upright tall frame is where transform 1 sends it:
    // the whole of the turn, on a real frame rather than six pixels.
    let (tall_width, tall_height) = (HEIGHT, WIDTH);
    let upright = frame_on(
        (tall_width, tall_height),
        &plain_style(),
        Settings::default(),
    );
    let value = |bytes: &[u8], at: usize| -> [u8; 4] {
        [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]
    };
    for y in 0..i64::from(tall_height) {
        for x in 0..i64::from(tall_width) {
            let (bx, by) = crate::transform::point(
                Transform::Rotated90,
                (i64::from(tall_width), i64::from(tall_height)),
                (x, y),
            );
            // Hyprland's `(y, W - 1 - x)`, spelled out once more.
            assert_eq!((bx, by), (y, i64::from(tall_width) - 1 - x));
            let from = ((y * i64::from(tall_width) + x) * 4) as usize;
            let to = ((by * i64::from(WIDTH) + bx) * 4) as usize;
            assert_eq!(value(&one, to), value(&upright, from), "({x}, {y})");
        }
    }

    // 3 is 1 upside down, and neither is the upright monitor's picture,
    // which is what a compositor that ignored the line would show.
    let pixels = WIDTH as usize * HEIGHT as usize;
    for at in 0..pixels {
        assert_eq!(
            value(&three, at * 4),
            value(&one, (pixels - 1 - at) * 4),
            "pixel {at}"
        );
    }
    let flat = two_client_frame();
    assert_ne!(one, flat, "transform 1 drew the upright picture");
    assert_ne!(three, flat, "transform 3 drew the upright picture");
    assert_ne!(one, three);
}

/// `layerrule = xray`: a bar's blur is of the wallpaper rather than of
/// whatever happens to be under it.
///
/// Hyprland's `m_blurFB` is everything behind the windows, blurred and
/// kept, and a layer surface takes its blur from the frame as it stands
/// unless this rule asks for that picture instead. So the test is what the
/// rule promises: with it on, the bar's pixels do not depend on what is
/// under the bar. Rendered twice with the same wallpaper and two different
/// sets of windows, the strip must come out byte for byte the same -- and
/// with the rule off it must not, which is the control that says the two
/// frames were not identical for some other reason.
#[test]
fn an_xray_bar_blurs_the_wallpaper_and_not_the_windows_under_it() {
    let style = Style {
        blur: Some(Blur::new(8, 2)),
        ..plain_style()
    };
    let full = Damage::full(WIDTH, HEIGHT);
    // Across the middle of the screen, where the windows certainly are.
    let strip = Rect::new(0, 300, i64::from(WIDTH), 120);
    let bar = Pattern::Gradient.draw(strip.width as u32, strip.height as u32);

    let behind = wallpaper(None);
    let draw = |windows: bool, xray: bool| {
        let (_, mut layout) = two_clients();
        let buffers = client_buffers(&layout);
        if !windows {
            layout.windows.clear();
        }
        let over = [
            LayerFrame {
                rect: Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT)),
                above: false,
                surface: Some(
                    Surface::new(
                        &behind,
                        WIDTH,
                        HEIGHT,
                        WIDTH * 4,
                        Pattern::Checkerboard.format(),
                    )
                    .unwrap(),
                ),
                dim_around: false,
                blur: false,
                xray: false,
            },
            LayerFrame {
                rect: strip,
                above: true,
                surface: Some(
                    Surface::new(
                        &bar,
                        strip.width as u32,
                        strip.height as u32,
                        strip.width as u32 * 4,
                        Pattern::Gradient.format(),
                    )
                    .unwrap(),
                ),
                dim_around: false,
                blur: true,
                xray,
            },
        ];
        let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
        let mut backdrop = Backdrop::new(WIDTH, HEIGHT).unwrap();
        let _ = render_onto(
            &mut canvas,
            Some(&mut backdrop),
            &layout,
            (0, 0),
            &Styles::plain(&style),
            &surfaces(&buffers),
            &over,
            &full,
        );
        rows(canvas.data(), strip)
    };

    assert_eq!(
        draw(true, true),
        draw(false, true),
        "an xray bar is the same pixels whether or not there are windows under it"
    );
    assert_ne!(
        draw(true, false),
        draw(false, false),
        "without the rule the windows under it are what it blurs"
    );
}

/// The bytes of `rect`'s rows out of a canvas `WIDTH` pixels across.
fn rows(data: &[u8], rect: Rect) -> Vec<u8> {
    let stride = WIDTH as usize * 4;
    (rect.y..rect.bottom())
        .flat_map(|y| {
            let at = y as usize * stride + rect.x as usize * 4;
            data[at..at + rect.width as usize * 4].to_vec()
        })
        .collect()
}

/// The blend written out ([`Canvas::composite_with`] on a premultiplied
/// surface) against tiny-skia's pattern shader, which drew every expected
/// image: every alpha, every colour a pixel of that alpha can have, over
/// every byte a canvas can hold, at full opacity and at two others.
#[test]
fn a_translucent_surface_blends_as_tiny_skia_blends_it() {
    use tiny_skia::{BlendMode, FilterQuality, Paint, Pattern, PixmapMut, PixmapRef, SpreadMode};

    const SIDE: u32 = 256;
    for opacity in [1.0_f32, 0.5, 0.8] {
        for alpha in 0..=255u32 {
            // Row `y` is colour `min(y, alpha)` in all three channels, over a
            // canvas whose column `x` is `x` in blue, `255 - x` in green, and
            // whose own alpha runs down the rows.
            let surface_bytes: Vec<u8> = (0..SIDE * SIDE)
                .flat_map(|at| {
                    let (x, y) = (at % SIDE, at / SIDE);
                    let colour = |value: u32| u8::try_from(value.min(alpha)).unwrap();
                    [
                        colour(y),
                        colour(x),
                        colour((x * 7 + y) & 0xFF),
                        u8::try_from(alpha).unwrap(),
                    ]
                })
                .collect();
            let under: Vec<u8> = (0..SIDE * SIDE)
                .flat_map(|at| {
                    let (x, y) = (at % SIDE, at / SIDE);
                    [
                        u8::try_from(x).unwrap(),
                        u8::try_from(255 - x).unwrap(),
                        u8::try_from((x + y) & 0xFF).unwrap(),
                        u8::try_from(y.max(alpha)).unwrap(),
                    ]
                })
                .collect();
            let whole = Rect::new(0, 0, i64::from(SIDE), i64::from(SIDE));

            let mut canvas = Canvas::new(SIDE, SIDE).unwrap();
            canvas.data_mut().copy_from_slice(&under);
            let surface =
                Surface::new(&surface_bytes, SIDE, SIDE, SIDE * 4, Format::Argb8888).unwrap();
            canvas.composite_with(
                &surface,
                whole,
                Rounding::none(),
                opacity,
                &Damage::from(whole),
            );

            let mut expected = under.clone();
            let mut pixmap = PixmapMut::from_bytes(&mut expected, SIDE, SIDE).unwrap();
            let shader = Pattern::new(
                PixmapRef::from_bytes(&surface_bytes, SIDE, SIDE).unwrap(),
                SpreadMode::Pad,
                FilterQuality::Nearest,
                opacity,
                tiny_skia::Transform::identity(),
            );
            let paint = Paint {
                shader,
                blend_mode: BlendMode::SourceOver,
                anti_alias: false,
                force_hq_pipeline: true,
                ..Paint::default()
            };
            pixmap.fill_rect(
                tiny_skia::Rect::from_xywh(0.0, 0.0, SIDE as f32, SIDE as f32).unwrap(),
                &paint,
                tiny_skia::Transform::identity(),
                None,
            );

            if let Some((at, (got, want))) = canvas
                .data()
                .iter()
                .zip(&expected)
                .enumerate()
                .find(|(_, (got, want))| got != want)
            {
                panic!(
                    "alpha {alpha} at opacity {opacity}: byte {at} is {got}, tiny-skia made {want}"
                );
            }
        }
    }
}

/// The two clients after the gradient, tiled on the right, was dragged with
/// `bindm = SUPER, mouse:272, movewindow` from its own middle and let go at
/// (300, 40), near the top of the left half of the screen: what
/// `cargo xtask test-compositor --boot drag` requires.
///
/// With `dwindle:precise_mouse_move` the drop takes the quarter of the box
/// it was let go in -- the top, since it is nearer the top edge than the
/// side one in the box's proportions -- and the two are stacked. Without it,
/// the half: the gradient goes on the left and the checkerboard on the
/// right, which is the control.
fn dropped_frame(precise: bool) -> Vec<u8> {
    let text = if precise {
        "dwindle:precise_mouse_move = true\n"
    } else {
        ""
    };
    let settings = Settings::from_config(&parse("t.conf", text, &mut NoSources).config);
    let (mut state, _) = two_clients_on((WIDTH, HEIGHT), settings);
    let _ = state.lift_window(GRADIENT, (768.0, 384.0)).unwrap();
    let changes = state.drop_window(GRADIENT, (300.0, 40.0)).unwrap();
    assert!(!changes.is_empty(), "the drop changed nothing");
    let layout = state.layout().remove(0);
    let buffers = client_buffers(&layout);
    let mut canvas = Canvas::new(WIDTH, HEIGHT).unwrap();
    let full = Damage::full(WIDTH, HEIGHT);
    let _ = render(
        &mut canvas,
        &layout,
        (0, 0),
        &plain_style(),
        &surfaces(&buffers),
        &full,
    );
    canvas.data().to_vec()
}

#[test]
fn a_window_dropped_beside_another_matches_the_expected_image() {
    golden::check("dropped-two-clients", WIDTH, HEIGHT, &dropped_frame(false));
}

#[test]
fn a_window_dropped_precisely_matches_the_expected_image() {
    let precise = dropped_frame(true);
    golden::check("dropped-precisely-two-clients", WIDTH, HEIGHT, &precise);
    assert_ne!(
        precise,
        dropped_frame(false),
        "precise_mouse_move changed nothing"
    );
}

/// What a screenshot of the two clients shows when the gradient's
/// `windowrule = no_screen_share` hides it: the frame, with a black box
/// over the gradient's own rectangle -- not its border -- as Hyprland's
/// `CScreenshareFrame::renderMonitor` draws one. The screen itself is
/// still `dwindle-two-clients`; this is what `/bin/shot` must be handed in
/// `cargo xtask test-compositor --boot screenshot-unshared`.
#[test]
fn an_unshared_window_is_a_black_box_in_the_expected_screenshot() {
    let mut frame = two_client_frame();
    let (_, layout) = two_clients();
    let hidden = layout
        .windows
        .iter()
        .find(|placed| placed.window == GRADIENT)
        .map(|placed| placed.rect)
        .unwrap();
    for y in hidden.y..hidden.bottom() {
        for x in hidden.x..hidden.right() {
            let at = ((y * i64::from(WIDTH) + x) * 4) as usize;
            frame[at..at + 4].copy_from_slice(&0xFF00_0000_u32.to_le_bytes());
        }
    }
    assert_ne!(frame, two_client_frame());
    golden::check("unshared-two-clients", WIDTH, HEIGHT, &frame);
}
