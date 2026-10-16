//! `cargo xtask test-seat`: type into a window on Ferrix, from QEMU's far
//! end.
//!
//! `test-input` proves the path from QEMU's `input-send-event` to a program
//! reading `/dev/input/eventN`. `test-compositor` proves the path from a
//! Wayland client's buffer to the pixels on the card. This is the join:
//! QEMU's key goes into a `virtio-keyboard-pci`, out of the kernel's evdev
//! node, into the compositor's seat, across the Wayland socket as
//! `wl_keyboard.key`, and the client says which key it was and draws
//! something else, which the screen then shows.
//!
//! Four things are required, in order, because each is a different failure:
//!
//! 1. **The client has a keyboard and a keymap.** `wl_seat.capabilities` said
//!    the seat has one, and `wl_keyboard.keymap` handed over a real one --
//!    format 1, `XKB_V1`, with a length. `no_keymap` here would be a client
//!    that can never turn a keycode into a letter.
//! 2. **Focus reached it.** `wl_keyboard.enter`, which is the compositor
//!    saying this window is the one being typed into.
//! 3. **The key arrived, and the screen changed.** The line the client
//!    printed names the evdev code QMP sent, and the screendump taken after
//!    it differs from the one before: the client redrew because of the key,
//!    so the whole path is proven by a picture and not only by a log.
//! 4. **A keybind fired.** `bind = SUPER, Q, killactive` in the carried
//!    configuration, pressed through QMP, and the window closes -- which
//!    means the compositor matched the modifier, ate the key rather than
//!    sending it on, and ran a Hyprland dispatcher.
//!
//! The pointer is checked on the way: an absolute position and a click, and
//! the client reports `enter` and the button.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, mismatches, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-seat",
    help_at: 350,
    help: &[
        "test-seat     Boot the compositor with a client, type into it over QMP, and require the key and the keybind to land",
    ],
    run: |_, args| test_seat(args),
}];

/// What the compositor prints once it is on a screen.
const MARKER: &str = "hyprix: card0";

/// What it prints instead when it could not start.
const FAILED: &str = "hyprix: failed";

/// Either, so the boot stops at whichever comes.
const EITHER: &str = "hyprix: ";

/// The compositor's own background, `src/user/system/linux/compositor/render`'s `Style::BACKGROUND`.
const BACKGROUND: [u8; 3] = [0x11, 0x11, 0x11];

/// Where the client and the configuration go in the initramfs.
const CLIENT_PATH: &str = "bin/pattern";
const CONFIG_PATH: &str = "etc/hyprland.conf";

/// How wide and tall the cursor the compositor draws is.
///
/// `src/user/system/linux/compositor/render`'s `cursor` module: a 24x24 arrow whose tip is the
/// pointer, drawn down and to the right of it. A screen with the pointer on
/// it is the background plus this, and a check that wants the background
/// alone has to say where the cursor is allowed to be.
const CURSOR: usize = 24;

/// The configuration the compositor is given: one keybind, and a client.
///
/// `killactive` is the bind because its effect is visible from outside --
/// the window goes -- and because it is the one every Hyprland
/// configuration has.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-seat`.
bind = SUPER, Q, killactive
";

/// The evdev code of `a`, and of the left `Super`, from
/// `linux/input-event-codes.h`. What the client prints is the code the
/// compositor sent, which is evdev's.
const KEY_A: u32 = 30;
const BTN_LEFT: u32 = 272;

/// How long to wait for each thing, which crosses a virtqueue, a device node,
/// a socket and a page flip in an emulated machine.
const PATIENCE: Duration = Duration::from_secs(25);

/// How long to wait for the screen to show what was asked of it: a client
/// has to draw and commit, the compositor has to compose, and the flip is
/// queued after that.
///
/// A bound on a wait for the picture, not a pause: the screen is asked again
/// and again until it shows it, so a fast machine goes on at once and a slow
/// one is given what it takes. It used to be a pause of four seconds, then
/// one screendump, and under QEMU's coverage plugin a frame can take longer
/// than that on its own (`test-compositor` measured 19 s, 2026-09-26): the key
/// reached the client and the screendump was taken before its frame.
const SCREEN_PATIENCE: Duration = Duration::from_secs(90);

/// How often to ask for the screen while waiting for it.
const SCREEN_EVERY: Duration = Duration::from_millis(250);

/// The checkerboard's two greys, `src/user/system/linux/compositor/render`'s `Pattern::LIGHT` and
/// `Pattern::DARK`: what the client draws first.
const CHECKERBOARD: [[u8; 3]; 2] = [[0xE0, 0xE0, 0xE0], [0x30, 0x30, 0x30]];

/// More colours than this in the middle of the screen is the gradient, the
/// pattern the client draws after a key: it steps every sixteen pixels, so
/// the middle half of any screen this runs on holds dozens.
const GRADIENT_COLOURS: usize = 16;

/// Build one of the compositor's programs for `arch`, and say where it is.
fn build(arch: Arch, package: &str, binary: &str) -> Result<PathBuf> {
    let target = crate::display::target(arch).ok_or_else(|| {
        Error::new(format!(
            "{arch} has no virtio-gpu in QEMU; the seat test runs on x86_64 and aarch64"
        ))
    })?;
    let target_dir = paths::target_dir().join("compositor").join("seat");
    println!("  building src/user/system/linux/compositor/{binary} for {target}");
    let program = target_dir.join(target).join("release").join(binary);
    crate::builds::Build::cargo(
        format!("cargo build (src/user/system/linux/compositor/{binary}) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/compositor"),
    )
    .args(["build", "--release", "-p", package, "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program)
    .run()?;
    Ok(program)
}

/// What one boot of the compositor saw.
struct Seen {
    /// Every line the guest printed.
    lines: Vec<String>,
    /// The screen before the key, after it, and after the keybind.
    screens: Vec<Image>,
}

/// Wait for `wanted` among the lines the guest has printed, or the ones it
/// prints next.
///
/// The lines before the marker are searched too: the boot stops at the first
/// `hyprix:` line, which is the seat's, so the lines this test waits for
/// mostly come afterwards -- but not all of them do, and a test that missed
/// one that had already arrived would wait for a line that will never come
/// again.
fn wait_for(watching: &mut Watching<'_>, wanted: &str, arch: Arch) -> Result<()> {
    if watching.lines().iter().any(|line| line.contains(wanted)) {
        return Ok(());
    }
    let found = watching.read_more(Instant::now() + PATIENCE, |lines| {
        lines.iter().any(|line| line.contains(wanted))
    })?;
    if found {
        return Ok(());
    }
    Err(Error::new(format!(
        "{arch}: `{wanted}` was never printed, within {}s of it being asked for",
        PATIENCE.as_secs()
    )))
}

/// A screendump, parsed.
fn screen(qmp: &mut Qmp, dump: &Path) -> Result<Image> {
    qmp.screendump(Some(DEVICE_ID), dump)?;
    let bytes = std::fs::read(dump)
        .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
    parse_ppm(&bytes)
}

/// What the client is showing, read from the middle of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Showing {
    /// Its first pattern, and nothing else, in the middle of the screen.
    Checkerboard,
    /// The pattern a key turns it into.
    Gradient,
    /// Neither: the background, a window part-drawn, or something else.
    Other,
}

/// Where the cursor's tip is: the test puts the pointer in the middle of the
/// screen, and the compositor draws a [`CURSOR`]-square arrow down and to the
/// right of it.
fn tip(screen: &Image) -> (usize, usize) {
    (screen.width / 2, screen.height / 2)
}

/// What the middle half of `screen` shows, leaving out the cursor and a
/// pixel's margin round it. The only window is tiled over the whole screen,
/// so its middle is the client's and nothing else's.
fn showing(screen: &Image) -> Showing {
    let tip = tip(screen);
    let cursor = |x: usize, y: usize| {
        (tip.0.saturating_sub(2)..tip.0 + CURSOR + 2).contains(&x)
            && (tip.1.saturating_sub(2)..tip.1 + CURSOR + 2).contains(&y)
    };
    let (columns, rows) = (
        screen.width / 4..screen.width * 3 / 4,
        screen.height / 4..screen.height * 3 / 4,
    );
    let mut colours = std::collections::BTreeSet::new();
    for (index, pixel) in screen.pixels.chunks_exact(3).enumerate() {
        let (x, y) = (index % screen.width.max(1), index / screen.width.max(1));
        if !columns.contains(&x) || !rows.contains(&y) || cursor(x, y) {
            continue;
        }
        if let [red, green, blue] = *pixel {
            let _ = colours.insert([red, green, blue]);
        }
        if colours.len() > GRADIENT_COLOURS {
            return Showing::Gradient;
        }
    }
    if colours.len() == CHECKERBOARD.len() && CHECKERBOARD.iter().all(|c| colours.contains(c)) {
        Showing::Checkerboard
    } else {
        Showing::Other
    }
}

/// Up to four pixels of `screen` that are neither the compositor's
/// background nor the cursor: what a screen the keybind cleared has none of.
fn stray(screen: &Image) -> (Vec<(usize, usize, [u8; 3])>, usize) {
    let tip = tip(screen);
    let (found, wrong) = mismatches(screen, BACKGROUND, usize::MAX);
    let stray = found
        .iter()
        .filter(|(x, y, _)| x.saturating_sub(tip.0) >= CURSOR || y.saturating_sub(tip.1) >= CURSOR)
        .take(4)
        .copied()
        .collect::<Vec<_>>();
    (stray, wrong)
}

/// Take screendumps until one passes `wanted`, or [`SCREEN_PATIENCE`] is up,
/// and give back the last with whether it passed.
fn screen_until(
    qmp: &mut Qmp,
    dump: &Path,
    wanted: impl Fn(&Image) -> bool,
) -> Result<(Image, bool)> {
    let deadline = Instant::now() + SCREEN_PATIENCE;
    loop {
        let shown = screen(qmp, dump)?;
        if wanted(&shown) {
            return Ok((shown, true));
        }
        if Instant::now() >= deadline {
            return Ok((shown, false));
        }
        std::thread::sleep(SCREEN_EVERY);
    }
}

/// Boot the compositor with a client and a keybind, and type into it.
fn boot_and_type(arch: Arch, program: &Path, client: &Path, args: &Args) -> Result<Seen> {
    let loader = crate::cargo::build_loader(arch, args.release)?;
    // One argument a line: `Options::unshell` says why.
    let script = format!("--config\n/{CONFIG_PATH}\n--exec\n/{CLIENT_PATH} checkerboard one");
    let kernel = crate::cargo::build_kernel_with_init(arch, args.release, program, &script)?;
    let natives = crate::native::build(arch, args.release)?;
    let bytes = std::fs::read(client)
        .map_err(|error| Error::new(format!("reading {}: {error}", client.display())))?;
    let carried = [
        crate::ports::File {
            path: CLIENT_PATH.to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(bytes),
        },
        crate::ports::File {
            path: CONFIG_PATH.to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(CONFIG.as_bytes().to_vec()),
        },
    ];
    let initramfs = crate::initramfs::build(None, &natives, None, &carried)?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;

    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("seat.ppm");

    let mut seen = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        if let Some(line) = watching
            .lines()
            .iter()
            .rev()
            .find(|line| line.contains(FAILED))
        {
            return Err(Error::new(format!("{arch}: {}", line.trim())));
        }
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;

        // The boot stops at the first `hyprix:` line, which is the seat
        // saying what it opened; the screen is up a little later.
        wait_for(watching, MARKER, arch)?;

        // 1 and 2: the client has a keymap, and focus reached it.
        wait_for(watching, "pattern: keymap format 1", arch)?;
        wait_for(watching, "pattern: keyboard enter", arch)?;
        // The first frame, once the client has drawn into it and it is on
        // the card.
        let (before, drawn) = screen_until(&mut qmp, &dump, |shown| {
            showing(shown) == Showing::Checkerboard
        })?;
        if !drawn {
            return Err(Error::new(format!(
                "{arch}: the client's checkerboard never reached the screen within {}s",
                SCREEN_PATIENCE.as_secs()
            )));
        }

        // The pointer, on the way past: somewhere inside the only window,
        // then a click.
        qmp.input_send_event(&[absolute("x", 0x4000), absolute("y", 0x4000)])?;
        wait_for(watching, "pattern: pointer enter", arch)?;
        qmp.input_send_event(&[button("left", true)])?;
        wait_for(
            watching,
            &format!("pattern: button {BTN_LEFT} state 1"),
            arch,
        )?;
        qmp.input_send_event(&[button("left", false)])?;

        // 3: the key, and the screen it changed.
        qmp.input_send_event(&[key("a", true)])?;
        wait_for(watching, &format!("pattern: key {KEY_A} state 1"), arch)?;
        qmp.input_send_event(&[key("a", false)])?;
        // The client draws its other pattern on a key: wait until that is
        // what the card shows. A screen that never shows it is judged below,
        // as the picture it stopped at.
        let (after, _) =
            screen_until(&mut qmp, &dump, |shown| showing(shown) == Showing::Gradient)?;

        // 4: the keybind, and the window it closed.
        qmp.input_send_event(&[key("meta_l", true)])?;
        qmp.input_send_event(&[key("q", true)])?;
        // Which window, and not merely that one was asked to close: the
        // client says so -- `the compositor asked the Checkerboard window
        // called one to close` -- because a keybind that closed the wrong
        // window would otherwise pass this.
        wait_for(
            watching,
            "the compositor asked the Checkerboard window called one to close",
            arch,
        )?;
        qmp.input_send_event(&[key("q", false)])?;
        qmp.input_send_event(&[key("meta_l", false)])?;
        let (closed, _) = screen_until(&mut qmp, &dump, |shown| stray(shown).0.is_empty())?;

        seen = Some(Seen {
            lines: watching
                .lines()
                .iter()
                .chain(watching.after())
                .cloned()
                .collect(),
            screens: vec![before, after, closed],
        });
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    seen.ok_or_else(|| {
        Error::new(format!(
            "{arch}: the compositor never printed `{MARKER}` within {}s",
            args.timeout
        ))
    })
}

/// One `InputEvent` of QMP's `input-send-event`, as its JSON.
fn key(name: &str, down: bool) -> String {
    format!(
        "{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":\
         {{\"type\":\"qcode\",\"data\":{}}}}}}}",
        crate::display::json_string(name)
    )
}

fn button(name: &str, down: bool) -> String {
    format!(
        "{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":{}}}}}",
        crate::display::json_string(name)
    )
}

fn absolute(axis: &str, value: i32) -> String {
    format!(
        "{{\"type\":\"abs\",\"data\":{{\"axis\":{},\"value\":{value}}}}}",
        crate::display::json_string(axis)
    )
}

/// How many pixels of `left` and `right` differ.
///
/// A screen that changed size is not comparable at all, which is
/// [`usize::MAX`]: the caller only asks whether anything changed, and a
/// different mode is not the answer it wanted.
fn differences(left: &Image, right: &Image) -> usize {
    if left.width != right.width || left.height != right.height {
        return usize::MAX;
    }
    left.pixels
        .chunks(3)
        .zip(right.pixels.chunks(3))
        .filter(|(one, other)| one != other)
        .count()
}

/// `test-seat` on each architecture asked for that has a screen.
///
/// # Errors
///
/// A key that never reached a window, a screen that did not change when it
/// did, or a keybind that did not close the window it was bound to close.
pub(crate) fn test_seat(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if crate::display::target(arch).is_none() {
            println!("  {arch}: no virtio-gpu in QEMU's machine; skipped");
            continue;
        }
        let program = build(arch, "hyprix", "hyprix")?;
        let client = build(arch, "compositor-pattern", "pattern")?;
        let seen = boot_and_type(arch, &program, &client, args)?;

        let has = |wanted: &str| seen.lines.iter().any(|line| line.contains(wanted));
        // The seat announced a keyboard and a pointer, which is 3.
        if !has("pattern: seat capabilities 0x3") {
            return Err(Error::new(format!(
                "{arch}: the seat did not announce both a keyboard and a pointer"
            )));
        }
        // A keymap of a real length: `no_keymap` is format 0 and size 0, and
        // a client given that can never name a key.
        if has("pattern: keymap format 1 size 0") {
            return Err(Error::new(format!(
                "{arch}: the keymap was sent with a length of zero"
            )));
        }

        let [before, after, closed] = seen.screens.as_slice() else {
            return Err(Error::new(format!("{arch}: not three screendumps")));
        };
        let pixels = before.width * before.height;
        let changed = differences(before, after);
        if changed == 0 {
            return Err(Error::new(format!(
                "{arch}: the key reached the client and the screen did not change within {}s; \
                 the client redraws on a key, so nothing it drew reached the card",
                SCREEN_PATIENCE.as_secs()
            )));
        }
        if showing(after) != Showing::Gradient {
            return Err(Error::new(format!(
                "{arch}: the key reached the client and the screen changed, but never to the \
                 gradient the client redraws to on a key within {}s",
                SCREEN_PATIENCE.as_secs()
            )));
        }
        println!(
            "  {arch}: a key from QEMU reached the window and redrew {changed} of {pixels} pixels"
        );

        // The keybind closed the window, so the screen is the compositor's
        // background -- everywhere but the cursor. The pointer was put in
        // the middle of the screen a few steps up, and the compositor draws
        // a 24x24 arrow whose tip is the pointer (`src/user/system/linux/compositor/render`'s
        // `cursor`), down and to the right of it. That is what is left, and
        // a check that asked for the background *everywhere* was written
        // before the cursor was drawn at all.
        let tip = tip(closed);
        let (stray, wrong) = stray(closed);
        if !stray.is_empty() {
            return Err(Error::new(format!(
                "{arch}: `SUPER, Q, killactive` did not clear the screen; {} of {pixels} \
                 pixels are not the background or the cursor at {tip:?}, the first: {stray:?}",
                stray.len()
            )));
        }
        println!(
            "  {arch}: `SUPER, Q, killactive` closed the window; {wrong} pixels of cursor left at {tip:?}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BACKGROUND, CHECKERBOARD, CONFIG, Showing, absolute, button, differences, key, showing,
        stray,
    };
    use crate::display::Image;

    /// A 64x64 screen whose every pixel is `paint(x, y)`.
    fn painted(paint: impl Fn(usize, usize) -> [u8; 3]) -> Image {
        let (width, height) = (64, 64);
        let pixels = (0..width * height)
            .flat_map(|index| paint(index % width, index / width))
            .collect();
        Image {
            width,
            height,
            pixels,
        }
    }

    /// The cursor's arrow, down and to the right of the middle.
    fn with_cursor(x: usize, y: usize, under: [u8; 3]) -> [u8; 3] {
        if (32..40).contains(&x) && (32..40).contains(&y) {
            [0xFF, 0xFF, 0xFF]
        } else {
            under
        }
    }

    #[test]
    fn the_middle_of_the_screen_says_which_pattern_the_client_shows() {
        let [light, dark] = CHECKERBOARD;
        let checkerboard = |x: usize, y: usize| {
            if (x / 16 + y / 16).is_multiple_of(2) {
                light
            } else {
                dark
            }
        };
        let gradient = |x: usize, y: usize| [(x / 2 * 8) as u8, (y / 16 * 8) as u8, 0xC0];
        assert_eq!(showing(&painted(checkerboard)), Showing::Checkerboard);
        // The cursor over it is left out.
        assert_eq!(
            showing(&painted(|x, y| with_cursor(x, y, checkerboard(x, y)))),
            Showing::Checkerboard
        );
        assert_eq!(showing(&painted(gradient)), Showing::Gradient);
        // The background alone, or one grey alone, is neither: a window not
        // drawn yet is not the checkerboard.
        assert_eq!(showing(&painted(|_, _| BACKGROUND)), Showing::Other);
        assert_eq!(showing(&painted(|_, _| light)), Showing::Other);
    }

    #[test]
    fn a_cleared_screen_is_the_background_but_the_cursor() {
        let cleared = painted(|x, y| with_cursor(x, y, BACKGROUND));
        let (found, wrong) = stray(&cleared);
        assert!(found.is_empty());
        assert_eq!(wrong, 64);
        let [light, _] = CHECKERBOARD;
        let left = painted(|x, y| {
            if x < 8 {
                light
            } else {
                with_cursor(x, y, BACKGROUND)
            }
        });
        assert_eq!(stray(&left).0.len(), 4);
    }

    #[test]
    fn an_event_is_the_json_qmp_takes() {
        assert_eq!(
            key("meta_l", true),
            "{\"type\":\"key\",\"data\":{\"down\":true,\"key\":{\"type\":\"qcode\",\"data\":\"meta_l\"}}}"
        );
        assert_eq!(
            button("left", true),
            "{\"type\":\"btn\",\"data\":{\"down\":true,\"button\":\"left\"}}"
        );
        assert_eq!(
            absolute("y", 16384),
            "{\"type\":\"abs\",\"data\":{\"axis\":\"y\",\"value\":16384}}"
        );
    }

    /// The bind the test presses has to be in the configuration it carries,
    /// or the test would pass on a compositor that closed the window for
    /// another reason.
    #[test]
    fn the_carried_configuration_holds_the_bind_the_test_presses() {
        assert!(CONFIG.contains("bind = SUPER, Q, killactive"));
    }

    #[test]
    fn two_screens_differ_by_their_pixels_and_another_mode_is_not_comparable() {
        let image = |width: usize, height: usize, fill: u8| Image {
            width,
            height,
            pixels: vec![fill; width * height * 3],
        };
        let plain = image(2, 2, 0);
        assert_eq!(differences(&plain, &plain), 0);
        assert_eq!(differences(&plain, &image(2, 2, 1)), 4);
        let mut one = image(2, 2, 0);
        // One byte of one pixel is one pixel, not three.
        if let Some(byte) = one.pixels.get_mut(1) {
            *byte = 9;
        }
        assert_eq!(differences(&plain, &one), 1);
        assert_eq!(differences(&plain, &image(3, 2, 0)), usize::MAX);
    }
}
