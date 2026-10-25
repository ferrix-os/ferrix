//! `test-compositor --boot files`: Files is shelf on Yoru (the customer's
//! choice). The dock's Files button opens shelf's window, and the stash's
//! folder button opens another, on the stash's folder.
//!
//! The image carries shelf as an installed app, so the dock finds it among
//! the desktop's entries. The host moves QEMU's tablet along the dock until
//! the dock says the pointer is over "Files" (what its tooltip shows), and
//! presses there; then it opens the stash's panel, which a script filled,
//! and presses its folder button. A script in the guest says the windows'
//! classes and titles every two seconds. Required: a window of shelf's class
//! after the dock's button, and a second after the panel's. Kept as
//! `files.ppm`.

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

/// The class shelf's windows have.
const SHELF: &str = "class: rocks.magical.shelf";

/// The guest's side: a file in the stash, and the windows.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
mkdir -p "$HOME/Documents"
echo notes > "$HOME/Documents/notes.md"
/bin/yoru-shell --stash "$HOME/Documents/notes.md"
while :; do
  echo "files-probe: windows $(hyprctl clients | grep -E '^[[:space:]]*(class|title):' | tr '\n' ';')"
  sleep 2
done
"#;

/// The pointer to `(x, y)`.
fn at(qmp: &mut Qmp, (x, y): (i64, i64)) -> Result<()> {
    let ax = i32::try_from(x.clamp(0, SCREEN.0 - 1) * 0x7fff / (SCREEN.0 - 1)).unwrap_or(0);
    let ay = i32::try_from(y.clamp(0, SCREEN.1 - 1) * 0x7fff / (SCREEN.1 - 1)).unwrap_or(0);
    qmp.input_send_event(&[absolute("x", ax), absolute("y", ay)])
}

/// A left press where the pointer is.
fn press_here(qmp: &mut Qmp) -> Result<()> {
    let button = |down: bool| {
        format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
    };
    qmp.input_send_event(&[button(true)])?;
    std::thread::sleep(Duration::from_millis(120));
    qmp.input_send_event(&[button(false)])
}

/// How many of shelf's windows the last probe line names.
fn shelves(watching: &Watching<'_>) -> usize {
    watching
        .after()
        .iter()
        .rev()
        .find(|line| line.contains("files-probe: windows"))
        .map_or(0, |line| line.matches(SHELF).count())
}

/// `test-compositor --boot files`.
///
/// # Errors
///
/// The dock had no Files, or shelf did not come from it or the stash.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each press and what it must have done"
)]
pub(super) fn test_files(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str("exec-once = /bin/sh /etc/files-probe.sh\n");
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    // shelf installed, its desktop entry with it: what the dock lists.
    carried.ports.extend(crate::apps::taken(arch, &["shelf"])?);
    carried.ports.push(crate::ports::File {
        path: "etc/files-probe.sh".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(SCRIPT.as_bytes().to_vec()),
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
            lines
                .iter()
                .any(|l| l.contains("yoru-shell: the dock is up"))
                && lines.iter().any(|l| l.contains("files-probe: windows"))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: files: the dock or the probe never came")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        // Along the dock, the middle of its bar, until it says Files.
        let y = SCREEN.1 - 6 - 30;
        let mut found = None;
        for x in (200..SCREEN.0 - 200).step_by(8) {
            let from = watching.after().len();
            at(&mut qmp, (x, y))?;
            let said =
                watching.read_more(Instant::now() + Duration::from_millis(400), |lines| {
                    lines
                        .iter()
                        .skip(from)
                        .any(|l| l.contains("yoru-shell: over Files"))
                })?;
            if said {
                found = Some(x);
                break;
            }
        }
        let Some(x) = found else {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: files: the dock has no Files")),
                watching,
            ));
        };
        println!("  {arch}: files: the dock's Files is at {x},{y}");
        std::thread::sleep(Duration::from_millis(500));
        press_here(&mut qmp)?;
        let opened = watching.read_more(Instant::now() + Duration::from_secs(60), |lines| {
            lines
                .iter()
                .rev()
                .find(|l| l.contains("files-probe: windows"))
                .is_some_and(|l| l.contains(SHELF))
        })?;
        if !opened {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: files: the dock's Files opened no shelf")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        let dump = paths::build_dir(arch).join("files.ppm");
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        println!(
            "  {arch}: files: shelf is up (the screen is {})",
            dump.display()
        );
        // The stash's panel, and its folder button: the first of the head's
        // three, on the panel 340 wide in the middle, its foot 84 above the
        // screen's (one row of tiles).
        let from = watching.after().len();
        let mut opened_panel = false;
        for x in (200..SCREEN.0 - 200).step_by(8) {
            let since = watching.after().len();
            at(&mut qmp, (x, y))?;
            let over =
                watching.read_more(Instant::now() + Duration::from_millis(400), |lines| {
                    lines
                        .iter()
                        .skip(since)
                        .any(|l| l.contains("yoru-shell: over Stash"))
                })?;
            if over {
                std::thread::sleep(Duration::from_millis(500));
                press_here(&mut qmp)?;
                opened_panel =
                    watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
                        lines
                            .iter()
                            .skip(from)
                            .any(|l| l.contains("the stash panel is up"))
                    })?;
                break;
            }
        }
        if !opened_panel {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: files: the stash's panel did not open")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        let left = (SCREEN.0 - 340) / 2;
        let top = SCREEN.1 - 84 - (80 + 96 + 12);
        let before = shelves(watching);
        at(&mut qmp, (left + 340 - 14 - 2 * 34 - 14 - 30, top + 28))?;
        std::thread::sleep(Duration::from_millis(800));
        at(&mut qmp, (left + 340 - 14 - 2 * 34 - 14, top + 28))?;
        std::thread::sleep(Duration::from_millis(800));
        press_here(&mut qmp)?;
        let since = watching.after().len();
        let second = watching.read_more(Instant::now() + Duration::from_secs(60), |lines| {
            lines
                .iter()
                .skip(since)
                .any(|l| l.contains("files-probe: windows") && l.matches(SHELF).count() > before)
        })?;
        let said = watching
            .after()
            .iter()
            .any(|l| l.contains("stash: open the folder") && l.contains(": ok"));
        if !second || !said {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: files: the stash's folder button opened no shelf ({} before, {} after)",
                    before,
                    shelves(watching)
                )),
                watching,
            ));
        }
        println!(
            "  {arch}: files: the dock's Files and the stash's folder button each opened shelf"
        );
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
