//! `test-compositor --boot edge-drag`: a window dragged by its title bar to
//! the screen's edge snaps there, and its title bar still takes the next
//! press and drag (the customer on the TV, 2026-10-25: after a drag to the
//! edge "the window's title bar stops responding to clicks and drags").
//!
//! On Yoru's desktop, QEMU's tablet presses on the bar of the window on top
//! (the two open cascaded, either first), drags it to
//! the left edge and lets go; then presses on the bar where the snapped
//! window now draws it and drags it right. A script in the guest prints
//! `hyprctl clients` every second. Required: the window snapped to the left
//! half, and the second drag moved it out again.

use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, with_the_transcript};
use super::{MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::paths::Arch;
use crate::qemu::Watching;
use crate::{Error, Result};

/// The screen, as the Yoru boot sets it.
const SCREEN: (i64, i64) = (1024, 768);

/// How far above a window's rectangle the middle of its bar is.
const BAR_MIDDLE: i64 = 19;

/// The guest's side: where every window is, once a second.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
while :; do
  echo "edge: clients $(hyprctl clients | grep -E 'title:|at:|size:' | tr '\n' ' ' | tr '\t' ' ')"
  sleep 1
done
"#;

fn button(down: bool) -> String {
    format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
}

fn at(qmp: &mut Qmp, (x, y): (i64, i64)) -> Result<()> {
    let ax = i32::try_from(x.clamp(0, SCREEN.0 - 1) * 0x7fff / (SCREEN.0 - 1)).unwrap_or(0);
    let ay = i32::try_from(y.clamp(0, SCREEN.1 - 1) * 0x7fff / (SCREEN.1 - 1)).unwrap_or(0);
    qmp.input_send_event(&[absolute("x", ax), absolute("y", ay)])
}

/// Press at `from`, move in steps to `to`, let go.
fn drag(qmp: &mut Qmp, from: (i64, i64), to: (i64, i64)) -> Result<()> {
    at(qmp, from)?;
    std::thread::sleep(Duration::from_millis(200));
    qmp.input_send_event(&[button(true)])?;
    std::thread::sleep(Duration::from_millis(150));
    for step in 1..=20 {
        let x = from.0 + (to.0 - from.0) * step / 20;
        let y = from.1 + (to.1 - from.1) * step / 20;
        at(qmp, (x, y))?;
        std::thread::sleep(Duration::from_millis(40));
    }
    std::thread::sleep(Duration::from_millis(300));
    qmp.input_send_event(&[button(false)])?;
    Ok(())
}

/// Window `title`'s rectangle in the last clients line after `from`.
fn rect_of(watching: &Watching<'_>, from: usize, title: &str) -> Option<(i64, i64, i64, i64)> {
    let line = watching
        .after()
        .iter()
        .skip(from)
        .rev()
        .find(|line| line.contains("edge: clients"))?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut at = None;
    let mut size = None;
    for (i, word) in words.iter().enumerate() {
        match *word {
            "at:" => at = words.get(i + 1).copied(),
            "size:" => size = words.get(i + 1).copied(),
            "title:" if words.get(i + 1) == Some(&title) => {
                let pair = |text: &str| {
                    let (a, b) = text.split_once(',')?;
                    Some((a.parse().ok()?, b.parse().ok()?))
                };
                let (x, y) = pair(at?)?;
                let (w, h) = pair(size?)?;
                return Some((x, y, w, h));
            }
            _ => {}
        }
    }
    None
}

/// `test-compositor --boot edge-drag`.
///
/// # Errors
///
/// The window did not snap, or its bar took no second drag.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: two drags and what each left"
)]
pub(super) fn test_edge_drag(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str("exec-once = /bin/sh /etc/edge-drag.sh\n");
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    carried.ports.push(crate::ports::File {
        path: "etc/edge-drag.sh".to_owned(),
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
                .any(|l| l.contains("edge: clients") && l.contains("title: two"))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: edge-drag: window two never came")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        let wait = |watching: &mut Watching<'_>| -> Result<usize> {
            let from = watching.after().len();
            let _ = watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
                lines
                    .iter()
                    .skip(from)
                    .filter(|l| l.contains("edge: clients"))
                    .count()
                    >= 4
            })?;
            Ok(from)
        };
        let from = wait(watching)?;
        // The window whose bar no other window covers: the one on top. The
        // two open cascaded, in either order.
        let one = rect_of(watching, from, "one");
        let two = rect_of(watching, from, "two");
        let bar = |(x, y, w, _): (i64, i64, i64, i64)| (x + w / 2, y - BAR_MIDDLE);
        let inside = |(px, py): (i64, i64), (x, y, w, h): (i64, i64, i64, i64)| {
            px >= x && px < x + w && py >= y - 2 * BAR_MIDDLE && py < y + h
        };
        let Some((title, (x, y, w, _))) = [("two", two, one), ("one", one, two)]
            .into_iter()
            .find_map(|(title, own, other)| {
                let own = own?;
                (!other.is_some_and(|other| inside(bar(own), other))).then_some((title, own))
            })
        else {
            return Err(with_the_transcript(
                &Error::new("edge-drag: no window with its bar uncovered"),
                watching,
            ));
        };
        println!("  {arch}: edge-drag: {title} at {x},{y} width {w}");
        let dump = crate::paths::build_dir(arch).join("edge-drag-0.ppm");
        qmp.screendump(Some(crate::display::DEVICE_ID), &dump)?;
        drag(&mut qmp, (x + w / 2, y - BAR_MIDDLE), (0, 300))?;
        let from = wait(watching)?;
        let Some((sx, sy, sw, sh)) = rect_of(watching, from, title) else {
            return Err(with_the_transcript(
                &Error::new("edge-drag: no rectangle after the snap"),
                watching,
            ));
        };
        println!("  {arch}: edge-drag: after the drag to the edge {title} at {sx},{sy} {sw}x{sh}");
        let dump = crate::paths::build_dir(arch).join("edge-drag-1.ppm");
        qmp.screendump(Some(crate::display::DEVICE_ID), &dump)?;
        let snapped = sx <= 8 && sw >= SCREEN.0 / 2 - 40 && sw <= SCREEN.0 / 2 + 40;
        // Let go at the left edge, the pointer opened Yoru's left sidebar
        // over the snapped window's bar: away from the edge, it closes.
        at(&mut qmp, (SCREEN.0 / 2 + 100, SCREEN.1 / 2))?;
        std::thread::sleep(Duration::from_secs(2));
        drag(
            &mut qmp,
            (sx + sw / 2, sy - BAR_MIDDLE),
            (sx + sw / 2 + 250, sy + 120),
        )?;
        let from = wait(watching)?;
        let Some((ax, ay, aw, ah)) = rect_of(watching, from, title) else {
            return Err(with_the_transcript(
                &Error::new("edge-drag: no rectangle after the second drag"),
                watching,
            ));
        };
        println!("  {arch}: edge-drag: after the second drag {title} at {ax},{ay} {aw}x{ah}");
        let mut failures = Vec::new();
        if !snapped {
            failures.push(format!("a window dragged to the left edge did not snap to the left half: {sx},{sy} {sw}x{sh}"));
        }
        if (ax, ay) == (sx, sy) {
            failures.push(format!(
                "the title bar took no drag after the snap: still at {ax},{ay}"
            ));
        }
        if failures.is_empty() {
            println!(
                "  {arch}: edge-drag: the window snapped to the left half and its bar took the next drag"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: edge-drag: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
