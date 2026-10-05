//! `cargo xtask test-compositor`: the compositor itself, on Ferrix, on a
//! screen.
//!
//! `cargo xtask test-display` boots `src/user/system/linux/compositor/blank`, which fills the card
//! with one colour: the proof that the path from a program through
//! `/dev/dri/card0`, the kernel's display core and the ring-3 virtio-gpu
//! driver to QEMU's window works at all. This boots the compositor, which
//! goes through the same path with everything above it in place -- the
//! configuration, the layout, the renderer and its own DRM backend with two
//! dumb buffers and a page flip.
//!
//! Two `src/user/system/linux/compositor/pattern` clients are carried in the initramfs at
//! `/bin/pattern` and started by the compositor's own `exec-once`, so what
//! reaches the screen is two real Wayland clients tiled by the dwindle
//! layout -- the same picture `src/user/system/linux/compositor/hyprix/tests/two_clients.rs` makes
//! on the host, and compared against the same expected image that
//! `src/user/system/linux/compositor/render`'s own tests bless.
//!
//! # Three pictures, and two keybinds between them
//!
//! `docs/ROADMAP.md` stage 18's exit criterion asks for more than one
//! picture: the windows tiled, then a keybind sent through QEMU moving the
//! focus, then another swapping them, with each state required from a
//! screendump. So the configuration carried in the initramfs holds the two
//! `exec-once` lines *and* two binds, and this presses them through QMP's
//! `input-send-event` -- the same way a person would press them, through a
//! `virtio-keyboard-pci`, the kernel's evdev node and the compositor's seat.
//!
//! Each of the three states has an expected image of its own, blessed by
//! `src/user/system/linux/compositor/render`'s own tests by calling the renderer with rectangles
//! from `src/user/system/linux/compositor/layout`. The pictures compared here came from two
//! programs talking Wayland to a server that worked the same rectangles out
//! from their requests, so a difference between the two paths is a real one.
//!
//! # The two sockets
//!
//! `hyprctl` is carried in the initramfs beside the clients, because
//! Hyprland's own is not on Ferrix's image. The configuration's first
//! `exec-once` subscribes to `.socket2.sock` and prints every line, which is
//! what a bar does, so the transcript holds the compositor's whole event
//! stream: the monitor, the workspace, each window arriving, and the focus
//! moving as the keybinds are pressed. Two more binds ask `.socket.sock` for
//! `clients` and `activewindow`.
//!
//! # Where the rest is
//!
//! This file keeps what every boot shares -- the programs, what an image
//! carries, and the table of boots `test-compositor` makes -- and its
//! children hold the rest by what it is for. `boot` and `picture` are the
//! plumbing every judged boot uses. `layout`, `drawing`, `monitors`,
//! `pointer`, `protocols`, `machine`, `drawn_here`, `user_desktop` and
//! `idle` are the boots in [`BOOTS`], each with the pictures, the
//! configuration and the verdict it needs. `run` and `desktop` are the
//! desktops a person watches; `browser`, `apps`, `bench`, `steam_window` and
//! `steam_store` are the other commands that boot the compositor.

use std::path::{Path, PathBuf};
use std::time::Duration;

mod apps;
mod bench;
mod boot;
mod browser;
mod desktop;
mod drawing;
mod drawn_here;
mod hyprlock;
mod idle;
mod layout;
mod machine;
mod monitors;
mod picture;
mod pointer;
mod protocols;
mod run;
mod steam_store;
pub(crate) mod steam_window;
mod user_desktop;

pub(crate) use apps::{test_foot, test_vkgears, test_xwindow};
pub(crate) use bench::{bench_chrome, bench_chrome_video};
#[cfg(unix)]
pub(crate) use boot::desktop_image;
pub(crate) use boot::{absolute, button_event, client_image, press};
pub(crate) use browser::{test_chrome_audio, test_chrome_window};
pub(crate) use drawing::test_video;
pub(crate) use run::{board_files, run_compositor};
pub(crate) use steam_store::test_steam_store;

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result};
use drawing::{test_animation, test_decorations, test_gpu, test_terminal};
use drawn_here::{test_caption, test_waybar, test_waybar_volume};
use layout::{test_dispatchers, test_groups, test_plugins, test_rules, test_submap, test_twin};
use machine::{test_desktop, test_driver_restart};
use monitors::{test_edid, test_mode, test_monitors, test_scale, test_transform};
use pointer::{test_cursor, test_pointer};
use protocols::{
    test_bar, test_clipboard, test_lock, test_menu, test_screenshot, test_taskbar, test_typing,
};
use user_desktop::{test_everything_desktop, test_fuzzel, test_fuzzel_user};

/// The compositor's own background: `src/user/system/linux/compositor/render`'s `Style::BACKGROUND`,
/// which is Hyprland's `misc:background_color` default.
const BACKGROUND: [u8; 3] = [0x11, 0x11, 0x11];

/// What the compositor prints once it is on a screen, followed by the mode.
pub(crate) const MARKER: &str = "hyprix: card0";

/// What it prints instead when it could not start.
const FAILED: &str = "hyprix: failed";

/// Either, so the boot stops at whichever comes.
const EITHER: &str = "hyprix: ";

/// How long to let the screen settle after the marker: the clients have to
/// connect, be configured, draw and commit, and the flip is queued after
/// that. `test-display` allows four seconds for one program's fill.
///
/// Half a minute because of the slowest case, which is a window left alone
/// when its neighbour's client went: it has to be told its new size, draw a
/// buffer at it and commit, and only then is a frame drawn and flipped --
/// four round trips at a second and a half a frame under emulation. A
/// screen that is already right costs none of it: the wait ends the moment
/// the picture matches.
const SETTLE: Duration = Duration::from_secs(30);

/// The three pictures, in the order the keybinds make them. Each is blessed
/// by `src/user/system/linux/compositor/render`'s own tests.
const EXPECTED: [(&str, &str); 3] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "the focus moved left",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients-focus-left.xrle",
    ),
    (
        "the windows swapped",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients-swapped.xrle",
    ),
];

/// Where the clients, the control program, the plugin and the configuration
/// go in the initramfs, which is what the compositor's `exec-once`, its
/// `plugin` line and the binds name.
const CLIENT_PATH: &str = "bin/pattern";
const CTL_PATH: &str = "bin/hyprctl";
const PLUG_PATH: &str = "bin/plug";
const TERM_PATH: &str = "bin/term";
const CLIP_PATH: &str = "bin/clip";
const LSWT_PATH: &str = "bin/lswt";
const SHOT_PATH: &str = "bin/shot";
const LOCK_PATH: &str = "bin/lock";
const VKBD_PATH: &str = "bin/vkbd";
const VDAGENT_PATH: &str = "bin/vdagent";

/// Where `sessiond` goes: seat0's owner, which starts the `--everything`
/// desktop as its user (`crate::session`).
pub(crate) const SESSIOND_PATH: &str = "bin/sessiond";
/// hypridle, and the `loginctl` that reaches it: the hypridle app's, which
/// every compositor image carries whether or not the app is installed.
const HYPRIDLE_PATH: &str = "bin/hypridle";
const LOGINCTL_PATH: &str = "bin/loginctl";
/// `reboot`, with the word for the firmware busybox's cannot pass; it takes
/// the name, which busybox then does not link.
const REBOOT_PATH: &str = "bin/reboot";
const CONFIG_PATH: &str = "etc/hyprland.conf";

/// `build_image`.
#[derive(Clone, Debug)]
struct Programs {
    /// The compositor itself.
    hyprix: PathBuf,
    /// The test client that draws a pattern in a window.
    client: PathBuf,
    /// `hyprctl`.
    ctl: PathBuf,
    /// The plugin.
    plug: PathBuf,
    /// The terminal emulator.
    term: PathBuf,
    /// `clip`, the copy-and-paste program.
    clip: PathBuf,
    /// `lswt`, which lists the windows as a taskbar does.
    lswt: PathBuf,
    /// `shot`, which takes a screenshot as `grim` does.
    shot: PathBuf,
    /// `lock`, which locks the screen as `hyprlock` does.
    lock: PathBuf,
    /// `vkbd`, which types as `wtype` does.
    vkbd: PathBuf,
    /// `hypridle`, which runs commands when the seat goes idle.
    hypridle: PathBuf,
    /// `loginctl lock-session`, which reaches hypridle's `lock_cmd`.
    loginctl: PathBuf,
    /// `reboot`, which asks the firmware to come back up somewhere.
    reboot: PathBuf,
    /// `vdagent`, which joins the host's clipboard to this one.
    vdagent: PathBuf,
    /// `sessiond`, which starts a session as its user (`crate::session`).
    sessiond: PathBuf,
}

impl Programs {
    /// Build them all for `arch`.
    fn build(arch: Arch) -> Result<Self> {
        Ok(Self {
            hyprix: build(arch, "hyprix", "hyprix")?,
            client: build(arch, "compositor-pattern", "pattern")?,
            ctl: build(arch, "compositor-ctl", "hyprctl")?,
            plug: build(arch, "compositor-plug", "plug")?,
            term: crate::apps::program(arch, "term", "term")?,
            clip: build(arch, "compositor-clip", "clip")?,
            lswt: build(arch, "compositor-lswt", "lswt")?,
            shot: build(arch, "compositor-shot", "shot")?,
            lock: build(arch, "compositor-lock", "lock")?,
            vkbd: build(arch, "compositor-vkbd", "vkbd")?,
            hypridle: crate::apps::program(arch, "hypridle", "hypridle")?,
            loginctl: crate::apps::program(arch, "hypridle", "loginctl")?,
            reboot: build(arch, "compositor-reboot", "reboot")?,
            vdagent: build(arch, "compositor-vdagent", "vdagent")?,
            sessiond: build(arch, "compositor-sessiond", "sessiond")?,
        })
    }

    /// The ones the initramfs carries, each with the path it goes at.
    fn carried(&self) -> [(&'static str, &Path); 14] {
        [
            (CLIENT_PATH, self.client.as_path()),
            (CTL_PATH, self.ctl.as_path()),
            (PLUG_PATH, self.plug.as_path()),
            (TERM_PATH, self.term.as_path()),
            (CLIP_PATH, self.clip.as_path()),
            (LSWT_PATH, self.lswt.as_path()),
            (SHOT_PATH, self.shot.as_path()),
            (LOCK_PATH, self.lock.as_path()),
            (VKBD_PATH, self.vkbd.as_path()),
            (HYPRIDLE_PATH, self.hypridle.as_path()),
            (LOGINCTL_PATH, self.loginctl.as_path()),
            (REBOOT_PATH, self.reboot.as_path()),
            (VDAGENT_PATH, self.vdagent.as_path()),
            (SESSIOND_PATH, self.sessiond.as_path()),
        ]
    }
}

/// Build [`Programs`] for `arch`, for a test that boots the compositor after
/// a boot of its own and builds them before its first (stage 20, S-2).
pub(crate) fn build_programs(arch: Arch) -> Result<()> {
    Programs::build(arch).map(drop)
}

/// Build one of the compositor's programs for `arch`, and say where it is.
fn build(arch: Arch, package: &str, binary: &str) -> Result<PathBuf> {
    let target = crate::display::target(arch).ok_or_else(|| {
        Error::new(format!(
            "{arch} has no virtio-gpu in QEMU; the compositor test runs on x86_64 and aarch64"
        ))
    })?;
    let target_dir = paths::target_dir().join("compositor").join("hyprix");
    println!("  building src/user/system/linux/compositor/{binary} for {target}");
    let program = target_dir.join(target).join("release").join(binary);
    crate::builds::Build::cargo(
        format!("cargo build (src/user/system/linux/compositor/{binary}) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/compositor"),
    )
    .args(["build", "--release", "-p", package, "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program)
    .run()?;
    Ok(program)
}

/// `test-compositor` on each architecture that has a virtio-gpu.
///
/// # Errors
///
/// A screen that is not the compositor's background, or a compositor that
/// never reached one.
pub(crate) fn test_compositor(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if crate::display::target(arch).is_none() {
            println!("  {arch}: no virtio-gpu in QEMU's machine; skipped");
            continue;
        }
        let programs = Programs::build(arch)?;
        // `--gl` puts a GPU behind the card, and then the compositor draws
        // on it. Every boot below judges QEMU's screendump, which cannot
        // read such a card's console (`docs/GPU.md` §3.1), so a GPU boot is
        // one of its own, judged from inside the guest.
        if args.gl {
            test_gpu(arch, &programs, args)?;
            continue;
        }
        build_ahead(arch, args)?;
        // The first boot runs the kernel's self-checks on this machine, with
        // its card, input and seat; each one after it skips them, since the
        // same checks on the same machine would say the same thing again,
        // at 8 to 10 s a boot under emulation (`docs/TEST-TIME.md`, C2).
        // What a boot is judged on comes from its place here, never from the
        // flag that built its image, so an image that skipped when it should
        // not have cannot pass for one that was meant to.
        let mut boot_args = args.clone();
        let mut first = true;
        for (name, boot) in BOOTS {
            if wanted(args, name) {
                boot(arch, &programs, &boot_args)?;
                judge_checks(arch, name, first)?;
                first = false;
                boot_args.checks_skipped = true;
            }
        }
    }
    Ok(())
}

/// Build, before the first boot, every program a boot of [`BOOTS`] that
/// `--boot` asked for builds for itself beyond [`Programs`]: each boot still
/// builds it where it did, which is then cargo saying it is current. A run
/// under `FERRIX_BUILDS=plan:`, which records the builds and stops at the
/// first boot, then has every build the whole test makes (stage 20, S-2).
/// What a boot carries only when this machine has it -- the user's own
/// desktop for `fuzzel-user` and `everything-desktop` -- is built by those
/// boots alone.
fn build_ahead(arch: Arch, args: &Args) -> Result<()> {
    let asked = |names: &[&str]| names.iter().any(|name| wanted(args, name));
    if asked(&["restart", "waybar", "waybar-volume"]) {
        let _ = crate::zinc::build(arch)?;
    }
    if asked(&[
        "hyprlock",
        "hyprlock-unset",
        "hyprlock-session",
        "session-end",
    ]) {
        let _ = crate::apps::program(arch, "hyprlock", "hyprlock")?;
        let _ = crate::auth::carried(arch, None)?;
    }
    if asked(&["caption"]) {
        let _ = build(arch, "compositor-caption", "caption")?;
        let _ = build(Arch::X86_64, "compositor-caption", "caption")?;
    }
    if asked(&["waybar", "waybar-volume"]) {
        let _ = crate::apps::program(arch, "waybar", "waybar")?;
        let _ = crate::apps::program(Arch::X86_64, "waybar", "waybar")?;
    }
    if asked(&["waybar-volume"]) {
        let _ = crate::audio::build_media(arch, "media-pulsed", "pulsed")?;
    }
    if asked(&["fuzzel"]) {
        let _ = crate::apps::program(arch, "fuzzel", "fuzzel")?;
    }
    Ok(())
}

/// What a boot's image carries to type into: the busybox whose applets are
/// most of what a person types, zinc, the shell that runs them, and the
/// programs ported onto ferrousli.
///
/// A gate boot carries neither. Its archive is compared byte for byte against
/// what it has always been, and nothing it checks needs `ls`.
///
/// A watched boot carries both, and the terminal bind is the reason: a shell
/// whose every command answers `command not found` is not a terminal anybody
/// can use. That was the first thing tried in the window and it is what this
/// exists to fix.
struct Carried {
    /// The busybox, which goes to `/bin/busybox` with a link for each applet.
    busybox: Option<PathBuf>,
    /// zinc's bytes, which go to `/bin/zinc` with `/bin/zsh` beside them.
    zinc: Option<Vec<u8>>,
    /// The apps -- the ported programs, curl, git and foot among them --
    /// when this machine has them built. `build` and `run` put them on every image they make; a
    /// watched boot wants them for the same reason it wants the applets, and
    /// a gate boot's archive names none.
    ports: Vec<crate::ports::File>,
    /// `pulsed`, the sound server, as a service of the desktop beside the
    /// compositor, whose clients are told where it listens
    /// (`crate::init::desktop_files`).
    pulsed: Option<Vec<u8>>,
}

impl Carried {
    /// Neither, which is what every judged boot asks for.
    fn none() -> Self {
        Self {
            busybox: None,
            zinc: None,
            ports: Vec::new(),
            pulsed: None,
        }
    }

    /// What a watched boot should carry, from `--init` or from whatever
    /// busybox this machine already has.
    ///
    /// `--init` is the same flag `build`, `run` and `test-shell` take, with
    /// the same meanings: a path, `{arch}` replaced, or `ferrousli` for the
    /// one built against this tree's own library. Given nothing, an installed
    /// busybox is used where there is one and skipped where there is not:
    /// somebody who asked to look at the compositor did not ask to wait for a
    /// busybox to be built, and a screen with no shell is still the screen
    /// they wanted. Except under `--everything`, which is everything: there a
    /// busybox or an app that is not built yet is built, and a build that
    /// fails stops the run.
    ///
    /// # Errors
    ///
    /// A `--init` that names no file, a zinc that will not build, or under
    /// `--everything` a busybox or an app that will not.
    fn wanted(arch: Arch, args: &Args) -> Result<Self> {
        let asked = crate::optional_program(arch, args)?;
        let busybox = match asked {
            Some(program) => Some(program),
            None => match crate::busybox::installed_program(arch) {
                None if args.everything => {
                    println!("  no busybox yet; --everything carries one, so building it");
                    Some(crate::busybox::build(arch)?)
                }
                installed => installed,
            },
        };
        match &busybox {
            Some(program) => println!("  busybox {} in /bin", program.display()),
            None => {
                println!("  no busybox: zinc's own builtins are all the shell has");
                println!("    `cargo xtask busybox` builds one, or --init <PATH> names one");
            }
        }
        // The apps, each installed from its package (`docs/APPS.md` §5): the
        // ported programs among them, curl and git, foot and the rest. Under
        // `--everything` every app, one with no package built, and a build
        // that fails stops the run.
        let mut ports = crate::apps::installed(arch, args)?;
        // The package manager, which is the system's, not an app.
        ports.extend(crate::pkg::carried(arch)?);
        Ok(Self {
            busybox,
            zinc: crate::zinc::build(arch)?,
            ports,
            pulsed: None,
        })
    }
}

/// The static busybox the gates boot as `--init`, at
/// `~/.local/share/ferrix/busybox/<arch>/bin/busybox.static` (Alpine's
/// `busybox-static`), if this machine has one.
fn gates_busybox(arch: Arch) -> Option<String> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let path = Path::new(&home)
        .join(".local/share/ferrix/busybox")
        .join(arch.name())
        .join("bin/busybox.static");
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// Every boot `test-compositor` makes, in the order it makes them.
///
/// A table rather than a list of calls so that `--boot <name>` can pick one:
/// each takes minutes under emulation and there are twenty of them, so a
/// change to one is otherwise an hour a try.
type Boot = fn(Arch, &Programs, &Args) -> Result<()>;
const BOOTS: [(&str, Boot); 37] = [
    ("restart", test_driver_restart),
    ("dispatchers", test_dispatchers),
    ("bar", test_bar),
    ("decorations", test_decorations),
    ("scale", test_scale),
    ("groups", test_groups),
    ("monitors", test_monitors),
    ("plugins", test_plugins),
    ("animation", test_animation),
    ("terminal", test_terminal),
    ("rules", test_rules),
    ("clipboard", test_clipboard),
    ("submap", test_submap),
    ("taskbar", test_taskbar),
    ("twin", test_twin),
    ("screenshot", test_screenshot),
    ("lock", test_lock),
    ("hyprlock", hyprlock::test_hyprlock),
    ("hyprlock-unset", hyprlock::test_hyprlock_unset),
    ("hyprlock-session", hyprlock::test_hyprlock_session),
    ("session-end", hyprlock::test_session_end),
    ("menu", test_menu),
    ("pointer", test_pointer),
    ("cursor", test_cursor),
    ("mode", test_mode),
    ("transform", test_transform),
    ("typing", test_typing),
    ("edid", test_edid),
    ("desktop", test_desktop),
    ("idle", idle::test_idle),
    ("idle-user", idle::test_idle_user),
    ("caption", test_caption),
    ("waybar", test_waybar),
    ("waybar-volume", test_waybar_volume),
    ("fuzzel", test_fuzzel),
    ("fuzzel-user", test_fuzzel_user),
    ("everything-desktop", test_everything_desktop),
];

/// The boot that always skips the self-checks, whatever its place: the
/// desktop's own image, which carries `DESKTOP_DEFAULTS`.
const ALWAYS_UNCHECKED: &str = "desktop";

/// Whether the boot `name` just made ran the kernel's self-checks as it was
/// meant to, read from its serial log: an architecture's `first` boot must
/// end them with `FERRIX-BOOT-OK`, so a boot meant to be checked can never
/// skip them unseen, and each one after it with `FERRIX-BOOT-UNCHECKED`. A
/// self-check that fails panics, which ends the boot as a failure before
/// this is reached.
fn judge_checks(arch: Arch, name: &str, first: bool) -> Result<()> {
    let log = paths::build_dir(arch).join("serial.log");
    let said = std::fs::read_to_string(&log)
        .map_err(|error| Error::new(format!("reading {}: {error}", log.display())))?;
    let skipped = !first;
    let (want, other) = if skipped || name == ALWAYS_UNCHECKED {
        (crate::qemu::UNCHECKED_MARKER, crate::qemu::SUCCESS_MARKER)
    } else {
        (crate::qemu::SUCCESS_MARKER, crate::qemu::UNCHECKED_MARKER)
    };
    if !said.contains(want) || said.contains(other) {
        return Err(Error::new(format!(
            "{arch}: the {name} boot was meant to end `{want}` and did not, \
             so it {} the kernel's self-checks.\n  Serial output is in {}",
            if skipped { "did not skip" } else { "skipped" },
            log.display()
        )));
    }
    if !skipped && name != ALWAYS_UNCHECKED {
        println!("  {arch}: {name} ran the kernel's self-checks; the boots after it skip them");
    }
    Ok(())
}

/// Whether `--boot` asked for this one.
fn wanted(args: &Args, name: &str) -> bool {
    args.boot
        .as_ref()
        .is_none_or(|asked| name.contains(asked.as_str()))
}
