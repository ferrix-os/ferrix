//! What a desktop a person uses carries beside the compositor: the desktop's
//! own clients, a wallpaper, the dotfiles, and with `--chrome` and
//! `--everything` the browser, the compiler, yserver and Steam, with the
//! configuration lines that start them.
//!
//! A judged boot carries none of it, so its archive stays the bytes it was.

use std::path::Path;

use super::apps::VKGEARS_PATH;
use super::run::{with_layout, with_network};
use super::{CLIENT_PATH, CONFIG_PATH, Carried, build, steam_window};
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// The desktop's own clients, written for Ferrix from waybar, fuzzel,
/// hyprlock and hypridle (`docs/DESKTOP-CLIENTS.md`): each `(package,
/// binary)` is built and carried as `/bin/<binary>` on every desktop a person
/// uses (`run-compositor`, `flash --compositor`), so the user's own
/// `exec-once = waybar` and `bind = …, exec, hyprlock` find it. One line a
/// program, added by its stream when it lands. A judged boot carries none,
/// so its archive stays the bytes it was.
const DESKTOP_CLIENTS: &[(&str, &str)] = &[
    ("compositor-waybar", "waybar"),
    ("compositor-fuzzel", "fuzzel"),
    ("compositor-hyprlock", "hyprlock"),
];

/// Where `run-compositor` puts the wallpaper it carries.
const WALLPAPER_PATH: &str = "etc/wallpaper.fxwall";

/// Where it puts one that moves, which is a different file and a different
/// flag rather than the same name holding either: a boot that carried the
/// wrong one would say nothing until the screen was grey.
pub(super) const MOVIE_PATH: &str = "etc/wallpaper.ivf";

/// A desktop a person uses, from `config`: the layout and network lines,
/// the ssh server when asked for, the screen's size as a `monitor =` line,
/// and a wallpaper; with the busybox, zinc and ports that ride along.
/// Which wallpapers a desktop may be given.
#[derive(Clone, Copy, Debug)]
pub(super) enum Backdrop {
    /// Any kept one, still or moving: a desktop in QEMU.
    Any,
    /// A still one unless one is named, cut for a screen laid out at this
    /// size: a board's desktop (`crate::wallpaper::for_board` says why).
    Board((u32, u32)),
}

/// The size a desktop on a `size` screen is laid out at: the screen's own,
/// or with its width and height exchanged where the configuration turns the
/// monitor a quarter -- `monitor = ..., transform, 1` or `3`, and the
/// flipped `5` and `7` -- as the customer's portrait monitor on the DK1 is.
/// The last `monitor` line that says a transform wins, as it does in the
/// compositor, where a configuration's own line comes after the default.
pub(super) fn laid_out(size: (u32, u32), config: &str) -> (u32, u32) {
    let turned = config
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            if key.trim() != "monitor" {
                return None;
            }
            let fields: Vec<&str> = value.split(',').map(str::trim).collect();
            let at = fields.iter().position(|field| *field == "transform")?;
            fields.get(at + 1)?.parse::<u32>().ok()
        })
        .next_back()
        .is_some_and(|transform| transform % 2 == 1);
    if turned { (size.1, size.0) } else { size }
}

/// [`DESKTOP_CLIENTS`] built for `arch`, each at `/bin/<binary>`, and,
/// unless `carried` has a `/bin/foot` already, `/bin/foot` linked to
/// `/bin/term`: a real config's `$terminal = foot` gets term, which runs the
/// shell when started with no program, as foot does. With the foot app
/// installed it is foot itself; the link laid over it made foot unreachable
/// on every desktop until 2026-10-03.
pub(super) fn desktop_programs(
    arch: Arch,
    carried: &[crate::ports::File],
) -> Result<Vec<crate::ports::File>> {
    let mut files = Vec::new();
    for (package, binary) in DESKTOP_CLIENTS {
        let program = build(arch, package, binary)?;
        let bytes = std::fs::read(&program)
            .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
        files.push(crate::ports::File {
            path: format!("bin/{binary}"),
            mode: 0o755,
            content: crate::ports::Content::Bytes(bytes),
        });
    }
    if !carried.iter().any(|file| file.path == "bin/foot") {
        files.push(crate::ports::File {
            path: "bin/foot".to_owned(),
            mode: 0o777,
            content: crate::ports::Content::Link("/bin/term".to_owned()),
        });
    }
    Ok(files)
}

/// authd on a desktop, so that hyprlock has something to check a password
/// with (`docs/AUTH.md` P1.5): its programs, policies and units, its
/// account, and a seed only where `--auth-seed` or `--auth-seed-file` asked
/// for one. Without a seed the session's account has no password, and
/// hyprlock says so and does not lock (decision 4); `passwd` on the desktop
/// sets one. The accounts replace the ones the image's busybox slot writes,
/// a later entry in the archive replacing an earlier one. Nothing on an
/// architecture authd is not built for.
pub(super) fn desktop_auth(arch: Arch, args: &Args) -> Result<Vec<crate::ports::File>> {
    let mut files = crate::auth::carried(arch, None)?;
    if files.is_empty() {
        return Ok(files);
    }
    files.extend(crate::auth::seeds(args)?);
    for (path, text) in [
        (
            "etc/passwd",
            format!(
                "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/home/ferrix:/bin/zsh\n{}",
                crate::auth::PASSWD_LINE
            ),
        ),
        (
            "etc/group",
            format!("root:x:0:\nferrix:x:1000:\n{}", crate::auth::GROUP_LINE),
        ),
    ] {
        files.push(crate::ports::File {
            path: path.to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(text.into_bytes()),
        });
    }
    Ok(files)
}

pub(super) fn desktop(
    arch: Arch,
    config: String,
    size: (u32, u32),
    follow: bool,
    backdrop: Backdrop,
    args: &Args,
) -> Result<(String, Carried)> {
    let config = with_network(with_layout(config, args), args);
    let mut carried = Carried::wanted(arch, args)?;
    let programs = desktop_programs(arch, &carried.ports)?;
    carried.ports.extend(programs);
    carried.ports.extend(desktop_auth(arch, args)?);
    // The user's dotfiles and the fonts they name, from beside a real
    // `hyprland.conf`: `crate::dotfiles` says which and where.
    if let Some(path) = &args.config
        && !args.no_dotfiles
    {
        carried
            .ports
            .extend(crate::dotfiles::carried(Path::new(path))?);
    }
    carried.pulsed = pulsed(arch, args)?;
    if args.chrome {
        let mut links = chrome_links(arch, &carried.ports);
        if crate::chrome::on_ferrousli(args) {
            // ferrousli's loader takes `/lib64`'s place, so every program
            // on the volume runs on it -- Chrome, and with `--everything`
            // the compiler in the desktop's terminals too.
            println!("  {arch}: Chrome on ferrousli's loader and libc.so.6");
            links.retain(|file| file.path != "lib64");
            links.extend(crate::chrome::ferrousli_loader(
                arch,
                &crate::chrome::volume_for(arch)?,
                crate::chrome::WINDOW_PROGRAM,
                args,
            )?);
        }
        carried.ports.extend(links);
        carried.ports.extend(crate::chrome::window_files());
        carried.ports.push(crate::chrome::desktop_policy());
        carried
            .ports
            .push(crate::chrome::opener(arch, chrome_profile(args)));
        if args.everything {
            carry_everything(arch, args, &mut carried);
        }
    } else {
        carried.ports.extend(crate::rustc::default_links(args));
    }
    // The applications fuzzel lists, and their icons: vkgears among them
    // where the port was built and the card will offer Venus.
    let chrome = args
        .chrome
        .then(|| crate::chrome::window_command(crate::start_page::URL));
    let vkgears = args.venus && carried.ports.iter().any(|file| file.path == VKGEARS_PATH);
    carried
        .ports
        .extend(crate::fuzzel::files(chrome.as_deref(), vkgears)?);
    let config = crate::ssh::with_server(config, args, &mut carried.ports)?;
    let config = crate::badapple::on_the_desktop(arch, config, &mut carried.ports, args)?;
    // The page Chrome opens, with the keys this configuration binds.
    crate::start_page::carry(arch, &config, CONFIG_PATH, &mut carried.ports, args)?;
    // A wallpaper, from this machine's own and from nowhere else:
    // `crate::wallpaper` says where they come from and why a run never goes
    // looking. It is started before anything the configuration starts, so
    // the first thing on the screen is a desktop and not a grey one; a real
    // `hyprland.conf` starts a wallpaper program the image does not have,
    // so the line is added to one of those too.
    // The screen's size, said twice. To QEMU, as the card's `xres` and
    // `yres`, which is what a screen served over VNC is. And to the
    // compositor, as a `monitor =` line ahead of whatever the configuration
    // says itself: under a *window* the card prefers the window's size,
    // which for a window QEMU has just opened is 640x480 whatever it was
    // told, and the kernel lists the standard sizes beside that one so that
    // such a line can pick. A configuration's own line for the monitor
    // comes later and wins.
    //
    // Unless the desktop is to `follow` its window: then the line says
    // `preferred`, the card's preferred size is the window's, and when the
    // window is resized the driver hears of it and the compositor takes the
    // new size up. A pinned size in a larger window is stretched to fit,
    // pixels and all, which is what made small text look unsmoothed.
    let mode = if follow {
        "preferred".to_owned()
    } else {
        format!("{}x{}@60", size.0, size.1)
    };
    let scale = args.scale.as_deref().unwrap_or("1");
    let config = format!("monitor = , {mode}, auto, {scale}\n{config}");
    // A wallpaper that moves is started the way a still one is, and the way
    // `mpvpaper ALL <file>` is started from a Linux desktop's `exec-once`:
    // the difference is the flag, and that the frames were decoded on a
    // machine that has a decoder.
    let chosen = match backdrop {
        Backdrop::Any => crate::wallpaper::file(args, size)?,
        Backdrop::Board(screen) => crate::wallpaper::for_board(args, screen)?,
    };
    let config = match chosen {
        Some(chosen) => {
            // A moving one is started the way `mpvpaper` is on a desktop,
            // down to the words: `-o no-audio` and `ALL` mean here what they
            // mean there, so a line copied either way says the same thing.
            let (path, how) = match chosen {
                crate::wallpaper::Chosen::Still(_) => {
                    (WALLPAPER_PATH, format!("--wallpaper /{WALLPAPER_PATH}"))
                }
                // One word after `-o`, as mpvpaper's own examples write it.
                // A quoted string would arrive whole too: the compositor
                // splits `exec-once` as a shell would for its quoting
                // (`hyprix::command`).
                crate::wallpaper::Chosen::Moving(_) => {
                    (MOVIE_PATH, format!("--video -o no-audio ALL /{MOVIE_PATH}"))
                }
            };
            carried.ports.push(crate::ports::File {
                path: path.to_owned(),
                mode: 0o644,
                content: crate::ports::Content::Bytes(chosen.bytes()),
            });
            format!("exec-once = /{CLIENT_PATH} {how}\n{config}")
        }
        None => config,
    };
    Ok((config, carried))
}

/// The sound server for Chrome with a sound card, which Chrome takes over
/// ALSA once libpulse loads (docs/AUDIO.md, U2d): `None` without `--chrome`
/// and a card, or when the volume has no libpulse.
fn pulsed(arch: Arch, args: &Args) -> Result<Option<Vec<u8>>> {
    if !(args.chrome && (args.audio.is_some() || args.everything)) {
        return Ok(None);
    }
    if !crate::chrome::has_pulse(&crate::chrome::volume_for(arch)?) {
        println!(
            "  {arch}: Chrome's volume has no libpulse, so its sound goes through ALSA \
             with no pulsed (tools/common/fetch/fetch-chrome.sh again for it)"
        );
        return Ok(None);
    }
    let pulsed = crate::audio::build_media(arch, "media-pulsed", "pulsed")?;
    println!("  {arch}: pulsed, the sound server, as a service beside the compositor");
    std::fs::read(&pulsed)
        .map(Some)
        .map_err(|error| Error::new(format!("{}: {error}", pulsed.display())))
}

/// The volume's links for a desktop that already carries files of its own.
///
/// A link cannot stand where the archive has made a directory with files in
/// it -- the initramfs refuses the entry, and the boot stops. The desktop
/// carries foot's port, which puts fontconfig's configuration in
/// `/etc/fonts` and its one font in `/usr/share/fonts`; that
/// configuration scans every directory under `/usr/share/fonts`, so there the
/// volume's fonts are linked in beside foot's as `truetype`, where Debian
/// keeps them. `/etc/fonts/fonts.conf` stays foot's, and includes
/// `/etc/fonts/conf.d`, which foot's port does not make: that is linked to
/// the volume's, Debian's. Without it fontconfig knew no generic family and
/// no metric alias, and Chrome drew every face, its own tabs and toolbar
/// too, in foot's one font, a monospace.
fn chrome_links(arch: Arch, carried: &[crate::ports::File]) -> Vec<crate::ports::File> {
    let taken = |path: &str| {
        carried.iter().any(|file| {
            file.path == path
                || file
                    .path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut links: Vec<(&str, &str)> = Vec::new();
    for &(path, target) in crate::chrome::links(arch) {
        if !taken(path) {
            links.push((path, target));
        } else if path == "usr/share/fonts" {
            links.push(("usr/share/fonts/truetype", "/data/usr/share/fonts/truetype"));
        } else if path == "etc/fonts" && !taken("etc/fonts/conf.d") {
            links.push(("etc/fonts/conf.d", "/data/etc/fonts/conf.d"));
        }
    }
    crate::rustc::files(&links)
}

/// What `--everything` carries beside Chrome: the compiler's links, and
/// steamcmd, Claude Code, yserver and Steam's window where each volume has
/// been made.
fn carry_everything(arch: Arch, args: &Args, carried: &mut Carried) {
    let links = rustc_links(&carried.ports);
    carried.ports.extend(links);
    if crate::steamcmd::volume().is_ok() {
        let files = crate::steamcmd::desktop_files(&carried.ports);
        carried.ports.extend(files);
    }
    if arch == Arch::X86_64 && crate::claude_code::volume().is_ok() {
        let files = crate::claude_code::desktop_files(&carried.ports);
        carried.ports.extend(files);
    }
    let steam = with_steam_volume(args, arch);
    if arch == Arch::X86_64 && (crate::yserver::volume().is_ok() || steam) {
        let files = crate::yserver::desktop_files(&carried.ports);
        carried.ports.extend(files);
    }
    if steam {
        let files = steam_window::desktop_files(&carried.ports);
        carried.ports.extend(files);
    }
}

/// The compiler's links for `--everything`, less any path Chrome's links
/// have already made: the two volumes' glibc is one Debian's, so where both
/// name a path they name the same place on `/data`. Nor a path the archive
/// already has files under, as `/lib64` when ferrousli's loader is in it.
fn rustc_links(carried: &[crate::ports::File]) -> Vec<crate::ports::File> {
    let links: Vec<(&str, &str)> = crate::rustc::DEFAULT_LINKS
        .iter()
        .copied()
        .filter(|(path, _)| {
            !carried.iter().any(|file| {
                file.path == *path
                    || file
                        .path
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .collect();
    crate::rustc::files(&links)
}

/// The profile the desktop's Chrome is started with: on the volume that is
/// kept under `--persistent`, and in `/dev/shm` otherwise.
fn chrome_profile(args: &Args) -> &'static str {
    if args.persistent {
        crate::persistent::CHROME_PROFILE
    } else {
        "/dev/shm/chrome"
    }
}

/// What `run-compositor --chrome` adds to the desktop's configuration:
/// Chrome's environment, a window as the desktop starts, and SUPER+B for
/// another. Under `--persistent` its profile is on the volume that is kept.
pub(super) fn with_chrome(config: String, args: &Args, arch: Arch) -> String {
    if !args.chrome {
        return config;
    }
    let command = format!(
        "{} {}",
        crate::chrome::WINDOW_HOME,
        crate::chrome::window_command_with_profile(
            arch,
            crate::start_page::URL,
            chrome_profile(args)
        )
    );
    format!(
        "{config}\n# Added by `cargo xtask run-compositor --chrome`.\n{}{}exec-once = {command}\n\
         bind = SUPER, B, exec, {command}\n",
        crate::chrome::DESKTOP_ENV,
        crate::chrome::window_library_path(crate::chrome::on_ferrousli(args))
    )
}

/// What `run-compositor --everything` adds to the desktop's configuration
/// for the X server, when `tools/common/fetch/fetch-yserver.sh` has made its
/// volume: `crate::yserver::desktop_config`. Only with Chrome, whose flag
/// `--everything` sets, because that is what puts the merged volume, and so
/// yserver, on `/data`.
pub(super) fn with_yserver(config: String, args: &Args, arch: Arch) -> String {
    if !(args.everything && args.chrome && arch == Arch::X86_64)
        || (crate::yserver::volume().is_err() && !with_steam_volume(args, arch))
    {
        return config;
    }
    println!("  {arch}: yserver, the X server, on :0 beside the compositor");
    format!("{config}\n{}", crate::yserver::desktop_config())
}

/// Whether `run-compositor --everything` merges the volume
/// `tools/common/fetch/fetch-steam-window.sh` makes into its own
/// (`crate::everything`, which fetches it when it is missing), and so starts
/// Steam: x86-64 only, as the client is, and with Chrome, whose flag
/// `--everything` sets.
pub(super) fn with_steam_volume(args: &Args, arch: Arch) -> bool {
    args.everything && args.chrome && arch == Arch::X86_64 && steam_window::volume().is_ok()
}

/// What `run-compositor --everything` adds to the desktop's configuration
/// for Steam when its volume is merged: `steam_window::desktop_config`,
/// after yserver's, whose `:0` it waits for.
pub(super) fn with_steam(config: String, args: &Args, arch: Arch) -> String {
    if !with_steam_volume(args, arch) {
        return config;
    }
    println!("  {arch}: Steam, from its bootstrap, as uid 1000 on yserver's :0 (docs/STEAM.md)");
    format!("{config}\n{}", steam_window::desktop_config())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_compilers_links_leave_out_a_path_the_archive_has_files_under() {
        // ferrousli's loader in `/lib64`: a link there would be refused.
        let carried = vec![crate::ports::File {
            path: "lib64/ld-linux-x86-64.so.2".to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(Vec::new()),
        }];
        let links = super::rustc_links(&carried);
        assert!(!links.iter().any(|file| file.path == "lib64"));
        assert!(links.iter().any(|file| file.path == "bin/rustc"));
    }

    /// A monitor turned a quarter lays its desktop out the other way up; a
    /// half turn, none, or a line for no monitor at all leaves it be; and the
    /// configuration's own line, which comes later, wins.
    #[test]
    fn a_turned_monitor_lays_its_desktop_out_the_other_way() {
        let screen = (1280, 720);
        let turned = "monitor = HDMI-A-1, 1280x720@60, 0x0, 1, transform, 3\n";
        assert_eq!(super::laid_out(screen, turned), (720, 1280));
        let half = "monitor = , 1280x720@60, auto, 1, transform, 2\n";
        assert_eq!(super::laid_out(screen, half), screen);
        assert_eq!(super::laid_out(screen, "exec-once = /bin/term\n"), screen);
        let both = format!("{turned}monitor = HDMI-A-1, 1280x720@60, 0x0, 1, transform, 0\n");
        assert_eq!(super::laid_out(screen, &both), screen);
    }
}
