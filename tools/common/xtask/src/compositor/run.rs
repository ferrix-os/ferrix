//! `run-compositor` and `flash --compositor`: the desktop a person watches,
//! in QEMU's window or on a board's screen.
//!
//! These are the watched boots, not the judged ones. Their configuration is
//! the one a keyboard is for ([`RUN_CONFIG`]) or the person's own, with the
//! keyboard layout, the network and `--everything`'s additions appended to
//! it; `desktop` builds what the image carries beside it.

use std::path::Path;

use super::boot::{build_desktop_image, build_parts};
use super::browser::chrome_libc;
use super::desktop::{
    Backdrop, desktop, laid_out, with_chrome, with_steam, with_steam_volume, with_yserver,
};
use super::{Programs, gates_busybox, steam_window};
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// The configuration `run-compositor` writes when none was given: a desktop
/// somebody can drive.
///
/// The gate's `CONFIG` is written for a screendump -- two clients, and binds
/// a test presses -- and a person sitting in front of the window wants the
/// rest of what a keyboard is for. So this one opens a terminal as its first
/// `exec-once`: the boot ends at a shell prompt rather than at a picture,
/// which is what somebody who asked to watch the compositor asked for.
///
/// Every dispatcher named here is one `src/user/system/linux/compositor/layout` has. `SUPER+P`
/// starts a `src/user/system/linux/compositor/pattern` client, which is how the tiling a gate boot
/// shows is reached from a configuration that starts none.
const RUN_CONFIG: &str = "# Written into the initramfs by `cargo xtask run-compositor`.
# `--config <PATH>` carries a real `hyprland.conf` instead of this one.
# A terminal first, because a screen somebody is watching is one they want to
# type into: `/bin/term` runs a program on a pseudoterminal and `/bin/zinc`
# is the shell, with the busybox applets the image carries beside it. The
# pattern clients a gate boot tiles are a keybind away rather than started
# here: somebody who opened a desktop wants a shell, not a test pattern.
#
# `decoration:blur:enabled` is already Hyprland's own default, but the blur
# it draws is only what shows through a translucent window -- an opaque one
# covers it completely -- so the terminal needs an opacity below 1 for its
# own default to be visible at all.
windowrule = opacity 0.88, match:class ^(rocks\\.magical\\.term)$
# The clipboard agent, which joins this desktop's selection to the clipboard
# of whoever is watching (`docs/CLIPBOARD.md` §6). It needs `--clipboard` to
# have put the port on the bus; without one it says so and leaves, so a boot
# without the flag is a boot without a clipboard and not a boot with an error.
exec-once = /bin/vdagent
exec-once = /bin/term /bin/zinc
bind = SUPER, RETURN, exec, /bin/term /bin/zinc
bind = SUPER, P, exec, /bin/pattern gradient another
bind = SUPER, Q, killactive
bind = SUPER, F, fullscreen
bind = SUPER, V, togglefloating
bind = SUPER, L, movefocus, l
bind = SUPER, H, movefocus, r
bind = SUPER SHIFT, L, movewindow, r
bind = SUPER SHIFT, H, movewindow, l
bind = SUPER, 1, workspace, 1
bind = SUPER, 2, workspace, 2
bind = SUPER SHIFT, 1, movetoworkspace, 1
bind = SUPER SHIFT, 2, movetoworkspace, 2
bind = SUPER, C, exec, /bin/hyprctl clients
bind = SUPER, W, exec, /bin/hyprctl activewindow
";

/// Venus on the 3D card of an `--everything` desktop, where this host can
/// give it ([`crate::window::offers_venus`]): Vulkan on the host's GPU is a
/// feature of a watched desktop like the others `--everything` turns on, and
/// fuzzel then lists vkgears. Elsewhere the card stays virgl's, as it was.
fn everything_venus(arch: Arch, args: &mut Args) {
    // Not on the 3060 (`--nvidia`), whose domain has no virtio-gpu.
    if args.everything
        && args.gl
        && !args.venus
        && !args.nvidia
        && crate::window::offers_venus(arch)
    {
        println!("  gpu: Venus on the 3D card, so Vulkan runs on the host's GPU");
        args.venus = true;
    }
}

/// `--everything` with nothing else naming a configuration: the customer's
/// own desktop, the same one `--config` would carry, found where hyprland
/// keeps it. Without this, `--everything` shows [`RUN_CONFIG`]'s pattern
/// desktop and never carries the fonts, `waybar` or the launcher script a
/// real config names, which is why `/bin/waybar` on such a boot found no
/// `~/.config/waybar`.
///
/// Sets `args.config` to the file found, so the caller's own dotfile
/// carrying (keyed off it) runs unchanged, and returns the file's text with
/// one line appended: the real config's own binds are whatever the host's
/// programs are (`$terminal = foot`, and the rest), and one Ferrix does not
/// have fails quietly, as any missing `exec` does. `SUPER RETURN` is kept
/// working regardless, appended rather than substituted, so a desktop that
/// carries someone else's binds still opens a shell.
///
/// `None` when nothing changed: an explicit `--config`, no `--everything`,
/// `--no-dotfiles`, or no such file, in which case the caller keeps its own
/// default.
pub(super) fn everything_config(args: &mut Args) -> Result<Option<String>> {
    if args.config.is_some() || !args.everything || args.no_dotfiles {
        return Ok(None);
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(None);
    };
    let path = Path::new(&home).join(".config/hypr/hyprland.conf");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    args.config = Some(path.to_string_lossy().into_owned());
    Ok(Some(format!(
        "{}\n# Appended by `cargo xtask run-compositor --everything`: the clipboard agent a \
         host's config has no line for, and a terminal always a key away even when the \
         config's own $terminal is not one of Ferrix's programs.\n\
         exec-once = /bin/vdagent\n\
         bind = SUPER, RETURN, exec, /bin/term /bin/zinc\n",
        text.trim_end()
    )))
}

/// `config` with the interface brought up, when the boot has a network.
///
/// `--net` puts a virtio-net device on the bus and xtask's own gateway behind
/// it, and that is all it does: the guest has a device and no address, so a
/// name does not resolve and `ping` answers `bad address` for want of a
/// route rather than for want of a resolver. Every boot check that uses the
/// network runs `udhcpc` first, from its init script; a watched boot has no
/// init script, so the same command goes in as an `exec-once`. It is the
/// busybox applet, so it needs the busybox this boot carries: without one
/// there is nothing to run and the line would be a diagnostic at boot rather
/// than an address.
///
/// The address, the route and `/etc/resolv.conf` all come from the lease.
/// What answers the names is the gateway's own resolver at `10.0.2.3`, which
/// forwards what it does not serve to this host's.
pub(super) fn with_network(config: String, args: &Args) -> String {
    if !args.net {
        return config;
    }
    let mut config = config;
    if !config.ends_with('\n') {
        config.push('\n');
    }
    config.push_str("# Appended by `cargo xtask run-compositor --net`:\n");
    config.push_str(&format!("exec-once = /bin/{DHCP}\n"));
    println!("  network: the guest runs `{DHCP}` for its address");
    config
}

/// What brings the interface up, which is what every boot check that uses
/// the network runs before it: busybox's client, the tries and the timeout
/// `initramfs`'s own profile gives it.
const DHCP: &str = "udhcpc -i eth0 -n -q -t 5 -T 2";

/// `config` with the layouts `--layout` and `--variant` asked for.
///
/// Appended rather than substituted, and appended to a person's own file as
/// readily as to [`RUN_CONFIG`]: a later line overrides an earlier one, which
/// is hyprlang's rule and this configuration parser's, so two lines at the
/// end are the whole of it. A `--config` that sets `input:kb_layout` and a
/// `--layout` that disagrees means the flag wins, which is the way round a
/// flag typed now should win over a file written earlier.
///
/// The names are not checked here. The compositor ships the keymaps it has
/// and says which one it gave -- `hyprix: no keymap for kb_layout = ru,
/// kb_variant = ; using us` -- and a second opinion in this tool would be
/// one more place to keep the list.
pub(super) fn with_layout(config: String, args: &Args) -> String {
    if args.layout.is_none() && args.variant.is_none() {
        return config;
    }
    let mut config = config;
    if !config.ends_with('\n') {
        config.push('\n');
    }
    config.push_str("# Appended by `cargo xtask run-compositor`:\n");
    if let Some(layout) = &args.layout {
        println!("  keyboard: input:kb_layout = {layout}");
        config.push_str(&format!("input:kb_layout = {layout}\n"));
    }
    if let Some(variant) = &args.variant {
        println!("  keyboard: input:kb_variant = {variant}");
        config.push_str(&format!("input:kb_variant = {variant}\n"));
    }
    config
}

/// `cargo xtask run-compositor`: the compositor on a screen a person watches.
///
/// The same image `test-compositor` boots -- the compositor as init, its
/// clients and `hyprctl` in the initramfs -- with three differences, each of
/// which is what "watch it" means rather than "judge it":
///
/// * QEMU gets a window, or a VNC server where this host has no way to open
///   one. `window` decides which and says so.
/// * The serial port is this terminal, as `run`'s is, so the compositor's own
///   log is in front of the person watching it and `Ctrl-A x` ends the boot.
/// * The accelerator is `auto`, again as `run`'s is: a screen somebody is
///   looking at wants the hypervisor this host has, where a test wants `tcg`
///   on every host to be the same test.
///
/// The keyboard and the pointer are QEMU's own virtio devices, which the
/// window forwards to -- the same path `test-seat` drives over QMP -- so the
/// binds in [`RUN_CONFIG`] are pressed by pressing them.
///
/// # Errors
///
/// An architecture QEMU has no virtio-gpu for, a `--config` that cannot be
/// read, or a QEMU that cannot show a screen at all.
pub(crate) fn run_compositor(args: &Args) -> Result<()> {
    let arch = args.single_arch()?;
    if crate::display::target(arch).is_none() {
        return Err(Error::new(format!(
            "{arch} has no virtio-gpu in QEMU's machine; the compositor runs on x86_64 and aarch64"
        )));
    }
    let mut args = args.clone();
    // The layout of the keyboard in front of the window, when the command
    // line does not name one: `crate::keyboard` says why and from where.
    if args.layout.is_none()
        && args.variant.is_none()
        && let Some((layout, variant, file)) = crate::keyboard::host()
    {
        println!("  keyboard: this machine's, from {file} (--layout us for another)");
        args.layout = Some(layout);
        args.variant = variant;
    }
    everything_venus(arch, &mut args);
    if args.chrome {
        // One data disk: the browser's, in the rustc volume's place --
        // Chrome for Testing's on x86-64, Debian's Chromium on AArch64.
        // `--everything` has both downloads on the one disk the kernel
        // mounts, and is x86-64's.
        args.data_image = Some(if args.everything && arch == Arch::X86_64 {
            crate::everything::volume()?
        } else {
            crate::chrome::volume_for(arch)?
        });
        if !args.memory_given {
            args.memory = if with_steam_volume(&args, arch) {
                steam_window::MEMORY
            } else {
                crate::chrome::MEMORY
            };
        }
        // Chrome, and with `--everything` the compiler, run on ferrousli.
        chrome_libc(&mut args);
    } else if crate::chrome::on_ferrousli(&args) {
        return Err(Error::new(
            "--interpreter and --library need --chrome here: they put Chrome, and with \
             --everything rustc, on ferrousli",
        ));
    } else {
        crate::rustc::prepare_default(arch, &mut args)?;
    }
    let config = match everything_config(&mut args)? {
        // A terminal as the desktop starts, beside Chrome and Steam, as
        // `RUN_CONFIG`'s desktop has one; the host's own config names a
        // terminal Ferrix may not have. Not in `everything_config`, which the
        // `everything-desktop` boot shares: its first window must be the one
        // its SUPER Q opens.
        Some(config) => format!(
            "{config}# Appended by `cargo xtask run-compositor --everything`: a shell to \
             start with.\nexec-once = /bin/term /bin/zinc\n"
        ),
        None => match &args.config {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|error| Error::new(format!("reading {path}: {error}")))?,
            None => RUN_CONFIG.to_owned(),
        },
    };
    let config = with_steam(
        with_yserver(with_chrome(config, &args, arch), &args, arch),
        &args,
        arch,
    );
    // A watched boot has a network unless it was told not to: a person at a
    // screen expects a machine that can fetch something, and finding out
    // that `ping` says `bad address` for want of a device is nobody's
    // idea of a lesson. A boot that is judged still asks for `--net`,
    // because the bus a check enumerates must be the bus it has always
    // enumerated.
    let args = &Args {
        net: !args.no_net,
        session_user: args.everything.then(|| crate::session::USER.to_owned()),
        ..crate::ssh::checked(&args)
    };
    let programs = Programs::build(arch)?;
    let size = args.size.unwrap_or(crate::wallpaper::SCREEN);
    // A desktop somebody watches is the size of what it is watched in, and
    // follows it when that is resized: QEMU's GTK and SDL windows tell the
    // card their size whenever it changes, and so does a VNC viewer that
    // asks for a resize (`SetDesktopSize`, which TigerVNC sends to match its
    // window). A screen nobody resizes stays the size QEMU was given.
    // `--size` pins it.
    let follow = args.size.is_none();
    let (config, mut carried) = desktop(arch, config, size, follow, Backdrop::Any, args)?;
    // `--everything` runs as its user, through `sessiond` (customer,
    // 2026-10-03): what has to stay root becomes init's, and the dotfiles
    // seed the user's home.
    let config = if args.session_user.is_some() {
        println!(
            "  session: the desktop runs as {}, started by sessiond",
            crate::session::USER
        );
        let host = args
            .config
            .as_deref()
            .filter(|_| args.everything)
            .and_then(|path| std::fs::read_to_string(path).ok());
        crate::session::for_session(&config, &mut carried.ports, host.as_deref())
    } else {
        config
    };
    // A monitor of this machine's for the screen, so that a configuration
    // naming its monitors by description finds this one (`crate::edid`).
    // Only the EDID's name for itself is taken: the screen keeps the modes
    // the card offers, so it still follows its window (`docs/DISPLAY.md` §7).
    let defaults = match crate::edid::for_run(args.edid.as_deref())? {
        Some(edid) => {
            carried.ports.extend(edid.files);
            format!("{} {}\n", DESKTOP_DEFAULTS.trim_end(), edid.argument)
        }
        None => DESKTOP_DEFAULTS.to_owned(),
    };
    // The desktop a person watches boots as a board's does: with its
    // self-checks skipped, which only the `desktop` boot of the judged ones
    // is.
    let (image, _) = build_desktop_image(arch, &programs, &config, carried, args, &defaults)?;
    // On the 3060's own monitor: libvirt's domain, not a QEMU window.
    if args.nvidia {
        if std::env::var_os("FERRIX_N3C_BUILD_ONLY").is_some() {
            println!("  n3c: built {}; not booting", image.display());
            return Ok(());
        }
        return crate::nvidia::run_desktop(&image, args);
    }
    // The host's GPU behind the card where it can be had: `window::watched_gl`
    // says when, and why it is the default for a desktop somebody watches.
    let args = crate::window::watched_gl(arch, args)?;
    let args = Args {
        // The card, the keyboard and the tablet: `--display` is what puts a
        // virtio-gpu on the bus, and without one the compositor has no
        // `/dev/dri` to open. `test-compositor` sets it the same way.
        display: true,
        size: Some(size),
        accel: args
            .accel
            .clone()
            .or_else(|| (!args.gdb).then(|| "auto".to_owned())),
        ..args
    };
    println!("  {arch}: the compositor is init; its log is this terminal");
    crate::qemu::run(arch, &image, &args)
}

/// The serial port's shell on a board's desktop: busybox's, in a session of
/// its own with the console as its terminal.
const SERIAL_SHELL: &str = "/bin/busybox setsid -c /bin/busybox sh -i";

/// The command-line options a desktop's image carries, in the file the loader
/// reads after the card owner's `CMDLINE.TXT` (`FERRIX/DEFAULTS.TXT`).
///
/// `ferrix.checks=skip`: every stage is brought up and none of its self-checks
/// run (`src/kernel/src/checks.rs`). They were most of the DK1's 6.5 s from the
/// kernel's banner to its marker on 2026-09-24, and a desktop somebody
/// switches on is not a boot test; the rows that are boot tests never carry
/// this file, and a boot waited on for `FERRIX-BOOT-OK` that skipped them
/// fails. A card's `CMDLINE.TXT` saying `ferrix.checks=run` runs them anyway.
pub(crate) const DESKTOP_DEFAULTS: &str = "ferrix.checks=skip\n";

/// [`DESKTOP_DEFAULTS`] on a card, which also names `/sbin/init`: a QEMU
/// image says so in its `CMDLINE.TXT`, and a card's `CMDLINE.TXT` is its
/// owner's.
const DESKTOP_BOARD_DEFAULTS: &str = "ferrix.checks=skip ferrix.init=/sbin/init\n";

/// The screen a board's HDMI output runs: the DK1's LTDC scans out 720p60
/// and nothing else (`docs/DISPLAY.md` §6).
const BOARD_SCREEN: (u32, u32) = (1280, 720);

/// The keyboard a board's desktop reads unless told otherwise: the German
/// layout, which is the keyboard plugged into the DK1. It goes first in the
/// configuration, so a `--config` that sets `input:kb_layout` and a
/// `--layout`, appended at the end, each override it: a later line wins.
const BOARD_LAYOUT: &str = "input:kb_layout = de\n";

/// What `flash --compositor` puts on a card: the desktop `run-compositor`
/// boots -- the compositor as init, its clients, `hyprctl`, a shell -- as the
/// loader, the kernel and the initramfs `flash` copies.
///
/// No network, since the board has none Ferrix drives. A wallpaper as
/// `run-compositor` has one, but still unless one that moves is named, and
/// cut for the screen as the desktop lays it out (`laid_out`): a still
/// picture of the screen's own size is copied once and costs a frame
/// nothing, where a video decoded on a 650 MHz Cortex-A7 would cost most of
/// the machine. `--wallpaper none` is a bare desktop.
pub(crate) fn board_files(arch: Arch, args: &Args) -> Result<crate::flash::BoardFiles> {
    if crate::display::target(arch).is_none() {
        return Err(Error::new(format!(
            "the compositor is not built for {arch}"
        )));
    }
    let host = match &args.config {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|error| Error::new(format!("reading {path}: {error}")))?,
        ),
        None => None,
    };
    let config = match (&host, args.session) {
        // A session's desktop is a person's own configuration, which names
        // a terminal Ferrix may not have: one always a key away, and one to
        // start with, as `run-compositor --everything` adds.
        (Some(host), true) => format!(
            "{}\n# Appended by `cargo xtask flash --compositor --session`: a shell to start \
             with, and one always a key away.\n\
             exec-once = /bin/term /bin/zinc\n\
             bind = SUPER, RETURN, exec, /bin/term /bin/zinc\n",
            host.trim_end()
        ),
        (Some(host), false) => host.clone(),
        (None, _) => RUN_CONFIG.to_owned(),
    };
    // `flash --compositor --chrome` is a board's desktop with the browser
    // on it, from a volume the board attaches itself: the Pixel 7's VM gives
    // crosvm Chromium's as a disk (`tools/vendor/google/pixel7`).
    let config = with_chrome(format!("{BOARD_LAYOUT}{config}"), args, arch);
    // A shell with no `ls` or `mkdir` is what the board's first desktop had:
    // the only busybox `Carried::wanted` finds unasked is ferrousli's, which
    // has no ARM port. So the static one the gates boot is carried, when it
    // is where they keep it and nothing else was named.
    let init = args
        .init
        .clone()
        .or_else(|| std::env::var("FERRIX_INIT").ok())
        .or_else(|| gates_busybox(arch));
    let args = &Args {
        net: false,
        init,
        session_user: args.session.then(|| crate::session::USER.to_owned()),
        ..args.clone()
    };
    let programs = Programs::build(arch)?;
    let size = args.size.unwrap_or(BOARD_SCREEN);
    let screen = laid_out(size, &config);
    let (config, mut carried) = desktop(arch, config, size, false, Backdrop::Board(screen), args)?;
    // `--session`: the desktop runs as its user, through `sessiond`, and the
    // dotfiles seed the home disk (`crate::session`). The board's screen is
    // said again after the configuration, whose own catch-all `monitor =`
    // line -- a desktop's, at its own scale -- would otherwise win over the
    // one `desktop` put first: a phone's screen at a monitor's scale is
    // text a few millimetres high. A board boots with no root disk, and the
    // kernel's committer, which commits `/home` and `/data` every 30 s, runs
    // only beside one: without a `sync` of its own, the seeded home and
    // every edit there would stay in memory, and a VM stopped from outside
    // -- the Pixel 7 app's `crosvm stop` -- would leave the disk empty.
    let config = if args.session_user.is_some() {
        println!(
            "  session: the desktop runs as {}, started by sessiond",
            crate::session::USER
        );
        let config = crate::session::for_session(&config, &mut carried.ports, host.as_deref());
        if let Some(by) = args.bar_zoom {
            crate::session::zoom_bar(&mut carried.ports, by);
            println!("  session: waybar {by} times its size");
        }
        if !args.bar_drop.is_empty() || args.bar_margin_right.is_some() {
            crate::session::trim_bar(&mut carried.ports, &args.bar_drop, args.bar_margin_right);
            println!(
                "  session: waybar without {}, {} px from the right edge",
                args.bar_drop.join(", "),
                args.bar_margin_right.unwrap_or(0)
            );
        }
        let scale = args.scale.as_deref().unwrap_or("1");
        format!(
            "{config}# The board's screen, over the configuration's own monitor lines \
             (`cargo xtask flash --compositor --session`).\n\
             monitor = , {}x{}@60, auto, {scale}\n\
             # /home and /data committed every 30 s, as a root disk's committer would.\n\
             exec-once = /bin/busybox sh -c 'while /bin/busybox sleep 30; do /bin/busybox sync; done'\n",
            size.0, size.1
        )
    } else {
        config
    };
    // Nor the curl and git apps: on ARMv7-A they are curl, git and the TLS
    // test server curl's gate talks to, programs for a network the board
    // does not have, and 13 MB of an archive the loader reads off the card
    // at some 16 MB/s on every boot (2026-09-24). `run-compositor` still
    // carries them.
    let networked: Vec<String> = crate::apps::taken(arch, &["curl", "git"])?
        .into_iter()
        .map(|file| file.path)
        .collect();
    let before = carried.ports.len();
    carried.ports.retain(|file| !networked.contains(&file.path));
    if carried.ports.len() < before {
        println!("  curl and git stay off the card: the board has no network for them");
    }
    // A shell on the serial port beside the desktop, which is how a board
    // with nobody at its screen is reached -- and where `reboot
    // --firmware-setup` takes it back to U-Boot's prompt. It inherits the
    // compositor's console, and `setsid -c` makes that its terminal, so
    // Ctrl-C reaches what it runs. busybox's `sh`, since the shell a
    // terminal window opens is the desktop's own.
    let config = if carried.busybox.is_some() {
        format!("exec-once = {SERIAL_SHELL}\n{config}")
    } else {
        config
    };
    let (loader, kernel, initramfs) = build_parts(arch, &programs, &config, carried, args)?;
    Ok(crate::flash::BoardFiles {
        loader,
        kernel,
        initramfs,
        defaults: Some(DESKTOP_BOARD_DEFAULTS),
    })
}

#[cfg(test)]
mod tests {
    use super::{BOARD_LAYOUT, RUN_CONFIG, with_layout};
    use crate::args::Args;

    /// The flags a watched boot takes for its keyboard, appended so that they
    /// beat whatever the configuration said: a later line overrides an
    /// earlier one, which the configuration parser has its own test for.
    #[test]
    fn the_layout_flags_are_appended_to_whatever_configuration_is_used() {
        let asked = |layout: Option<&str>, variant: Option<&str>| {
            with_layout(
                "input:kb_layout = fr\n".to_owned(),
                &Args {
                    layout: layout.map(str::to_owned),
                    variant: variant.map(str::to_owned),
                    ..Args::default()
                },
            )
        };

        // Neither flag leaves the configuration exactly as it was, which is
        // what a person's own file must get.
        assert_eq!(asked(None, None), "input:kb_layout = fr\n");

        // Both, in the order the options are read.
        let both = asked(Some("de,us"), Some("nodeadkeys,"));
        assert!(both.starts_with("input:kb_layout = fr\n"), "{both}");
        assert!(
            both.ends_with("input:kb_layout = de,us\ninput:kb_variant = nodeadkeys,\n"),
            "{both}"
        );

        // A variant on its own is a variant of whatever the file's layout is.
        let variant = asked(None, Some("dvorak"));
        assert!(
            variant.ends_with("input:kb_variant = dvorak\n"),
            "{variant}"
        );
        assert!(!variant.contains("kb_layout = de"), "{variant}");
    }

    /// A configuration that does not end in a newline still gets its own
    /// line: the flag would otherwise land on the end of the last one.
    #[test]
    fn a_configuration_with_no_final_newline_gets_one() {
        let appended = with_layout(
            "bind = SUPER, Q, killactive".to_owned(),
            &Args {
                layout: Some("de".to_owned()),
                ..Args::default()
            },
        );
        assert!(appended.contains("killactive\n"), "{appended}");
        assert!(appended.ends_with("input:kb_layout = de\n"), "{appended}");
    }

    /// A board's desktop types German unless a `--config` or a `--layout`
    /// says otherwise. The default is the first line, so either of those
    /// comes after it and wins: the parser takes the last line that sets an
    /// option, and has its own test for that.
    #[test]
    fn a_boards_keyboard_is_german_until_something_later_says_otherwise() {
        assert_eq!(BOARD_LAYOUT, "input:kb_layout = de\n");
        assert!(!RUN_CONFIG.contains("kb_layout"));

        let flag = with_layout(
            format!("{BOARD_LAYOUT}{RUN_CONFIG}"),
            &Args {
                layout: Some("fr".to_owned()),
                ..Args::default()
            },
        );
        assert!(flag.starts_with(BOARD_LAYOUT), "{flag}");
        assert!(flag.ends_with("input:kb_layout = fr\n"), "{flag}");
    }

    /// The configuration a watched boot writes when nothing else was named
    /// starts a terminal, because a screen somebody is watching is one they
    /// want to type into.
    #[test]
    fn the_default_configuration_opens_a_terminal() {
        assert!(RUN_CONFIG.contains("exec-once = /bin/term /bin/zinc"));
        assert!(RUN_CONFIG.contains("bind = SUPER, RETURN, exec, /bin/term /bin/zinc"));
    }
}
