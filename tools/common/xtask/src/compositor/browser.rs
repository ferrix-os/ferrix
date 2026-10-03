//! `test-chrome-window` and `test-chrome-audio`: Google's Chrome in a window
//! on the compositor, on Ferrix, drawing a page and playing its sound.
//!
//! The libc Chrome runs on is chosen here too, because `run-compositor
//! --chrome` chooses it the same way and the two must not drift.

use std::path::Path;
use std::time::{Duration, Instant};

use super::boot::{
    absolute, build_image, press, said_on_its_own, say_the_marker, undithered, with_the_transcript,
};
use super::{BACKGROUND, Carried, EITHER, FAILED, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The page Chrome's window shows: `test-chrome`'s picture, a yellow a
/// screen counts. No spaces, because the compositor splits `exec-once` at
/// them.
pub(super) const CHROME_WINDOW_PAGE: &str =
    "data:text/html,<body%20style=background:%23fc0><h1>Hello%20from%20Chrome%20on%20Ferrix</h1>";

/// That page's background, `#fc0`, as the screen has it.
const CHROME_YELLOW: [u8; 3] = [0xff, 0xcc, 0x00];

/// The fewest yellow pixels a screen with the page on it has: a tenth of a
/// 1024x768 screen. The window is most of the screen and the page most of
/// the window; the firmware's screen and a window Chrome has not yet drawn
/// into have none.
const CHROME_YELLOW_PIXELS: usize = 78_643;

/// How long Chrome gets from the compositor coming up to its page on the
/// screen: a 294 MB browser and three of its processes, started from a btrfs
/// volume, laying out and drawing in software.
pub(super) const CHROME_WINDOW_PATIENCE: Duration = Duration::from_secs(240);

/// The configuration `test-chrome-window` gives the compositor.
///
/// Chrome's environment and command are [`crate::chrome::window_command`]'s,
/// which `run-compositor --chrome` starts too; on ferrousli, with its search
/// path.
fn chrome_window_config(ferrousli: bool) -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-chrome-window`.\n{}{}exec-once = {}\n",
        crate::chrome::WINDOW_ENV,
        crate::chrome::window_library_path(ferrousli),
        crate::chrome::window_command(CHROME_WINDOW_PAGE)
    )
}

/// What `--interpreter` takes on `run-compositor --chrome` for the volume's
/// own glibc, which ferrousli otherwise stands in for.
const GLIBC: &str = "glibc";

/// What puts the libc [`chrome_libc`] chose under Chrome on `volume`:
/// ferrousli's loader and `libc.so.6`, or the links to the volume's glibc.
fn chrome_libc_files(arch: Arch, volume: &Path, args: &Args) -> Result<Vec<crate::ports::File>> {
    if crate::chrome::on_ferrousli(args) {
        println!("  {arch}: Chrome on ferrousli's loader and libc.so.6");
        crate::chrome::ferrousli_files(arch, volume, crate::chrome::WINDOW_PROGRAM, args)
    } else {
        println!("  {arch}: Chrome on the volume's glibc");
        Ok(crate::rustc::files(crate::chrome::LINKS))
    }
}

/// The libc Chrome runs on: ferrousli's loader and `libc.so.6` unless
/// `--interpreter glibc` asks for the volume's own, the customer's choice of
/// 2026-09-26. An `--interpreter` or `--library` of another is kept.
pub(super) fn chrome_libc(args: &mut Args) {
    // `--nvidia`: NVIDIA's libraries are glibc's, so Chrome on the 3060 runs
    // on the volume's glibc, as `--interpreter glibc` asks.
    if args.nvidia {
        args.interpreter = Some(GLIBC.to_owned());
    }
    if args.interpreter.as_deref() == Some(GLIBC) {
        args.interpreter = None;
        args.libraries.clear();
    } else if !crate::chrome::on_ferrousli(args) {
        args.interpreter = Some(crate::shell::FERROUSLI.to_owned());
        args.libraries = vec![crate::shell::FERROUSLI.to_owned()];
    }
}

/// How many pixels of a screen are Chrome's page yellow, or none when the
/// screen is not the compositor's.
fn yellow_pixels(screen: &Image) -> usize {
    let (pixels, _) = screen.pixels.as_chunks::<3>();
    if !pixels.contains(&BACKGROUND) {
        return 0;
    }
    pixels
        .iter()
        .filter(|pixel| **pixel == CHROME_YELLOW)
        .count()
}

/// `test-chrome-window`: Google's Chrome in a window on the compositor, on
/// Ferrix.
///
/// The full browser from the volume `tools/common/fetch/fetch-chrome.sh` makes -- the
/// same version `test-chrome` runs headless -- started by the compositor's
/// `exec-once` as a Wayland client, drawing its tabs, its toolbar and a page
/// into a window the compositor tiles. What is required is the page on the
/// screen: its yellow, over a tenth of it, with the compositor's background
/// around the window. The busiest screen is kept, to be looked at.
pub(crate) fn test_chrome_window(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-chrome-window runs on x86-64 only: Chrome for Testing publishes linux64 alone",
        ));
    }
    let mut args = args.clone();
    let volume = crate::chrome::volume()?;
    args.data_image = Some(volume.clone());
    if !args.memory_given {
        args.memory = crate::chrome::MEMORY;
    }
    let ferrousli = crate::chrome::on_ferrousli(&args);
    let programs = Programs::build(arch)?;
    let mut ports = if ferrousli {
        println!("  {arch}: Chrome in a window on ferrousli's loader and libc.so.6");
        crate::chrome::ferrousli_files(arch, &volume, crate::chrome::WINDOW_PROGRAM, &args)?
    } else {
        crate::rustc::files(crate::chrome::LINKS)
    };
    ports.extend(crate::chrome::window_files());
    let carried = Carried {
        ports,
        ..Carried::none()
    };
    let (image, kernel) = build_image(
        arch,
        &programs,
        &undithered(&chrome_window_config(ferrousli)),
        carried,
        &args,
    )?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("chrome-window.ppm");
    let mut said: Vec<String> = Vec::new();
    let mut best: Option<Image> = None;
    // The emptiest screen after the page was clicked into and typed at.
    let mut after_input: Option<Image> = None;
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
        let deadline = Instant::now() + CHROME_WINDOW_PATIENCE;
        loop {
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            let bytes = std::fs::read(&dump)
                .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
            let screen = parse_ppm(&bytes)?;
            let found = yellow_pixels(&screen);
            if best.as_ref().is_none_or(|kept| yellow_pixels(kept) < found) {
                best = Some(screen);
            }
            if found >= CHROME_YELLOW_PIXELS || Instant::now() >= deadline {
                break;
            }
            let _ = watching.read_more(Instant::now() + Duration::from_secs(2), |_| false)?;
        }
        // Clicked into and typed at, as a person does: the window must
        // still show its page. It went blank on the desktop when the
        // compositor knew pools by object id, and a pool Chrome made after
        // the click took the id of the one its window was drawn from.
        if best
            .as_ref()
            .is_some_and(|screen| yellow_pixels(screen) >= CHROME_YELLOW_PIXELS)
        {
            after_input = Some(click_and_type(&mut qmp, watching, &dump)?);
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
    judge_chrome_window(arch, &said, best.as_ref(), after_input.as_ref(), &dump)
}

/// The page `test-chrome-audio` opens: a tone of [`CHROME_TONE_HZ`], started
/// as the page loads, which `--autoplay-policy=no-user-gesture-required`
/// lets it do without a click.
const CHROME_TONE_PAGE: &str = "data:text/html,<body%20style=background:%23fc0><h1>tone</h1><script>c=new%20AudioContext();o=c.createOscillator();o.frequency.value=440;g=c.createGain();g.gain.value=0.2;o.connect(g).connect(c.destination);o.start();document.title=c.state</script>";

/// The tone's pitch.
const CHROME_TONE_HZ: f64 = 440.0;

/// Frames of tone the file must hold: a second's worth, at the card's rate.
const CHROME_TONE_FRAMES: usize = 48_000;

/// `test-chrome-audio`: Google's Chrome in a window on the compositor, on
/// Ferrix, playing a page's `AudioContext` through `/dev/snd`.
///
/// The window of `test-chrome-window`, on a page that plays 440 Hz as it
/// loads, with a virtio-snd card whose far end is QEMU's `wav` backend
/// (`docs/AUDIO.md` §4). Chrome's audio service opens alsa-lib's `default`,
/// which is `plug` over the card, and writes through the ALSA ioctls to the
/// kernel's audio core; what the device consumes is in the file. What is
/// required is a second of it that is not silence, with the tone's pitch:
/// its zero crossings, counted over what was played, give the frequency.
///
/// # Errors
///
/// When the volume is missing, the boot fails, or the file holds no second
/// of a 440 Hz tone.
pub(crate) fn test_chrome_audio(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-chrome-audio runs on x86-64 only: Chrome for Testing publishes linux64 alone",
        ));
    }
    let mut args = args.clone();
    let volume = crate::chrome::pulse_volume()?;
    args.data_image = Some(volume.clone());
    if !args.memory_given {
        args.memory = crate::chrome::MEMORY;
    }
    // On the libc `run-compositor --chrome` gives Chrome: a client of pulsed
    // loads libpulse on it, which needs what glibc has and ferrousli may
    // not (`backtrace_symbols`, missed on 2026-09-27 by a gate on glibc).
    chrome_libc(&mut args);
    let ferrousli = crate::chrome::on_ferrousli(&args);
    let programs = Programs::build(arch)?;
    let mut ports = chrome_libc_files(arch, &volume, &args)?;
    ports.extend(crate::chrome::window_files());
    let pulsed = crate::audio::build_media(arch, "media-pulsed", "pulsed")?;
    let carried = Carried {
        ports,
        pulsed: Some(
            std::fs::read(&pulsed)
                .map_err(|error| Error::new(format!("{}: {error}", pulsed.display())))?,
        ),
        ..Carried::none()
    };
    let config = format!(
        "# Carried into the initramfs by `cargo xtask test-chrome-audio`.\n{}{}exec-once = {}\n",
        crate::chrome::WINDOW_ENV,
        crate::chrome::window_library_path(ferrousli),
        crate::chrome::window_command(CHROME_TONE_PAGE)
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let wav = paths::build_dir(arch).join("chrome-audio.wav");
    if wav.exists() {
        std::fs::remove_file(&wav)
            .map_err(|error| Error::new(format!("{}: {error}", wav.display())))?;
    }
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.audio = Some(format!("wav:{}", wav.display()));
    let mut heard = None;
    let mut said: Vec<String> = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + CHROME_WINDOW_PATIENCE;
        loop {
            if let Some(tone) = std::fs::read(&wav).ok().and_then(|bytes| tone_in(&bytes)) {
                heard = Some(tone);
                if tone.0 >= CHROME_TONE_FRAMES {
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            let _ = watching.read_more(Instant::now() + Duration::from_secs(2), |_| false)?;
        }
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    // Through the sound server, not around it: Chrome takes Pulse once
    // libpulse loads and the server answers, and falls back to ALSA
    // without a word otherwise (docs/AUDIO.md, U2d).
    let Some(stream) = said.iter().find(|line| line.contains(PULSED_STREAM)) else {
        return Err(Error::new(format!(
            "{arch}: pulsed never said `{PULSED_STREAM}`: Chrome's sound did not go through \
             the sound server"
        )));
    };
    println!("  {arch}: {}", stream.trim());
    match heard {
        Some((frames, hz)) if frames >= CHROME_TONE_FRAMES => {
            if (hz - CHROME_TONE_HZ).abs() > CHROME_TONE_HZ * 0.02 {
                return Err(Error::new(format!(
                    "{arch}: Chrome played {frames} frames, but at {hz:.1} Hz, not \
                     {CHROME_TONE_HZ} Hz; {}",
                    wav.display()
                )));
            }
            println!(
                "  {arch}: Chrome played {:.2} s of a {hz:.1} Hz tone through pulsed and \
                 /dev/snd",
                frames as f64 / 48_000.0
            );
            Ok(())
        }
        Some((frames, _)) => Err(Error::new(format!(
            "{arch}: Chrome played only {frames} frames that were not silence; {}",
            wav.display()
        ))),
        None => Err(Error::new(format!(
            "{arch}: nothing Chrome played reached the card's file {}",
            wav.display()
        ))),
    }
}

/// What `pulsed` says of each stream a client makes.
const PULSED_STREAM: &str = "pulsed: a stream: ";

/// The frames of a WAV file's S16 stereo data that are not silence, and the
/// pitch of their left channel by its zero crossings, or `None` when there
/// is no data yet.
fn tone_in(wav: &[u8]) -> Option<(usize, f64)> {
    let data = wav.windows(4).position(|window| window == b"data")?;
    let (whole, _) = wav.get(data + 8..)?.as_chunks::<4>();
    let samples: Vec<i16> = whole
        .iter()
        .map(|&[a, b, _, _]| i16::from_le_bytes([a, b]))
        .collect();
    let start = samples
        .iter()
        .position(|sample| sample.unsigned_abs() > 64)?;
    let end = samples
        .iter()
        .rposition(|sample| sample.unsigned_abs() > 64)?
        + 1;
    let heard = samples.get(start..end)?;
    let crossings = heard
        .windows(2)
        .filter(|pair| matches!(pair, [a, b] if (*a < 0) != (*b < 0)))
        .count();
    let seconds = heard.len() as f64 / 48_000.0;
    Some((heard.len(), crossings as f64 / 2.0 / seconds.max(1e-9)))
}

/// Click into Chrome's page and type three letters, as a person does, and
/// answer the emptiest of the screens taken after the click, after the
/// typing and five seconds later.
fn click_and_type(qmp: &mut Qmp, watching: &mut Watching<'_>, dump: &Path) -> Result<Image> {
    let click = |down: bool| {
        format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
    };
    let mut emptiest: Option<Image> = None;
    let mut look = |qmp: &mut Qmp| -> Result<()> {
        qmp.screendump(Some(DEVICE_ID), dump)?;
        let bytes = std::fs::read(dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        let screen = parse_ppm(&bytes)?;
        if emptiest
            .as_ref()
            .is_none_or(|kept| yellow_pixels(&screen) < yellow_pixels(kept))
        {
            emptiest = Some(screen);
        }
        Ok(())
    };
    qmp.input_send_event(&[absolute("x", 16384), absolute("y", 20000)])?;
    std::thread::sleep(Duration::from_millis(500));
    qmp.input_send_event(&[click(true)])?;
    std::thread::sleep(Duration::from_millis(100));
    qmp.input_send_event(&[click(false)])?;
    std::thread::sleep(Duration::from_millis(500));
    look(qmp)?;
    for name in ["a", "b", "c"] {
        press(qmp, &[name])?;
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_secs(2));
    look(qmp)?;
    let _ = watching.read_more(Instant::now() + Duration::from_secs(5), |_| false)?;
    look(qmp)?;
    emptiest.ok_or_else(|| Error::new("no screen was taken after the click"))
}

/// What [`test_chrome_window`] requires of what the guest said and showed.
fn judge_chrome_window(
    arch: Arch,
    said: &[String],
    screen: Option<&Image>,
    after_input: Option<&Image>,
    dump: &Path,
) -> Result<()> {
    let transcript = || {
        said.iter()
            .map(|line| said_on_its_own(line).to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let fail = |why: String| Err(Error::new(format!("{arch}: {why}\n{}", transcript())));
    if let Some(line) = said.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return fail(format!(
            "the kernel stopped while Chrome ran: {}",
            line.trim()
        ));
    }
    let Some(screen) = screen else {
        return fail("the boot took no picture".to_owned());
    };
    let kept = dump.with_file_name("chrome-window-busiest.ppm");
    let mut ppm = format!("P6\n{} {}\n255\n", screen.width, screen.height).into_bytes();
    ppm.extend_from_slice(&screen.pixels);
    std::fs::write(&kept, &ppm)
        .map_err(|error| Error::new(format!("writing {}: {error}", kept.display())))?;
    let found = yellow_pixels(screen);
    if found < CHROME_YELLOW_PIXELS {
        return fail(format!(
            "the screen has {found} pixels of the page's yellow, fewer than the \
             {CHROME_YELLOW_PIXELS} of Chrome's window with it drawn; the busiest screen is {}",
            kept.display()
        ));
    }
    let Some(after) = after_input else {
        return fail("the page was never clicked into and typed at".to_owned());
    };
    let after_found = yellow_pixels(after);
    if after_found < CHROME_YELLOW_PIXELS {
        let blank = dump.with_file_name("chrome-window-after-input.ppm");
        let mut ppm = format!("P6\n{} {}\n255\n", after.width, after.height).into_bytes();
        ppm.extend_from_slice(&after.pixels);
        std::fs::write(&blank, &ppm)
            .map_err(|error| Error::new(format!("writing {}: {error}", blank.display())))?;
        return fail(format!(
            "clicked into and typed at, the window went blank: {after_found} pixels of the \
             page's yellow at the emptiest, from {found}; the screen is {}",
            blank.display()
        ));
    }
    println!(
        "  {arch}: Chrome drew its window and the page on the compositor, {found} pixels of \
         the page's yellow, and still {after_found} after it was clicked into and typed at; \
         the screen is {}",
        kept.display()
    );
    Ok(())
}
