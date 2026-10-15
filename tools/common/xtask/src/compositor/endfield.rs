//! `test-endfield-launcher`: Arknights: Endfield's Windows launcher
//! (GRYPHLINK) in Debian's Wine on hyprix through yserver, on the network
//! (stage 23, step 4; docs/WINE.md §8). `test-endfield-proton` is the same
//! boot in GE-Proton's Wine instead, on a volume made from
//! `fetch-proton.sh`'s (docs/WINE.md §10), and runs the 32-bit `cmd` first.
//!
//! `tools/common/fetch/fetch-endfield-launcher.sh` makes the volume: the
//! Wine volume's tree with the launcher installed under `/GRYPHLINK`, as
//! its 32-bit installer would install it (the installer itself waits on
//! E1.6). The desktop is `test-wine`'s second boot's, with the network
//! through xtask's gateway. [`SCRIPT`] makes a prefix, links
//! `C:\GRYPHLINK` to the volume's, starts `Launcher.exe`, which starts
//! `Games.exe`, and says [`WINDOW`] once X has its window viewable. The
//! gate then takes the screen until the launcher's yellow main button is
//! on it, and passes when hyprix lists the window by its title, the button
//! is drawn, and the launcher's own log says Gryphline's server answered
//! its query for the game's version with HTTP 200: its web view drew its
//! first screen, and it reached its server over TLS through the gateway.
//!
//! Nothing is logged into and nothing is downloaded: the gate stops at the
//! launcher's first screen, whose buttons say "Log In" and "Download".

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{build_image, undithered, with_the_transcript};
use super::{Carried, EITHER, Programs, gates_busybox};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The script that makes the volume, for the launcher version it pins.
const FETCH: &str = include_str!("../../../fetch/fetch-endfield-launcher.sh");

/// The script that makes Proton's volume, for the Proton version it pins.
const FETCH_PROTON: &str = include_str!("../../../fetch/fetch-proton.sh");

/// Which Wine the launcher runs in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wine {
    /// Debian 13's Wine 10.0, the Wine volume's.
    Debian,
    /// GE-Proton's Wine, at `/proton` on the volume, with Debian's i386
    /// libraries for its 32-bit half.
    Proton,
}

impl Wine {
    /// The lines that name the loader and the server, and Proton's
    /// environment (what its `proton` script sets for Wine).
    pub(crate) fn prelude(self) -> &'static str {
        match self {
            Wine::Debian => {
                "WINE=/usr/lib/wine/wine64\nexport WINESERVER=/usr/lib/wine/wineserver64\n"
            }
            // Proton's fonts go into the prefix's Fonts before wineboot,
            // as its script's `create_fonts_symlinks` puts them: linked
            // after, Tahoma is missing from DirectWrite's system set and
            // QtWebEngine ends on a null typeface (docs/WINE.md §10).
            Wine::Proton => {
                "WINE=/proton/files/bin/wine\nexport WINESERVER=/proton/files/bin/wineserver\n\
                 export LD_LIBRARY_PATH=/proton/files/lib/x86_64-linux-gnu:/proton/files/lib/i386-linux-gnu\n\
                 export WINEDLLPATH=/proton/files/lib/vkd3d:/proton/files/lib/wine\n\
                 mkdir -p /tmp/wine/prefix/drive_c/windows/Fonts\n\
                 for f in /proton/files/share/fonts/*.tt[fc] /proton/files/share/wine/fonts/*.tt[fc] \
                 /data/usr/share/fonts/truetype/open-sans/*.ttf; do\n\
                 \x20   ln -sf \"$f\" /tmp/wine/prefix/drive_c/windows/Fonts/\n\
                 done\n"
            }
        }
    }

    /// The directory under `~/.local/share/ferrix` its volume is in.
    fn directory(self) -> &'static str {
        match self {
            Wine::Debian => "endfield-launcher",
            Wine::Proton => "endfield-launcher-proton",
        }
    }
}

/// Proton's paths the boot links into the volume: Proton itself, glibc's
/// i386 loader and directory, which its 32-bit half names, and the
/// certificate bundle. Wine's `crypt32` fills the Windows root store from
/// `/etc/ssl/certs/ca-certificates.crt` (and a few other absolute paths);
/// without it the store holds only Wine's built-in roots, and the
/// launcher's download SDK -- libcurl over OpenSSL, taking its roots from
/// that store -- fails every file with curl's error 60 (docs/WINE.md §10).
pub(crate) const PROTON_LINKS: &[(&str, &str)] = &[
    ("proton", "/data/proton"),
    (
        "etc/ssl/certs/ca-certificates.crt",
        "/data/etc/ssl/certs/ca-certificates.crt",
    ),
    (
        "lib/ld-linux.so.2",
        "/data/usr/lib/i386-linux-gnu/ld-linux.so.2",
    ),
    ("lib/i386-linux-gnu", "/data/usr/lib/i386-linux-gnu"),
    ("usr/lib/i386-linux-gnu", "/data/usr/lib/i386-linux-gnu"),
];

/// What the script puts before each line the 32-bit `cmd /c ver` printed.
const CMD32: &str = "endfield: cmd32: ";

/// Where [`SCRIPT`] is in the image.
const SCRIPT_PATH: &str = "etc/endfield.sh";

/// The title of the launcher's main window, as hyprix lists it.
const TITLE: &str = "GRYPHLINK";

/// What the script prints once the launcher's window is viewable.
const WINDOW: &str = "endfield: window";

/// What the script prints last, whatever happened.
const END: &str = "endfield: end";

/// What the script puts before each line of the launcher's own log.
const LOG: &str = "endfield: games.log: ";

/// What the launcher logs when Gryphline's server answered its query for
/// the launcher's and the game's versions.
const ANSWERED: &str = "OnQuery_ProtocolVer_LauncherVer_GameVer_Result";

/// How long the script has to reach [`WINDOW`]: yserver's start,
/// `wineboot`, and the launcher's start with its web engine.
const PATIENCE: Duration = Duration::from_secs(600);

/// How long the gate waits after [`WINDOW`] for the button to be drawn: the
/// web view starts its render process and loads its page.
const DRAWN: Duration = Duration::from_secs(240);

/// The part of the screen, in thousandths, that must be the main button's
/// yellow. On nazuna's Linux, in the same Wine at 1280 by 800, it was 10‰.
const YELLOW_PERMILLE: usize = 4;

/// Megabytes of guest RAM: a prefix on tmpfs, and Chromium's processes.
const MEMORY: u32 = 8192;

/// The script hyprix's `exec-once` runs. Every line of its own starts
/// `endfield:`, and it ends with [`END`] whatever happened.
///
/// The prefix is made as `test-wine`'s second boot makes it. The launcher
/// reads and writes beside itself (its database, a web cache), which the
/// volume takes under QEMU's snapshot. After [`WINDOW`] the script waits
/// for the gate to take its screens, then prints the launcher's log.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
# fsync, as Proton's own script turns it on; Debian's Wine has none and
# ignores it (docs/WINE.md §11).
export WINEFSYNC=@FSYNC@
echo "endfield: start, WINEFSYNC=$WINEFSYNC"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "endfield: the X socket was there after ${waited}s, DISPLAY is $DISPLAY"
if udhcpc -i eth0 -n -q -t 5 -T 2 > /dev/null 2>&1; then
    echo "endfield: a lease for eth0"
else
    echo "endfield: no lease for eth0"
fi
nslookup launcher.gryphline.com 2>&1 | sed 's/^/endfield: dns: /'
mkdir -p /tmp/wine
$WINE wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "endfield: wineboot exited $?"
$WINESERVER -w
# Segoe UI, which Windows programs name and no prefix has, drawn in Open
# Sans where the volume carries it (fetch-proton.sh says why, and why not
# Microsoft's Selawik).
if [ -e /tmp/wine/prefix/drive_c/windows/Fonts/OpenSans-Regular.ttf ]; then
    for pair in "Segoe UI=Open Sans" "Segoe UI Semibold=Open Sans Semibold" "Segoe UI Light=Open Sans Light" \
        "Segoe UI Semilight=Open Sans Light" "Segoe UI Bold=Open Sans"; do
        $WINE reg add 'HKCU\Software\Wine\Fonts\Replacements' /v "${pair%%=*}" /d "${pair#*=}" /f > /dev/null 2>&1
    done
    $WINESERVER -w
    echo "endfield: Segoe UI drawn in Open Sans"
fi
$WINE 'C:\windows\syswow64\cmd.exe' /c ver 2>&1 | sed 's/^/endfield: cmd32: /'
ln -s /data/GRYPHLINK /tmp/wine/prefix/drive_c/GRYPHLINK
cd /tmp/wine/prefix/drive_c/GRYPHLINK
$WINE 'C:\GRYPHLINK\Launcher.exe' 2>&1 | grep -E 'err:' > /tmp/wine/launcher.log &
waited=0
until /bin/hyprctl clients 2>/dev/null | grep -q "title: GRYPHLINK" || [ $waited -ge 300 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "endfield: the launcher's window was viewable after ${waited}s"
xwininfo -name "GRYPHLINK" 2>&1 | grep -E "Width|Height|Map State" | sed 's/^/endfield: launcher: /'
/bin/hyprctl clients | sed 's/^/endfield: clients: /'
echo "endfield: window"
# Processor time, in 10 ms ticks, of wineserver and of the Windows
# programs, summed over /proc, across the last two minutes of the wait.
sample() {
    server=0
    programs=0
    for d in /proc/[0-9]*; do
        c=$(tr '\0' ' ' < $d/cmdline 2>/dev/null)
        t=$(awk '{ print $14 + $15 }' $d/stat 2>/dev/null)
        case "$c" in
            *wineserver*) server=$((server + ${t:-0})) ;;
            *.exe*|*.EXE*) programs=$((programs + ${t:-0})) ;;
        esac
    done
    echo "$server $programs"
}
sleep 120
first=$(sample)
sleep 120
second=$(sample)
echo "endfield: cpu over 120s: wineserver $(( ${second% *} - ${first% *} )) ticks, Windows programs $(( ${second#* } - ${first#* } )) ticks, WINEFSYNC=$WINEFSYNC"
grep -h 'fsync: up and running' /tmp/wine/*.log 2>/dev/null | head -n 1 | sed 's/^/endfield: server: /'
logs=$(echo /tmp/wine/prefix/drive_c/users/*/AppData/LocalLow/Gryphline/*/logs)
cut -c1-400 "$logs/games.log" | sed 's/^/endfield: games.log: /'
grep -v ':fixme:' /tmp/wine/launcher.log | sort | uniq -c | sort -rn | head -n 40 | cut -c1-300 | sed 's/^/endfield: wine: /'
/bin/hyprctl clients | sed 's/^/endfield: clients: /'
ps | grep -c '[Q]tWebEngineProcess' | sed 's/^/endfield: render processes: /'
$WINESERVER -k
echo "endfield: end"
"#;

/// Where [`SCRIPT`]'s own start of the launcher begins: the prefix, the
/// fonts' names, the 32-bit `cmd`, and `Launcher.exe` in the background.
const OWN_START: &str = "mkdir -p /tmp/wine\n$WINE wineboot";

/// What ends [`SCRIPT`]'s own start of the launcher.
const OWN_END: &str = "> /tmp/wine/launcher.log &\n";

/// What starts the launcher instead with `--everything`: the volume's
/// `gryphlink`, as a user of that desktop starts it, which makes the prefix
/// its own way (`fetch-endfield-launcher.sh`) at [`SCRIPT`]'s
/// `WINEPREFIX`.
const GRYPHLINK_START: &str = "mkdir -p /tmp/wine\necho \"endfield: gryphlink from the merged volume\"\n\
     /data/usr/bin/gryphlink 2>&1 | grep -E 'err:' > /tmp/wine/launcher.log &\n";

/// [`SCRIPT`] after the lines that name `wine`'s loader and server; with
/// `gryphlink`, its own start of the launcher replaced by
/// [`GRYPHLINK_START`].
fn script(wine: Wine, gryphlink: bool) -> String {
    let replaced = SCRIPT
        .split_once(OWN_START)
        .and_then(|(before, rest)| {
            rest.split_once(OWN_END)
                .map(|(_, after)| format!("{before}{GRYPHLINK_START}{after}"))
        })
        .filter(|_| gryphlink);
    let body = replaced.unwrap_or_else(|| SCRIPT.to_owned());
    let script = format!("{}{body}", wine.prelude()).replace(FSYNC_PLACE, fsync());
    // ep-wine scratch, never land: FERRIX_EP_PRE names a shell file run
    // before the launcher starts (in the GRYPHLINK directory, prefix made).
    match std::env::var_os("FERRIX_EP_PRE") {
        Some(path) => {
            let pre = std::fs::read_to_string(path).expect("FERRIX_EP_PRE");
            script.replacen(
                "cd /tmp/wine/prefix/drive_c/GRYPHLINK\n",
                &format!("cd /tmp/wine/prefix/drive_c/GRYPHLINK\n{pre}\n"),
                1,
            )
        }
        None => script,
    }
}

/// Where [`SCRIPT`] says whether fsync is on.
const FSYNC_PLACE: &str = "@FSYNC@";

/// `WINEFSYNC` for the launcher: `1`, as Proton's own script sets it, or `0`
/// when `FERRIX_WINEFSYNC=0` asks for wineserver's synchronisation alone, to
/// compare the two (docs/WINE.md §11).
fn fsync() -> &'static str {
    match std::env::var("FERRIX_WINEFSYNC").as_deref() {
        Ok("0") => "0",
        _ => "1",
    }
}

/// The launcher version `fetch-endfield-launcher.sh` pins.
fn pinned() -> &'static str {
    FETCH
        .lines()
        .find_map(|line| line.strip_prefix("LAUNCHER_VERSION="))
        .unwrap_or_default()
}

/// The Proton version `fetch-proton.sh` pins.
pub(crate) fn proton_pinned() -> &'static str {
    FETCH_PROTON
        .lines()
        .find_map(|line| line.strip_prefix("PROTON_VERSION="))
        .unwrap_or_default()
}

/// The volume `tools/common/fetch/fetch-endfield-launcher.sh` makes.
///
/// # Errors
///
/// It has not been made, or was made with another launcher or another
/// yserver than the ones the scripts pin.
pub(crate) fn volume(wine: Wine) -> Result<PathBuf> {
    let directory = match std::env::var_os("FERRIX_ENDFIELD_VOLUME") {
        Some(directory) => PathBuf::from(directory),
        None => paths::volume_directory(wine.directory())?,
    };
    let image = directory.join("endfield.img");
    let fetch = match wine {
        Wine::Debian => "tools/common/fetch/fetch-endfield-launcher.sh",
        Wine::Proton => "FERRIX_ENDFIELD_WINE=proton tools/common/fetch/fetch-endfield-launcher.sh",
    };
    let again = format!("{fetch} makes it again");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: {fetch} makes it",
            image.display()
        )));
    }
    let read = |name: &str| {
        std::fs::read_to_string(directory.join(name))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let launcher = read("launcher.version");
    if launcher != pinned() {
        return Err(Error::new(format!(
            "{} carries launcher {launcher:?}, not {}: {again}",
            image.display(),
            pinned()
        )));
    }
    if wine == Wine::Proton && read("proton.version") != proton_pinned() {
        return Err(Error::new(format!(
            "{} carries Proton {:?}, not {}: {again}",
            image.display(),
            read("proton.version"),
            proton_pinned()
        )));
    }
    let yserver = read("yserver.commit");
    if yserver != crate::yserver::pinned() {
        return Err(Error::new(format!(
            "{} carries yserver {yserver:?}, not {}: {again}",
            image.display(),
            crate::yserver::pinned()
        )));
    }
    Ok(image)
}

/// [`volume`] for Proton, the one `--everything` merges.
///
/// # Errors
///
/// As [`volume`].
pub(crate) fn proton_volume() -> Result<PathBuf> {
    volume(Wine::Proton)
}

/// Where `run-compositor --everything`'s launcher entry for GRYPHLINK is.
const ENTRY_PATH: &str = "usr/share/applications/gryphlink.desktop";

/// fuzzel's entry for GRYPHLINK, with Steam's games icon.
const ENTRY: &str = "[Desktop Entry]\nType=Application\nName=GRYPHLINK\n\
     GenericName=Game Launcher\nComment=Arknights: Endfield's launcher, in GE-Proton's Wine\n\
     Exec=/bin/gryphlink\nIcon=ferrix-games\nTerminal=false\nKeywords=endfield;arknights;gryphline;wine;proton;\n\
     Categories=Game;\n";

/// What `run-compositor --everything` adds to the archive for GRYPHLINK,
/// when the Proton launcher volume is merged into its own: Wine's and
/// Proton's links less any path `carried` already has (Steam's name the
/// same i386 paths), `/bin/gryphlink`, a link to the volume's, and fuzzel's
/// entry for it.
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
    let links: Vec<(&str, &str)> = crate::wine::LINKS
        .iter()
        .chain(PROTON_LINKS)
        .chain(&[("bin/gryphlink", "/data/usr/bin/gryphlink")])
        .copied()
        .filter(|(path, _)| !taken(path))
        .collect();
    let mut files = crate::rustc::files(&links);
    files.push(File {
        path: ENTRY_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(ENTRY.as_bytes().to_vec()),
    });
    files
}

/// The part of `screen`, in thousandths, that is the main button's yellow.
fn yellow_permille(screen: &Image) -> usize {
    let (pixels, _) = screen.pixels.as_chunks::<3>();
    let yellow = pixels
        .iter()
        .filter(|[r, g, b]| *r > 230 && *g > 230 && *b < 60)
        .count();
    yellow * 1000 / pixels.len().max(1)
}

/// Whether hyprix's list, among the script's lines, has the launcher's
/// window.
fn listed(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        line.split_once("endfield: clients:")
            .and_then(|(_, rest)| rest.trim().split_once(" -> "))
            .is_some_and(|(_, title)| title.trim_end_matches(':') == TITLE)
    })
}

/// Whether the launcher's log, as the script printed it, says Gryphline's
/// server answered with HTTP 200.
fn answered(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        line.split_once(LOG)
            .is_some_and(|(_, said)| said.contains(ANSWERED) && said.contains("nCode=200"))
    })
}

/// A screendump, read.
fn screen(qmp: &mut Qmp, dump: &Path) -> Result<Image> {
    qmp.screendump(Some(DEVICE_ID), dump)?;
    let bytes = std::fs::read(dump)
        .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
    parse_ppm(&bytes)
}

/// The gate's boot: the network, no 3D card, [`MEMORY`] unless one was
/// given, and the launcher's volume -- or, with `--everything`, the volume
/// run-compositor --everything attaches, whose `gryphlink` then starts the
/// launcher (said by the `bool`). Only Proton's launcher is merged there.
fn boot_args(args: &Args, wine: Wine) -> Result<(Args, bool)> {
    let gryphlink = args.everything;
    if gryphlink && wine != Wine::Proton {
        return Err(Error::new(
            "--everything carries the launcher in Proton: test-endfield-proton --everything",
        ));
    }
    let mut args = args.clone();
    args.gl = false;
    args.net = true;
    args.data_image = Some(if gryphlink {
        let _ = proton_volume()?;
        crate::everything::volume()?
    } else {
        volume(wine)?
    });
    if !args.memory_given {
        args.memory = MEMORY;
    }
    Ok((args, gryphlink))
}

/// Boot hyprix with yserver, the network and [`SCRIPT`], and require the
/// launcher's window, its main button and its server's answer.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails,
/// or the window, the button or the answer did not come.
pub(crate) fn test_endfield_launcher(args: &Args, wine: Wine) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-endfield-launcher runs on x86-64")),
    };
    let (args, gryphlink) = boot_args(args, wine)?;
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-endfield-launcher needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
    })?;
    let programs = Programs::build(arch)?;
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.extend(crate::rustc::files(crate::wine::LINKS));
    if wine == Wine::Proton {
        ports.extend(crate::rustc::files(PROTON_LINKS));
    }
    ports.push(File {
        path: SCRIPT_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(script(wine, gryphlink).into_bytes()),
    });
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports,
        ..Carried::none()
    };
    let config = format!(
        "# Carried into the initramfs by `cargo xtask test-endfield-launcher`.\n\
         env = XDG_RUNTIME_DIR,/tmp\n{}exec-once = /bin/busybox sh /{SCRIPT_PATH}\n",
        crate::yserver::desktop_config(),
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("endfield-launcher.ppm");
    let which = match wine {
        Wine::Debian => "Debian's Wine".to_owned(),
        Wine::Proton if gryphlink => format!(
            "{}, started by gryphlink on run-compositor --everything's volume",
            proton_pinned()
        ),
        Wine::Proton => proton_pinned().to_owned(),
    };
    println!(
        "  {arch}: GRYPHLINK {} in {which} on hyprix through yserver, on the network, {} MiB",
        pinned(),
        qemu_args.memory
    );
    let mut said: Vec<String> = Vec::new();
    let mut yellow = 0;
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
        if window {
            let deadline = Instant::now() + DRAWN;
            loop {
                yellow = yellow_permille(&screen(&mut qmp, &dump)?);
                if yellow >= YELLOW_PERMILLE || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
        let ended = watching.read_more(
            Instant::now() + DRAWN + Duration::from_secs(120),
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
                &Error::new(format!("{arch}: the launcher's script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    report_cmd32(arch, &said);
    report_cpu(arch, &said);
    judge(arch, &said, yellow, &dump)
}

/// What the script puts before the processor time it measured.
const CPU: &str = "endfield: cpu over ";

/// Say what processor time wineserver and the Windows programs took over
/// the last two minutes, with fsync on or off: a figure, not judged.
fn report_cpu(arch: Arch, said: &[String]) {
    if let Some(cpu) = said
        .iter()
        .find_map(|line| line.split_once(CPU).map(|(_, said)| said.trim()))
    {
        println!("  {arch}: processor time over {cpu} (10 ms ticks)");
    }
}

/// Say whether the 32-bit `cmd` answered with a Windows version.
fn report_cmd32(arch: Arch, said: &[String]) {
    let cmd32 = said
        .iter()
        .filter_map(|line| line.split_once(CMD32).map(|(_, said)| said.trim()))
        .find(|said| said.contains(crate::wine::VERSION));
    match cmd32 {
        Some(version) => println!("  {arch}: the 32-bit cmd answered `{version}`"),
        None => println!("  {arch}: the 32-bit cmd did not answer with a Windows version"),
    }
}

/// Whether hyprix listed the launcher's window, its main button was drawn
/// (`yellow` thousandths of the screen), and its server answered.
fn judge(arch: Arch, said: &[String], yellow: usize, dump: &Path) -> Result<()> {
    if !listed(said) {
        return Err(Error::new(format!(
            "{arch}: hyprix lists no window titled `{TITLE}`; the `endfield:` lines say how far \
             the launcher came"
        )));
    }
    if yellow < YELLOW_PERMILLE {
        return Err(Error::new(format!(
            "{arch}: the launcher's window is listed, but its main button's yellow is {yellow}‰ \
             of the screen, less than {YELLOW_PERMILLE}‰: its web view drew no first screen: {}",
            dump.display()
        )));
    }
    if !answered(said) {
        return Err(Error::new(format!(
            "{arch}: the launcher drew its first screen, but its log does not say Gryphline's \
             server answered `{ANSWERED}` with 200: the `{LOG}` lines say what it got"
        )));
    }
    println!(
        "  {arch}: the launcher is a window of hyprix's titled `{TITLE}`, its main button is \
         drawn ({yellow}‰ of the screen yellow), and Gryphline's server answered it: {}",
        dump.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fetch script pins a version, and the title the script waits for
    /// is the one the judge looks for.
    #[test]
    fn the_pin_and_the_title_agree() {
        assert!(!pinned().is_empty());
        assert!(SCRIPT.contains(&format!("xwininfo -name \"{TITLE}\"")));
        assert!(SCRIPT.contains(&format!("sed 's/^/{LOG}/'")));
        assert!(SCRIPT.contains(&format!("echo \"{CPU}120s")));
        let launcher = script(Wine::Proton, false);
        assert!(
            launcher.contains("export WINEFSYNC=1\n") || launcher.contains("export WINEFSYNC=0\n")
        );
        assert!(!launcher.contains(FSYNC_PLACE));
    }

    /// The window is found by its title in hyprix's list, and the server's
    /// answer only with 200.
    #[test]
    fn the_window_and_the_answer_are_read_from_the_lines() {
        let lines = vec![
            format!("  9.0 | endfield: clients: Window 5a -> {TITLE}:"),
            format!("  9.1 | {LOG}[info] cgm {ANSWERED} result:bRet=true, nCode=200, x"),
        ];
        assert!(listed(&lines));
        assert!(answered(&lines));
        let refused = vec![format!(
            "{LOG}[info] cgm {ANSWERED} result:bRet=false, nCode=0"
        )];
        assert!(!answered(&refused));
        assert!(!listed(&[
            "endfield: clients: Window 1 -> Default IME:".to_owned()
        ]));
    }

    /// Yellow is counted in thousandths of the screen.
    #[test]
    fn yellow_is_counted_in_thousandths() {
        let mut pixels = vec![0_u8; 3 * 999];
        pixels.extend([250_u8, 240, 10]);
        let screen = Image {
            width: 1000,
            height: 1,
            pixels,
        };
        assert_eq!(yellow_permille(&screen), 1);
    }

    /// With `gryphlink`, the script's own start of the launcher -- wineboot
    /// to `Launcher.exe` -- is replaced by the volume's `gryphlink`, and
    /// the rest (the wait for the window, the logs, the end) is kept.
    #[test]
    fn gryphlink_replaces_only_the_scripts_own_start() {
        let own = script(Wine::Proton, false);
        assert!(own.contains("wineboot -i") && own.contains("Launcher.exe"));
        assert!(!own.contains("gryphlink"));
        let everything = script(Wine::Proton, true);
        assert!(everything.contains("/data/usr/bin/gryphlink 2>&1"));
        assert!(!everything.contains("wineboot") && !everything.contains("Launcher.exe"));
        assert!(!everything.contains("cmd32"));
        for kept in [
            "nslookup launcher.gryphline.com",
            "title: GRYPHLINK",
            "echo \"endfield: window\"",
            "echo \"endfield: end\"",
        ] {
            assert!(everything.contains(kept), "{kept}");
        }
    }

    /// The `--everything` archive gets `/bin/gryphlink` and the entry, and
    /// no link a path already carried has.
    #[test]
    fn the_desktop_files_skip_what_is_carried() {
        let carried = crate::rustc::files(&[("lib/i386-linux-gnu", "/data/elsewhere")]);
        let files = desktop_files(&carried);
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert!(paths.contains(&"bin/gryphlink"));
        assert!(paths.contains(&ENTRY_PATH));
        assert!(paths.contains(&"etc/ssl/certs/ca-certificates.crt"));
        assert!(!paths.contains(&"lib/i386-linux-gnu"));
    }
}
