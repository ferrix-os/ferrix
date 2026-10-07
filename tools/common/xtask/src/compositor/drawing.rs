//! The boots about how the compositor draws: Hyprland's decorations, a
//! window's slide watched frame by frame against the frame-time bound, a
//! terminal's text, the frame drawn on the GPU (`--gl`), and `test-video`'s
//! wallpaper that moves.
//!
//! Each is held to a picture `src/user/system/linux/compositor/render` blesses, except the GPU's
//! and the video's: QEMU cannot dump a GL console, so the GPU's is judged by
//! the guest itself, and a video's frames are required to change rather than
//! to be one picture.

use std::path::Path;
use std::time::{Duration, Instant};

use super::boot::{
    Moving, Wanted, boot_and_dump, build_image, press, said_on_its_own, say_the_marker, undithered,
    with_the_transcript,
};
use super::desktop::MOVIE_PATH;
use super::picture::{differences, expected, unexpected};
use super::{CLIENT_PATH, Carried, EITHER, FAILED, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The picture Hyprland's two window decorations make, which the third boot
/// requires.
const DECORATED_EXPECTED: (&str, &str) = (
    "corners cut, a shadow under each window, and the unfocused one dimmed",
    "src/user/system/linux/compositor/render/tests/data/decorated-two-clients.xrle",
);

/// The configuration the third boot is given: the same two windows, with
/// `decoration:rounding` and `decoration:inactive_opacity` set.
const DECORATED_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
# The same settings `src/user/system/linux/compositor/render`'s `decorated_style` blesses the
# picture with; a line changed here and not there is a picture that cannot
# match.
decoration:rounding = 12
decoration:inactive_opacity = 0.6
decoration:shadow:range = 12
decoration:shadow:render_power = 2
decoration:dim_inactive = 1
decoration:dim_strength = 0.4
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
";

/// The picture the animated boot starts from: the same decorations the third
/// boot requires, since the slide is watched with them on.
const ANIMATED_EXPECTED: [(&str, &str); 1] = [(
    "corners cut, a shadow under each window, and the unfocused one dimmed",
    "src/user/system/linux/compositor/render/tests/data/decorated-two-clients.xrle",
)];

/// Where the slide ends: the same two windows, exchanged.
const ANIMATED_MOVING: Moving<'static> = Moving {
    what: "a window sliding to the other side with its decorations on",
    path: "src/user/system/linux/compositor/render/tests/data/decorated-two-clients-swapped.xrle",
    keys: &["meta_l", "a"],
};

/// How long a frame may take on the guest, in microseconds.
///
/// Not the bound the renderer's software fallback has -- that one is stated
/// and checked where it means something, by `src/user/system/linux/compositor/render`'s own
/// release-build test, at 250 ms for a frame with every effect on. This is
/// the same frame under QEMU's `tcg`, which emulates every instruction and
/// is tens of times slower than the processor it is emulating: the numbers
/// the guest reports are around 1.5 seconds a frame, and what this catches
/// is a compositor that stopped drawing or a frame that became minutes
/// rather than seconds.
///
/// Twice that on ARMv7-A, whose frames under `tcg` take about twice
/// AArch64's in the same runs (1.3 to 2 s against 0.6 to 0.8 s on a loaded
/// example, 2026-09-23), and whose slowest reached 5.25 s there with the
/// host's load average near 11: a bound the 64-bit machines keep a margin
/// under would fail it on load alone.
///
/// Ten times that under a TCG plugin (`FERRIX_QEMU_PLUGIN`), which slows the
/// guest again: the coverage plugin takes one process-wide lock for every
/// translated block it runs, and x86-64's slowest frame under it came to
/// 8.4, 10.0 and 19.1 s in three runs on 2026-09-26, where plain `tcg` stays
/// under the 5 s. What the bound is for -- a compositor that stopped drawing,
/// or frames of minutes -- still fails.
fn frame_bound(arch: Arch) -> u128 {
    let bound = match arch {
        Arch::Armv7a => 10_000_000,
        Arch::X86_64 | Arch::AArch64 => 5_000_000,
    };
    if std::env::var_os("FERRIX_QEMU_PLUGIN").is_some() {
        bound * 10
    } else {
        bound
    }
}

/// The configuration the eighth boot is given: the decorations of the third,
/// with the animations on and a keybind that sends a window to the other
/// side so that its slide can be watched.
///
/// Two seconds for the slide rather than Hyprland's 0.8, because what is
/// watched here is a sequence of screendumps and a screendump of a
/// virtio-gpu is not a fast thing: a longer slide is the same curve with
/// more points on it.
const ANIMATED_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
decoration:rounding = 12
decoration:inactive_opacity = 0.6
decoration:shadow:range = 12
decoration:shadow:render_power = 2
decoration:dim_inactive = 1
decoration:dim_strength = 0.4
animation = windows, 1, 20, default
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = SUPER, A, exec, /bin/hyprctl --batch dispatch movefocus l ; \
dispatch movewindow r
";

/// The picture a terminal makes, which the ninth boot requires.
const TERMINAL_EXPECTED: [(&str, &str); 1] = [(
    "a terminal with a program's output in it",
    "src/user/system/linux/compositor/render/tests/data/terminal-hyprctl-version.xrle",
)];

/// The configuration the ninth boot is given: a terminal, and nothing else.
///
/// `docs/ROADMAP.md` stage 18's exit asks for a terminal on the compositor.
/// The program it runs is `hyprctl version`, which prints three lines and
/// stops: short enough to be an expected image, and a round trip through
/// the control socket on the way, so what is on the screen came from the
/// compositor through a pseudoterminal and back.
const TERMINAL_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/term /bin/hyprctl version
";

/// `cargo xtask test-video`: boot a wallpaper that moves, and require the
/// screen to show its frames in turn.
///
/// The video is a four-frame AV1 test pattern checked into the repository,
/// so the gate needs no `ffmpeg` on the machine that runs it. What is being
/// tested is the whole path: the format, the client that decodes and plays
/// it, the layer surface it plays on, and the compositor drawing frame after
/// frame of it.
///
/// Nothing else is started, so the wallpaper is the whole screen.
///
/// # Errors
///
/// A guest whose screen never showed both frames.
pub(crate) fn test_video(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if crate::display::target(arch).is_none() {
            println!("  {arch}: no virtio-gpu in QEMU's machine; skipped");
            continue;
        }
        let programs = Programs::build(arch)?;
        video_boot(arch, &programs, args)?;
    }
    Ok(())
}

/// The boot `test_video` judges.
fn video_boot(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let size = args.size.unwrap_or(crate::wallpaper::SCREEN);
    let video = crate::wallpaper::fixture();
    println!(
        "  {arch}: a four-frame AV1 video, scaled to {}x{}, {} KiB",
        size.0,
        size.1,
        video.len() / 1024
    );
    let config = format!(
        "# Written into the initramfs by `cargo xtask test-video`.\n\
         monitor = , {}x{}@60, auto, 1\n\
         exec-once = /{CLIENT_PATH} --video /{MOVIE_PATH}\n",
        size.0, size.1
    );
    let mut carried = Carried::none();
    carried.ports.push(crate::ports::File {
        path: MOVIE_PATH.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(video),
    });
    let mut qemu_args = args.clone();
    qemu_args.size = Some(size);
    let (image, kernel) = build_image(arch, programs, &config, carried, &qemu_args)?;

    let port = free_port()?;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("video.ppm");
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let seen =
            both_frames(&mut qmp, &dump).map_err(|error| with_the_transcript(&error, watching))?;
        println!(
            "  {arch}: the screen showed two AV1 frames of the video, {seen} screendumps apart"
        );
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}

/// Take screendumps until the moving AV1 clip produced two distinct, non-flat
/// screens, and say how many dumps that took.
fn both_frames(qmp: &mut Qmp, dump: &Path) -> Result<usize> {
    let deadline = Instant::now() + SETTLE;
    let mut first: Option<Vec<u8>> = None;
    let mut distinct = false;
    let mut dumps = 0_usize;
    let mut last;
    loop {
        qmp.screendump(Some(DEVICE_ID), dump)?;
        let bytes = std::fs::read(dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        let screen = parse_ppm(&bytes)?;
        dumps = dumps.saturating_add(1);
        let pixels = screen.width.saturating_mul(screen.height);
        let varied = screen
            .pixels
            .chunks_exact(3)
            .any(|pixel| pixel != screen.pixels.get(..3).unwrap_or(&[]));
        last = format!(
            "{}x{}, {pixels} pixels, {}",
            screen.width,
            screen.height,
            if varied {
                "a non-flat frame"
            } else {
                "a flat frame"
            }
        );
        if varied && pixels > 0 {
            match &first {
                Some(previous) => distinct |= previous != &screen.pixels,
                None => first = Some(screen.pixels),
            }
        }
        if distinct {
            return Ok(dumps);
        }
        if Instant::now() >= deadline {
            return Err(Error::new(format!(
                "the screen never showed two AV1 frames of the video in {}s: {} dumps, the last {last}",
                SETTLE.as_secs(),
                dumps
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Where the GPU boot's expected image is on the guest.
const GPU_EXPECTED_PATH: &str = "etc/expected.xrle";

/// The configuration the GPU boot is given: the decorated pair -- corners,
/// shadows, an opacity and a dim, which between them are every shader but
/// the blur's -- and a key that holds a screenshot to the expected image.
///
/// The settings are `DECORATED_CONFIG`'s, for its reason: the picture is
/// `src/user/system/linux/compositor/render`'s `decorated_style`.
const GPU_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --gl`.
decoration:rounding = 12
decoration:inactive_opacity = 0.6
decoration:shadow:range = 12
decoration:shadow:render_power = 2
decoration:dim_inactive = 1
decoration:dim_strength = 0.4
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , S, exec, /bin/shot 0 /etc/expected.xrle
";

/// What the compositor says when a screen's frames are drawn on the GPU,
/// and what it says when they stop being.
/// `test-compositor --gl --boot dmabuf`'s configuration: the same two
/// windows, drawn into GPU buffers and handed over through
/// `zwp_linux_dmabuf_v1` (`docs/GPU.md` §3.13). The picture is the
/// `wl_shm` boot's, which is the claim: what the compositor shows of a
/// client's GPU buffer, sampled where it lies, is what the client drew.
const DMABUF_CONFIG: &str =
    "# Carried into the initramfs by `cargo xtask test-compositor --gl --boot dmabuf`.
decoration:rounding = 12
decoration:inactive_opacity = 0.6
decoration:shadow:range = 12
decoration:shadow:render_power = 2
decoration:dim_inactive = 1
decoration:dim_strength = 0.4
exec-once = /bin/pattern checkerboard one --dmabuf
exec-once = /bin/pattern gradient two --after one --dmabuf
bind = , S, exec, /bin/shot 0 /etc/expected.xrle
";

/// What the dmabuf boot must have said, beside the picture: each client
/// presented a dmabuf, hyprix imported one, and each of the client's own
/// checks of the render node's import passed (the certification
/// consultant's C3, 2026-10-07). A client that fell back to `wl_shm`, or a
/// node that imported anything, says none of these.
///
/// The import's own checks come first, so that a failure names the check
/// that failed rather than the presentation that never came after it.
const DMABUF_SAID: [&str; 7] = [
    "pattern: import: a pipe is no dmabuf (EINVAL)",
    "pattern: import: its own export is its own handle",
    "pattern: import: imported, closed and imported again on a second open",
    "pattern: import: the maker's buffer still takes pixels after the importer let go",
    "pattern: one presents through zwp_linux_dmabuf_v1",
    "pattern: two presents through zwp_linux_dmabuf_v1",
    "hyprix: imported a dmabuf",
];

/// What the dmabuf boot must not have said.
const DMABUF_REFUSED: &str = "hyprix: a dmabuf could not be imported";

const ON_THE_GPU: &str = "frames are drawn on the GPU";
const IN_SOFTWARE: &str = "drawing in software";

/// How long the GPU boot keeps asking for a screenshot that matches: the
/// clients have to connect, draw and be tiled first, and a picture taken
/// before they have is a true picture of something else.
const GPU_PATIENCE: Duration = Duration::from_secs(90);

/// `test-compositor --gl`: the compositor drawing on the GPU, in the guest.
///
/// The frame is drawn by `src/user/system/linux/compositor/render`'s GPU painter through
/// `/dev/dri/renderD128` -- the kernel's render node, the ring-3 driver,
/// virtio-gpu's 3D commands and the host's virglrenderer -- and what is
/// required is that it is the picture the software renderer blesses.
///
/// QEMU cannot be asked: `screendump` reads a surface and a GL console has
/// none. So the guest judges itself. `/bin/shot` takes a screenshot through
/// `zwlr_screencopy_v1`, reads the expected image off its own filesystem,
/// and says how many channels are more than a step from it; a GPU's frame
/// is the same picture and not the same bytes, which is why the verdict
/// crosses the serial port and a digest does not. Two more things are
/// required, because a screenshot that matches proves the picture and not
/// who drew it: the compositor must say it draws on the GPU, and must not
/// say it fell back.
pub(super) fn test_gpu(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let expected_image = std::fs::read(paths::workspace_root().join(DECORATED_EXPECTED.1))
        .map_err(|error| Error::new(format!("{}: {error}", DECORATED_EXPECTED.1)))?;
    let carried = Carried {
        ports: vec![crate::ports::File {
            path: GPU_EXPECTED_PATH.to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(expected_image),
        }],
        ..Carried::none()
    };
    // `--boot dmabuf` is the same boot with the windows presented as
    // dmabufs; no other boot has a GPU form.
    let (config, said_too): (&str, &[&str]) = match args.boot.as_deref() {
        None => (GPU_CONFIG, &[]),
        Some("dmabuf") => (DMABUF_CONFIG, &DMABUF_SAID),
        Some(other) => {
            return Err(Error::new(format!(
                "test-compositor --gl has one named boot, dmabuf; not {other}"
            )));
        }
    };
    let (image, kernel) = build_image(arch, programs, &undithered(config), carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let mut said: Vec<String> = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let up = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines
                .iter()
                .any(|line| line.contains(MARKER) || line.contains(FAILED))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: the compositor never printed `{MARKER}`")),
                watching,
            ));
        }
        say_the_marker(watching, arch);
        // Ask until the picture is the expected one. Each press is one more
        // `shot:` line; the last of them is what is judged.
        let deadline = Instant::now() + GPU_PATIENCE;
        loop {
            press(&mut qmp, &["s"])?;
            // `shot: ` begins its own line and ends the compositor's
            // `started /bin/shot`: only the first is an answer.
            let answers = |lines: &[String]| {
                lines
                    .iter()
                    .filter(|line| said_on_its_own(line).starts_with("shot: "))
                    .count()
            };
            // What `read_more` hands its closure is what was said after the
            // boot's first line, so that is what is counted before too.
            let before = answers(watching.after());
            let _ = watching.read_more(Instant::now() + Duration::from_secs(20), |lines| {
                answers(lines) > before
            })?;
            let matched = watching
                .lines()
                .iter()
                .chain(watching.after())
                .rev()
                .map(|line| said_on_its_own(line))
                .find(|line| line.starts_with("shot: "))
                .is_some_and(|line| line.contains(": 0 channels more than"));
            if matched || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = watching.read_more(Instant::now() + Duration::from_secs(2), |_| false)?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_gpu(arch, &said, said_too)
}

/// What [`test_gpu`] requires of what the guest said.
fn judge_gpu(arch: Arch, said: &[String], said_too: &[&str]) -> Result<()> {
    let transcript = || {
        said.iter()
            .map(|line| said_on_its_own(line).to_owned())
            .filter(|line| {
                line.starts_with("hyprix: ")
                    || line.starts_with("shot: ")
                    || line.starts_with("pattern: ")
            })
            .collect::<Vec<_>>()
            .join("\n    ")
    };
    if let Some(line) = said.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Err(Error::new(format!(
            "{arch}: the kernel stopped while the compositor ran: {}",
            line.trim()
        )));
    }
    for wanted in said_too {
        if !said.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!(
                "{arch}: the guest never said `{wanted}`:
    {}",
                transcript()
            )));
        }
    }
    if !said_too.is_empty()
        && let Some(line) = said.iter().find(|line| line.contains(DMABUF_REFUSED))
    {
        return Err(Error::new(format!(
            "{arch}: a client's dmabuf was refused: {}",
            line.trim()
        )));
    }
    if !said.iter().any(|line| line.contains(ON_THE_GPU)) {
        return Err(Error::new(format!(
            "{arch}: the compositor did not say it draws on the GPU:\n    {}",
            transcript()
        )));
    }
    if let Some(line) = said.iter().find(|line| line.contains(IN_SOFTWARE)) {
        return Err(Error::new(format!(
            "{arch}: the compositor gave the GPU up: {}",
            line.trim()
        )));
    }
    let Some(verdict) = said
        .iter()
        .rev()
        .map(|line| said_on_its_own(line))
        .find(|line| line.starts_with("shot: "))
    else {
        return Err(Error::new(format!(
            "{arch}: the guest took no screenshot:\n    {}",
            transcript()
        )));
    };
    if !verdict.contains(": 0 channels more than") {
        return Err(Error::new(format!(
            "{arch}: the GPU's frame is not the expected image: `{verdict}`\n    {}",
            transcript()
        )));
    }
    println!("  {arch}: the compositor draws on the GPU, and the guest's own screenshot says:");
    println!("  {arch}:   {verdict}");
    Ok(())
}

/// The third boot: Hyprland's two window decorations.
pub(super) fn test_decorations(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    one_picture(
        arch,
        programs,
        args,
        "rounded corners, a shadow, a dimmed window and a blurred background",
        DECORATED_CONFIG,
        DECORATED_EXPECTED,
    )
}

/// A boot with a configuration of its own, one picture and no keys.
///
/// Each is a boot rather than another picture in the first, because each
/// changes every picture and the first boot's three states are the stage's
/// exit criterion.
pub(super) fn one_picture(
    arch: Arch,
    programs: &Programs,
    args: &Args,
    what: &str,
    config: &str,
    wanted: (&str, &str),
) -> Result<()> {
    let (screens, _) = boot_and_dump(
        arch,
        programs,
        config,
        &Wanted {
            states: &[wanted],
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &[],
        },
        &[],
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!("{arch}: {what}: no screendump")));
    };
    println!(
        "  {arch}: {what}, every one of {} pixels",
        screen.width * screen.height
    );
    Ok(())
}

/// A ninth boot: a terminal, with a program running in it.
///
/// The whole path at once: the compositor starts the term app, which
/// opens `/dev/ptmx`, opens the slave, runs a program on it with the slave
/// for its session and its three descriptors, reads what it wrote back
/// through the master, draws it in a grid with its antialiased Hack, and
/// puts that in a `wl_shm` buffer the compositor composes into the frame.
/// Every pixel of that frame is compared against the one
/// the term app's own test blesses.
pub(super) fn test_terminal(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        TERMINAL_CONFIG,
        &Wanted {
            states: &TERMINAL_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &[],
        },
        &[],
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!(
            "{arch}: the terminal boot took no picture"
        )));
    };
    // The terminal said what it drew, which says the pseudoterminal carried
    // the program's output rather than the picture having come from
    // somewhere else.
    if !said.iter().any(|line| line.contains("term: ")) {
        return Err(Error::new(format!(
            "{arch}: the terminal never said anything"
        )));
    }
    println!(
        "  {arch}: a terminal ran a program on a pseudoterminal and drew its output, every one \
         of {} pixels",
        screen.width * screen.height
    );
    Ok(())
}

/// An eighth boot: a window sliding, watched frame by frame.
///
/// `docs/ROADMAP.md` stage 19's exit asks for a sequence of screendumps
/// showing a window moving along the configured curve with rounded corners
/// and blur behind a translucent client, inside the stated frame-time bound
/// under the software fallback. This is that: the decorated picture, a
/// keybind, every distinct picture until the windows have changed places,
/// and the compositor's own frame times from the same boot.
pub(super) fn test_animation(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        ANIMATED_CONFIG,
        &Wanted {
            states: &ANIMATED_EXPECTED,
            others: &[],
            moving: Some(ANIMATED_MOVING),
            pointer: None,
            awaiting: &[],
        },
        &[],
        args,
    )?;
    // The first is the state it started in; the rest are the slide, the
    // first of which is usually that same state, since a screendump asked
    // for the instant the keys go in is taken before the compositor has
    // drawn anything new.
    let sliding = screens.get(ANIMATED_EXPECTED.len()..).unwrap_or(&[]);
    let (Some(first), Some(last)) = (screens.first(), sliding.last()) else {
        return Err(Error::new(format!("{arch}: no pictures at all")));
    };
    // It arrived: the loop that took them stops at the goal or at the time,
    // and a run that stopped at the time is a window that never got there.
    let want = expected(ANIMATED_MOVING.path)?;
    let (found, count) = differences(last, &want);
    if count != 0 {
        return Err(unexpected(arch, ANIMATED_MOVING.what, last, found, count));
    }
    // And it went through somewhere else on the way: a compositor that drew
    // the goal at once would have the state it started from and the state it
    // ended in and nothing between them, however many dumps were taken.
    let between = sliding
        .iter()
        .filter(|screen| screen.pixels != first.pixels && screen.pixels != last.pixels)
        .count();
    if between == 0 {
        return Err(Error::new(format!(
            "{arch}: the window jumped: {} pictures, none of them between the two states",
            sliding.len()
        )));
    }
    println!(
        "  {arch}: a window slid through {} pictures with its decorations on, {between} of them \
         places neither layout put it, ending in the one the renderer blesses",
        sliding.len()
    );
    frames_were_inside_the_bound(arch, &said)
}

/// What the compositor said about its own frames, against the bound.
fn frames_were_inside_the_bound(arch: Arch, said: &[String]) -> Result<()> {
    let mut slowest = 0u128;
    for line in said {
        // `hyprix: frames <n> slowest of the last <m> <us> us`.
        let Some(rest) = line.split("slowest of the last ").nth(1) else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let (Some(_count), Some(number)) = (words.next(), words.next()) else {
            continue;
        };
        if let Ok(micros) = number.parse::<u128>() {
            slowest = slowest.max(micros);
        }
    }
    if slowest == 0 {
        return Err(Error::new(format!(
            "{arch}: the compositor never said how long its frames took"
        )));
    }
    let bound = frame_bound(arch);
    if slowest > bound {
        return Err(Error::new(format!(
            "{arch}: the slowest frame took {slowest} us, past the {bound} us a frame under \
             emulation is allowed"
        )));
    }
    println!(
        "  {arch}: the slowest frame the guest drew took {slowest} us, under emulation; the \
         renderer's own bound is checked in release by `src/user/system/linux/compositor/render`"
    );
    Ok(())
}
