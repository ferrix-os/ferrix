//! Anthropic's Claude desktop app on the `--everything` desktop
//! (`docs/CLAUDE-CODE.md` §7), and `test-claude-desktop`, which requires its
//! window and its sign-in screen on the screen.
//!
//! # Where the app comes from
//!
//! `claude-desktop` is Anthropic's own Linux build, an Electron program,
//! fetched on this machine by `tools/common/fetch/fetch-claude-desktop.sh`
//! from Anthropic's apt repository, checked from a pinned key down to the
//! package, and unpacked without its maintainer scripts. It is proprietary,
//! so Ferrix carries none of it: the script keeps a tree beside a stamp, and
//! `crate::everything` merges that tree, with GTK 3 and the client library
//! of the sound server `pipewire` from Debian, into the `--everything`
//! volume. No image of it is made on its own.
//!
//! # On the desktop
//!
//! [`desktop_files`] gives the `--everything` desktop the app as
//! `/bin/claude-desktop` ([`WRAPPER`]), its entry in the launcher, and
//! `claude:` links and its file types for `/bin/xdg-open` to hand it
//! (`crate::chrome::opener`). It runs as Chrome does there: a Wayland client
//! of hyprix, drawing in software, without Chromium's sandbox, on
//! ferrousli's loader and `libc.so.6` where the desktop puts them in glibc's
//! place.
//!
//! # The gate
//!
//! `test-claude-desktop` boots that desktop as `run-compositor --everything`
//! builds it, less what only a person watching wants -- as
//! `test-steam-store` does, the host's configuration, Chrome's window, the
//! wallpaper and the 3D card -- and starts the app from hyprix's
//! `exec-once`. A watcher ([`WATCH`]) lists hyprix's windows as they
//! change. What is required: a window whose class is the app's
//! ([`APP_ID`]), and in it the sign-in screen, drawn ([`signed_out`]):
//! its background, Claude's mark and the button to sign in with. No
//! account is used, and nothing is typed: the gate stops at the screen a
//! person signs in on.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{build_desktop_image, say_the_marker, with_the_transcript};
use super::browser::chrome_libc;
use super::desktop::{Backdrop, desktop};
use super::run::DESKTOP_DEFAULTS;
use super::{EITHER, FAILED, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The app's directory on the volume, as Debian's package lays it out.
const APP: &str = "usr/lib/claude-desktop";

/// The program, under the tree.
const PROGRAM: &str = "usr/lib/claude-desktop/claude-desktop";

/// The class hyprix gives the app's window: the Wayland app id Chromium
/// takes from the package's `desktopName`, which the package's own launcher
/// entry names as its `StartupWMClass`.
pub(crate) const APP_ID: &str = "com.anthropic.Claude";

/// `/bin/claude-desktop`: the app from the volume, as a Wayland client.
///
/// * `--ozone-platform=wayland`: a client of hyprix, through `wl_shm`.
/// * `--disable-gpu`: its GPU process in software, as Chrome's on the
///   desktop, since the render node is the compositor's.
/// * `--no-sandbox`: Chromium's sandbox does not run on Ferrix yet
///   (`docs/SECCOMP.md`, S7 and S8), as for Chrome.
/// * `XDG_DATA_DIRS`: GTK's schemas, its icons and the volume's MIME data
///   are under `/data/usr/share`, which no program looks in by itself.
/// * A home that can be written: Electron keeps its profile under
///   `~/.config/Claude`, and refuses to start without one.
const WRAPPER: &str = r#"#!/bin/sh
app=/data/usr/lib/claude-desktop
[ -x $app/claude-desktop ] || { echo "claude-desktop: $app is not on this volume" >&2; exit 1; }
export XDG_DATA_DIRS=/data/usr/share:/usr/share
[ -n "$HOME" ] && [ -w "$HOME" ] || export HOME=/dev/shm
exec $app/claude-desktop --no-sandbox --ozone-platform=wayland --disable-gpu "$@"
"#;

/// The launcher's entry: the package's own, less what Ferrix has no use
/// for, with the app's icon on the volume. `%U` is a `claude:` link or a
/// file, which a second start hands to the first.
const DESKTOP_ENTRY: &str = "[Desktop Entry]\nType=Application\nName=Claude\n\
     GenericName=AI Assistant\nComment=Anthropic's Claude desktop app, from the data disk\n\
     Exec=claude-desktop %U\n\
     Icon=/data/usr/share/icons/hicolor/256x256/apps/claude-desktop.png\n\
     StartupWMClass=com.anthropic.Claude\nSingleMainWindow=true\n\
     Keywords=AI;Chat;Assistant;Claude;Code;LLM;\nCategories=Utility;Development;\n\
     MimeType=x-scheme-handler/claude;application/vnd.anthropic.mcpb;application/vnd.anthropic.skill;\n";

/// Where [`DESKTOP_ENTRY`] goes: the name the package gives its own.
const DESKTOP_ENTRY_PATH: &str = "usr/share/applications/com.anthropic.Claude.desktop";

/// What the desktop opens `claude:` links and the app's files with, as
/// `xdg-mime` reads it.
const MIMEAPPS: &str = "[Default Applications]\n\
     x-scheme-handler/claude=com.anthropic.Claude.desktop\n\
     application/vnd.anthropic.mcpb=com.anthropic.Claude.desktop\n\
     application/vnd.anthropic.skill=com.anthropic.Claude.desktop\n";

/// Where [`MIMEAPPS`] goes.
const MIMEAPPS_PATH: &str = "usr/share/applications/mimeapps.list";

/// The app's own file types, which its package's `postinst` registers:
/// MCP bundles (`.mcpb`, and `.dxt` from before they were renamed) and
/// skills. Ferrix has no MIME database to add them to, so they are written
/// here as shared-mime-info's source format, and `/bin/xdg-open` knows them
/// by their extensions ([`crate::chrome::opener`]).
const MIME_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/vnd.anthropic.mcpb">
    <comment>MCP bundle</comment>
    <sub-class-of type="application/zip"/>
    <glob pattern="*.mcpb"/>
    <glob pattern="*.dxt"/>
  </mime-type>
  <mime-type type="application/vnd.anthropic.skill">
    <comment>Claude skill</comment>
    <sub-class-of type="application/zip"/>
    <glob pattern="*.skill"/>
  </mime-type>
</mime-info>
"#;

/// Where [`MIME_TYPES`] goes.
const MIME_TYPES_PATH: &str = "usr/share/mime/packages/claude-desktop.xml";

/// Where `tools/common/fetch/fetch-claude-desktop.sh` writes, unless
/// `FERRIX_CLAUDE_DESKTOP_VOLUME` names another directory.
fn directory() -> Result<PathBuf> {
    match std::env::var_os("FERRIX_CLAUDE_DESKTOP_VOLUME") {
        Some(directory) => Ok(PathBuf::from(directory)),
        None => paths::volume_directory("claude-desktop"),
    }
}

/// The stamp the fetch script writes once its tree is whole, which stands
/// for the volume where the others have an image: `crate::everything`
/// merges the tree beside it, and makes its volume again when the stamp is
/// newer.
///
/// # Errors
///
/// The app has not been fetched.
pub(crate) fn volume() -> Result<PathBuf> {
    let directory = directory()?;
    let stamp = directory.join("claude-desktop.stamp");
    if !stamp.is_file() || !directory.join("tree").join(PROGRAM).is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-claude-desktop.sh fetches the app",
            stamp.display()
        )));
    }
    Ok(stamp)
}

/// Whether `tree` is the whole tree the fetch script leaves.
pub(crate) fn is_tree(tree: &Path) -> bool {
    tree.join(PROGRAM).is_file() && tree.join(APP).join("resources/app.asar").is_file()
}

/// What the `--everything` desktop carries for the app: [`WRAPPER`] as
/// `/bin/claude-desktop`, its launcher entry, which links and files it
/// opens, and its file types; none of them at a path `carried` already has.
/// The volume's glibc links are Chrome's, which that desktop carries.
pub(crate) fn desktop_files(carried: &[File]) -> Vec<File> {
    [
        ("bin/claude-desktop", 0o755, WRAPPER),
        (DESKTOP_ENTRY_PATH, 0o644, DESKTOP_ENTRY),
        (MIMEAPPS_PATH, 0o644, MIMEAPPS),
        (MIME_TYPES_PATH, 0o644, MIME_TYPES),
    ]
    .into_iter()
    .filter(|(path, _, _)| !carried.iter().any(|file| file.path == *path))
    .map(|(path, mode, text)| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(text.as_bytes().to_vec()),
    })
    .collect()
}

/// The watcher of hyprix's windows: each time the list changes, one line a
/// window -- its place, its size, its class and its title -- and an end
/// line with the snapshot's number.
const WATCH: &str = r#"# `cargo xtask test-claude-desktop`: hyprix's windows, each time they change.
export PATH=/bin
n=0
seen=
while :; do
    now=$(/bin/hyprctl clients 2>/dev/null | awk '
        /^\tat: / { at = $2 }
        /^\tsize: / { size = $2 }
        /^\tclass: / { class = $2 }
        /^\ttitle: / { sub(/^\ttitle: /, ""); print at " " size " " class " " $0 }')
    if [ "$now" != "$seen" ]; then
        n=$((n + 1))
        echo "$now" | while IFS= read -r window; do
            [ -n "$window" ] && echo "claude-desktop-gate: window $n: $window"
        done
        echo "claude-desktop-gate: windows $n: end"
        seen=$now
    fi
    sleep 2
done
"#;

/// Where [`WATCH`] is in the image.
const WATCH_PATH: &str = "claude-desktop/watch.sh";

/// What the watcher prints before each window of a snapshot.
const WINDOW: &str = "claude-desktop-gate: window ";

/// What the line `/bin/claude-desktop` is started with says once the app
/// has exited. Without its status: hyprix's configuration reads `$` as its
/// own variables.
const EXITED: &str = "claude-desktop-gate: the app exited";

/// Whether `line` is the shell saying the app exited, and not hyprix
/// saying it started that shell: hyprix's line quotes the command, which
/// holds [`EXITED`] too.
fn exited(line: &str) -> bool {
    line.contains(EXITED) && !line.contains("hyprix: started")
}

/// The screen, as `run-compositor` makes it.
const SCREEN: (u32, u32) = crate::wallpaper::SCREEN;

/// Guest memory: Electron's browser process, its renderer and its GPU
/// process in software, and the page cache holding 230 MB of program.
const MEMORY: u32 = 8192;

/// Seconds for the boot.
const TIMEOUT: u64 = 1500;

/// How long the app has, from the compositor coming up, to list its window
/// and draw its sign-in screen in it.
const PATIENCE: Duration = Duration::from_secs(600);

/// How often the screen is looked at while waiting.
const EVERY: Duration = Duration::from_secs(10);

/// Of the screens looked at, every this many is kept.
const KEEP_EVERY: usize = 6;

/// The desktop's configuration: the watcher, the environment the desktop
/// gives Chrome's programs, and the app, its output on the console.
fn config(ferrousli: bool) -> String {
    format!(
        "# Written into the initramfs by `cargo xtask test-claude-desktop` (docs/CLAUDE-CODE.md §7).\n\
         exec-once = /bin/busybox sh /{WATCH_PATH}\n\
         # Without the 3D card hyprix composites in software: what the gate judges is the app.\n\
         decoration {{\n    blur {{\n        enabled = false\n    }}\n}}\n\
         {}{}\
         exec-once = /bin/busybox sh -c 'HOME=/dev/shm /bin/claude-desktop --enable-logging=stderr; echo {EXITED}'\n",
        crate::chrome::DESKTOP_ENV,
        crate::chrome::window_library_path(ferrousli),
    )
}

/// `args` as `run-compositor --everything` has them, less what the module's
/// header says the gate leaves out.
fn everything_args(args: &Args) -> Result<Args> {
    let mut args = args.clone();
    args.everything = true;
    args.chrome = true;
    args.gl = false;
    args.no_gl = true;
    args.display = true;
    args.release = true;
    args.clipboard = false;
    args.wallpaper = Some("none".to_owned());
    args.net = true;
    // Its error names the script that fetches the app.
    let _ = volume()?;
    args.data_image = Some(crate::everything::volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    if args.accel.is_none() {
        args.accel = Some("kvm".to_owned());
    }
    chrome_libc(&mut args);
    Ok(args)
}

/// A window of hyprix's, as the watcher lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    at: (i32, i32),
    size: (i32, i32),
    class: String,
    title: String,
}

/// One of the watcher's window lines, `<x>,<y> <w>,<h> <class> <title>`.
fn parse_listed(text: &str) -> Option<Listed> {
    let mut words = text.splitn(4, ' ');
    let pair = |word: Option<&str>| -> Option<(i32, i32)> {
        let (a, b) = word?.split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    };
    let at = pair(words.next())?;
    let size = pair(words.next())?;
    let class = words.next()?.to_owned();
    let title = words.next().unwrap_or_default().trim().to_owned();
    Some(Listed {
        at,
        size,
        class,
        title,
    })
}

/// The app's window in the last snapshot `lines` hold, if it lists one.
fn app_window(lines: &[String]) -> Option<Listed> {
    let mut last: Option<(u64, Listed)> = None;
    for line in lines {
        let Some(rest) = line.split_once(WINDOW).map(|(_, rest)| rest) else {
            continue;
        };
        let Some((number, window)) = rest.split_once(": ") else {
            continue;
        };
        let (Ok(number), Some(window)) = (number.parse::<u64>(), parse_listed(window)) else {
            continue;
        };
        if window.class == APP_ID && last.as_ref().is_none_or(|(seen, _)| number >= *seen) {
            last = Some((number, window));
        }
    }
    last.map(|(_, window)| window)
}

/// The colours of `window`'s part of `screen`, by how many pixels have each.
fn colours(screen: &Image, window: &Listed) -> std::collections::HashMap<[u8; 3], usize> {
    let mut counts = std::collections::HashMap::new();
    let clamp = |value: i32, most: usize| usize::try_from(value.max(0)).unwrap_or(0).min(most);
    let (left, top) = (
        clamp(window.at.0, screen.width),
        clamp(window.at.1, screen.height),
    );
    let (right, bottom) = (
        clamp(window.at.0 + window.size.0, screen.width),
        clamp(window.at.1 + window.size.1, screen.height),
    );
    for y in top..bottom {
        let row = screen
            .pixels
            .get((y * screen.width + left) * 3..(y * screen.width + right) * 3)
            .unwrap_or_default();
        let (pixels, _) = row.as_chunks::<3>();
        for pixel in pixels {
            *counts.entry(*pixel).or_insert(0) += 1;
        }
    }
    counts
}

/// What the sign-in screen is made of, as the app (package 2.37337.1)
/// drew it on Ferrix on 2026-10-15, its window 1878x1038: 94% of the window
/// its background, `#fcfcfb`; Claude's mark in the middle, 3783 pixels of
/// its orange; the black "Get started" button below it, 14017 pixels of
/// `#0b0b0b`; 1505 colours in all. Each least amount below is well under
/// that: a window the app has not drawn into is one colour, and a page
/// without the mark or the button is some other screen than this one.
const BACKGROUND_SHARE: f64 = 0.5;
const DRAWN_COLOURS: usize = 64;
const MARK_PIXELS: usize = 1000;
const BUTTON_PIXELS: usize = 5000;

/// Whether `window`'s part of `screen` is the app's sign-in screen drawn:
/// most of it the screen's light background ([`is_paper`]), with Claude's
/// mark ([`is_mark`]), the button a person signs in through ([`is_ink`])
/// and the text around them.
fn signed_out(screen: &Image, window: &Listed) -> (bool, String) {
    let counts = colours(screen, window);
    let total: usize = counts.values().sum();
    let having = |kind: fn([u8; 3]) -> bool| -> usize {
        counts
            .iter()
            .filter(|(colour, _)| kind(**colour))
            .map(|(_, count)| count)
            .sum()
    };
    let (paper, mark, ink) = (having(is_paper), having(is_mark), having(is_ink));
    let to_float = |count: usize| f64::from(u32::try_from(count).unwrap_or(u32::MAX));
    let share = if total == 0 {
        0.0
    } else {
        to_float(paper) / to_float(total)
    };
    let what = format!(
        "{} colours, {:.0}% of the window the sign-in screen's background, {mark} pixels of \
         Claude's mark, {ink} of its button",
        counts.len(),
        share * 100.0
    );
    (
        share >= BACKGROUND_SHARE
            && counts.len() >= DRAWN_COLOURS
            && mark >= MARK_PIXELS
            && ink >= BUTTON_PIXELS,
        what,
    )
}

/// Whether `colour` is the sign-in screen's background: Claude's light
/// off-white, warm or grey.
fn is_paper([red, green, blue]: [u8; 3]) -> bool {
    red >= 0xe8 && green >= 0xe4 && blue >= 0xd8 && red >= blue && red - blue <= 0x18
}

/// Whether `colour` is Claude's mark: its terracotta orange, `#db6943`,
/// and the shades its edges blend to.
fn is_mark([red, green, blue]: [u8; 3]) -> bool {
    red >= 0xb0
        && (0x40..=0xa0).contains(&green)
        && blue <= 0x80
        && red - green >= 0x40
        && green >= blue
}

/// Whether `colour` is the black of the sign-in button.
fn is_ink([red, green, blue]: [u8; 3]) -> bool {
    red <= 0x18 && green <= 0x18 && blue <= 0x18
}

/// `image` as a binary PPM.
fn ppm(image: &Image) -> Vec<u8> {
    let mut bytes = format!("P6\n{} {}\n255\n", image.width, image.height).into_bytes();
    bytes.extend_from_slice(&image.pixels);
    bytes
}

/// `cargo xtask test-claude-desktop`.
///
/// # Errors
///
/// When the app or a volume is missing, the image cannot be built, the boot
/// fails, the app lists no window, or its window never shows the sign-in
/// screen.
pub(crate) fn test_claude_desktop(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-claude-desktop runs on x86-64 only: the --everything volume is x86-64's",
        ));
    }
    let args = everything_args(args)?;
    let ferrousli = crate::chrome::on_ferrousli(&args);
    let programs = Programs::build(arch)?;
    let (config, mut carried) =
        desktop(arch, config(ferrousli), SCREEN, false, Backdrop::Any, &args)?;
    if !carried
        .ports
        .iter()
        .any(|file| file.path == "bin/claude-desktop")
    {
        return Err(Error::new(
            "the --everything desktop carries no /bin/claude-desktop",
        ));
    }
    carried.ports.push(File {
        path: WATCH_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(WATCH.as_bytes().to_vec()),
    });
    let (image, kernel) =
        build_desktop_image(arch, &programs, &config, carried, &args, DESKTOP_DEFAULTS)?;
    let port = free_port()?;
    let qemu_args = Args {
        display: true,
        size: Some(SCREEN),
        qmp_port: Some(port),
        ..args.clone()
    };
    let shots = paths::build_dir(arch).join("claude-desktop");
    let _ = std::fs::remove_dir_all(&shots);
    std::fs::create_dir_all(&shots)
        .map_err(|error| Error::new(format!("making {}: {error}", shots.display())))?;
    println!(
        "  {arch}: the Claude desktop app on the --everything desktop, on {}, {} MiB; \
         its sign-in screen within {}s; screens in {}",
        if ferrousli {
            "ferrousli's loader and libc.so.6"
        } else {
            "the volume's glibc"
        },
        args.memory,
        PATIENCE.as_secs(),
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

/// Wait for the app's window and its sign-in screen, keeping a screen every
/// [`EVERY`] in `shots`.
fn watch(arch: Arch, watching: &mut Watching<'_>, qmp: &mut Qmp, shots: &Path) -> Result<()> {
    let up = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines
            .iter()
            .any(|line| line.contains(MARKER) || line.contains(FAILED))
    })?;
    if !up {
        return Err(with_the_transcript(
            &Error::new(format!("{arch}: the compositor never printed `{MARKER}`")),
            watching,
        ));
    }
    say_the_marker(watching, arch);
    let dump = shots.join("screen.ppm");
    let deadline = Instant::now() + PATIENCE;
    let mut taken = 0;
    let mut last = String::from("no window of the app was listed");
    loop {
        let _ = watching.read_more(Instant::now() + EVERY, |_| false)?;
        let lines: Vec<String> = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        if let Some(line) = lines.iter().find(|line| line.contains("FERRIX-PANIC")) {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: the kernel stopped while the app ran: {}",
                    line.trim()
                )),
                watching,
            ));
        }
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        let bytes = std::fs::read(&dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        let screen = parse_ppm(&bytes)?;
        // One screen a minute is kept, to be looked at, and the last.
        taken += 1;
        if taken % KEEP_EVERY == 1 {
            let kept = shots.join(format!("screen-{taken:03}.ppm"));
            std::fs::write(&kept, ppm(&screen))
                .map_err(|error| Error::new(format!("writing {}: {error}", kept.display())))?;
        }
        let kept = shots.join("last.ppm");
        std::fs::write(&kept, ppm(&screen))
            .map_err(|error| Error::new(format!("writing {}: {error}", kept.display())))?;
        if let Some(window) = app_window(&lines) {
            let (drawn, what) = signed_out(&screen, &window);
            last = format!(
                "its window \"{}\" at {},{} of {}x{}: {what}",
                window.title, window.at.0, window.at.1, window.size.0, window.size.1
            );
            if drawn {
                println!(
                    "  {arch}: hyprix lists the app's window, class {APP_ID}, and it shows the \
                     sign-in screen: {last}; the screen is {}",
                    kept.display()
                );
                return Ok(());
            }
        }
        if let Some(line) = lines.iter().find(|line| exited(line)) {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: the app exited before its sign-in screen was drawn: {}",
                    line.trim()
                )),
                watching,
            ));
        }
        if Instant::now() >= deadline {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: no sign-in screen of the app's within {}s: {last}; the last screen \
                     is {}",
                    PATIENCE.as_secs(),
                    kept.display()
                )),
                watching,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watchers_lines_name_the_apps_window() {
        let lines = [
            "  12.0 | claude-desktop-gate: window 1: 0,0 1920,1080 foot zinc".to_owned(),
            "  14.0 | claude-desktop-gate: window 2: 10,20 1900,1040 com.anthropic.Claude Claude"
                .to_owned(),
            "  14.0 | claude-desktop-gate: windows 2: end".to_owned(),
        ];
        let window = app_window(&lines).expect("the app's window");
        assert_eq!(window.at, (10, 20));
        assert_eq!(window.size, (1900, 1040));
        assert_eq!(window.title, "Claude");
        assert!(app_window(&lines[..1]).is_none());
    }

    /// A window of `width` by 100 pixels: the background, with 2000 pixels
    /// of the mark and 6000 of the button when `whole`, and a row of 100
    /// shades of grey for its text.
    fn sign_in(whole: bool) -> (Image, Listed) {
        let width = 400;
        let mut pixels = Vec::new();
        for at in 0..width * 100 {
            let pixel: [u8; 3] = match at {
                _ if at < 100 => {
                    let grey = u8::try_from(at).unwrap_or(0);
                    [grey, grey, grey]
                }
                _ if whole && at < 2100 => [0xdb, 0x69, 0x43],
                _ if whole && at < 8100 => [0x0b, 0x0b, 0x0b],
                _ => [0xfc, 0xfc, 0xfb],
            };
            pixels.extend_from_slice(&pixel);
        }
        let screen = Image {
            width,
            height: 100,
            pixels,
        };
        let window = Listed {
            at: (0, 0),
            size: (400, 100),
            class: APP_ID.to_owned(),
            title: "Claude".to_owned(),
        };
        (screen, window)
    }

    #[test]
    fn the_sign_in_screen_needs_its_mark_and_its_button() {
        let (screen, window) = sign_in(true);
        let (drawn, what) = signed_out(&screen, &window);
        assert!(drawn, "{what}");
        let (screen, window) = sign_in(false);
        let (drawn, what) = signed_out(&screen, &window);
        assert!(!drawn, "{what}");
    }

    #[test]
    fn only_the_shells_line_says_the_app_exited() {
        let started = format!(
            "    2.83 | hyprix: started /bin/busybox sh -c 'HOME=/dev/shm /bin/claude-desktop \\
             --enable-logging=stderr; echo {EXITED}' as 28"
        );
        assert!(!exited(&started));
        assert!(exited(&format!("    2.84 | {EXITED}")));
    }

    #[test]
    fn the_carried_files_skip_what_is_there() {
        let all = desktop_files(&[]);
        assert!(all.iter().any(|file| file.path == "bin/claude-desktop"));
        let there = File {
            path: MIMEAPPS_PATH.to_owned(),
            mode: 0o644,
            content: Content::Bytes(Vec::new()),
        };
        let less = desktop_files(&[there]);
        assert_eq!(less.len(), all.len() - 1);
    }
}
