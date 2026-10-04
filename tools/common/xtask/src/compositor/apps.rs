//! `test-foot`, `test-vkgears` and `test-xwindow`: programs nobody here
//! wrote, on the compositor. foot and vkgears are built against ferrousli;
//! `xdpyinfo` and `xev` are Debian's, through yserver.
//!
//! No picture is blessed for any of them, since each draws in its own way:
//! what is required is what each program said, and counts taken of the
//! screen.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{build_image, said_on_its_own, say_the_marker, undithered, with_the_transcript};
use super::browser::CHROME_WINDOW_PATIENCE;
use super::{BACKGROUND, Carried, EITHER, FAILED, MARKER, Programs, SETTLE, gates_busybox};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The configuration `test-foot` gives the compositor: foot, running
/// `hyprctl version` and holding its window open after it exits.
///
/// `hyprctl version` for `TERMINAL_CONFIG`'s reason: three lines, and a
/// round trip through the control socket on the way. What foot draws of them
/// came through a pseudoterminal ferrousli opened, into a grid laid out in a
/// font fontconfig found and freetype rasterised, into a `wl_shm` buffer
/// libwayland-client handed the compositor. `--log-level=info` makes foot say
/// which font it loaded, which is how the serial port shows it was the
/// image's, and `--log-colorize=never` leaves its lines plain enough to read
/// an error off.
const FOOT_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-foot`.
exec-once = /bin/foot --log-level=info --log-colorize=never --log-no-syslog --hold /usr/libexec/ferrix/foot-check
";

/// What foot runs in `test-foot`: something to draw (`hyprctl version`), and
/// then a line read from the keyboard -- the keys the test presses, through
/// the compositor, foot's keymap and the pseudoterminal -- said on the
/// console, where the test reads it.
const FOOT_CHECK: &str =
    "#!/bin/sh\n/bin/hyprctl version\nread line\necho \"foot-typed: $line\" > /dev/console\n";

/// Where [`FOOT_CHECK`] goes.
const FOOT_CHECK_PATH: &str = "usr/libexec/ferrix/foot-check";

/// The line the console shows once foot has passed the typed keys on.
const FOOT_TYPED: &str = "foot-typed: ok";

/// Who uid 0 is. foot looks its user up for the shell it would start, and
/// says it could not as an error even when it was given a program instead;
/// with the user there, an error from foot is one worth failing on.
const FOOT_PASSWD: &str = "root:x:0:0:root:/:/bin/sh\n";

/// Where fontconfig keeps its cache, which it will not make itself.
const FONTCONFIG_CACHE: [&str; 3] = ["var", "var/cache", "var/cache/fontconfig"];

/// Where the port installs foot, and so where the configuration starts it.
const FOOT_PATH: &str = "bin/foot";

/// What foot says when fontconfig has found the image's font and freetype
/// has opened it: the path of the file it loaded.
const FOOT_FONT: &str = "/usr/share/fonts/dejavu/DejaVuSansMono.ttf";

/// What foot says once it has a grid, which it lays out from the font's
/// metrics: the last thing before its first frame.
const FOOT_GRID: &str = "cell width=";

/// What foot prints ahead of an error of its own, after the padding that
/// lines it up with `info` and `warn`.
const FOOT_ERROR: &str = "err: ";

/// The fewest colours a screen with foot's text on it has.
///
/// A window with nothing drawn in it is a handful: the compositor's
/// background, the border, and foot's own background -- seven, as the
/// development host's probe of a foot with no output records
/// (`src/user/system/linux/compositor/hyprix/probe/real-client.txt`). Text is antialiased, and
/// every glyph's edges are greys between foot's foreground and background,
/// so three short lines of it are dozens more.
const TEXT_COLOURS: usize = 24;

/// How long foot gets to connect, load its font, start `hyprctl` and draw
/// what it printed, after the compositor is on the screen: a static C
/// program of four megabytes and a font of a third of one, read from the
/// initramfs under emulation.
const FOOT_PATIENCE: Duration = Duration::from_secs(120);

/// How many different colours a screen has, or none when it is not the
/// compositor's: a screen with none of its background is the firmware's
/// console, whose antialiased text has as many colours as a terminal's.
fn colours(screen: &Image) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    let (pixels, _) = screen.pixels.as_chunks::<3>();
    for pixel in pixels {
        let _ = seen.insert(*pixel);
    }
    if seen.contains(&BACKGROUND) {
        seen.len()
    } else {
        0
    }
}

/// `ports` with what the foot boot adds beside the foot app: root's account,
/// the program foot runs (a script, for the zinc the boot carries), and
/// fontconfig's cache directories.
fn with_foot_files(mut ports: Vec<crate::ports::File>) -> Vec<crate::ports::File> {
    ports.push(crate::ports::File {
        path: "etc/passwd".to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(FOOT_PASSWD.as_bytes().to_vec()),
    });
    ports.push(crate::ports::File {
        path: FOOT_CHECK_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(FOOT_CHECK.as_bytes().to_vec()),
    });
    for directory in FONTCONFIG_CACHE {
        ports.push(crate::ports::File {
            path: directory.to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Directory,
        });
    }
    ports
}

/// `test-foot`: foot, a Wayland terminal nobody here wrote, on the
/// compositor, on Ferrix.
///
/// `docs/CHROME.md` §6's first milestone. The compositor's own tests use
/// clients written against its own crates, which proves the two halves
/// agree, not that the protocol is right; `src/user/system/linux/compositor/hyprix/probe` runs
/// foot against the compositor on a development host, which proves the
/// protocol and nothing about Ferrix. This is both at once: foot and every
/// library it links -- libwayland-client, libxkbcommon, pixman, freetype,
/// fontconfig, fcft -- built against ferrousli by
/// the foot app, started by the compositor on the guest,
/// drawing text in a font the image carries.
///
/// No picture is blessed: foot's text is foot's rendering of a font, and an
/// expected image would bless both. What is required is what foot said --
/// the image's font loaded, a grid laid out, no error of its own -- and a
/// screen with antialiased text on it, which a window with nothing drawn in
/// it is not.
pub(crate) fn test_foot(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if crate::display::target(arch).is_none() {
            println!("  {arch}: no virtio-gpu in QEMU's machine; skipped");
            continue;
        }
        let ports = crate::apps::taken(arch, &["foot"])?;
        if !ports.iter().any(|file| file.path == FOOT_PATH) {
            if arch == Arch::X86_64 {
                return Err(Error::new(format!(
                    "{arch}: foot is not built: `cargo xtask build-apps --app foot` builds it"
                )));
            }
            println!("  {arch}: foot is ported to x86-64 only; skipped");
            continue;
        }
        let programs = Programs::build(arch)?;
        // zinc is the `/bin/sh` foot's program is a script for.
        let carried = Carried {
            ports: with_foot_files(ports),
            zinc: crate::zinc::build(arch)?,
            ..Carried::none()
        };
        let (image, kernel) =
            build_image(arch, &programs, &undithered(FOOT_CONFIG), carried, args)?;
        let port = free_port()?;
        let mut qemu_args = args.clone();
        qemu_args.display = true;
        qemu_args.qmp_port = Some(port);
        let dump = paths::build_dir(arch).join("foot.ppm");
        let mut said: Vec<String> = Vec::new();
        let mut best: Option<Image> = None;
        let hook = |watching: &mut Watching<'_>| -> Result<()> {
            let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
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
            let deadline = Instant::now() + FOOT_PATIENCE;
            let _ = watching.read_more(deadline, |lines| {
                lines
                    .iter()
                    .any(|line| line.contains(FOOT_GRID) || line.contains(FOOT_ERROR))
            })?;
            // The screen until it carries text, or the time is up: the grid
            // is laid out before `hyprctl` has printed anything, and its
            // lines reach the screen a frame or two later.
            loop {
                qmp.screendump(Some(DEVICE_ID), &dump)?;
                let bytes = std::fs::read(&dump)
                    .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
                let screen = parse_ppm(&bytes)?;
                let enough = colours(&screen) >= TEXT_COLOURS;
                if best
                    .as_ref()
                    .is_none_or(|kept| colours(kept) < colours(&screen))
                {
                    best = Some(screen);
                }
                if enough || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            // Type into it: a line foot hands the program on its
            // pseudoterminal, which says it on the console. A client whose
            // libxkbcommon has no context drops every key, and draws all
            // the same.
            for key in ["o", "k", "ret"] {
                super::boot::press(&mut qmp, &[key])?;
            }
            let _ = watching.read_more(Instant::now() + FOOT_PATIENCE, |lines| {
                lines.iter().any(|line| line.contains(FOOT_TYPED))
            })?;
            let _ = watching.read_more(Instant::now() + Duration::from_secs(2), |_| false)?;
            said = watching
                .lines()
                .iter()
                .chain(watching.after())
                .cloned()
                .collect();
            Ok(())
        };
        let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
        judge_foot(arch, &said, best.as_ref(), &dump)?;
        if !said.iter().any(|line| line.contains(FOOT_TYPED)) {
            return Err(Error::new(format!(
                "{arch}: foot drew, but the keys pressed never reached its program: \
                 no `{FOOT_TYPED}` on the console"
            )));
        }
        println!("  {arch}: the keys typed into foot reached its program: `{FOOT_TYPED}`");
    }
    Ok(())
}

/// The configuration `test-vkgears` gives the compositor: vkgears, printing
/// the Vulkan device it drew on before it starts.
///
/// `MESA_VK_WSI_DEBUG,sw` for `docs/GPU.md` §6.1's reason: until the
/// compositor takes dmabuf, a frame the host's GPU drew is copied into
/// shared memory and handed over as `wl_shm`, which is what Mesa's Wayland
/// code does for a device it is told is a software one.
const VKGEARS_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-vkgears`.
env = MESA_VK_WSI_DEBUG,sw
exec-once = /bin/vkgears -info
";

/// Where the port installs vkgears, and so where the configuration starts it.
pub(super) const VKGEARS_PATH: &str = "bin/vkgears";

/// What vkgears prints of the device it is drawing on, before its first frame.
const VKGEARS_DEVICE: &str = "deviceName    = ";

/// What Mesa's Venus driver calls every device it hands out: the host's own
/// GPU, by its own name, after this.
const VENUS_DEVICE: &str = "Virtio-GPU Venus";

/// What vkgears prints every five seconds of drawing.
const VKGEARS_FRAMES: &str = " frames in ";

/// How long vkgears gets to start, reach the host's GPU through Venus and
/// draw its first five seconds: a static program of a few megabytes read
/// from the initramfs, then a Vulkan instance, device and pipeline made over
/// the render node.
const VKGEARS_PATIENCE: Duration = Duration::from_secs(120);

/// `test-vkgears`: Vulkan's gears on Ferrix, drawn by the host's GPU.
///
/// `docs/GPU.md` §6.1's exit. vkgears is mesa-demos' own, and Mesa's Venus
/// driver is linked into it (the vkgears app): it opens the
/// render node, makes a Venus context and its rings in host memory mapped
/// through the device's window, compiles nothing -- the SPIR-V goes to the
/// host's Vulkan driver -- and fences each frame on a ring, polling the
/// descriptors the node answers with. The host is Linux with a Venus-built
/// virglrenderer: QEMU's card is `--venus`'s.
///
/// No picture is blessed, and none could be taken: a GL console cannot be
/// dumped (§3.1). What is required is what vkgears said -- that its device is
/// the host's GPU through Venus, and that it drew frames -- and that the
/// kernel did not stop while it did.
pub(crate) fn test_vkgears(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    let ports = crate::apps::taken(arch, &["vkgears"])?;
    if !ports.iter().any(|file| file.path == VKGEARS_PATH) {
        return Err(Error::new(format!(
            "{arch}: vkgears is not built: `cargo xtask build-apps --app vkgears` builds it"
        )));
    }
    let programs = Programs::build(arch)?;
    let carried = Carried {
        ports,
        ..Carried::none()
    };
    let (image, kernel) = build_image(arch, &programs, &undithered(VKGEARS_CONFIG), carried, args)?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.gl = true;
    qemu_args.venus = true;
    let mut said: Vec<String> = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
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
        let _ = watching.read_more(Instant::now() + VKGEARS_PATIENCE, |lines| {
            lines
                .iter()
                .any(|line| line.contains(VKGEARS_FRAMES) || line.contains("FERRIX-PANIC"))
        })?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_vkgears(arch, &said)
}

/// What [`test_vkgears`] requires of what the guest said.
fn judge_vkgears(arch: Arch, said: &[String]) -> Result<()> {
    let transcript = || {
        said.iter()
            .map(|line| said_on_its_own(line).to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let fail = |why: String| Err(Error::new(format!("{arch}: {why}\n{}", transcript())));
    if let Some(line) = said.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return fail(format!(
            "the kernel stopped while vkgears ran: {}",
            line.trim()
        ));
    }
    let Some(device) = said
        .iter()
        .find_map(|line| said_on_its_own(line).split_once(VKGEARS_DEVICE))
        .map(|(_, name)| name.trim().to_owned())
    else {
        return fail("vkgears never named its Vulkan device".to_owned());
    };
    if !device.starts_with(VENUS_DEVICE) {
        return fail(format!(
            "vkgears drew on `{device}`, which is not the host's GPU through Venus"
        ));
    }
    let Some(frames) = said
        .iter()
        .map(|line| said_on_its_own(line))
        .find(|line| line.contains(VKGEARS_FRAMES))
    else {
        return fail(format!("vkgears found `{device}` and drew no frames"));
    };
    let drawn: u64 = frames
        .split_whitespace()
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    if drawn == 0 {
        return fail(format!("vkgears drew no frames on `{device}`: `{frames}`"));
    }
    println!("  {arch}: vkgears drew on `{device}`: {}", frames.trim());
    Ok(())
}

/// What [`test_foot`] requires of what the guest said and showed.
fn judge_foot(arch: Arch, said: &[String], screen: Option<&Image>, dump: &Path) -> Result<()> {
    let transcript = || {
        said.iter()
            .map(|line| said_on_its_own(line).to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let fail = |why: String| Err(Error::new(format!("{arch}: {why}\n{}", transcript())));
    if let Some(line) = said.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return fail(format!(
            "the kernel stopped while foot ran: {}",
            line.trim()
        ));
    }
    if let Some(line) = said
        .iter()
        .find(|line| said_on_its_own(line).trim_start().starts_with(FOOT_ERROR))
    {
        return fail(format!("foot said: {}", said_on_its_own(line)));
    }
    if !said.iter().any(|line| line.contains(FOOT_FONT)) {
        return fail(format!("foot never said it loaded {FOOT_FONT}"));
    }
    if !said.iter().any(|line| line.contains(FOOT_GRID)) {
        return fail("foot never laid out its grid".to_owned());
    }
    let Some(screen) = screen else {
        return fail("the boot took no picture".to_owned());
    };
    // The busiest screen, where it can be looked at: what is judged is a
    // count, and a person reading a failure wants the picture.
    let kept = dump.with_file_name("foot-busiest.ppm");
    let mut ppm = format!("P6\n{} {}\n255\n", screen.width, screen.height).into_bytes();
    ppm.extend_from_slice(&screen.pixels);
    std::fs::write(&kept, &ppm)
        .map_err(|error| Error::new(format!("writing {}: {error}", kept.display())))?;
    let found = colours(screen);
    if found < TEXT_COLOURS {
        return fail(format!(
            "the screen has {found} colours, fewer than the {TEXT_COLOURS} of a terminal with \
             text in it; the busiest screen is {}",
            kept.display()
        ));
    }
    println!(
        "  {arch}: foot loaded the image's DejaVu Sans Mono, laid out its grid, and drew \
         `hyprctl version` in {found} colours; the screen is {}",
        kept.display()
    );
    Ok(())
}

/// `test-xwindow`'s boot: yserver's volume at `/data`, or with
/// `--everything` the volume `run-compositor --everything` attaches, with
/// yserver merged into it. Not that desktop's 3D card: QMP's `screendump`
/// has no surface to read from `egl-headless`.
fn xwindow_args(args: &Args) -> Result<Args> {
    let mut args = args.clone();
    args.gl = false;
    args.data_image = Some(if args.everything {
        crate::everything::volume()?
    } else {
        crate::yserver::volume()?
    });
    if !args.memory_given {
        args.memory = crate::yserver::MEMORY;
    }
    Ok(args)
}

/// What `test-xwindow`'s image carries besides the programs: yserver's
/// start, the script, and what hands a press on the dialog to the window
/// manager, as Steam's login window does -- no X client in the volume sends
/// `_NET_WM_MOVERESIZE`.
fn xwindow_ports(arch: Arch) -> Result<Vec<crate::ports::File>> {
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.push(crate::ports::File {
        path: crate::yserver::XWINDOW_PATH.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(crate::yserver::XWINDOW_SCRIPT.as_bytes().to_vec()),
    });
    let xmoveresize = super::build(arch, "compositor-xmoveresize", "xmoveresize")?;
    ports.push(crate::ports::File {
        path: "bin/xmoveresize".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(
            std::fs::read(&xmoveresize).map_err(|error| {
                Error::new(format!("reading {}: {error}", xmoveresize.display()))
            })?,
        ),
    });
    Ok(ports)
}

/// `cargo xtask test-xwindow`: yserver as a client of the compositor, from
/// the volume `tools/common/fetch/fetch-yserver.sh` makes, and `xdpyinfo` and
/// `xev` against it (docs/YSERVER.md, Y2 to Y4): the root window must be
/// the compositor's screen, xev's window one of the compositor's, by its
/// title and class and on the screen, and the pointer, a click, the wheel
/// and keys put in through QEMU must reach xev as X events; later cases
/// check windows, menus and the clipboard both ways (Y5, Y6). yserver is
/// started as `run-compositor --everything` starts it (Y7), and with
/// `--everything` the gate attaches that desktop's merged volume.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, QEMU cannot be
/// run, the root window is not the screen's size, xev's window is not the
/// compositor's, or xev did not report the input.
pub(crate) fn test_xwindow(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-xwindow runs on x86-64 only, as yserver's volume does",
        ));
    }
    let args = xwindow_args(args)?;
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-xwindow needs ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static")
    })?;
    let programs = Programs::build(arch)?;
    let ports = xwindow_ports(arch)?;
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports,
        ..Carried::none()
    };
    // yserver is started as the `--everything` desktop starts it; its input
    // module says each cursor it gives the compositor at debug.
    let config = format!(
        "# Carried into the initramfs by `cargo xtask test-xwindow`.\n{}\
         env = RUST_LOG,info,yserver::wayland::input=debug\n{}exec-once = /bin/busybox sh /{}\n",
        crate::chrome::WINDOW_ENV,
        crate::yserver::desktop_config(),
        crate::yserver::XWINDOW_PATH
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("xwindow.ppm");
    let mut said: Vec<String> = Vec::new();
    let mut screen: Option<Image> = None;
    let mut menu: (Option<Image>, Option<Image>) = (None, None);
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let waiting = |marker: &'static str| {
            move |lines: &[String]| {
                lines
                    .iter()
                    .any(|line| line.contains(marker) || line.contains(crate::yserver::XWINDOW_END))
            }
        };
        let _ = watching.read_more(
            Instant::now() + CHROME_WINDOW_PATIENCE,
            waiting(crate::yserver::XWINDOW_INPUT),
        )?;
        // xev is running: the screen until its window is on it, or the time
        // is up; then xev's input, while the script waits for it.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut found = None;
        while found.is_none() {
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            let bytes = std::fs::read(&dump)
                .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
            let shown = parse_ppm(&bytes)?;
            found = crate::yserver::find_xev(&shown).map(|at| (at, (shown.width, shown.height)));
            screen = Some(shown);
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if let Some((at, size)) = found {
            crate::yserver::drive_xev(&mut qmp, watching, at, size)?;
            crate::yserver::drive_drag(&mut qmp, watching, size)?;
        }
        menu = crate::yserver::watch_menu(&mut qmp, watching, &dump)?;
        let ended = watching.read_more(
            Instant::now() + CHROME_WINDOW_PATIENCE,
            waiting(crate::yserver::XWINDOW_END),
        )?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        if ended {
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: the xwindow script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    crate::yserver::judge_xwindow(arch, &said)?;
    crate::yserver::judge_window_manager(arch, &said)?;
    crate::yserver::judge_xev(arch, &said, screen.as_ref(), &dump)?;
    crate::yserver::judge_xev_input(arch, &said)?;
    crate::yserver::judge_windows(arch, &said, screen.as_ref())?;
    crate::yserver::judge_drag(arch, &said)?;
    crate::yserver::judge_menu(arch, &said, (menu.0.as_ref(), menu.1.as_ref()))?;
    crate::yserver::judge_clipboard(arch, &said)
}
