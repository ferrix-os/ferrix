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
//! installer's daemon listening, the graphical installer's window, and a
//! screen that is not one colour.

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

/// The live session's words after the image's own command line.
const LIVE_WORDS: &str = "ferrix.root=tmpfs ferrix.live";

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
    let config = format!("{}{LIVE_LINES}", super::run::RUN_CONFIG);
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
        ] {
            if !said.iter().any(|line| line.contains(wanted)) {
                return Err(Error::new(format!(
                    "{arch}: booted as {how}, {problem} (no `{wanted}`)"
                )));
            }
        }
        println!("  {arch}: the live desktop came up from the ISO as {how}");
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
    for (wanted, problem) in [
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
            "hyprlock: authenticated",
            "the person's password did not open it",
        ),
    ] {
        if !said.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!(
                "{arch}: {problem} (no `{wanted}`); the serial log has the rest"
            )));
        }
    }
    println!(
        "  {arch}: the installed disk booted alone to {GATE_USER}'s locked session, and \
         {GATE_USER}'s password opened it ({})",
        dump.display()
    );
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
