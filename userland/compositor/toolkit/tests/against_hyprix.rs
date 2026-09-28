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
    let work = workspace(name);
    let socket = work.join("wayland");
    let frames = work.join("frames");
    let config = work.join("hyprland.conf");
    std::fs::write(
        &config,
        "decoration:blur:noise = 0\nanimations:enabled = false\n",
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
            })
            .map_err(|error| error.to_string())?;
        let dialog = client
            .toplevel(&ToplevelOptions {
                title: "toolkit dialog".to_owned(),
                app_id: "toolkit-test".to_owned(),
                size: (120, 80),
                parent: Some(window),
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
