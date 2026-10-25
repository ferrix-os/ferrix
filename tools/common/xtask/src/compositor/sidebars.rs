//! `test-compositor --boot sidebars`: Yoru's widget sidebars (Y4). The
//! pointer at the left edge opens the left one, on its clock; away from
//! it, it closes; the right one takes a task typed into its to-do list and
//! keeps it in the person's file; its pin keeps it open.
//!
//! The host drives QEMU's tablet and keyboard; a script in the guest says
//! the to-do file every two seconds. QEMU's
//! screendumps are kept as `sidebars-<step>.ppm`.

use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, press, with_the_transcript};
use super::{MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The screen, as the Yoru boot sets it.
const SCREEN: (i64, i64) = (1024, 768);

/// The guest's side.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
while :; do
  echo "side: todo $(cat ${XDG_DATA_HOME:-$HOME/.local/share}/yoru-shell/todo 2>/dev/null | tr '\n' ';')"
  sleep 2
done
"#;

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

/// `test-compositor --boot sidebars`.
///
/// # Errors
///
/// A step that never came, with the transcript.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each move and what it must have done"
)]
pub(super) fn test_sidebars(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str("exec-once = /bin/sh /etc/sidebars.sh\n");
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    carried.ports.push(crate::ports::File {
        path: "etc/sidebars.sh".to_owned(),
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
            [
                "the left sidebar is up",
                "the right sidebar is up",
                "side: todo",
            ]
            .iter()
            .all(|word| lines.iter().any(|l| l.contains(word)))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: sidebars: the sidebars or the probe never came"
                )),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        let mut failures = Vec::new();
        let dump = |qmp: &mut Qmp, step: &str| -> Result<()> {
            let path = paths::build_dir(arch).join(format!("sidebars-{step}.ppm"));
            qmp.screendump(Some(DEVICE_ID), &path)
        };
        // The left edge opens the left sidebar; away, it closes.
        let from = watching.after().len();
        at(&mut qmp, (300, 300))?;
        std::thread::sleep(Duration::from_millis(500));
        at(&mut qmp, (0, 300))?;
        if !wait(watching, "the left sidebar opened", from)? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: sidebars: the left edge opened nothing")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        dump(&mut qmp, "left")?;
        let from = watching.after().len();
        at(&mut qmp, (700, 300))?;
        if !wait(watching, "the left sidebar closed", from)? {
            failures.push("the left sidebar stayed open with the pointer away".to_owned());
        }
        // The right one: its to-do list, a task typed in.
        let from = watching.after().len();
        at(&mut qmp, (SCREEN.0 - 1, 300))?;
        if !wait(watching, "the right sidebar opened", from)? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: sidebars: the right edge opened nothing")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        // The field under the empty list, 336 from the right edge.
        let left = SCREEN.0 - 336;
        click(&mut qmp, (left + 8 + 16 + 100, 8 + 90 + 30 + 17))?;
        std::thread::sleep(Duration::from_secs(1));
        for key in ["m", "i", "l", "k"] {
            press(&mut qmp, &[key])?;
            std::thread::sleep(Duration::from_millis(150));
        }
        let from = watching.after().len();
        press(&mut qmp, &["ret"])?;
        let typed = watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
            lines
                .iter()
                .skip(from)
                .any(|line| line.contains("side: todo") && line.contains("[ ] milk"))
        })?;
        if !typed {
            failures.push("a task typed into the to-do list was not kept".to_owned());
        }
        dump(&mut qmp, "todo")?;
        // The pin: the column stays, and the windows give it room.
        let from = watching.after().len();
        click(&mut qmp, (left + 8 + 320 - 12 - 15, 8 + 12 + 15))?;
        if !wait(watching, "Pin", from)? {
            failures.push("the pin took no press".to_owned());
        }
        at(&mut qmp, (300, 300))?;
        std::thread::sleep(Duration::from_secs(6));
        let from = watching.after().len();
        let _ = watching.read_more(Instant::now() + Duration::from_secs(10), |lines| {
            lines
                .iter()
                .skip(from)
                .any(|line| line.contains("side: todo"))
        })?;
        if watching
            .after()
            .iter()
            .any(|line| line.contains("the right sidebar closed"))
        {
            failures.push("the pinned sidebar closed".to_owned());
        }
        dump(&mut qmp, "pinned")?;
        if failures.is_empty() {
            println!(
                "  {arch}: sidebars: the edges opened them, away the left closed, a task typed in was kept, the pin kept the right open"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: sidebars: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
