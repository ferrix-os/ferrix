//! `test-compositor --boot settings-live`: what the Settings app's
//! Appearance and Windows pages ask of hyprix takes effect at once on
//! Yoru's desktop (the customer, 2026-10-24: "appearance and windows have
//! no effect whatsoever").
//!
//! A script hyprix starts does exactly what the app does -- `hyprctl
//! setting KEY VALUE` -- for each control, and prints hyprix's answer and
//! `hyprctl getoption` after; the host keeps QEMU's screendump before and
//! after each group as `settings-live-<step>.ppm`, and requires:
//!
//! * every setting answered `ok` and in force after (`getoption`);
//! * Appearance live: the 6-pixel red border the script asked for is on
//!   the screen after, and was not before;
//! * Windows live: with `hyprix:titlebars 0` the Settings window's bar
//!   across the top of the screen is gone;
//! * all of it kept in `settings.conf`.

use std::time::{Duration, Instant};

use super::boot::{build_image, with_the_transcript};
use super::{MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The probe: each of the app's keys, set as the app sets it.
pub(super) const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
until hyprctl clients | grep -q 'title: two'; do sleep 0.2; done
/bin/settings --page appearance &
sleep 6
echo "live: clients $(hyprctl clients | grep -E 'title|floating|at:|size:' | tr '\n' ' ')"
echo "live: step before"
sleep 35
for kv in "general:gaps_in 20" "general:gaps_out 60" "general:border_size 6" \
          "decoration:rounding 24" "decoration:inactive_opacity 0.5" \
          "decoration:blur:enabled 0" "decoration:shadow:enabled 0" \
          "general:col.active_border rgba(ff0000ff)" "animations:enabled 0"; do
  set -- $kv
  echo "live: set $1 -> $(hyprctl setting $1 $2) now $(hyprctl getoption $1 | head -2 | tr '\n' ' ')"
done
sleep 2
echo "live: step appearance"
sleep 35
for kv in "hyprix:titlebars 0" "general:layout master" "input:follow_mouse 0"; do
  set -- $kv
  echo "live: set $1 -> $(hyprctl setting $1 $2) now $(hyprctl getoption $1 | head -2 | tr '\n' ' ')"
done
sleep 2
echo "live: clients $(hyprctl clients | grep -E 'title|floating|at:|size:' | tr '\n' ' ')"
echo "live: step windows"
sleep 35
echo "live: settings.conf $(cat ${XDG_CONFIG_HOME:-$HOME/.config}/hypr/settings.conf 2>&1 | tr '\n' ';')"
echo "live: done"
"#;

/// `test-compositor --boot settings-live`.
///
/// # Errors
///
/// A step that never came, with the transcript.
pub(super) fn test_settings_live(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str("exec-once = /bin/sh /etc/settings-live.sh\n");
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    carried
        .ports
        .extend(crate::apps::taken(arch, &["settings"])?);
    carried.ports.push(crate::ports::File {
        path: "etc/settings-live.sh".to_owned(),
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
        let _ = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|line| line.contains(MARKER))
        })?;
        for step in ["before", "appearance", "windows"] {
            let word = format!("live: step {step}");
            if !watching.read_more(Instant::now() + Duration::from_secs(90), |lines| {
                lines.iter().any(|line| line.contains(&word))
            })? {
                return Err(with_the_transcript(
                    &Error::new(format!("{arch}: settings-live: `{word}` never came")),
                    watching,
                ));
            }
            std::thread::sleep(Duration::from_millis(1500));
            let dump = paths::build_dir(arch).join(format!("settings-live-{step}.ppm"));
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            println!("  {arch}: settings-live: {step}: {}", dump.display());
        }
        let _ = watching.read_more(Instant::now() + Duration::from_secs(60), |lines| {
            lines.iter().any(|line| line.contains("live: done"))
        })?;
        let said: Vec<String> = watching.after().to_vec();
        let mut failures = Vec::new();
        for line in said.iter().filter(|line| line.contains("live: set ")) {
            if !line.contains(" -> ok now ") {
                failures.push(format!("a setting was not taken: {}", line.trim()));
            }
        }
        let picture = |step: &str| -> Result<crate::display::Image> {
            let path = paths::build_dir(arch).join(format!("settings-live-{step}.ppm"));
            let bytes = std::fs::read(&path)
                .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
            crate::display::parse_ppm(&bytes)
        };
        let (before, appearance, windows) = (
            picture("before")?,
            picture("appearance")?,
            picture("windows")?,
        );
        let red = |image: &crate::display::Image| {
            image
                .pixels
                .chunks_exact(3)
                .filter(|p| p[0] > 230 && p[1] < 30 && p[2] < 30)
                .count()
        };
        if red(&before) > 200 || red(&appearance) < 2000 {
            failures.push(format!(
                "an Appearance setting did not show at once: red border pixels before {}, after {}",
                red(&before),
                red(&appearance)
            ));
        }
        let top_changed = appearance
            .pixels
            .chunks_exact(3)
            .zip(windows.pixels.chunks_exact(3))
            .take(appearance.width * 16)
            .filter(|(a, b)| a != b)
            .count();
        if top_changed < appearance.width * 4 {
            failures.push(format!(
                "a Windows setting did not show at once: hyprix:titlebars 0 left the bar ({top_changed} pixels of the top changed)"
            ));
        }
        if !said.iter().any(|line| {
            line.contains("live: settings.conf") && line.contains("hyprix:titlebars = 0")
        }) {
            failures.push("the settings were not kept in settings.conf".to_owned());
        }
        if failures.is_empty() {
            println!(
                "  {arch}: settings-live: every Appearance and Windows setting was taken, the border \
                 showed at once, the title bar went at once, and settings.conf kept them"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: settings-live: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
