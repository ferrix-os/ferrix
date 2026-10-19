//! `test-unity`: a real Unity game on Direct3D 11, in Wine, through DXVK on
//! Mesa's lavapipe, in a window of hyprix's drawn through yserver, under QEMU
//! with no card (stage 23, step 3; docs/WINE.md §9).
//!
//! The game is Unity Technologies' Spaceship Demo (Unity 2021.2.3f1, the
//! High Definition Render Pipeline, Mono), on the volume
//! `tools/common/fetch/fetch-unity.sh` makes: the Wine volume's tree with the
//! game at `/unity` and DXVK's two DLLs beside its executable. The desktop
//! is `test-wine`'s: hyprix, with yserver on `:0` as a Wayland client of it.
//! [`SCRIPT`] makes a prefix and starts the game with `--benchmark`, which
//! skips its menus and plays its walkthrough of the ship, in a window
//! hyprix leaves floating at the size the game takes, which is the
//! screen's: the game sets its own resolution (its options' defaults) and
//! does not keep Unity's `-screen-width`. Once Unity's log says the
//! benchmark began recording, the gate dumps the screen twice, [`APART`]
//! apart.
//!
//! It passes when Unity's log says the game started on Direct3D 11 at
//! feature level 11.1 on llvmpipe, DXVK's own log line is there (so not
//! Wine's `wined3d`), the benchmark's scene loaded, and the game's window
//! on the screen is drawn and moving: its pixels are not mostly black, and
//! a large part of them changed between the two dumps, which a window that
//! drew one frame and stopped does not do.

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

/// The script that fetches the game and makes the volume.
const FETCH: &str = include_str!("../../../fetch/fetch-unity.sh");

/// Where [`SCRIPT`] is in the image.
const SCRIPT_PATH: &str = "etc/unity.sh";

/// The class Wine gives the game's X window, which hyprix matches.
const CLASS: &str = "spaceship.exe";

/// The Unity version the release was built with, which its log names.
const ENGINE: &str = "Initialize engine version: 2021.2.3f1";

/// What Unity's log says when the benchmark's walkthrough begins: the
/// scene is loaded and its frames are being drawn and timed.
const RECORDING: &str = "Started Recording benchmark";

/// What the script prints once the benchmark records, or once it gave up.
const SCENE: &str = "unity: scene";

/// What the script prints last, whatever happened.
const END: &str = "unity: end";

/// What the script prefixes each line of Unity's log with.
const PLAYER: &str = "unity: player: ";

/// What the script prefixes each line of Wine's and DXVK's output with.
const WINE: &str = "unity: wine: ";

/// How long the script has to reach [`SCENE`]: yserver's start, `wineboot`
/// and the game's start and first scene on lavapipe.
const PATIENCE: Duration = Duration::from_secs(1200);

/// The time between the two screendumps.
const APART: Duration = Duration::from_secs(30);

/// How long the script lets the game run after [`SCENE`] before it prints
/// the logs: the two dumps and a margin. Written out in [`SCRIPT`] too,
/// where a test holds the two together.
const RUN_ON: u64 = 75;

/// The part of the window's pixels, in percent, that must differ between
/// the two dumps: the walkthrough moves the camera through the ship.
const CHANGED_PERCENT: usize = 20;

/// The part of the window's pixels, in percent, that must not be near
/// black in the second dump: a drawn scene, not an empty window.
const LIT_PERCENT: usize = 30;

/// The script hyprix's `exec-once` runs. Every line of its own starts
/// `unity:`, and it ends with [`END`] whatever happened.
///
/// Wine's and the game's output go to files that are printed at the end;
/// the game's own log is Unity's `-logFile`. `--benchmark` is the game's
/// own option (its `CommandLineArgumentLogic`): it starts the walkthrough
/// with no menu to click through, and records the frames' times.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
echo "unity: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "unity: the X socket was there after ${waited}s"
mkdir -p /tmp/wine /tmp/unity
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "unity: wineboot exited $?"
/usr/lib/wine/wineserver64 -w &
server=$!
waited=0
while kill -0 $server 2>/dev/null && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "unity: the prefix's programs had ended after ${waited}s"
ps | sed 's/^/unity: ps: /'
sed 's/^/unity: wineboot: /' /tmp/wine/wineboot.log
cd /data/unity
VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/lvp_icd.json \
WINEDLLOVERRIDES="d3d11,dxgi=n" DXVK_LOG_LEVEL=info DXVK_LOG_PATH=none \
    /usr/lib/wine/wine64 /data/unity/Spaceship.exe -screen-fullscreen 0 -force-d3d11 --benchmark \
    -logFile /tmp/unity/player.log > /tmp/unity/wine.log 2>&1 &
game=$!
echo "unity: the game's process is $game"
waited=0
until grep -q "Started Recording benchmark" /tmp/unity/player.log 2>/dev/null \
    || ! kill -0 $game 2>/dev/null || [ $waited -ge 840 ]; do
    sleep 2
    waited=$((waited + 2))
done
echo "unity: the benchmark recorded after ${waited}s of the game's"
kill -0 $game 2>/dev/null && echo "unity: the game is running" || echo "unity: the game ended"
/bin/hyprctl clients | sed 's/^/unity: clients: /'
echo "unity: scene"
sleep 75
kill -0 $game 2>/dev/null && echo "unity: the game was still running" || echo "unity: the game had ended"
sed 's/^/unity: player: /' /tmp/unity/player.log
grep -v '^info:    ' /tmp/unity/wine.log | grep -v '^info:  VK_' | head -n 200 | sed 's/^/unity: wine: /'
tail -n 20 /tmp/yserver.log | sed 's/^/unity: yserver: /'
/usr/lib/wine/wineserver64 -k
echo "unity: end"
"#;

/// Megabytes of guest RAM: the prefix on tmpfs, the game's assets, and
/// lavapipe's render targets.
const MEMORY: u32 = 8192;

/// Virtual processors unless `--smp` says otherwise: lavapipe draws on
/// all of them.
const PROCESSORS: u32 = 8;

/// Seconds the boot may take unless `--timeout` says otherwise.
const TIMEOUT: u64 = 1800;

/// The game's release `fetch-unity.sh` pins.
fn pinned() -> &'static str {
    FETCH
        .lines()
        .find_map(|line| line.strip_prefix("GAME_RELEASE="))
        .unwrap_or_default()
}

/// The volume `tools/common/fetch/fetch-unity.sh` makes.
///
/// It carries the Wine volume's tree, so yserver, and DXVK: each must be
/// the one its own fetch script pins, and the game the one this one pins.
///
/// # Errors
///
/// The volume has not been made, or was made with another yserver, DXVK
/// or game.
fn volume() -> Result<PathBuf> {
    let directory = match std::env::var_os("FERRIX_UNITY_VOLUME") {
        Some(directory) => PathBuf::from(directory),
        None => paths::volume_directory("unity")?,
    };
    let image = directory.join("unity.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-unity.sh makes it",
            image.display()
        )));
    }
    let read = |name: &str| {
        std::fs::read_to_string(directory.join(name))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let wanted = [
        ("yserver.commit", crate::yserver::pinned().to_owned()),
        ("dxvk.version", crate::dxvk::pinned().to_owned()),
        ("game.release", pinned().to_owned()),
    ];
    for (stamp, want) in wanted {
        let held = read(stamp);
        if held != want {
            return Err(Error::new(format!(
                "{} carries {stamp} {held:?}, not {want:?}: tools/common/fetch/fetch-unity.sh \
                 makes it again (after fetch-wine.sh and fetch-dxvk.sh, if their pins moved)",
                image.display()
            )));
        }
    }
    Ok(image)
}

/// The desktop's configuration: yserver on `:0`, the game's window left
/// floating at its own size, and [`SCRIPT`].
fn config() -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-unity`.\n\
         env = XDG_RUNTIME_DIR,/tmp\n{}\
         windowrule = float, match:class ^({CLASS})$\n\
         exec-once = /bin/busybox sh /{SCRIPT_PATH}\n",
        crate::yserver::desktop_config(),
    )
}

/// A rectangle on the screen: left, top, width, height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rect {
    pub(super) x: usize,
    pub(super) y: usize,
    pub(super) width: usize,
    pub(super) height: usize,
}

/// Where hyprix put the game's window, from the `at:` and `size:` of the
/// entry in its list whose class is [`CLASS`].
fn window(lines: &[String]) -> Option<Rect> {
    window_of(lines, "unity: clients:", CLASS)
}

/// Where hyprix put the window of class `class`, from the `at:` and `size:`
/// of its entry in the `hyprctl clients` lines a script printed after
/// `prefix` (`test-lol` shares it).
pub(super) fn window_of(lines: &[String], prefix: &str, class: &str) -> Option<Rect> {
    let entries: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.split_once(prefix))
        .map(|(_, rest)| rest.trim())
        .collect();
    let blocks = entries.split(|entry| entry.contains(" -> "));
    let pair = |value: &str| -> Option<(usize, usize)> {
        let (a, b) = value.trim().split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    };
    for block in blocks {
        let listed = block
            .iter()
            .find_map(|entry| entry.strip_prefix("class:"))
            .map(str::trim);
        if !listed.is_some_and(|listed| listed.eq_ignore_ascii_case(class)) {
            continue;
        }
        let at = block
            .iter()
            .find_map(|entry| entry.strip_prefix("at:"))
            .and_then(pair);
        let size = block
            .iter()
            .find_map(|entry| entry.strip_prefix("size:"))
            .and_then(pair);
        if let (Some((x, y)), Some((width, height))) = (at, size) {
            return Some(Rect {
                x,
                y,
                width,
                height,
            });
        }
    }
    None
}

/// The pixel at (`x`, `y`) of `image`.
fn pixel(image: &Image, x: usize, y: usize) -> Option<[u8; 3]> {
    let at = (y.checked_mul(image.width)?.checked_add(x)?).checked_mul(3)?;
    image.pixels.get(at..at + 3)?.try_into().ok()
}

/// Of the pixels inside `within`: how many there are, how many differ
/// between `first` and `second` by more than a little, and how many are
/// not near black in `second`.
pub(super) fn compare(first: &Image, second: &Image, within: Rect) -> (usize, usize, usize) {
    let (mut all, mut changed, mut lit) = (0, 0, 0);
    for y in within.y..within.y + within.height {
        for x in within.x..within.x + within.width {
            let (Some(a), Some(b)) = (pixel(first, x, y), pixel(second, x, y)) else {
                continue;
            };
            all += 1;
            let difference: u32 = a.iter().zip(b).map(|(a, b)| u32::from(a.abs_diff(b))).sum();
            if difference > 24 {
                changed += 1;
            }
            if b.iter().map(|c| u32::from(*c)).sum::<u32>() > 3 * 24 {
                lit += 1;
            }
        }
    }
    (all, changed, lit)
}

/// A screendump, read.
pub(super) fn screen(qmp: &mut Qmp, dump: &Path) -> Result<Image> {
    qmp.screendump(Some(DEVICE_ID), dump)?;
    let bytes = std::fs::read(dump)
        .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
    parse_ppm(&bytes)
}

/// The image and kernel of the desktop [`test_unity`] boots: hyprix with
/// [`config`], yserver's and Wine's files and links, and [`SCRIPT`].
///
/// # Errors
///
/// No static busybox, or a build that fails.
fn desktop(arch: Arch, args: &Args) -> Result<(PathBuf, PathBuf)> {
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-unity needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
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
    build_image(arch, &programs, &undithered(&config()), carried, args)
}

/// Boot hyprix with yserver and [`SCRIPT`], dump the screen twice once the
/// game's benchmark records, and judge both.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails,
/// or the game did not start on DXVK and lavapipe, load its scene, or draw
/// a moving picture in its window.
pub(crate) fn test_unity(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-unity runs on x86-64")),
    };
    let mut args = args.clone();
    args.gl = false;
    args.data_image = Some(volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.smp_given {
        args.smp = PROCESSORS;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    let (image, kernel) = desktop(arch, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let first_dump = paths::build_dir(arch).join("unity-first.ppm");
    let second_dump = paths::build_dir(arch).join("unity-second.ppm");
    println!(
        "  {arch}: Spaceship Demo {} in Wine through DXVK on lavapipe, on hyprix through yserver, \
         with {} MiB and {} processors (timeout {}s)",
        pinned(),
        qemu_args.memory,
        qemu_args.smp,
        qemu_args.timeout
    );
    let mut said: Vec<String> = Vec::new();
    let mut dumps: Option<(Image, Image)> = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let reached = |marker: &'static str| {
            move |lines: &[String]| {
                lines
                    .iter()
                    .any(|line| line.contains(marker) || line.contains(END))
            }
        };
        let scene = watching.read_more(Instant::now() + PATIENCE, reached(SCENE))?;
        if scene && !watching.after().iter().any(|line| line.contains(END)) {
            let first = screen(&mut qmp, &first_dump)?;
            std::thread::sleep(APART);
            let second = screen(&mut qmp, &second_dump)?;
            dumps = Some((first, second));
        }
        let ended = watching.read_more(
            Instant::now() + Duration::from_secs(RUN_ON + 120),
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
                &Error::new(format!("{arch}: the Unity script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_logs(&said).map_err(|error| Error::new(format!("{arch}: {error}")))?;
    let Some((first, second)) = dumps else {
        return Err(Error::new(format!(
            "{arch}: the script never said the scene was there, so the screen was not dumped"
        )));
    };
    let summary = judge_screen(&said, &first, &second).map_err(|error| {
        Error::new(format!(
            "{arch}: {error}: {} and {}",
            first_dump.display(),
            second_dump.display()
        ))
    })?;
    println!(
        "  {arch}: Spaceship Demo drew its walkthrough through DXVK on lavapipe in Wine on \
         Ferrix: {summary}; {} and {}",
        first_dump.display(),
        second_dump.display()
    );
    Ok(())
}

/// Whether the script's lines show Unity started on DXVK's Direct3D 11 on
/// lavapipe and its benchmark's scene loaded.
///
/// # Errors
///
/// What is missing, in words.
fn judge_logs(said: &[String]) -> Result<()> {
    let player: Vec<&str> = said
        .iter()
        .filter_map(|line| line.split_once(PLAYER).map(|(_, rest)| rest))
        .collect();
    if !player.iter().any(|line| line.contains(ENGINE)) {
        return Err(Error::new(format!(
            "Unity's log has no `{ENGINE}`: the game did not start; the `unity:` lines say how \
             far it came"
        )));
    }
    let version = format!("DXVK: v{}", crate::dxvk::pinned());
    if !said
        .iter()
        .any(|line| line.contains(WINE) && line.contains(&version))
    {
        return Err(Error::new(format!(
            "Wine's output has no `{version}`: Wine's own d3d11 ran, or DXVK did not start"
        )));
    }
    if !player
        .iter()
        .any(|line| line.contains("Direct3D 11.0 [level 11.1]"))
        || !player
            .iter()
            .any(|line| line.contains("Renderer:") && line.contains("llvmpipe"))
    {
        return Err(Error::new(
            "Unity's log does not say Direct3D 11 at feature level 11.1 on llvmpipe",
        ));
    }
    if !player.iter().any(|line| line.contains(RECORDING)) {
        return Err(Error::new(format!(
            "Unity's log never says `{RECORDING}`: the benchmark's scene did not load"
        )));
    }
    Ok(())
}

/// Whether the game's window, in the two dumps, is drawn and moving; what
/// was measured, in words.
///
/// # Errors
///
/// No window of the game's in hyprix's list, or a window mostly black or
/// standing still.
fn judge_screen(said: &[String], first: &Image, second: &Image) -> Result<String> {
    let Some(at) = window(said) else {
        return Err(Error::new(format!(
            "hyprix lists no window of class `{CLASS}`; the `unity: clients:` lines say what it \
             had"
        )));
    };
    let (all, changed, lit) = compare(first, second, at);
    let changed_percent = changed * 100 / all.max(1);
    let lit_percent = lit * 100 / all.max(1);
    let summary = format!(
        "its {}x{} window at ({}, {}) is {lit_percent}% lit and {changed_percent}% changed in {}s",
        at.width,
        at.height,
        at.x,
        at.y,
        APART.as_secs()
    );
    if all == 0 {
        return Err(Error::new(format!(
            "{summary}, and no pixel of it is on the screen"
        )));
    }
    if lit_percent < LIT_PERCENT {
        return Err(Error::new(format!(
            "{summary}: less than {LIT_PERCENT}% lit, so no scene is drawn in it"
        )));
    }
    if changed_percent < CHANGED_PERCENT {
        return Err(Error::new(format!(
            "{summary}: less than {CHANGED_PERCENT}% changed, so the game is not drawing frames"
        )));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script asks for the window's size, runs on as long as the gate
    /// waits for, and looks for the line the judge requires.
    #[test]
    fn the_script_matches_the_judge() {
        assert!(SCRIPT.contains(&format!("sleep {RUN_ON}\n")));
        assert!(SCRIPT.contains(&format!("grep -q \"{RECORDING}\"")));
        assert!(SCRIPT.contains(&format!("echo \"{SCENE}\"")));
        assert!(SCRIPT.contains(&format!("echo \"{END}\"")));
        assert!(RUN_ON > APART.as_secs() + 30);
    }

    /// The fetch script pins a release, and the volume is checked for it.
    #[test]
    fn a_release_is_pinned() {
        assert_eq!(pinned(), "2021.2.0");
    }

    /// The game's window is found by its class, not by another window's.
    #[test]
    fn the_window_is_read_from_the_games_entry() {
        let lines: Vec<String> = [
            "Window 1 -> foot:",
            "\tclass: foot",
            "\tat: 500,21",
            "\tsize: 400,300",
            "Window 2 -> Spaceship:",
            "\tmapped: true",
            "\tat: 272,249",
            "\tsize: 480,270",
            "\tclass: spaceship.exe",
            "Window 3 -> Other:",
            "\tat: 1,1",
        ]
        .iter()
        .map(|entry| format!("  12.00 | unity: clients: {entry}"))
        .collect();
        assert_eq!(
            window(&lines),
            Some(Rect {
                x: 272,
                y: 249,
                width: 480,
                height: 270
            })
        );
        assert_eq!(window(&lines[..4]), None);
    }

    fn image(width: usize, height: usize, rgb: [u8; 3]) -> Image {
        Image {
            width,
            height,
            pixels: rgb.repeat(width * height),
        }
    }

    /// A lit, moving window passes; a black one or a still one does not.
    #[test]
    fn the_screen_judge_wants_a_lit_moving_window() {
        let lines: Vec<String> = [
            "Window 2 -> Spaceship:",
            "\tat: 0,0",
            "\tsize: 4,4",
            "\tclass: spaceship.exe",
        ]
        .iter()
        .map(|entry| format!("unity: clients: {entry}"))
        .collect();
        let grey = image(4, 4, [90, 90, 90]);
        let blue = image(4, 4, [40, 80, 160]);
        let black = image(4, 4, [0, 0, 0]);
        assert!(judge_screen(&lines, &grey, &blue).is_ok());
        assert!(judge_screen(&lines, &grey, &grey).is_err(), "still");
        assert!(judge_screen(&lines, &grey, &black).is_err(), "black");
        assert!(judge_screen(&[], &grey, &blue).is_err(), "no window");
    }

    /// The logs pass only with the engine, DXVK, lavapipe and the scene.
    #[test]
    fn the_log_judge_wants_dxvk_on_lavapipe_and_the_scene() {
        let good: Vec<String> = [
            "unity: player: Initialize engine version: 2021.2.3f1 (32358a8527b4)",
            "unity: player:     Version:  Direct3D 11.0 [level 11.1]",
            "unity: player:     Renderer: llvmpipe (LLVM 19.1.7, 256 bits) (ID=0x0)",
            "unity: wine: info:  DXVK: v3.1.1",
            "unity: player: Started Recording benchmark at path: Benchmark-1.html",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert!(judge_logs(&good).is_ok());
        for missing in 0..good.len() {
            let mut lines = good.clone();
            let _ = lines.remove(missing);
            assert!(judge_logs(&lines).is_err(), "line {missing} removed");
        }
    }
}
