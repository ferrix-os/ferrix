//! The Yoru boots (`docs/DESKTOP.md` §6): the desktop in the customer's
//! design, as `run-compositor` now draws it by default, screenshotted.

use std::time::{Duration, Instant};

use super::boot::{build_image, with_the_transcript};
use super::{Carried, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The screen the Yoru boots are drawn on.
const SCREEN: (usize, usize) = (1024, 768);

/// The theme's look on two floating windows that asked for the
/// compositor's frame: Nocturne's wallpaper, title bars on the surface with
/// the focused one's accent line, the focused window's glow.
const CONFIG: &str = "\
# Written into the initramfs by `cargo xtask test-compositor`'s yoru boot.
exec-once = /bin/pattern gradient one
exec-once = /bin/pattern checkerboard two --after one
# No grain in the blur, so the picture is the same each run.
decoration:blur:noise = 0
";

/// A boot of Yoru's look (step Y1): the theme sourced as `run-compositor`
/// sources it, the wallpaper drawn, two pattern windows with Yoru's title
/// bars, and the picture judged by its colours -- Nocturne's ground, the
/// bars' surface, the accent's line.
pub(super) fn test_yoru(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let themed = Args {
        theme: Some("yoru".to_owned()),
        config: None,
        ..args.clone()
    };
    let mut carried = Carried::none();
    let config = format!(
        "monitor = , {}x{}@60, auto, 1\n{CONFIG}",
        SCREEN.0, SCREEN.1
    );
    let config = super::theme::apply(config, &themed, &mut carried.ports)?;
    let (image, kernel) = build_image(arch, programs, &config, carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("yoru.ppm");
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let ready = watching.read_more(Instant::now() + SETTLE, |lines| {
            let drawn = lines
                .iter()
                .any(|line| line.contains("wallpaper: ") && line.contains(" drawn at "));
            let framed = lines
                .iter()
                .any(|line| line.contains("pattern: two decorations server"));
            drawn && framed
        })?;
        if !ready {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: yoru: the wallpaper or the second window never said it was drawn"
                )),
                watching,
            ));
        }
        let deadline = Instant::now() + SETTLE;
        loop {
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            let bytes = std::fs::read(&dump)
                .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
            let screen = parse_ppm(&bytes)?;
            match judge(&screen) {
                Ok(said) => {
                    println!("  {arch}: yoru: {said}; the picture is {}", dump.display());
                    return Ok(());
                }
                Err(why) if Instant::now() >= deadline => {
                    return Err(with_the_transcript(
                        &Error::new(format!(
                            "{arch}: yoru: {why}; the screen is {}",
                            dump.display()
                        )),
                        watching,
                    ));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}

/// Whether `screen` is Yoru's desktop: Nocturne's ground in the lower
/// right corner, which no window reaches; title bars on Nocturne's surface,
/// `#232532`; and the focused one's accent line, accent to accent-2 to
/// accent-3.
fn judge(screen: &Image) -> core::result::Result<String, String> {
    if (screen.width, screen.height) != SCREEN {
        return Err(format!("a {}x{} screen", screen.width, screen.height));
    }
    let pixel = |x: usize, y: usize| -> [i32; 3] {
        let at = (y * screen.width + x) * 3;
        match screen.pixels.get(at..at + 3) {
            Some(&[r, g, b]) => [i32::from(r), i32::from(g), i32::from(b)],
            _ => [0, 0, 0],
        }
    };
    let [r, g, b] = pixel(screen.width - 3, screen.height - 3);
    if !(r < 0x30 && g < 0x30 && b < 0x48 && b >= r) {
        return Err(format!("the corner is {r},{g},{b}, not Nocturne's ground"));
    }
    let near = |[r, g, b]: [i32; 3], [r0, g0, b0]: [i32; 3], by: i32| {
        (r - r0).abs() <= by && (g - g0).abs() <= by && (b - b0).abs() <= by
    };
    let mut surface = 0usize;
    let mut accent = 0usize;
    for y in 0..screen.height {
        for x in 0..screen.width {
            let seen = pixel(x, y);
            if near(seen, [0x23, 0x25, 0x32], 2) {
                surface += 1;
            }
            if near(seen, [0x91, 0x84, 0xd9], 24)
                || near(seen, [0x00, 0xb2, 0xba], 24)
                || near(seen, [0xce, 0x7b, 0xb4], 24)
            {
                accent += 1;
            }
        }
    }
    // A bar's worth at least 200 pixels wide and 30 high; a line along one.
    if surface < 200 * 30 {
        return Err(format!("{surface} pixels of the bars' surface"));
    }
    if accent < 80 {
        return Err(format!("{accent} pixels of the accent's line"));
    }
    Ok(format!(
        "Nocturne's ground, {surface} pixels of title bar, {accent} of the accent"
    ))
}

#[cfg(test)]
mod tests {
    use super::{SCREEN, judge};
    use crate::display::Image;

    /// A screen of the ground alone is not Yoru's desktop: it has no bars.
    #[test]
    fn the_ground_alone_has_no_bars() {
        let screen = Image {
            width: SCREEN.0,
            height: SCREEN.1,
            pixels: [0x1a, 0x1c, 0x2a].repeat(SCREEN.0 * SCREEN.1),
        };
        let said = judge(&screen).expect_err("no bars");
        assert!(said.contains("surface"), "{said}");
    }

    /// Bars and a line on the ground are.
    #[test]
    fn bars_and_a_line_on_the_ground_are() {
        let mut pixels = [0x1a, 0x1c, 0x2a].repeat(SCREEN.0 * SCREEN.1);
        for y in 100..134 {
            for x in 100..700 {
                let at = (y * SCREEN.0 + x) * 3;
                let colour: [u8; 3] = if y == 100 {
                    [0x91, 0x84, 0xd9]
                } else {
                    [0x23, 0x25, 0x32]
                };
                pixels[at..at + 3].copy_from_slice(&colour);
            }
        }
        let screen = Image {
            width: SCREEN.0,
            height: SCREEN.1,
            pixels,
        };
        assert!(judge(&screen).is_ok());
    }
}
