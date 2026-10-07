//! The compositor, two clients and a screen, in one process.
//!
//! This is the compositor's everyday gate, and the headless half of stage
//! 18's exit in `docs/ROADMAP.md`: two pattern clients connect over a real
//! socket, are tiled by the dwindle layout, draw into shared memory, and the
//! frame the compositor composed is compared pixel for pixel against the
//! image `src/user/system/linux/compositor/render`'s own tests bless.
//!
//! That the two agree is the point. `src/user/system/linux/compositor/render`'s expected image is
//! built by calling the renderer directly with rectangles from
//! `src/user/system/linux/compositor/layout`; this one is built by two programs talking Wayland
//! over a socket to a server that works out the same rectangles from the
//! requests they sent. Nothing but the pixels is shared between the two
//! paths, so a difference is a real one.
//!
//! Everything runs in threads of one process rather than as spawned
//! binaries, so there is no target directory to find and no orphan to leave
//! behind if an assertion fails.

// An integration test's helpers are not inside a `#[test]` function, so the
// workspace's ban on `expect` and `panic` -- which is about a compositor that
// must not take every client's windows down with it -- reaches them. Here a
// fixture that cannot be built should stop the test loudly.
#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test's fixtures should fail loudly, and the workspace's ban is about the compositor"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use compositor_pattern::Shape;
use compositor_render::Pattern;
use hyprix::{Options, Renderer};

/// The screen, which is the size `src/user/system/linux/compositor/render`'s expected image is.
const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;

/// Where `src/user/system/linux/compositor/render` keeps the image both paths must produce.
const EXPECTED: &str = "../render/tests/data/dwindle-two-clients.xrle";

/// The same, with a bar across the top.
const BAR_EXPECTED: &str = "../render/tests/data/layer-bar-two-clients.xrle";

/// A directory of this test's own, named for the process so two runs at once
/// do not share one, and removed when the test is done with it.
///
/// The removal is in `Drop` so that it happens however the test ends: a
/// passing test, a failed assertion and a panicking fixture all unwind
/// through it. Removing it by hand at the end of each test left it behind
/// whenever an assertion fired first, and most tests never removed it at
/// all -- 882 of these directories, 21 GB of frames, once filled `/tmp`'s
/// quota and failed the next run with `EDQUOT`.
struct Workspace(PathBuf);

impl std::ops::Deref for Workspace {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn workspace(name: &str) -> Workspace {
    let path = std::env::temp_dir().join(format!("hyprix-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a directory to work in");
    Workspace(path)
}

/// A configuration with the blur's dither turned off, written into `work`.
///
/// `decoration:blur:noise` is 0.0117 in Hyprland and 0.0117 here, and the
/// dither is drawn. But `src/user/system/linux/compositor/render`'s expected images are
/// run-length encoded and a dither is the one thing a run of pixels cannot
/// survive, so they are blessed without it -- and this test compares
/// against those images, so the compositor under test is told the same
/// thing. `src/user/system/linux/compositor/render`'s `graded-blur-two-clients` is the picture
/// that holds the dither.
fn undithered(work: &Path) -> PathBuf {
    let path = work.join("no-dither.conf");
    std::fs::write(&path, "decoration:blur:noise = 0\n").expect("a configuration");
    path
}

/// Run the compositor with the clients in `patterns`, and give back the last
/// frame it drew as `XRGB8888` rows.
fn run(name: &str, patterns: &[(Pattern, &str)]) -> (Vec<u8>, String) {
    let shaped: Vec<(Pattern, &str, Shape)> = patterns
        .iter()
        .map(|(pattern, title)| (*pattern, *title, Shape::Window))
        .collect();
    run_shaped(name, &shaped)
}

/// The same, with each client given the role it asks for: a window, or a bar
/// through `zwlr_layer_shell_v1`.
fn run_shaped(name: &str, patterns: &[(Pattern, &str, Shape)]) -> (Vec<u8>, String) {
    run_drawn_by(name, patterns, Renderer::Software).expect("the compositor ran")
}

/// The same, with the frame drawn by `renderer`. `Err` is the compositor
/// saying it could not run at all, which for a renderer that needs something
/// of the host is how a test finds out the host has not got it.
fn run_drawn_by(
    name: &str,
    patterns: &[(Pattern, &str, Shape)],
    renderer: Renderer,
) -> Result<(Vec<u8>, String), String> {
    run_configured(name, patterns, renderer, "")
}

/// The same, with `lines` added to the configuration after the dither is
/// turned off.
fn run_configured(
    name: &str,
    patterns: &[(Pattern, &str, Shape)],
    renderer: Renderer,
    lines: &str,
) -> Result<(Vec<u8>, String), String> {
    run_refusing(name, patterns, renderer, lines, 0)
}

/// The same, on a screen that refuses its first `refused` frames as a card
/// whose driver timed out does.
fn run_refusing(
    name: &str,
    patterns: &[(Pattern, &str, Shape)],
    renderer: Renderer,
    lines: &str,
    refused: u32,
) -> Result<(Vec<u8>, String), String> {
    let work = workspace(name);
    let socket = work.join("wayland");
    let frames = work.join("frames");
    let config = undithered(&work);
    if !lines.is_empty() {
        let mut text = std::fs::read_to_string(&config).expect("the configuration");
        text.push_str(lines);
        std::fs::write(&config, text).expect("the configuration");
    }

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        dump: Some(frames.clone()),
        deadline: Some(8000),
        config: Some(config),
        renderer,
        refuse_flips: refused,
        ..Options::default()
    };

    let clients: Vec<_> = patterns
        .iter()
        .map(|(pattern, title, shape)| (*pattern, (*title).to_owned(), *shape))
        .collect();
    let socket_for_clients = socket.clone();
    let work_for_clients = work.to_path_buf();
    let started = std::thread::spawn(move || {
        // The clients wait for the socket rather than racing it: the
        // compositor binds it before it accepts anything.
        for _ in 0..400 {
            if socket_for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut handles = Vec::new();
        for (number, (pattern, title, shape)) in clients.into_iter().enumerate() {
            let path = socket_for_clients.clone();
            let drawn = work_for_clients.join(format!("drawn-{number}"));
            let marker = drawn.clone();
            handles.push(std::thread::spawn(move || {
                // Each client is told where to connect the way any client is.
                connect_announced(&path, pattern, &title, shape, marker)
            }));
            // In order, so the dwindle tree is the one the expected image was
            // built from: the first window takes the whole area and the
            // second splits it. So the next client starts once this one has
            // been drawn (`until_drawn`).
            until_drawn(&drawn);
        }
        let mut lines = Vec::new();
        for handle in handles {
            lines.push(
                handle
                    .join()
                    .unwrap_or_else(|_| "a client panicked".to_owned()),
            );
        }
        lines.join("; ")
    });

    let ran = hyprix::run(&options);
    let clients = started.join().expect("the clients finished");
    let line = ran?;

    let last = last_frame(&frames);
    Ok((last, format!("{line} | {clients}")))
}

/// One client, connected to `socket`.
fn connect(socket: &Path, pattern: Pattern, title: &str, shape: Shape) -> String {
    // `WAYLAND_DISPLAY` is how every client is told, but the environment is
    // one per process and these clients share it, so the path goes in
    // directly. `socket_path` takes a name with a slash as an absolute path,
    // which is what this is.
    compositor_pattern::client::run_shaped_on(socket, pattern, title, shape)
        .unwrap_or_else(|error| format!("pattern failed: {error}"))
}

/// The same, making `drawn` once its window has been drawn.
fn connect_announced(
    socket: &Path,
    pattern: Pattern,
    title: &str,
    shape: Shape,
    drawn: PathBuf,
) -> String {
    compositor_pattern::client::run_announced_on(socket, pattern, title, shape, drawn)
        .unwrap_or_else(|error| format!("pattern failed: {error}"))
}

/// Wait until a client has said its window was drawn, by making `drawn`,
/// and then a quarter of a second more.
///
/// The wait is what orders the windows: the next client starts only once
/// this one is placed. A fixed sleep alone was a guess at how long that
/// takes, and on a loaded host a first client slower than the guess mapped
/// second, so the two windows swapped sides and every channel of both was
/// wrong (`the_same_frame_is_drawn_on_a_gpu`, 2087572 of them each time).
///
/// The quarter of a second after it is not ordering. Two windows mapped
/// within a few milliseconds of each other leave hyprix drawing three or
/// four frames of their opening animation and then no more, so the last
/// frame is caught half-way (`docs/BACKLOG.md`, the hyprix row about
/// windows mapped together). Until that is fixed, the tests keep the gap
/// they were written with.
///
/// Bounded by the compositor's own deadline: a client that is never drawn
/// is not waited for past it, and the frame the test compares then says
/// what went wrong.
fn until_drawn(drawn: &Path) {
    for _ in 0..1600 {
        if drawn.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(250));
}

/// The last PPM in `directory`, as `XRGB8888`-order bytes.
fn last_frame(directory: &Path) -> Vec<u8> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(directory)
        .expect("the compositor wrote frames")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|kind| kind == "ppm"))
        .collect();
    names.sort();
    let last = names.last().expect("at least one frame");
    let bytes = std::fs::read(last).expect("a frame");
    let mut parts = bytes.splitn(4, |byte| *byte == b'\n');
    assert_eq!(parts.next(), Some(&b"P6"[..]));
    let size: Vec<u32> = core::str::from_utf8(parts.next().expect("a size"))
        .expect("utf-8")
        .split_whitespace()
        .map(|value| value.parse().expect("a number"))
        .collect();
    assert_eq!(size, [WIDTH, HEIGHT], "the frame is the screen's size");
    assert_eq!(parts.next(), Some(&b"255"[..]));
    parts.next().expect("pixels").to_vec()
}

/// `src/user/system/linux/compositor/render`'s expected image, as the same `(red, green, blue)`
/// bytes a PPM holds.
fn expected() -> Vec<u8> {
    image(&Path::new(env!("CARGO_MANIFEST_DIR")).join(EXPECTED))
}

/// One expected image, as the same `(red, green, blue)` bytes a PPM holds.
fn image(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let word = |at: usize| -> u32 {
        let slice: [u8; 4] = bytes
            .get(at..at + 4)
            .and_then(|slice| slice.try_into().ok())
            .expect("in range");
        u32::from_le_bytes(slice)
    };
    assert_eq!(bytes.get(..16), Some(&b"ferrix-xrgb-rle\n"[..]));
    let (width, height) = (word(16), word(20));
    assert_eq!((width, height), (WIDTH, HEIGHT));

    let mut out = Vec::with_capacity((width * height * 3) as usize);
    let mut at = 24;
    let mut previous: Vec<u8> = Vec::new();
    for _ in 0..height {
        let tag = *bytes.get(at).expect("a row tag");
        at += 1;
        if tag == 0 {
            out.extend_from_slice(&previous);
            continue;
        }
        let mut row = Vec::with_capacity((width * 3) as usize);
        while row.len() < (width * 3) as usize {
            let count = u16::from_le_bytes([
                *bytes.get(at).expect("a count"),
                *bytes.get(at + 1).expect("a count"),
            ]);
            let pixel = word(at + 2);
            at += 6;
            for _ in 0..count {
                row.extend_from_slice(&[
                    ((pixel >> 16) & 0xFF) as u8,
                    ((pixel >> 8) & 0xFF) as u8,
                    (pixel & 0xFF) as u8,
                ]);
            }
        }
        out.extend_from_slice(&row);
        previous = row;
    }
    assert_eq!(at, bytes.len(), "the expected image has bytes left over");
    out
}

/// One differing pixel: where it is, what it should have been, what it was.
type Difference = (u32, u32, [u8; 3], [u8; 3]);

/// Where two images differ: how many pixels, and the first of them.
fn compare(got: &[u8], want: &[u8]) -> (usize, Option<Difference>) {
    let mut differing = 0;
    let mut first = None;
    for index in 0..(WIDTH * HEIGHT) as usize {
        let at = index * 3;
        let (Some(mine), Some(theirs)) = (got.get(at..at + 3), want.get(at..at + 3)) else {
            break;
        };
        if mine != theirs {
            differing += 1;
            if first.is_none() {
                let x = (index as u32) % WIDTH;
                let y = (index as u32) / WIDTH;
                first = Some((
                    x,
                    y,
                    [theirs[0], theirs[1], theirs[2]],
                    [mine[0], mine[1], mine[2]],
                ));
            }
        }
    }
    (differing, first)
}

#[test]
fn two_clients_are_tiled_and_drawn_exactly_as_the_renderer_says() {
    let (frame, report) = run(
        "tiled",
        &[(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")],
    );
    assert!(
        report.contains("most 2"),
        "the compositor should have had two windows: {report}"
    );
    assert!(
        !report.contains("failed"),
        "a client did not get its window: {report}"
    );

    let (differing, first) = compare(&frame, &expected());
    assert_eq!(
        differing, 0,
        "the compositor drew something else than the renderer's expected image; \
         first difference {first:?}"
    );
}

/// The same two clients, the same compositor, the frame drawn on a GPU --
/// virglrenderer's test server, which is the renderer a guest's frames reach
/// through QEMU -- and held to the same expected image.
///
/// Not to the byte: `src/user/system/linux/compositor/render`'s `gpu` module says why a GPU's
/// frame is a step or two of a channel from the software one, and its own
/// tests hold the painter to that. What this adds is everything round it:
/// real clients' buffers named by their connection, moved as textures under
/// each frame's damage over a run of frames, fetched back for the screen and
/// dumped where a screenshot would read them.
/// A card that does not answer in time loses frames, not the desktop: on
/// 2026-10-03 a host too busy to run the GPU for five seconds made a flip
/// time out, the compositor ended on it, and every window went with it.
/// Here the screen refuses the first frames, and the two clients are still
/// drawn, exactly, once it answers again.
#[test]
fn a_card_that_times_out_drops_frames_and_keeps_the_clients() {
    let patterns = [
        (Pattern::Checkerboard, "one", Shape::Window),
        (Pattern::Gradient, "two", Shape::Window),
    ];
    let (frame, report) = run_refusing("refused", &patterns, Renderer::Software, "", 5)
        .expect("the compositor rode the refused frames out");
    assert!(
        report.contains("most 2") && !report.contains("failed"),
        "both clients should have kept their windows: {report}"
    );
    let (differing, first) = compare(&frame, &expected());
    assert_eq!(
        differing, 0,
        "the frame after the refused ones is not the whole one; first difference {first:?}"
    );
}

#[test]
fn the_same_frame_is_drawn_on_a_gpu() {
    let shaped = [
        (Pattern::Checkerboard, "one", Shape::Window),
        (Pattern::Gradient, "two", Shape::Window),
    ];
    let (frame, report) = match run_drawn_by("tiled-gpu", &shaped, Renderer::Vtest) {
        Ok(ran) => ran,
        // No test server on this host: nothing to run the GPU's frames on.
        Err(why) if why.contains("virgl_test_server") => return,
        Err(why) => panic!("the compositor did not run: {why}"),
    };
    assert!(
        report.contains("most 2") && !report.contains("failed"),
        "the compositor should have had two windows: {report}"
    );
    let want = expected();
    let apart = frame
        .iter()
        .zip(&want)
        .filter(|(mine, theirs)| mine.abs_diff(**theirs) > 3)
        .count();
    assert_eq!(frame.len(), want.len());
    assert_eq!(
        apart, 0,
        "{apart} channels of the GPU's frame are more than 3 from the expected image"
    );
}

#[test]
fn the_comparison_would_notice_a_different_frame() {
    // The check above is only worth having if it fails when the picture is
    // wrong. One client rather than two is a different picture, and the
    // comparison must say so.
    let (frame, report) = run("one-client", &[(Pattern::Checkerboard, "only")]);
    assert!(report.contains("most 1"), "{report}");
    let (differing, _) = compare(&frame, &expected());
    assert!(
        differing > 0,
        "one window drew the same picture as two, which cannot be right"
    );
}

/// A monitor stood on its edge: `monitor = , preferred, auto, 1, transform,
/// N` on the 1024x768 screen.
///
/// The clients are tiled on a monitor 768 wide and 1024 tall -- which is
/// what they are configured to and draw at -- and the screen's buffer holds
/// that picture turned, pixel for pixel the image `src/user/system/linux/compositor/render`
/// blesses for the same transform. Both quarter turns, since a compositor
/// that turned the wrong way would pass either one alone by being the other.
#[test]
fn a_turned_monitor_is_tiled_tall_and_drawn_turned() {
    let shaped = [
        (Pattern::Checkerboard, "one", Shape::Window),
        (Pattern::Gradient, "two", Shape::Window),
    ];
    for (transform, image_name) in [
        (1, "dwindle-two-clients-transform-1.xrle"),
        (3, "dwindle-two-clients-transform-3.xrle"),
    ] {
        let (frame, report) = run_configured(
            &format!("turned-{transform}"),
            &shaped,
            Renderer::Software,
            &format!("monitor = , preferred, auto, 1, transform, {transform}\n"),
        )
        .expect("the compositor ran");
        assert!(
            report.contains("most 2") && !report.contains("failed"),
            "the compositor should have had two windows: {report}"
        );
        let want = image(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../render/tests/data")
                .join(image_name),
        );
        let (differing, first) = compare(&frame, &want);
        assert_eq!(
            differing, 0,
            "transform {transform}: the screen is not the turned picture; first difference \
             {first:?}"
        );
        // And not the upright one, which a compositor that ignored the
        // line would have drawn.
        let (upright, _) = compare(&frame, &expected());
        assert!(
            upright > 0,
            "transform {transform} drew the upright picture"
        );
    }
}

// ---------------------------------------------------------------------------
// A real toolkit
//
// The test above proves that this tree's two halves agree with each other.
// `probe/real-client.sh` proves the other thing: that an application written
// against libwayland and every other compositor, which knows nothing about
// this one, gets a window and draws in it. Every gap it found -- the
// clipboard's objects, subsurfaces, an output that described itself -- was
// one the pattern client could never have found, because the pattern client
// is written against the same crates the server is.
// ---------------------------------------------------------------------------

/// What `probe/real-client.sh` recorded.
const REAL_CLIENT: &str = include_str!("../probe/real-client.txt");

/// A `frame <name> <value>` line from the record.
fn frame_value(name: &str) -> Option<&'static str> {
    REAL_CLIENT
        .lines()
        .find_map(|line| line.strip_prefix(&format!("frame {name} ")))
}

#[test]
fn a_real_toolkit_gets_a_window_and_draws_in_it() {
    assert!(
        REAL_CLIENT
            .lines()
            .next()
            .is_some_and(|line| line.starts_with("# foot ")),
        "probe/real-client.txt should say which client wrote it"
    );

    // It read the output's mode. A compositor whose wl_output describes
    // nothing gets `(null): 0x0+0x0@0Hz` here, which is what the first run
    // printed.
    assert!(
        REAL_CLIENT.contains("HEADLESS-1: 1024x768+0x0@60Hz hyprix"),
        "the client did not read the output's mode:\n{REAL_CLIENT}"
    );
    // It worked out its own geometry from it, which means the mode was
    // usable and not merely present.
    assert!(
        REAL_CLIENT.contains("cell width=10, height=19"),
        "the client did not lay out its terminal"
    );
    // And it ended by choice rather than by a protocol error.
    assert!(
        REAL_CLIENT.contains("client info: main.c:696: goodbye"),
        "the client did not exit cleanly"
    );
    assert!(
        !REAL_CLIENT.contains("Protocol error"),
        "the compositor refused a real client:\n{REAL_CLIENT}"
    );
    assert!(
        !REAL_CLIENT.contains("Broken pipe"),
        "the compositor went away before the client did"
    );
    assert!(
        REAL_CLIENT.contains("most 1"),
        "the compositor never gave the client a window"
    );

    // And it drew: most of the screen is its window rather than the
    // compositor's background.
    assert_eq!(frame_value("size"), Some("1024x768"));
    let drawn: u64 = frame_value("not-background")
        .and_then(|value| value.parse().ok())
        .expect("a count of drawn pixels");
    assert!(
        drawn > 600_000,
        "only {drawn} pixels of 786432 were the client's window"
    );
    let colours: u32 = frame_value("colours")
        .and_then(|value| value.parse().ok())
        .expect("a count of colours");
    assert!(colours > 1, "the frame is one flat colour, so nothing drew");
}

// ---------------------------------------------------------------------------
// Hyprland's own client
//
// `src/user/system/linux/compositor/ipc`'s tests check the answers against Hyprland's source.
// `probe/hyprctl.sh` checks them against Hyprland's client: the program people
// type, and the one every script and bar is written around. If `hyprctl
// clients` prints nothing here, no Hyprland script works on this compositor
// whatever the JSON says.
// ---------------------------------------------------------------------------

/// What `probe/hyprctl.sh` recorded.
const HYPRCTL: &str = include_str!("../probe/hyprctl.txt");

/// What `<client> <command>` printed, up to the next command of either
/// client's.
fn recorded(client: &str, command: &str) -> &'static str {
    let marker = format!("\n$ {client} {command}\n");
    let start = HYPRCTL
        .find(&marker)
        .unwrap_or_else(|| panic!("{client} {command:?} is not in probe/hyprctl.txt"))
        + marker.len();
    let rest = HYPRCTL.get(start..).unwrap_or("");
    let end = rest.find("\n$ ").unwrap_or(rest.len());
    rest.get(..end).unwrap_or("").trim_end()
}

/// What Hyprland's own `hyprctl <command>` printed.
fn hyprctl(command: &str) -> &'static str {
    recorded("hyprctl", command)
}

/// What `src/user/system/linux/compositor/ctl`'s `hyprctl <command>` printed.
fn ours(command: &str) -> &'static str {
    recorded("ours", command)
}

/// Every read-only command the probe ran through both clients must have got
/// one answer.
///
/// That is the whole claim `src/user/system/linux/compositor/ctl` makes: a script written for
/// `hyprctl` works when the program it calls is ours, which is what Ferrix's
/// image carries because Hyprland's is not on it. The two ran against one
/// compositor in one session, so even the window addresses are comparable.
#[test]
fn our_client_and_hyprlands_get_one_answer() {
    for command in [
        "version",
        "monitors",
        "workspaces",
        "activewindow",
        "-j activewindow",
        "clients",
        "-j clients",
        "nonsense",
    ] {
        assert_eq!(
            ours(command),
            hyprctl(command),
            "`{command}` was answered differently"
        );
        assert!(!ours(command).is_empty(), "`{command}` answered nothing");
    }
}

#[test]
fn hyprlands_own_client_reads_this_compositor() {
    // The commands a person types and a script calls.
    assert!(
        hyprctl("version").contains("hyprix"),
        "version said nothing"
    );
    assert!(
        hyprctl("monitors").contains("Monitor HEADLESS-1 (ID 0):"),
        "monitors: {}",
        hyprctl("monitors")
    );
    assert!(
        hyprctl("monitors").contains("1024x768@60.00000 at 0x0"),
        "the mode is not in Hyprland's own shape"
    );
    assert!(
        hyprctl("workspaces").contains("workspace ID 1 (1) on monitor HEADLESS-1:"),
        "workspaces: {}",
        hyprctl("workspaces")
    );
    assert!(
        hyprctl("workspaces").contains("windows: 2"),
        "both windows should be on the workspace"
    );

    // Both clients are there, with the titles and the app id they set.
    let clients = hyprctl("clients");
    assert!(clients.contains("title: one"), "clients: {clients}");
    assert!(clients.contains("title: two"), "clients: {clients}");
    assert!(clients.contains("class: rocks.magical.pattern"));
}

#[test]
fn a_dispatcher_typed_at_hyprctl_moves_the_focus() {
    // The focus starts on the window opened last, and `movefocus l` is what
    // every Hyprland configuration binds.
    assert!(
        hyprctl("activewindow").contains("title: two"),
        "the focus should start on the second window"
    );
    assert_eq!(hyprctl("dispatch movefocus l").trim(), "ok");
    // The second `activewindow` in the record is after the dispatch.
    let after = HYPRCTL
        .rsplit_once("\n$ hyprctl activewindow\n")
        .map(|(_, rest)| rest)
        .expect("a second activewindow");
    assert!(
        after.contains("title: one"),
        "movefocus did not move the focus:\n{after}"
    );
}

#[test]
fn an_option_changed_at_hyprctl_re_tiles_every_window() {
    assert_eq!(hyprctl("keyword general:gaps_in 40").trim(), "ok");
    // Before: 485 wide with the default gaps. After: 450, and both windows
    // moved. A compositor that took the keyword and did not re-tile would
    // still say `ok`.
    let before = hyprctl("-j activewindow");
    assert!(before.contains("\"size\": [485, 726]"), "before: {before}");
    // The `-j clients` the probe ran after the keyword, which is the only
    // one: the ones before it were `activewindow`.
    let after = hyprctl("-j clients");
    assert!(
        after.contains("\"size\": [450, 726]"),
        "the windows were not re-tiled:\n{after}"
    );
    assert_eq!(
        after.matches("\"size\": [450, 726]").count(),
        2,
        "both windows should have been re-tiled"
    );
}

#[test]
fn an_unknown_request_is_answered_rather_than_hanging_hyprctl() {
    // Hyprland answers a line and keeps the connection. A compositor that
    // closed it instead leaves `hyprctl` with nothing to print, which is what
    // a person sees as a hang.
    assert!(
        hyprctl("nonsense").contains("unknown request nonsense"),
        "{}",
        hyprctl("nonsense")
    );
}

/// A request that arrives well after its connection is still heard.
///
/// A client connects and then writes, and a guest that did not run
/// `hyprctl` for a quarter of a second between the two had its request
/// answered as an empty one, `unknown request`, or its write refused with
/// `Broken pipe` (docs/BACKLOG.md, P1 flakes, 2026-09-28). The compositor
/// now reads a request as it arrives, beside everything else it waits on.
#[test]
fn a_request_that_is_slow_to_arrive_is_still_answered() {
    let work = workspace("slow-request");
    let runtime = work.join("runtime");
    let instance = runtime.join("hypr").join("ferrix-test");
    std::fs::create_dir_all(&instance).expect("an instance directory");
    let requests = instance.join(compositor_ipc::REQUEST_SOCKET);
    let options = Options {
        display: work.join("wayland").to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        instance: Some(instance.to_string_lossy().into_owned()),
        deadline: Some(2500),
        ..Options::default()
    };
    let asker = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        for _ in 0..400 {
            if requests.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut stream = std::os::unix::net::UnixStream::connect(&requests)
            .map_err(|error| format!("connecting: {error}"))?;
        // Well past the quarter of a second the compositor used to wait.
        std::thread::sleep(Duration::from_millis(600));
        stream
            .write_all(b"version\n")
            .map_err(|error| format!("writing: {error}"))?;
        stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(|error| format!("finishing: {error}"))?;
        let mut answer = String::new();
        let _ = stream
            .read_to_string(&mut answer)
            .map_err(|error| format!("reading: {error}"))?;
        Ok::<_, String>(answer)
    });
    let _ = hyprix::run(&options).expect("the compositor ran");
    let answer = asker.join().expect("the asker finished").expect("asked");
    assert!(
        !answer.contains("unknown request") && answer.contains("hyprix"),
        "the request was not heard: {answer:?}"
    );
}

// ---------------------------------------------------------------------------
// The event socket
//
// `.socket2.sock` is what a bar reads. `src/user/system/linux/compositor/ipc`'s tests check each
// line's shape against Hyprland's own `postEvent` calls; this checks that the
// compositor puts them on a socket a reader can get at, in the order a reader
// needs, while two clients come and go.
// ---------------------------------------------------------------------------

/// Run the compositor with a subscriber on its event socket, and give back
/// every line the subscriber read.
fn subscribed(name: &str, kill: bool) -> Vec<String> {
    let work = workspace(name);
    let socket = work.join("wayland");
    let runtime = work.join("runtime");
    std::fs::create_dir_all(&runtime).expect("a runtime directory");
    // The instance is given as a directory rather than a name, so this test
    // needs no `XDG_RUNTIME_DIR`: an environment variable is one per process
    // and the tests run in threads of one.
    let instance = runtime.join("hypr").join("ferrix-test");
    let events = instance.join(compositor_ipc::EVENT_SOCKET);
    let requests = instance.join(compositor_ipc::REQUEST_SOCKET);

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        instance: Some(instance.to_string_lossy().into_owned()),
        deadline: Some(4000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let listening = events.clone();
    let reader = std::thread::spawn(move || {
        for _ in 0..800 {
            if listening.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut lines = Vec::new();
        let _ = compositor_ctl::subscribe(&listening, &mut |line| lines.push(line.to_owned()));
        lines
    });

    let socket_for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if socket_for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The subscriber has to be connected before the first window, or it
        // is told the state instead of being told the window arrived.
        std::thread::sleep(Duration::from_millis(400));
        let mut handles = Vec::new();
        for (pattern, title) in [(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")] {
            let path = socket_for_clients.clone();
            handles.push(std::thread::spawn(move || {
                connect(&path, pattern, title, Shape::Window)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        // `killactive` through the request socket, so the window really
        // closes and the socket really says so: the clients themselves run
        // until the compositor goes, as a window does.
        if kill {
            std::thread::sleep(Duration::from_millis(500));
            let _ = compositor_ctl::ask(&requests, "dispatch killactive");
        }
        for handle in handles {
            let _ = handle.join();
        }
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    clients.join().expect("the clients finished");
    let lines = reader.join().expect("the subscriber finished");
    assert!(
        line.contains("subscribers 1"),
        "the compositor saw no subscriber: {line}"
    );
    lines
}

#[test]
fn a_bar_on_the_event_socket_is_told_what_happens() {
    let lines = subscribed("events", false);
    assert!(!lines.is_empty(), "the subscriber read nothing");

    // What already existed when it connected: the monitor and the workspace.
    assert!(
        lines.iter().any(|line| line == "monitoradded>>HEADLESS-1"),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "createworkspacev2>>1,1"),
        "{lines:?}"
    );

    // Then each window arriving, with its class and title.
    let opened: Vec<&String> = lines
        .iter()
        .filter(|line| line.starts_with("openwindow>>"))
        .collect();
    assert_eq!(opened.len(), 2, "{lines:?}");
    assert!(
        opened[0].ends_with(",1,rocks.magical.pattern,one"),
        "{:?}",
        opened[0]
    );
    assert!(
        opened[1].ends_with(",1,rocks.magical.pattern,two"),
        "{:?}",
        opened[1]
    );

    // And the focus moving onto each, in both shapes.
    assert!(
        lines
            .iter()
            .any(|line| line == "activewindow>>rocks.magical.pattern,two"),
        "{lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("activewindowv2>>"))
            .count(),
        2,
        "{lines:?}"
    );

    // Every line is one line and holds no newline of its own, which is what
    // `formatEvent` promises a reader.
    for line in &lines {
        assert!(!line.contains('\n'), "{line:?}");
        assert!(line.contains(">>"), "{line:?}");
    }
}

/// A bar that is not told a window closed shows a window that is gone.
///
/// The window is closed by `hyprctl dispatch killactive` on the request
/// socket, which is how a person closes one: the clients themselves run
/// until the compositor goes.
#[test]
fn a_window_closing_reaches_the_socket_too() {
    let lines = subscribed("events-closing", true);
    assert!(
        lines.iter().any(|line| line.starts_with("closewindow>>")),
        "nothing said a window closed: {lines:?}"
    );
    // And the focus moved to the one that is left, rather than being left on
    // a window that no longer exists.
    let focused: Vec<&String> = lines
        .iter()
        .filter(|line| line.starts_with("activewindow>>"))
        .collect();
    assert!(focused.len() >= 3, "{lines:?}");
}

/// A bar through `zwlr_layer_shell_v1`, and the windows tiling under it.
///
/// This is the whole of what a Hyprland setup needs before it will start:
/// `waybar` is a layer surface, `hyprpaper` is a layer surface, and a
/// compositor that does not place them puts the windows over the bar or
/// under it.
#[test]
fn a_bar_takes_its_strip_and_the_windows_tile_under_it() {
    let (frame, report) = run_shaped(
        "bar",
        &[
            (Pattern::Checkerboard, "bar", Shape::Bar(30)),
            (Pattern::Checkerboard, "one", Shape::Window),
            (Pattern::Gradient, "two", Shape::Window),
        ],
    );
    assert!(
        report.contains("most 2"),
        "the bar should not be a window: {report}"
    );
    assert!(!report.contains("failed"), "{report}");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(BAR_EXPECTED);
    let (differing, first) = compare(&frame, &image(&path));
    assert_eq!(
        differing, 0,
        "the compositor drew something else than the renderer's expected image; \
         first difference {first:?}"
    );
}

// ---------------------------------------------------------------------------
// Animations
//
// `src/user/system/linux/compositor/anim`'s tests check the curves against the numbers hyprutils'
// own algorithm produces. This checks that a window really slides: that the
// frames between two layouts hold the window at places neither layout put it,
// and that those places are on the curve rather than a straight line.
// ---------------------------------------------------------------------------

/// Run the compositor with two clients and a configuration, dispatching
/// `after` through the control socket once both windows are up, and give
/// back every frame it drew.
///
/// The deadline is wall-clock and the dispatch is a second into the run, so
/// it has to leave room for the whole animation after that: a run that ends
/// mid-slide gives a test the first part of the curve and nothing else,
/// which under a loaded machine is what a four-second deadline did.
fn frames_after(name: &str, config: &str, after: &str) -> Vec<Vec<u8>> {
    let work = workspace(name);
    let socket = work.join("wayland");
    let frames = work.join("frames");
    let runtime = work.join("runtime");
    let instance = runtime.join("hypr").join("ferrix-test");
    std::fs::create_dir_all(&instance).expect("an instance directory");
    let requests = instance.join(compositor_ipc::REQUEST_SOCKET);
    let config_path = work.join("hyprland.conf");
    // The dither off, for the reason `undithered` gives; a test that
    // wants it says so after this line and wins.
    std::fs::write(&config_path, format!("decoration:blur:noise = 0\n{config}"))
        .expect("a configuration");

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        instance: Some(instance.to_string_lossy().into_owned()),
        config: Some(config_path),
        dump: Some(frames.clone()),
        deadline: Some(4000),
        ..Options::default()
    };

    let socket_for_clients = socket.clone();
    let after = after.to_owned();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if socket_for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut handles = Vec::new();
        for (pattern, title) in [(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")] {
            let path = socket_for_clients.clone();
            handles.push(std::thread::spawn(move || {
                connect(&path, pattern, title, Shape::Window)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        // Both windows are up and settled; now make them move.
        std::thread::sleep(Duration::from_millis(900));
        let _ = compositor_ctl::ask(&requests, &after);
        for handle in handles {
            let _ = handle.join();
        }
    });

    let _ = hyprix::run(&options).expect("the compositor ran");
    clients.join().expect("the clients finished");

    let mut names: Vec<PathBuf> = std::fs::read_dir(&frames)
        .expect("the compositor wrote frames")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|kind| kind == "ppm"))
        .collect();
    names.sort();
    names
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path).expect("a frame");
            let mut parts = bytes.splitn(4, |byte| *byte == b'\n');
            let _ = parts.next();
            let _ = parts.next();
            let _ = parts.next();
            parts.next().expect("pixels").to_vec()
        })
        .collect()
}

/// Where the gradient window's left edge is on a row, once both windows are
/// on the screen.
///
/// The gradient is the only thing on the screen whose pixels are none of the
/// colours everything else is: the compositor's background, the
/// checkerboard's two greys and the two border colours. So the first column
/// that is none of them is its left edge, wherever it is -- including while
/// it is drawn scaled, part-way through a move, where a measure that looked
/// for the gap between two windows finds several.
///
/// `None` until the row holds both patterns, which is the frames before the
/// second client has drawn: a screen with one window on it says nothing
/// about where two of them are.
fn gradient_edge(frame: &[u8], row: usize) -> Option<usize> {
    let checkerboard: [[u8; 3]; 2] = [[0xE0, 0xE0, 0xE0], [0x30, 0x30, 0x30]];
    let others: [[u8; 3]; 3] = [[0x11, 0x11, 0x11], [0xFF, 0xFF, 0xFF], [0x44, 0x44, 0x44]];
    let pixel = |x: usize| -> Option<&[u8]> {
        let at = (row * WIDTH as usize + x) * 3;
        frame.get(at..at + 3)
    };
    let is_gradient = |x: usize| {
        pixel(x).is_some_and(|colour| {
            !checkerboard.iter().any(|one| one == colour) && !others.iter().any(|one| one == colour)
        })
    };
    let has_checkerboard = (0..WIDTH as usize)
        .any(|x| pixel(x).is_some_and(|colour| checkerboard.iter().any(|one| one == colour)));
    if !has_checkerboard {
        return None;
    }
    let edge = (0..WIDTH as usize).find(|x| is_gradient(*x))?;
    Some(edge)
}

/// The edges from the moment the two windows were settled in their first
/// arrangement, which is the first frame the moving window is at its
/// right-hand place.
///
/// The frames before it are the two clients arriving: a window whose client
/// has not redrawn at the size it was just configured to is drawn stretched,
/// and a stretched checkerboard has greys between its two, which the edge
/// measure cannot tell from the gradient. Nothing about the move is in those
/// frames.
fn once_settled(edges: &[usize]) -> &[usize] {
    let Some(&start) = edges.iter().max() else {
        return &[];
    };
    let at = edges.iter().position(|edge| *edge == start).unwrap_or(0);
    edges.get(at..).unwrap_or(&[])
}

/// A window swapped with its neighbour slides there, and the frames on the
/// way hold it at places neither layout put it.
#[test]
fn a_window_moves_through_the_frames_between_two_layouts() {
    let frames = frames_after(
        "animated",
        // Two seconds rather than Hyprland's default 0.8, and the blur off.
        // What is measured here is where the window was in each frame that
        // was drawn, and the compositor draws a handful of them a second in
        // a debug build: a short slide is sampled two or three times, and
        // the first sample is already most of the way there. A longer one
        // is the same curve with more points on it.
        "animation = windows, 1, 20, default\ndecoration:blur:enabled = 0\n",
        "dispatch movewindow l",
    );
    assert!(frames.len() > 10, "only {} frames", frames.len());

    // The upper quarter, which is the gradient's opaque half.
    let row = HEIGHT as usize / 4;
    let measured: Vec<usize> = frames
        .iter()
        .filter_map(|frame| gradient_edge(frame, row))
        .collect();
    let seams = once_settled(&measured).to_vec();
    assert!(seams.len() > 5, "the window was never found: {seams:?}");

    // A swap moves the seam, and the frames in between hold it somewhere
    // else again: with no animation there would be two positions and no
    // more.
    let mut distinct: Vec<usize> = seams.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() > 3,
        "the seam only ever had {} positions, so nothing slid: {distinct:?}",
        distinct.len()
    );

    // It slid one way and did not wander: the seam never goes backwards
    // once it has started, which a window drawn from a stale rectangle
    // would.
    let start = seams.first().copied();
    let moving: Vec<usize> = seams
        .iter()
        .copied()
        .skip_while(|at| Some(*at) == start)
        .collect();
    let forwards = moving.windows(2).all(|pair| pair[0] <= pair[1]);
    let backwards = moving.windows(2).all(|pair| pair[0] >= pair[1]);
    assert!(forwards || backwards, "the seam wandered: {moving:?}");

    // And it is on a curve rather than a straight line. Hyprland's
    // `default` starts fast: by the middle frame of the move it is well
    // past half way, which `src/user/system/linux/compositor/anim`'s own test puts at 0.843 of
    // the distance a quarter of the way in.
    let (Some(first), Some(last)) = (seams.first().copied(), seams.last().copied()) else {
        panic!("no seam at all");
    };
    let span = last.abs_diff(first);
    // Fifty pixels rather than the whole slide: what is measured is the
    // leading edge of the moving window in the frames that were *drawn*,
    // and the first of those is already some way along a curve that starts
    // fast. The distance that proves a slide is one no border, gap or
    // rounding could account for, and this is four times the widest of
    // them.
    assert!(span > 50, "the seam moved only {span} pixels: {seams:?}");
    let middle = seams.get(seams.len() / 2).copied().expect("a middle frame");
    let covered = middle.abs_diff(first);
    assert!(
        covered * 2 > span,
        "half way through the move it had covered {covered} of {span}, which is not a curve \
         that starts fast"
    );
}

/// With `animations:enabled = 0` the same swap is instant: the seam has the
/// two positions the two layouts give it and nothing between.
#[test]
fn animations_can_be_turned_off() {
    let frames = frames_after(
        "instant",
        "animations:enabled = 0\ndecoration:blur:enabled = 0\n",
        "dispatch movewindow l",
    );
    let row = HEIGHT as usize / 4;
    let measured: Vec<usize> = frames
        .iter()
        .filter_map(|frame| gradient_edge(frame, row))
        .collect();
    let order = once_settled(&measured).to_vec();
    let mut distinct: Vec<usize> = order.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() <= 2,
        "with animations off the window was at {} places: {order:?}",
        distinct.len()
    );
}

// ---------------------------------------------------------------------------
// Plugins
//
// A plugin is a program the compositor starts, which connects to the control
// socket, says what it is, and adds a dispatcher. `src/user/system/linux/compositor/plug` is the
// one Ferrix carries; this speaks the same protocol from a thread, which is
// what lets a host test press the dispatcher and look at the frame.
// ---------------------------------------------------------------------------

/// Run the compositor with two clients and a plugin, and give back the last
/// frame, the compositor's line, and what `hyprctl plugin list` said.
fn with_a_plugin(name: &str) -> (Vec<u8>, String, String) {
    let work = workspace(name);
    let socket = work.join("wayland");
    let frames = work.join("frames");
    let runtime = work.join("runtime");
    let instance = runtime.join("hypr").join("ferrix-test");
    std::fs::create_dir_all(&instance).expect("an instance directory");
    let requests = instance.join(compositor_ipc::REQUEST_SOCKET);

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        instance: Some(instance.to_string_lossy().into_owned()),
        dump: Some(frames.clone()),
        deadline: Some(8000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let socket_for_clients = socket.clone();
    let listed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let said = std::sync::Arc::clone(&listed);
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if socket_for_clients.exists() && requests.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut handles = Vec::new();
        for (pattern, title) in [(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")] {
            let path = socket_for_clients.clone();
            handles.push(std::thread::spawn(move || {
                connect(&path, pattern, title, Shape::Window)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        // The plugin: the protocol `src/user/system/linux/compositor/plug` speaks, from here.
        let plugin = std::os::unix::net::UnixStream::connect(&requests)
            .expect("the plugin connects to the control socket");
        let mut reading = plugin.try_clone().expect("the plugin's connection");
        let mut writing = plugin;
        let mut say = move |line: &str| {
            use std::io::Write as _;
            writing
                .write_all(line.as_bytes())
                .expect("the plugin writes");
            writing.flush().expect("the plugin flushes");
        };
        say("[[PLUGIN]]swap,ferrix,1.0,swaps the focused window with its neighbour\n");
        say("handle swapthem\n");
        std::thread::sleep(Duration::from_millis(500));

        // `hyprctl plugin list` sees it, and `hyprctl dispatch swapthem`
        // reaches it: a dispatcher the layout has never heard of.
        if let Ok(text) = compositor_ctl::ask(&requests, "plugin list") {
            said.lock().expect("the answer").push_str(&text);
        }
        let _ = compositor_ctl::ask(&requests, "dispatch swapthem");

        // The plugin answers `dispatch movewindow r`, which the compositor
        // runs on its next pass; this reads the line and sends it, as the
        // program does.
        let mut reply = String::new();
        {
            use std::io::Read as _;
            reading
                .set_read_timeout(Some(Duration::from_millis(500)))
                .expect("a read timeout");
            // Until the dispatch arrives: the two `ok`s for the hello and
            // the registration come first, and each may be a read of its
            // own.
            for _ in 0..8 {
                let mut buffer = [0u8; 512];
                match reading.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        reply.push_str(&String::from_utf8_lossy(buffer.get(..read).unwrap_or(&[])));
                    }
                    Err(_) => {}
                }
                if reply.contains("dispatch>>") {
                    break;
                }
            }
        }
        if reply.contains("dispatch>>swapthem") {
            // What `src/user/system/linux/compositor/plug` sends: the two dispatchers that
            // exchange the windows, as one batch.
            say("[[BATCH]]dispatch movefocus l ; dispatch movewindow r\n");
        }
        std::thread::sleep(Duration::from_millis(500));

        let mut lines = vec![format!("plugin heard {}", reply.trim())];
        for handle in handles {
            lines.push(
                handle
                    .join()
                    .unwrap_or_else(|_| "a client panicked".to_owned()),
            );
        }
        lines.join("; ")
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let clients = clients.join().expect("the clients finished");
    let frame = last_frame(&frames);
    let listed = listed.lock().expect("the answer").clone();
    (frame, format!("{line} | {clients}"), listed)
}

/// A plugin adds a dispatcher, the compositor hands it the dispatch, and what
/// the plugin asks for in return is what the screen shows.
#[test]
fn a_plugin_adds_a_dispatcher_and_the_compositor_hands_it_over() {
    let (frame, report, listed) = with_a_plugin("plugin");
    assert!(!report.contains("a client panicked"), "{report}");

    // `hyprctl plugin list` names it, in Hyprland's own shape.
    assert!(listed.contains("Plugin swap by ferrix:"), "{listed}");
    assert!(listed.contains("Dispatchers: swapthem"), "{listed}");

    // The compositor handed the dispatcher over rather than refusing it.
    assert!(
        report.contains("dispatch>>swapthem"),
        "the plugin was not given its dispatcher: {report}"
    );

    // And what the plugin asked for happened: `movewindow r` with the focus
    // on the gradient swaps the two, which is the picture the renderer's own
    // tests bless for that state.
    let want = image(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../render/tests/data/dwindle-two-clients-swapped.xrle"),
    );
    let (differing, first) = compare(&frame, &want);
    assert_eq!(
        differing, 0,
        "the plugin's dispatcher did not swap the windows; first difference {first:?}"
    );
}

// ---------------------------------------------------------------------------
// The clipboard
//
// Wayland's clipboard is a promise: the program that copied keeps the data,
// and the compositor passes a pipe from whoever pastes to whoever copied.
// What is tested is that the two ends meet -- text put in by one process
// comes out of another, with the compositor in between and never holding it.
// ---------------------------------------------------------------------------

/// What the copying client puts on the clipboard.
const COPIED: &str = "a line that crossed the clipboard";

#[test]
fn what_one_client_copies_another_pastes() {
    // Both selections, because they are the same protocol twice over and a
    // compositor that carried one and not the other would pass a test of
    // either alone.
    for which in [
        compositor_clip::Which::Clipboard,
        compositor_clip::Which::Primary,
    ] {
        one_selection(which);
    }
}

/// One selection, copied in one program and pasted in another.
fn one_selection(which: compositor_clip::Which) {
    let work = workspace("clipboard");
    let socket = work.join("wayland");

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        deadline: Some(8000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The copy stays alive to answer, as every Wayland clipboard owner
        // must; the paste runs beside it.
        let copying = for_clients.clone();
        let copier = std::thread::spawn(move || {
            compositor_clip::copy(&copying, which, COPIED, Duration::from_secs(6))
        });
        // A moment for the selection to be set before anything asks for it:
        // a paste that arrives first is told there is nothing, which is
        // true.
        std::thread::sleep(Duration::from_millis(400));
        let pasted = compositor_clip::paste(&for_clients, which);
        let copied = copier.join().unwrap_or_else(|_| Err("panicked".to_owned()));
        (copied, pasted)
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let (copied, pasted) = clients.join().expect("the clients finished");
    // Both, before either is unwrapped: a failure in one usually explains
    // the other.
    assert!(
        copied.is_ok() && pasted.is_ok(),
        "the copy said {copied:?} and the paste said {pasted:?}; the compositor said {line}"
    );
    let copied = copied.expect("the copy");
    let pasted = pasted.expect("the paste");

    assert_eq!(pasted, COPIED, "what came out is not what went in");
    assert!(
        copied.contains("asked for 1 times"),
        "the copying client was asked for its data once: {copied}"
    );
    // And the compositor counted both halves.
    assert!(
        line.contains("copied 1 pasted 1"),
        "the compositor's line does not say what the clipboard did: {line}"
    );
}

/// A bar's half of the protocol: `zwlr_foreign_toplevel_management_v1`.
///
/// `src/user/system/linux/compositor/lswt` is a taskbar with the drawing taken out -- it binds the
/// manager, takes a handle for each window, and reads the title, the
/// application id and the states. What is required here is that the two
/// windows the compositor is tiling are the two it describes, with the
/// focused one marked, and that a request sent back through a handle reaches
/// the window it names: `close` on a window nobody owns is what a middle
/// click on a taskbar entry does.
#[test]
fn a_bar_is_told_which_windows_there_are_and_can_close_one() {
    let work = workspace("toplevels");
    let socket = work.join("wayland");

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        deadline: Some(8000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut windows = Vec::new();
        for (pattern, title) in [(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")] {
            let path = for_clients.clone();
            windows.push(std::thread::spawn(move || {
                connect(&path, pattern, title, Shape::Window)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        // Both windows are up by now, which is what the list is of.
        let listed = compositor_lswt::run(&for_clients, &compositor_lswt::Want::List);
        // And one of them is asked to close, from outside it.
        let closed = compositor_lswt::run(
            &for_clients,
            &compositor_lswt::Want::Close("one".to_owned()),
        );
        // A moment for the window to go, and then the list again: what the
        // bar was told is only true if the window it named is the one that
        // went.
        std::thread::sleep(Duration::from_millis(500));
        let after = compositor_lswt::run(&for_clients, &compositor_lswt::Want::List);
        for window in windows {
            let _ = window.join();
        }
        (listed, closed, after)
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let (listed, closed, after) = clients.join().expect("the clients finished");
    let listed = listed.unwrap_or_else(|why| panic!("listing: {why}; the compositor said {line}"));

    assert_eq!(
        listed.len(),
        2,
        "the bar was told about {} windows, not two: {listed:?}; the compositor said {line}",
        listed.len()
    );
    // Both, by application id and title, in the order they opened.
    assert!(
        listed[0].contains("rocks.magical.pattern") && listed[0].contains("\"one\""),
        "the first window: {listed:?}"
    );
    assert!(
        listed[1].contains("\"two\""),
        "the second window: {listed:?}"
    );
    // The focused one is marked, and it is the one that opened last.
    assert!(
        !listed[0].contains("activated") && listed[1].contains("activated"),
        "the focused window is not the one marked: {listed:?}"
    );

    let closed = closed.unwrap_or_else(|why| panic!("closing: {why}"));
    assert!(
        closed.iter().any(|line| line.contains("closed \"one\"")),
        "the close was not sent: {closed:?}"
    );

    // And the window it named is the one that went: `pattern` leaves when
    // the compositor asks its window to close, so the list is now one.
    let after = after.unwrap_or_else(|why| panic!("listing again: {why}"));
    assert_eq!(
        after.len(),
        1,
        "after closing one window the bar sees {after:?}; the compositor said {line}"
    );
    assert!(
        after[0].contains("\"two\""),
        "the wrong window was closed: {after:?}; before: {listed:?}; the compositor said {line}"
    );
}

/// The screen once it has stopped changing: shots until [`SAME`] of them in
/// a row are equal, and that one.
///
/// This is what the 500 ms sleep it replaced was reaching for. It waits on
/// the screen itself rather than on a clock, so a host that takes ten times
/// as long to paint two windows gives the same picture rather than an
/// earlier one. It never looks at what is *in* a shot, only at whether two
/// are the same, so it cannot make a wrong frame pass.
fn settled(socket: &Path) -> Result<compositor_shot::Shot, String> {
    /// How many equal shots in a row mean the screen has stopped.
    const SAME: usize = 4;
    /// Between shots.
    const APART: Duration = Duration::from_millis(100);
    /// At most, which is well inside the compositor's own deadline.
    const TRIES: usize = 60;

    let mut last: Option<compositor_shot::Shot> = None;
    let mut run = 0;
    let mut latest = Err("no screenshot was ever taken".to_string());
    for _ in 0..TRIES {
        let taken = compositor_shot::take(socket, 0);
        run = match (&taken, &last) {
            (Ok(now), Some(before)) if now == before => run + 1,
            (Ok(_), _) => 1,
            (Err(_), _) => 0,
        };
        last = taken.as_ref().ok().cloned();
        latest = taken;
        if run >= SAME {
            break;
        }
        std::thread::sleep(APART);
    }
    latest
}

/// A screenshot, through `zwlr_screencopy_v1`.
///
/// `src/user/system/linux/compositor/shot` is `grim` without the file format: it binds the
/// manager and a `wl_output`, is told what buffer to make, makes one, hands
/// it over and reads back what the compositor wrote into it. What is
/// required is that those pixels are the picture `src/user/system/linux/compositor/render`
/// blesses for the same two windows -- so a screenshot taken over the socket
/// and a frame built by calling the renderer with rectangles agree byte for
/// byte, which is the same standard every other picture in this tree is held
/// to.
#[test]
fn a_screenshot_is_the_frame_the_renderer_blesses() {
    let (taken, _, line) = screenshot_of("screenshot", "");
    let want = expected();
    assert_eq!(
        taken.pixels.len(),
        want.len(),
        "the screenshot has {} bytes and the expected image {}",
        taken.pixels.len(),
        want.len()
    );
    let differing = taken
        .pixels
        .chunks_exact(3)
        .zip(want.chunks_exact(3))
        .filter(|(shot, blessed)| shot != blessed)
        .count();
    assert_eq!(
        differing,
        0,
        "{differing} of {} pixels in the screenshot are not the renderer's; the compositor said \
         {line}",
        (WIDTH * HEIGHT) as usize
    );
}

/// `windowrule = no_screen_share`: a screenshot shows a black box where the
/// window is, and the screen shows the window as ever.
///
/// Hyprland's `CScreenshareFrame::renderMonitor` copies the monitor's frame
/// and draws a black box over the window's own rectangle -- not its border
/// -- rounded as the window is. So the test is three claims about one run:
/// the screen's last frame is still the renderer's blessed picture; inside
/// the second window's rectangle the screenshot is black; and outside it the
/// screenshot is that blessed picture too, so nothing else went with it.
#[test]
fn a_window_that_is_not_shared_is_a_black_box_in_a_screenshot() {
    let (taken, frame, line) = screenshot_of(
        "unshared",
        "windowrule = no_screen_share, match:title ^(two)$\n",
    );
    let want = expected();
    let (on_screen, first) = compare(&frame, &want);
    assert_eq!(
        on_screen, 0,
        "the screen itself changed, first at {first:?}; the compositor said {line}"
    );
    let two = second_window();
    let mut inside = 0;
    for (index, (shot, blessed)) in taken
        .pixels
        .chunks_exact(3)
        .zip(want.chunks_exact(3))
        .enumerate()
    {
        let (x, y) = (
            i64::try_from(index % WIDTH as usize).expect("a column"),
            i64::try_from(index / WIDTH as usize).expect("a row"),
        );
        let hidden =
            (two.x..two.x + two.width).contains(&x) && (two.y..two.y + two.height).contains(&y);
        if hidden {
            inside += 1;
            assert_eq!(shot, [0, 0, 0], "({x}, {y}) shows through the box; {line}");
        } else {
            assert_eq!(shot, blessed, "({x}, {y}) is not the screen's; {line}");
        }
    }
    assert_eq!(
        inside,
        two.width * two.height,
        "the box is the window's rectangle"
    );
}

/// The second window's client rectangle in the blessed picture: the dwindle
/// layout's, from the same settings the picture was built with.
fn second_window() -> compositor_layout::Rect {
    let mut state = compositor_layout::State::new(compositor_layout::Settings::default());
    let _ = state
        .add_monitor(compositor_layout::Monitor {
            scale: 1.0,
            transform: Default::default(),
            name: "Virtual-1".to_owned(),
            id: compositor_layout::MonitorId(1),
            rect: compositor_layout::Rect::new(0, 0, i64::from(WIDTH), i64::from(HEIGHT)),
            reserved: compositor_config::Gaps::all(0),
            description: String::new(),
            made: <(String, String, String)>::default(),
        })
        .expect("a monitor");
    let _ = state
        .open_window(compositor_layout::WindowId(1))
        .expect("a window");
    let _ = state
        .open_window(compositor_layout::WindowId(2))
        .expect("a window");
    state
        .layout()
        .iter()
        .flat_map(|output| output.windows.iter())
        .find(|placed| placed.window == compositor_layout::WindowId(2))
        .map(|placed| placed.rect)
        .expect("the second window is on the screen")
}

/// The two pattern windows, settled, and a screenshot of them taken over
/// the socket, with `lines` added to the configuration: the screenshot, the
/// last frame the screen itself showed, and what the compositor said.
fn screenshot_of(name: &str, lines: &str) -> (compositor_shot::Shot, Vec<u8>, String) {
    let work = workspace(name);
    let socket = work.join("wayland");
    let frames = work.join("frames");
    let config = undithered(&work);
    if !lines.is_empty() {
        let mut text = std::fs::read_to_string(&config).expect("the configuration");
        text.push_str(lines);
        std::fs::write(&config, text).expect("the configuration");
    }

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        dump: Some(frames.clone()),
        // Room for `settled` to wait out a busy host: it gives up long
        // before this, and a run that reaches the deadline has gone wrong
        // in some other way.
        deadline: Some(30_000),
        config: Some(config),
        ..Options::default()
    };
    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut windows = Vec::new();
        for (pattern, title) in [(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")] {
            let path = for_clients.clone();
            windows.push(std::thread::spawn(move || {
                connect(&path, pattern, title, Shape::Window)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        // Both windows are drawn by now, which is what the picture is of --
        // and *by now* is not a length of time. A window is listed and laid
        // out on its first commit, the one that carries no buffer, so the
        // screen holds it as border and background alone for a while before
        // its client paints. Sleeping through that gap works until the host
        // is busy, and then the screenshot is of one particular wrong
        // picture: both windows placed, the second unpainted.
        //
        // So wait for the screen to stop changing instead. Shots are taken
        // until several in a row are the same one, which is the frame both
        // clients have finished with, however long the host took to get
        // there. Nothing here compares against the expected image, so a
        // compositor that settles on the wrong picture still fails below
        // with the real difference.
        let taken = settled(&for_clients);
        for window in windows {
            let _ = window.join();
        }
        taken
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let taken = clients.join().expect("the clients finished");
    let taken =
        taken.unwrap_or_else(|why| panic!("the screenshot: {why}; the compositor said {line}"));

    assert_eq!(
        (taken.width, taken.height),
        (WIDTH, HEIGHT),
        "the screenshot is not the screen's size"
    );
    (taken, last_frame(&frames), line)
}

/// What is left after a window is closed from outside it.
///
/// The taskbar test says the *list* is right afterwards; this says the
/// *picture* is. They are different claims: the list comes from the layout
/// and the picture from whichever client's buffer the compositor reaches
/// for, and a compositor that held a window's client by its place in a list
/// it had just compacted would get the first right and the second wrong.
#[test]
fn the_window_left_after_a_close_is_drawn_from_its_own_buffer() {
    let work = workspace("closed");
    let socket = work.join("wayland");

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        deadline: Some(12000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut windows = Vec::new();
        for (pattern, title) in [(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")] {
            let path = for_clients.clone();
            windows.push(std::thread::spawn(move || {
                connect(&path, pattern, title, Shape::Window)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        let closed = compositor_lswt::run(
            &for_clients,
            &compositor_lswt::Want::Close("one".to_owned()),
        );
        // Long enough for the window that is left to be told its new size
        // and to draw at it.
        std::thread::sleep(Duration::from_millis(2500));
        let taken = compositor_shot::take(&for_clients, 0);
        for window in windows {
            let _ = window.join();
        }
        (closed, taken)
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let (closed, taken) = clients.join().expect("the clients finished");
    let _ = closed.unwrap_or_else(|why| panic!("closing: {why}"));
    let taken = taken.unwrap_or_else(|why| panic!("the screenshot: {why}"));

    let want = image(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../render/tests/data/one-client-alone.xrle"),
    );
    let differing = taken
        .pixels
        .chunks_exact(3)
        .zip(want.chunks_exact(3))
        .filter(|(shot, blessed)| shot != blessed)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} pixels of the window that was left are not the renderer's; the compositor \
         said {line}"
    );
}

/// One client, two windows, and the second one destroyed: the layout must
/// stop tiling the window that went.
///
/// The test beside this one closes a whole *connection* and is passed by a
/// compositor that notices nothing smaller. Until 2026-09-18 this tree was
/// exactly that compositor: only a client going took its windows out of the
/// layout, so a client that destroyed one of its two `xdg_toplevel`s left
/// the layout tiling a window that no longer existed, and the window that
/// remained kept half the screen.
///
/// Nothing else in this tree opens two windows from one client, which is
/// why the fix had no test. `Shape::Twin` is a client that does: it opens a
/// second window once the first has drawn, and destroys it once it has
/// drawn in its turn, with the connection open throughout.
///
/// Three claims, in order: the taskbar's list has both windows while they
/// are up; it has one after the destroy; and the picture is the one
/// `src/user/system/linux/compositor/render` blesses for a single window with the whole
/// workspace, which a compositor still tiling a ghost cannot produce.
#[test]
fn a_client_that_destroys_one_of_its_two_windows_leaves_the_other_alone() {
    let work = workspace("twin");
    let socket = work.join("wayland");

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        deadline: Some(16000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // One client, whose kept window is the gradient: that is the window
        // `src/user/system/linux/compositor/render` blesses alone, and the second window this
        // client opens draws the other pattern.
        let path = for_clients.clone();
        let client =
            std::thread::spawn(move || connect(&path, Pattern::Gradient, "two", Shape::Twin));

        let both = titles_until(&for_clients, 2);
        let left = titles_until(&for_clients, 1);
        // Long enough for the window that is left to be told its new size
        // and to draw at it, as the close test beside this one waits.
        std::thread::sleep(Duration::from_millis(2500));
        let taken = compositor_shot::take(&for_clients, 0);
        let _ = client.join();
        (both, left, taken)
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let (both, left, taken) = clients.join().expect("the clients finished");
    let taken = taken.unwrap_or_else(|why| panic!("the screenshot: {why}"));

    assert_eq!(
        both.len(),
        2,
        "both of the client's windows should have been in the list while they were up, not {both:?}"
    );
    assert!(
        both.iter().any(|title| title.contains("two second")),
        "the second window should have been in the list under its own title, not {both:?}"
    );
    assert_eq!(
        left.len(),
        1,
        "destroying one xdg_toplevel should leave one window in the list, not {left:?}"
    );
    assert!(
        left.first()
            .is_some_and(|title| title.contains("two") && !title.contains("second")),
        "the window left should be the one the client kept, not {left:?}"
    );

    let want = image(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../render/tests/data/one-client-alone.xrle"),
    );
    let differing = taken
        .pixels
        .chunks_exact(3)
        .zip(want.chunks_exact(3))
        .filter(|(shot, blessed)| shot != blessed)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} pixels are not those of the window that was kept, drawn alone; the \
         compositor said {line}"
    );
}

/// The windows a taskbar can see, once there are `count` of them.
///
/// Polled rather than slept for, because what this waits on is a client
/// making a window and the compositor telling a third program about it --
/// two round trips whose timing is the machine's, not the test's. Gives
/// what the list held on the last look, so a caller that never saw `count`
/// windows fails with the list it did see.
fn titles_until(socket: &Path, count: usize) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..100 {
        if let Ok(lines) = compositor_lswt::run(socket, &compositor_lswt::Want::List) {
            seen = lines;
            if seen.len() == count {
                return seen;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    seen
}

/// A locked screen shows the lock and nothing of what was under it.
///
/// `ext-session-lock-v1` is the one protocol whose whole point is that the
/// compositor stops drawing everything else, so the check is a picture: two
/// windows are tiled, a program takes the lock and draws a checkerboard over
/// the whole screen, and a screenshot of the *locked* screen must be the
/// image `src/user/system/linux/compositor/render` blesses for one -- not the windows, and not a
/// strip of them at any edge.
#[test]
fn a_locked_screen_shows_the_lock_and_none_of_the_windows() {
    let work = workspace("locked");
    let socket = work.join("wayland");

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        deadline: Some(14000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut windows = Vec::new();
        for (pattern, title) in [(Pattern::Checkerboard, "one"), (Pattern::Gradient, "two")] {
            let path = for_clients.clone();
            windows.push(std::thread::spawn(move || {
                connect(&path, pattern, title, Shape::Window)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        // The lock is taken in a thread of its own and held, so that the
        // screenshot below is taken while it is up.
        let locking = for_clients.clone();
        let lock =
            std::thread::spawn(move || compositor_lock::lock(&locking, Duration::from_secs(4)));
        std::thread::sleep(Duration::from_millis(2500));
        let taken = compositor_shot::take(&for_clients, 0);
        let locked = lock.join().unwrap_or_else(|_| Err("panicked".to_owned()));
        for window in windows {
            let _ = window.join();
        }
        (locked, taken)
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let (locked, taken) = clients.join().expect("the clients finished");
    let locked = locked.unwrap_or_else(|why| panic!("locking: {why}; the compositor said {line}"));
    assert_eq!(
        locked.screens, 1,
        "the lock covered {} screens",
        locked.screens
    );
    assert!(
        locked.told,
        "the compositor never said the screen was covered"
    );

    let taken = taken.unwrap_or_else(|why| panic!("the screenshot: {why}"));
    let want = image(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../render/tests/data/locked-screen.xrle"),
    );
    let differing = taken
        .pixels
        .chunks_exact(3)
        .zip(want.chunks_exact(3))
        .filter(|(shot, blessed)| shot != blessed)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} pixels of the locked screen are not the lock's; the compositor said {line}"
    );
}

/// A menu is drawn where `xdg_positioner` says, over the window it hangs
/// off.
///
/// Every right-click menu, dropdown and tooltip in every toolkit is an
/// `xdg_popup`, and a client that makes one and is never configured waits
/// for ever: the menu simply does not appear. What is required here is the
/// picture -- the popup over the window, at the rectangle the placement
/// rules put it, compared against the image `src/user/system/linux/compositor/render` blesses by
/// calling those same rules.
#[test]
fn a_menu_is_drawn_where_the_positioner_puts_it() {
    menu_is("menu", Shape::Menu(200), "", "menu-on-a-window");
}

/// `decoration:blur:popups`: a window's menu with what is behind it
/// blurred, the picture `src/user/system/linux/compositor/render` blesses for it. Hyprland's
/// `renderWindow` blurs a window's popups when the option is on; the
/// unblurred picture above is the control, and the two differ.
#[test]
fn a_menu_is_blurred_when_decoration_blur_popups_is_on() {
    menu_is(
        "blurred-menu",
        Shape::Menu(200),
        "decoration:blur:popups = 1\n",
        "blurred-menu-on-a-window",
    );
}

/// `layerrule = blur_popups`: a bar's menu, taken with
/// `zwlr_layer_surface_v1.get_popup`, with what is behind it blurred, as
/// Hyprland's `renderLayer` does for the popups of a surface whose rule
/// says so. The bar is the pattern client's, whose namespace is
/// `pattern-bar`; the windows tile under it.
#[test]
fn a_bars_menu_is_blurred_when_its_layerrule_says_blur_popups() {
    menu_is(
        "bar-menu",
        Shape::BarMenu(30, 200),
        "layerrule = blur_popups on, match:namespace ^(pattern-bar)$\n",
        "blurred-menu-on-a-bar",
    );
}

/// The screen with one client opening a menu, `shape`, and the two pattern
/// windows, required to be `src/user/system/linux/compositor/render`'s `golden` image, with
/// `lines` added to the configuration.
fn menu_is(name: &str, shape: Shape, lines: &str, golden: &str) {
    let work = workspace(name);
    let socket = work.join("wayland");
    let config = undithered(&work);
    if !lines.is_empty() {
        let mut text = std::fs::read_to_string(&config).expect("the configuration");
        text.push_str(lines);
        std::fs::write(&config, text).expect("the configuration");
    }

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        deadline: Some(12000),
        config: Some(config),
        ..Options::default()
    };

    // A bar is a third client, before the windows, so they tile under it.
    let mut clients = Vec::new();
    if let Shape::BarMenu(..) = shape {
        clients.push((Pattern::Checkerboard, "bar", shape));
        clients.push((Pattern::Checkerboard, "one", Shape::Window));
    } else {
        clients.push((Pattern::Checkerboard, "one", shape));
    }
    clients.push((Pattern::Gradient, "two", Shape::Window));

    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut windows = Vec::new();
        for (pattern, title, shape) in clients {
            let path = for_clients.clone();
            windows.push(std::thread::spawn(move || {
                connect(&path, pattern, title, shape)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
        // Long enough for the popup to be placed, configured and drawn.
        std::thread::sleep(Duration::from_millis(3000));
        let taken = compositor_shot::take(&for_clients, 0);
        for window in windows {
            let _ = window.join();
        }
        taken
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let taken = clients.join().expect("the clients finished");
    let taken =
        taken.unwrap_or_else(|why| panic!("the screenshot: {why}; the compositor said {line}"));

    let want = image(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../render/tests/data/{golden}.xrle")),
    );
    let differing = taken
        .pixels
        .chunks_exact(3)
        .zip(want.chunks_exact(3))
        .filter(|(shot, blessed)| shot != blessed)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} pixels of the menu's frame are not the renderer's {golden}; the compositor \
         said {line}"
    );
}

// ---------------------------------------------------------------------------
// Damage
//
// The compositor redraws the part of each screen that changed and leaves the
// rest of the canvas as the last frame left it. What shows that is a client
// that changes a little: `src/user/system/linux/compositor/pattern` damages the whole of its buffer
// every time it draws, so this brings a client of its own that repaints a
// square of a few hundred pixels and says which square.
//
// Two things have to hold at once, and the test is worth nothing without
// both: the frame that comes out is the right picture, and the damage the
// compositor handed the renderer was not the whole screen.
// ---------------------------------------------------------------------------

/// What the patching client is made of: the ids it gives its objects, the
/// square it repaints, and the two colours.
mod patch {
    use compositor_wire::ObjectId;

    pub(super) const DISPLAY: ObjectId = ObjectId(1);
    pub(super) const REGISTRY: ObjectId = ObjectId(2);
    pub(super) const SYNC: ObjectId = ObjectId(3);
    pub(super) const COMPOSITOR: ObjectId = ObjectId(4);
    pub(super) const SHM: ObjectId = ObjectId(5);
    pub(super) const SHELL: ObjectId = ObjectId(6);
    pub(super) const SURFACE: ObjectId = ObjectId(7);
    pub(super) const XDG_SURFACE: ObjectId = ObjectId(8);
    pub(super) const TOPLEVEL: ObjectId = ObjectId(9);
    pub(super) const POOL: ObjectId = ObjectId(10);
    pub(super) const BUFFER: ObjectId = ObjectId(11);

    /// The side of the square it repaints, and where it is in the buffer.
    /// Away from the corners, so no rounding cuts it.
    pub(super) const SIDE: i32 = 24;
    pub(super) const AT: (i32, i32) = (40, 40);

    /// The window's colour and the square's, as `XRGB8888`: blue, green,
    /// red, and one byte the compositor does not read. Neither is a colour
    /// the background, the borders or the other client's pattern has, so
    /// counting them in a frame counts this window's own pixels.
    pub(super) const BASE: [u8; 4] = [0x40, 0x40, 0x40, 0x00];
    pub(super) const MARK: [u8; 4] = [0x00, 0x00, 0xFF, 0x00];
}

use compositor_protocol::core::{
    WL_BUFFER, WL_CALLBACK, WL_DISPLAY, WL_REGISTRY, WL_SHM, WL_SHM_POOL, WL_SURFACE, wl_callback,
    wl_compositor, wl_display, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use compositor_protocol::xdg_shell::{self, xdg_surface, xdg_toplevel, xdg_wm_base};
use compositor_socket::{Connection, RecvError};
use compositor_wire::{Arg, ArgType, Fd, Interface, ObjectId, Reader, Writer};

/// What the client knows.
struct Patcher {
    globals: std::collections::BTreeMap<String, (u32, u32)>,
    size: (i32, i32),
    shared: Option<compositor_shm::Shared>,
    drawn: u32,
    marked: u32,
}

/// Queue one request. The only way this fails is a message longer than
/// the format allows, which none of these is.
fn send(
    out: &mut Writer,
    sender: ObjectId,
    opcode: u16,
    signature: &'static [ArgType],
    args: &[Arg<'_>],
) {
    let _ = out.write(sender, opcode, signature, args);
}

/// Which interface an object of this client's speaks, so an event for it
/// can be decoded.
fn interface_of(id: ObjectId) -> Option<&'static Interface> {
    Some(match id {
        patch::DISPLAY => &WL_DISPLAY,
        patch::REGISTRY => &WL_REGISTRY,
        patch::SYNC => &WL_CALLBACK,
        patch::SHM => &WL_SHM,
        patch::SURFACE => &WL_SURFACE,
        patch::XDG_SURFACE => &xdg_shell::XDG_SURFACE,
        patch::TOPLEVEL => &xdg_shell::XDG_TOPLEVEL,
        patch::SHELL => &xdg_shell::XDG_WM_BASE,
        patch::POOL => &WL_SHM_POOL,
        patch::BUFFER => &WL_BUFFER,
        _ => return None,
    })
}

impl Patcher {
    /// Bind what a window needs and ask for one.
    fn bind(&mut self, title: &str, out: &mut Writer) -> Result<(), String> {
        for (interface, id, version) in [
            ("wl_compositor", patch::COMPOSITOR, 6u32),
            ("wl_shm", patch::SHM, 1),
            ("xdg_wm_base", patch::SHELL, 6),
        ] {
            let (name, offered) = self
                .globals
                .get(interface)
                .copied()
                .ok_or_else(|| format!("the compositor offers no {interface}"))?;
            out.write(
                patch::REGISTRY,
                wl_registry::request::BIND,
                &[ArgType::Uint, ArgType::AnyNewId],
                &[
                    Arg::Uint(name),
                    Arg::AnyNewId {
                        interface,
                        version: version.min(offered),
                        id,
                    },
                ],
            )
            .map_err(|error| format!("binding {interface}: {error:?}"))?;
        }
        send(
            out,
            patch::COMPOSITOR,
            wl_compositor::request::CREATE_SURFACE,
            &[ArgType::NewId],
            &[Arg::NewId(patch::SURFACE)],
        );
        send(
            out,
            patch::SHELL,
            xdg_wm_base::request::GET_XDG_SURFACE,
            &[ArgType::NewId, ArgType::Object { nullable: false }],
            &[Arg::NewId(patch::XDG_SURFACE), Arg::Object(patch::SURFACE)],
        );
        send(
            out,
            patch::XDG_SURFACE,
            xdg_surface::request::GET_TOPLEVEL,
            &[ArgType::NewId],
            &[Arg::NewId(patch::TOPLEVEL)],
        );
        send(
            out,
            patch::TOPLEVEL,
            xdg_toplevel::request::SET_TITLE,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(title))],
        );
        send(
            out,
            patch::TOPLEVEL,
            xdg_toplevel::request::SET_APP_ID,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some("rocks.magical.patch"))],
        );
        // The first commit carries no buffer: it asks to be configured.
        send(out, patch::SURFACE, wl_surface::request::COMMIT, &[], &[]);
        Ok(())
    }

    /// Make the pool and the buffer once the size is known, fill the
    /// window with its colour, and commit the whole of it.
    fn draw(&mut self, out: &mut Writer) -> Result<(), String> {
        let (width, height) = self.size;
        if width <= 0 || height <= 0 || self.shared.is_some() {
            return Ok(());
        }
        let stride = width.saturating_mul(4);
        let len = usize::try_from(stride.saturating_mul(height))
            .map_err(|_| "a window too large to draw".to_owned())?;
        let mut shared =
            compositor_shm::Shared::new(len).map_err(|error| format!("memory: {error}"))?;
        for pixel in shared.bytes_mut().chunks_exact_mut(4) {
            pixel.copy_from_slice(&patch::BASE);
        }
        let fd = shared.as_raw_fd();
        self.shared = Some(shared);
        send(
            out,
            patch::SHM,
            wl_shm::request::CREATE_POOL,
            &[ArgType::NewId, ArgType::Fd, ArgType::Int],
            &[
                Arg::NewId(patch::POOL),
                Arg::Fd(Fd(fd)),
                Arg::Int(i32::try_from(len).unwrap_or(i32::MAX)),
            ],
        );
        send(
            out,
            patch::POOL,
            wl_shm_pool::request::CREATE_BUFFER,
            &[
                ArgType::NewId,
                ArgType::Int,
                ArgType::Int,
                ArgType::Int,
                ArgType::Int,
                ArgType::Uint,
            ],
            &[
                Arg::NewId(patch::BUFFER),
                Arg::Int(0),
                Arg::Int(width),
                Arg::Int(height),
                Arg::Int(stride),
                Arg::Uint(wl_shm::format::XRGB8888),
            ],
        );
        self.attach(out, (0, 0, width, height));
        Ok(())
    }

    /// Repaint the square and commit it, damaging nothing else.
    fn mark(&mut self, out: &mut Writer) {
        let stride = usize::try_from(self.size.0.saturating_mul(4)).unwrap_or(0);
        let (left, top) = patch::AT;
        let side = usize::try_from(patch::SIDE).unwrap_or(0);
        if let Some(shared) = self.shared.as_mut() {
            let bytes = shared.bytes_mut();
            for y in top..top.saturating_add(patch::SIDE) {
                let at = usize::try_from(y).unwrap_or(0) * stride
                    + usize::try_from(left).unwrap_or(0) * 4;
                let Some(row) = bytes.get_mut(at..at + side * 4) else {
                    continue;
                };
                for pixel in row.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&patch::MARK);
                }
            }
        }
        self.marked = self.marked.saturating_add(1);
        self.attach(out, (left, top, patch::SIDE, patch::SIDE));
    }

    /// Attach the buffer, say which part of it changed, and commit.
    fn attach(&mut self, out: &mut Writer, damage: (i32, i32, i32, i32)) {
        send(
            out,
            patch::SURFACE,
            wl_surface::request::ATTACH,
            &[
                ArgType::Object { nullable: true },
                ArgType::Int,
                ArgType::Int,
            ],
            &[Arg::Object(patch::BUFFER), Arg::Int(0), Arg::Int(0)],
        );
        send(
            out,
            patch::SURFACE,
            wl_surface::request::DAMAGE_BUFFER,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[
                Arg::Int(damage.0),
                Arg::Int(damage.1),
                Arg::Int(damage.2),
                Arg::Int(damage.3),
            ],
        );
        send(out, patch::SURFACE, wl_surface::request::COMMIT, &[], &[]);
        self.drawn = self.drawn.saturating_add(1);
    }

    /// Answer one event.
    fn event(
        &mut self,
        sender: ObjectId,
        opcode: u16,
        args: &[Arg<'_>],
        title: &str,
        out: &mut Writer,
    ) -> Result<(), String> {
        match sender {
            patch::DISPLAY if opcode == wl_display::event::ERROR => {
                let text = args.get(2).and_then(Arg::as_str).unwrap_or("");
                return Err(format!("the compositor refused this client: {text}"));
            }
            patch::REGISTRY if opcode == wl_registry::event::GLOBAL => {
                let (Some(name), Some(interface), Some(version)) = (
                    args.first().and_then(Arg::as_uint),
                    args.get(1).and_then(Arg::as_str),
                    args.get(2).and_then(Arg::as_uint),
                ) else {
                    return Ok(());
                };
                let _ = self.globals.insert(interface.to_owned(), (name, version));
            }
            patch::SYNC if opcode == wl_callback::event::DONE => {
                self.bind(title, out)?;
            }
            patch::SHELL if opcode == xdg_wm_base::event::PING => {
                let serial = args.first().and_then(Arg::as_uint).unwrap_or(0);
                send(
                    out,
                    patch::SHELL,
                    xdg_wm_base::request::PONG,
                    &[ArgType::Uint],
                    &[Arg::Uint(serial)],
                );
            }
            patch::TOPLEVEL if opcode == xdg_toplevel::event::CONFIGURE => {
                let (width, height) = (
                    args.first().and_then(Arg::as_int).unwrap_or(0),
                    args.get(1).and_then(Arg::as_int).unwrap_or(0),
                );
                // A zero is "you choose", which every client answers
                // with the size it would like.
                self.size = (
                    if width > 0 { width } else { 640 },
                    if height > 0 { height } else { 480 },
                );
            }
            patch::XDG_SURFACE if opcode == xdg_surface::event::CONFIGURE => {
                let serial = args.first().and_then(Arg::as_uint).unwrap_or(0);
                send(
                    out,
                    patch::XDG_SURFACE,
                    xdg_surface::request::ACK_CONFIGURE,
                    &[ArgType::Uint],
                    &[Arg::Uint(serial)],
                );
                self.draw(out)?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// A client that draws its window one colour and then repaints one small
/// square of it `marks` times, damaging only that square.
///
/// The requests are every toolkit's, in the order the protocol requires: a
/// surface, an `xdg_surface` over it, a toplevel, a commit with no buffer
/// that asks to be configured, and then a buffer attached and committed for
/// each frame. What is unusual is only the damage.
fn patching(socket: &Path, title: &str, marks: u32) -> String {
    let stream = std::os::unix::net::UnixStream::connect(socket)
        .unwrap_or_else(|error| panic!("connecting to {}: {error}", socket.display()));
    let mut connection = Connection::new(stream).expect("a connection");
    let mut out = Writer::new();
    send(
        &mut out,
        patch::DISPLAY,
        wl_display::request::GET_REGISTRY,
        &[ArgType::NewId],
        &[Arg::NewId(patch::REGISTRY)],
    );
    send(
        &mut out,
        patch::DISPLAY,
        wl_display::request::SYNC,
        &[ArgType::NewId],
        &[Arg::NewId(patch::SYNC)],
    );
    let mut state = Patcher {
        globals: std::collections::BTreeMap::new(),
        size: (0, 0),
        shared: None,
        drawn: 0,
        marked: 0,
    };
    let started = std::time::Instant::now();
    let mut last = started;
    while started.elapsed() < Duration::from_secs(30) {
        match connection.receive() {
            Ok(_) => {}
            Err(RecvError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
            Err(RecvError::Closed) => break,
            Err(error) => return format!("reading: {error:?}"),
        }
        let bytes = connection.bytes().to_vec();
        let fds = connection.fds();
        let mut reader = Reader::new(&bytes, &fds);
        while !reader.is_done() {
            let Ok(header) = reader.peek() else {
                break;
            };
            let Some(method) =
                interface_of(header.sender).and_then(|interface| interface.event(header.opcode))
            else {
                // An event for an object this client did not make, or one
                // its interface has not got: what follows it in the buffer
                // cannot be decoded either.
                break;
            };
            let Ok((_, read)) = reader.read(method.signature) else {
                break;
            };
            if let Err(why) = state.event(header.sender, header.opcode, &read, title, &mut out) {
                return why;
            }
        }
        let consumed = reader.consumed();
        if consumed > 0 {
            connection.consume(consumed, 0);
        }
        // One square every so often, which is what this client is for: far
        // enough apart that each of them is a frame of its own.
        if state.shared.is_some()
            && state.marked < marks
            && last.elapsed() > Duration::from_millis(200)
        {
            last = std::time::Instant::now();
            state.mark(&mut out);
        }
        if !out.is_empty() {
            let (queued, handed) = out.take();
            if connection.send(&queued, &handed).is_err() {
                break;
            }
        }
    }
    format!(
        "patch: {title} {}x{} drew {} marked {}",
        state.size.0, state.size.1, state.drawn, state.marked
    )
}

/// How many pixels the smallest frame of the run redrew, and how many the
/// screen holds, from the line the compositor says when it stops.
fn smallest(report: &str) -> Option<(i64, i64)> {
    let rest = report.split("smallest frame ").nth(1)?;
    let mut words = rest.split_whitespace();
    let least: i64 = words.next()?.parse().ok()?;
    let _of = words.next()?;
    let whole: i64 = words.next()?.parse().ok()?;
    Some((least, whole))
}

/// How many pixels of `frame` are exactly `colour`, which is given as the
/// `XRGB8888` bytes a client writes.
fn how_many(frame: &[u8], colour: [u8; 4]) -> usize {
    let want = [colour[2], colour[1], colour[0]];
    frame
        .chunks_exact(3)
        .filter(|pixel| *pixel == want.as_slice())
        .count()
}

/// A client that repaints a square of its window gets a frame with that
/// square in it, and the compositor redrew a fraction of the screen to
/// produce it.
///
/// Both halves matter. Without the first a compositor that damaged nothing
/// at all would pass; without the second one that redraws every pixel of
/// every frame would -- which is what this compositor did before the damage
/// was worked out.
#[test]
fn a_small_commit_redraws_a_small_part_of_the_screen() {
    let work = workspace("damage");
    let socket = work.join("wayland");
    let frames = work.join("frames");

    let options = Options {
        display: socket.to_string_lossy().into_owned(),
        headless: Some((WIDTH, HEIGHT)),
        dump: Some(frames.clone()),
        deadline: Some(8000),
        config: Some(undithered(&work)),
        ..Options::default()
    };

    let for_clients = socket.clone();
    let clients = std::thread::spawn(move || {
        for _ in 0..400 {
            if for_clients.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The pattern client first, so the dwindle tree is the one every
        // other test here builds: the first window takes the whole area
        // and the second splits it.
        let first = for_clients.clone();
        let one = std::thread::spawn(move || {
            connect(&first, Pattern::Checkerboard, "one", Shape::Window)
        });
        std::thread::sleep(Duration::from_millis(250));
        let second = for_clients.clone();
        let two = std::thread::spawn(move || patching(&second, "two", 12));
        let lines = [
            one.join().unwrap_or_else(|_| "one panicked".to_owned()),
            two.join().unwrap_or_else(|_| "two panicked".to_owned()),
        ];
        lines.join("; ")
    });

    let line = hyprix::run(&options).expect("the compositor ran");
    let said = clients.join().expect("the clients finished");
    let report = format!("{line} | {said}");
    let frame = last_frame(&frames);

    assert!(
        said.contains("marked 12"),
        "the client did not repaint its square: {report}"
    );
    // The picture: the square is in the frame, whole, and the window it is
    // in is still around it.
    let marked = how_many(&frame, patch::MARK);
    let side = (patch::SIDE * patch::SIDE) as usize;
    assert_eq!(
        marked, side,
        "the square is {marked} pixels of the frame rather than {side}: {report}"
    );
    let base = how_many(&frame, patch::BASE);
    assert!(
        base > side * 20,
        "only {base} pixels around the square are the window's own colour: {report}"
    );

    // And the damage: some frame of the run redrew a small part of the
    // screen. A compositor that asks for the whole screen every time
    // cannot pass this, and "small" is generous on purpose -- what is
    // tested is that the damage is a region at all, not how tight it is.
    let (least, whole) = smallest(&report).unwrap_or_else(|| panic!("no damage in {report}"));
    assert_eq!(whole, i64::from(WIDTH) * i64::from(HEIGHT));
    assert!(
        least < whole / 16,
        "the smallest frame of the run redrew {least} pixels of {whole}, which is the screen: \
         {report}"
    );
}

/// An instance directory whose `.socket.sock` just fits in `sun_path` and
/// whose `.socket2.sock` does not -- 107 bytes and 108 -- is refused whole,
/// as a request socket that does not fit is: a compositor that ran without
/// its event socket gave every bar no events and said so only in its own
/// report, which read like a test failing on a slow machine.
#[test]
fn an_event_socket_too_long_for_sun_path_is_refused_as_the_request_socket_is() {
    let base = PathBuf::from("/tmp").join(format!("hyprix-sunlen-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("a directory to work in");
    // `<base>/<padding>/.socket2.sock` at 108 bytes, one past what fits.
    let fixed = base.to_string_lossy().len() + "/".len() + "/.socket2.sock".len();
    let padding = "x".repeat(108 - fixed);
    let instance = base.join(padding);
    assert_eq!(
        instance
            .join(compositor_ipc::EVENT_SOCKET)
            .to_string_lossy()
            .len(),
        108
    );
    assert_eq!(
        instance
            .join(compositor_ipc::REQUEST_SOCKET)
            .to_string_lossy()
            .len(),
        107,
        "the request socket's path fits"
    );
    let options = Options {
        display: base.join("wayland").to_string_lossy().into_owned(),
        headless: Some((64, 48)),
        deadline: Some(200),
        instance: Some(instance.to_string_lossy().into_owned()),
        ..Options::default()
    };
    let said = hyprix::run(&options);
    let _ = std::fs::remove_dir_all(&base);
    let error = said.expect_err("a compositor whose event socket cannot be bound does not run");
    assert!(
        error.contains("hyprctl's event socket") && error.contains("SUN_LEN"),
        "{error}"
    );
}
