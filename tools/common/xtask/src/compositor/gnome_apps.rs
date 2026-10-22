//! `test-gnome-apps`: GTK 4 and libadwaita apps from Debian 13 as clients of
//! hyprix, in a session of the user `ferrix` (`docs/GNOME.md`, stage G1).
//!
//! # The volume
//!
//! `tools/common/fetch/fetch-gnome-apps.sh` makes it: GTK 4, libadwaita and
//! their demos, Mesa and a session bus, every Debian package pinned by its
//! SHA-256 at one snapshot, unpacked without maintainer scripts, refused on
//! a deny-list and on any set-id file (the certification consultant's G1
//! conditions, ledger lines 3100 and 3123). It is the boot's one data disk,
//! at `/data`, which `data-nosuid.service` remounts `nosuid,nodev` before
//! the compositor's unit may start ([`crate::session::data_nosuid`]). Its
//! glibc runs its programs, linked where they look for it as the rustc
//! volume's is ([`crate::rustc::DEFAULT_LINKS`]).
//!
//! # The session
//!
//! As `run-compositor --everything` runs it: `sessiond` starts hyprix as
//! `ferrix`, and with it -- in the same scope, under the same rules -- the
//! volume's `dbus-daemon` as the session's bus, with Ferrix's own
//! configuration ([`crate::session::SESSION_BUS_CONF`]). No system bus is
//! started. The card is the plain one, as `test-claude-desktop`'s: QEMU
//! cannot take a screendump of a virgl card's scanout, and the window is
//! judged from one, so GTK draws with Mesa's software rasterizer here. GTK
//! on virgl under hyprix and under mutter is what `test-display --gl`'s
//! render checks and gaps 1 and 2 stand for.
//!
//! # What is required
//!
//! hyprix's `exec-once` starts `adwaita-1-demo` and a probe of `ferrix`'s
//! ([`PROBE`]). Once the app's window is listed, the probe says what runs
//! and as whom, and the gate requires:
//!
//! * `/data`'s line in `/proc/mounts` with `nosuid` and `nodev`;
//! * `sessiond` saying it read the bus's address,
//!   `unix:path=/run/user/1000/bus`, and the socket being `ferrix`'s there;
//! * `dbus-daemon` running as `ferrix` on that address and no
//!   `--system` one, and no system bus socket;
//! * the volume's files root's: its `dbus-daemon` `0 0 755`, and a write to
//!   it by `ferrix` refused;
//! * the app running as `ferrix`, connected to the bus -- a connection the
//!   bus says is `ferrix`'s and the app's pid -- and its window -- class
//!   `org.gnome.Adwaita1.Demo` -- drawn: [`DRAWN_COLOURS`] colours or more
//!   in it on the screen;
//! * then the probe kills the compositor, as a crashed one would end, and a
//!   watcher of root's ([`AFTER`]) finds no `dbus-daemon` and no app left
//!   once the session has ended.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{build_desktop_image, say_the_marker, with_the_transcript};
use super::desktop::{Backdrop, desktop};
use super::run::DESKTOP_DEFAULTS;
use super::{EITHER, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The app's class, its `GApplication` id.
const APP_ID: &str = "org.gnome.Adwaita1.Demo";

/// The volume's session bus daemon.
const DAEMON: &str = "/data/usr/bin/dbus-daemon";

/// The account's bus socket.
const SOCKET: &str = "/run/user/1000/bus";

/// What every line the probe and the watcher print starts with.
const SAID: &str = "gnome-gate: ";

/// The probe, as `ferrix`: waits for the app's window, says what runs and
/// as whom, holds still for the screen, then kills the compositor.
const PROBE: &str = r#"#!/bin/sh
# `cargo xtask test-gnome-apps` (docs/GNOME.md, G1), as the session's user.
export PATH=/bin
say() { echo "gnome-gate: $*"; }
named() {
    for dir in /proc/[0-9]*; do
        [ "$(cat $dir/comm 2>/dev/null)" = "$1" ] && echo ${dir#/proc/}
    done
}
uid_of() { awk '/^Uid:/ { print $2 }' /proc/$1/status; }
n=0
until /bin/hyprctl clients 2>/dev/null | grep -q "class: org.gnome.Adwaita1.Demo"; do
    n=$((n + 1))
    if [ $n -gt 300 ]; then
        say "no window of adwaita-1-demo in 300 s; hyprctl says:"
        /bin/hyprctl clients 2>&1 | head -40 | sed 's/^/gnome-gate: hyprctl: /'
        break
    fi
    sleep 1
done
sleep 3
/bin/hyprctl clients | awk '
    /^\tat: / { at = $2 }
    /^\tsize: / { size = $2 }
    /^\tclass: / { class = $2 }
    /^\ttitle: / { sub(/^\ttitle: /, ""); print "gnome-gate: window " at " " size " " class " " $0 }'
for p in $(named dbus-daemon); do
    say "process dbus-daemon uid $(uid_of $p): $(tr '\0' ' ' < /proc/$p/cmdline)"
done
for p in $(named adwaita-1-demo); do say "process adwaita-1-demo uid $(uid_of $p) pid $p"; done
say "bus socket: $(stat -c '%u %F' $XDG_RUNTIME_DIR/bus 2>&1) at $XDG_RUNTIME_DIR/bus"
say "system bus socket: $(ls /run/dbus/system_bus_socket 2>&1)"
say "daemon file: $(stat -c '%u %g %a' /data/usr/bin/dbus-daemon)"
if ( : >> /data/usr/bin/dbus-daemon ) 2>/dev/null; then
    say "write by ferrix: allowed"
else
    say "write by ferrix: refused"
fi
names=$(/data/usr/bin/dbus-send --session --print-reply --dest=org.freedesktop.DBus / \
    org.freedesktop.DBus.ListNames 2>&1 | grep -o '"[^"]*"' | tr '\n' ' ')
say "bus names: $names"
ask() {
    /data/usr/bin/dbus-send --session --print-reply --dest=org.freedesktop.DBus / \
        org.freedesktop.DBus.$1 string:$2 2>&1 | awk '/uint32/ { print $2 }'
}
for name in $names; do
    name=${name#\"}; name=${name%\"}
    case $name in :*) say "bus connection $name uid $(ask GetConnectionUnixUser $name) pid $(ask GetConnectionUnixProcessID $name)" ;; esac
done
say "ready for the screen"
sleep 20
say "killing the compositor"
kill -9 $(named hyprix)
"#;

/// Where [`PROBE`] is in the image.
const PROBE_PATH: &str = "etc/gnome-gate/probe.sh";

/// The watcher, as root, a unit of its own: waits for the compositor to
/// have come and gone, then for the session's end, and lists what of the
/// session's bus and app is left.
const AFTER: &str = r#"#!/bin/sh
# `cargo xtask test-gnome-apps` (docs/GNOME.md, G1), as root.
named() {
    for dir in /proc/[0-9]*; do
        [ "$(cat $dir/comm 2>/dev/null)" = "$1" ] && echo ${dir#/proc/}
    done
}
while [ -z "$(named hyprix)" ]; do sleep 1; done
while [ -n "$(named hyprix)" ]; do sleep 1; done
sleep 5
left=
for name in dbus-daemon adwaita-1-demo; do
    for p in $(named $name); do left="$left $name:$p"; done
done
echo "gnome-gate: after the end:${left:- none}"
"#;

/// Where [`AFTER`] is in the image.
const AFTER_PATH: &str = "etc/gnome-gate/after.sh";

/// The watcher's unit.
const AFTER_UNIT: &str = "# `cargo xtask test-gnome-apps`'s watcher of the session's end, root's.\n\
     [Unit]\n\
     Description=What of the session is left after its end\n\
     \n\
     [Service]\n\
     ExecStart=/bin/busybox sh /etc/gnome-gate/after.sh\n\
     StandardOutput=console\n\
     StandardError=console\n";

/// fontconfig's file for the volume's fonts, which the volume's own
/// `fonts.conf` names by paths Ferrix's root does not have.
const FONTS: &str = r#"<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>/data/usr/share/fonts</dir>
  <cachedir prefix="xdg">fontconfig</cachedir>
</fontconfig>
"#;

/// Where [`FONTS`] is in the image.
const FONTS_PATH: &str = "etc/gnome-gate/fonts.conf";

/// The least number of colours the app's window shows when drawn: a
/// window the app never drew into is one or two.
const DRAWN_COLOURS: usize = 32;

/// Seconds for the boot.
const TIMEOUT: u64 = 900;

/// Guest memory.
const MEMORY: u32 = 4096;

/// How long from the compositor's marker to the watcher's last line.
const PATIENCE: Duration = Duration::from_secs(480);

/// Where the fetch script writes, unless `FERRIX_GNOME_APPS_VOLUME` names
/// another directory.
fn directory() -> Result<PathBuf> {
    match std::env::var_os("FERRIX_GNOME_APPS_VOLUME") {
        Some(directory) => Ok(PathBuf::from(directory)),
        None => paths::volume_directory("gnome-apps"),
    }
}

/// The volume's image, once the fetch script's stamp says it is whole.
///
/// # Errors
///
/// It has not been fetched.
pub(crate) fn volume() -> Result<PathBuf> {
    let directory = directory()?;
    let image = directory.join("gnome-apps.img");
    if !directory.join("gnome-apps.stamp").is_file() || !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-gnome-apps.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// The session's bus, as `sessiond` is told of it.
fn bus() -> crate::session::Bus {
    crate::session::Bus {
        daemon: DAEMON.to_owned(),
        config: format!("/{}", crate::session::SESSION_BUS_CONF_PATH),
        env: vec![(
            "XDG_DATA_DIRS".to_owned(),
            "/data/usr/share:/usr/share".to_owned(),
        )],
    }
}

/// The desktop's configuration: the environment GTK needs from the volume,
/// the app, and the probe.
fn config() -> String {
    format!(
        "# Written into the initramfs by `cargo xtask test-gnome-apps` (docs/GNOME.md, G1).\n\
         env = XDG_DATA_DIRS,/data/usr/share:/usr/share\n\
         env = XKB_CONFIG_ROOT,/data/usr/share/X11/xkb\n\
         env = FONTCONFIG_FILE,/{FONTS_PATH}\n\
         env = GTK_A11Y,none\n\
         env = LANG,C.UTF-8\n\
         exec-once = /data/usr/bin/adwaita-1-demo\n\
         exec-once = /bin/busybox sh /{PROBE_PATH}\n"
    )
}

/// The files the image carries beside the desktop's own.
fn files() -> Vec<File> {
    let text = |path: &str, mode: u32, body: &str| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(body.as_bytes().to_vec()),
    };
    vec![
        text(PROBE_PATH, 0o755, PROBE),
        text(AFTER_PATH, 0o755, AFTER),
        text(FONTS_PATH, 0o644, FONTS),
        text(
            "etc/ferrix/units/gnome-gate-after.service",
            0o644,
            AFTER_UNIT,
        ),
        File {
            path: "etc/ferrix/units/graphical.target.wants/gnome-gate-after.service".to_owned(),
            mode: 0o777,
            content: Content::Link("/etc/ferrix/units/gnome-gate-after.service".to_owned()),
        },
    ]
}

/// `cargo xtask test-gnome-apps`.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails,
/// or any requirement in the module's header is not met.
/// What `sessiond` says when the session's bus was not ended with the
/// session's scope.
const BUS_OUTLIVED: &str = "the session's bus was still running after the session's end";

pub(crate) fn test_gnome_apps(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-gnome-apps runs on x86-64 only: the volume is Debian's amd64",
        ));
    }
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    args.gl = false;
    args.no_gl = true;
    args.display = true;
    args.release = true;
    args.wallpaper = Some("none".to_owned());
    args.session_user = Some(crate::session::USER.to_owned());
    args.session_bus = Some(bus());
    args.data_nosuid = true;
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    if args.accel.is_none() {
        args.accel = Some("kvm".to_owned());
    }
    let programs = Programs::build(arch)?;
    let size = crate::wallpaper::SCREEN;
    let (config, mut carried) = desktop(arch, config(), size, false, Backdrop::Any, &args)?;
    let config = crate::session::for_session(&config, &mut carried.ports, None);
    carried.ports.extend(files());
    let (image, kernel) =
        build_desktop_image(arch, &programs, &config, carried, &args, DESKTOP_DEFAULTS)?;
    let port = free_port()?;
    let qemu_args = Args {
        size: Some(size),
        qmp_port: Some(port),
        ..args.clone()
    };
    let shots = paths::build_dir(arch).join("gnome-apps");
    let _ = std::fs::remove_dir_all(&shots);
    std::fs::create_dir_all(&shots)
        .map_err(|error| Error::new(format!("making {}: {error}", shots.display())))?;
    println!(
        "  {arch}: Debian's adwaita-1-demo on hyprix as ferrix, from {}, with sessiond's \
         session bus; the screen in {}",
        args.data_image
            .as_deref()
            .map(Path::display)
            .map(|path| path.to_string())
            .unwrap_or_default(),
        shots.display()
    );
    let mut verdict: Option<Result<()>> = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        verdict = Some(watch(arch, watching, &mut qmp, &shots));
        Ok(())
    };
    let ran = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook);
    ran.and_then(|_| {
        verdict.unwrap_or_else(|| Err(Error::new(format!("{arch}: hyprix never started"))))
    })
}

/// The probe's and the watcher's words after [`SAID`] and `what`, from the
/// last line that has them.
fn said<'a>(lines: &'a [String], what: &str) -> Option<&'a str> {
    let marker = format!("{SAID}{what}");
    lines
        .iter()
        .rev()
        .find_map(|line| line.split_once(&marker).map(|(_, rest)| rest.trim()))
}

/// A window line, `<x>,<y> <w>,<h> <class> <title>`, for the app's class.
fn app_window(lines: &[String]) -> Option<((i32, i32), (i32, i32))> {
    let pair = |word: &str| -> Option<(i32, i32)> {
        let (a, b) = word.split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    };
    lines.iter().rev().find_map(|line| {
        let rest = line.split_once(&format!("{SAID}window "))?.1;
        let mut words = rest.split_whitespace();
        let (at, size, class) = (words.next()?, words.next()?, words.next()?);
        (class == APP_ID).then_some((pair(at)?, pair(size)?))
    })
}

/// How many colours `screen` shows in the rectangle at `at` of `size`.
fn colours(screen: &Image, (at, size): ((i32, i32), (i32, i32))) -> usize {
    let clamp = |value: i32, most: usize| usize::try_from(value.max(0)).unwrap_or(0).min(most);
    let (left, top) = (clamp(at.0, screen.width), clamp(at.1, screen.height));
    let (right, bottom) = (
        clamp(at.0 + size.0, screen.width),
        clamp(at.1 + size.1, screen.height),
    );
    let mut seen = std::collections::HashSet::new();
    for y in top..bottom {
        let row = screen
            .pixels
            .get((y * screen.width + left) * 3..(y * screen.width + right) * 3)
            .unwrap_or_default();
        let (pixels, _) = row.as_chunks::<3>();
        seen.extend(pixels.iter().copied());
    }
    seen.len()
}

/// What the lines show wrong of the module header's requirements, before
/// the end: the first, if any.
fn wrong_before_end(lines: &[String]) -> Option<String> {
    let mounts = lines
        .iter()
        .rev()
        .find_map(|line| line.split_once(crate::session::DATA_NOSUID_SAID))
        .map(|(_, rest)| rest.trim().to_owned());
    match mounts {
        Some(line) if line.contains("nosuid") && line.contains("nodev") => {}
        Some(line) => return Some(format!("/data is not nosuid,nodev: `{line}`")),
        None => return Some("data-nosuid.service said no /data line".to_owned()),
    }
    if !lines.iter().any(|line| {
        line.contains(&format!(
            "sessiond: the session's bus is up: unix:path={SOCKET}"
        ))
    }) {
        return Some(format!("sessiond never read a bus address at {SOCKET}"));
    }
    let daemons: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains(&format!("{SAID}process dbus-daemon")))
        .collect();
    if daemons.is_empty() {
        return Some("no dbus-daemon runs in the session".to_owned());
    }
    for line in &daemons {
        if !line.contains("uid 1000:") || !line.contains(&format!("--address=unix:path={SOCKET}")) {
            return Some(format!(
                "a dbus-daemon not ferrix's session bus: `{}`",
                line.trim()
            ));
        }
        if line.contains("--system") {
            return Some(format!("a system bus runs: `{}`", line.trim()));
        }
    }
    let app = said(lines, "process adwaita-1-demo uid ");
    let Some(pid) = app.and_then(|said| said.strip_prefix("1000 pid ")) else {
        return Some(format!(
            "adwaita-1-demo is not ferrix's: uid {}",
            app.unwrap_or("none")
        ));
    };
    match said(lines, "bus socket: ") {
        Some(socket) if socket.starts_with("1000 socket at ") && socket.ends_with(SOCKET) => {}
        other => {
            return Some(format!(
                "the bus socket is not ferrix's at {SOCKET}: {}",
                other.unwrap_or("not said")
            ));
        }
    }
    // The volume's files root's (the consultant's A1, ledger 3137): the
    // daemon root's and `ferrix` refused a write to it.
    match said(lines, "daemon file: ") {
        Some("0 0 755") => {}
        other => {
            return Some(format!(
                "the volume's dbus-daemon is not root's 0755: {}",
                other.unwrap_or("not said")
            ));
        }
    }
    if said(lines, "write by ferrix: ") != Some("refused") {
        return Some("ferrix could write the volume's dbus-daemon".to_owned());
    }
    match said(lines, "system bus socket: ") {
        Some(answer) if answer.contains("No such file") => {}
        other => {
            return Some(format!(
                "there is a system bus socket: {}",
                other.unwrap_or("not said")
            ));
        }
    }
    // The app on the bus: a connection the bus itself says is ferrix's
    // and the app's process (`GetConnectionUnixUser` and `...ProcessID`).
    // The demo is a non-unique application, so it owns no name of its own.
    let connected = lines.iter().any(|line| {
        line.split_once(&format!("{SAID}bus connection :"))
            .is_some_and(|(_, rest)| rest.ends_with(&format!(" uid 1000 pid {pid}")))
    });
    if !connected {
        return Some(format!(
            "no connection on the session bus is ferrix's adwaita-1-demo (pid {pid})"
        ));
    }
    None
}

/// Wait for the probe, judge the window on the screen, then the end.
fn watch(arch: Arch, watching: &mut Watching<'_>, qmp: &mut Qmp, shots: &Path) -> Result<()> {
    let up = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines.iter().any(|line| line.contains(MARKER))
    })?;
    if !up {
        return Err(with_the_transcript(
            &Error::new(format!("{arch}: the compositor never printed `{MARKER}`")),
            watching,
        ));
    }
    say_the_marker(watching, arch);
    let deadline = Instant::now() + PATIENCE;
    let ready = watching.read_more(deadline, |lines| {
        lines.iter().any(|line| {
            line.contains(&format!("{SAID}ready for the screen"))
                || line.contains(&format!("{SAID}no window"))
                || line.contains("FERRIX-PANIC")
        })
    })?;
    let lines: Vec<String> = watching
        .lines()
        .iter()
        .chain(watching.after())
        .cloned()
        .collect();
    if !ready
        || !lines
            .iter()
            .any(|line| line.contains("ready for the screen"))
    {
        return Err(with_the_transcript(
            &Error::new(format!(
                "{arch}: the probe never saw adwaita-1-demo's window"
            )),
            watching,
        ));
    }
    if let Some(wrong) = wrong_before_end(&lines) {
        return Err(with_the_transcript(
            &Error::new(format!("{arch}: {wrong}")),
            watching,
        ));
    }
    let Some(window) = app_window(&lines) else {
        return Err(with_the_transcript(
            &Error::new(format!("{arch}: hyprix lists no window of class {APP_ID}")),
            watching,
        ));
    };
    let dump = shots.join("screen.ppm");
    qmp.screendump(Some(DEVICE_ID), &dump)?;
    let bytes = std::fs::read(&dump)
        .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
    let screen = parse_ppm(&bytes)?;
    let drawn = colours(&screen, window);
    if drawn < DRAWN_COLOURS {
        return Err(with_the_transcript(
            &Error::new(format!(
                "{arch}: the app's window at {:?} of {:?} shows {drawn} colours, fewer than \
                 {DRAWN_COLOURS}: not drawn; the screen is {}",
                window.0,
                window.1,
                dump.display()
            )),
            watching,
        ));
    }
    println!(
        "  {arch}: /data nosuid,nodev; ferrix's session bus at {SOCKET}, no system bus; \
         adwaita-1-demo ferrix's and on the bus, its window drawn ({drawn} colours); \
         the screen is {}",
        dump.display()
    );
    let ended = watching.read_more(deadline, |lines| {
        lines
            .iter()
            .any(|line| line.contains(&format!("{SAID}after the end:")))
    })?;
    let lines: Vec<String> = watching
        .lines()
        .iter()
        .chain(watching.after())
        .cloned()
        .collect();
    match wrong_after_end(&lines, ended) {
        None => {
            println!(
                "  {arch}: the compositor killed, the session ended, and neither its bus nor the \
                 app was left"
            );
            Ok(())
        }
        Some(why) => Err(with_the_transcript(
            &Error::new(format!("{arch}: after the session's end: {why}")),
            watching,
        )),
    }
}

/// What is wrong after the session's end, if anything: the bus must go with
/// the scope, not after it -- `sessiond` says when it was still running once
/// the scope ended, even if it was gone by the time the watcher looked (the
/// control g1-ctl-bus-out-of-scope) -- and the watcher must find nothing.
fn wrong_after_end(lines: &[String], ended: bool) -> Option<String> {
    if lines.iter().any(|line| line.contains(BUS_OUTLIVED)) {
        return Some(BUS_OUTLIVED.to_owned());
    }
    match said(lines, "after the end:") {
        Some("none") if ended => None,
        other => Some(other.unwrap_or("the watcher never said").to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bus_that_outlived_the_scope_fails_the_end() {
        let clean = lines("  35.2 | gnome-gate: after the end: none");
        assert_eq!(wrong_after_end(&clean, true), None);
        assert!(wrong_after_end(&clean, false).is_some());
        let outlived = lines(
            "  34.3 | sessiond: the session's bus was still running after the session's end\n  \
             35.2 | gnome-gate: after the end: none",
        );
        assert_eq!(
            wrong_after_end(&outlived, true).as_deref(),
            Some(BUS_OUTLIVED)
        );
        let left = lines("  35.2 | gnome-gate: after the end: dbus-daemon:40");
        assert_eq!(
            wrong_after_end(&left, true).as_deref(),
            Some("dbus-daemon:40")
        );
    }

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    const GOOD: &str = "\
  3.0 | data-nosuid: btrfs /data btrfs rw,nosuid,nodev 0 0
  4.0 | sessiond: the session's bus is up: unix:path=/run/user/1000/bus,guid=ab
  9.0 | gnome-gate: window 960,20 940,1040 org.gnome.Adwaita1.Demo Adwaita Demo
  9.0 | gnome-gate: process dbus-daemon uid 1000: /data/usr/bin/dbus-daemon --config-file=/etc/ferrix/session-bus.conf --nofork --nopidfile --address=unix:path=/run/user/1000/bus --print-address=3
  9.0 | gnome-gate: process adwaita-1-demo uid 1000 pid 45
  9.0 | gnome-gate: bus socket: 1000 socket at /run/user/1000/bus
  9.0 | gnome-gate: system bus socket: ls: /run/dbus/system_bus_socket: No such file or directory
  9.0 | gnome-gate: daemon file: 0 0 755
  9.0 | gnome-gate: write by ferrix: refused
  9.0 | gnome-gate: bus names: \"org.freedesktop.DBus\" \":1.0\" \":1.1\"
  9.0 | gnome-gate: bus connection :1.0 uid 1000 pid 45
  9.0 | gnome-gate: bus connection :1.1 uid  pid
";

    #[test]
    fn a_good_session_passes_and_each_fault_fails() {
        assert_eq!(wrong_before_end(&lines(GOOD)), None);
        assert_eq!(app_window(&lines(GOOD)), Some(((960, 20), (940, 1040))));
        for (from, to) in [
            ("rw,nosuid,nodev", "rw,nodev"),
            ("rw,nosuid,nodev", "rw,nosuid"),
            (
                "bus is up: unix:path=/run/user/1000/bus",
                "printed no address",
            ),
            (
                "process dbus-daemon uid 1000:",
                "process dbus-daemon uid 0:",
            ),
            ("--print-address=3", "--print-address=3 --system"),
            (
                "process adwaita-1-demo uid 1000",
                "process adwaita-1-demo uid 0",
            ),
            ("bus socket: 1000 socket", "bus socket: 0 socket"),
            ("daemon file: 0 0 755", "daemon file: 1000 1000 755"),
            ("write by ferrix: refused", "write by ferrix: allowed"),
            (
                "ls: /run/dbus/system_bus_socket: No such file or directory",
                "/run/dbus/system_bus_socket",
            ),
        ] {
            let bad = GOOD.replacen(from, to, 1);
            assert!(
                wrong_before_end(&lines(&bad)).is_some(),
                "`{from}` -> `{to}` passed"
            );
        }
    }

    #[test]
    fn a_window_of_one_colour_is_not_drawn() {
        let screen = Image {
            width: 4,
            height: 2,
            pixels: vec![255; 4 * 2 * 3],
        };
        assert_eq!(colours(&screen, ((0, 0), (4, 2))), 1);
    }
}
