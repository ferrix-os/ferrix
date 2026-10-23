//! `test-compositor --boot kept-settings`: what the Settings app sets
//! takes at once and is still there after hyprix starts again
//! (`docs/RUNTIME-SETTINGS.md` §3.3, `hyprix/src/settings.rs`).
//!
//! A script hyprix starts does what the app does, through `hyprctl`: a key
//! repeat keyword, then `hyprctl setting` of the gaps at the screen's edge
//! -- the window moves at once -- and of a wallpaper line, which hyprix
//! hands the wallpaper client. It locks the screen with a see-through lock
//! twice: the first time the wallpaper must show under it
//! (`hyprix:wallpaper_lock`, on by default), the second time, after
//! `hyprctl setting hyprix:wallpaper_lock 0`, the screen under the lock
//! show none of it. Then it starts hyprix again -- a second, headless one
//! with no configuration of its own -- and reads the options back from it,
//! and that hyprix must start the wallpaper client with the kept line by
//! itself. (`svc restart hyprix.service` is what a person would do, but on
//! these images the restarted unit is refused `/dev/dri`.)

use std::time::{Duration, Instant};

use super::boot::{build_image, undithered, with_the_transcript};
use super::{Carried, FAILED, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// Where the script goes.
const SCRIPT_PATH: &str = "etc/kept-settings.sh";

/// The wallpaper's colour, `#203040`.
const COLOUR: (u8, u8, u8) = (0x20, 0x30, 0x40);

/// The compositor's configuration: the script, and nothing of the
/// settings, so that what the restarted compositor has is the kept file's.
fn config() -> String {
    format!(
        "# Carried by `cargo xtask test-compositor --boot kept-settings`.\n\
         exec-once = /bin/sh /{SCRIPT_PATH}\n"
    )
}

/// What the Settings app does, as a script; then a hyprix started again.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
/bin/pattern checkerboard one &
until hyprctl clients | grep -q 'title: one'; do sleep 0.2; done
echo "kept: before $(hyprctl clients | grep -m1 'at: ')"
echo "kept: repeat $(hyprctl keyword input:repeat_rate 40)"
echo "kept: gaps $(hyprctl setting general:gaps_out 60)"
sleep 1
echo "kept: after $(hyprctl clients | grep -m1 'at: ')"
echo "kept: wallpaper $(hyprctl setting wallpaper ', colour 203040')"
sleep 3
echo "kept: locking under"
/bin/lock --clear 10
echo "kept: lock-off $(hyprctl setting hyprix:wallpaper_lock 0)"
echo "kept: locking dark"
/bin/lock --clear 10
echo "kept: restarting"
# A second hyprix, started now with no configuration of its own, as a
# restarted one is: what it has of the settings is the kept file's.
/bin/hyprix --headless 640x480 --display /tmp/kept-second --instance kept-second \
    --deadline 30000 &
export HYPRLAND_INSTANCE_SIGNATURE=kept-second
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
echo "kept: restarted gaps_out $(hyprctl getoption general:gaps_out | head -1)"
echo "kept: restarted lock $(hyprctl getoption hyprix:wallpaper_lock | head -1)"
echo "kept: restarted settings $(hyprctl settings | tr '\n' ';')"
echo "kept: done"
"#;

/// The boot.
pub(super) fn test_kept_settings(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(busybox) = super::gates_busybox(arch) else {
        println!("  {arch}: no busybox on this machine for the boot's script; skipped");
        return Ok(());
    };
    let carried = Carried {
        busybox: Some(std::path::PathBuf::from(busybox)),
        ports: vec![crate::ports::File {
            path: SCRIPT_PATH.to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(SCRIPT.as_bytes().to_vec()),
        }],
        ..Carried::none()
    };
    let (image, kernel) = build_image(arch, programs, &undithered(&config()), carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("kept-settings.ppm");
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let mut judge = Judge {
            arch,
            watching,
            qmp: &mut qmp,
            dump: &dump,
        };
        judge.live()?;
        judge.locks()?;
        judge.restarted()
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}

/// What the boot's hook holds while it judges each step.
struct Judge<'w, 'g> {
    arch: Arch,
    watching: &'w mut Watching<'g>,
    qmp: &'w mut Qmp,
    dump: &'w std::path::Path,
}

impl Judge<'_, '_> {
    /// A failure, with the transcript.
    fn fail(&self, why: &str) -> Error {
        with_the_transcript(&Error::new(format!("{}: {why}", self.arch)), self.watching)
    }

    /// Every line read so far.
    fn said(&self) -> impl Iterator<Item = &String> {
        self.watching.lines().iter().chain(self.watching.after())
    }

    /// How many lines so far hold `what`.
    fn seen(&self, what: &str) -> usize {
        self.said().filter(|line| line.contains(what)).count()
    }

    /// What follows `prefix` on the first line that has it.
    fn after(&self, prefix: &str) -> String {
        self.said()
            .find_map(|line| line.split(prefix).nth(1).map(|rest| rest.trim().to_owned()))
            .unwrap_or_default()
    }

    /// Read until `times` lines hold `what`, or the compositor failed.
    fn wait(&mut self, what: &str, times: usize) -> Result<bool> {
        self.watching
            .read_more(Instant::now() + SETTLE * 2, |lines| {
                lines.iter().filter(|line| line.contains(what)).count() >= times
                    || lines.iter().any(|line| line.contains(FAILED))
            })
    }

    /// A keyword and a setting take at once: repeat, then the gaps at the
    /// screen's edge, which move the window.
    fn live(&mut self) -> Result<()> {
        if !self.wait("kept: after ", 1)? {
            return Err(self.fail("the script never changed the gaps"));
        }
        if self.seen("hyprix: key repeat 40 a second") == 0 {
            return Err(self.fail("`keyword input:repeat_rate 40` was not put in force"));
        }
        let (before, after) = (self.after("kept: before "), self.after("kept: after "));
        let left = |text: &str| {
            text.split("at: ")
                .nth(1)
                .and_then(|rest| rest.split(',').next())
                .and_then(|x| x.trim().parse::<i32>().ok())
        };
        let moved = matches!(
            (left(&before), left(&after)),
            (Some(was), Some(now)) if was < 60 && now >= 60
        );
        if self.seen("kept: gaps ok") == 0 || !moved {
            return Err(self.fail(&format!(
                "`setting general:gaps_out 60` did not move the window: {before} then {after}"
            )));
        }
        println!(
            "  {}: a keyword and a setting took at once: the window {before} then {after}",
            self.arch
        );
        Ok(())
    }

    /// The wallpaper line, handed to the client, shows under the first
    /// lock and not under the second, once `hyprix:wallpaper_lock` is 0.
    fn locks(&mut self) -> Result<()> {
        if !self.wait("hyprix: the session is locked", 1)? {
            return Err(self.fail("the first lock never held"));
        }
        if self.seen("wallpaper: colour ff203040 drawn at") == 0 {
            return Err(self.fail("the wallpaper line was never drawn"));
        }
        let under = settle(self.qmp, self.dump, |screen| share(screen, COLOUR) > 0.9)
            .map_err(|why| self.fail(&format!("under the lock: {why}")))?;
        println!(
            "  {}: the kept wallpaper shows under a see-through lock ({:.0}% of the screen)",
            self.arch,
            under * 100.0
        );
        if !self.wait("hyprix: the session is locked", 2)? {
            return Err(self.fail("the second lock never held"));
        }
        if self.seen("kept: lock-off ok") == 0 {
            return Err(self.fail("`setting hyprix:wallpaper_lock 0` was refused"));
        }
        let _ = settle(self.qmp, self.dump, |screen| share(screen, COLOUR) < 0.01)
            .map_err(|why| self.fail(&format!("with the wallpaper off the lock: {why}")))?;
        println!(
            "  {}: with hyprix:wallpaper_lock 0 the wallpaper is gone from under the lock",
            self.arch
        );
        Ok(())
    }

    /// A hyprix started again has the options and the wallpaper back from
    /// the file.
    fn restarted(&mut self) -> Result<()> {
        if !self.wait("kept: done", 1)? {
            return Err(self.fail("the restarted compositor never answered the script"));
        }
        let gaps = self.after("kept: restarted gaps_out ");
        let lock = self.after("kept: restarted lock ");
        let settings = self.after("kept: restarted settings ");
        let started = self
            .said()
            .filter(|line| line.contains("hyprix: wallpaper ") && line.contains("started as"))
            .count();
        if !gaps.contains("60") || !lock.starts_with("int: 0") || started < 2 {
            return Err(self.fail(&format!(
                "after the restart: gaps_out {gaps:?}, wallpaper_lock {lock:?}, wallpaper \
                 started {started} times (two wanted)"
            )));
        }
        println!(
            "  {}: after hyprix restarted the kept settings are back: {settings}",
            self.arch
        );
        Ok(())
    }
}

/// Take screendumps until `good` holds of one, for [`SETTLE`]; gives the
/// share [`share`] measured of the last.
fn settle(
    qmp: &mut Qmp,
    dump: &std::path::Path,
    good: impl Fn(&Image) -> bool,
) -> core::result::Result<f64, String> {
    let deadline = Instant::now() + SETTLE;
    loop {
        qmp.screendump(Some(DEVICE_ID), dump)
            .map_err(|error| error.to_string())?;
        let bytes = std::fs::read(dump).map_err(|error| format!("{}: {error}", dump.display()))?;
        let screen = parse_ppm(&bytes).map_err(|error| error.to_string())?;
        if good(&screen) {
            return Ok(share(&screen, COLOUR));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the screen ({}) is {:.0}% the wallpaper's colour",
                dump.display(),
                share(&screen, COLOUR) * 100.0,
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// How much of `screen` is `colour`, give or take two steps a channel.
fn share(screen: &Image, colour: (u8, u8, u8)) -> f64 {
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    let pixels = screen.pixels.chunks_exact(3);
    let total = pixels.len().max(1);
    let matching = screen
        .pixels
        .chunks_exact(3)
        .filter(|pixel| {
            matches!(pixel, [r, g, b] if near(*r, colour.0) && near(*g, colour.1) && near(*b, colour.2))
        })
        .count();
    matching as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_counts_the_colour() {
        let screen = Image {
            width: 2,
            height: 1,
            pixels: vec![0x20, 0x30, 0x41, 0, 0, 0],
        };
        assert!((share(&screen, COLOUR) - 0.5).abs() < 1e-9);
        assert!((share(&screen, (0, 0, 0)) - 0.5).abs() < 1e-9);
    }
}
