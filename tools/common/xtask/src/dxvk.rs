//! `test-dxvk`: a Direct3D 11 program in Wine, through DXVK, on Mesa's
//! lavapipe, under QEMU with no card (stage 23, E2.4; docs/WINE.md §6).
//!
//! The Wine volume (`tools/common/fetch/fetch-wine.sh`) is attached at
//! `/data` as `test-wine` attaches it; it has Wine, the Vulkan loader and
//! lavapipe. `tools/common/fetch/fetch-dxvk.sh` fetches DXVK's release and
//! builds `tools/common/test/d3d11-triangle.c`; those three files go into
//! the image at [`DIRECTORY`]. [`RUN`] starts yserver headless on `:1`
//! (DXVK's Vulkan instance asks for Wine's surface extension, which Wine
//! offers only with a display driver), makes a prefix, and runs the program
//! with DXVK's `d3d11.dll` and `dxgi.dll` beside it, chosen over Wine's own
//! by `WINEDLLOVERRIDES`. The program renders frames into a texture and
//! prints a line describing the last one; [`judge`] compares that line
//! with the frame [`expected`] draws on the host.
//!
//! That first boot presents nothing: it holds DXVK, lavapipe and the
//! threads and memory they ask of Ferrix. A second boot,
//! `crate::compositor::dxvk`, has the program present through a swap chain
//! in a window of hyprix's, by way of Wine's Vulkan surface on yserver's
//! X11, as a game's frames go, and looks for them on the screen.
//!
//! The gate needs no network. It attaches a volume, so it runs on demand as
//! `test-wine` does and is not in the image row.

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-dxvk",
    help_at: 780,
    help: &[
        "test-dxvk     The same volume, with DXVK and a Direct3D 11 program from tools/common/fetch/fetch-dxvk.sh: require the",
        "              program's frame, drawn through DXVK on lavapipe with no card, to be the one xtask draws, then its",
        "              frames presented through a swap chain in a window on hyprix through yserver (docs/WINE.md §6)",
    ],
    run: |_, args| test_dxvk(args),
}];

/// Where DXVK's DLLs and the program are in the image.
const DIRECTORY: &str = "dxvk";

/// The files `fetch-dxvk.sh` writes that go into the image.
const FILES: &[&str] = &["d3d11.dll", "dxgi.dll", "d3d11-triangle.exe"];

/// The script that fetches DXVK, for the release it pins.
const FETCH: &str = include_str!("../../fetch/fetch-dxvk.sh");

/// The program's source, which `fetch-dxvk.sh` copies beside the program
/// it built from it.
const SOURCE: &str = include_str!("../../test/d3d11-triangle.c");

/// The colour [`RUN`] asks the triangle to be drawn in, as `RRGGBB`. It is
/// written out in [`RUN`] too, where a test holds the two together.
pub(crate) const COLOUR: [u8; 3] = [0xff, 0x80, 0x20];

/// The colour the program clears its target to: blue, opaque.
const CLEAR: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// [`CLEAR`] as a screen shows it.
pub(crate) const CLEAR_RGB: [u8; 3] = [CLEAR[0], CLEAR[1], CLEAR[2]];

/// The side of the program's square target, in pixels.
const SIZE: usize = 64;

/// How many frames the program renders before it reads the last one back.
const FRAMES: u32 = 30;

/// What every line of the program's own starts with.
const SAID: &str = "d3d11-triangle:";

/// Where [`RUN`] is in the image.
const RUN_PATH: &str = "bin/dxvk-test";

/// yserver on `:1`, as `test-yserver` starts it, a prefix made with no
/// `DISPLAY`, then the program with `DISPLAY` set and DXVK's DLLs chosen.
/// Run by busybox's `sh`; its status is the program's.
///
/// `UseXRandR` is turned off in the prefix before a program gets a display,
/// `test-wine`'s workaround (docs/WINE.md §5): on yserver's `RandR`, Wine's
/// X driver finds no monitor and wineserver ends on a divide by zero.
const RUN: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp RUST_LOG=info
export YSERVER_ALLOW_NO_INPUT=1 YSERVER_ALLOW_SOFTWARE_VULKAN=1
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
export VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/lvp_icd.json
/data/yserver/yserver :1 -nolisten tcp > /tmp/yserver.log 2>&1 &
server=$!
mkdir -p /tmp/wine
echo "dxvk-gate: wineboot"
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "dxvk-gate: wineboot exited $?"
/usr/lib/wine/wineserver64 -w
/usr/lib/wine/wine64 reg add 'HKCU\Software\Wine\X11 Driver' /v UseXRandR /d N /f > /tmp/wine/reg.log 2>&1
echo "dxvk-gate: reg exited $?"
/usr/lib/wine/wineserver64 -w
waited=0
while [ ! -S /tmp/.X11-unix/X1 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "dxvk-gate: the X socket was there after ${waited}s"
cd /dxvk
echo "dxvk-gate: d3d11-triangle"
DISPLAY=:1 WINEDLLOVERRIDES="d3d11,dxgi=n" DXVK_LOG_LEVEL=info DXVK_LOG_PATH=none \
    /usr/lib/wine/wine64 /dxvk/d3d11-triangle.exe ff8020 > /tmp/triangle.txt 2>&1
status=$?
echo "dxvk-gate: d3d11-triangle exited $status"
cat /tmp/triangle.txt
/usr/lib/wine/wineserver64 -k
kill $server
sleep 1
tail -n 20 /tmp/yserver.log | sed 's/^/dxvk-gate: yserver: /'
exit $status
"#;

/// The script: [`RUN`], whose status says whether the program read its
/// frame back.
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/usr/lib/wine/wine64 ] || exit 3
[ -e /data/usr/share/vulkan/icd.d/lvp_icd.json ] || exit 5
busybox sh /bin/dxvk-test || exit 4
exit 17
"#;

/// What the script exits with when the program read its frame back.
const STATUS: i32 = 17;

/// Megabytes of guest RAM: a prefix on tmpfs, as `test-wine`'s, and
/// lavapipe's.
const MEMORY: u32 = 6144;

/// Seconds the boot may take unless `--timeout` says otherwise: yserver's
/// start, `wineboot`, and the program's shaders compiled by LLVM.
const TIMEOUT: u64 = 900;

/// The DXVK release `fetch-dxvk.sh` pins.
pub(crate) fn pinned() -> &'static str {
    FETCH
        .lines()
        .find_map(|line| line.strip_prefix("DXVK_VERSION="))
        .unwrap_or_default()
}

/// The directory `fetch-dxvk.sh` writes, holding the release it pins and
/// the program built from this tree's source.
///
/// # Errors
///
/// A file is missing, or the directory holds another release or a program
/// built from another source.
fn directory() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_DXVK_DIR") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("dxvk")?,
    };
    for file in FILES {
        if !directory.join(file).is_file() {
            return Err(Error::new(format!(
                "{} is not there: tools/common/fetch/fetch-dxvk.sh makes it",
                directory.join(file).display()
            )));
        }
    }
    let stamp = directory.join("dxvk.version");
    let held = std::fs::read_to_string(&stamp).unwrap_or_default();
    if held.trim() != pinned() {
        return Err(Error::new(format!(
            "{} holds DXVK {:?}, not {}: tools/common/fetch/fetch-dxvk.sh fetches it again",
            directory.display(),
            held.trim(),
            pinned()
        )));
    }
    let built = std::fs::read_to_string(directory.join("d3d11-triangle.c")).unwrap_or_default();
    if built != SOURCE {
        return Err(Error::new(format!(
            "{}'s d3d11-triangle.exe was built from another tools/common/test/d3d11-triangle.c: \
             tools/common/fetch/fetch-dxvk.sh builds it again",
            directory.display()
        )));
    }
    Ok(directory)
}

/// DXVK's DLLs and the program from `directory`, as the image carries them
/// at [`DIRECTORY`].
///
/// # Errors
///
/// A file cannot be read.
pub(crate) fn files(directory: &std::path::Path) -> Result<Vec<crate::ports::File>> {
    let mut files = vec![crate::ports::File {
        path: DIRECTORY.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Directory,
    }];
    for name in FILES {
        let path = directory.join(name);
        let content = std::fs::read(&path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
        files.push(crate::ports::File {
            path: format!("{DIRECTORY}/{name}"),
            mode: 0o755,
            content: crate::ports::Content::Bytes(content),
        });
    }
    Ok(files)
}

/// FNV-1a over `bytes`, as the program hashes its frame.
fn fnv(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

/// The frame the program must read back, as RGBA rows: [`CLEAR`], and a
/// triangle in [`COLOUR`] with its corners at (16, 48), (32, 16) and
/// (48, 48), where its vertices put it. No pixel centre lies on an edge
/// (the slanted edges cross the centres' rows at a quarter of a pixel), so
/// whether a pixel is covered needs no tie-breaking rule.
fn expected() -> Vec<[u8; 4]> {
    let triangle = [COLOUR[0], COLOUR[1], COLOUR[2], 0xff];
    let mut frame = Vec::with_capacity(SIZE * SIZE);
    for y in 0..SIZE {
        for x in 0..SIZE {
            // Pixel centres, doubled to stay whole: the left edge is
            // x = 16 + (48 - y) / 2 and the right x = 48 - (48 - y) / 2.
            let (cx, cy) = (4 * x + 2, 4 * y + 2);
            let height = 4 * 48 - cy as i64;
            let covered = cy < 4 * 48
                && cy > 4 * 16
                && 2 * (cx as i64) > 2 * 4 * 16 + height
                && 2 * (cx as i64) < 2 * 4 * 48 - height;
            frame.push(if covered { triangle } else { CLEAR });
        }
    }
    frame
}

/// What the program said of its last frame, from its line.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    frames: u32,
    clear: u32,
    triangle: u32,
    other: u32,
    centre: u32,
    corner: u32,
    fnv: u32,
}

/// The program's frame line among `lines`, read.
fn frame(lines: &[String]) -> Option<Frame> {
    let line = lines
        .iter()
        .filter_map(|line| line.split_once(SAID).map(|(_, rest)| rest))
        .find(|rest| rest.trim_start().starts_with("frames="))?;
    let field = |name: &str, radix: u32| {
        line.split_whitespace()
            .find_map(|item| item.strip_prefix(name)?.strip_prefix('='))
            .and_then(|value| u32::from_str_radix(value, radix).ok())
    };
    Some(Frame {
        frames: field("frames", 10)?,
        clear: field("clear", 10)?,
        triangle: field("triangle", 10)?,
        other: field("other", 10)?,
        centre: field("centre", 16)?,
        corner: field("corner", 16)?,
        fnv: field("fnv", 16)?,
    })
}

/// The frame the program must have read back after `frames` frames, in its
/// line's terms.
fn wanted(frames: u32) -> Frame {
    let pixels = expected();
    let rgba = |x: usize, y: usize| {
        pixels
            .get(y * SIZE + x)
            .map_or(0, |p| u32::from_be_bytes(*p))
    };
    let triangle = pixels.iter().filter(|pixel| **pixel != CLEAR).count() as u32;
    Frame {
        frames,
        clear: (SIZE * SIZE) as u32 - triangle,
        triangle,
        other: 0,
        centre: rgba(SIZE / 2, SIZE / 2),
        corner: rgba(1, 1),
        fnv: fnv(pixels.as_flattened()),
    }
}

/// Whether the program's `lines` show DXVK, on lavapipe, drawing the frame
/// [`expected`] draws, as its line for `frames` frames.
///
/// # Errors
///
/// What is missing or different, in words.
pub(crate) fn judge(lines: &[String], frames: u32) -> Result<()> {
    let version = format!("DXVK: v{}", pinned());
    if !lines.iter().any(|line| line.contains(&version)) {
        return Err(Error::new(format!(
            "DXVK's log has no `{version}`: Wine's own d3d11 ran, or DXVK did not start"
        )));
    }
    let device = lines
        .iter()
        .find(|line| line.contains(SAID) && line.contains("device on"));
    match device {
        Some(line) if line.contains("llvmpipe") && line.contains("feature level 11.1") => {}
        Some(line) => {
            return Err(Error::new(format!(
                "the device is not lavapipe's at feature level 11.1: {}",
                line.trim()
            )));
        }
        None => return Err(Error::new("the program made no Direct3D 11 device")),
    }
    let Some(got) = frame(lines) else {
        return Err(Error::new("the program printed no line for its frame"));
    };
    let want = wanted(frames);
    if got == want {
        Ok(())
    } else {
        Err(Error::new(format!(
            "the frame is not the one drawn on the host: got {got:?}, want {want:?}"
        )))
    }
}

/// Boot a shell whose script starts yserver, makes a Wine prefix and runs
/// the Direct3D 11 program through DXVK, and judge its frame.
///
/// # Errors
///
/// When the volume or DXVK's files are missing, an image cannot be built,
/// the boot fails, or the frame is not the expected one.
pub(crate) fn test_dxvk(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-dxvk runs on x86-64")),
    };
    let dxvk = directory()?;
    let mut args = args.clone();
    args.data_image = Some(crate::wine::volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }

    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!(
        "  {arch}: building an image with DXVK {} and d3d11-triangle",
        pinned()
    );
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let links: Vec<(&str, &str)> = crate::yserver::LINKS
        .iter()
        .chain(crate::wine::LINKS)
        .copied()
        .collect();
    let mut files = rustc::files(&links);
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(RUN.as_bytes().to_vec()),
    });
    files.extend(self::files(&dxvk)?);
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: running Direct3D 11 through DXVK on lavapipe on Ferrix with {} MiB \
         (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let exited = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    match exited {
        Some(status) if status == STATUS.to_string() => {}
        Some("3") => {
            return Err(Error::new(format!(
                "{arch}: /data/usr/lib/wine/wine64 is not there: is the Wine volume attached?"
            )));
        }
        Some("5") => {
            return Err(Error::new(format!(
                "{arch}: the Wine volume has no lavapipe: tools/common/fetch/fetch-wine.sh \
                 makes it again from yserver's tree"
            )));
        }
        Some("4") => {
            return Err(Error::new(format!(
                "{arch}: d3d11-triangle did not read its frame back; the `dxvk-gate:` and \
                 `{SAID}` lines and DXVK's log say how far it came"
            )));
        }
        other => {
            return Err(Error::new(format!(
                "{arch}: the DXVK script ended with {other:?}"
            )));
        }
    }
    judge(&lines, FRAMES).map_err(|error| Error::new(format!("{arch}: offscreen: {error}")))?;
    println!(
        "  {arch}: DXVK {} drew d3d11-triangle's frame on lavapipe in Wine on Ferrix, as xtask \
         draws it",
        pinned()
    );
    crate::compositor::dxvk::test_dxvk_window(&args, crate::wine::volume()?, &dxvk)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The colour the script passes is the one the judge expects.
    #[test]
    fn the_script_asks_for_the_judges_colour() {
        let hex = format!("{:02x}{:02x}{:02x}", COLOUR[0], COLOUR[1], COLOUR[2]);
        assert!(RUN.contains(&format!("d3d11-triangle.exe {hex} ")));
    }

    /// The fetch script pins a release, and the judge reads it.
    #[test]
    fn a_release_is_pinned() {
        assert_eq!(pinned(), "3.1.1");
    }

    /// The expected frame has the 512 pixels the triangle covers, half its
    /// 32 by 32 box, and the hash the program printed for that frame on
    /// Linux with the same Wine, DXVK and lavapipe (2026-10-12).
    #[test]
    fn the_expected_frame_is_the_hosts() {
        let want = wanted(FRAMES);
        assert_eq!(want.triangle, 512);
        assert_eq!(want.clear, 3584);
        assert_eq!(want.centre, 0xff80_20ff);
        assert_eq!(want.corner, 0x0000_ffff);
        assert_eq!(want.fnv, 0x06a7_a745);
    }

    fn lines(frame: &str) -> Vec<String> {
        [
            "info:  DXVK: v3.1.1",
            "d3d11-triangle: device on \"llvmpipe (LLVM 19.1.7, 256 bits)\" (vendor 10005), \
             feature level 11.1",
            frame,
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
    }

    /// The host's frame passes; a frame of another colour, a missing pixel
    /// or Wine's own Direct3D does not.
    #[test]
    fn the_judge_takes_only_the_hosts_frame() {
        let good = "d3d11-triangle: frames=30 clear=3584 triangle=512 other=0 centre=ff8020ff \
                    corner=0000ffff fnv=06a7a745";
        assert!(judge(&lines(good), FRAMES).is_ok());
        let colour = "d3d11-triangle: frames=30 clear=3584 triangle=512 other=0 \
                      centre=20ff80ff corner=0000ffff fnv=12345678";
        assert!(judge(&lines(colour), FRAMES).is_err());
        let short = "d3d11-triangle: frames=30 clear=3585 triangle=511 other=0 centre=ff8020ff \
                     corner=0000ffff fnv=06a7a745";
        assert!(judge(&lines(short), FRAMES).is_err());
        assert!(judge(&lines(good), 1).is_err());
        let mut wined3d = lines(good);
        let _ = wined3d.remove(0);
        assert!(judge(&wined3d, FRAMES).is_err());
    }
}
