//! `test-wine`'s second boot: Wine's notepad in a window of hyprix's, drawn
//! through yserver in software (stage 23, step 1; docs/WINE.md).
//!
//! The desktop is `test-xwindow`'s: hyprix with yserver started on `:0` as a
//! Wayland client of it. [`SCRIPT`], started beside the server, makes a
//! prefix, runs `cmd /c ver`, then starts `notepad` and waits for X to say
//! its window is viewable. The gate passes when hyprix lists a window with
//! notepad's title and the screen shows its white page.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::boot::{build_image, undithered, with_the_transcript};
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

/// The script hyprix's `exec-once` runs. Every line of its own starts
/// `wine:`, and it ends with [`END`] whatever happened.
///
/// Wine's output goes to files that are printed afterwards: `wineboot`
/// leaves services running that keep a pipe open. Notepad's includes the X
/// driver's warnings, which say how it took the sizes X gave its window.
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
xwininfo -name "Untitled - Notepad" 2>&1 | grep -E "Width|Height|Map State" | sed 's/^/wine: notepad: /'
sleep 5
/bin/hyprctl clients | sed 's/^/wine: clients: /'
echo "wine: window"
sleep 20
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

/// Boot hyprix with yserver and [`SCRIPT`], and require notepad's window
/// listed by hyprix and its page on the screen.
///
/// # Errors
///
/// When the image cannot be built, the boot fails, `cmd` did not answer,
/// or notepad's window is not listed or not drawn.
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
    println!("  {arch}: Wine's notepad on hyprix through yserver");
    let mut said: Vec<String> = Vec::new();
    let mut screen: Option<Image> = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let reached = |marker: &'static str| {
            move |lines: &[String]| {
                lines
                    .iter()
                    .any(|line| line.contains(marker) || line.contains(END))
            }
        };
        let _ = watching.read_more(Instant::now() + PATIENCE, reached(WINDOW))?;
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        let bytes = std::fs::read(&dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        screen = Some(parse_ppm(&bytes)?);
        let ended = watching.read_more(Instant::now() + Duration::from_secs(90), reached(END))?;
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
    if !listed(&said) {
        return Err(Error::new(format!(
            "{arch}: hyprix lists no window titled `{TITLE}`; the `wine: notepad` and \
             `wine: clients:` lines say what X and hyprix had"
        )));
    }
    let white = screen.as_ref().map_or(0, white_percent);
    if white < WHITE_PERCENT {
        return Err(Error::new(format!(
            "{arch}: hyprix lists notepad's window, but {white}% of the screen is white, less \
             than the {WHITE_PERCENT}% of its page: {}",
            dump.display()
        )));
    }
    println!(
        "  {arch}: Wine's notepad is a window of hyprix's and its page is on the screen \
         ({white}% white), {}",
        dump.display()
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
}
