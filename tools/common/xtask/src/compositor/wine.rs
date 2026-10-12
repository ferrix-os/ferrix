//! `test-wine`'s second boot: Wine's notepad in a window of hyprix's, drawn
//! through yserver in software, typed into (stage 23, step 1; docs/WINE.md).
//!
//! The desktop is `test-xwindow`'s: hyprix with yserver started on `:0` as a
//! Wayland client of it. [`SCRIPT`], started beside the server, makes a
//! prefix, runs `cmd /c ver`, then starts `notepad` and waits for X to say
//! its window is viewable. The gate then puts input in through QEMU, as
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

/// What the script prints before each new content of X's clipboard.
const CLIPBOARD: &str = "wine: clipboard: ";

/// What the script prints last, whatever happened.
const END: &str = "wine: end";

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
    lines.iter().any(|line| {
        line.split_once("wine: clients:")
            .and_then(|(_, rest)| rest.trim().split_once(" -> "))
            .is_some_and(|(_, title)| title.trim_end_matches(':') == TITLE)
    })
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
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.extend(crate::rustc::files(crate::wine::LINKS));
    ports.push(File {
        path: SCRIPT_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(SCRIPT.as_bytes().to_vec()),
    });
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports,
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
    let typed_dump = paths::build_dir(arch).join("wine-notepad-typed.ppm");
    println!("  {arch}: Wine's notepad on hyprix through yserver, typed into");
    let mut said: Vec<String> = Vec::new();
    let mut blank: Option<Image> = None;
    let mut typed: Option<Image> = None;
    let mut placed: Option<Rect> = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let reached = |marker: &'static str| {
            move |lines: &[String]| {
                lines
                    .iter()
                    .any(|line| line.contains(marker) || line.contains(END))
            }
        };
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
