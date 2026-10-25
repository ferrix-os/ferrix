//! `test-compositor --boot walls`: Yoru's wallpaper switcher (the design's
//! picker, `yoru-shell --walls`) opens on SUPER I, closes on it again,
//! browses with the arrows and sets the wallpaper with Return.
//!
//! On Yoru's desktop with the Settings app's six pictures installed,
//! QEMU's keyboard presses SUPER I twice (open, then closed), then SUPER
//! I, Left and Return. A script in the guest prints the wallpaper line of
//! `settings.conf` every two seconds. The host keeps QEMU's screendumps
//! as `walls-<step>.ppm` and requires: the switcher said it was up and
//! closed on the second press; it was on the screen (the shade over the
//! top of it); Return set the picture in hand with hyprix's `ok`, kept in
//! `settings.conf`; and the desktop's ground changed to it.

use std::time::{Duration, Instant};

use super::boot::{build_image, press, with_the_transcript};
use super::{MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The guest's side: the wallpaper kept, every two seconds.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
while :; do
  echo "walls: conf $(grep '^wallpaper' ${XDG_CONFIG_HOME:-$HOME/.config}/hypr/settings.conf 2>/dev/null | tr '\n' ';')"
  sleep 2
done
"#;

/// QEMU's screendump of now, kept as `walls-<step>.ppm`.
fn dump(qmp: &mut Qmp, arch: Arch, step: &str) -> Result<Image> {
    let path = paths::build_dir(arch).join(format!("walls-{step}.ppm"));
    qmp.screendump(Some(DEVICE_ID), &path)?;
    let bytes =
        std::fs::read(&path).map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    crate::display::parse_ppm(&bytes)
}

/// How many pixels of the strip `rows` differ by more than 24 in a
/// channel between `a` and `b`.
fn changed(a: &Image, b: &Image, rows: std::ops::Range<usize>) -> usize {
    let mut count = 0;
    for y in rows {
        for x in 0..a.width.min(b.width) {
            let at = (y * a.width + x) * 3;
            if let (Some(p), Some(q)) = (a.pixels.get(at..at + 3), b.pixels.get(at..at + 3))
                && p.iter().zip(q).any(|(m, n)| m.abs_diff(*n) > 24)
            {
                count += 1;
            }
        }
    }
    count
}

/// `test-compositor --boot walls`.
///
/// # Errors
///
/// A step that never came, or a judgement that failed, with the
/// transcript.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each key and what it must have done"
)]
pub(super) fn test_walls(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str("exec-once = /bin/sh /etc/walls.sh\n");
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    carried
        .ports
        .extend(crate::apps::taken(arch, &["settings"])?);
    carried.ports.push(crate::ports::File {
        path: "etc/walls.sh".to_owned(),
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
                && lines.iter().any(|l| l.contains("walls: conf"))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: walls: the dock or the probe never came")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        let before = dump(&mut qmp, arch, "before")?;
        let mut failures = Vec::new();
        // Open, then closed by the same key.
        let from = watching.after().len();
        press(&mut qmp, &["meta_l", "i"])?;
        if !wait(watching, "the wallpaper switcher is up", from)? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: walls: SUPER I opened no switcher")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(2));
        let from = watching.after().len();
        press(&mut qmp, &["meta_l", "i"])?;
        if !wait(
            watching,
            "the wallpaper switcher was asked again: closed",
            from,
        )? {
            failures.push("SUPER I again did not close the switcher".to_owned());
        }
        std::thread::sleep(Duration::from_secs(2));
        // Open, browse one right, set it.
        let from = watching.after().len();
        press(&mut qmp, &["meta_l", "i"])?;
        if !wait(watching, "the wallpaper switcher is up", from)? {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: walls: SUPER I opened no switcher the second time"
                )),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(4));
        let open = dump(&mut qmp, arch, "open")?;
        let from = watching.after().len();
        // The one in force is Yoru's, the last: one to the left.
        press(&mut qmp, &["left"])?;
        if !wait(watching, "yoru-shell: in hand:", from)? {
            failures.push("Left moved nothing into hand".to_owned());
        }
        std::thread::sleep(Duration::from_secs(2));
        let from = watching.after().len();
        press(&mut qmp, &["ret"])?;
        if !wait(watching, "yoru-shell: wallpaper set:", from)? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: walls: Return set no wallpaper")),
                watching,
            ));
        }
        let set_line = watching
            .after()
            .iter()
            .skip(from)
            .find(|line| line.contains("yoru-shell: wallpaper set:"))
            .cloned()
            .unwrap_or_default();
        println!("  {arch}: walls: {}", set_line.trim());
        if !set_line.trim_end().ends_with(": ok") {
            failures.push(format!("hyprix did not take it: {}", set_line.trim()));
        }
        let path = set_line
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(path, _)| path.to_owned())
            .unwrap_or_default();
        let from = watching.after().len();
        let kept = !path.is_empty()
            && watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
                lines
                    .iter()
                    .skip(from)
                    .any(|line| line.contains("walls: conf") && line.contains(&path))
            })?;
        if !kept {
            failures.push(format!("settings.conf does not keep {path}"));
        }
        std::thread::sleep(Duration::from_secs(4));
        let after = dump(&mut qmp, arch, "after")?;
        // The switcher's shade over the top of the screen, and its title.
        let shown = changed(&before, &open, 20..60);
        println!("  {arch}: walls: the switcher changed {shown} pixels of the top strip");
        if shown < before.width * 4 {
            failures.push(format!(
                "the switcher was not on the screen: {shown} pixels changed"
            ));
        }
        // The ground in the strip above the dock, which no window covers.
        let rows = before.height.saturating_sub(140)..before.height.saturating_sub(80);
        let ground = changed(&before, &after, rows);
        println!("  {arch}: walls: the wallpaper changed {ground} pixels above the dock");
        if ground < before.width * 10 {
            failures.push(format!(
                "the desktop's ground did not change: {ground} pixels"
            ));
        }
        if failures.is_empty() {
            println!(
                "  {arch}: walls: SUPER I opened and closed the switcher, Left and Return set the picture before, kept and drawn"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: walls: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
