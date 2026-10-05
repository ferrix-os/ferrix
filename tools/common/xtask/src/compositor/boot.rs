//! What every judged boot shares: the image it is built into, the boot
//! itself, and the hands and eyes it has on the guest.
//!
//! A gate boot is the compositor under init with its programs in the
//! initramfs (`build_parts`), a QEMU with a QMP socket, keys and the pointer
//! sent through `input-send-event`, and screendumps taken until the screen is
//! the picture wanted or the time is up. `boot_and_dump_carrying` is that
//! whole loop for a boot a `Wanted` describes; a boot that needs something
//! the loop does not do builds its image with `build_image` and writes a hook
//! of its own from the same pieces, so the pieces live here and not beside
//! the first boot that needed them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::picture::{differences, expected, unexpected};
use super::pointer::{SWEEP, swept};
use super::{CONFIG_PATH, Carried, EITHER, FAILED, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The instance the control socket is under, which `hyprctl` finds by
/// looking when `HYPRLAND_INSTANCE_SIGNATURE` is not set.
const INSTANCE: &str = "ferrix";

/// The two binds that ask the compositor about itself, pressed after the
/// three pictures so that what they print describes the last of them.
const ASKED: [(&str, &[&str]); 2] = [("SUPER C", &["meta_l", "c"]), ("SUPER W", &["meta_l", "w"])];

/// Every program a boot carries: the compositor the kernel starts as init,
/// and the ones the initramfs holds for it to `exec`.
///
/// One value rather than a parameter apiece, because each boot below passes
/// the whole set through unchanged and a program added for one boot would
/// otherwise be a new argument in every signature between here and
/// A gate's compositor image for a client built elsewhere: the compositor
/// and its own programs, `config` as `/etc/hyprland.conf`, and `files`
/// carried beside them. The `badapple` app's window (`crate::badapple`) is
/// such a client.
pub(crate) fn client_image(
    arch: Arch,
    config: &str,
    files: Vec<crate::ports::File>,
    args: &Args,
) -> Result<(PathBuf, PathBuf)> {
    let programs = Programs::build(arch)?;
    let mut carried = Carried::none();
    carried.ports = files;
    build_image(arch, &programs, config, carried, args)
}

/// Boot the compositor, and take a screendump of each state in turn.
///
/// The compositor is the kernel's init program; the client and the
/// configuration are files in the initramfs, since the kernel embeds one
/// program and unpacks the rest. The arguments reach the compositor through
/// the init script, which `Options::parse` reads as its own command line.
/// What a boot must show.
///
/// The states are the first screen's, one to a keybind; `others` is what
/// every screen after the first must show once the last state has been
/// reached, which is how a second monitor is judged.
#[derive(Clone, Copy, Debug)]
pub(super) struct Wanted<'a> {
    /// The pictures the first screen must show, in order.
    pub(super) states: &'a [(&'a str, &'a str)],
    /// What each further screen must show at the end.
    pub(super) others: &'a [(&'a str, &'a str)],
    /// A window sliding: the keybind that starts it and the picture it ends
    /// in, with every distinct screendump along the way kept.
    pub(super) moving: Option<Moving<'a>>,
    /// Where to put the pointer, in QMP's own 0..0x7FFF coordinates, before
    /// the second picture is judged.
    ///
    /// Once and before that one picture, because that is the whole of what a
    /// pointer test needs: the first picture is the screen with no pointer
    /// on it and the second is the same screen with one, and a compositor
    /// that drew it in the wrong place fails the comparison.
    pub(super) pointer: Option<(i32, i32)>,
    /// Lines the boot is waited for before its transcript is taken.
    ///
    /// A picture can be right before the guest has finished saying what it
    /// did -- a program that prints when it exits has not exited yet -- and
    /// a boot that only drained for a moment would judge a transcript that
    /// was merely early. What is required of the lines is still the
    /// caller's; this only says which ones are worth waiting for.
    pub(super) awaiting: &'a [&'a str],
}

/// A window on its way somewhere: what starts it, and where it stops.
#[derive(Clone, Copy, Debug)]
pub(super) struct Moving<'a> {
    /// What the sequence shows, for the line the test prints.
    pub(super) what: &'a str,
    /// The expected image it must end in.
    pub(super) path: &'a str,
    /// The keys that start it, as QMP's `qcode` names them.
    pub(super) keys: &'a [&'a str],
}

impl Wanted<'_> {
    /// How many virtio-gpu devices the boot needs: one a screen.
    fn screens(&self) -> u32 {
        u32::try_from(self.others.len())
            .unwrap_or(0)
            .saturating_add(1)
    }
}

/// `config` with the blur's dither turned off, which is what every gate boot
/// is given.
///
/// `decoration:blur:noise` is 0.0117 in Hyprland and here, and the dither is
/// drawn. But the pictures a boot is judged against are
/// `src/user/system/linux/compositor/render`'s expected images, and those are blessed without it
/// (`Style::undithered` says why: a dither is the one thing a run-length
/// encoded image cannot hold). `src/user/system/linux/compositor/hyprix/tests/two_clients.rs`
/// tells the compositor under test the same thing for the same reason.
///
/// Without this a boot fails on a dither and nothing else: every pixel that
/// differs is inside the translucent half of the gradient client and is one
/// step up in each of its three channels, which is what a dither seen
/// through a window a quarter transparent rounds to. How many of them there
/// are depends on what is behind the window, so a change to the blur moves
/// the count and looks like the cause.
///
/// `run-compositor` does not come this way, and draws the dither.
pub(super) fn undithered(config: &str) -> String {
    format!("decoration:blur:noise = 0\n{config}")
}

pub(super) fn boot_and_dump(
    arch: Arch,
    programs: &Programs,
    config: &str,
    wanted: &Wanted<'_>,
    binds: &[(&str, &[&str])],
    args: &Args,
) -> Result<(Vec<Image>, Vec<String>)> {
    boot_and_dump_carrying(
        arch,
        programs,
        config,
        (Carried::none(), None),
        wanted,
        binds,
        args,
    )
}

/// A judged boot's image and kernel: `config` undithered, `carried` beside
/// the programs, and a command line of init's own, as `build_image` gives
/// every boot, with `words` after it.
pub(super) fn judged_image(
    arch: Arch,
    programs: &Programs,
    config: &str,
    carried: Carried,
    words: Option<&str>,
    args: &Args,
) -> Result<(PathBuf, PathBuf)> {
    let (loader, kernel, initramfs) =
        build_parts(arch, programs, &undithered(config), carried, args)?;
    let init = command_line(args);
    let command_line = match words {
        Some(words) => format!("{} {words}\n", init.trim_end()),
        None => init,
    };
    let image =
        crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, Some(&command_line))?;
    Ok((image, kernel.elf))
}

/// [`boot_and_dump`], with files carried beside the programs and words for
/// the kernel's command line after init's, as the EDID boot needs.
pub(super) fn boot_and_dump_carrying(
    arch: Arch,
    programs: &Programs,
    config: &str,
    (carried, cmdline): (Carried, Option<&str>),
    wanted: &Wanted<'_>,
    binds: &[(&str, &[&str])],
    args: &Args,
) -> Result<(Vec<Image>, Vec<String>)> {
    let (image, kernel) = judged_image(arch, programs, config, carried, cmdline, args)?;

    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    qemu_args.screens = wanted.screens();
    let dump = paths::build_dir(arch).join("compositor.ppm");
    let mut taken = Vec::new();
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        // A desktop never powers itself off: once the hook is done with it,
        // QEMU is stopped rather than waited for.
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        if let Some(line) = watching
            .lines()
            .iter()
            .rev()
            .find(|line| line.contains(FAILED))
        {
            return Err(Error::new(format!("{arch}: {}", line.trim())));
        }
        // The boot stops at the first `hyprix:` line, which is the seat
        // saying what it opened; the screen is up a little later.
        let up = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|line| line.contains(MARKER))
        })?;
        if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
            return Err(Error::new(format!(
                "{arch}: the compositor never printed `{MARKER}`"
            )));
        }
        say_the_marker(watching, arch);

        for (index, (what, path)) in wanted.states.iter().enumerate() {
            ask_for_state(&mut qmp, arch, index, binds, wanted.pointer)?;
            let want = expected(path)?;
            let screen = settle(&mut qmp, &dump, &want)?;
            let (found, count) = differences(&screen, &want);
            if count != 0 {
                // What the guest said, which is where the reason usually
                // is: a keybind that did not fire, a client that died, a
                // program that could not start.
                let _ = watching.read_more(Instant::now() + Duration::from_secs(2), |_| false)?;
                say_where_processors_are(&mut qmp, watching, arch);
                return Err(with_the_transcript(
                    &unexpected(arch, what, &screen, found, count),
                    watching,
                ));
            }
            println!(
                "  {arch}: {what}, every one of {} pixels as the renderer draws them",
                screen.width * screen.height
            );
            taken.push(screen);
        }

        // A window sliding: the keybind, then every distinct picture until
        // the one it ends in. What is required of them is the caller's --
        // how many there were, and what the last is -- because a frame
        // part-way through a slide is drawn at whatever time it was drawn
        // and cannot be an expected image.
        if let Some(moving) = wanted.moving {
            press(&mut qmp, moving.keys)?;
            println!("  {arch}: pressed the keys that start {}", moving.what);
            let mut kept = follow(&mut qmp, &dump, &expected(moving.path)?)?;
            println!(
                "  {arch}: {}: {} pictures, the last of them the one it ends in",
                moving.what,
                kept.len()
            );
            taken.append(&mut kept);
            // The compositor says how long its frames took while it draws,
            // and a boot with no keybinds to press asks the serial port for
            // nothing else: read what it said, so the caller can judge it.
            let _ = watching.read_more(Instant::now() + SETTLE, |lines| {
                lines
                    .iter()
                    .any(|line| line.contains("slowest of the last"))
            })?;
        }

        // The screens after the first, once the states have been reached:
        // each is a device of its own in QEMU, and a screendump names it.
        for (index, (what, path)) in wanted.others.iter().enumerate() {
            let which = u32::try_from(index).unwrap_or(0).saturating_add(1);
            let want = expected(path)?;
            let screen = settle_on(&mut qmp, &crate::display::device_id(which), &dump, &want)?;
            let (found, count) = differences(&screen, &want);
            if count != 0 {
                return Err(unexpected(arch, what, &screen, found, count));
            }
            println!(
                "  {arch}: screen {which}: {what}, every one of {} pixels as the renderer draws \
                 them",
                screen.width * screen.height
            );
            taken.push(screen);
        }

        if !binds.is_empty() && binds_the_asked(config) {
            ask_the_sockets(&mut qmp, watching, arch)?;
        }
        wait_for(watching, wanted.awaiting)?;
        // Whatever else the guest said by now, so that what is checked
        // against the transcript is what the boot actually printed rather
        // than what had been read when the last picture matched. That is
        // read once the guest goes quiet, except where the compositor's
        // frame reports are judged -- the pointer's sweep, a slide -- which
        // come a second or more after the frames they count: those boots
        // read for the whole two seconds, as every boot used to.
        if wanted.pointer.is_some() || wanted.moving.is_some() {
            let _ = watching.read_more(Instant::now() + Duration::from_secs(2), |_| false)?;
        } else {
            watching.read_what_was_said(Duration::from_secs(2))?;
        }
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    // A machine that stopped is not a boot that passed, whatever its screen
    // showed first. The pictures are judged as they are taken, and a kernel
    // that panics a moment after the last one matched had a right picture on
    // a dead machine: three boots did exactly that and said they had passed,
    // the night the compositor first drew on several threads, before a
    // fourth stopped early enough to spoil its picture.
    judge_still_running(arch, &said)?;
    Ok((taken, said))
}

/// When the kernel panicked during a boot whose picture then never came:
/// where every virtual processor is, from QEMU, since the panic's own
/// report names only the processor that gave up. FX-0001's is waiting on a
/// processor that stopped answering, and that one is still running
/// wherever it stuck. The whole dump is kept beside the screen's, and each
/// processor's instruction pointer is printed, for the kernel's symbols to
/// name (subtract the report's `kaslr slide`).
fn say_where_processors_are(qmp: &mut Qmp, watching: &Watching<'_>, arch: Arch) {
    let panicked = watching
        .lines()
        .iter()
        .chain(watching.after())
        .any(|line| line.contains(crate::qemu::PANIC_MARKER));
    if !panicked {
        return;
    }
    let dump = match qmp.registers() {
        Ok(dump) => dump,
        Err(error) => {
            println!(
                "  {arch}: the kernel panicked, and QEMU would not say where the processors \
                 are: {error}"
            );
            return;
        }
    };
    let path = paths::build_dir(arch).join("panic-registers.txt");
    let _ = std::fs::write(&path, &dump);
    for (cpu, line) in dump
        .lines()
        .filter(|line| line.contains("RIP=") || line.contains(" PC="))
        .enumerate()
    {
        println!(
            "  {arch}: after the panic, processor {cpu}: {}",
            line.trim()
        );
    }
    println!(
        "  {arch}: every register of every processor is in {}",
        path.display()
    );
}

/// That neither the kernel nor the compositor stopped during a boot whose
/// pictures matched.
pub(super) fn judge_still_running(arch: Arch, said: &[String]) -> Result<()> {
    if let Some(line) = said.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Err(Error::new(format!(
            "{arch}: the kernel stopped while the compositor ran: {}",
            line.trim()
        )));
    }
    if let Some(line) = said.iter().find(|line| compositor_ended(line)) {
        return Err(Error::new(format!(
            "{arch}: the compositor ended while it was being tested: {}",
            line.trim()
        )));
    }
    Ok(())
}

/// Whether `line` says the compositor ended. It is `hyprix.service` under
/// init (L10), which restarts it after a failure, so a crash reads as the
/// unit failing or being restarted, not as pid 1 exiting; a test's pictures
/// taken after a restart would be of a second compositor.
pub(super) fn compositor_ended(line: &str) -> bool {
    line.contains(crate::shell::EXITED)
        || (line.contains("hyprix.service: ")
            && (line.contains("failed") || line.contains("restarting")))
}

/// Read the guest until every line in `awaiting` has been said, or the time
/// is up.
///
/// Giving up quietly is right: what is required of the lines is the caller's
/// to say, and a failure there prints the whole transcript, which is more
/// use than "the wait timed out".
fn wait_for(watching: &mut Watching<'_>, awaiting: &[&str]) -> Result<()> {
    if awaiting.is_empty() {
        return Ok(());
    }
    let _ = watching.read_more(Instant::now() + SETTLE, |lines| {
        awaiting
            .iter()
            .all(|want| lines.iter().any(|line| line.contains(want)))
    })?;
    Ok(())
}

/// Build the bootable image for one boot: the compositor as init, the
/// client, `hyprctl` and the plugin in the initramfs, and the configuration
/// beside them.
#[cfg(unix)]
/// A desktop image whose compositor starts with `config`, and its kernel,
/// for a gate beside `test-compositor` that needs the compositor, its
/// programs and a shell to run them in sequence: `test-clipboard`. zinc and
/// nothing else of what a watched boot carries.
///
/// # Errors
///
/// A build that failed.
pub(crate) fn desktop_image(arch: Arch, config: &str, args: &Args) -> Result<(PathBuf, PathBuf)> {
    let programs = Programs::build(arch)?;
    let carried = Carried {
        zinc: crate::zinc::build(arch)?,
        ..Carried::none()
    };
    build_image(arch, &programs, config, carried, args)
}

pub(super) fn build_image(
    arch: Arch,
    programs: &Programs,
    config: &str,
    carried_too: Carried,
    args: &Args,
) -> Result<(PathBuf, PathBuf)> {
    let (loader, kernel, initramfs) = build_parts(arch, programs, config, carried_too, args)?;
    // The kernel as well as the image: the watcher symbolises a panic's
    // addresses out of it.
    let command_line = command_line(args);
    let image =
        crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, Some(&command_line))?;
    Ok((image, kernel.elf))
}

/// Init's command line, with `ferrix.checks=skip` after it when
/// [`Args::checks_skipped`] says this boot is not its architecture's first.
/// No gate or report counts a compositor boot as the self-checks' evidence:
/// that is `test-boot`'s, on every row.
fn command_line(args: &Args) -> String {
    let init = crate::init::command_line();
    if args.checks_skipped {
        format!("{} ferrix.checks=skip\n", init.trim_end())
    } else {
        init
    }
}

/// [`build_image`] for a desktop: the same image, carrying `defaults` --
/// [`DESKTOP_DEFAULTS`], and whatever `run-compositor` adds to them -- as
/// `FERRIX/DEFAULTS.TXT`, so that the kernel skips its self-checks as a
/// board's desktop does.
///
/// [`DESKTOP_DEFAULTS`]: super::run::DESKTOP_DEFAULTS
pub(super) fn build_desktop_image(
    arch: Arch,
    programs: &Programs,
    config: &str,
    carried_too: Carried,
    args: &Args,
    defaults: &str,
) -> Result<(PathBuf, PathBuf)> {
    let (loader, kernel, initramfs) = build_parts(arch, programs, config, carried_too, args)?;
    let command_line = crate::init::command_line();
    let image = crate::fat::write_image_carrying(
        arch,
        &loader,
        &kernel,
        &initramfs,
        Some(&command_line),
        Some(defaults),
    )?;
    Ok((image, kernel.elf))
}

/// libxkbcommon's default include directory, which every compositor image
/// carries (empty) so `xkb_context_new` succeeds in its clients.
const XKB_DIRECTORIES: [&str; 2] = ["usr/share/X11", "usr/share/X11/xkb"];

/// What [`build_image`] puts in an image, which is also what `flash` copies
/// onto a card: the loader, a kernel with no program in it, and the
/// initramfs, in which `/sbin/init` is pid 1 and the compositor
/// `hyprix.service` under `graphical.target` (`docs/INIT.md`, L10). Every
/// image made from these parts names `/sbin/init` on its command line.
pub(super) fn build_parts(
    arch: Arch,
    programs: &Programs,
    config: &str,
    carried_too: Carried,
    args: &Args,
) -> Result<(PathBuf, crate::cargo::Kernel, Vec<u8>)> {
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel(arch, args.release)?;
    let natives = crate::native::build(arch, args.release)?;
    let read = |path: &Path| -> Result<Vec<u8>> {
        std::fs::read(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    // `--instance` is what puts the control socket where `hyprctl` looks for
    // it.
    let config_path = format!("/{CONFIG_PATH}");
    let mut carried = crate::init::desktop_files(
        arch,
        &read(&programs.hyprix)?,
        &["--config", &config_path, "--instance", INSTANCE],
        carried_too.zinc.is_some(),
        carried_too.pulsed.as_deref(),
        args.session_user.as_deref(),
    )?;
    for (path, program) in programs.carried() {
        // The desktop's `reboot` takes the name from init's link to `svc`,
        // since it can pass a board's firmware the word that says where to
        // come back up.
        carried.retain(|file| file.path != path);
        carried.push(crate::ports::File {
            path: path.to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(read(program)?),
        });
    }
    // libxkbcommon's default include directory. The keymap a client is
    // handed is whole (`hyprix::keymap`), so nothing is read from it -- but
    // `xkb_context_new` fails outright when none of its default directories
    // exists, and a client then has no keymap and drops every key: foot,
    // and every client that links libxkbcommon, until 2026-10-04.
    for directory in XKB_DIRECTORIES {
        if !carried.iter().any(|file| file.path == directory) {
            carried.push(crate::ports::File {
                path: directory.to_owned(),
                mode: 0o755,
                content: crate::ports::Content::Directory,
            });
        }
    }
    carried.push(crate::ports::File {
        path: CONFIG_PATH.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(config.as_bytes().to_vec()),
    });
    // The busybox, zinc and the ported programs, when a caller asked for
    // them. A gate boot asks for none and its archive is the bytes it always
    // was; `run-compositor` asks for all three, because a shell whose every
    // command answers `command not found` is not one anybody can use. The
    // busybox and zinc go in through the same slots `build` and `run` use, so
    // the applet links, `/etc/passwd` and `/bin/zsh` come with them.
    carried.extend(carried_too.ports);
    drop_linked_xkb_directories(&mut carried);
    let initramfs = crate::initramfs::build(
        carried_too.busybox.as_deref(),
        &natives,
        carried_too.zinc.as_deref(),
        &carried,
    )?;
    Ok((loader, kernel, initramfs))
}

/// Leave out [`XKB_DIRECTORIES`] where the image links one of them, or a
/// directory above them, somewhere else.
///
/// The `--everything` desktop links `usr/share/X11` into its volume
/// (`yserver::LINKS`), where the real directory is, keymaps and all. The
/// empty placeholder went into the archive first, and the kernel cannot put
/// a link over a directory the archive has already filled: every
/// `run-compositor --everything` since 2026-10-04 stopped at "the initramfs
/// did not unpack: entry 739 could not be created: errno 39".
fn drop_linked_xkb_directories(carried: &mut Vec<crate::ports::File>) {
    let linked = |directory: &str| {
        carried.iter().any(|file| {
            matches!(file.content, crate::ports::Content::Link(_))
                && (directory == file.path || directory.starts_with(&format!("{}/", file.path)))
        })
    };
    let covered: Vec<&str> = XKB_DIRECTORIES
        .into_iter()
        .filter(|directory| linked(directory))
        .collect();
    carried.retain(|file| {
        !(matches!(file.content, crate::ports::Content::Directory)
            && covered.contains(&file.path.as_str()))
    });
}

/// Print the line the compositor said when its screen came up.
pub(super) fn say_the_marker(watching: &Watching<'_>, arch: Arch) {
    let marker = watching
        .lines()
        .iter()
        .chain(watching.after())
        .rev()
        .find(|line| line.contains(MARKER))
        .map_or("", |line| line.trim());
    println!("  {arch}: {}", marker.trim_start_matches("| ").trim());
}

/// Whether `config` binds [`ASKED`]'s keys to `hyprctl`, so that pressing
/// them asks the compositor about itself. A boot whose configuration binds
/// other keys and not these has nothing to ask with, and pressing them would
/// only wait out [`SETTLE`] for answers nothing prints.
fn binds_the_asked(config: &str) -> bool {
    [
        "bind = SUPER, C, exec, /bin/hyprctl",
        "bind = SUPER, W, exec, /bin/hyprctl",
    ]
    .iter()
    .all(|bind| config.lines().any(|line| line.starts_with(bind)))
}

/// Ask the compositor about itself, from inside the guest.
///
/// `hyprctl` is carried in the initramfs and a keybind `exec`s it, because
/// Hyprland's own is not on Ferrix's image and `exec` is how a person starts
/// anything.
fn ask_the_sockets(qmp: &mut Qmp, watching: &mut Watching<'_>, arch: Arch) -> Result<()> {
    for (name, keys) in ASKED {
        press(qmp, keys)?;
        println!("  {arch}: pressed {name}");
    }
    // The answers are whole when the second command's last line is in.
    let _ = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines.iter().any(|line| line.contains("workspace: 1"))
            && lines.iter().filter(|line| line.contains("title: ")).count() >= 3
    })?;
    Ok(())
}

/// Do whatever the state at `index` is reached by: the keybind before it,
/// and the pointer movement if the boot asked for one.
///
/// Each keybind but the first is sent after the picture before it has
/// settled, so that a state is never judged before the compositor has been
/// asked to make it.
pub(super) fn ask_for_state(
    qmp: &mut Qmp,
    arch: Arch,
    index: usize,
    binds: &[(&str, &[&str])],
    pointer: Option<(i32, i32)>,
) -> Result<()> {
    if let Some((name, keys)) = binds.get(index.wrapping_sub(1)) {
        press(qmp, keys)?;
        println!("  {arch}: pressed {name}");
    }
    if index == 1
        && let Some((x, y)) = pointer
    {
        let (steps, every) = SWEEP;
        for step in 0..steps {
            let (x, y) = swept(step);
            qmp.input_send_event(&[absolute("x", x), absolute("y", y)])?;
            std::thread::sleep(every);
        }
        qmp.input_send_event(&[absolute("x", x), absolute("y", y)])?;
        println!("  {arch}: swept the pointer through {steps} places and put it down");
    }
    Ok(())
}

/// One axis of an absolute pointer movement, as QMP takes it.
///
/// The value is 0..0x7FFF across the screen, which is what QEMU's virtio
/// tablet reports and what the compositor's seat turns back into pixels.
pub(crate) fn absolute(axis: &str, value: i32) -> String {
    format!(
        "{{\"type\":\"abs\",\"data\":{{\"axis\":{},\"value\":{value}}}}}",
        crate::display::json_string(axis)
    )
}

/// Press and release `keys` in order, as a hand does: the modifiers first,
/// the key last, and everything let go in reverse.
pub(crate) fn press(qmp: &mut Qmp, keys: &[&str]) -> Result<()> {
    for name in keys {
        qmp.input_send_event(&[key(name, true)])?;
    }
    for name in keys.iter().rev() {
        qmp.input_send_event(&[key(name, false)])?;
    }
    Ok(())
}

/// One `key` event of QMP's `input-send-event`, as its JSON.
fn key(name: &str, down: bool) -> String {
    format!(
        "{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":\
         {{\"type\":\"qcode\",\"data\":{}}}}}}}",
        crate::display::json_string(name)
    )
}

/// Take screendumps as fast as QEMU gives them until one is `want` or the
/// time is up, keeping every picture that differs from the one before it.
///
/// This is how a slide is watched: a window part-way along its curve is at a
/// place no expected image can hold, so what a test can require is that
/// there were several of them and that the last is where it was going.
fn follow(qmp: &mut Qmp, dump: &Path, want: &[u8]) -> Result<Vec<Image>> {
    let mut kept: Vec<Image> = Vec::new();
    let deadline = Instant::now() + SETTLE;
    loop {
        qmp.screendump(Some(DEVICE_ID), dump)?;
        let bytes = std::fs::read(dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        let screen = parse_ppm(&bytes)?;
        let fresh = kept.last().is_none_or(|last| last.pixels != screen.pixels);
        let done = differences(&screen, want).1 == 0;
        if fresh {
            kept.push(screen);
        }
        if done || Instant::now() >= deadline {
            return Ok(kept);
        }
    }
}

/// Take screendumps until one is `want`, or until the time is up.
///
/// A client has to draw and commit and the compositor has to compose and
/// flip, and each step is a round trip; the last dump taken is the one
/// judged, so a state that never arrives is reported as the picture it
/// stopped at rather than as a timeout.
pub(super) fn settle(qmp: &mut Qmp, dump: &Path, want: &[u8]) -> Result<Image> {
    settle_on(qmp, DEVICE_ID, dump, want)
}

/// The same, of one named device: a machine with two screens has a
/// virtio-gpu each, and a screendump names which.
fn settle_on(qmp: &mut Qmp, device: &str, dump: &Path, want: &[u8]) -> Result<Image> {
    let deadline = Instant::now() + SETTLE;
    loop {
        qmp.screendump(Some(device), dump)?;
        let bytes = std::fs::read(dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        let screen = parse_ppm(&bytes)?;
        if differences(&screen, want).1 == 0 || Instant::now() >= deadline {
            return Ok(screen);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// An error with the last of what the guest said after it.
///
/// A picture that is not the one expected says nothing about why; the lines
/// the compositor and its clients printed usually do.
pub(super) fn with_the_transcript(error: &Error, watching: &Watching<'_>) -> Error {
    let every: Vec<&String> = watching.lines().iter().chain(watching.after()).collect();
    let said: Vec<String> = every
        .iter()
        .skip(every.len().saturating_sub(20))
        .map(|line| line.trim().to_owned())
        .collect();
    Error::new(format!(
        "{error}\n  the guest's last lines:\n    {}",
        said.join("\n    ")
    ))
}

/// What the guest printed on a transcript line, without whatever the
/// watcher put in front of it.
///
/// The lines the watcher keeps are the guest's own; the timestamp and the
/// `|` are added when one is printed. Both forms are stripped here, because
/// a check that matched a whole line in one and not the other would pass or
/// fail on where the line came from rather than on what it said.
pub(super) fn said_on_its_own(line: &str) -> &str {
    line.split_once("| ").map_or(line, |(_, rest)| rest).trim()
}

/// A button of QEMU's pointer, pressed or let go: `wheel-down` is a wheel
/// click.
pub(crate) fn button_event(button: &str, down: bool) -> String {
    format!(
        "{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":{}}}}}",
        crate::display::json_string(button)
    )
}

#[cfg(test)]
mod tests {
    use crate::compositor::layout::{CONFIG, GROUP_CONFIG, PLUGIN_CONFIG};
    use crate::compositor::monitors::MONITOR_CONFIG;
    use crate::compositor::protocols::BAR_CONFIG;

    #[test]
    fn only_a_configuration_binding_the_asked_keys_is_asked() {
        // The boots that judge what `hyprctl` answered bind both keys.
        for config in [CONFIG, GROUP_CONFIG, MONITOR_CONFIG, PLUGIN_CONFIG] {
            assert!(super::binds_the_asked(config), "{config}");
        }
        // The bar's binds `A` alone: pressing SUPER C there asks nothing.
        assert!(!super::binds_the_asked(BAR_CONFIG));
    }

    fn file(path: &str, content: crate::ports::Content) -> crate::ports::File {
        crate::ports::File {
            path: path.to_owned(),
            mode: 0o755,
            content,
        }
    }

    fn paths(files: &[crate::ports::File]) -> Vec<&str> {
        files.iter().map(|file| file.path.as_str()).collect()
    }

    #[test]
    fn a_linked_x11_directory_takes_the_place_of_the_xkb_placeholder() {
        use crate::ports::Content::{Directory, Link};
        // The `--everything` image: the placeholder, then the volume's link.
        let mut carried = vec![
            file("usr/share/X11", Directory),
            file("usr/share/X11/xkb", Directory),
            file("usr/share/X11", Link("/data/usr/share/X11".to_owned())),
        ];
        super::drop_linked_xkb_directories(&mut carried);
        assert_eq!(paths(&carried), ["usr/share/X11"]);
        assert!(matches!(carried[0].content, Link(_)));

        // A link above it stands in the placeholder's way just the same.
        let mut carried = vec![
            file("usr/share/X11", Directory),
            file("usr/share/X11/xkb", Directory),
            file("usr/share", Link("/data/usr/share".to_owned())),
        ];
        super::drop_linked_xkb_directories(&mut carried);
        assert_eq!(paths(&carried), ["usr/share"]);

        // Every other image keeps the directory libxkbcommon needs.
        let mut carried = vec![
            file("usr/share/X11", Directory),
            file("usr/share/X11/xkb", Directory),
            file("usr/share/fonts", Link("/data/usr/share/fonts".to_owned())),
        ];
        super::drop_linked_xkb_directories(&mut carried);
        assert_eq!(
            paths(&carried),
            ["usr/share/X11", "usr/share/X11/xkb", "usr/share/fonts"]
        );
    }
}
