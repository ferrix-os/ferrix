//! `test-endfield-launcher`: Arknights: Endfield's Windows launcher
//! (GRYPHLINK) in Debian's Wine on hyprix through yserver, on the network
//! (stage 23, step 4; docs/WINE.md §7).
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
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
echo "endfield: start"
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
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "endfield: wineboot exited $?"
/usr/lib/wine/wineserver64 -w
/usr/lib/wine/wine64 reg add 'HKCU\Software\Wine\X11 Driver' /v UseXVidMode /d N /f > /tmp/wine/reg.log 2>&1
echo "endfield: reg exited $?"
/usr/lib/wine/wineserver64 -w
ln -s /data/GRYPHLINK /tmp/wine/prefix/drive_c/GRYPHLINK
cd /tmp/wine/prefix/drive_c/GRYPHLINK
sed 's/^/endfield: resolv.conf: /' /etc/resolv.conf
/usr/lib/wine/wine64 cmd /c ping -n 1 launcher.gryphline.com 2>&1 | head -n 3 | sed 's/^/endfield: ping: /'
WINEDEBUG="$WINEDEBUG,trace+winsock" /usr/lib/wine/wine64 'C:\GRYPHLINK\Launcher.exe' 2>&1 \
    | grep -E 'winsock|err:' > /tmp/wine/launcher.log &
waited=0
until /bin/hyprctl clients 2>/dev/null | grep -q "title: GRYPHLINK" || [ $waited -ge 300 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "endfield: the launcher's window was viewable after ${waited}s"
xwininfo -name "GRYPHLINK" 2>&1 | grep -E "Width|Height|Map State" | sed 's/^/endfield: launcher: /'
/bin/hyprctl clients | sed 's/^/endfield: clients: /'
echo "endfield: window"
sleep 240
logs=$(echo /tmp/wine/prefix/drive_c/users/*/AppData/LocalLow/Gryphline/*/logs)
grep -E "error|warning|OnQuery_ProtocolVer|OnGetNvGPUConfigResult" "$logs/games.log" | cut -c1-300 | sed 's/^/endfield: games.log: /'
grep -v ':fixme:' /tmp/wine/launcher.log | grep -E 'err:|getaddrinfo' | sort | uniq -c | sort -rn | head -n 40 | cut -c1-300 | sed 's/^/endfield: wine: /'
thread=$(grep -m1 -E 'WSAAsyncSelect socket' /tmp/wine/launcher.log | cut -d: -f1)
echo "endfield: the first thread to use WSAAsyncSelect: $thread"
grep "^$thread:" /tmp/wine/launcher.log | grep -vE 'getaddrinfo|sock_ioctl' | head -n 200 | cut -c1-240 | sed 's/^/endfield: thread: /'
/bin/hyprctl clients | sed 's/^/endfield: clients: /'
ps | grep -c '[Q]tWebEngineProcess' | sed 's/^/endfield: render processes: /'
/usr/lib/wine/wineserver64 -k
echo "endfield: end"
"#;

/// The launcher version `fetch-endfield-launcher.sh` pins.
fn pinned() -> &'static str {
    FETCH
        .lines()
        .find_map(|line| line.strip_prefix("LAUNCHER_VERSION="))
        .unwrap_or_default()
}

/// The volume `tools/common/fetch/fetch-endfield-launcher.sh` makes.
///
/// # Errors
///
/// It has not been made, or was made with another launcher or another
/// yserver than the ones the scripts pin.
pub(crate) fn volume() -> Result<PathBuf> {
    let directory = match std::env::var_os("FERRIX_ENDFIELD_VOLUME") {
        Some(directory) => PathBuf::from(directory),
        None => paths::volume_directory("endfield-launcher")?,
    };
    let image = directory.join("endfield.img");
    let again = "tools/common/fetch/fetch-endfield-launcher.sh makes it again";
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-endfield-launcher.sh makes it",
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

/// Boot hyprix with yserver, the network and [`SCRIPT`], and require the
/// launcher's window, its main button and its server's answer.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails,
/// or the window, the button or the answer did not come.
pub(crate) fn test_endfield_launcher(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-endfield-launcher runs on x86-64")),
    };
    let mut args = args.clone();
    args.gl = false;
    args.net = true;
    args.data_image = Some(volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-endfield-launcher needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
    })?;
    let programs = Programs::build(arch)?;
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.extend(crate::rustc::files(crate::wine::LINKS));
    ports.push(File {
        path: SCRIPT_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(SCRIPT.as_bytes().to_vec()),
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
    println!(
        "  {arch}: GRYPHLINK {} in Wine on hyprix through yserver, on the network, {} MiB",
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
        let ended = watching.read_more(Instant::now() + DRAWN + Duration::from_secs(120), reached(END))?;
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
    judge(arch, &said, yellow, &dump)
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
        let refused = vec![format!("{LOG}[info] cgm {ANSWERED} result:bRet=false, nCode=0")];
        assert!(!answered(&refused));
        assert!(!listed(&["endfield: clients: Window 1 -> Default IME:".to_owned()]));
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
}
