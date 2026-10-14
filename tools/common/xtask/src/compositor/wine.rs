//! `test-wine`'s second boot: Wine's notepad in a window of hyprix's, drawn
//! through yserver in software, typed into (stage 23, step 1; docs/WINE.md).
//!
//! The desktop is `test-xwindow`'s: hyprix with yserver started on `:0` as a
//! Wayland client of it. [`SCRIPT`], started beside the server, makes a
//! prefix, runs `cmd /c ver`, then a 32-bit Windows program,
//! `tools/common/test/window32.c` built with mingw-w64's i686 compiler,
//! which says it runs as a 32-bit process under `WoW64` and paints a window
//! in [`WOW64_COLOUR`] (stage 23, E1.6; docs/WINE.md §7). The gate requires
//! hyprix to list that window by [`WOW64_TITLE`] and the colour to cover
//! [`WOW64_PERCENT`] of the screen. Once it has closed, the script starts
//! `notepad` and waits for X to say its window is viewable. The gate then puts input in through QEMU, as
//! `test-xwindow` does for xev: a click into notepad's page, the keys of
//! [`LINE`], `Ctrl+A` and `Ctrl+C`, then a double click on the line's first
//! word and `Ctrl+C`, while the script prints what X's clipboard holds.
//!
//! It passes when hyprix lists a window with notepad's title, its white page
//! fills the tile hyprix gave it, the typed line is on the screen, and the
//! clipboard held the line and then its first word: the keys reached
//! notepad as the characters they are, and the double click reached the
//! word where it is drawn.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, button_event, press, undithered, with_the_transcript};
use super::{Carried, EITHER, Programs, gates_busybox};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// Where [`SCRIPT`] is in the image.
const SCRIPT_PATH: &str = "etc/wine.sh";

/// The title Wine's notepad gives its window.
const TITLE: &str = "Untitled - Notepad";

/// What the script prints once hyprix lists its windows with notepad up.
const WINDOW: &str = "wine: window";

/// The title the 32-bit program gives its window.
const WOW64_TITLE: &str = "Ferrix WoW64";

/// The colour the script asks the 32-bit program to paint its window in,
/// as [`SCRIPT`] passes it (`20c060`).
const WOW64_COLOUR: [u8; 3] = [0x20, 0xC0, 0x60];

/// What the script prints once hyprix lists its windows with the 32-bit
/// program's up. Not a line [`WINDOW`] is found in.
const WOW64_WINDOW: &str = "wine: wow64 window";

/// What the script puts before each line of hyprix's list while the 32-bit
/// program's window is up.
const WOW64_CLIENTS: &str = "wine: wow64 clients:";

/// What the script puts before each line the 32-bit program printed.
const WOW64_SAID: &str = "wine: wow64 said: ";

/// What the 32-bit program prints when it runs as one: four-byte pointers,
/// `IsWow64Process` true, and `IsWow64Process2`'s i386 process on an x86-64
/// machine.
const WOW64_AS: &str = "window32: pointer=4 wow64=1 process=014c native=8664";

/// The part of the screen, in percent, the 32-bit program's colour must
/// cover: its window fills the tile hyprix gives it, as notepad's does.
const WOW64_PERCENT: usize = 50;

/// What the script prints before each new content of X's clipboard.
const CLIPBOARD: &str = "wine: clipboard: ";

/// What the script prints last, whatever happened.
const END: &str = "wine: end";

/// Where the 32-bit program is in the image.
const WINDOW32_PATH: &str = "etc/window32.exe";

/// How long the script has to reach [`WINDOW`]: yserver's start on
/// lavapipe, `wineboot`, `cmd` and notepad's start.
const PATIENCE: Duration = Duration::from_secs(420);

/// The part of the screen notepad's white page must cover. Its window is
/// the 982 by 726 tile hyprix gives it on the 1024 by 768 screen, and the
/// page fills the tile but for the menu, scroll and status bars: 85% was
/// white on 2026-10-12. Until yserver set `WM_STATE`, notepad drew at the
/// 721 by 512 it asked for, and 43% was white (docs/WINE.md, section 5).
const WHITE_PERCENT: usize = 70;

/// The line typed into notepad, and the QEMU keys that type it. No `y` or
/// `z`, which a German layout swaps.
const LINE: &str = "hello ferrix";
/// See [`LINE`].
const LINE_KEYS: &[&str] = &["h", "e", "l", "l", "o", "spc", "f", "e", "r", "r", "i", "x"];
/// The first word of [`LINE`], which the double click selects.
const WORD: &str = "hello";

/// Dark pixels a typed line of [`LINE`]'s twelve characters puts on the
/// page at the least: about 300 were drawn on 2026-10-12.
const LINE_PIXELS: usize = 60;

/// The script hyprix's `exec-once` runs. Every line of its own starts
/// `wine:`, and it ends with [`END`] whatever happened.
///
/// Wine's output goes to files that are printed afterwards: `wineboot`
/// leaves services running that keep a pipe open. Notepad's includes the X
/// driver's warnings, which say how it took the sizes X gave its window.
/// After [`WINDOW`], the script prints X's clipboard each time it changes,
/// twice, for the input the gate puts in.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
echo "wine: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "wine: the X socket was there after ${waited}s, DISPLAY is $DISPLAY"
mkdir -p /tmp/wine
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "wine: wineboot exited $?"
/usr/lib/wine/wineserver64 -w
sed 's/^/wine: wineboot: /' /tmp/wine/wineboot.log
/usr/lib/wine/wine64 cmd /c ver > /tmp/wine/ver.txt 2> /tmp/wine/ver.err
echo "wine: cmd exited $?"
sed 's/^/wine: cmd: /' /tmp/wine/ver.txt
xdpyinfo | grep -E "dimensions|resolution" | sed 's/^/wine: x: /'
/usr/lib/wine/wine64 /etc/window32.exe 20c060 20 > /tmp/wine/window32.log 2>&1 &
waited=0
until xwininfo -name "Ferrix WoW64" 2>/dev/null | grep -q IsViewable || [ $waited -ge 60 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "wine: wow64: the 32-bit program's window was viewable after ${waited}s"
sleep 3
/bin/hyprctl clients | sed 's/^/wine: wow64 clients: /'
echo "wine: wow64 window"
wait
sed 's/^/wine: wow64 said: /' /tmp/wine/window32.log
WINEDEBUG="$WINEDEBUG,warn+x11drv" /usr/lib/wine/wine64 notepad > /tmp/wine/notepad.log 2>&1 &
waited=0
until xwininfo -name "Untitled - Notepad" 2>/dev/null | grep -q IsViewable || [ $waited -ge 60 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "wine: notepad's window was viewable after ${waited}s"
sleep 5
xwininfo -name "Untitled - Notepad" 2>&1 | grep -E "Width|Height|Map State" | sed 's/^/wine: notepad: /'
xprop -name "Untitled - Notepad" WM_STATE 2>&1 | sed 's/^/wine: notepad: /'
/bin/hyprctl clients | sed 's/^/wine: clients: /'
echo "wine: window"
last=""
changes=0
waited=0
while [ $changes -lt 2 ] && [ $waited -lt 120 ]; do
    now=$(xclip -o -selection clipboard 2>/dev/null)
    if [ -n "$now" ] && [ "$now" != "$last" ]; then
        echo "wine: clipboard: $now"
        last=$now
        changes=$((changes + 1))
    fi
    sleep 1
    waited=$((waited + 1))
done
echo "wine: the clipboard changed $changes times in ${waited}s"
sed 's/^/wine: notepad said: /' /tmp/wine/notepad.log
tail -n 20 /tmp/yserver.log | sed 's/^/wine: yserver: /'
/usr/lib/wine/wineserver64 -k
echo "wine: end"
"#;

/// Where `run-compositor --wine` carries [`DESKTOP_SCRIPT`].
const DESKTOP_SCRIPT_PATH: &str = "etc/wine-desktop.sh";

/// Where `run-compositor --wine` carries [`NOTEPAD_SCRIPT`].
const NOTEPAD_SCRIPT_PATH: &str = "etc/wine-notepad.sh";

/// What `run-compositor --wine` starts beside yserver: once the X socket is
/// there, a prefix in `/tmp` (about 40 s under KVM), then notepad. What
/// Wine says goes to `/tmp/wine/*.log`.
const DESKTOP_SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
mkdir -p /tmp/wine
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
/usr/lib/wine/wineserver64 -w
/usr/lib/wine/wine64 notepad > /tmp/wine/notepad.log 2>&1
"#;

/// What `SUPER+N` runs on `run-compositor --wine`'s desktop: another
/// notepad in the prefix [`DESKTOP_SCRIPT`] made.
const NOTEPAD_SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
exec /usr/lib/wine/wine64 notepad >> /tmp/wine/notepad.log 2>&1
"#;

/// What `run-compositor --wine` adds to the archive: yserver's desktop
/// files and links, Wine's links, and the two scripts.
///
/// `carried` is what the desktop's archive holds already. The desktop
/// carries foot's port, whose fontconfig lives in `/etc/fonts` and whose one
/// font is in `/usr/share/fonts`, so yserver's links for those two are left
/// out and the volume's fonts are linked in beside foot's, as the Chrome
/// desktop does (`desktop.rs`, `chrome_links`). Linking `/etc/fonts` over
/// foot's directory stopped the boot (2026-10-12: FX-0801, ENOTEMPTY).
pub(crate) fn desktop_files(carried: &[File]) -> Vec<File> {
    let taken = |path: &str| {
        carried.iter().any(|file| {
            file.path == path
                || file
                    .path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut files = crate::yserver::desktop_files(carried);
    let mut links: Vec<(&str, &str)> = crate::wine::LINKS
        .iter()
        .copied()
        .filter(|(path, _)| !taken(path))
        .collect();
    if taken("usr/share/fonts") && !taken("usr/share/fonts/truetype") {
        links.push(("usr/share/fonts/truetype", "/data/usr/share/fonts/truetype"));
    }
    if taken("etc/fonts") && !taken("etc/fonts/conf.d") {
        links.push(("etc/fonts/conf.d", "/data/etc/fonts/conf.d"));
    }
    files.extend(crate::rustc::files(&links));
    for (path, script) in [
        (DESKTOP_SCRIPT_PATH, DESKTOP_SCRIPT),
        (NOTEPAD_SCRIPT_PATH, NOTEPAD_SCRIPT),
    ] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o644,
            content: Content::Bytes(script.as_bytes().to_vec()),
        });
    }
    files
}

/// What `run-compositor --wine` adds to the desktop's configuration:
/// yserver on `:0`, [`DESKTOP_SCRIPT`] as the desktop starts, and `SUPER+N`
/// for another notepad.
pub(crate) fn with_wine(config: String, args: &Args) -> String {
    if !args.wine {
        return config;
    }
    println!("  x86_64: yserver on :0 and Wine's notepad, from the Wine volume");
    format!(
        "{config}\n{}# Added by `cargo xtask run-compositor --wine`.\n\
         env = XDG_RUNTIME_DIR,/tmp\n\
         exec-once = /bin/busybox sh /{DESKTOP_SCRIPT_PATH}\n\
         bind = SUPER, N, exec, /bin/busybox sh /{NOTEPAD_SCRIPT_PATH}\n",
        crate::yserver::desktop_config()
    )
}

/// The part of `screen`, in percent, that is pure white.
fn white_percent(screen: &Image) -> usize {
    let (pixels, _) = screen.pixels.as_chunks::<3>();
    let white = pixels
        .iter()
        .filter(|pixel| **pixel == [255, 255, 255])
        .count();
    white * 100 / pixels.len().max(1)
}

/// Whether hyprix's list, among the script's lines, has notepad's window.
fn listed(lines: &[String]) -> bool {
    listed_under(lines, "wine: clients:", TITLE)
}

/// Whether hyprix's list, as the script printed it after `prefix`, has a
/// window titled `title`.
fn listed_under(lines: &[String], prefix: &str, title: &str) -> bool {
    lines.iter().any(|line| {
        line.split_once(prefix)
            .and_then(|(_, rest)| rest.trim().split_once(" -> "))
            .is_some_and(|(_, listed)| listed.trim_end_matches(':') == title)
    })
}

/// The part of `screen`, in percent, that is exactly `colour`.
fn colour_percent(screen: &Image, colour: [u8; 3]) -> usize {
    let (pixels, _) = screen.pixels.as_chunks::<3>();
    let matching = pixels.iter().filter(|pixel| **pixel == colour).count();
    matching * 100 / pixels.len().max(1)
}

/// A rectangle on the screen: left, top, width, height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

/// Where hyprix put notepad's window, from its `at:` and `size:` in the
/// list the script printed.
fn tile(lines: &[String]) -> Option<Rect> {
    let mut entries = lines
        .iter()
        .filter_map(|line| line.split_once("wine: clients:"))
        .map(|(_, rest)| rest.trim())
        .skip_while(|entry| {
            entry
                .split_once(" -> ")
                .is_none_or(|(_, title)| title.trim_end_matches(':') != TITLE)
        })
        .skip(1)
        .take_while(|entry| !entry.contains(" -> "));
    let pair = |value: &str| -> Option<(usize, usize)> {
        let (a, b) = value.trim().split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    };
    let (mut at, mut size) = (None, None);
    for entry in entries.by_ref() {
        if let Some(value) = entry.strip_prefix("at:") {
            at = pair(value);
        } else if let Some(value) = entry.strip_prefix("size:") {
            size = pair(value);
        }
    }
    let ((x, y), (width, height)) = (at?, size?);
    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

/// The pixels inside `within` that are dark in `after` and were white in
/// `before`, and the rectangle around them: what was drawn on notepad's
/// page between the two pictures.
fn drawn(before: &Image, after: &Image, within: Rect) -> (usize, Option<Rect>) {
    let pixel = |image: &Image, x: usize, y: usize| -> Option<[u8; 3]> {
        let at = (y.checked_mul(image.width)?.checked_add(x)?).checked_mul(3)?;
        image.pixels.get(at..at + 3)?.try_into().ok()
    };
    let mut count = 0;
    let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
    for y in within.y..within.y + within.height {
        for x in within.x..within.x + within.width {
            let was_white = pixel(before, x, y).is_some_and(|rgb| rgb == [255, 255, 255]);
            let dark = pixel(after, x, y).is_some_and(|rgb| rgb.iter().all(|&c| c < 0x80));
            if was_white && dark {
                count += 1;
                left = left.min(x);
                top = top.min(y);
                right = right.max(x);
                bottom = bottom.max(y);
            }
        }
    }
    let rect = (count > 0).then(|| Rect {
        x: left,
        y: top,
        width: right - left + 1,
        height: bottom - top + 1,
    });
    (count, rect)
}

/// Notepad's page inside its window `tile`: below the menu bar, above the
/// status bar, left of the scroll bar.
fn page(tile: Rect) -> Rect {
    Rect {
        x: tile.x + 4,
        y: tile.y + 26,
        width: tile.width.saturating_sub(4 + 24),
        height: tile.height.saturating_sub(26 + 34),
    }
}

/// What the script said X's clipboard held, in order.
fn clipboard(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| line.split_once(CLIPBOARD))
        .map(|(_, text)| text.trim_end().to_owned())
        .collect()
}

/// A screendump, read.
fn screen(qmp: &mut Qmp, dump: &Path) -> Result<Image> {
    qmp.screendump(Some(DEVICE_ID), dump)?;
    let bytes = std::fs::read(dump)
        .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
    parse_ppm(&bytes)
}

/// Put the input in: a click into notepad's `tile` at its middle, the keys
/// of [`LINE`], `Ctrl+A` and `Ctrl+C`; once the script says the clipboard
/// changed and the line is drawn, a double click on its first word and
/// `Ctrl+C`. The picture with the line typed is returned, `None` when no
/// line came.
fn type_into(
    qmp: &mut Qmp,
    watching: &mut Watching<'_>,
    blank: &Image,
    tile: Rect,
    dump: &Path,
) -> Result<Option<Image>> {
    let pause = |millis| std::thread::sleep(Duration::from_millis(millis));
    let tablet = |at: usize, across: usize| {
        let at = i64::try_from(at).unwrap_or(0);
        let across = i64::try_from(across.max(1)).unwrap_or(1);
        i32::try_from(at * 0x7FFF / across).unwrap_or(0)
    };
    let (width, height) = (blank.width, blank.height);
    let point = |qmp: &mut Qmp, x: usize, y: usize| -> Result<()> {
        qmp.input_send_event(&[
            absolute("x", tablet(x, width)),
            absolute("y", tablet(y, height)),
        ])
    };
    let click = |qmp: &mut Qmp| -> Result<()> {
        qmp.input_send_event(&[button_event("left", true)])?;
        pause(60);
        qmp.input_send_event(&[button_event("left", false)])
    };
    let copied = |count: usize| {
        move |lines: &[String]| {
            clipboard(lines).len() >= count || lines.iter().any(|line| line.contains(END))
        }
    };

    point(qmp, tile.x + tile.width / 2, tile.y + tile.height / 2)?;
    pause(500);
    click(qmp)?;
    pause(500);
    for key in LINE_KEYS {
        press(qmp, &[key])?;
        pause(80);
    }
    // The line, once drawn: dark where the page was white.
    let deadline = Instant::now() + Duration::from_secs(20);
    let typed = loop {
        let shown = screen(qmp, dump)?;
        if drawn(blank, &shown, page(tile)).0 >= LINE_PIXELS || Instant::now() >= deadline {
            break shown;
        }
        pause(500);
    };
    let (_, line) = drawn(blank, &typed, page(tile));
    press(qmp, &["ctrl", "a"])?;
    pause(300);
    press(qmp, &["ctrl", "c"])?;
    let _ = watching.read_more(Instant::now() + Duration::from_secs(60), copied(1))?;
    if let Some(line) = line {
        // The first word: a few pixels into the line, at its middle.
        point(qmp, line.x + 6, line.y + line.height / 2)?;
        pause(500);
        click(qmp)?;
        pause(80);
        click(qmp)?;
        pause(500);
        press(qmp, &["ctrl", "c"])?;
        let _ = watching.read_more(Instant::now() + Duration::from_secs(60), copied(2))?;
    }
    Ok(line.map(|_| typed))
}

/// What the gate's boot carries besides the desktop's programs: yserver's
/// desktop files, Wine's links, [`SCRIPT`] and the 32-bit program, built
/// here with mingw-w64.
///
/// # Errors
///
/// The 32-bit program cannot be built.
fn gate_files() -> Result<Vec<File>> {
    let mut files = crate::yserver::desktop_files(&[]);
    files.extend(crate::rustc::files(crate::wine::LINKS));
    files.push(File {
        path: SCRIPT_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(SCRIPT.as_bytes().to_vec()),
    });
    files.push(File {
        path: WINDOW32_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(crate::wine::windows_program(
            "i686-w64-mingw32-gcc",
            "window32",
            &["-lgdi32"],
        )?),
    });
    Ok(files)
}

/// Boot hyprix with yserver and [`SCRIPT`], type into notepad, and require
/// its window listed by hyprix, its page filling the tile, the typed line on
/// the screen and the clipboard holding what was copied.
///
/// # Errors
///
/// When the image cannot be built, the boot fails, `cmd` did not answer,
/// notepad's window is not listed or not drawn at its tile's size, or the
/// typed line or the copies did not come.
pub(crate) fn test_wine_window(args: &Args, volume: PathBuf) -> Result<()> {
    let arch = Arch::X86_64;
    let mut args = args.clone();
    args.gl = false;
    args.data_image = Some(volume);
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-wine needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
    })?;
    let programs = Programs::build(arch)?;
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports: gate_files()?,
        ..Carried::none()
    };
    let config = format!(
        "# Carried into the initramfs by `cargo xtask test-wine`.\n\
         env = XDG_RUNTIME_DIR,/tmp\n{}exec-once = /bin/busybox sh /{SCRIPT_PATH}\n",
        crate::yserver::desktop_config(),
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("wine-notepad.ppm");
    let wow64_dump = paths::build_dir(arch).join("wine-wow64.ppm");
    let typed_dump = paths::build_dir(arch).join("wine-notepad-typed.ppm");
    println!("  {arch}: Wine's notepad on hyprix through yserver, typed into");
    let mut said: Vec<String> = Vec::new();
    let mut blank: Option<Image> = None;
    let mut typed: Option<Image> = None;
    let mut placed: Option<Rect> = None;
    let mut wow64_screen: Option<Image> = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let reached = |marker: &'static str| {
            move |lines: &[String]| {
                lines
                    .iter()
                    .any(|line| line.contains(marker) || line.contains(END))
            }
        };
        if watching.read_more(Instant::now() + PATIENCE, reached(WOW64_WINDOW))? {
            wow64_screen = Some(screen(&mut qmp, &wow64_dump)?);
        }
        let window = watching.read_more(Instant::now() + PATIENCE, reached(WINDOW))?;
        let shown = screen(&mut qmp, &dump)?;
        // The lines since the hook began, where the script's are.
        placed = tile(watching.after());
        if window && let Some(at) = placed {
            typed = type_into(&mut qmp, watching, &shown, at, &typed_dump)?;
        }
        blank = Some(shown);
        let ended = watching.read_more(Instant::now() + Duration::from_secs(150), reached(END))?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        if ended {
            watching.stop_when_done();
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: the Wine script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    let white = judge_window(arch, &said, blank.as_ref(), placed, &dump)?;
    println!(
        "  {arch}: Wine's notepad is a window of hyprix's, its page fills its tile ({white}% \
         white), {}",
        dump.display()
    );
    let covered = judge_wow64(arch, &said, wow64_screen.as_ref(), &wow64_dump)?;
    println!(
        "  {arch}: a 32-bit Windows program ran under WoW64 (`{WOW64_AS}`) and its window is \
         hyprix's, painted in its colour over {covered}% of the screen, {}",
        wow64_dump.display()
    );
    let (Some(blank), Some(at)) = (blank.as_ref(), placed) else {
        return Err(Error::new(format!(
            "{arch}: hyprix's list gave no place for notepad's window"
        )));
    };
    judge_typing(arch, &said, blank, typed.as_ref(), at, &typed_dump)
}

/// Whether `cmd` answered, hyprix listed notepad's window, and its page
/// filled its tile on the screen `blank`; the screen's white part.
fn judge_window(
    arch: Arch,
    said: &[String],
    blank: Option<&Image>,
    placed: Option<Rect>,
    dump: &Path,
) -> Result<usize> {
    if !said
        .iter()
        .any(|line| line.contains("wine: cmd:") && line.contains(crate::wine::VERSION))
    {
        return Err(Error::new(format!(
            "{arch}: Wine's cmd did not print `{}` on the desktop; the `wine:` lines say how \
             far it came",
            crate::wine::VERSION
        )));
    }
    if !listed(said) {
        return Err(Error::new(format!(
            "{arch}: hyprix lists no window titled `{TITLE}`; the `wine: notepad` and \
             `wine: clients:` lines say what X and hyprix had"
        )));
    }
    let white = blank.map_or(0, white_percent);
    if white < WHITE_PERCENT {
        return Err(Error::new(format!(
            "{arch}: hyprix lists notepad's window, but {white}% of the screen is white, less \
             than the {WHITE_PERCENT}% of its page filling its tile {placed:?}: {}",
            dump.display()
        )));
    }
    Ok(white)
}

/// Whether the 32-bit program said it ran as one under `WoW64`, hyprix listed
/// its window, and its colour covered [`WOW64_PERCENT`] of the screen
/// `shown`; the part it covered.
fn judge_wow64(arch: Arch, said: &[String], shown: Option<&Image>, dump: &Path) -> Result<usize> {
    let ran_as: Vec<&str> = said
        .iter()
        .filter_map(|line| line.split_once(WOW64_SAID))
        .map(|(_, rest)| rest.trim())
        .collect();
    if !ran_as.iter().any(|line| line.starts_with(WOW64_AS)) {
        return Err(Error::new(format!(
            "{arch}: the 32-bit program did not say `{WOW64_AS}`; it said {ran_as:?}"
        )));
    }
    if !listed_under(said, WOW64_CLIENTS, WOW64_TITLE) {
        return Err(Error::new(format!(
            "{arch}: hyprix lists no window titled `{WOW64_TITLE}` while the 32-bit program \
             ran; the `{WOW64_CLIENTS}` lines say what it had"
        )));
    }
    let covered = shown.map_or(0, |shown| colour_percent(shown, WOW64_COLOUR));
    if covered < WOW64_PERCENT {
        return Err(Error::new(format!(
            "{arch}: hyprix lists the 32-bit program's window, but its colour {WOW64_COLOUR:02x?} \
             covers {covered}% of the screen, less than {WOW64_PERCENT}%: {}",
            dump.display()
        )));
    }
    Ok(covered)
}

/// Whether the keys made a line on notepad's page in its tile `at`, between
/// the screens `blank` and `typed`, and the clipboard held the line and then
/// its first word.
fn judge_typing(
    arch: Arch,
    said: &[String],
    blank: &Image,
    typed: Option<&Image>,
    at: Rect,
    typed_dump: &Path,
) -> Result<()> {
    let line = typed
        .map(|typed| drawn(blank, typed, page(at)))
        .filter(|(count, line)| {
            *count >= LINE_PIXELS && line.is_some_and(|line| line.height <= 24)
        });
    let Some((count, Some(line))) = line else {
        return Err(Error::new(format!(
            "{arch}: no line of {LINE_PIXELS} or more dark pixels was drawn on notepad's page \
             after the keys of {LINE:?}: {}",
            typed_dump.display()
        )));
    };
    let held = clipboard(said);
    let wanted = [LINE, WORD];
    let matches = held.len() == 2
        && held
            .iter()
            .zip(wanted)
            .all(|(held, wanted)| held.trim() == wanted);
    if !matches {
        return Err(Error::new(format!(
            "{arch}: X's clipboard held {held:?} after Ctrl+A, Ctrl+C and a double click on \
             the first word and Ctrl+C, not {wanted:?}"
        )));
    }
    println!(
        "  {arch}: the keys of {LINE:?} are a line on notepad's page ({count} pixels, {}x{} at \
         ({}, {})), and the clipboard held {held:?}, the line and the word double-clicked: {}",
        line.width,
        line.height,
        line.x,
        line.y,
        typed_dump.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a desktop that carries foot's fontconfig, `--wine` links nothing
    /// over `/etc/fonts` or `/usr/share/fonts` and links the volume's fonts in
    /// beside them; on one that carries neither, it links both whole.
    #[test]
    fn the_wine_desktop_links_nothing_over_foots_fonts() {
        let foot = [
            File {
                path: "etc/fonts/fonts.conf".to_owned(),
                mode: 0o644,
                content: Content::Bytes(b"<fontconfig/>".to_vec()),
            },
            File {
                path: "usr/share/fonts/foot.ttf".to_owned(),
                mode: 0o644,
                content: Content::Bytes(Vec::new()),
            },
        ];
        let links = |files: &[File]| -> Vec<String> {
            files
                .iter()
                .filter(|file| matches!(file.content, Content::Link(_)))
                .map(|file| file.path.clone())
                .collect()
        };
        let beside = links(&desktop_files(&foot));
        for over in ["etc/fonts", "usr/share/fonts"] {
            assert!(
                !beside.iter().any(|path| path == over),
                "{over} linked over foot's"
            );
        }
        for added in [
            "etc/fonts/conf.d",
            "usr/share/fonts/truetype",
            "usr/lib/wine",
        ] {
            assert!(beside.iter().any(|path| path == added), "{added} missing");
        }
        let alone = links(&desktop_files(&[]));
        assert!(alone.iter().any(|path| path == "etc/fonts"));
        assert!(!alone.iter().any(|path| path == "etc/fonts/conf.d"));
    }

    /// Notepad's window is found by its title in hyprix's list, and no other
    /// window's title stands in for it.
    #[test]
    fn notepads_window_is_found_by_its_title() {
        let lines = |title: &str| {
            vec![
                format!("wine: clients: Window 5a -> {title}:"),
                "wine: clients:     class: notepad.exe".to_owned(),
            ]
        };
        assert!(listed(&lines(TITLE)));
        assert!(!listed(&lines("Event Tester")));
        assert!(!listed(&["wine: notepad: Untitled - Notepad".to_owned()]));
    }

    /// The 32-bit program's window is found under its own prefix, by its
    /// title, and its markers are not ones notepad's are found in.
    #[test]
    fn the_wow64_window_is_found_apart_from_notepads() {
        let lines = vec![
            format!("{WOW64_CLIENTS} Window 5a -> {WOW64_TITLE}:"),
            format!("{WOW64_CLIENTS}     class: window32.exe"),
        ];
        assert!(listed_under(&lines, WOW64_CLIENTS, WOW64_TITLE));
        assert!(!listed(&lines), "notepad's list is not the 32-bit one's");
        assert!(!WOW64_WINDOW.contains(WINDOW) && !WOW64_CLIENTS.contains("wine: clients:"));
        assert!(SCRIPT.contains(&format!("/{WINDOW32_PATH} 20c060 ")));
        assert_eq!(
            WOW64_COLOUR,
            [0x20, 0xC0, 0x60],
            "the colour the script passes"
        );
        assert!(SCRIPT.contains(&format!("xwininfo -name \"{WOW64_TITLE}\"")));
        assert!(SCRIPT.contains(&format!("echo \"{WOW64_WINDOW}\"")));
    }

    /// A colour's part of the screen is counted in whole percent, exactly.
    #[test]
    fn a_colour_is_counted_exactly() {
        let mut pixels = WOW64_COLOUR.repeat(3);
        pixels.extend([0x20, 0xC0, 0x61]);
        let screen = Image {
            width: 2,
            height: 2,
            pixels,
        };
        assert_eq!(colour_percent(&screen, WOW64_COLOUR), 75);
    }

    /// A screen's white part is counted in whole percent.
    #[test]
    fn white_is_counted_in_percent() {
        let mut pixels = vec![255_u8; 3 * 3];
        pixels.extend([0_u8; 3]);
        let screen = Image {
            width: 2,
            height: 2,
            pixels,
        };
        assert_eq!(white_percent(&screen), 75);
    }

    /// Notepad's place is its own entry's `at:` and `size:`, not another
    /// window's.
    #[test]
    fn notepads_tile_is_read_from_its_entry() {
        let lines: Vec<String> = [
            "Window 1 -> Event Tester:",
            "\tat: 500,21",
            "\tsize: 400,300",
            "Window 2 -> Untitled - Notepad:",
            "\tmapped: true",
            "\tat: 21,21",
            "\tsize: 982,726",
            "Window 3 -> Other:",
            "\tat: 1,1",
        ]
        .iter()
        .map(|entry| format!("  12.00 | wine: clients: {entry}"))
        .collect();
        assert_eq!(
            tile(&lines),
            Some(Rect {
                x: 21,
                y: 21,
                width: 982,
                height: 726
            })
        );
        assert_eq!(tile(&lines[..3]), None);
    }

    /// What was drawn is what turned from white to dark inside the page,
    /// not what was dark before or lies outside.
    #[test]
    fn drawn_counts_white_turned_dark_inside() {
        let blank = Image {
            width: 4,
            height: 2,
            pixels: vec![255; 4 * 2 * 3],
        };
        let mut after = Image {
            width: 4,
            height: 2,
            pixels: blank.pixels.clone(),
        };
        for at in [1_usize, 2, 7] {
            after.pixels[at * 3..at * 3 + 3].copy_from_slice(&[10, 10, 10]);
        }
        let within = Rect {
            x: 0,
            y: 0,
            width: 3,
            height: 2,
        };
        assert_eq!(
            drawn(&blank, &after, within),
            (
                2,
                Some(Rect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 1
                })
            )
        );
    }

    /// The script prints the clipboard where the gate reads it, and the
    /// keys type the line.
    #[test]
    fn the_clipboard_is_read_back_and_the_keys_spell_the_line() {
        assert!(SCRIPT.contains(&format!("echo \"{CLIPBOARD}$now\"")));
        let lines = vec![
            format!("  40.0 | {CLIPBOARD}{LINE}"),
            format!("  41.0 | {CLIPBOARD}{WORD} "),
        ];
        assert_eq!(
            clipboard(&lines),
            [LINE, WORD],
            "read without the trailing space"
        );
        let spelt: String = LINE_KEYS
            .iter()
            .map(|key| if *key == "spc" { " " } else { key })
            .collect();
        assert_eq!(spelt, LINE);
        assert!(LINE.starts_with(WORD));
    }
}
