//! The runtime against the real compositor, in one process.
//!
//! `hyprix::run` serves a socket in this process, headless, writing each
//! frame it composes as a PPM; a client built on the toolkit connects to it
//! from a thread, does what a desktop client does, and the frame is looked
//! at. Nothing is mocked: the server is the one Ferrix boots.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "a test's fixtures should fail loudly, and the workspace's ban is about the compositor"
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compositor_toolkit::tiny_skia;
use compositor_toolkit::{
    Anchor, ChildOutput, Client, Command, Event, KeyboardInteractivity, Layer, LayerOptions,
    ToplevelOptions,
};
use hyprix::{Options, Renderer};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;

fn workspace(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("toolkit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a directory to work in");
    path
}

/// Run the compositor for `millis`, with `client` connected from a thread;
/// give back the client's answer and the last frame as RGB rows.
fn with_compositor<T: Send + 'static>(
    name: &str,
    millis: u64,
    client: impl FnOnce(&Path) -> T + Send + 'static,
) -> (T, Vec<u8>) {
    with_compositor_configured(name, millis, "", client)
}

/// [`with_compositor`], with `extra` added to the compositor's
/// configuration.
fn with_compositor_configured<T: Send + 'static>(
    name: &str,
    millis: u64,
    extra: &str,
    client: impl FnOnce(&Path) -> T + Send + 'static,
) -> (T, Vec<u8>) {
    let work = workspace(name);
    let socket = work.join("wayland");
    let frames = work.join("frames");
    let config = work.join("hyprland.conf");
    std::fs::write(
        &config,
        format!("decoration:blur:noise = 0\nanimations:enabled = false\n{extra}"),
    )
    .expect("a configuration");
    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        dump: Some(frames.clone()),
        deadline: Some(millis),
        config: Some(config),
        renderer: Renderer::Software,
        ..Options::default()
    };
    let path = socket.clone();
    let started = std::thread::spawn(move || {
        for _ in 0..400 {
            if path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        client(&path)
    });
    let ran = hyprix::run(&options);
    let answer = started.join().expect("the client finished");
    let _ = ran.expect("the compositor ran");
    let frame = last_frame(&frames);
    let _ = std::fs::remove_dir_all(&work);
    (answer, frame)
}

fn last_frame(directory: &Path) -> Vec<u8> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(directory)
        .expect("frames")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|kind| kind == "ppm"))
        .collect();
    names.sort();
    let bytes = std::fs::read(names.last().expect("a frame")).expect("a frame");
    let mut parts = bytes.splitn(4, |byte| *byte == b'\n');
    let _ = (parts.next(), parts.next(), parts.next());
    parts.next().expect("pixels").to_vec()
}

fn pixel(frame: &[u8], x: u32, y: u32) -> (u8, u8, u8) {
    let at = ((y * WIDTH + x) * 3) as usize;
    (frame[at], frame[at + 1], frame[at + 2])
}

/// Turn the loop until `wanted` says stop or `patience` runs out; every
/// event goes through `each`.
fn until(
    client: &mut Client,
    patience: Duration,
    mut each: impl FnMut(&mut Client, &Event) -> bool,
) -> Result<(), String> {
    let end = Instant::now() + patience;
    while Instant::now() < end {
        let events = client
            .dispatch(Some(Duration::from_millis(50)))
            .map_err(|error| error.to_string())?;
        for event in &events {
            if each(client, event) {
                return Ok(());
            }
        }
    }
    Err("ran out of patience".to_owned())
}

#[test]
fn a_bar_on_a_layer_surface_is_drawn_where_it_asked() {
    let (answer, frame) = with_compositor("bar", 3000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let outputs = client.outputs().len();
        let described = client
            .outputs()
            .first()
            .map(|output| (output.mode, output.name.clone()));
        let bar = client
            .layer_surface(&LayerOptions {
                layer: Layer::Top,
                namespace: "toolkit-test".to_owned(),
                size: (0, 40),
                anchor: Anchor::TOP.with(Anchor::LEFT).with(Anchor::RIGHT),
                exclusive_zone: 40,
                keyboard: KeyboardInteractivity::None,
                ..LayerOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut size = None;
        until(&mut client, Duration::from_secs(2), |client, event| {
            if let Event::Configure {
                surface,
                width,
                height,
            } = event
                && *surface == bar
            {
                size = Some((*width, *height));
                let _ = client.draw(bar, |pixmap| {
                    pixmap.fill(tiny_skia::Color::from_rgba8(200, 30, 40, 255));
                });
                client.request_frame(bar);
            }
            matches!(event, Event::Frame { surface, .. } if *surface == bar)
        })?;
        // Kept connected until the compositor ends, so its last frame is
        // one drawn with the bar still there.
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>((outputs, described, size))
    });
    let (outputs, described, size) = answer.expect("the client worked");
    assert_eq!(outputs, 1, "one headless screen");
    let (mode, _name) = described.expect("described");
    assert_eq!(mode, (WIDTH as i32, HEIGHT as i32));
    assert_eq!(size, Some((WIDTH, 40)), "stretched across by its anchors");
    assert_eq!(
        pixel(&frame, 10, 10),
        (200, 30, 40),
        "the bar is drawn at the top"
    );
    assert_eq!(pixel(&frame, WIDTH - 10, 39), (200, 30, 40));
    assert_ne!(pixel(&frame, 10, 45), (200, 30, 40), "and nowhere below it");
}

#[test]
fn a_window_is_tiled_by_the_compositor_and_drawn_there() {
    let (answer, frame) = with_compositor("toplevel", 3000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let window = client
            .toplevel(&ToplevelOptions {
                title: "toolkit test".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (200, 100),
                parent: None,
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut size = None;
        until(&mut client, Duration::from_secs(2), |client, event| {
            if let Event::Configure {
                surface,
                width,
                height,
            } = event
                && *surface == window
            {
                size = Some((*width, *height));
                client.set_title(window, "toolkit test, retitled");
                let _ = client.draw(window, |pixmap| {
                    pixmap.fill(tiny_skia::Color::from_rgba8(30, 160, 90, 255));
                });
                client.request_frame(window);
            }
            matches!(event, Event::Frame { surface, .. } if *surface == window)
        })?;
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>(size)
    });
    let (width, height) = answer.expect("the client worked").expect("configured");
    // A tiling compositor gives the one window the screen less its gaps,
    // not the 200x100 asked for.
    assert!(
        width > 200 && height > 100 && width <= WIDTH && height <= HEIGHT,
        "tiled to {width}x{height}"
    );
    assert_eq!(
        pixel(&frame, WIDTH / 2, HEIGHT / 2),
        (30, 160, 90),
        "the window is drawn where it was tiled"
    );
}

/// A window that cannot be made -- here a title with a NUL in it, which
/// the wire cannot carry -- is an error and leaves nothing behind: the
/// objects already sent are destroyed in the protocol's order, so the
/// compositor does not end the client, and the next window is tiled alone.
/// yserver retries a window that failed on every pass of its loop, and each
/// toplevel left over was an empty tile on the N150 box (2026-10-24).
#[test]
fn a_window_that_cannot_be_made_leaves_nothing_behind() {
    let (answer, _) = with_compositor("toplevel-failed", 3000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let mut failed = 0;
        for _ in 0..5 {
            if client
                .toplevel(&ToplevelOptions {
                    title: "bad\0title".to_owned(),
                    ..ToplevelOptions::default()
                })
                .is_err()
            {
                failed += 1;
            }
        }
        let window = client
            .toplevel(&ToplevelOptions {
                title: "kept".to_owned(),
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut size = None;
        until(&mut client, Duration::from_secs(2), |_, event| {
            if let Event::Configure {
                surface,
                width,
                height,
            } = event
                && *surface == window
            {
                size = Some((*width, *height));
            }
            size.is_some()
        })?;
        Ok::<_, String>((failed, size))
    });
    let (failed, size) = answer.expect("the client was not ended by the compositor");
    assert_eq!(failed, 5, "a title with a NUL in it should fail to be sent");
    let (width, height) = size.expect("configured");
    assert!(
        width > WIDTH / 2 && height > HEIGHT / 2,
        "the window was tiled beside what the failed ones left: {width}x{height}"
    );
}

/// Lock, draw a colour on the lock surface, and either unlock or leave
/// the lock held when the client goes; the last frame and what the client
/// saw come back.
fn locked(name: &str, unlock: bool) -> (Option<(u32, u32)>, Vec<u8>) {
    let (answer, frame) = with_compositor(name, 3000, move |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        client.lock().map_err(|error| error.to_string())?;
        let output = client
            .outputs()
            .first()
            .and_then(|output| output.id)
            .ok_or("no output")?;
        let surface = client
            .lock_surface(output)
            .map_err(|error| error.to_string())?;
        let mut locked = false;
        let mut configured = None;
        until(&mut client, Duration::from_secs(2), |client, event| {
            match event {
                Event::Configure {
                    surface: which,
                    width,
                    height,
                } if *which == surface => {
                    configured = Some((*width, *height));
                    let _ = client.draw(surface, |pixmap| {
                        pixmap.fill(tiny_skia::Color::from_rgba8(10, 120, 60, 255));
                    });
                }
                Event::Locked => locked = true,
                _ => {}
            }
            locked
        })?;
        let _ = until(&mut client, Duration::from_millis(400), |_, _| false);
        if unlock {
            client.unlock().map_err(|error| error.to_string())?;
        }
        // Kept connected until the compositor ends, so its last frame is
        // one drawn with this client still there.
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>(configured)
    });
    (answer.expect("the client worked"), frame)
}

#[test]
fn the_lock_covers_the_screen() {
    let (configured, frame) = locked("lock", false);
    assert_eq!(configured, Some((WIDTH, HEIGHT)));
    assert_eq!(
        pixel(&frame, 320, 240),
        (10, 120, 60),
        "the lock is what is drawn"
    );
}

#[test]
fn an_unlock_gives_the_screen_back() {
    let (_, frame) = locked("unlock", true);
    assert_ne!(pixel(&frame, 320, 240), (10, 120, 60), "the lock came off");
}

#[test]
fn timers_children_and_signals_come_back_as_events() {
    let (answer, _) = with_compositor("loop", 2500, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let started = Instant::now();
        let timer = client.add_timer(Duration::from_millis(100), None);
        let lines = client
            .run(&Command::new("echo one; echo two"))
            .map_err(|error| error.to_string())?;
        let mut whole = Command::new("printf 'a\\nb'; exit 3");
        whole.output = ChildOutput::Whole;
        let whole = client.run(&whole).map_err(|error| error.to_string())?;
        let signal = libc::SIGUSR1;
        client
            .watch_signals(&[signal])
            .map_err(|error| error.to_string())?;
        let waker = client.waker().map_err(|error| error.to_string())?;
        let mut seen = Vec::new();
        until(&mut client, Duration::from_secs(2), |_, event| {
            match event {
                Event::Timer(id) if *id == timer => {
                    // Not before it was due; how much after is the machine's.
                    let late = started.elapsed() >= Duration::from_millis(100);
                    seen.push(format!("timer, not early: {late}"));
                    #[expect(
                        unsafe_code,
                        reason = "AUDIT: raise a signal this thread has blocked for its signalfd"
                    )]
                    // SAFETY: raise is async-signal-safe and takes an integer.
                    let _ = unsafe { libc::raise(signal) };
                    waker.wake();
                }
                Event::ChildLine { child, line } if *child == lines => {
                    seen.push(format!("line {line}"));
                }
                Event::ChildExited {
                    child,
                    status,
                    output,
                } if *child == whole => {
                    seen.push(format!("whole {status:?} {output:?}"));
                }
                Event::ChildExited { child, status, .. } if *child == lines => {
                    seen.push(format!("lines done {status:?}"));
                }
                Event::Signal(number) if *number == signal => seen.push("signal".to_owned()),
                Event::Woken => seen.push("woken".to_owned()),
                _ => {}
            }
            seen.len() >= 7
        })?;
        Ok::<_, String>(seen)
    });
    let mut seen = answer.expect("the client worked");
    seen.sort();
    assert_eq!(
        seen,
        [
            "line one",
            "line two",
            "lines done Some(0)",
            "signal",
            "timer, not early: true",
            "whole Some(3) \"a\\nb\"",
            "woken",
        ]
    );
}

#[test]
fn a_tooltip_hangs_under_the_bar_it_belongs_to() {
    let (answer, frame) = with_compositor("popup", 3000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let bar = client
            .layer_surface(&LayerOptions {
                namespace: "waybar".to_owned(),
                size: (0, 40),
                anchor: Anchor::TOP.with(Anchor::LEFT).with(Anchor::RIGHT),
                exclusive_zone: 40,
                ..LayerOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut tooltip = None;
        let mut placed = None;
        let _ = until(&mut client, Duration::from_secs(2), |client, event| {
            if let Event::Configure {
                surface,
                width,
                height,
            } = event
            {
                if *surface == bar {
                    fill(client, bar, (0, 0, 200));
                    tooltip = tooltip.or_else(|| open_tooltip(client, bar));
                } else if Some(*surface) == tooltip {
                    placed = Some((*width, *height));
                    fill(client, *surface, (0, 200, 0));
                }
            }
            false
        });
        // Kept connected until the compositor ends, so its last frame is
        // one drawn with the bar and its tooltip still there: a client that
        // has gone takes its surfaces with it.
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>(placed)
    });
    let placed = answer.expect("the client worked");
    assert_eq!(
        placed,
        Some((100, 30)),
        "the popup was configured at its size"
    );
    // Centred under the anchor rectangle (anchor bottom, gravity bottom):
    // x 75..175, y 40..70.
    assert_eq!(
        pixel(&frame, 125, 55),
        (0, 200, 0),
        "the tooltip is under the bar"
    );
    assert_eq!(
        pixel(&frame, 300, 20),
        (0, 0, 200),
        "the bar is still there"
    );
}

fn fill(client: &mut Client, surface: compositor_toolkit::SurfaceId, (r, g, b): (u8, u8, u8)) {
    let _ = client.draw(surface, |pixmap| {
        pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, 255));
    });
}

fn open_tooltip(
    client: &mut Client,
    bar: compositor_toolkit::SurfaceId,
) -> Option<compositor_toolkit::SurfaceId> {
    use compositor_toolkit::{PopupOptions, Rect};
    client
        .popup(
            bar,
            &PopupOptions {
                size: (100, 30),
                anchor_rect: Rect {
                    x: 100,
                    y: 0,
                    width: 50,
                    height: 40,
                },
                ..PopupOptions::default()
            },
        )
        .ok()
}

#[test]
fn a_cursor_picture_is_taken_replaced_and_given_up_without_a_protocol_error() {
    let (answer, _) = with_compositor("cursor-image", 2000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let _window = client
            .toplevel(&ToplevelOptions {
                title: "cursor".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (100, 100),
                parent: None,
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let arrow = [0xff_u8; 2 * 3 * 4];
        assert!(
            client.set_cursor_image(2, 2, (0, 0), &arrow).is_err(),
            "six pixels are not two by two"
        );
        client
            .set_cursor_image(2, 3, (1, 1), &arrow)
            .map_err(|error| error.to_string())?;
        client
            .set_cursor_image(3, 2, (0, 1), &arrow)
            .map_err(|error| error.to_string())?;
        let _ = client.roundtrip().map_err(|error| error.to_string())?;
        client.set_cursor(compositor_toolkit::CursorShape::default());
        client.roundtrip().map_err(|error| error.to_string())
    });
    let _ = answer.expect("the compositor took every request");
}

/// Where the frame's first pixel of `colour` is, reading rows top down.
fn first(frame: &[u8], colour: (u8, u8, u8)) -> Option<(u32, u32)> {
    (0..HEIGHT)
        .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
        .find(|&(x, y)| pixel(frame, x, y) == colour)
}

/// A window with a menu: the popup hangs from a point of the window, as an
/// X server's override-redirect menu is placed (docs/YSERVER.md §4.2). The
/// window draws at a size of its own rather than the tile's, which hyprix
/// stretches to the tile.
#[test]
fn a_menu_hangs_from_the_point_of_the_window_it_was_opened_at() {
    let (answer, frame) = with_compositor("menu", 3000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let window = client
            .toplevel(&ToplevelOptions {
                title: "toolkit menu test".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (200, 100),
                parent: None,
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut menu = None;
        let mut placed = None;
        let _ = until(&mut client, Duration::from_secs(2), |client, event| {
            if let Event::Configure {
                surface,
                width,
                height,
            } = event
            {
                if *surface == window {
                    draw_own_size(client, window);
                    menu = menu.or_else(|| open_menu(client, window));
                } else if Some(*surface) == menu {
                    placed = Some((*width, *height));
                    fill(client, *surface, (0, 200, 0));
                }
            }
            false
        });
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>(placed)
    });
    let placed = answer.expect("the client worked");
    assert_eq!(
        placed,
        Some((60, 40)),
        "the menu was configured at its size"
    );
    let window = first(&frame, (0, 0, 200)).expect("the window is drawn");
    let menu = first(&frame, (0, 200, 0)).expect("the menu is drawn");
    assert_eq!(
        (menu.0 - window.0, menu.1 - window.1),
        (20, 30),
        "the menu hangs from the point it was opened at"
    );
}

/// Blue, at the window's own 200x100 whatever the tile is.
fn draw_own_size(client: &mut Client, window: compositor_toolkit::SurfaceId) {
    let _ = client.draw_sized(window, (200, 100), |pixmap| {
        pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 200, 255));
    });
}

/// A 60x40 menu hanging from (20, 30) of `window`: the top left of that
/// point, growing right and down, and never moved.
fn open_menu(
    client: &mut Client,
    window: compositor_toolkit::SurfaceId,
) -> Option<compositor_toolkit::SurfaceId> {
    use compositor_toolkit::{PopupOptions, Rect};
    client
        .popup(
            window,
            &PopupOptions {
                size: (60, 40),
                anchor_rect: Rect {
                    x: 20,
                    y: 30,
                    width: 1,
                    height: 1,
                },
                anchor: 5,
                gravity: 8,
                constraint_adjustment: 0,
                ..PopupOptions::default()
            },
        )
        .ok()
}

/// A dialog, a window with a parent, floats at the size it draws, centred
/// over its parent, as in Hyprland; an X server's transient windows are
/// such dialogs.
#[test]
fn a_dialog_floats_at_its_own_size_over_its_parent() {
    let (answer, frame) = with_compositor("dialog", 3000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let window = client
            .toplevel(&ToplevelOptions {
                title: "toolkit parent".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (200, 100),
                parent: None,
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let dialog = client
            .toplevel(&ToplevelOptions {
                title: "toolkit dialog".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (120, 80),
                parent: Some(window),
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut configured = None;
        let _ = until(&mut client, Duration::from_secs(2), |client, event| {
            if let Event::Configure {
                surface,
                width,
                height,
            } = event
            {
                if *surface == window {
                    fill(client, window, (0, 0, 200));
                } else if *surface == dialog {
                    configured = Some((*width, *height));
                    fill(client, dialog, (0, 200, 0));
                }
            }
            false
        });
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>(configured)
    });
    let configured = answer.expect("the client worked");
    assert_eq!(configured, Some((120, 80)), "the dialog chose its own size");
    let (left, top) = first(&frame, (0, 200, 0)).expect("the dialog is drawn");
    let wide = (left..WIDTH)
        .take_while(|&x| pixel(&frame, x, top + 40) == (0, 200, 0))
        .count();
    assert_eq!(wide, 120, "the dialog floats at the width it drew");
    // The parent's top row, which the dialog, centred, does not reach.
    let (parent_left, parent_top) = first(&frame, (0, 0, 200)).expect("the parent is drawn");
    let parent_wide = (parent_left..WIDTH)
        .take_while(|&x| pixel(&frame, x, parent_top) == (0, 0, 200))
        .count();
    let centre = left + 60;
    let parent_centre = parent_left + u32::try_from(parent_wide / 2).expect("a width");
    assert!(
        centre.abs_diff(parent_centre) <= 2,
        "the dialog is centred over its parent: {centre} and {parent_centre}"
    );
}

/// A window of a fixed size -- its least size its greatest -- floats at
/// that size, as in Hyprland; an X window's `WM_NORMAL_HINTS` become such
/// limits. The same window without them is tiled.
#[test]
fn a_window_of_a_fixed_size_floats_at_it() {
    let floated = |name: &str, limits: Option<(u32, u32)>| {
        let (answer, _) = with_compositor(name, 1500, move |socket| {
            let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
            let (min_size, max_size) = limits.map_or(((0, 0), (0, 0)), |size| (size, size));
            let window = client
                .toplevel(&ToplevelOptions {
                    title: "toolkit fixed".to_owned(),
                    app_id: "toolkit-test".to_owned(),
                    size: (150, 90),
                    min_size,
                    max_size,
                    ..ToplevelOptions::default()
                })
                .map_err(|error| error.to_string())?;
            let mut configured = None;
            let _ = until(&mut client, Duration::from_secs(1), |client, event| {
                if let Event::Configure {
                    surface,
                    width,
                    height,
                } = event
                    && *surface == window
                {
                    configured = Some((*width, *height));
                    fill(client, window, (0, 200, 0));
                }
                false
            });
            Ok::<_, String>(configured)
        });
        answer.expect("the client worked")
    };
    // Floated, it is first told to choose (0x0), then given what it drew;
    // tiled, it would be given the tile.
    assert_eq!(
        floated("fixed", Some((150, 90))),
        Some((150, 90)),
        "a fixed-size window floats at its own size"
    );
    let tiled = floated("free", None).expect("configured");
    assert_ne!(tiled, (0, 0), "a window with no limits is tiled");
    assert!(tiled.0 > 150, "and given the tile: {tiled:?}");
}

/// A floating window that goes on to draw at a size of its own floats at
/// that size, where it was: an X program resizing its dialog after mapping
/// it, which yserver shows as buffers of the new size (`draw_sized`), as
/// Steam's "not responding" dialog does. hyprix used to keep the size of
/// the first buffer and squeeze every later one into it, which drew that
/// dialog at 130x70.
#[test]
fn a_floating_dialog_takes_a_size_its_program_gives_it_later() {
    let (answer, frame) = with_compositor("dialog-grows", 3000, |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let window = client
            .toplevel(&ToplevelOptions {
                title: "toolkit parent".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (200, 100),
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let dialog = client
            .toplevel(&ToplevelOptions {
                title: "toolkit dialog".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (60, 40),
                parent: Some(window),
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        // The first configure of the dialog is the one it chooses its own
        // size at; any after it, the program has resized its window.
        let mut configures = 0;
        let _ = until(&mut client, Duration::from_secs(2), |client, event| {
            let Event::Configure { surface, .. } = event else {
                return false;
            };
            if *surface == window {
                fill(client, window, (0, 0, 200));
            } else if *surface == dialog {
                configures += 1;
                let size = [(60, 40), (240, 160)][usize::from(configures > 1)];
                fill_sized(client, dialog, size, (0, 200, 0));
            }
            false
        });
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>(configures)
    });
    let configures = answer.expect("the client worked");
    assert!(
        configures >= 2,
        "the dialog was configured again: {configures}"
    );
    let (left, top) = first(&frame, (0, 200, 0)).expect("the dialog is drawn");
    let wide = (left..WIDTH)
        .take_while(|&x| pixel(&frame, x, top + 20) == (0, 200, 0))
        .count();
    let tall = (top..HEIGHT)
        .take_while(|&y| pixel(&frame, left + 20, y) == (0, 200, 0))
        .count();
    assert_eq!(
        (wide, tall),
        (240, 160),
        "the dialog floats at the size it drew last, not squeezed into its first"
    );
}

/// `colour` at a size of the program's own, as yserver draws an X window.
fn fill_sized(
    client: &mut Client,
    surface: compositor_toolkit::SurfaceId,
    size: (u32, u32),
    (r, g, b): (u8, u8, u8),
) {
    let _ = client.draw_sized(surface, size, |pixmap| {
        pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, 255));
    });
}

/// A program whose own loop polls the socket (`Client::as_raw_fd`) and then
/// dispatches with a zero timeout must get what arrived, even with events
/// still queued from before: `connect`'s round trips leave each screen's
/// arrival queued. An edge-triggered poll does not wake again for bytes
/// left unread, so a dispatch that handed back only the queued events
/// stalled the program until the compositor happened to send more --
/// yserver under sway, which sends a window's configure and a ping once and
/// then waits for the answer.
#[test]
fn a_zero_timeout_dispatch_reads_what_arrived_behind_queued_events() {
    let (answer, _) =
        with_compositor("queued", 2000, |socket| {
            let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
            let window = client
                .toplevel(&ToplevelOptions {
                    title: "queued".to_owned(),
                    app_id: "toolkit-test".to_owned(),
                    size: (100, 100),
                    parent: None,
                    ..ToplevelOptions::default()
                })
                .map_err(|error| error.to_string())?;
            client.flush().map_err(|error| error.to_string())?;
            // The configure is on the socket by now; one dispatch, as a
            // program does when its poll says the socket is readable.
            std::thread::sleep(Duration::from_millis(500));
            let events = client
                .dispatch(Some(Duration::ZERO))
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(events.iter().any(
                |event| matches!(event, Event::Configure { surface, .. } if *surface == window),
            ))
        });
    assert!(
        answer.expect("the client worked"),
        "the configure waiting on the socket was not read"
    );
}

/// A clipboard manager on the toolkit: `ext_data_control` bound and driven
/// by the program, as yserver's clipboard bridge does. What it copies comes
/// back to it as an offer whose types were read with it, which needs the
/// offer adopted as the `data_offer` event that makes it is read
/// ([`Client::adopt_new_ids`]): the offer's own `offer` events follow in the
/// same read, and an offer adopted after the fact never hears its types.
/// Then it pastes its own copy through a pipe it answers itself.
#[test]
fn a_data_control_offer_is_read_with_its_types_and_pasted() {
    use compositor_toolkit::Value;
    use compositor_toolkit::protocol::ext_data_control::{
        self as control, ext_data_control_device_v1 as device,
        ext_data_control_manager_v1 as manager, ext_data_control_offer_v1 as offer,
        ext_data_control_source_v1 as source,
    };
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    const TEXT: &str = "text/plain;charset=utf-8";
    let (answer, _) = with_compositor("control", 3000, |socket| {
        let fail = |error: compositor_toolkit::Error| error.to_string();
        let mut client = Client::connect_to(socket).map_err(fail)?;
        let seat = client.seat().ok_or("no seat")?;
        let (bound, _) = client
            .bind(&control::EXT_DATA_CONTROL_MANAGER_V1, 1, None)
            .map_err(fail)?;
        let made = client.new_object(&control::EXT_DATA_CONTROL_DEVICE_V1, 1);
        client
            .request(
                bound,
                manager::request::GET_DATA_DEVICE,
                &[Value::NewId(made), Value::Object(seat)],
            )
            .map_err(fail)?;
        client.adopt_new_ids(
            made,
            device::event::DATA_OFFER,
            &control::EXT_DATA_CONTROL_OFFER_V1,
            1,
        );
        let copied = client.new_object(&control::EXT_DATA_CONTROL_SOURCE_V1, 1);
        client
            .request(
                bound,
                manager::request::CREATE_DATA_SOURCE,
                &[Value::NewId(copied)],
            )
            .map_err(fail)?;
        client
            .request(
                copied,
                source::request::OFFER,
                &[Value::Str(Some(TEXT.to_owned()))],
            )
            .map_err(fail)?;
        client
            .request(
                made,
                device::request::SET_SELECTION,
                &[Value::Object(copied)],
            )
            .map_err(fail)?;
        client.flush().map_err(fail)?;

        // The offer the selection names, and the types it said it has.
        let mut types: Vec<(compositor_toolkit::ObjectId, String)> = Vec::new();
        let mut held = None;
        until(&mut client, Duration::from_secs(2), |_, event| {
            if let Event::Object {
                object,
                opcode,
                args,
                ..
            } = event
            {
                if *opcode == offer::event::OFFER
                    && let Some(mime) = args.first().and_then(Value::as_str)
                {
                    types.push((*object, mime.to_owned()));
                }
                if *object == made
                    && *opcode == device::event::SELECTION
                    && let Some(named) = args.first().and_then(Value::as_object)
                    && !named.is_null()
                {
                    held = Some(named);
                    return true;
                }
            }
            false
        })?;
        let held = held.ok_or("no selection")?;
        let offered: Vec<&str> = types
            .iter()
            .filter(|(object, _)| *object == held)
            .map(|(_, mime)| mime.as_str())
            .collect();
        if offered != [TEXT] {
            return Err(format!("the offer said {offered:?}"));
        }

        let (mut reading, writing) =
            std::os::unix::net::UnixStream::pair().map_err(|error| error.to_string())?;
        client
            .request(
                held,
                offer::request::RECEIVE,
                &[
                    Value::Str(Some(TEXT.to_owned())),
                    Value::Fd(writing.as_raw_fd()),
                ],
            )
            .map_err(fail)?;
        client.flush().map_err(fail)?;
        drop(writing);
        until(&mut client, Duration::from_secs(2), |_, event| {
            let Event::Object {
                object,
                opcode,
                args,
                ..
            } = event
            else {
                return false;
            };
            if *object != copied || *opcode != source::event::SEND {
                return false;
            }
            if let Some(Value::Fd(fd)) = args.get(1) {
                #[expect(
                    unsafe_code,
                    reason = "AUDIT: the descriptor arrived with the event and is the test's to own"
                )]
                // SAFETY: a descriptor this process received and nothing else holds.
                let mut pipe = unsafe { std::fs::File::from_raw_fd(*fd) };
                let _ = pipe.write_all(b"copied by the toolkit");
            }
            true
        })?;
        let mut text = String::new();
        let _ = reading
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(text)
    });
    assert_eq!(
        answer.expect("the clipboard worked"),
        "copied by the toolkit"
    );
}

/// A square on a test frame: its left, top, side and colour.
type Square = (u32, u32, u32, (u8, u8, u8));

/// A `w` × `h` frame in tiny-skia's order, `colour` all over and the
/// squares `squares` (`x`, `y`, side, colour) on it.
fn frame_with(w: u32, h: u32, colour: (u8, u8, u8), squares: &[Square]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = squares
                .iter()
                .find(|(sx, sy, side, _)| x >= *sx && x < sx + side && y >= *sy && y < sy + side)
                .map_or(colour, |square| square.3);
            rgba.extend([r, g, b, 255]);
        }
    }
    rgba
}

#[test]
fn a_damaged_draw_copies_what_changed_and_what_the_buffer_missed() {
    let green = (30, 160, 90);
    let red = (200, 40, 40);
    let blue = (40, 40, 200);
    let (answer, frame) = with_compositor("damaged", 4000, move |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let window = client
            .toplevel(&ToplevelOptions {
                title: "damaged".to_owned(),
                app_id: "toolkit-damaged".to_owned(),
                size: (200, 100),
                parent: None,
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut size = None;
        until(&mut client, Duration::from_secs(2), |_, event| {
            if let Event::Configure {
                surface,
                width,
                height,
            } = event
                && *surface == window
            {
                size = Some((*width, *height));
                return true;
            }
            false
        })?;
        let (w, h) = size.ok_or("not configured")?;
        // Three frames, each drawn when the one before was shown: all
        // green; a red square, damaged alone; a blue square, damaged
        // alone, into the buffer that held only the green -- which must
        // get the red square too.
        let frames = [
            (frame_with(w, h, green, &[]), vec![]),
            (
                frame_with(w, h, green, &[(0, 0, 20, red)]),
                vec![(0, 0, 20, 20)],
            ),
            (
                frame_with(w, h, green, &[(0, 0, 20, red), (40, 0, 20, blue)]),
                vec![(40, 0, 20, 20)],
            ),
        ];
        for (rgba, damage) in &frames {
            let _ = client
                .draw_damaged(window, rgba, (w, h), 1, damage)
                .map_err(|error| error.to_string())?;
            client.request_frame(window);
            until(
                &mut client,
                Duration::from_secs(2),
                |_, event| matches!(event, Event::Frame { surface, .. } if *surface == window),
            )?;
        }
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>(())
    });
    answer.expect("the client worked");
    let red_at = first(&frame, red).expect("the red square is still there");
    let blue_at = first(&frame, blue).expect("the blue square was drawn");
    assert_eq!(
        (blue_at.0 - red_at.0, blue_at.1),
        (40, red_at.1),
        "each where it was drawn"
    );
    assert_eq!(
        pixel(&frame, red_at.0 + 25, red_at.1 + 5),
        green,
        "between them, the first frame's green"
    );
}

/// A floating desktop that draws title bars, as Yoru's: no shadow, so what
/// is above a window is the bar or the background and nothing between.
const TITLE_BARS: &str = "general:layout = floating\nhyprix:titlebars = 1\n\
                          decoration:shadow:enabled = false\ngeneral:border_size = 2\n";

/// Open one window that says `own_frame` through `xdg-decoration`, under
/// `config`; give back what the toolkit reported of who draws its frame
/// (every [`Event::Decorated`], in order, and [`Client::own_frame`] at the
/// end) and the last frame.
fn decorated(
    name: &str,
    config: &str,
    own_frame: Option<bool>,
) -> ((Vec<bool>, Option<bool>), Vec<u8>) {
    let config = config.to_owned();
    let (answer, frame) = with_compositor_configured(name, 3000, &config, move |socket| {
        let mut client = Client::connect_to(socket).map_err(|error| error.to_string())?;
        let window = client
            .toplevel(&ToplevelOptions {
                title: "frame test".to_owned(),
                app_id: "toolkit-frame".to_owned(),
                size: (300, 200),
                own_frame,
                ..ToplevelOptions::default()
            })
            .map_err(|error| error.to_string())?;
        let mut said = Vec::new();
        let _ = until(&mut client, Duration::from_secs(2), |client, event| {
            match event {
                Event::Decorated { surface, own } if *surface == window => said.push(*own),
                Event::Configure { surface, .. } if *surface == window => {
                    let _ = client.draw(window, |pixmap| {
                        pixmap.fill(tiny_skia::Color::from_rgba8(30, 160, 90, 255));
                    });
                    client.request_frame(window);
                }
                _ => {}
            }
            false
        });
        let own = client.own_frame(window);
        // Kept open until the compositor stops, so its last frame has it.
        let _ = until(&mut client, Duration::from_secs(5), |_, _| false);
        Ok::<_, String>((said, own))
    });
    (answer.expect("the client worked"), frame)
}

/// The colour of the row above a window's top edge, `above` pixels up,
/// in the middle of the window's width; the window found by its green.
fn above_window(frame: &[u8], above: u32) -> (u8, u8, u8) {
    let (x, y) = first(frame, (30, 160, 90)).expect("the window is drawn");
    pixel(frame, x + 150, y.saturating_sub(above))
}

/// `xdg-decoration` as a program sees it through the toolkit: where the
/// compositor draws title bars, a window asking to draw its own frame is
/// told `client_side` and gets no bar, one asking for the compositor's is
/// told `server_side` and gets one -- the lode viewer's own buttons and
/// Settings' bar from hyprix. The answer is reported once, with the
/// configure that commits it.
#[test]
fn a_window_asking_for_its_own_frame_is_told_yes_and_drawn_with_no_bar() {
    let ((client_said, client_own), client_frame) = decorated("csd", TITLE_BARS, Some(true));
    let ((server_said, server_own), server_frame) = decorated("ssd", TITLE_BARS, Some(false));
    assert_eq!((client_said, client_own), (vec![true], Some(true)));
    assert_eq!((server_said, server_own), (vec![false], Some(false)));
    // Half a bar above the window: the bar's colour where the compositor
    // draws it, the background where the window draws its own.
    let background = pixel(&client_frame, 2, HEIGHT - 2);
    assert_eq!(
        above_window(&client_frame, 17),
        background,
        "no compositor title bar over a window that draws its own"
    );
    assert_ne!(
        above_window(&server_frame, 17),
        background,
        "the compositor's title bar over a window that asked for it"
    );
}

/// A window that said nothing through `xdg-decoration` hears nothing and
/// gets the compositor's bar, as before.
#[test]
fn a_window_that_says_nothing_hears_nothing_and_gets_the_bar() {
    let ((said, own), frame) = decorated("unsaid", TITLE_BARS, None);
    assert_eq!((said, own), (Vec::new(), None));
    assert_ne!(above_window(&frame, 17), pixel(&frame, 2, HEIGHT - 2));
}

/// A tiling desktop draws no title bars and tells every window
/// `server_side`, whatever it asked: a tile has no room for the program's
/// own bar.
#[test]
fn a_tiling_desktop_tells_a_window_the_compositor_draws_its_frame() {
    let ((said, own), _) = decorated("tiled", "", Some(true));
    assert_eq!((said, own), (vec![false], Some(false)));
}

/// A window rule decides over what the window asked: `title_bar on` gives
/// a window that asked to draw its own the compositor's bar and tells it
/// `server_side`, so it hides its own buttons; `title_bar off` takes the
/// bar from one that asked for it and tells it `client_side`.
#[test]
fn a_title_bar_rule_decides_over_what_the_window_asked() {
    let forced_on =
        format!("{TITLE_BARS}windowrule = title_bar on, match:class ^(toolkit-frame)$\n");
    let ((said, own), frame) = decorated("rule-on", &forced_on, Some(true));
    assert_eq!((said, own), (vec![false], Some(false)));
    assert_ne!(above_window(&frame, 17), pixel(&frame, 2, HEIGHT - 2));

    let forced_off =
        format!("{TITLE_BARS}windowrule = title_bar off, match:class ^(toolkit-frame)$\n");
    let ((said, own), frame) = decorated("rule-off", &forced_off, Some(false));
    assert_eq!((said, own), (vec![true], Some(true)));
    assert_eq!(above_window(&frame, 17), pixel(&frame, 2, HEIGHT - 2));
}
