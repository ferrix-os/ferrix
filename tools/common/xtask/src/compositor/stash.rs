//! `test-compositor --boot stash`: Yoru's stash (the customer's, after the
//! design's dock, 2026-10-25): files put in it show as a count on the
//! dock's Stash button; a press on it opens the panel of them; a pick
//! opens one; the bin clears it, and the count goes.
//!
//! A script in the guest makes four files of the four tinted kinds and
//! puts them in with `yoru-shell --stash`, as a drop on the button does.
//! The host finds the button by its badge (Nocturne's accent-2 600, which
//! nothing else on the desktop is) in QEMU's screendump, presses it, then
//! the first tile and the bin, and keeps `stash-<step>.ppm`. Required: the
//! badge drawn; the panel up with four items; the pick asked to open the
//! newest, the first tile; the bin cleared the stash; and the badge gone after.
//! The first tile is over the title bar of a window under the panel: a
//! press there is the panel's, not the bar's (hyprix took it, and the
//! release with it, until 2026-10-25).

use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, press, with_the_transcript};
use super::{MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The screen, as the Yoru boot sets it.
const SCREEN: (i64, i64) = (1024, 768);

/// The guest's side: four files into the stash.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
mkdir -p "$HOME/Documents"
cd "$HOME/Documents" || exit 1
for f in receipt-0417.pdf night-drive.flac layout.rs plan.md; do echo "$f" > "$f"; done
sleep 2
/bin/yoru-shell --stash receipt-0417.pdf night-drive.flac layout.rs plan.md
echo "stash-probe: put in"
"#;

/// QEMU's screendump of now, kept as `stash-<step>.ppm`.
fn dump(qmp: &mut Qmp, arch: Arch, step: &str) -> Result<Image> {
    let path = paths::build_dir(arch).join(format!("stash-{step}.ppm"));
    qmp.screendump(Some(DEVICE_ID), &path)?;
    let bytes =
        std::fs::read(&path).map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    crate::display::parse_ppm(&bytes)
}

/// The middle of the badge's pixels (`#009199`) along the dock, and how
/// many there are.
fn badge(screen: &Image) -> (usize, Option<(i64, i64)>) {
    let (mut n, mut sx, mut sy) = (0usize, 0usize, 0usize);
    for y in screen.height.saturating_sub(80)..screen.height {
        for x in 0..screen.width {
            let at = (y * screen.width + x) * 3;
            if let Some(&[r, g, b]) = screen.pixels.get(at..at + 3)
                && r < 0x18
                && g.abs_diff(0x91) <= 0x14
                && b.abs_diff(0x99) <= 0x14
            {
                n += 1;
                sx += x;
                sy += y;
            }
        }
    }
    let middle = (n > 0).then(|| {
        (
            i64::try_from(sx / n).unwrap_or(0),
            i64::try_from(sy / n).unwrap_or(0),
        )
    });
    (n, middle)
}

/// A left press at `(x, y)`.
fn click(qmp: &mut Qmp, (x, y): (i64, i64)) -> Result<()> {
    let ax = i32::try_from(x.clamp(0, SCREEN.0 - 1) * 0x7fff / (SCREEN.0 - 1)).unwrap_or(0);
    let ay = i32::try_from(y.clamp(0, SCREEN.1 - 1) * 0x7fff / (SCREEN.1 - 1)).unwrap_or(0);
    // Over it, then a step, as a hand moves: the surface under it has
    // its enter before the press.
    qmp.input_send_event(&[absolute("x", ax - 40), absolute("y", ay)])?;
    std::thread::sleep(Duration::from_millis(300));
    qmp.input_send_event(&[absolute("x", ax), absolute("y", ay)])?;
    // A surface drawn on the CPU of an emulated machine takes its time.
    std::thread::sleep(Duration::from_secs(2));
    let button = |down: bool| {
        format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
    };
    qmp.input_send_event(&[button(true)])?;
    std::thread::sleep(Duration::from_millis(120));
    qmp.input_send_event(&[button(false)])
}

/// `test-compositor --boot stash`.
///
/// # Errors
///
/// A step that never came, or a judgement that failed, with the
/// transcript.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each press and what it must have done"
)]
pub(super) fn test_stash(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str("exec-once = /bin/sh /etc/stash-probe.sh\n");
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    carried.ports.push(crate::ports::File {
        path: "etc/stash-probe.sh".to_owned(),
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
        let wait = |watching: &mut Watching<'_>, word: &str, from: usize| -> Result<bool> {
            watching.read_more(Instant::now() + Duration::from_secs(60), |lines| {
                lines.iter().skip(from).any(|line| line.contains(word))
            })
        };
        let up = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines
                .iter()
                .any(|l| l.contains("yoru-shell: the dock is up"))
                && lines.iter().any(|l| l.contains("stash-probe: put in"))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: stash: the dock or the probe never came")),
                watching,
            ));
        }
        // The dock counts the stash each second.
        std::thread::sleep(Duration::from_secs(4));
        let before = dump(&mut qmp, arch, "before")?;
        let (drawn, middle) = badge(&before);
        println!("  {arch}: stash: the badge is {drawn} pixels at {middle:?}");
        let Some((bx, by)) = middle.filter(|_| drawn >= 40) else {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: stash: no count on the Stash button ({drawn} pixels)"
                )),
                watching,
            ));
        };
        let mut failures = Vec::new();
        // The button is below and left of its badge.
        let from = watching.after().len();
        click(&mut qmp, (bx - 9, by + 11))?;
        if !wait(watching, "the stash panel is up", from)? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: stash: the button opened no panel")),
                watching,
            ));
        }
        let said = watching
            .after()
            .iter()
            .skip(from)
            .find(|line| line.contains("the stash panel is up"))
            .cloned()
            .unwrap_or_default();
        if !said.contains(", 4 items") {
            failures.push(format!("the panel did not list four: {}", said.trim()));
        }
        std::thread::sleep(Duration::from_secs(3));
        let _ = dump(&mut qmp, arch, "panel")?;
        // The panel, 340 wide in the middle, its foot 84 above the screen's.
        let left = (SCREEN.0 - 340) / 2;
        let top = SCREEN.1 - 84 - (80 + 96 + 12);
        let from = watching.after().len();
        click(&mut qmp, (left + 14 + 36, top + 80 + 30))?;
        let _ = wait(watching, "yoru-shell: stash: open ", from)?;
        let _ = dump(&mut qmp, arch, "picked")?;
        if !watching
            .after()
            .iter()
            .skip(from)
            .any(|l| l.contains("yoru-shell: stash: open "))
        {
            failures.push("a pick on the first tile opened nothing".to_owned());
        } else if !watching
            .after()
            .iter()
            .skip(from)
            .any(|line| line.contains("stash: open ") && line.contains("plan.md"))
        {
            failures.push("the pick opened another file than the newest".to_owned());
        }
        std::thread::sleep(Duration::from_secs(2));
        // Opened, the panel goes: open it again. Where nothing opens
        // files (no `xdg-open`) it stays.
        let opened = watching
            .after()
            .iter()
            .skip(from)
            .any(|line| line.contains("stash: open ") && line.trim_end().ends_with(": ok"));
        if opened {
            let from = watching.after().len();
            click(&mut qmp, (bx - 9, by + 11))?;
            if !wait(watching, "the stash panel is up", from)? {
                failures.push("the button opened no panel the second time".to_owned());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        // The bin: the middle of the head's three buttons.
        let from = watching.after().len();
        click(&mut qmp, (left + 340 - 14 - 28 - 6 - 14, top + 28))?;
        if !wait(watching, "yoru-shell: stash: cleared", from)? {
            failures.push("the bin did not clear the stash".to_owned());
        }
        std::thread::sleep(Duration::from_secs(1));
        let from = watching.after().len();
        press(&mut qmp, &["esc"])?;
        if !wait(watching, "the stash panel closed", from)? {
            failures.push("Escape did not close the panel".to_owned());
        }
        std::thread::sleep(Duration::from_secs(4));
        let after = dump(&mut qmp, arch, "after")?;
        let (left_over, _) = badge(&after);
        println!("  {arch}: stash: after clearing the badge is {left_over} pixels");
        if left_over >= 10 {
            failures.push(format!(
                "the count is still on the button: {left_over} pixels"
            ));
        }
        if failures.is_empty() {
            println!(
                "  {arch}: stash: the count showed, the button opened four, a pick opened the newest, the bin cleared it and the count went"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: stash: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
