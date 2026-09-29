//! `run-steam` and `test-steam-window`: Valve's Steam client in a window of
//! hyprix, drawn through yserver, on Ferrix (`docs/STEAM.md`).
//!
//! The image boots hyprix, whose `exec-once` runs `scripts/steam/run.sh`:
//! yserver on `:0` as a Wayland client of hyprix, a lease for `eth0`, then
//! `scripts/steam/client.sh` as uid 1000, which starts the 32-bit
//! `ubuntu12_32/steam` from the volume. From the bootstrap, that downloads
//! and installs the client (about 500 MB), restarts, and the client opens
//! its sign-in window, drawn by `steamwebhelper` (Chromium).
//!
//! # The volume
//!
//! `scripts/fetch/fetch-steam-window.sh` makes it: yserver's tree,
//! `fetch-steam.sh`'s bootstrap tree, i386 Mesa, lsof, and the launch-side
//! workarounds of `scripts/steam/workarounds/` compiled. It is attached
//! under QEMU's `snapshot=on`, as the other Steam volumes are, so every boot
//! starts from the bootstrap and downloads the client again.
//!
//! # What the image carries
//!
//! Beyond the Steam gates' links and `uname` (`crate::steam`) and yserver's
//! links, the guest scripts and the stand-ins `client.sh` names, all from
//! `scripts/steam/`: the steamrt64 entry point that runs `steamwebhelper`
//! without pressure-vessel, a logger that logs nothing, and `lsof` at
//! `/usr/bin`, the one place of four the client looks for it that the
//! initramfs can hold. `docs/STEAM.md` has the table of these and of the
//! kernel fixes that retire them.
//!
//! # The two commands
//!
//! `run-steam` dumps the screen every few seconds into `build/x86_64/steam/`,
//! keeping each dump that differs from the last, until the script ends or
//! the timeout: for watching, and for finding what went wrong.
//! `test-steam-window` waits for `run.sh`'s watcher to say hyprix lists the
//! window titled "Sign in to Steam", dumps the screen, and passes when the
//! dump is a drawn window rather than an empty screen. Both need KVM (for
//! `cpu MHz`, as `test-steamcmd`) and the internet, so neither is in
//! `cargo xtask check`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{Carried, EITHER, Programs, build_image, gates_busybox, undithered};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The script hyprix's `exec-once` runs, as root.
const RUN: &[u8] = include_bytes!("../../../scripts/steam/run.sh");

/// The client's half, which `RUN` runs as uid 1000.
const CLIENT: &[u8] = include_bytes!("../../../scripts/steam/client.sh");

/// The steamrt64 entry point's stand-in.
const ENTRY_POINT: &[u8] = include_bytes!("../../../scripts/steam/_v2-entry-point");

/// The Steam Runtime logger's stand-in.
const LOGGER: &[u8] = include_bytes!("../../../scripts/steam/logger-0.bash");

/// `lsof` where the client looks for it, running the volume's.
const LSOF: &[u8] = include_bytes!("../../../scripts/steam/lsof");

/// The `--everything` desktop's script, in `RUN`'s place.
const DESKTOP: &[u8] = include_bytes!("../../../scripts/steam/desktop.sh");

/// Where [`DESKTOP`] is in the image.
const DESKTOP_PATH: &str = "steam/desktop.sh";

/// Where `crate::steam::UNAME` is: a directory `client.sh` alone puts first
/// in `PATH`, so the desktop's own `uname` still says Ferrix.
const UNAME_PATH: &str = "steam/bin/uname";

/// Where each is carried, with its mode. `client.sh` names the stand-ins'
/// directories in `STEAM_RUNTIME_STEAMRT` and `STEAM_RUNTIME_SCOUT`; all but
/// `run.sh` go on the `--everything` desktop too.
const SCRIPTS: &[(&str, &[u8], u32)] = &[
    ("steam/run.sh", RUN, 0o644),
    ("steam/client.sh", CLIENT, 0o644),
    ("steam/steamrt/_v2-entry-point", ENTRY_POINT, 0o755),
    (
        "steam/scout/usr/libexec/steam-runtime-tools-0/logger-0.bash",
        LOGGER,
        0o644,
    ),
    ("usr/bin/lsof", LSOF, 0o755),
];

/// What `run.sh`'s watcher prints once hyprix lists the sign-in window.
const LOGIN: &str = "steam-window: login window";

/// What `run.sh` prints last.
const END: &str = "steam-window: end";

/// How long the window gets to be painted after hyprix lists it: the
/// title comes with the toplevel, before Chromium's first frame.
const PAINT: Duration = Duration::from_secs(20);

/// Fewer colours than this is not a drawn sign-in window: the window has
/// Steam's gradients, antialiased text and a QR code, in hundreds, where
/// an unpainted toplevel over the wallpaper-less background has a handful.
const DRAWN_COLOURS: usize = 64;

/// Memory for the guest. At 8 GiB, processes died of `SIGBUS` on library
/// pages while Chromium started (steam-sigbus); 16 GiB has not shown it.
pub(crate) const MEMORY: u32 = 16384;

/// Seconds: the download and install of the client (about nine minutes
/// under KVM), a restart, and Chromium's start in software.
const TIMEOUT: u64 = 2400;

/// Where `scripts/fetch/fetch-steam-window.sh` writes, unless
/// `FERRIX_STEAM_WINDOW_VOLUME` names another directory.
pub(crate) fn volume() -> Result<PathBuf> {
    let directory = match std::env::var_os("FERRIX_STEAM_WINDOW_VOLUME") {
        Some(directory) => PathBuf::from(directory),
        None => paths::volume_directory("steam-window")?,
    };
    let image = directory.join("steam-window.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: scripts/fetch/fetch-steam-window.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// The files the image carries beside the compositor's.
fn files() -> Vec<File> {
    let mut links: Vec<(&str, &str)> = crate::yserver::LINKS.to_vec();
    for link in crate::steam::LINKS {
        if !links.iter().any(|(path, _)| path == &link.0) {
            links.push(*link);
        }
    }
    let mut files = crate::rustc::files(&links);
    files.extend(crate::chrome::window_files());
    files.extend(scripts(SCRIPTS));
    files
}

/// `crate::steam::UNAME` and each of `scripts`, as files of the archive.
fn scripts(scripts: &[(&str, &[u8], u32)]) -> Vec<File> {
    let mut files = vec![File {
        path: UNAME_PATH.to_owned(),
        mode: 0o755,
        content: Content::Bytes(crate::steam::UNAME.as_bytes().to_vec()),
    }];
    for (path, bytes, mode) in scripts {
        files.push(File {
            path: (*path).to_owned(),
            mode: *mode,
            content: Content::Bytes(bytes.to_vec()),
        });
    }
    files
}

/// What `run-compositor --everything` adds to the archive for Steam, when
/// the volume `scripts/fetch/fetch-steam-window.sh` makes is merged into
/// its own: [`SCRIPTS`] but `run.sh`, [`DESKTOP`], and Steam's links less
/// any path `carried` already has -- Chrome's, the compiler's and yserver's
/// name the same Debian's paths.
pub(crate) fn desktop_files(carried: &[File]) -> Vec<File> {
    let taken = |path: &str| {
        carried.iter().any(|file| {
            file.path == path
                || file
                    .path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
                || path
                    .strip_prefix(&file.path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let links: Vec<(&str, &str)> = crate::steam::LINKS
        .iter()
        .copied()
        .filter(|(path, _)| !taken(path))
        .collect();
    let mut files = crate::rustc::files(&links);
    let mut desktop: Vec<(&str, &[u8], u32)> = SCRIPTS
        .iter()
        .copied()
        .filter(|(path, _, _)| *path != "steam/run.sh" && !taken(path))
        .collect();
    desktop.push((DESKTOP_PATH, DESKTOP, 0o644));
    files.extend(scripts(&desktop));
    files
}

/// What `run-compositor --everything` adds to the desktop's configuration
/// for Steam: [`DESKTOP`] from `exec-once`, which waits for the desktop's
/// yserver and starts the client.
pub(crate) fn desktop_config() -> String {
    format!(
        "# Added by `cargo xtask run-compositor --everything`: Valve's Steam client \
         (docs/STEAM.md).\nexec-once = /bin/busybox sh /{DESKTOP_PATH}\n"
    )
}

/// `run-steam`, or `test-steam-window` when `gate`.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails,
/// or, for the gate, the window did not come.
pub(crate) fn run(args: &Args, gate: bool) -> Result<()> {
    let arch = Arch::X86_64;
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    args.net = true;
    if args.accel.is_none() {
        args.accel = Some("kvm".to_owned());
    }
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("run-steam needs ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static")
    })?;
    let programs = Programs::build(arch)?;
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports: files(),
        ..Carried::none()
    };
    let config = format!(
        "# Carried into the initramfs by `cargo xtask run-steam`.\n{}exec-once = /bin/busybox sh /steam/run.sh\n",
        crate::chrome::WINDOW_ENV,
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let shots = paths::build_dir(arch).join("steam");
    let _ = std::fs::remove_dir_all(&shots);
    std::fs::create_dir_all(&shots)
        .map_err(|error| Error::new(format!("making {}: {error}", shots.display())))?;
    let every = std::env::var("FERRIX_STEAM_SHOT_EVERY")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or(3u64);
    let timeout = args.timeout;
    println!(
        "  {arch}: Steam on hyprix through yserver, {} MiB, timeout {timeout}s, on the network; \
         screens in {}",
        args.memory,
        shots.display()
    );
    let mut verdict: Option<Result<()>> = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        verdict = Some(watch(watching, &mut qmp, &shots, every, timeout, gate));
        watching.stop_when_done();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    verdict.unwrap_or_else(|| Err(Error::new(format!("{arch}: hyprix never started"))))
}

/// Dump the screen every `every` seconds, keeping each that differs, until
/// `run.sh` ends or `timeout`; as the gate, until the login window instead,
/// and judge it.
fn watch(
    watching: &mut Watching<'_>,
    qmp: &mut Qmp,
    shots: &std::path::Path,
    every: u64,
    timeout: u64,
    gate: bool,
) -> Result<()> {
    let latest = shots.join("latest.ppm");
    let began = Instant::now();
    let mut previous: Vec<u8> = Vec::new();
    let mut taken = 0u32;
    let mut announced = false;
    loop {
        let ended = watching.read_more(Instant::now() + Duration::from_secs(every), |lines| {
            lines.iter().any(|line| line.contains(END))
        })?;
        let login = watching.after().iter().any(|line| line.contains(LOGIN));
        if login && !announced {
            announced = true;
            println!(
                "  steam-window: hyprix lists Steam's sign-in window after {}s",
                began.elapsed().as_secs()
            );
            if gate {
                return judge(watching, qmp, shots);
            }
        }
        qmp.screendump(Some(DEVICE_ID), &latest)?;
        let bytes = std::fs::read(&latest)
            .map_err(|error| Error::new(format!("reading {}: {error}", latest.display())))?;
        if bytes != previous {
            taken += 1;
            let secs = began.elapsed().as_secs();
            let keep = shots.join(format!("shot-{taken:04}-{secs:05}s.ppm"));
            std::fs::write(&keep, &bytes)
                .map_err(|error| Error::new(format!("writing {}: {error}", keep.display())))?;
            println!("  steam-window: the screen changed, {}", keep.display());
            previous = bytes;
        }
        if ended || began.elapsed().as_secs() >= timeout {
            return if gate {
                Err(Error::new(format!(
                    "Steam's sign-in window did not come ({}); the `steam-window:` lines say \
                     how far the client got, and {} has the screens",
                    if ended { "run.sh ended" } else { "timed out" },
                    shots.display()
                )))
            } else {
                Ok(())
            };
        }
    }
}

/// Give the listed window [`PAINT`] to be drawn, dump the screen to
/// `login-window.ppm`, and pass when it has [`DRAWN_COLOURS`].
fn judge(watching: &mut Watching<'_>, qmp: &mut Qmp, shots: &std::path::Path) -> Result<()> {
    watching.read_what_was_said(PAINT)?;
    let dump = shots.join("login-window.ppm");
    qmp.screendump(Some(DEVICE_ID), &dump)?;
    let bytes = std::fs::read(&dump)
        .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
    let screen = parse_ppm(&bytes)?;
    let (pixels, _) = screen.pixels.as_chunks::<3>();
    let colours = pixels
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if colours >= DRAWN_COLOURS {
        println!(
            "  steam-window: Steam's sign-in window is on the screen ({colours} colours), {}",
            dump.display()
        );
        Ok(())
    } else {
        Err(Error::new(format!(
            "hyprix lists Steam's sign-in window, but the screen has {colours} colours, fewer \
             than the {DRAWN_COLOURS} of a drawn one: {}",
            dump.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every script is carried, the lsof wrapper where the client looks,
    /// and each link has one place to point.
    #[test]
    fn the_image_carries_the_scripts_and_lsof() {
        let files = files();
        for (path, _, _) in SCRIPTS {
            assert!(files.iter().any(|file| file.path == *path), "{path}");
        }
        let lsof = files
            .iter()
            .find(|file| file.path == "usr/bin/lsof")
            .expect("lsof");
        assert_eq!(lsof.mode, 0o755);
        let mut paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        paths.sort_unstable();
        let before = paths.len();
        paths.dedup();
        assert_eq!(before, paths.len(), "a path carried twice");
        let run = std::str::from_utf8(RUN).expect("run.sh is text");
        assert!(run.contains(&format!("echo \"{LOGIN}\"")));
        assert!(run.contains(&format!("echo \"{END}\"")));
        assert!(run.contains("/steam/client.sh"));
        let client = std::str::from_utf8(CLIENT).expect("client.sh is text");
        assert!(client.contains("STEAM_RUNTIME_STEAMRT=/steam/steamrt"));
        assert!(client.contains("STEAM_RUNTIME_SCOUT=/steam/scout"));
        assert!(
            client.contains("export PATH=/steam/bin:"),
            "Linux's uname first"
        );
    }

    /// The `--everything` desktop gets the client's half and the stand-ins
    /// without `run.sh`, whose yserver the desktop already starts, and no
    /// path a volume's link already has.
    #[test]
    fn the_desktop_carries_the_client_without_run_sh() {
        let carried = crate::rustc::files(&[("usr/bin", "/data/usr/bin")]);
        let files = desktop_files(&carried);
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert!(paths.contains(&DESKTOP_PATH));
        assert!(paths.contains(&"steam/client.sh"));
        assert!(paths.contains(&UNAME_PATH));
        assert!(!paths.contains(&"steam/run.sh"));
        assert!(!paths.contains(&"usr/bin/lsof"), "under a link: {paths:?}");
        assert!(desktop_config().contains(&format!("exec-once = /bin/busybox sh /{DESKTOP_PATH}")));
        let desktop = std::str::from_utf8(DESKTOP).expect("desktop.sh is text");
        assert!(desktop.contains("/steam/client.sh"));
        assert!(desktop.contains("/tmp/.X11-unix/X0"));
    }
}
