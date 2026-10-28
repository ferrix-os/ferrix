//! The live ISO (`docs/INSTALLER.md` §3, `docs/TRY-FERRIX.md`): `cargo
//! xtask iso` and its gate, `test-iso`.
//!
//! The medium is the release desktop -- Yoru on hyprix, the apps a person's
//! image carries, the release profile with the kernel stripped -- with four
//! differences:
//!
//! * `/` stays in memory (`ferrix.root=tmpfs`), so booting it never mounts
//!   or installs onto a disk the machine has, and `ferrix.live` says the
//!   session is a live one;
//! * the session is uid 1000's, `ferrix`, with no password, logged in at
//!   once as Ubuntu's live session is (§10 decision 2): `sessiond --user
//!   1000`, so the installed system, whose first boot renames that account
//!   to the person's, starts its session the same way;
//! * the installer is in it: `/sbin/ferrix-install`, its daemon
//!   `ferrix-installd.service` (only under `ferrix.live`), the first-boot
//!   `ferrix-setup.service` (which does nothing on the live session), and
//!   the graphical installer, which the session starts at login;
//! * the volume carries `FERRIX/SETUP.TXT`, a placeholder the installer
//!   fills in on the installed disk's copy, `README.TXT`, and the licence
//!   inventory `LICENSES.MD`.
//!
//! The volume goes into a hybrid ISO (`crate::iso`): El Torito for a CD and
//! a GUID partition table for a disk, the partition named `FERRIX-LIVE` as
//! `cargo xtask live`'s medium's is, so `ferrix-install` finds it the same
//! way when the medium is a disk Ferrix drives.
//!
//! `test-iso` boots the ISO under OVMF twice, as a CD on q35's AHCI port and
//! as a USB stick on an xHCI controller, and requires the live desktop:
//! hyprix's card, the session started as uid 1000 by `sessiond`, the
//! installer's daemon listening, the graphical installer's window, a
//! screen that is not one colour, and a first click on "Install Ferrix",
//! where the screen shows it, that moves the installer on to its next page.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::build_parts;
use super::desktop::{Backdrop, desktop};
use super::{MARKER, Programs};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The ISO, in the architecture's build directory.
pub(crate) const ISO: &str = "ferrix-live.iso";

/// The licence notice the ISO carries, beside it in the build directory.
pub(crate) const NOTICE: &str = "ferrix-live-THIRD-PARTY-LICENSES.md";

/// The volume identifier, which a file manager shows for the CD.
const LABEL: &str = "FERRIX_LIVE";

/// What the live session adds to the desktop's configuration.
const LIVE_LINES: &str = "# The live session (cargo xtask iso): the installer opens at login, and only\n\
# on a live session -- it exits at once where the command line lacks ferrix.live.\n\
exec-once = /bin/ferrix-installer --autostart\n";

/// The line of the desktop's configuration that opens a terminal at login,
/// which the live session leaves out ([`live_config`]).
const BOOT_TERMINAL: &str = "exec-once = /bin/term /bin/zinc";

/// The live session's `hyprland.conf`: the desktop's, less the terminal it
/// opens at login, and the installer started instead.
///
/// The installer is to be the one window a live session opens with. With a
/// terminal started beside it, which of the two mapped last -- and so lay
/// on top, focused -- was a race the boot's timing decided: on about one
/// boot in seven (2026-10-28, v0.2.0's candidate aa86f862d) the terminal
/// covered the installer's buttons, and a click on "Install Ferrix" went to
/// it. A terminal stays a key (SUPER RETURN) or the dock's button away.
fn live_config() -> String {
    let desktop: String = super::run::RUN_CONFIG
        .lines()
        .filter(|line| line.trim() != BOOT_TERMINAL)
        .map(|line| format!("{line}\n"))
        .collect();
    format!("{desktop}{LIVE_LINES}")
}

/// The live session's words after the image's own command line.
///
/// `ferrix.efifb` lends the firmware's framebuffer to `efifb` where it is
/// a GPU's memory, so a PC whose GPU Ferrix does not drive -- nearly every
/// PC, and the display adapters of `VirtualBox` and `VMware` -- shows the desktop at the
/// firmware's mode instead of the loader's last screen; where firmware's
/// framebuffer is RAM, as virtio-gpu's in QEMU, it is not lent and nothing
/// changes (`docs/DISPLAY.md` §8). `ferrix.fbcon` draws the boot log on that
/// screen until then, which is the only log a person at a PC sees.
///
/// `ferrix.unisolated` lets the live session drive a machine with no IOMMU
/// -- the defaults of `VirtualBox` and `VMware`, a PC with VT-d or AMD-Vi
/// off -- whose drivers are then trusted with all of memory, as on Linux;
/// where an IOMMU isolates a device it changes nothing (the customer,
/// 2026-10-28). The installed system never keeps it: the installer blanks
/// it with `ferrix.live` and keeps the rest. None of these is a
/// configuration a claim is made of.
const LIVE_WORDS: &str =
    "ferrix.root=tmpfs ferrix.live ferrix.unisolated ferrix.efifb ferrix.fbcon";

/// What the kernel says at stage 10 when `ferrix.unisolated` is on.
const UNISOLATED_SAID: &str = "ferrix.unisolated: devmgr may start drivers";

/// The units the installer brings, as `(name, text)`; each is wanted by
/// `graphical.target`.
const UNITS: [(&str, &str); 2] = [
    (
        "ferrix-installd",
        "# The live session's installer daemon (docs/INSTALLER.md); it exits at once\n\
         # unless the command line says ferrix.live.\n\
         [Unit]\n\
         Description=The installer's daemon\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=/sbin/ferrix-install --serve\n\
         StandardOutput=console\n\
         StandardError=console\n",
    ),
    (
        "ferrix-setup",
        "# The installer's answers applied at every boot, before the session\n\
         # (docs/INSTALLER.md); nothing to do on the live session.\n\
         [Unit]\n\
         Description=The installer's answers\n\
         Before=hyprix.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=/sbin/ferrix-install --apply\n\
         StandardOutput=console\n\
         StandardError=console\n",
    ),
];

/// The lock screen an installed system's session starts behind, which is
/// its login: hyprlock in Yoru's Nocturne colours, where hyprlock looks for
/// a system-wide file (`/etc/xdg/hypr`).
const HYPRLOCK_CONF: &str =
    "# The installed system's login (cargo xtask iso): hyprlock in Nocturne.
$font = Inter

general {
    hide_cursor = false
    ignore_empty_input = true
}

auth {
    pam {
        enabled = true
    }
}

background {
    monitor =
    color = rgb(161826)
    blur_passes = 0
}

label {
    monitor =
    text = cmd[update:10000] date +%H:%M
    color = rgb(e9e9ed)
    font_size = 72
    font_family = $font
    position = 0, 220
    halign = center
    valign = center
}

label {
    monitor =
    text = Welcome back, $USER
    color = rgb(e9e9ed)
    font_size = 22
    font_family = $font
    position = 0, 60
    halign = center
    valign = center
}

input-field {
    monitor =
    size = 340, 52
    outline_thickness = 2
    dots_size = 0.22
    dots_spacing = 0.3
    dots_center = true
    outer_color = rgb(9184d9)
    inner_color = rgb(232532)
    font_color = rgb(e9e9ed)
    font_family = $font
    fade_on_empty = false
    placeholder_text = <span foreground=\"##8b8b9a\">Password</span>
    fail_text = <span foreground=\"##ff8a8a\">$PAMFAIL</span>
    check_color = rgb(9184d9)
    fail_color = rgb(e06c75)
    rounding = 8
    position = 0, -40
    halign = center
    valign = center
}
";

/// What an installed system's session starts: hyprlock, again until it
/// has locked and been opened. A lock asked for before `authd` offers the
/// seat's channel is ended by the compositor at once, and at a session's
/// start that is a race the lock must not lose.
const LOGIN_LOCK: &str = "#!/bin/sh
# The installed system's login (cargo xtask iso, ferrix-install --apply):
# the session starts locked, and only the person's password opens it.
while :; do
    /bin/hyprlock 2>&1 | /bin/busybox tee /tmp/login-lock.log
    if /bin/busybox grep -q 'hyprlock: unlocked' /tmp/login-lock.log; then
        exit 0
    fi
    /bin/busybox sleep 1
done
";

/// The setup file's placeholder, as `ferrix-install` expects it.
const SETUP_BYTES: usize = 4096;

/// What `iso` made.
pub(crate) struct Built {
    pub(crate) iso: PathBuf,
    pub(crate) kernel: crate::cargo::Kernel,
}

/// `cargo xtask iso`.
pub(crate) fn iso(args: &Args) -> Result<()> {
    let built = build(args)?;
    println!("  live ISO {}", built.iso.display());
    Ok(())
}

/// Build the ISO.
pub(crate) fn build(args: &Args) -> Result<Built> {
    let arch = args.single_arch()?;
    if arch != Arch::X86_64 {
        return Err(Error::new(
            "iso is x86_64's for now: the live session is the release desktop, which is",
        ));
    }
    if args.config.is_some() || args.everything || args.chrome || args.wine {
        return Err(Error::new(
            "iso builds the release desktop; --config, --everything, --chrome and --wine are not in it",
        ));
    }
    let args = &Args {
        release: true,
        layout: Some("us".to_owned()),
        variant: None,
        edid: Some("none".to_owned()),
        wallpaper: Some(crate::wallpaper::BRAND.to_owned()),
        theme: Some(args.theme.clone().unwrap_or_else(|| "yoru".to_owned())),
        net: true,
        session_user: Some("1000".to_owned()),
        ..args.clone()
    };
    crate::fat::set_strip_kernel(true);
    pinned_shell_trees()?;
    crate::omz::require_installed()?;
    crate::apps::build_carried(arch, args)?;
    let programs = Programs::build(arch)?;
    let config = live_config();
    let (config, mut carried) = desktop(
        arch,
        config,
        crate::wallpaper::SCREEN,
        true,
        Backdrop::Any,
        args,
    )?;
    let config = crate::session::for_session(&config, &mut carried.ports, None);
    carried.ports.extend(installer_files(arch)?);
    let (loader, kernel, initramfs) = build_parts(arch, &programs, &config, carried, args)?;

    let build = paths::build_dir(arch);
    std::fs::create_dir_all(&build)?;
    let cmdline = format!("{} {LIVE_WORDS}\n", crate::init::command_line().trim_end());
    let (loader_bytes, kernel_bytes, carried) =
        crate::fat::live_parts(arch, &loader, &kernel, &initramfs)?;
    refuse_host_programs(&carried)?;
    let licences = notice(&build, &carried)?;
    let readme = README.replace('\n', "\r\n").into_bytes();
    let volume = build.join("ferrix-live.esp");
    crate::fat::write_live_volume(
        arch,
        (&loader_bytes, &kernel_bytes, &carried),
        (&cmdline, Some(super::run::DESKTOP_DEFAULTS)),
        &[
            ("FERRIX/SETUP.TXT", vec![b'\n'; SETUP_BYTES]),
            ("README.TXT", readme.clone()),
            ("LICENSES.MD", licences.clone()),
        ],
        &volume,
    )?;
    let iso = build.join(ISO);
    let layout = crate::iso::write(
        &volume,
        &[
            crate::iso::Extra {
                name: "README.TXT".to_owned(),
                data: readme,
            },
            crate::iso::Extra {
                name: "LICENSE.TXT".to_owned(),
                data: std::fs::read(paths::workspace_root().join("LICENSE"))?,
            },
            crate::iso::Extra {
                name: "THIRD_PARTY_LICENSES.MD".to_owned(),
                data: licences,
            },
        ],
        LABEL,
        &iso,
    )?;
    let _ = std::fs::remove_file(&volume);
    println!(
        "  {} ({} MiB): El Torito's EFI image and the GPT's {} partition at block {}, {} MiB",
        iso.display(),
        layout.bytes >> 20,
        crate::installer::LIVE_NAME,
        layout.esp_block,
        layout.esp_bytes >> 20
    );
    Ok(Built { iso, kernel })
}

/// oh-my-zsh and zsh's functions at their pins, as the release desktop's
/// packer takes them (`tools/common/release/package-desktop.py`): an image
/// that leaves this machine carries the pinned trees, not whatever this
/// machine has, and the licence inventory reads their tarballs. Unless the
/// caller named them (`FERRIX_SHELL_TREES`, as the release workflow does),
/// `tools/common/fetch/fetch-shell-trees.sh` fetches them into
/// the target directory's `live-iso-shell`, checked against the pins.
fn pinned_shell_trees() -> Result<()> {
    if std::env::var_os("FERRIX_SHELL_TREES").is_some() {
        return Ok(());
    }
    // In this tree's target directory: two trees building at once must not
    // unpack into one directory.
    let dir = paths::target_dir().join("live-iso-shell");
    let script = paths::workspace_root().join("tools/common/fetch/fetch-shell-trees.sh");
    let status = std::process::Command::new("bash")
        .arg(&script)
        .arg(&dir)
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|error| Error::new(format!("running {}: {error}", script.display())))?;
    if !status.success() {
        return Err(Error::new(format!(
            "{} could not fetch the pinned shell trees",
            script.display()
        )));
    }
    for (name, value) in [
        ("FERRIX_OMZ", dir.join("oh-my-zsh")),
        ("FERRIX_ZSH_FUNCTIONS", dir.join("zsh-functions")),
        ("FERRIX_SHELL_TREES", dir.clone()),
    ] {
        // SAFETY: set before any thread of this process reads the
        // environment in parallel; xtask's builds run as child processes.
        unsafe { std::env::set_var(name, value) };
    }
    println!("  shell trees: the pinned ones, in {}", dir.display());
    Ok(())
}

/// The licence notice of the initramfs `carried`, as the ISO carries it:
/// licences.py's inventory and the time zones' release, written beside
/// the ISO for the release's asset too.
fn notice(build: &Path, carried: &[u8]) -> Result<Vec<u8>> {
    let initrd = build.join("ferrix-live.initrd");
    std::fs::write(&initrd, carried)
        .map_err(|error| Error::new(format!("writing {}: {error}", initrd.display())))?;
    let mut licences = licences(&initrd, &build.join("live-licences"))?;
    licences.extend_from_slice(
        format!(
            "\n## Time zones\n\n`/usr/share/zoneinfo` is the IANA time zone database, release \
             {}, compiled, as the build machine's tzdata package had it. The database is in the \
             public domain (https://www.iana.org/time-zones).\n",
            tzdata_version()?
        )
        .as_bytes(),
    );
    let path = build.join(NOTICE);
    std::fs::write(&path, &licences)
        .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))?;
    Ok(licences)
}

/// What the volume's and the ISO's `README.TXT` says.
const README: &str = "Ferrix live medium
==================

Ferrix is an operating system with its own Rust microkernel that runs
ordinary Linux programs. This medium starts its desktop, Yoru, from memory:
nothing on the machine's disks is touched unless you install.

This is a development preview. It is not signed for Secure Boot, so turn
Secure Boot off to start it. Write it to a USB stick (dd, Rufus in DD mode,
balenaEtcher) or attach it to a virtual machine as a CD or a disk.

What works, what does not, and how to start it in QEMU:
https://github.com/ferrix-os/ferrix/blob/develop/docs/TRY-FERRIX.md

Ferrix is MIT licensed (LICENSE.TXT). The programs it carries from others,
their licences and the licences' texts are in THIRD_PARTY_LICENSES.MD here
and LICENSES.MD on the EFI system partition.
";

/// The licence inventory of the initramfs at `initrd`, made by
/// `tools/common/release/licences.py`, which refuses an image with a part
/// whose licence it cannot name.
fn licences(initrd: &Path, out: &Path) -> Result<Vec<u8>> {
    let script = paths::workspace_root().join("tools/common/release/licences.py");
    let status = std::process::Command::new("python3")
        .arg(&script)
        .arg("--initrd")
        .arg(initrd)
        .arg("--out")
        .arg(out)
        .args(["--tag", "live", "--title", "Ferrix live ISO, x86-64"])
        .status()
        .map_err(|error| Error::new(format!("running {}: {error}", script.display())))?;
    if !status.success() {
        return Err(Error::new(
            "the live image's licence inventory refused it (licences.py's REFUSED lines above)",
        ));
    }
    let path = out.join("THIRD-PARTY-LICENSES.md");
    std::fs::read(&path).map_err(|error| Error::new(format!("{}: {error}", path.display())))
}

/// The installer, its units and the graphical installer, for the image.
fn installer_files(arch: Arch) -> Result<Vec<File>> {
    let mut files = crate::installer::files(arch)?
        .ok_or_else(|| Error::new("ferrix-install is not built for x86_64"))?;
    files.push(File {
        path: "etc/ferrix/login-lock.sh".to_owned(),
        mode: 0o755,
        content: Content::Bytes(LOGIN_LOCK.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/xdg/hypr/hyprlock.conf".to_owned(),
        mode: 0o644,
        content: Content::Bytes(HYPRLOCK_CONF.as_bytes().to_vec()),
    });
    files.extend(zones()?);
    for (name, text) in UNITS {
        files.push(File {
            path: format!("etc/ferrix/units/{name}.service"),
            mode: 0o644,
            content: Content::Bytes(text.as_bytes().to_vec()),
        });
        files.push(File {
            path: format!("etc/ferrix/units/graphical.target.wants/{name}.service"),
            mode: 0o777,
            content: Content::Link(format!("/etc/ferrix/units/{name}.service")),
        });
    }
    // `FERRIX_ISO_WITHOUT_GUI=1` leaves the window out, for working on the
    // rest while it is not built; a release never sets it.
    let without = std::env::var_os("FERRIX_ISO_WITHOUT_GUI").is_some();
    if !without {
        files.extend(crate::apps::taken(arch, &["installer"])?);
    } else {
        println!(
            "  FERRIX_ISO_WITHOUT_GUI: the graphical installer left out; the ISO has the daemon only"
        );
    }
    Ok(files)
}

/// The time zones the installer offers and `/etc/localtime` names: the
/// IANA database's regions (public domain) from this machine's tzdata,
/// each zone a `TZif` file, links copied as files.
fn zones() -> Result<Vec<File>> {
    const ROOT: &str = "/usr/share/zoneinfo";
    const REGIONS: [&str; 11] = [
        "Africa",
        "America",
        "Antarctica",
        "Asia",
        "Atlantic",
        "Australia",
        "Europe",
        "Indian",
        "Pacific",
        "Etc",
        "UTC",
    ];
    let mut files = Vec::new();
    let mut pending: Vec<PathBuf> = REGIONS
        .iter()
        .map(|region| Path::new(ROOT).join(region))
        .collect();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            let entries = std::fs::read_dir(&path)
                .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
            pending.extend(entries.flatten().map(|entry| entry.path()));
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if !bytes.starts_with(b"TZif") {
            continue;
        }
        let relative = path
            .strip_prefix("/")
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        files.push(File {
            path: relative,
            mode: 0o644,
            content: Content::Bytes(bytes),
        });
    }
    // Which release of the database: the notice says it, so two ISOs built
    // on machines with different tzdata tell it (consultant, ledger 4417).
    let version = tzdata_version()?;
    files.push(File {
        path: "usr/share/zoneinfo/VERSION".to_owned(),
        mode: 0o644,
        content: Content::Bytes(format!("{version}\n").into_bytes()),
    });
    if files.len() < 300 {
        return Err(Error::new(format!(
            "{ROOT} has {} zones; the live image wants the IANA database (install tzdata)",
            files.len()
        )));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    println!("  {} time zones from {ROOT}", files.len());
    Ok(files)
}

/// Words no part of the live ISO may be named for: the programs xtask merges
/// into a developer's `--everything` desktop from volumes on the build host
/// -- Endfield's launcher (Gryphlink), yserver, Steam, the Claude desktop app
/// and Chrome -- none of which the release may carry. `iso` refuses the flags
/// that merge them; this reads the image itself, so a path that merges a
/// host's volume some other way fails the build on a machine that has one.
const NOT_IN_THE_ISO: [&str; 7] = [
    "gryphlink",
    "endfield",
    "yserver",
    "steam",
    "claude-desktop",
    "claude_desktop",
    "chrome",
];

/// Refuse a live image whose archive names a word of [`NOT_IN_THE_ISO`]: in
/// an entry's name, a link's target, a launcher entry (`.desktop`) or the
/// session's configuration, where a merged program would be started.
fn refuse_host_programs(archive: &[u8]) -> Result<()> {
    let entries = crate::initramfs::entries(archive)
        .ok_or_else(|| Error::new("the live image's archive does not read as newc".to_owned()))?;
    let found = host_programs_in(entries.iter().map(|entry| {
        let read =
            entry.link || entry.name.ends_with(".desktop") || entry.name == super::CONFIG_PATH;
        (entry.name, read.then_some(entry.data))
    }));
    if found.is_empty() {
        println!("  live image: none of {} in it", NOT_IN_THE_ISO.join(", "));
        return Ok(());
    }
    Err(Error::new(format!(
        "the live image carries what only a developer's desktop may: {} \
         (a build host's volume merged into it?)",
        found.join(", ")
    )))
}

/// The entries among `entries` -- each a name, and the bytes read for it --
/// that name a word of [`NOT_IN_THE_ISO`], each with the word.
fn host_programs_in<'a>(entries: impl Iterator<Item = (&'a str, Option<&'a [u8]>)>) -> Vec<String> {
    let mut found = Vec::new();
    for (name, data) in entries {
        let text = format!(
            "{name}\n{}",
            data.map(String::from_utf8_lossy).unwrap_or_default()
        )
        .to_ascii_lowercase();
        if let Some(word) = NOT_IN_THE_ISO.iter().find(|word| text.contains(**word)) {
            found.push(format!("{name} ({word})"));
        }
    }
    found
}

/// The IANA release this machine's zones are, from `tzdata.zi`'s
/// `# version` line; a machine without one cannot build the ISO.
fn tzdata_version() -> Result<String> {
    let path = "/usr/share/zoneinfo/tzdata.zi";
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::new(format!(
            "{path}: {error}; the ISO's notice names the tzdata release, so it needs this file"
        ))
    })?;
    text.lines()
        .next()
        .and_then(|line| line.strip_prefix("# version "))
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
        .ok_or_else(|| Error::new(format!("{path} has no `# version` line")))
}

/// What Yoru, the desktop both the live session and the installed system
/// start by default, says when its dock is up (`yoru-shell`).
const YORU_DOCK: &str = "yoru-shell: the dock is up";

/// `cargo xtask test-iso`: see the module documentation.
pub(crate) fn test_iso(args: &Args) -> Result<()> {
    let built = build(args)?;
    let arch = Arch::X86_64;
    for (how, cdrom) in [("a CD", true), ("a USB stick", false)] {
        let boot = Args {
            boot_cdrom: cdrom,
            boot_usb: !cdrom,
            display: true,
            ..args.clone()
        };
        let said = boot_live(arch, &built, &boot, how)?;
        for (wanted, problem) in [
            ("sessiond: ", "sessiond never started the session"),
            (
                "ferrix-installd: listening",
                "the installer's daemon never listened",
            ),
            (
                "ferrix-installer: the window is up",
                "the graphical installer never opened its window",
            ),
            (
                YORU_DOCK,
                "Yoru, the default desktop, never put up its dock",
            ),
            (
                UNISOLATED_SAID,
                "the live session did not carry ferrix.unisolated",
            ),
        ] {
            if !said.iter().any(|line| line.contains(wanted)) {
                return Err(Error::new(format!(
                    "{arch}: booted as {how}, {problem} (no `{wanted}`)"
                )));
            }
        }
        println!("  {arch}: the live desktop came up from the ISO as {how}");
    }
    seat_power(arch, &built, args, Power::Off)?;
    seat_power(arch, &built, args, Power::Restart)?;
    Ok(())
}

/// What the live session's person asks of the machine from a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Power {
    /// `poweroff`.
    Off,
    /// `reboot`.
    Restart,
}

impl Power {
    /// The command typed.
    const fn command(self) -> &'static str {
        match self {
            Power::Off => "poweroff",
            Power::Restart => "reboot",
        }
    }
}

/// What init says when it lets the seat's person turn the machine off
/// (`docs/INIT.md` §10.1).
const SEAT_ALLOWED: &str = "allowed: the seat's";

/// The seat's person turns the machine off or restarts it
/// (`docs/INIT.md` §10.1): in the live session, as `ferrix`, a terminal
/// opened with SUPER RETURN, and `poweroff` or `reboot` typed into it. A
/// power-off must end QEMU by itself through the kernel's power-off; a
/// restart must bring the desktop back. Booted from the ISO as a CD.
///
/// The power-off runs on a QEMU line with no `isa-debug-exit` device, and
/// must end with QEMU exiting by itself with status 0: ACPI's S5, as a PC
/// powers off.
/// Verifies: `L.x86_64.213`, H.BOOT.6
fn seat_power(arch: Arch, built: &Built, args: &Args, power: Power) -> Result<()> {
    let port = free_port()?;
    let boot = Args {
        boot_cdrom: true,
        boot_usb: false,
        display: true,
        qmp_port: Some(port),
        // A restart restarts the machine rather than ending QEMU.
        reset: power == Power::Restart,
        // A power-off must switch the machine off as a PC's does, through
        // ACPI's S5, with no debug-exit device to end QEMU (L.x86_64.213).
        no_debug_exit: power == Power::Off,
        ..args.clone()
    };
    let command = power.command();
    let mut restarted = false;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let up = watching.read_more(Instant::now() + Duration::from_secs(240), |lines| {
            lines.iter().any(|line| line.contains(YORU_DOCK))
                && lines.iter().any(|line| line.contains("sessiond: "))
        })?;
        if !up {
            return Err(Error::new(format!(
                "{arch}: `{command}` as the seat's person: the desktop never came up"
            )));
        }
        std::thread::sleep(Duration::from_secs(5));
        super::boot::press(&mut qmp, &["meta_l", "ret"])?;
        // The terminal and its shell.
        std::thread::sleep(Duration::from_secs(8));
        for letter in command.chars() {
            super::boot::press(&mut qmp, &[&letter.to_string()])?;
            std::thread::sleep(Duration::from_millis(80));
        }
        super::boot::press(&mut qmp, &["ret"])?;
        let deadline = Instant::now() + Duration::from_secs(180);
        match power {
            // Until QEMU exits, which closes the serial port.
            Power::Off => {
                let _ = watching.read_more(deadline, |_| false)?;
            }
            Power::Restart => {
                restarted = watching.read_more(deadline, |lines| {
                    lines.iter().filter(|line| line.contains(YORU_DOCK)).count() >= 2
                })?;
                watching.stop_when_done();
            }
        }
        let said: Vec<&String> = watching.lines().iter().chain(watching.after()).collect();
        if !said.iter().any(|line| line.contains(SEAT_ALLOWED)) {
            return Err(Error::new(format!(
                "{arch}: `{command}` typed as the seat's person: init never said `{SEAT_ALLOWED}`"
            )));
        }
        Ok(())
    };
    match power {
        Power::Off => {
            let _ = crate::qemu::watch_then_powered_off(
                arch,
                &built.iso,
                &built.kernel,
                &boot,
                super::EITHER,
                hook,
                // QEMU's own status for a guest that entered S5.
                0,
            )?;
            println!(
                "  {arch}: the live session's person typed `poweroff`, and the machine powered off"
            );
        }
        Power::Restart => {
            let _ = crate::qemu::watch_then(
                arch,
                &built.iso,
                &built.kernel,
                &boot,
                super::EITHER,
                hook,
            )?;
            if !restarted {
                return Err(Error::new(format!(
                    "{arch}: the live session's person typed `reboot`, and the desktop did not come back"
                )));
            }
            println!(
                "  {arch}: the live session's person typed `reboot`, and the desktop came back"
            );
        }
    }
    Ok(())
}

/// Boot the ISO as `how` says and wait for the desktop: its marker, then a
/// screen that is not one colour. The transcript.
fn boot_live(arch: Arch, built: &Built, args: &Args, how: &str) -> Result<Vec<String>> {
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join(if args.boot_cdrom {
        "live-iso-cd.ppm"
    } else {
        "live-iso-usb.ppm"
    });
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let up = watching.read_more(Instant::now() + Duration::from_secs(240), |lines| {
            lines.iter().any(|line| line.contains(MARKER))
        })?;
        if !up {
            return Err(Error::new(format!(
                "{arch}: booted as {how}, the compositor never printed `{MARKER}`"
            )));
        }
        // The session's clients, the dock and the installer's window.
        let _ = watching.read_more(Instant::now() + Duration::from_secs(60), |lines| {
            lines
                .iter()
                .any(|line| line.contains("ferrix-installer: the window is up"))
                && lines.iter().any(|line| line.contains("sessiond: "))
                && lines.iter().any(|line| line.contains(YORU_DOCK))
        })?;
        std::thread::sleep(Duration::from_secs(15));
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        let colours = distinct_colours(&dump)?;
        if colours < 16 {
            return Err(Error::new(format!(
                "{arch}: booted as {how}, the screen has {colours} colours: no desktop ({})",
                dump.display()
            )));
        }
        println!(
            "  {arch}: the screen as {how} has {colours} colours: {}",
            dump.display()
        );
        let heard: Vec<String> = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        let desk = desktop_size(&heard).ok_or_else(|| {
            Error::new(format!(
                "{arch}: booted as {how}, hyprix never named its monitors"
            ))
        })?;
        first_click(&mut qmp, &dump, desk, arch, how)?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(
        arch,
        &built.iso,
        &built.kernel,
        &qemu_args,
        super::EITHER,
        hook,
    )?;
    Ok(said)
}

/// The installer's "Install Ferrix" button: its fill, the Nocturne accent.
const BUTTON: [u8; 3] = [145, 132, 217];

/// How far a channel may be from [`BUTTON`] and still be the button.
const BUTTON_STEP: u8 = 12;

/// The button's rectangle in `image`, `(left, top, right, bottom)`
/// exclusive: the tallest band of rows, 20 at least, each with a twentieth
/// of the screen's width in the button's fill. That leaves out the Welcome
/// step's marker and the language field's outline, which are the same
/// colour and a few rows tall. `None` when nothing is: the Welcome page is
/// not showing, or something covers its button.
fn button_rect(image: &crate::display::Image) -> Option<(usize, usize, usize, usize)> {
    let mut bands: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut open = false;
    for y in 0..image.height {
        let row: Vec<usize> = (0..image.width)
            .filter(|x| is_button(image, (y * image.width + x) * 3))
            .collect();
        let (Some(&left), Some(&last)) = (row.first(), row.last()) else {
            open = false;
            continue;
        };
        if row.len() < image.width / 20 {
            open = false;
            continue;
        }
        match bands.last_mut() {
            Some(band) if open => *band = (band.0.min(left), band.1, band.2.max(last + 1), y + 1),
            _ => bands.push((left, y, last + 1, y + 1)),
        }
        open = true;
    }
    bands
        .into_iter()
        .filter(|band| band.3 - band.1 >= 20)
        .max_by_key(|band| band.3 - band.1)
}

/// Whether the pixel whose red byte is at `at` is the button's fill.
fn is_button(image: &crate::display::Image, at: usize) -> bool {
    image.pixels.get(at..at + 3).is_some_and(|rgb| {
        rgb.iter()
            .zip(BUTTON)
            .all(|(c, b)| c.abs_diff(b) <= BUTTON_STEP)
    })
}

/// How many pixels inside `rect` are still the button's fill.
fn button_left(image: &crate::display::Image, rect: (usize, usize, usize, usize)) -> usize {
    let (left, top, right, bottom) = rect;
    (top..bottom)
        .flat_map(|y| (left..right).map(move |x| (y * image.width + x) * 3))
        .filter(|&at| is_button(image, at))
        .count()
}

/// How big every screen is together, from hyprix's line naming its
/// monitors (`hyprix: 2 monitors [card0 Virtual-1 1024x768 1024x768, card1
/// Virtual-2 1280x800 1280x800]`): their widths added, as hyprix lays them
/// out left to right, and the tallest's height.
fn desktop_size(lines: &[String]) -> Option<(usize, usize)> {
    let line = lines
        .iter()
        .find(|line| line.contains("hyprix: ") && line.contains(" monitor"))?;
    let listed = line.split_once('[')?.1.split_once(']')?.0;
    let mut size = (0, 0);
    for monitor in listed.split(", ") {
        let (width, height) = monitor.split_whitespace().nth(2)?.split_once('x')?;
        let (width, height): (usize, usize) = (width.parse().ok()?, height.parse().ok()?);
        size = (size.0 + width, size.1.max(height));
    }
    (size.0 > 0).then_some(size)
}

/// The first click a person makes: on "Install Ferrix", where the screen
/// shows it, after which the installer must show its next page.
///
/// A window that opens under another, or a page that does not follow the
/// click, is a live session whose installer cannot be started by its first
/// click, which is the one thing the live session is for (2026-10-28).
fn first_click(
    qmp: &mut Qmp,
    dump: &Path,
    desk: (usize, usize),
    arch: Arch,
    how: &str,
) -> Result<()> {
    let bytes =
        std::fs::read(dump).map_err(|error| Error::new(format!("{}: {error}", dump.display())))?;
    let before = crate::display::parse_ppm(&bytes)?;
    let Some(rect) = button_rect(&before) else {
        return Err(Error::new(format!(
            "{arch}: booted as {how}, no \"Install Ferrix\" button is to be seen: the installer's \
             Welcome page is not on top ({})",
            dump.display()
        )));
    };
    let (left, top, right, bottom) = rect;
    // A tablet's place is a fraction of every screen together, which
    // hyprix lays out left to right from the first; the screendump is the
    // first one's.
    let (across, down) = desk;
    let at =
        |middle: usize, size: usize| i32::try_from(middle * 0x7fff / size.max(1)).unwrap_or(0x3fff);
    let (x, y) = (at((left + right) / 2, across), at((top + bottom) / 2, down));
    let axis = |name: &str, value: i32| {
        format!(
            "{{\"type\":\"abs\",\"data\":{{\"axis\":{},\"value\":{value}}}}}",
            crate::display::json_string(name)
        )
    };
    let button = |down: bool| {
        format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
    };
    // The tablet, not whichever pointer QEMU last saw become active: an
    // absolute event sent to a relative mouse moves nothing.
    qmp.choose_pointer("Tablet")?;
    qmp.input_send_event(&[axis("x", x), axis("y", y)])?;
    std::thread::sleep(Duration::from_millis(300));
    qmp.input_send_event(&[button(true)])?;
    std::thread::sleep(Duration::from_millis(100));
    qmp.input_send_event(&[button(false)])?;
    // Off the window, so that a button still showing is not taken for a
    // page that moved on because the pointer over it changed its shade.
    qmp.input_send_event(&[axis("x", 0x100), axis("y", 0x100)])?;
    let after = dump.with_extension("click.ppm");
    let whole = (right - left) * (bottom - top);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        qmp.screendump(Some(DEVICE_ID), &after)?;
        let bytes = std::fs::read(&after)
            .map_err(|error| Error::new(format!("{}: {error}", after.display())))?;
        let image = crate::display::parse_ppm(&bytes)?;
        let left_over = button_left(&image, rect);
        // The next page has no button there: a quarter of its fill is
        // the most a page that moved on can still show in its place.
        if left_over * 4 < whole {
            println!(
                "  {arch}: booted as {how}, the first click on \"Install Ferrix\" moved the installer on: {}",
                after.display()
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::new(format!(
                "{arch}: booted as {how}, a click on \"Install Ferrix\" at {left}..{right}x{top}..{bottom} \
                 left the Welcome page showing ({} of its {whole} pixels still the button): {}",
                left_over,
                after.display()
            )));
        }
    }
}

/// How many colours a binary PPM screendump has, up to 4096.
fn distinct_colours(path: &Path) -> Result<usize> {
    let bytes =
        std::fs::read(path).map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    // P6, width, height, maxval, each followed by one whitespace byte.
    let mut fields = 0;
    let mut at = 0;
    let space = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_whitespace);
    while fields < 4 && at < bytes.len() {
        while at < bytes.len() && space(at) {
            at += 1;
        }
        while at < bytes.len() && !space(at) {
            at += 1;
        }
        fields += 1;
    }
    let pixels = bytes.get(at + 1..).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for pixel in pixels.chunks_exact(3).step_by(7) {
        let _ = seen.insert(pixel.to_vec());
        if seen.len() >= 4096 {
            break;
        }
    }
    Ok(seen.len())
}

/// The person `test-iso-install` installs for.
const GATE_USER: &str = "kira";
/// Their password: lowercase letters, so QMP types it on any layout's
/// letters the same (the answers say `us`).
const GATE_PASSWORD: &str = "tryferrixnow";
/// The gate's guests' memory, in MiB: what Try Ferrix asks for.
const GATE_MEMORY: u32 = 4096;
/// The blank target's size: the root's smallest and the ESP, and room.
const TARGET_BYTES: u64 = 8 << 30;

/// The answers the gate's install runs with, in the unattended partition.
fn gate_answers(target: &str) -> String {
    format!(
        "ferrix-answers 1\n\
         target={target}\n\
         mode=erase\n\
         user={GATE_USER}\n\
         fullname=Kira Example\n\
         password={GATE_PASSWORD}\n\
         hostname=kiras-pc\n\
         keyboard=us\n\
         variant=\n\
         timezone=Europe/Berlin\n\
         language=en\n\
         then=poweroff\n\n"
    )
}

/// A small disk whose one GPT partition, `FERRIX-ANSWERS`, holds `text`.
fn answers_disk(path: &Path, text: &str) -> Result<()> {
    use ferrix_partition::{Guid, LINUX_FILESYSTEM, Partition, write_table};
    let sectors = (8_u64 << 20) / 512;
    let partition = Partition::new(
        LINUX_FILESYSTEM,
        Guid([0x42; 16]),
        2048,
        sectors - 2048,
        "FERRIX-ANSWERS",
    );
    let table = write_table(sectors, 512, Guid([0x41; 16]), &[partition])
        .map_err(|error| Error::new(format!("the answers disk's table: {error}")))?;
    let mut bytes = vec![0_u8; (sectors * 512) as usize];
    for (lba, sector) in table {
        crate::iso::set(&mut bytes, (lba * 512) as usize, &sector);
    }
    crate::iso::set(&mut bytes, 2048 * 512, text.as_bytes());
    std::fs::write(path, bytes)
        .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))
}

/// What the installed disk's boot must say, each with what its absence means.
const INSTALLED_SAYS: [(&str, &str); 6] = [
    (
        "/ is btrfs on vdd2; the system was installed on it",
        "the installed disk did not put / on its root partition",
    ),
    (
        "ferrix-setup: this system is kira's, named kiras-pc",
        "the first boot did not apply the answers",
    ),
    (
        "sessiond: seat0's session for kira (uid 1000)",
        "the session did not start as the person's account",
    ),
    ("hyprlock: locked", "the session did not start locked"),
    (
        YORU_DOCK,
        "the installed system's session is not Yoru (no dock)",
    ),
    (
        "hyprlock: authenticated",
        "the person's password did not open it",
    ),
];

/// `cargo xtask test-iso-install`: the live ISO, attached as a disk Ferrix
/// drives, installs onto a blank disk with the unattended answers of
/// [`gate_answers`] -- the graphical installer's own daemon and engine,
/// asked by a partition instead of by the window -- and powers off; then
/// the installed disk boots alone, its first boot applies the answers, the
/// session starts as the person's account, locked, and their password
/// opens it.
pub(crate) fn test_iso_install(args: &Args) -> Result<()> {
    let built = build(args)?;
    let arch = Arch::X86_64;
    let dir = paths::build_dir(arch);
    let target = dir.join("iso-install-target.img");
    let _ = std::fs::remove_file(&target);
    std::fs::File::create(&target)
        .and_then(|file| file.set_len(TARGET_BYTES))
        .map_err(|error| Error::new(format!("{}: {error}", target.display())))?;
    let answers = dir.join("iso-install-answers.img");
    // The medium is vdd (booted from, after the three test disks), the
    // target vde and the answers vdf.
    answers_disk(&answers, &gate_answers("/dev/vde"))?;

    // A machine a person installs on has memory to spare: authd rehashes a
    // seeded password at 256 MiB at its first use, beside the desktop.
    install_unattended(arch, &built, &target, answers, args)?;
    println!(
        "  {arch}: installed from the live ISO onto {}",
        target.display()
    );

    // The installed disk alone: answers applied, the session the person's,
    // locked until their password.
    let port = free_port()?;
    let installed = Args {
        memory: args.memory.max(GATE_MEMORY),
        boot_virtio: true,
        display: true,
        qmp_port: Some(port),
        ..args.clone()
    };
    let dump = dir.join("iso-installed-locked.ppm");
    let unlocked = dir.join("iso-installed-desktop.ppm");
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let locked = watching.read_more(Instant::now() + Duration::from_secs(300), |lines| {
            lines.iter().any(|line| line.contains("hyprlock: locked"))
        })?;
        if !locked {
            return Err(Error::new(format!(
                "{arch}: the installed system's session never locked (no `hyprlock: locked`)"
            )));
        }
        std::thread::sleep(Duration::from_secs(5));
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        for letter in GATE_PASSWORD.chars() {
            super::press(&mut qmp, &[&letter.to_string()])?;
            std::thread::sleep(Duration::from_millis(80));
        }
        super::press(&mut qmp, &["ret"])?;
        let _ = watching.read_more(Instant::now() + Duration::from_secs(90), |lines| {
            lines.iter().any(|line| line.contains("hyprlock: unlocked"))
        })?;
        // The desktop behind the lock, for whoever reads the run: Yoru's.
        std::thread::sleep(Duration::from_secs(5));
        qmp.screendump(Some(DEVICE_ID), &unlocked)?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(
        arch,
        &target,
        &built.kernel,
        &installed,
        super::EITHER,
        hook,
    )?;
    for (wanted, problem) in INSTALLED_SAYS {
        if !said.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!(
                "{arch}: {problem} (no `{wanted}`); the serial log has the rest"
            )));
        }
    }
    installed_isolates(arch, &said)?;
    println!(
        "  {arch}: the installed disk booted alone to {GATE_USER}'s locked session, and \
         {GATE_USER}'s password opened it to Yoru ({}, {})",
        dump.display(),
        unlocked.display()
    );
    Ok(())
}

/// The installed system isolates its drivers: the live session's
/// `ferrix.unisolated` is not on its command line, nor said by its kernel,
/// and the command line is the live one made installed.
fn installed_isolates(arch: Arch, said: &[String]) -> Result<()> {
    let installed_line = said
        .iter()
        .find(|line| line.contains("cmdline ") && line.contains("(from /FERRIX/CMDLINE.TXT)"))
        .ok_or_else(|| {
            Error::new(format!(
                "{arch}: the installed boot's loader named no command line"
            ))
        })?;
    if !installed_line.contains("ferrix.root=btrfs")
        || !installed_line.contains("ferrix.efifb")
        || installed_line
            .split_whitespace()
            .any(|word| word == "ferrix.live")
    {
        return Err(Error::new(format!(
            "{arch}: the installed command line is not the live one made installed: {installed_line}"
        )));
    }
    if let Some(line) = said.iter().find(|line| line.contains("ferrix.unisolated")) {
        return Err(Error::new(format!(
            "{arch}: the installed system kept a live-only word: {line}"
        )));
    }
    Ok(())
}

/// The ISO booted as a virtio disk beside `target` and the answers disk:
/// the unattended install must find the answers, fill in the setup file and
/// power off.
fn install_unattended(
    arch: Arch,
    built: &Built,
    target: &Path,
    answers: PathBuf,
    args: &Args,
) -> Result<()> {
    let install = Args {
        memory: args.memory.max(GATE_MEMORY),
        boot_virtio: true,
        display: true,
        extra_disks: vec![target.to_path_buf(), answers],
        ..args.clone()
    };
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let _ = watching.read_more(Instant::now() + Duration::from_secs(900), |lines| {
            lines.iter().any(|line| {
                line.contains("ferrix-installd: powering off")
                    || line.contains("the install failed")
            })
        })?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &built.iso, &built.kernel, &install, "", hook)?;
    for (wanted, problem) in [
        (
            "unattended answers on /dev/vdf",
            "the daemon never found the answers",
        ),
        (
            "ferrix-install: the answers are in FERRIX/SETUP.TXT",
            "the answers never reached the ESP",
        ),
        (
            "ferrix-installd: powering off",
            "the install did not finish",
        ),
    ] {
        if !said.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!(
                "{arch}: {problem} (no `{wanted}`); the serial log has the rest"
            )));
        }
    }
    println!(
        "  {arch}: installed from the live ISO onto {}",
        target.display()
    );

    Ok(())
}

#[cfg(test)]
mod live_config_tests {
    use super::{BOOT_TERMINAL, LIVE_LINES, NOT_IN_THE_ISO, host_programs_in, live_config};

    /// The release's own entries pass; each host program is found by its
    /// name, by a link's target, or by a launcher entry's or the session's
    /// text, whatever its case; a program's bytes are not read.
    #[test]
    fn host_programs_are_found_in_names_links_and_launchers() {
        let clean: [(&str, Option<&[u8]>); 4] = [
            ("bin/yoru-shell", None),
            (
                "usr/share/applications/shelf.desktop",
                Some(b"Exec=/bin/shelf\n"),
            ),
            (
                "etc/hyprland.conf",
                Some(b"exec-once = /bin/ferrix-installer --autostart\n"),
            ),
            ("bin/busybox", None),
        ];
        assert!(host_programs_in(clean.into_iter()).is_empty());
        for word in NOT_IN_THE_ISO {
            let name = format!("bin/{word}");
            assert_eq!(
                host_programs_in([(name.as_str(), None)].into_iter()).len(),
                1,
                "{word}"
            );
        }
        let link: [(&str, Option<&[u8]>); 1] = [("bin/x", Some(b"/data/Steam/steam.sh"))];
        assert_eq!(host_programs_in(link.into_iter()), ["bin/x (steam)"]);
        let launcher: [(&str, Option<&[u8]>); 1] = [(
            "usr/share/applications/game.desktop",
            Some(b"Exec=/bin/GryphLink\n"),
        )];
        assert_eq!(
            host_programs_in(launcher.into_iter()),
            ["usr/share/applications/game.desktop (gryphlink)"]
        );
        let config: [(&str, Option<&[u8]>); 1] = [(
            "etc/hyprland.conf",
            Some(b"exec-once = /bin/claude-desktop\n"),
        )];
        assert_eq!(host_programs_in(config.into_iter()).len(), 1);
    }

    /// The installer is the only window the live session opens at login:
    /// no other `exec-once` starts a program with a window beside it.
    #[test]
    fn the_live_session_opens_the_installer_and_no_terminal() {
        let config = live_config();
        assert!(config.ends_with(LIVE_LINES));
        assert!(config.contains("exec-once = /bin/ferrix-installer --autostart\n"));
        assert!(
            !config.lines().any(|line| line.trim() == BOOT_TERMINAL),
            "the live session starts a terminal beside the installer"
        );
        let windows: Vec<&str> = config
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("exec-once") && !line.contains("/bin/vdagent"))
            .collect();
        assert_eq!(windows, ["exec-once = /bin/ferrix-installer --autostart"]);
        // A terminal is still a key away.
        assert!(config.contains("bind = SUPER, RETURN, exec, /bin/term /bin/zinc"));
    }

    /// The button is found as the tallest band of its fill, not with the
    /// language field's outline above it, and is gone when covered.
    #[test]
    fn the_install_button_is_the_tallest_band_of_its_colour() {
        let (width, height) = (400, 200);
        let mut pixels = vec![20u8; width * height * 3];
        let mut paint = |left: usize, top: usize, right: usize, bottom: usize| {
            for y in top..bottom {
                for x in left..right {
                    let at = (y * width + x) * 3;
                    pixels[at..at + 3].copy_from_slice(&super::BUTTON);
                }
            }
        };
        // The field's outline: two rows, as wide as the page.
        paint(40, 30, 360, 31);
        paint(40, 60, 360, 61);
        // The Welcome step's marker: too narrow to count.
        paint(300, 5, 310, 9);
        // The button.
        paint(200, 100, 280, 126);
        let image = crate::display::Image {
            width,
            height,
            pixels,
        };
        let rect = super::button_rect(&image);
        assert_eq!(rect, Some((200, 100, 280, 126)));
        assert_eq!(super::button_left(&image, (200, 100, 280, 126)), 80 * 26);
        let covered = crate::display::Image {
            pixels: vec![20u8; width * height * 3],
            ..image
        };
        assert_eq!(super::button_rect(&covered), None);
    }

    /// Every screen together, from hyprix's line naming them.
    #[test]
    fn the_desktop_is_every_screen_side_by_side() {
        let lines = [
            "    2.43 | hyprix: 2 monitors [card0 Virtual-1 1024x768 1024x768, card1 Virtual-2 1280x800 1280x800]".to_owned(),
        ];
        assert_eq!(super::desktop_size(&lines), Some((2304, 800)));
        let one = ["hyprix: 1 monitor [card0 Virtual-1 1920x1080 1920x1080]".to_owned()];
        assert_eq!(super::desktop_size(&one), Some((1920, 1080)));
        assert_eq!(super::desktop_size(&[]), None);
    }

    /// The desktop's own configuration still opens one, and the line the
    /// live session leaves out is that one, word for word.
    #[test]
    fn the_desktop_still_opens_its_terminal() {
        assert!(
            super::super::run::RUN_CONFIG
                .lines()
                .any(|line| line.trim() == BOOT_TERMINAL)
        );
    }
}
