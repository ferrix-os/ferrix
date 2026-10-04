//! The boots held to a picture this machine draws rather than one the tree
//! blesses: `caption`, a line in the user's own font, and `waybar` and
//! `waybar-volume`, the user's own bar.
//!
//! Nothing about the user's files is committed, so the expected picture is
//! made while the boot is built, by the `x86_64` build of the same client run
//! here with `--render` from the same files the guest is given, and compared
//! where the client puts its surface.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, button_event, undithered, with_the_transcript};
use super::{Carried, MARKER, Programs, SETTLE, build};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// What the `caption` boot draws: a clock's digits, letters with kerning
/// pairs, and a character (`…`) the lock screen's font may take from
/// another face.
const CAPTION_TEXT: &str = "Ferrix 12:34 — AVATAR To…";

/// The size it draws at, in points.
const CAPTION_POINTS: &str = "32";

/// `cargo xtask test-compositor`'s `caption` boot: a client built on the
/// desktop clients' foundation (`src/user/system/linux/compositor/caption`, on `src/user/system/linux/compositor/toolkit`
/// and `src/user/system/linux/compositor/text`) draws a line in the user's own font on a layer
/// surface, and the screen must show exactly the pixels the same program
/// draws on this machine with the same font files.
///
/// The font is the one `~/.config/hypr/hyprlock.conf` names first, resolved
/// on this machine with fontconfig and carried in as `run-compositor` carries
/// it (`crate::dotfiles`); a machine without that file or that font draws in
/// its own `sans-serif`, and says so. Nothing about the font is committed:
/// the expected picture is made now, by running the `x86_64` build of the
/// client here with `--render`, from the same files the guest is given
/// (`--fonts-dir` on both sides, so neither can reach a face the other
/// lacks). On `x86_64` the two must agree exactly; on another architecture a
/// channel may differ by one or two, which is SIMD rounding and not a
/// different drawing.
pub(super) fn test_caption(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let family = crate::dotfiles::users_font().unwrap_or_else(|| {
        println!("  {arch}: no font in ~/.config/hypr/hyprlock.conf; drawing in sans-serif");
        "sans-serif".to_owned()
    });
    let fonts = crate::dotfiles::font_files(std::slice::from_ref(&family));
    if fonts.is_empty() {
        return Err(Error::new(format!(
            "{arch}: this machine resolves no font file for `{family}`"
        )));
    }
    let (want, dir) = caption_expected(arch, &family, &fonts)?;
    let guest = build(arch, "compositor-caption", "caption")?;
    let mut carried = Carried::none();
    carried.ports.push(crate::ports::File {
        path: "bin/caption".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(
            std::fs::read(&guest)
                .map_err(|error| Error::new(format!("reading {}: {error}", guest.display())))?,
        ),
    });
    carried.ports.extend(fonts);
    let config = format!(
        "# Written into the initramfs by `cargo xtask test-compositor`'s caption boot.\n\
         monitor = , 1024x768@60, auto, 1\n\
         exec-once = /bin/caption --font '{family}' --size {CAPTION_POINTS} \
         --fonts-dir /{} {CAPTION_TEXT}\n",
        crate::dotfiles::FONT_DIR
    );
    let (image, kernel) = build_image(arch, programs, &undithered(&config), carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("caption.ppm");
    let tolerance = if arch == Arch::X86_64 { 0 } else { 2 };
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let drawn = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|line| line.contains("caption: drawn"))
        })?;
        if !drawn {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: the caption never said it was drawn")),
                watching,
            ));
        }
        let deadline = Instant::now() + SETTLE;
        loop {
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            let bytes = std::fs::read(&dump)
                .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
            let screen = parse_ppm(&bytes)?;
            let (differing, first) = caption_differences(&screen, &want, tolerance);
            if differing == 0 {
                println!(
                    "  {arch}: the caption's {}x{} pixels are the ones this machine draws",
                    want.width, want.height
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(with_the_transcript(
                    &Error::new(format!(
                        "{arch}: {differing} of the caption's pixels differ from this machine's \
                         drawing; the first at {first:?}; the screen is {}",
                        dump.display()
                    )),
                    watching,
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The picture the caption boot's screen must show: the `x86_64` build of
/// the client run here with `--render`, from `fonts` written to a directory
/// of their own. Gives the picture and the directory, for the caller to
/// remove.
fn caption_expected(
    arch: Arch,
    family: &str,
    fonts: &[crate::ports::File],
) -> Result<(Image, PathBuf)> {
    let dir = paths::build_dir(arch).join("caption-fonts");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;
    for file in fonts {
        if let (crate::ports::Content::Bytes(bytes), Some(name)) =
            (&file.content, file.path.rsplit('/').next())
        {
            std::fs::write(dir.join(name), bytes)
                .map_err(|error| Error::new(format!("writing a font: {error}")))?;
        }
    }
    let expected_path = paths::build_dir(arch).join("caption-expected.ppm");
    let host = build(Arch::X86_64, "compositor-caption", "caption")?;
    let rendered = std::process::Command::new(&host)
        .arg("--font")
        .arg(family)
        .args(["--size", CAPTION_POINTS, "--fonts-dir"])
        .arg(&dir)
        .arg("--render")
        .arg(&expected_path)
        .arg(CAPTION_TEXT)
        .output()
        .map_err(|error| Error::new(format!("running {}: {error}", host.display())))?;
    let said_here = String::from_utf8_lossy(&rendered.stdout).trim().to_owned();
    if !rendered.status.success() {
        return Err(Error::new(format!(
            "{arch}: the host's caption failed: {said_here}"
        )));
    }
    println!("  {arch}: here, {said_here}");
    let bytes = std::fs::read(&expected_path)
        .map_err(|error| Error::new(format!("reading {}: {error}", expected_path.display())))?;
    Ok((parse_ppm(&bytes)?, dir))
}

/// A pixel the caption boot found wrong: where, what was wanted, what was
/// shown.
type Differing = (usize, usize, [u8; 3], [u8; 3]);

/// Three bytes of a pixel, black where there is none.
fn triple(bytes: Option<&[u8]>) -> [u8; 3] {
    let bytes = bytes.unwrap_or(&[0, 0, 0]);
    [
        bytes.first().copied().unwrap_or(0),
        bytes.get(1).copied().unwrap_or(0),
        bytes.get(2).copied().unwrap_or(0),
    ]
}

/// How many of `want`'s pixels the screen at the caption's place does not
/// show, each channel allowed `tolerance`, and where the first is.
fn caption_differences(screen: &Image, want: &Image, tolerance: u8) -> (usize, Option<Differing>) {
    let origin = usize::try_from(CAPTION_MARGIN).unwrap_or(0);
    let mut count = 0;
    let mut first = None;
    for y in 0..want.height {
        for x in 0..want.width {
            let wanted = want
                .pixels
                .get((y * want.width + x) * 3..(y * want.width + x) * 3 + 3);
            let (sx, sy) = (x + origin, y + origin);
            let shown = (sx < screen.width && sy < screen.height)
                .then(|| {
                    screen
                        .pixels
                        .get((sy * screen.width + sx) * 3..(sy * screen.width + sx) * 3 + 3)
                })
                .flatten();
            let same = match (wanted, shown) {
                (Some(wanted), Some(shown)) => wanted
                    .iter()
                    .zip(shown)
                    .all(|(one, two)| one.abs_diff(*two) <= tolerance),
                _ => false,
            };
            if !same {
                count += 1;
                if first.is_none() {
                    first = Some((x, y, triple(wanted), triple(shown)));
                }
            }
        }
    }
    (count, first)
}

/// A module of the host's drawing, by config name, and its box:
/// `[x, y, width, height]`.
type WaybarModule = (String, [f32; 4]);

/// What both waybar boots carry and hold the screen to: the files, and the
/// picture the host draws from them.
struct WaybarBoot {
    /// The picture the top of the screen must show.
    want: Image,
    /// Where it was written.
    expected_path: PathBuf,
    /// Each module's box in it, by config name: `(x, y, width, height)`.
    modules: Vec<WaybarModule>,
    /// Whether the stylesheet is the user's.
    users: bool,
    /// waybar, its files and fonts, and zinc for its `/bin/sh`.
    carried: Carried,
}

impl WaybarBoot {
    /// Everything but the image: the files read here, the host's drawing of
    /// them, and the guest's waybar built.
    fn make(arch: Arch) -> Result<Self> {
        let (files, users) = crate::waybar::files()?;
        if !users {
            println!(
                "  {arch}: no ~/.config/waybar/style.css here; the bar is drawn in the tree's own \
                 style"
            );
        }
        let mut families = crate::waybar::families(&files);
        families.push("sans-serif".to_owned());
        let fonts = crate::dotfiles::font_files(&families);
        if fonts.is_empty() {
            return Err(Error::new(format!(
                "{arch}: this machine resolves no font file for {families:?}"
            )));
        }
        let (want, expected_path, modules) = waybar_expected(arch, &files, &fonts)?;
        let guest = crate::apps::program(arch, "waybar", "waybar")?;
        // waybar runs every `exec` as `/bin/sh -c`, and the test config's
        // scripts are `echo`s: zinc is the shell, and nothing else is needed.
        let mut carried = Carried {
            zinc: crate::zinc::build(arch)?,
            ..Carried::none()
        };
        carried.ports.push(crate::ports::File {
            path: "bin/waybar".to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(
                std::fs::read(&guest)
                    .map_err(|error| Error::new(format!("reading {}: {error}", guest.display())))?,
            ),
        });
        carried.ports.extend(files);
        carried.ports.extend(fonts);
        Ok(Self {
            want,
            expected_path,
            modules,
            users,
            carried,
        })
    }

    /// The compositor's config: the screen, and waybar started at `level`.
    fn config(level: &str) -> String {
        let (width, height) = crate::waybar::SIZE;
        format!(
            "# Written into the initramfs by `cargo xtask test-compositor`'s waybar boots.\n\
             monitor = , {width}x{height}@60, auto, 1\n\
             exec-once = /bin/waybar -l {level} -c /{home}/config.jsonc \
             -s /{home}/style.css --fonts-dir /{fonts}\n",
            home = crate::waybar::HOME_DIR,
            fonts = crate::dotfiles::FONT_DIR,
        )
    }
}

/// Wait for waybar's `Bar configured`, or say that it never came.
fn waybar_configured(arch: Arch, watching: &mut Watching<'_>) -> Result<()> {
    let configured = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines
            .iter()
            .any(|line| line.contains("Bar configured (width:"))
    })?;
    if configured {
        Ok(())
    } else {
        Err(with_the_transcript(
            &Error::new(format!("{arch}: waybar never said its bar was configured")),
            watching,
        ))
    }
}

/// The picture the waybar boot's screen must show: the same `files` and
/// `fonts` written here, and the `x86_64` build's `--render` of them, kept
/// as `build/<arch>/waybar-expected.ppm`; and where the render says each
/// module is.
fn waybar_expected(
    arch: Arch,
    files: &[crate::ports::File],
    fonts: &[crate::ports::File],
) -> Result<(Image, PathBuf, Vec<WaybarModule>)> {
    // The same files here, for the host's render.
    let here = paths::build_dir(arch).join("waybar-boot");
    let _ = std::fs::remove_dir_all(&here);
    crate::waybar::write_here(&here, files, crate::waybar::HOME_DIR)?;
    crate::waybar::write_here(&here.join("fonts"), fonts, crate::dotfiles::FONT_DIR)?;
    let expected_path = paths::build_dir(arch).join("waybar-expected.ppm");
    let host = crate::apps::program(Arch::X86_64, "waybar", "waybar")?;
    let (width, height) = crate::waybar::SIZE;
    let rendered = std::process::Command::new(&host)
        .arg("-c")
        .arg(here.join("config.jsonc"))
        .arg("-s")
        .arg(here.join("style.css"))
        .arg("--fonts-dir")
        .arg(here.join("fonts"))
        .args([
            "--output",
            crate::waybar::OUTPUT,
            "--over",
            crate::waybar::GROUND,
        ])
        .arg("--size")
        .arg(format!("{width}x{height}"))
        .arg("--render")
        .arg(&expected_path)
        .output()
        .map_err(|error| Error::new(format!("running {}: {error}", host.display())))?;
    let said_here = String::from_utf8_lossy(&rendered.stdout).trim().to_owned();
    if !rendered.status.success() {
        return Err(Error::new(format!(
            "{arch}: the host's waybar failed: {said_here} {}",
            String::from_utf8_lossy(&rendered.stderr).trim()
        )));
    }
    println!(
        "  {arch}: here, {}",
        said_here.lines().next().unwrap_or_default()
    );
    let modules = said_here
        .lines()
        .filter_map(crate::waybar::module_line)
        .collect();
    let bytes = std::fs::read(&expected_path)
        .map_err(|error| Error::new(format!("reading {}: {error}", expected_path.display())))?;
    Ok((parse_ppm(&bytes)?, expected_path, modules))
}

/// `cargo xtask test-compositor`'s `waybar` boot: `/bin/waybar` draws the
/// user's bar -- their own `~/.config/waybar/style.css`, its icons and the
/// fonts it names, with a config of the tree's that is the user's module for
/// module (`crate::waybar`) -- and the top of the screen must be exactly
/// the picture the `x86_64` build of the same program draws here with
/// `--render` from the same files: the bar laid over hyprix's ground.
///
/// So the chips, their SVG caps, the text in the user's font and the
/// translucent ground are all judged, pixel for pixel on `x86_64` and
/// within two a channel elsewhere. The screenshot is kept as
/// `build/<arch>/waybar.ppm`.
pub(super) fn test_waybar(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let WaybarBoot {
        want,
        expected_path,
        users,
        carried,
        ..
    } = WaybarBoot::make(arch)?;
    let config = WaybarBoot::config("info");
    let (image, kernel) = build_image(arch, programs, &undithered(&config), carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("waybar.ppm");
    let tolerance = if arch == Arch::X86_64 { 0 } else { 2 };
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        waybar_configured(arch, watching)?;
        let deadline = Instant::now() + SETTLE;
        loop {
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            let bytes = std::fs::read(&dump)
                .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
            let screen = parse_ppm(&bytes)?;
            let (differing, first) = picture_differences(&screen, &want, (0, 0), tolerance);
            if differing == 0 {
                println!(
                    "  {arch}: the bar's {}x{} pixels are the ones this machine draws from the \
                     {} style; the screen is {}",
                    want.width,
                    want.height,
                    if users { "user's" } else { "tree's" },
                    dump.display()
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(with_the_transcript(
                    &Error::new(format!(
                        "{arch}: {differing} of the bar's pixels differ from this machine's \
                         drawing ({}); the first at {first:?}; the screen is {}",
                        expected_path.display(),
                        dump.display()
                    )),
                    watching,
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}

/// What waybar says at debug level each time the server answers
/// (`modules/pulseaudio.rs`, `answered`): the prefix, and why it asked, on
/// connecting and on being told by its subscription that the sink changed.
const WAYBAR_SINK: &str = "pulseaudio: ";
/// Asked on connecting.
const WAYBAR_CONNECTING: &str = "on connecting, the default sink ";
/// Asked because the subscription said the sink changed.
const WAYBAR_TOLD: &str = "told the sink changed, the default sink ";

/// How many wheel clicks down the volume boot turns over the chip; with
/// the config's `scroll-step` unset, each is one percent.
const WAYBAR_CLICKS: u32 = 3;

/// `cargo xtask test-compositor`'s `waybar-volume` boot: the waybar boot's
/// bar with `pulsed` running beside it as the desktop runs it (a unit of
/// the session, `PULSE_SERVER` given to hyprix's clients) on the card, and
/// the volume chip held to the server.
///
/// waybar must connect, and read the sink at `PA_VOLUME_NORM`, 100%, the
/// volume `pulsed` starts at, with its label reading `vol 100%`. Then the
/// pointer goes to the chip where the host's drawing puts it and the wheel
/// turns three clicks down: waybar sets the sink's volume, `pulsed` tells
/// its subscribers the sink changed, and waybar, asking again because it
/// was told, must read 97% and label it `vol 97%`. So the client's asking,
/// setting and subscribed connections are all used.
/// The screenshot is kept as `build/<arch>/waybar-volume.ppm`.
pub(super) fn test_waybar_volume(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let WaybarBoot {
        modules,
        mut carried,
        ..
    } = WaybarBoot::make(arch)?;
    let chip = modules
        .iter()
        .find(|(name, _)| name == "pulseaudio")
        .map(|(_, rect)| *rect)
        .ok_or_else(|| Error::new(format!("{arch}: the host's drawing has no pulseaudio chip")))?;
    let pulsed = crate::audio::build_media(arch, "media-pulsed", "pulsed")?;
    carried.pulsed = Some(
        std::fs::read(&pulsed)
            .map_err(|error| Error::new(format!("{}: {error}", pulsed.display())))?,
    );
    let config = WaybarBoot::config("debug");
    let (image, kernel) = build_image(arch, programs, &undithered(&config), carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    if qemu_args.audio.is_none() {
        let wav = paths::build_dir(arch).join("waybar-volume.wav");
        qemu_args.audio = Some(format!("wav,path={}", wav.display()));
    }
    let dump = paths::build_dir(arch).join("waybar-volume.ppm");
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        waybar_configured(arch, watching)?;
        waybar_reads(arch, watching, WAYBAR_CONNECTING, 100)?;
        let (width, height) = crate::waybar::SIZE;
        let [x, y, w, h] = chip;
        let tablet = |at: f32, across: u32| {
            #[expect(clippy::cast_possible_truncation, reason = "a pixel on a screen")]
            let at = at.round() as i32;
            at * 0x7FFF / i32::try_from(across).unwrap_or(1)
        };
        qmp.input_send_event(&[
            absolute("x", tablet(x + w / 2.0, width)),
            absolute("y", tablet(y + h / 2.0, height)),
        ])?;
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..WAYBAR_CLICKS {
            qmp.input_send_event(&[button_event("wheel-down", true)])?;
            qmp.input_send_event(&[button_event("wheel-down", false)])?;
            std::thread::sleep(Duration::from_millis(150));
        }
        waybar_reads(arch, watching, WAYBAR_TOLD, 100 - WAYBAR_CLICKS)?;
        std::thread::sleep(Duration::from_millis(500));
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        println!(
            "  {arch}: waybar read pulsed's sink at 100%, set it {WAYBAR_CLICKS} clicks down \
             from the wheel, and was told it is at {}%; the screen is {}",
            100 - WAYBAR_CLICKS,
            dump.display()
        );
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}

/// Wait for waybar to say, having asked for the reason `why`, that the
/// sink is at `volume` and its label reads it.
fn waybar_reads(arch: Arch, watching: &mut Watching<'_>, why: &str, volume: u32) -> Result<()> {
    let at = format!("is at {volume}%: the label reads \"vol {volume}%\"");
    let read = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines
            .iter()
            .any(|line| line.contains(&format!("{WAYBAR_SINK}{why}")) && line.contains(&at))
    })?;
    if read {
        Ok(())
    } else {
        Err(with_the_transcript(
            &Error::new(format!(
                "{arch}: waybar never said, {why}, that the sink {at}"
            )),
            watching,
        ))
    }
}

/// How many of `want`'s pixels the screen at `origin` does not show, each
/// channel allowed `tolerance`, and where the first is.
fn picture_differences(
    screen: &Image,
    want: &Image,
    origin: (usize, usize),
    tolerance: u8,
) -> (usize, Option<Differing>) {
    let mut count = 0;
    let mut first = None;
    for y in 0..want.height {
        for x in 0..want.width {
            let wanted = want
                .pixels
                .get((y * want.width + x) * 3..(y * want.width + x) * 3 + 3);
            let (sx, sy) = (x + origin.0, y + origin.1);
            let shown = (sx < screen.width && sy < screen.height)
                .then(|| {
                    screen
                        .pixels
                        .get((sy * screen.width + sx) * 3..(sy * screen.width + sx) * 3 + 3)
                })
                .flatten();
            let same = match (wanted, shown) {
                (Some(wanted), Some(shown)) => wanted
                    .iter()
                    .zip(shown)
                    .all(|(one, two)| one.abs_diff(*two) <= tolerance),
                _ => false,
            };
            if !same {
                count += 1;
                if first.is_none() {
                    first = Some((x, y, triple(wanted), triple(shown)));
                }
            }
        }
    }
    (count, first)
}

/// Where `src/user/system/linux/compositor/caption` holds its surface from the top-left corner:
/// its `MARGIN`.
const CAPTION_MARGIN: i32 = 40;
