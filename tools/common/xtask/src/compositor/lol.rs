//! `test-lol`: League of Legends' game executable, on Direct3D 11, in Wine,
//! through DXVK on Mesa's lavapipe, in a window of hyprix's drawn through
//! yserver, under QEMU with no card and no network (docs/WINE.md §13,
//! milestone L1).
//!
//! The game is the customer's own installation, copied by
//! `tools/common/fetch/fetch-lol.sh` onto a volume beside a Wine tree, with
//! DXVK's two DLLs beside its executable, as `test-unity`'s volume is made.
//! Riot's launcher and client are not on it, and the boot has no network:
//! the executable is started on its own, with the command line the volume
//! carries (`ferrix-args.txt`, from milestone L0), and nothing it does can
//! reach Riot. [`SCRIPT`] makes a prefix, starts it, and waits for its
//! window; then the gate dumps the screen twice, [`APART`] apart.
//!
//! It passes when DXVK's own log line is there (so the game made a
//! Direct3D 11 device through DXVK, not Wine's `wined3d`), hyprix lists the
//! game's window, and the window is drawn: its pixels are not mostly black.
//! What the picture is -- a loading screen, a scene, the game's own error
//! screen for a missing client -- is not judged yet: L0 says which one a
//! standalone start reaches, and the judge is narrowed to it then. How much
//! changed between the dumps is reported, not required.

use std::time::{Duration, Instant};

use super::boot::{build_image, undithered, with_the_transcript};
use super::unity::{compare, screen, window_of};
use super::{Carried, EITHER, Programs, gates_busybox};
use crate::args::Args;
use crate::display::{Image, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The script that makes the volume.
const FETCH: &str = include_str!("../../../fetch/fetch-lol.sh");

/// Where [`SCRIPT`] is in the image.
const SCRIPT_PATH: &str = "etc/lol.sh";

/// The class Wine gives the game's X window, which hyprix matches.
const CLASS: &str = "league of legends.exe";

/// What the script prints before its `hyprctl clients` lines.
const CLIENTS: &str = "lol: clients:";

/// What the script prints once the game's window is there, or once it gave
/// up waiting for it.
const WINDOW: &str = "lol: window";

/// What the script prints last, whatever happened.
const END: &str = "lol: end";

/// What the script prefixes each line of Wine's and DXVK's output with.
const WINE: &str = "lol: wine: ";

/// How long the script has to reach [`WINDOW`]: yserver's start,
/// `wineboot`, and the game's start on lavapipe.
const PATIENCE: Duration = Duration::from_secs(1200);

/// The time between the two screendumps.
const APART: Duration = Duration::from_secs(30);

/// How long the script lets the game run after [`WINDOW`] before it prints
/// the logs: the two dumps and a margin. Written out in [`SCRIPT`] too,
/// where a test holds the two together.
const RUN_ON: u64 = 75;

/// The part of the window's pixels, in percent, that must not be near
/// black in the second dump: something drawn, not an empty window.
const LIT_PERCENT: usize = 30;

/// The script hyprix's `exec-once` runs. Every line of its own starts
/// `lol:`, and it ends with [`END`] whatever happened.
///
/// The game's command line is `ferrix-args.txt`'s one line, split at
/// spaces (L0 decides it; empty starts the executable with none). Wine's
/// and the game's output go to a file printed at the end, with the logs the
/// game wrote under its folder while it ran.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
echo "lol: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "lol: the X socket was there after ${waited}s"
mkdir -p /tmp/wine /tmp/lol
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "lol: wineboot exited $?"
/usr/lib/wine/wineserver64 -w &
server=$!
waited=0
while kill -0 $server 2>/dev/null && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "lol: the prefix's programs had ended after ${waited}s"
sed 's/^/lol: wineboot: /' /tmp/wine/wineboot.log
cd /data/lol
args=$(cat /data/lol/ferrix-args.txt 2>/dev/null)
echo "lol: the game's arguments: $args"
touch /tmp/lol/started
VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/lvp_icd.json \
WINEDLLOVERRIDES="d3d11,dxgi=n" DXVK_LOG_LEVEL=info DXVK_LOG_PATH=none \
    /usr/lib/wine/wine64 "/data/lol/League of Legends.exe" $args > /tmp/lol/wine.log 2>&1 &
game=$!
echo "lol: the game's process is $game"
waited=0
until /bin/hyprctl clients | grep -qi "class: league of legends.exe" \
    || ! kill -0 $game 2>/dev/null || [ $waited -ge 840 ]; do
    sleep 2
    waited=$((waited + 2))
done
echo "lol: the window was looked for for ${waited}s"
kill -0 $game 2>/dev/null && echo "lol: the game is running" || echo "lol: the game ended"
/bin/hyprctl clients | sed 's/^/lol: clients: /'
echo "lol: window"
sleep 75
kill -0 $game 2>/dev/null && echo "lol: the game was still running" || echo "lol: the game had ended"
grep -v '^info:    ' /tmp/lol/wine.log | grep -v '^info:  VK_' | head -n 200 | sed 's/^/lol: wine: /'
find /data/lol -newer /tmp/lol/started -type f \( -name '*.log' -o -name '*.txt' \) 2>/dev/null \
    | while read -r log; do tail -n 40 "$log" | sed "s|^|lol: log: ${log#/data/lol/}: |"; done
tail -n 20 /tmp/yserver.log | sed 's/^/lol: yserver: /'
/usr/lib/wine/wineserver64 -k
echo "lol: end"
"#;

/// Megabytes of guest RAM: the prefix on tmpfs, the game, and lavapipe's
/// render targets.
const MEMORY: u32 = 8192;

/// Virtual processors unless `--smp` says otherwise: lavapipe draws on
/// all of them.
const PROCESSORS: u32 = 8;

/// Seconds the boot may take unless `--timeout` says otherwise.
const TIMEOUT: u64 = 1800;

/// The executable's SHA-256 `fetch-lol.sh` pins.
fn pinned() -> &'static str {
    FETCH
        .lines()
        .find_map(|line| line.strip_prefix("GAME_EXE_SHA256="))
        .unwrap_or_default()
}

/// The patch `fetch-lol.sh` pins, for what the gate prints.
fn version() -> &'static str {
    FETCH
        .lines()
        .find_map(|line| line.strip_prefix("GAME_VERSION="))
        .unwrap_or_default()
}

/// The volume `tools/common/fetch/fetch-lol.sh` makes.
///
/// It carries the Wine volume's tree, so yserver, and DXVK: each must be
/// the one its own fetch script pins, and the game's executable the one
/// this one pins.
///
/// # Errors
///
/// The volume has not been made, or was made with another yserver, DXVK
/// or executable.
fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_LOL_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => paths::volume_directory("lol")?,
    };
    let image = directory.join("lol.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-lol.sh makes it from an installation \
             (LOL_GAME_DIR)",
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
        ("game.sha256", pinned().to_owned()),
    ];
    for (stamp, want) in wanted {
        let held = read(stamp);
        if held != want {
            return Err(Error::new(format!(
                "{} carries {stamp} {held:?}, not {want:?}: tools/common/fetch/fetch-lol.sh \
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
        "# Carried into the initramfs by `cargo xtask test-lol`.\n\
         env = XDG_RUNTIME_DIR,/tmp\n{}\
         windowrule = float, match:class ^({CLASS})$\n\
         exec-once = /bin/busybox sh /{SCRIPT_PATH}\n",
        crate::yserver::desktop_config(),
    )
}

/// The image and kernel of the desktop [`test_lol`] boots: hyprix with
/// [`config`], yserver's and Wine's files and links, and [`SCRIPT`].
///
/// # Errors
///
/// No static busybox, or a build that fails.
fn desktop(arch: Arch, args: &Args) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-lol needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
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
        busybox: Some(std::path::PathBuf::from(busybox)),
        ports,
        ..Carried::none()
    };
    build_image(arch, &programs, &undithered(&config()), carried, args)
}

/// Boot hyprix with yserver and [`SCRIPT`], dump the screen twice once the
/// game's window is there, and judge both.
///
/// # Errors
///
/// When `--net` is given, the volume is missing, the image cannot be built,
/// the boot fails, or the game did not reach DXVK or draw in a window.
pub(crate) fn test_lol(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-lol runs on x86-64")),
    };
    if args.net {
        return Err(Error::new(
            "test-lol runs with no network, so that nothing the game does reaches Riot: drop --net",
        ));
    }
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
    let first_dump = paths::build_dir(arch).join("lol-first.ppm");
    let second_dump = paths::build_dir(arch).join("lol-second.ppm");
    println!(
        "  {arch}: League of Legends {} in Wine through DXVK on lavapipe, on hyprix through \
         yserver, no network, with {} MiB and {} processors (timeout {}s)",
        version(),
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
        let window = watching.read_more(Instant::now() + PATIENCE, reached(WINDOW))?;
        if window && !watching.after().iter().any(|line| line.contains(END)) {
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
                &Error::new(format!("{arch}: the League script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_logs(&said).map_err(|error| Error::new(format!("{arch}: {error}")))?;
    let Some((first, second)) = dumps else {
        return Err(Error::new(format!(
            "{arch}: the script never said the window was looked for, so the screen was not dumped"
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
        "  {arch}: League of Legends drew in its window through DXVK on lavapipe in Wine on \
         Ferrix: {summary}; {} and {}",
        first_dump.display(),
        second_dump.display()
    );
    Ok(())
}

/// Whether Wine's output shows DXVK made the game's Direct3D 11 device.
///
/// # Errors
///
/// No DXVK line, in words.
fn judge_logs(said: &[String]) -> Result<()> {
    let version = format!("DXVK: v{}", crate::dxvk::pinned());
    if !said
        .iter()
        .any(|line| line.contains(WINE) && line.contains(&version))
    {
        return Err(Error::new(format!(
            "Wine's output has no `{version}`: the game ended before it made a Direct3D 11 \
             device, or Wine's own d3d11 ran; the `lol:` lines say how far it came"
        )));
    }
    Ok(())
}

/// Whether the game's window, in the second dump, is drawn; what was
/// measured, in words.
///
/// # Errors
///
/// No window of the game's in hyprix's list, or a window mostly black.
fn judge_screen(said: &[String], first: &Image, second: &Image) -> Result<String> {
    let Some(at) = window_of(said, CLIENTS, CLASS) else {
        return Err(Error::new(format!(
            "hyprix lists no window of class `{CLASS}`; the `{CLIENTS}` lines say what it had"
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
            "{summary}: less than {LIT_PERCENT}% lit, so nothing is drawn in it"
        )));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script runs on as long as the gate waits for, prints the lines
    /// the judges read, and matches the class hyprix is told about.
    #[test]
    fn the_script_matches_the_judge() {
        assert!(SCRIPT.contains(&format!("sleep {RUN_ON}\n")));
        assert!(SCRIPT.contains(&format!("echo \"{WINDOW}\"")));
        assert!(SCRIPT.contains(&format!("echo \"{END}\"")));
        assert!(SCRIPT.contains(&format!("sed 's/^/{CLIENTS} /'")));
        assert!(SCRIPT.contains(&format!("sed 's/^/{WINE}/'")));
        assert!(SCRIPT.contains(&format!("grep -qi \"class: {CLASS}\"")));
        assert!(RUN_ON > APART.as_secs() + 30);
    }

    /// The fetch script pins the executable and its patch, and the gate
    /// reads both.
    #[test]
    fn the_executable_is_pinned() {
        assert_eq!(pinned().len(), 64);
        assert!(pinned().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(version(), "16.20.8248524");
    }

    fn image(width: usize, height: usize, rgb: [u8; 3]) -> Image {
        Image {
            width,
            height,
            pixels: rgb.repeat(width * height),
        }
    }

    /// A drawn window passes, still or not; a black one, or none, does not.
    #[test]
    fn the_screen_judge_wants_a_drawn_window() {
        let lines: Vec<String> = [
            "Window 1 -> foot:",
            "\tclass: foot",
            "\tat: 0,0",
            "\tsize: 2,2",
            "Window 2 -> League of Legends (TM) Client:",
            "\tat: 0,0",
            "\tsize: 4,4",
            "\tclass: league of legends.exe",
        ]
        .iter()
        .map(|entry| format!("  12.00 | {CLIENTS} {entry}"))
        .collect();
        let grey = image(4, 4, [90, 90, 90]);
        let black = image(4, 4, [0, 0, 0]);
        assert!(
            judge_screen(&lines, &grey, &grey).is_ok(),
            "still but drawn"
        );
        assert!(judge_screen(&lines, &grey, &black).is_err(), "black");
        assert!(
            judge_screen(&lines[..4], &grey, &grey).is_err(),
            "no window"
        );
    }

    /// The logs pass only with DXVK's own line, in Wine's output.
    #[test]
    fn the_log_judge_wants_dxvk() {
        let line = format!("{WINE}info:  DXVK: v{}", crate::dxvk::pinned());
        assert!(judge_logs(&[line]).is_ok());
        assert!(judge_logs(&[]).is_err());
        let elsewhere = format!("lol: log: x: DXVK: v{}", crate::dxvk::pinned());
        assert!(judge_logs(&[elsewhere]).is_err(), "not Wine's output");
    }
}
