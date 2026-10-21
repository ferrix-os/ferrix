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

use std::path::{Path, PathBuf};
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
const SCRIPT_PATH: &str = "etc/dxvk.sh";

/// What the script prints once the program has presented for a while.
const WINDOW: &str = "dxvk: window";

/// What the script prints last, whatever happened.
const END: &str = "dxvk: end";

/// What the script puts before each line of the program's.
pub(crate) const PREFIX: &str = "dxvk: said: ";

/// How long the program presents, in seconds; the same number is in
/// [`SCRIPT`].
const SECONDS: u32 = 30;

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
    /usr/lib/wine/wine64 /dxvk/d3d11-triangle.exe ff8020 30 > /tmp/present.txt 2>&1 &
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
wait $program
echo "dxvk: the program exited $?"
grep -v '^info:    ' /tmp/present.txt | sed 's/^/dxvk: said: /'
tail -n 20 /tmp/yserver.log | sed 's/^/dxvk: yserver: /'
cat /tmp/yserver-node.txt | sed 's/^/dxvk: /'
grep -E 'DRI3|zero-copy|dmabuf|direct' /tmp/yserver.log | tail -n 30 | sed 's/^/dxvk: zc: /'
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
    let mut script = SCRIPT.to_owned();
    if let Some(tree) = std::env::var_os("FERRIX_DXVK_ANV") {
        // SCRATCH: anv over the fake back end, presenting through the sw WSI.
        let lib = Path::new(&tree).join("usr/lib/x86_64-linux-gnu/libvulkan_intel.so");
        let content = std::fs::read(&lib)
            .map_err(|error| Error::new(format!("reading {}: {error}", lib.display())))?;
        ports.push(File { path: "intel".to_owned(), mode: 0o755, content: Content::Directory });
        ports.push(File { path: "intel/libvulkan_intel.so".to_owned(), mode: 0o755, content: Content::Bytes(content) });
        ports.push(File { path: "intel/intel_icd.json".to_owned(), mode: 0o644, content: Content::Bytes(br#"{"ICD":{"api_version":"1.4.305","library_path":"/intel/libvulkan_intel.so"},"file_format_version":"1.0.0"}"#.to_vec()) });
        let wsi = std::env::var("FERRIX_DXVK_WSI").unwrap_or_else(|_| "sw".to_owned());
        script = script.replace(
            "export VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/lvp_icd.json",
            &format!("export VK_ICD_FILENAMES=/intel/intel_icd.json MESA_VK_WSI_DEBUG={wsi} MESA_SHADER_CACHE_DISABLE=1
ls -l /dev/dri | sed 's/^/dxvk: dri: /'"),
        );
    }
    ports.push(File {
        path: SCRIPT_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(script.into_bytes()),
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
    judge_window(arch, &said, screen.as_ref(), &dump)
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
