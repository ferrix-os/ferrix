//! `test-dxvk`'s second boot: the Direct3D 11 program presenting through
//! DXVK in a window of hyprix's, by way of Wine's Vulkan surface on
//! yserver's X11 (stage 23, E2.4; docs/WINE.md §6).
//!
//! The desktop is `test-wine`'s second boot: hyprix with yserver on `:0`.
//! [`SCRIPT`] makes a prefix, then runs `d3d11-triangle.exe` presenting for
//! [`SECONDS`] and says [`WINDOW`] once it has presented for a while, when
//! xtask takes the screen. The gate passes when the program's first frame
//! is the one `crate::dxvk` draws, it presented [`PRESENTS`] frames or more,
//! and the screen shows its clear colour and its triangle's, which come
//! there only through the swap chain.
//!
//! Then the script maximizes the window, as the title bar's button does,
//! and says [`MAXIMIZED`] with hyprix's view of it; xtask takes the screen
//! again and clicks into the window and types a key. A windowed game's
//! window must follow a maximize as it does on Linux's desktops: the
//! program must be told the window's new client size (`WM_SIZE`), make its
//! buffers that size, and the screen must show those new frames -- green,
//! which the program draws only at the new size -- over the window, not an
//! old frame. The click and the key must reach the program after it
//! (2026-10-28: Endfield on the N150 drew its old 1280 by 720 corner with
//! the rest black, showing a stale frame, after a maximize).

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
const SCRIPT_PATH: &str = "etc/dxvk.sh";

/// What the script prints once the program has presented for a while.
const WINDOW: &str = "dxvk: window";

/// What the script prints once the window is maximized, before each line
/// of `hyprctl clients` after it.
const MAXIMIZED: &str = "dxvk: maximized";

/// The key xtask types into the maximized window, as QMP names it, and the
/// virtual-key code the program must print for it.
const KEY: (&str, &str) = ("a", "41");

/// What the script prints last, whatever happened.
const END: &str = "dxvk: end";

/// What the script puts before each line of the program's.
pub(crate) const PREFIX: &str = "dxvk: said: ";

/// How long the program presents, in seconds; the same number is in
/// [`SCRIPT`].
const SECONDS: u32 = 45;

/// How many presents the program must have made in [`SECONDS`]: one a
/// second, so that a swap chain that stops after its first frames fails.
const PRESENTS: u32 = SECONDS;

/// How long the script has to reach [`WINDOW`]: yserver's start, `wineboot`
/// and the program's start.
const PATIENCE: Duration = Duration::from_secs(420);

/// The part of the screen, in thousandths, that must be the program's clear
/// colour, and the part that must be its triangle's. DXVK scales the 64 by
/// 64 buffer, seven eighths blue and an eighth the triangle, to the window.
/// The window is drawn at the size the program asked for, about 500 by
/// 480 of the 1024 by 768, and not at its tile's, as notepad's is
/// (docs/WINE.md §5): 267‰ blue and 37‰ triangle on 2026-10-12.
const CLEAR_PERMILLE: usize = 150;
const TRIANGLE_PERMILLE: usize = 20;

/// The part of the screen, in thousandths, that must be the program's
/// green once its window is maximized, and the most that may still be its
/// blue. The work area is the screen less hyprix's bar and gaps, and the
/// triangle an eighth of it: about 800‰ green.
const GREEN_PERMILLE: usize = 500;
const STALE_PERMILLE: usize = 20;

/// The green the program clears to at a new size.
const GREEN: [u8; 3] = [0x00, 0xff, 0x00];

/// The script hyprix's `exec-once` runs. Every line of its own starts
/// `dxvk:`, and it ends with [`END`] whatever happened. The prefix is made
/// as `test-wine`'s second boot makes it, with its workaround
/// (docs/WINE.md §5).
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
export VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/lvp_icd.json
echo "dxvk: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "dxvk: the X socket was there after ${waited}s, DISPLAY is $DISPLAY"
mkdir -p /tmp/wine
display=$DISPLAY
unset DISPLAY
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "dxvk: wineboot exited $?"
/usr/lib/wine/wineserver64 -w
/usr/lib/wine/wine64 reg add 'HKCU\Software\Wine\X11 Driver' /v UseXRandR /d N /f > /tmp/wine/reg.log 2>&1
echo "dxvk: reg exited $?"
/usr/lib/wine/wineserver64 -w
export DISPLAY=$display
cd /dxvk
WINEDLLOVERRIDES="d3d11,dxgi=n" DXVK_LOG_LEVEL=info DXVK_LOG_PATH=none \
    /usr/lib/wine/wine64 /dxvk/d3d11-triangle.exe ff8020 45 > /tmp/present.txt 2>&1 &
program=$!
waited=0
until grep -q "presented=" /tmp/present.txt || [ $waited -ge 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "dxvk: the program presented after ${waited}s"
sleep 5
/bin/hyprctl clients | sed 's/^/dxvk: clients: /'
echo "dxvk: window"
sleep 3
# The title bar's maximize button, on the focused window: the program's.
/bin/hyprctl dispatch snapwindow max | sed 's/^/dxvk: snapwindow: /'
waited=0
until [ "$(grep -c '^d3d11-triangle: buffers' /tmp/present.txt)" -ge 2 ] || [ $waited -ge 20 ]; do
    sleep 1
    waited=$((waited + 1))
done
sleep 3
/bin/hyprctl clients | sed 's/^/dxvk: maximized: /'
echo "dxvk: maximized"
wait $program
echo "dxvk: the program exited $?"
grep -v '^info:    ' /tmp/present.txt | sed 's/^/dxvk: said: /'
tail -n 20 /tmp/yserver.log | sed 's/^/dxvk: yserver: /'
/usr/lib/wine/wineserver64 -k
echo "dxvk: end"
"#;

/// The parts of `screen`, in thousandths, that are exactly `clear` and
/// exactly `triangle`.
fn coverage(screen: &Image, clear: [u8; 3], triangle: [u8; 3]) -> (usize, usize) {
    let (pixels, _) = screen.pixels.as_chunks::<3>();
    let count = |colour: [u8; 3]| pixels.iter().filter(|pixel| **pixel == colour).count();
    let whole = pixels.len().max(1);
    (count(clear) * 1000 / whole, count(triangle) * 1000 / whole)
}

/// The most presents the program reported among `lines`, after
/// [`PREFIX`], and whether it said it was done.
fn presents(lines: &[String]) -> (u32, bool) {
    let mut most = 0;
    let mut done = false;
    for line in lines {
        let Some((_, said)) = line.split_once(PREFIX) else {
            continue;
        };
        let Some(count) = said
            .split_whitespace()
            .find_map(|item| item.strip_prefix("presented="))
            .and_then(|count| count.parse::<u32>().ok())
        else {
            continue;
        };
        most = most.max(count);
        done |= said.trim_end().ends_with(", done");
    }
    (most, done)
}

/// Boot hyprix with yserver and [`SCRIPT`], and require the program's first
/// frame, its presents, and its colours on the screen.
///
/// # Errors
///
/// When the image cannot be built, the boot fails, or the frame, the
/// presents or the screen are not what they must be.
pub(crate) fn test_dxvk_window(args: &Args, volume: PathBuf, dxvk: &Path) -> Result<()> {
    let arch = Arch::X86_64;
    let mut args = args.clone();
    args.gl = false;
    args.data_image = Some(volume);
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-dxvk needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
    })?;
    let programs = Programs::build(arch)?;
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.extend(crate::rustc::files(crate::wine::LINKS));
    ports.extend(crate::dxvk::files(dxvk)?);
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
        "# Carried into the initramfs by `cargo xtask test-dxvk`.\n\
         env = XDG_RUNTIME_DIR,/tmp\n{}exec-once = /bin/busybox sh /{SCRIPT_PATH}\n",
        crate::yserver::desktop_config(),
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("dxvk-window.ppm");
    println!("  {arch}: d3d11-triangle presenting through DXVK on hyprix through yserver");
    let mut said: Vec<String> = Vec::new();
    let mut screen: Option<Image> = None;
    let mut maximized: Option<Image> = None;
    let maximized_dump = paths::build_dir(arch).join("dxvk-maximized.ppm");
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
        if watching.read_more(Instant::now() + Duration::from_secs(60), reached(MAXIMIZED))? {
            qmp.screendump(Some(DEVICE_ID), &maximized_dump)?;
            let bytes = std::fs::read(&maximized_dump).map_err(|error| {
                Error::new(format!("reading {}: {error}", maximized_dump.display()))
            })?;
            let image = parse_ppm(&bytes)?;
            // A click in the middle of the screen, which the maximized
            // window covers, then the key.
            let pause = |millis| std::thread::sleep(Duration::from_millis(millis));
            qmp.input_send_event(&[absolute("x", 0x4000), absolute("y", 0x4000)])?;
            pause(300);
            qmp.input_send_event(&[button_event("left", true)])?;
            pause(100);
            qmp.input_send_event(&[button_event("left", false)])?;
            pause(300);
            press(&mut qmp, &[KEY.0])?;
            maximized = Some(image);
        }
        let ended = watching.read_more(
            Instant::now() + Duration::from_secs(u64::from(SECONDS) + 90),
            reached(END),
        )?;
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
                &Error::new(format!("{arch}: the DXVK script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_window(arch, &said, screen.as_ref(), &dump)?;
    judge_maximized(arch, &said, maximized.as_ref(), &maximized_dump)
}

/// The size `hyprctl clients` gave the program's window among `lines`
/// after `prefix`: the `size: W,H` of the client whose title is the
/// program's.
fn window_size(lines: &[String], prefix: &str) -> Option<(u32, u32)> {
    let mut ours = false;
    let mut size = None;
    for line in lines {
        let Some((_, rest)) = line.split_once(prefix) else {
            continue;
        };
        let rest = rest.trim();
        if rest.starts_with("Window ") {
            ours = rest.ends_with("-> d3d11-triangle:");
        } else if ours && let Some(value) = rest.strip_prefix("size: ") {
            let (width, height) = value.split_once(',')?;
            size = Some((width.trim().parse().ok()?, height.trim().parse().ok()?));
        }
    }
    size
}

/// The program's lines among `lines` that start with `word`, after it.
fn program_said<'a>(lines: &'a [String], word: &str) -> Vec<&'a str> {
    lines
        .iter()
        .filter_map(|line| line.split_once(PREFIX))
        .filter_map(|(_, said)| said.strip_prefix("d3d11-triangle: ")?.strip_prefix(word))
        .map(str::trim)
        .collect()
}

/// Judge the maximize: the program told the window's new size, its
/// buffers made that size, its new frames on the screen and no old one,
/// and the click and the key after it reaching it.
///
/// # Errors
///
/// When any of those is not so.
fn judge_maximized(arch: Arch, said: &[String], screen: Option<&Image>, dump: &Path) -> Result<()> {
    let prefix = format!("{MAXIMIZED}: ");
    let before = window_size(said, "dxvk: clients: ");
    let Some((width, height)) = window_size(said, &prefix) else {
        return Err(Error::new(format!(
            "{arch}: the script never said the maximized window's size (`{MAXIMIZED}` lines)"
        )));
    };
    if before == Some((width, height)) {
        return Err(Error::new(format!(
            "{arch}: hyprix did not maximize the program's window: it stayed {width}x{height}"
        )));
    }
    let wanted = format!("{width}x{height}");
    let clients = program_said(said, "client ");
    if clients.last() != Some(&wanted.as_str()) {
        return Err(Error::new(format!(
            "{arch}: the window was maximized to {wanted}, but the program was told the client \
             sizes {clients:?} (WM_SIZE): Wine never saw the new size"
        )));
    }
    let buffers = program_said(said, "buffers ");
    if buffers.last() != Some(&wanted.as_str()) {
        return Err(Error::new(format!(
            "{arch}: the program's buffers are {buffers:?}, not the window's {wanted}"
        )));
    }
    let (green, blue) = screen.map_or((0, 1000), |screen| {
        coverage(screen, GREEN, crate::dxvk::CLEAR_RGB)
    });
    if green < GREEN_PERMILLE || blue > STALE_PERMILLE {
        return Err(Error::new(format!(
            "{arch}: the window was maximized to {wanted} and the program drew at it, but the \
             screen is {green}‰ the green of its new frames (not {GREEN_PERMILLE}‰ or more) and \
             {blue}‰ the blue of its old ones (not {STALE_PERMILLE}‰ or less): {}",
            dump.display()
        )));
    }
    let keys = program_said(said, "key ");
    let buttons = program_said(said, "button ");
    if !keys.contains(&KEY.1) || buttons.is_empty() {
        return Err(Error::new(format!(
            "{arch}: after the maximize the program got the keys {keys:?} and the clicks \
             {buttons:?}, not the key {} and a click",
            KEY.1
        )));
    }
    println!(
        "  {arch}: maximized to {wanted}, d3d11-triangle was told it, drew at it ({green}‰ \
         its new green, {blue}‰ old blue), and got the click at {} and the key, {}",
        buttons.first().copied().unwrap_or_default(),
        dump.display()
    );
    Ok(())
}

/// Judge what the windowed boot said and showed: the program's first frame,
/// its presents, and its colours on the screen taken while it presented.
///
/// # Errors
///
/// When the frame, the presents or the screen are not what they must be.
fn judge_window(arch: Arch, said: &[String], screen: Option<&Image>, dump: &Path) -> Result<()> {
    let first: Vec<String> = said
        .iter()
        .filter_map(|line| line.split_once(PREFIX).map(|(_, rest)| rest.to_owned()))
        .collect();
    crate::dxvk::judge(&first, 1)
        .map_err(|error| Error::new(format!("{arch}: presenting, the first frame: {error}")))?;
    let (count, done) = presents(said);
    if count < PRESENTS || !done {
        return Err(Error::new(format!(
            "{arch}: the program presented {count} frames{}, not {PRESENTS} or more in {SECONDS} s; \
             the `{PREFIX}` lines say how far it came",
            if done {
                ""
            } else {
                " and never said it was done"
            }
        )));
    }
    let (clear, triangle) = screen.map_or((0, 0), |screen| {
        coverage(screen, crate::dxvk::CLEAR_RGB, crate::dxvk::COLOUR)
    });
    if clear < CLEAR_PERMILLE || triangle < TRIANGLE_PERMILLE {
        return Err(Error::new(format!(
            "{arch}: the program presented {count} frames, but the screen is {clear}‰ its blue \
             and {triangle}‰ its triangle's colour, not {CLEAR_PERMILLE}‰ and \
             {TRIANGLE_PERMILLE}‰: {}",
            dump.display()
        )));
    }
    println!(
        "  {arch}: d3d11-triangle presented {count} frames in {SECONDS} s through DXVK and \
         yserver, and its frame is on the screen ({clear}‰ blue, {triangle}‰ triangle), {}",
        dump.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script presents for [`SECONDS`] in the judge's colour.
    #[test]
    fn the_script_presents_for_the_judges_seconds() {
        let hex = format!(
            "{:02x}{:02x}{:02x}",
            crate::dxvk::COLOUR[0],
            crate::dxvk::COLOUR[1],
            crate::dxvk::COLOUR[2]
        );
        assert!(SCRIPT.contains(&format!("d3d11-triangle.exe {hex} {SECONDS} ")));
    }

    /// Presents are counted from the program's own lines only, and the
    /// last line must say it was done.
    #[test]
    fn presents_are_counted_from_the_programs_lines() {
        let lines: Vec<String> = [
            "dxvk: said: d3d11-triangle: presented=40 in 1000 ms",
            "dxvk: said: d3d11-triangle: presented=95 in 2000 ms, done",
            "dxvk: log: presented=900",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert_eq!(presents(&lines), (95, true));
        assert_eq!(presents(&lines[..1]), (40, false));
    }

    /// What the maximize boot said: hyprix's clients before and after, and
    /// the program's lines.
    fn maximize_lines(client: &str, buffers: &str) -> Vec<String> {
        [
            "dxvk: clients: Window 1 -> /bin/zinc:".to_owned(),
            "dxvk: clients: \tsize: 1018,740".to_owned(),
            "dxvk: clients: Window 2 -> d3d11-triangle:".to_owned(),
            "dxvk: clients: \tsize: 504,478".to_owned(),
            "dxvk: maximized: Window 2 -> d3d11-triangle:".to_owned(),
            "dxvk: maximized: \tat: 0,22".to_owned(),
            "dxvk: maximized: \tsize: 1024,746".to_owned(),
            "dxvk: said: d3d11-triangle: client 504x478".to_owned(),
            "dxvk: said: d3d11-triangle: buffers 504x478".to_owned(),
            format!("dxvk: said: d3d11-triangle: client {client}"),
            format!("dxvk: said: d3d11-triangle: buffers {buffers}"),
            "dxvk: said: d3d11-triangle: button 512,373".to_owned(),
            "dxvk: said: d3d11-triangle: key 41".to_owned(),
        ]
        .to_vec()
    }

    /// A screen all of one colour.
    fn screen_of(colour: [u8; 3]) -> Image {
        Image {
            width: 4,
            height: 4,
            pixels: colour.repeat(16),
        }
    }

    /// The program's window is found by its title among hyprix's clients,
    /// and its last size taken.
    #[test]
    fn the_programs_window_size_is_read_by_its_title() {
        let lines = maximize_lines("1024x746", "1024x746");
        assert_eq!(window_size(&lines, "dxvk: clients: "), Some((504, 478)));
        assert_eq!(window_size(&lines, "dxvk: maximized: "), Some((1024, 746)));
        assert_eq!(program_said(&lines, "client "), ["504x478", "1024x746"]);
    }

    /// The maximize passes only when the program was told the new size,
    /// drew at it, the screen shows its new frames and no old one, and the
    /// input reached it.
    #[test]
    fn a_maximize_needs_the_new_size_new_frames_and_input() {
        let dump = Path::new("dump.ppm");
        let good = maximize_lines("1024x746", "1024x746");
        let green = screen_of(GREEN);
        assert!(judge_maximized(Arch::X86_64, &good, Some(&green), dump).is_ok());
        // Wine never told the program (2026-10-28's Endfield, as it looked).
        let untold = maximize_lines("504x478", "504x478");
        assert!(judge_maximized(Arch::X86_64, &untold, Some(&green), dump).is_err());
        // Told, but its buffers stayed.
        let kept = maximize_lines("1024x746", "504x478");
        assert!(judge_maximized(Arch::X86_64, &kept, Some(&green), dump).is_err());
        // A stale frame on the screen: the old blue.
        let stale = screen_of(crate::dxvk::CLEAR_RGB);
        assert!(judge_maximized(Arch::X86_64, &good, Some(&stale), dump).is_err());
        // No key after it.
        let deaf: Vec<String> = good
            .iter()
            .filter(|line| !line.ends_with("key 41"))
            .cloned()
            .collect();
        assert!(judge_maximized(Arch::X86_64, &deaf, Some(&green), dump).is_err());
    }

    /// A screen's colours are counted in thousandths.
    #[test]
    fn colours_are_counted_in_thousandths() {
        let mut pixels = Vec::new();
        for _ in 0..3 {
            pixels.extend([0_u8, 0, 255]);
        }
        pixels.extend([255_u8, 128, 32]);
        let screen = Image {
            width: 2,
            height: 2,
            pixels,
        };
        assert_eq!(coverage(&screen, [0, 0, 255], [255, 128, 32]), (750, 250));
    }
}
