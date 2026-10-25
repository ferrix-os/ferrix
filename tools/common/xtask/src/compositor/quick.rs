//! `test-compositor --boot quick`: the sidebar's quick settings
//! (`docs/DESKTOP.md` §6.1). The right sidebar opens on them (the
//! arrangement's file says so); focus mode, auto-tile, do not disturb and
//! a theme colour are pressed in turn. Required, for each, the shell's
//! line that the compositor took it and what it read back after (focus
//! on, tiled, the accent Settings calls Teal, quiet), and notifyd's line
//! that do not disturb is on. Kept as `quick.ppm`.

use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, with_the_transcript};
use super::{MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The screen, as the Yoru boot sets it.
const SCREEN: (i64, i64) = (1024, 768);

/// The quick settings' left edge on the right sidebar: the layer's, the
/// panel's margin, the widget's.
const LEFT: i64 = SCREEN.0 - 336 + 8 + 16;

/// The tiles' top.
const TILES: i64 = 8 + 90 + 148;

/// The pointer to `(x, y)`.
fn at(qmp: &mut Qmp, (x, y): (i64, i64)) -> Result<()> {
    let ax = i32::try_from(x.clamp(0, SCREEN.0 - 1) * 0x7fff / (SCREEN.0 - 1)).unwrap_or(0);
    let ay = i32::try_from(y.clamp(0, SCREEN.1 - 1) * 0x7fff / (SCREEN.1 - 1)).unwrap_or(0);
    qmp.input_send_event(&[absolute("x", ax), absolute("y", ay)])
}

/// A left press at `(x, y)`, the pointer there a moment first.
fn click(qmp: &mut Qmp, point: (i64, i64)) -> Result<()> {
    at(qmp, point)?;
    std::thread::sleep(Duration::from_millis(800));
    let button = |down: bool| {
        format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
    };
    qmp.input_send_event(&[button(true)])?;
    std::thread::sleep(Duration::from_millis(120));
    qmp.input_send_event(&[button(false)])
}

/// `test-compositor --boot quick`.
///
/// # Errors
///
/// The sidebar never came, or a press was not taken as it should be.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each press and what it must have done"
)]
pub(super) fn test_quick(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (config, mut carried) = super::yoru::desktop(arch, args, false)?;
    carried.ports.push(crate::ports::File {
        path: ".config/yoru-shell/sidebars".to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(
            b"left clock weather\nshown left clock\nright settings todo\nshown right settings\n"
                .to_vec(),
        ),
    });
    let (image, kernel) = build_image(arch, programs, &config, carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let up = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|l| l.contains("the right sidebar is up"))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: quick: the right sidebar never came")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(5));
        at(&mut qmp, (500, 300))?;
        std::thread::sleep(Duration::from_millis(500));
        let from = watching.after().len();
        at(&mut qmp, (SCREEN.0 - 1, 300))?;
        if !watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
            lines
                .iter()
                .skip(from)
                .any(|l| l.contains("the right sidebar opened"))
        })? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: quick: the right edge opened nothing")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        let mut failures = Vec::new();
        // Each press, and what the shell must say after it.
        for (what, point, wanted) in [
            (
                "focus mode",
                (LEFT + 70, TILES + 112 + 25),
                "quick: Focus: ok; now focus true",
            ),
            (
                "auto-tile",
                (LEFT + 147 + 70, TILES + 112 + 25),
                "quick: Tile: ok; now focus true, tiled true",
            ),
            (
                "do not disturb",
                (LEFT + 70, TILES + 56 + 25),
                "quick: do not disturb on;",
            ),
            (
                "the theme colour",
                (LEFT + 40 + 14, 8 + 90 + 402 + 14),
                "quick: Accent(1): ok; now focus true, tiled true, accent Some(1), quiet true",
            ),
        ] {
            let from = watching.after().len();
            click(&mut qmp, point)?;
            let said = watching.read_more(Instant::now() + Duration::from_secs(20), |lines| {
                lines.iter().skip(from).any(|l| l.contains(wanted))
            })?;
            if !said {
                let lines: Vec<&String> = watching
                    .after()
                    .iter()
                    .skip(from)
                    .filter(|l| l.contains("quick:"))
                    .collect();
                failures.push(format!("{what}: not \"{wanted}\" but {lines:?}"));
            }
        }
        // notifyd looks at its file every five seconds.
        let quiet = watching.read_more(Instant::now() + Duration::from_secs(15), |lines| {
            lines
                .iter()
                .any(|l| l.contains("notifyd: do not disturb on"))
        })?;
        if !quiet
            && !watching
                .lines()
                .iter()
                .any(|l| l.contains("notifyd: do not disturb on"))
        {
            failures.push("notifyd never said do not disturb is on".to_owned());
        }
        let dump = paths::build_dir(arch).join("quick.ppm");
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        println!(
            "  {arch}: quick: the quick settings pressed (the screen is {})",
            dump.display()
        );
        if failures.is_empty() {
            println!(
                "  {arch}: quick: focus mode, auto-tile, do not disturb and the theme colour each taken and read back; notifyd quiet"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: quick: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
