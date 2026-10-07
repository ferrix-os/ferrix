//! `test-chrome`: Google's headless Chrome on Ferrix, rendering a page.
//!
//! The browser is Chrome for Testing's prebuilt `chrome-headless-shell`, not
//! one built here: a 198 MB position-independent glibc program that loads
//! forty of the system's libraries, run by Debian's `ld-linux` on Debian's
//! glibc, all of it on a btrfs volume `tools/common/fetch/fetch-chrome.sh` makes from
//! pinned downloads. `docs/CHROME.md` says why this is the first Chrome and
//! what ferrousli standing in for that glibc would add.
//!
//! Three steps, each a proof of more than the one before: `--version`, which
//! says the program loaded and its libraries resolved; `--dump-dom` of a
//! page whose script computes a number, which says Chrome's processes
//! started, talked over Mojo, and V8 ran; and `--screenshot`, which says
//! Blink laid out and Skia drew a picture into a file.
//!
//! Chrome runs as Chrome, by `execve`, not as an argument to the loader: it
//! finds its ICU data and its resource packs beside `/proc/self/exe`, and it
//! starts its own renderer and GPU processes by running that again.
//!
//! # `--no-sandbox`, and the zygote
//!
//! `--no-sandbox` because the sandbox is namespaces and seccomp, which Ferrix
//! has neither of: `clone` refuses `CLONE_NEW*` rather than pretend
//! (`docs/CHROME.md` §2.4). The zygote, the process Chrome forks its
//! renderers from instead of starting each afresh, runs as it does on Linux
//! since 2026-09-26. Until then it reported that it could not fork, and the
//! tests ran with `--no-zygote`: it learns each child's pid from the
//! credentials the child's first message carries (`SCM_CREDENTIALS`, on a
//! socket whose reader set `SO_PASSCRED`), and Ferrix passed none.
//!
//! # Where Chrome lives
//!
//! The volume carries no `ferrix-root` label, so the kernel mounts it at
//! `/data`, as test-rustc's is, attached under QEMU's `snapshot=on` so a run
//! never changes it. glibc names its paths absolutely, so the initramfs
//! carries a symbolic link for each into `/data` ([`LINKS`]).
//!
//! # On ferrousli
//!
//! With `--interpreter ferrousli --library ferrousli`, as `test-shell` takes
//! them, the same program runs on ferrousli's loader and `libc.so.6` in
//! glibc's place: the loader goes where Chrome's `PT_INTERP` names, instead
//! of the volume's link to Debian's, and `libc.so.6` into `/lib`, which
//! `LD_LIBRARY_PATH` puts before the volume's libraries. glibc's other names,
//! `libm.so.6` and the rest, the loader answers with ferrousli whatever the
//! volume holds. The forty other libraries are Debian's as before.
//! `test-chrome-window` takes the same two flags, for the full browser and
//! the eighty objects it loads.
//!
//! # On AArch64: Debian's Chromium
//!
//! Chrome for Testing publishes linux64 only, so on AArch64 the browser is
//! Debian 13's Chromium 154.0.8037.57 for arm64 -- the same version -- on a
//! volume `tools/common/fetch/fetch-chromium-arm64.sh` makes the same way, with
//! Debian's arm64 glibc and loader. Its headless mode is the full browser's
//! `--headless`, since Debian builds no `chrome-headless-shell`, and the
//! three steps are the same. The Pixel 7's VM is the machine it is for.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// The page whose DOM Chrome is asked for: its script writes a number only
/// V8 could have computed into an element, so the DOM carries it as text the
/// script's own source does not.
const PAGE: &str = "data:text/html,<p id=v8></p><script>document.getElementById(\"v8\").textContent=\"computed \"+6*7</script>";

/// What that DOM holds once the script ran.
const COMPUTED: &str = "computed 42";

/// The page Chrome takes a picture of.
const PICTURE: &str =
    "data:text/html,<body style=background:%23fc0><h1>Hello from Chrome on Ferrix</h1>";

/// The script the shell runs. The version first, so a failure says whether
/// Chrome loaded at all or only a later step failed.
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
cd /tmp
chrome=/data/chrome/chrome-headless-shell
$chrome --version || exit 3
$chrome --no-sandbox --dump-dom 'PAGE' || exit 4
$chrome --no-sandbox --screenshot=/tmp/shot.png --window-size=640,360 'PICTURE' || exit 5
[ -s /tmp/shot.png ] || exit 6
echo chrome-gate: screenshot written
exit 16
"#;

/// What `--version` prints, which says Chrome and its libraries loaded.
const VERSION: &str = "Google Chrome for Testing 154.0.8037.57";

/// What the script says once the screenshot is on the disk.
const SHOT: &str = "chrome-gate: screenshot written";

/// The status the script exits with when every step succeeded.
const STATUS: i32 = 16;

/// Each path glibc and fontconfig name absolutely, and where on the volume
/// it is.
pub(crate) const LINKS: &[(&str, &str)] = &[
    ("lib64", "/data/usr/lib64"),
    ("lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("etc/fonts", "/data/etc/fonts"),
    ("usr/share/fonts", "/data/usr/share/fonts"),
    ("usr/share/fontconfig", "/data/usr/share/fontconfig"),
    // alsa-lib's configuration, which it reads from this path: Chrome's
    // sound goes through alsa-lib to `/dev/snd` (docs/AUDIO.md §3.5).
    ("usr/share/alsa", "/data/usr/share/alsa"),
];

/// [`LINKS`] for AArch64's Chromium volume: its loader where the program's
/// `PT_INTERP` names it, and glibc's directory, the fonts and alsa-lib's
/// configuration where they are looked for.
pub(crate) const ARM64_LINKS: &[(&str, &str)] = &[
    (
        "lib/ld-linux-aarch64.so.1",
        "/data/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
    ),
    ("lib/aarch64-linux-gnu", "/data/usr/lib/aarch64-linux-gnu"),
    (
        "usr/lib/aarch64-linux-gnu",
        "/data/usr/lib/aarch64-linux-gnu",
    ),
    ("etc/fonts", "/data/etc/fonts"),
    ("usr/share/fonts", "/data/usr/share/fonts"),
    ("usr/share/fontconfig", "/data/usr/share/fontconfig"),
    ("usr/share/alsa", "/data/usr/share/alsa"),
];

/// What Debian's Chromium says to `--version`.
const ARM64_VERSION: &str = "Chromium 154.0.8037.57";

/// [`ARM64_LINKS`] for ARMv7-A's Chromium volume, Debian's armhf: the loader
/// where the program's `PT_INTERP` names it, `/lib/ld-linux-armhf.so.3`.
pub(crate) const ARMHF_LINKS: &[(&str, &str)] = &[
    (
        "lib/ld-linux-armhf.so.3",
        "/data/usr/lib/arm-linux-gnueabihf/ld-linux-armhf.so.3",
    ),
    (
        "lib/arm-linux-gnueabihf",
        "/data/usr/lib/arm-linux-gnueabihf",
    ),
    (
        "usr/lib/arm-linux-gnueabihf",
        "/data/usr/lib/arm-linux-gnueabihf",
    ),
    ("etc/fonts", "/data/etc/fonts"),
    ("usr/share/fonts", "/data/usr/share/fonts"),
    ("usr/share/fontconfig", "/data/usr/share/fontconfig"),
    ("usr/share/alsa", "/data/usr/share/alsa"),
];

/// What Debian's armhf Chromium says to `--version`: Debian's armhf build is
/// a version behind its arm64 and amd64 ones (150 against 154, 2026-10-07).
const ARMHF_VERSION: &str = "Chromium 150.0.7871.181";

/// How long Chromium on AArch64 is given: QEMU emulates every instruction
/// of it on an x86-64 host, several times slower than x86-64 under KVM.
const ARM64_TIMEOUT: u64 = 3600;

/// [`SCRIPT`] for Chromium: the full browser in its `--headless` mode.
const ARM64_SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
cd /tmp
chrome=/data/usr/lib/chromium/chromium
$chrome --version || exit 3
$chrome --headless --no-sandbox --disable-gpu --dump-dom 'PAGE' || exit 4
$chrome --headless --no-sandbox --disable-gpu --screenshot=/tmp/shot.png --window-size=640,360 'PICTURE' || exit 5
[ -s /tmp/shot.png ] || exit 6
echo chrome-gate: screenshot written
exit 16
"#;

/// Where `tools/common/fetch/fetch-chromium-arm64.sh` writes, unless
/// `FERRIX_CHROMIUM_VOLUME` names another directory.
pub(crate) fn arm64_volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_CHROMIUM_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("chromium-arm64")?,
    };
    let image = directory.join("chromium.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-chromium-arm64.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// Where `tools/common/fetch/fetch-chromium-armhf.sh` writes, unless
/// `FERRIX_CHROMIUM_ARMHF_VOLUME` names another directory.
pub(crate) fn armhf_volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_CHROMIUM_ARMHF_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("chromium-armhf")?,
    };
    let image = directory.join("chromium.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-chromium-armhf.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// The search path that finds ferrousli's `libc.so.6` in `/lib` before the
/// volume's libraries.
pub(crate) const LIBRARY_PATH: &str = "/lib:/lib/x86_64-linux-gnu";

/// Whether `--interpreter` or `--library` asks for Chrome on ferrousli.
pub(crate) fn on_ferrousli(args: &Args) -> bool {
    args.interpreter.is_some() || !args.libraries.is_empty()
}

/// The files that stand ferrousli in for the volume's glibc: [`LINKS`] less
/// `/lib64`, whose place the loader takes, and the loader at the path the
/// program `name` beside the volume names, with `libc.so.6` in `/lib`.
///
/// # Errors
///
/// As [`shell::carried_for`], and when the program is not beside the volume.
pub(crate) fn ferrousli_files(
    arch: Arch,
    volume: &std::path::Path,
    name: &str,
    args: &Args,
) -> Result<Vec<crate::ports::File>> {
    let kept: Vec<_> = LINKS
        .iter()
        .copied()
        .filter(|(path, _)| *path != "lib64")
        .collect();
    let mut files = rustc::files(&kept);
    files.extend(ferrousli_loader(arch, volume, name, args)?);
    Ok(files)
}

/// ferrousli's loader at the path the program `name` beside the volume
/// names, and `libc.so.6` in `/lib`: [`ferrousli_files`] without the links,
/// for a desktop that makes its own.
///
/// # Errors
///
/// As [`shell::carried_for`], and when the program is not beside the volume.
pub(crate) fn ferrousli_loader(
    arch: Arch,
    volume: &std::path::Path,
    name: &str,
    args: &Args,
) -> Result<Vec<crate::ports::File>> {
    shell::carried_for(arch, &program_on_host(volume, name)?, args)
}

/// The `env =` line that gives what the compositor starts ferrousli's search
/// path, when Chrome is on ferrousli.
pub(crate) fn window_library_path(ferrousli: bool) -> String {
    if ferrousli {
        format!("env = LD_LIBRARY_PATH,{LIBRARY_PATH}\n")
    } else {
        String::new()
    }
}

/// The headless shell, under the tree beside the volume.
const HEADLESS_PROGRAM: &str = "chrome/chrome-headless-shell";

/// The full browser, under the tree beside the volume.
pub(crate) const WINDOW_PROGRAM: &str = "chrome-window/chrome";

/// Guest memory unless `--memory` says otherwise: Chrome wants about two
/// GiB to open a page, and the page cache holds its 260 MiB of code.
pub(crate) const MEMORY: u32 = 4096;

/// Seconds to wait unless `--timeout` says otherwise: three starts of a
/// browser, emulated when there is no KVM.
const TIMEOUT: u64 = 1800;

/// What the browser in a window says it is: Chrome's own user agent, reduced
/// as Chrome reduces it, with Ferrix in the platform, which says it is not
/// Linux in words a site that looks for `Linux x86_64` still finds.
///
/// `(Ferrix x86_64)` alone had Google's search answer with its page for a
/// browser it no longer supports, every time: a site that knows the
/// platforms it serves does not know Ferrix. With the words `Linux x86_64`
/// in it, as Ubuntu's Firefox said `X11; Ubuntu; Linux x86_64` for years,
/// the same search is answered as Chrome's own user agent is.
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (X11; Ferrix; not Linux x86_64) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36";

/// `--user-agent` changes the user agent and the `User-Agent` header, and
/// nothing else. `navigator.platform` stays `Linux x86_64`, and the client
/// hints -- `navigator.userAgentData` and the `Sec-CH-UA-Platform` header --
/// stay `Linux`. Those are constants in Chrome's build, whatever `uname`
/// says, and no switch reaches them: saying Ferrix there takes a Chromium
/// built from source.
/// Where the image carries the tree's `assets/fonts/`: Inter, Liberation, Noto
/// Sans CJK and the fontconfig file that adds them to the system's, which
/// `FONTCONFIG_FILE` names ([`WINDOW_ENV`]). `assets/fonts/README.md` says what
/// each face is for and where it came from.
const FONTS: &str = "usr/share/ferrix/fonts";

/// `assets/fonts/`'s files, by their path under it.
const FONT_FILES: &[(&str, &[u8])] = &[
    (
        "fonts.conf",
        include_bytes!("../../../../assets/fonts/fonts.conf"),
    ),
    (
        "inter/InterVariable.ttf",
        include_bytes!("../../../../assets/fonts/inter/InterVariable.ttf"),
    ),
    (
        "inter/InterVariable-Italic.ttf",
        include_bytes!("../../../../assets/fonts/inter/InterVariable-Italic.ttf"),
    ),
    (
        "inter/LICENSE",
        include_bytes!("../../../../assets/fonts/inter/LICENSE"),
    ),
    (
        "liberation/LiberationSans-Regular.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSans-Regular.ttf"),
    ),
    (
        "liberation/LiberationSans-Bold.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSans-Bold.ttf"),
    ),
    (
        "liberation/LiberationSans-Italic.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSans-Italic.ttf"),
    ),
    (
        "liberation/LiberationSans-BoldItalic.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSans-BoldItalic.ttf"),
    ),
    (
        "liberation/LiberationSerif-Regular.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSerif-Regular.ttf"),
    ),
    (
        "liberation/LiberationSerif-Bold.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSerif-Bold.ttf"),
    ),
    (
        "liberation/LiberationSerif-Italic.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSerif-Italic.ttf"),
    ),
    (
        "liberation/LiberationSerif-BoldItalic.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationSerif-BoldItalic.ttf"),
    ),
    (
        "liberation/LiberationMono-Regular.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationMono-Regular.ttf"),
    ),
    (
        "liberation/LiberationMono-Bold.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationMono-Bold.ttf"),
    ),
    (
        "liberation/LiberationMono-Italic.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationMono-Italic.ttf"),
    ),
    (
        "liberation/LiberationMono-BoldItalic.ttf",
        include_bytes!("../../../../assets/fonts/liberation/LiberationMono-BoldItalic.ttf"),
    ),
    (
        "liberation/LICENSE",
        include_bytes!("../../../../assets/fonts/liberation/LICENSE"),
    ),
    (
        "noto-cjk/NotoSansCJK-VF.otf.ttc",
        include_bytes!("../../../../assets/fonts/noto-cjk/NotoSansCJK-VF.otf.ttc"),
    ),
    (
        "noto-cjk/LICENSE",
        include_bytes!("../../../../assets/fonts/noto-cjk/LICENSE"),
    ),
];

/// The files an image with the browser in a window carries beside the
/// volume's links: the fonts.
pub(crate) fn window_files() -> Vec<crate::ports::File> {
    FONT_FILES
        .iter()
        .map(|(name, bytes)| crate::ports::File {
            path: format!("{FONTS}/{name}"),
            mode: 0o644,
            content: crate::ports::Content::Bytes(bytes.to_vec()),
        })
        .collect()
}

/// Where Chrome for Testing reads the policies an administrator sets: the
/// directory its binary names (`strings chrome | grep policies`), where
/// Google Chrome's is `/etc/opt/chrome` and Chromium's `/etc/chromium`.
const POLICIES: &str = "etc/opt/chrome_for_testing/policies/managed";

/// The policies a desktop somebody watches gives Chrome, as an administrator
/// would: no signing in (`BrowserSignin` 0), so the toolbar's profile button
/// does not ask "Sign in to Chromium?" on every fresh profile, and no sync
/// or offer to become the default browser, neither of which means anything
/// with a profile in tmpfs. Carried by `run-compositor` only: a gate's
/// Chrome is the one it has always judged.
pub(crate) fn desktop_policy() -> crate::ports::File {
    crate::ports::File {
        path: format!("{POLICIES}/ferrix.json"),
        mode: 0o644,
        content: crate::ports::Content::Bytes(
            br#"{ "BrowserSignin": 0, "SyncDisabled": true, "DefaultBrowserSettingEnabled": false }
"#
            .to_vec(),
        ),
    }
}

/// The command that starts the full browser in a window on the compositor,
/// showing `page`, which may hold no spaces or quotes: it is one word of the
/// command, which the compositor splits as a shell would.
///
/// `--ozone-platform=wayland` makes Chrome a Wayland client, drawing through
/// `wl_shm`; `--disable-gpu` keeps its GPU process to software, since the
/// render node is the compositor's. `--user-data-dir` is in `/dev/shm`,
/// which is tmpfs whatever the root is -- on the desktop's persistent btrfs
/// root too since 2026-09-26, when the kernel began mounting one there; before
/// that, it was devfs's bare directory there, and Chrome stopped at once.
/// `--no-sandbox` for the reason at the top of this file. `--disable-infobars`
/// takes away the bar Chrome for Testing shows under the toolbar, saying it
/// is for automated testing only. [`USER_AGENT`] says Ferrix where Chrome
/// says Linux.
///
/// Sound goes through `pulsed` where the image carries it and the volume has
/// `libpulse` ([`has_pulse`]), which Chrome takes over ALSA once it loads
/// (`docs/AUDIO.md`, U2d). Otherwise it goes through ALSA:
/// `--alsa-output-device=default` opens alsa-lib's
/// `default`, which is `plug` over the card (`docs/AUDIO.md` §2.2), without
/// the enumeration of name hints Chrome would otherwise need to believe a
/// card is there; `--audio-buffer-size=960` makes each packet Chrome writes
/// the card's 20 ms period; and `--autoplay-policy=no-user-gesture-required`
/// lets a page start sound without a click, as a test's must.
pub(crate) fn window_command(page: &str) -> String {
    window_command_for(Arch::X86_64, page)
}

/// [`window_command`] for the browser `arch`'s volume holds: Chrome for
/// Testing's on x86-64, Debian's Chromium on AArch64. Chromium keeps its own
/// user agent, which says `aarch64` truly; [`USER_AGENT`] says x86-64.
pub(crate) fn window_command_for(arch: Arch, page: &str) -> String {
    window_command_with_profile(arch, page, "/dev/shm/chrome")
}

/// [`window_command_for`] with the profile in `profile` rather than in
/// `/dev/shm`: `run-compositor --persistent`'s, on the volume it keeps
/// (`crate::persistent::CHROME_PROFILE`).
pub(crate) fn window_command_with_profile(arch: Arch, page: &str, profile: &str) -> String {
    window_command_on(arch, page, profile, false)
}

/// What [`window_command_with_profile`] puts where `--disable-gpu` is on a
/// desktop whose GPU is NVIDIA's (`run-compositor --nvidia`): ANGLE on
/// NVIDIA's Vulkan for WebGL and rasterization. The compositor takes only
/// `wl_shm` (`docs/NVIDIA.md` §4.6), so Chrome's frames are composited in
/// software and handed over in shared memory; what the GPU draws is read
/// back into them.
/// Not Chrome's own Vulkan compositor (`--enable-features=Vulkan`), which
/// Chrome refuses beside `--ozone-platform=wayland`.
pub(crate) const NVIDIA_GPU_FLAGS: &str = "--use-angle=vulkan \
     --enable-features=DefaultANGLEVulkan --ignore-gpu-blocklist \
     --enable-gpu-rasterization --disable-gpu-compositing";

/// [`window_command_with_profile`], with Chrome's GPU process on NVIDIA's
/// Vulkan when `nvidia` ([`NVIDIA_GPU_FLAGS`]) and in software otherwise.
pub(crate) fn window_command_on(arch: Arch, page: &str, profile: &str, nvidia: bool) -> String {
    let gpu = if nvidia {
        NVIDIA_GPU_FLAGS
    } else {
        "--disable-gpu"
    };
    let (program, agent) = if matches!(arch, Arch::AArch64 | Arch::Armv7a) {
        ("/data/usr/lib/chromium/chromium", String::new())
    } else {
        (
            "/data/chrome-window/chrome",
            format!(" '--user-agent={USER_AGENT}'"),
        )
    };
    format!(
        "{program} --no-sandbox --ozone-platform=wayland \
         --user-data-dir={profile} --no-first-run {gpu} --disable-crash-reporter \
         --disable-breakpad --enable-logging=stderr --disable-infobars \
         --alsa-output-device=default --audio-buffer-size=960 \
         --autoplay-policy=no-user-gesture-required{agent} {page}"
    )
}

/// `/bin/xdg-open` on a desktop with Chrome: what it is given, opened in
/// Chrome. Claude Code signs in by running `$BROWSER` or else `xdg-open`
/// with the sign-in address, and the desktop had neither. The command is
/// the desktop's own window command with `profile`, the one its Chrome was
/// started with, so a running Chrome is handed the address and opens it as
/// a tab rather than a second browser starting on the same profile.
pub(crate) fn opener(arch: Arch, profile: &str, nvidia: bool) -> crate::ports::File {
    let command = window_command_on(arch, r#""$@""#, profile, nvidia);
    crate::ports::File {
        path: "bin/xdg-open".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(
            format!("#!/bin/sh\nexec env {WINDOW_HOME} {command}\n").into_bytes(),
        ),
    }
}

/// The links the image carries into the volume `arch`'s browser is on.
pub(crate) const fn links(arch: Arch) -> &'static [(&'static str, &'static str)] {
    match arch {
        Arch::AArch64 => ARM64_LINKS,
        Arch::Armv7a => ARMHF_LINKS,
        Arch::X86_64 => LINKS,
    }
}

/// The volume `arch`'s browser is on: [`volume`] on x86-64, [`arm64_volume`]
/// on AArch64, [`armhf_volume`] on ARMv7-A.
///
/// # Errors
///
/// As those three.
pub(crate) fn volume_for(arch: Arch) -> Result<std::path::PathBuf> {
    match arch {
        Arch::X86_64 => volume(),
        Arch::AArch64 => arm64_volume(),
        Arch::Armv7a => armhf_volume(),
    }
}

/// The environment the compositor gives Chrome, as `env =` lines: a home in
/// tmpfs, for [`window_command`]'s reason -- NSS keeps its database there --
/// a runtime directory that can be written, and [`FONTS`]'s fontconfig file,
/// which includes the system's. The compositor gives these to every program
/// it starts, so foot draws with the same fonts file.
pub(crate) const WINDOW_ENV: &str = "env = HOME,/dev/shm\nenv = XDG_RUNTIME_DIR,/tmp\n\
     env = FONTCONFIG_FILE,/usr/share/ferrix/fonts/fonts.conf\n";

/// [`WINDOW_ENV`] on a desktop with other clients: all but the home, which
/// [`WINDOW_HOME`] gives Chrome alone. As an `env =` line it moved every
/// program's home, and the user's waybar found no `~/.config/waybar`.
pub(crate) const DESKTOP_ENV: &str = "env = XDG_RUNTIME_DIR,/tmp\n\
     env = FONTCONFIG_FILE,/usr/share/ferrix/fonts/fonts.conf\n";

/// Chrome's home on such a desktop, as words before its command.
pub(crate) const WINDOW_HOME: &str = "HOME=/dev/shm";

/// Whether the volume `image` has `libpulse`, in the tree
/// `tools/common/fetch/fetch-chrome.sh` keeps beside it: a volume fetched before
/// 2026-09-27 has not, and on it Chrome's sound can only be ALSA's, which a
/// `pulsed` holding the card would refuse.
pub(crate) fn has_pulse(image: &std::path::Path) -> bool {
    image
        .parent()
        .map(|directory| directory.join("tree/usr/lib/x86_64-linux-gnu/libpulse.so.0"))
        // The link itself: a Windows host reads the tree in WSL, where it
        // cannot follow one.
        .is_some_and(|library| std::fs::symlink_metadata(library).is_ok())
}

/// [`volume`], refused unless it [`has_pulse`]: for a gate that plays
/// Chrome's sound through `pulsed`.
///
/// # Errors
///
/// As [`volume`], and for a volume fetched before libpulse was on it.
pub(crate) fn pulse_volume() -> Result<std::path::PathBuf> {
    let volume = volume()?;
    if has_pulse(&volume) {
        Ok(volume)
    } else {
        Err(Error::new(format!(
            "{} has no libpulse beside it, which Chrome's sound goes through: \
             tools/common/fetch/fetch-chrome.sh again",
            volume.display()
        )))
    }
}

/// Where `tools/common/fetch/fetch-chrome.sh` writes, unless `FERRIX_CHROME_VOLUME`
/// names another directory.
pub(crate) fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_CHROME_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("chrome")?,
    };
    let image = directory.join("chrome.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-chrome.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// The script, with the pages in it, and for ferrousli the search path that
/// finds its `libc.so.6` in `/lib` before the volume's.
fn script(ferrousli: bool) -> String {
    let script = SCRIPT.replace("PAGE", PAGE).replace("PICTURE", PICTURE);
    if ferrousli {
        format!("export LD_LIBRARY_PATH={LIBRARY_PATH}\n{script}")
    } else {
        script
    }
}

/// Chrome's program `name` as `tools/common/fetch/fetch-chrome.sh` unpacked it beside
/// the volume, whose `PT_INTERP` says where a loader of ferrousli's must go.
fn program_on_host(volume: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    let program = volume
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("tree")
        .join(name);
    if program.is_file() {
        Ok(program)
    } else {
        Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-chrome.sh leaves it beside the volume",
            program.display()
        )))
    }
}

/// `test-chrome` or `test-chrome-window`, whichever `command` names: the
/// browser headless, or in a window on the compositor
/// ([`crate::compositor::test_chrome_window`]).
///
/// # Errors
///
/// As the one it runs.
pub(crate) fn run(command: &str, args: &Args) -> Result<()> {
    if command == "test-chrome-window" {
        crate::compositor::test_chrome_window(args)
    } else if command == "test-chrome-audio" {
        crate::compositor::test_chrome_audio(args)
    } else {
        test_chrome(args)
    }
}

/// Boot a shell whose script runs Chrome three times.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails, or
/// any step of the script does not do what it must.
pub(crate) fn test_chrome(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [arch @ (Arch::AArch64 | Arch::Armv7a)] => return test_chromium(*arch, args),
        [Arch::X86_64] => Arch::X86_64,
        _ => {
            return Err(Error::new(
                "test-chrome runs on x86-64, with Chrome for Testing, or on AArch64 or \
                 ARMv7-A, with Debian's Chromium: one at a time",
            ));
        }
    };
    let mut args = args.clone();
    let volume = volume()?;
    args.data_image = Some(volume.clone());
    let ferrousli = on_ferrousli(&args);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }

    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let libc = if ferrousli { "ferrousli" } else { "glibc" };
    println!("  {arch}: building an image whose shell runs headless Chrome on {libc}");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, &script(ferrousli))?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let links = if ferrousli {
        ferrousli_files(arch, &volume, HEADLESS_PROGRAM, &args)?
    } else {
        rustc::files(LINKS)
    };
    // zinc alone: the script is builtins, and every program it runs is on
    // the volume.
    let archive = initramfs::build(None, &natives, Some(&bytes), &links)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: running Chrome on Ferrix with {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    judge(arch, VERSION, &lines)
}

/// [`test_chrome`] on AArch64 or ARMv7-A: Debian's Chromium from its arm64
/// or armhf volume, on Debian's glibc. ferrousli has no build for either to
/// stand in for it.
///
/// # Errors
///
/// As [`test_chrome`].
fn test_chromium(arch: Arch, args: &Args) -> Result<()> {
    if on_ferrousli(args) {
        return Err(Error::new(format!(
            "test-chrome on {arch} runs on Debian's glibc: ferrousli is built for x86-64"
        )));
    }
    let mut args = args.clone();
    args.data_image = Some(volume_for(arch)?);
    // Read-only on ARMv7-A, the DK1's architecture: the page cache gives back
    // a read-only volume's clean pages under pressure, so Chromium can run in
    // the board's 512 MiB (`docs/CHROME.md` §10).
    args.data_image_read_only = arch == Arch::Armv7a;
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = ARM64_TIMEOUT;
    }
    let shell = zinc::built(arch)?
        .ok_or_else(|| Error::new(format!("zinc could not be built for {arch}")))?;
    println!("  {arch}: building an image whose shell runs headless Chromium on glibc");
    let script = ARM64_SCRIPT
        .replace("PAGE", PAGE)
        .replace("PICTURE", PICTURE);
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, &script)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let links = rustc::files(links(arch));
    let archive = initramfs::build(None, &natives, Some(&bytes), &links)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    println!(
        "  {arch}: running Chromium on Ferrix with {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let version = if arch == Arch::Armv7a {
        ARMHF_VERSION
    } else {
        ARM64_VERSION
    };
    judge(arch, version, &lines)
}

/// Whether the transcript is a Chrome that loaded, ran a page's script,
/// drew a picture, and a script that got to its end.
fn judge(arch: Arch, version: &str, lines: &[String]) -> Result<()> {
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default();
    let exited = after_boot
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    let loaded = after_boot.iter().any(|line| line.contains(version));
    let computed = after_boot.iter().any(|line| line.contains(COMPUTED));
    let drew = after_boot.iter().any(|line| line.trim_end() == SHOT);
    match exited {
        Some(status) if status == STATUS.to_string() && loaded && computed && drew => {
            println!(
                "  {arch}: {version} loaded, ran a page's script and drew a screenshot \
                 on Ferrix"
            );
            Ok(())
        }
        Some("3") => Err(Error::new(format!("{arch}: `chrome --version` failed"))),
        Some("4") => Err(Error::new(format!(
            "{arch}: Chrome loaded but `--dump-dom` failed"
        ))),
        Some("5") => Err(Error::new(format!(
            "{arch}: Chrome ran a page but `--screenshot` failed"
        ))),
        Some("6") => Err(Error::new(format!(
            "{arch}: Chrome said it took a screenshot, and the file is empty or missing"
        ))),
        Some(status) => Err(Error::new(format!(
            "{arch}: the script exited with {status}; version {loaded}, script ran {computed}, \
             screenshot {drew}"
        ))),
        None => Err(Error::new(format!("{arch}: the shell never exited"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn the_script_carries_both_pages_and_no_placeholder() {
        let script = script(false);
        assert!(script.contains("\"computed \"+6*7"));
        // The source must not already hold what the DOM is required to.
        assert!(!script.contains(COMPUTED));
        assert!(script.contains("Hello from Chrome on Ferrix"));
        assert!(!script.contains("'PAGE'") && !script.contains("'PICTURE'"));
    }

    #[test]
    fn the_user_agent_is_one_quoted_word_of_the_window_command() {
        let command = window_command("data:text/html,x");
        let quoted: Vec<&str> = command.split('\'').collect();
        assert_eq!(quoted.len(), 3, "{command}");
        assert_eq!(quoted[1], format!("--user-agent={USER_AGENT}"));
        // Ferrix by name, and the words a site looking for Linux looks for.
        assert!(USER_AGENT.contains("(X11; Ferrix; not Linux x86_64)"));
    }

    /// The opener hands Chrome the address as one word, `&`s and all, with
    /// the profile it is given: run here with `printf` for Chrome.
    #[test]
    fn the_opener_hands_chrome_the_address_whole() {
        let file = opener(Arch::X86_64, "/data/home/chrome", false);
        assert_eq!((file.path.as_str(), file.mode), ("bin/xdg-open", 0o755));
        let crate::ports::Content::Bytes(bytes) = file.content else {
            panic!("the opener is a script");
        };
        let script = String::from_utf8(bytes).expect("UTF-8");
        assert!(script.starts_with("#!/bin/sh\nexec env HOME=/dev/shm "));
        assert!(script.contains(" --user-data-dir=/data/home/chrome "));
        let script = script.replace("/data/chrome-window/chrome", "printf '%s\\n'");
        let address = "https://claude.com/cai/oauth/authorize?code=true&scope=a+b&state=x y";
        let output = std::process::Command::new("sh")
            .args(["-c", &script, "xdg-open", address])
            .output()
            .expect("sh runs");
        let printed = String::from_utf8_lossy(&output.stdout);
        assert_eq!(printed.lines().last(), Some(address), "{printed}");
    }

    #[test]
    fn fontconfig_is_pointed_at_the_carried_fonts() {
        let conf = String::from_utf8_lossy(FONT_FILES[0].1);
        assert_eq!(FONT_FILES[0].0, "fonts.conf");
        assert!(WINDOW_ENV.contains(&format!("env = FONTCONFIG_FILE,/{FONTS}/fonts.conf\n")));
        assert_eq!(
            WINDOW_ENV,
            format!("env = {}\n{DESKTOP_ENV}", WINDOW_HOME.replacen('=', ",", 1))
        );
        assert!(conf.contains(&format!("<dir>/{FONTS}</dir>")));
        assert!(conf.contains("<include ignore_missing=\"yes\">/etc/fonts/fonts.conf</include>"));
        let paths: Vec<_> = window_files().into_iter().map(|file| file.path).collect();
        for face in [
            "inter/InterVariable.ttf",
            "liberation/LiberationSans-Regular.ttf",
            "noto-cjk/NotoSansCJK-VF.otf.ttc",
        ] {
            assert!(paths.contains(&format!("{FONTS}/{face}")), "{face}");
        }
    }

    #[test]
    fn a_chrome_that_ran_passes() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            VERSION,
            "<html><head></head><body><p id=\"v8\">computed 42</p><script>document.\
             getElementById(\"v8\").textContent=\"computed \"+6*7</script></body></html>",
            SHOT,
            "  init     the shell exited with 16",
        ]);
        assert!(judge(Arch::X86_64, VERSION, &lines).is_ok());
    }

    #[test]
    fn a_dom_whose_script_never_ran_fails() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            VERSION,
            "<html><head></head><body><p id=\"v8\"></p><script>document.getElementById(\"v8\").\
             textContent=\"computed \"+6*7</script></body></html>",
            SHOT,
            "  init     the shell exited with 16",
        ]);
        assert!(judge(Arch::X86_64, VERSION, &lines).is_err());
    }

    #[test]
    fn each_failing_step_is_named() {
        for (status, words) in [
            ("3", "--version"),
            ("4", "--dump-dom"),
            ("5", "--screenshot"),
            ("6", "empty"),
        ] {
            let exit = format!("  init     the shell exited with {status}");
            let lines = transcript(&[qemu::SUCCESS_MARKER, &exit]);
            let error = judge(Arch::X86_64, VERSION, &lines)
                .unwrap_err()
                .to_string();
            assert!(error.contains(words), "{status}: {error}");
        }
    }
}
