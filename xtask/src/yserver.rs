//! `test-yserver`: the X server Steam will draw through, started on Ferrix
//! with no display of its own and asked who it is (stage 22, the yserver
//! feasibility pass).
//!
//! yserver (github.com/joske/yserver, v1.6.0) is built for x86-64 glibc
//! against Debian 13's libraries, and runs here on them from a data volume, as
//! Chrome does. With no DRM card it starts headless and renders through
//! Vulkan on the CPU, Mesa's lavapipe; with no input device it starts only
//! when `YSERVER_ALLOW_NO_INPUT` says so, which a patch of Ferrix's adds.
//! [`RUN`], carried in the image, starts it on `:1`, runs `xdpyinfo` against
//! it, and prints the server's log.
//!
//! `scripts/fetch/fetch-yserver.sh` builds yserver from the customer's fork
//! and makes the volume (docs/YSERVER.md §3). It carries no `ferrix-root`
//! label, so the kernel mounts it at `/data`, under QEMU's `snapshot=on`.
//! The gate needs no network, but it attaches a volume, so it runs on demand
//! as `test-steamcmd` does and is not in the image row.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// glibc's x86-64 paths and the data files yserver and its libraries name
/// absolutely, each a link into the volume.
pub(crate) const LINKS: &[(&str, &str)] = &[
    ("lib64", "/data/usr/lib64"),
    ("lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("etc/fonts", "/data/etc/fonts"),
    ("usr/share/fonts", "/data/usr/share/fonts"),
    ("usr/share/fontconfig", "/data/usr/share/fontconfig"),
    ("usr/share/vulkan", "/data/usr/share/vulkan"),
    ("usr/share/X11", "/data/usr/share/X11"),
    ("usr/share/drirc.d", "/data/usr/share/drirc.d"),
];

/// Where [`RUN`] is in the image.
const RUN_PATH: &str = "bin/yserver-test";

/// yserver on `:1` with no card and no input device, then `xdpyinfo`
/// against it, then the server's log. Run by busybox's `sh`, whose `&` and
/// `$!` it uses; its status is `xdpyinfo`'s.
const RUN: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp XDG_RUNTIME_DIR=/tmp RUST_LOG=info
export YSERVER_ALLOW_NO_INPUT=1 YSERVER_ALLOW_SOFTWARE_VULKAN=1
/data/yserver/yserver :1 -nolisten tcp > /tmp/yserver.log 2>&1 &
server=$!
waited=0
while [ ! -S /tmp/.X11-unix/X1 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "yserver-gate: the socket was there after ${waited}s"
DISPLAY=:1 xdpyinfo > /tmp/xdpyinfo.txt 2>&1
status=$?
echo "yserver-gate: xdpyinfo exited $status"
cat /tmp/xdpyinfo.txt
kill $server
sleep 2
echo "yserver-gate: the server's log follows"
cat /tmp/yserver.log
exit $status
"#;

/// The script: [`RUN`], whose status says whether `xdpyinfo` reached the
/// server.
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/yserver/yserver ] || exit 3
busybox sh /bin/yserver-test || exit 4
exit 17
"#;

/// What the script exits with when `xdpyinfo` reached the server.
const STATUS: i32 = 17;

/// Memory for the guest: lavapipe and a 130 MiB server.
pub(crate) const MEMORY: u32 = 2048;

/// Where `scripts/fetch/fetch-yserver.sh` writes, unless
/// `FERRIX_YSERVER_VOLUME` names another directory.
///
/// # Errors
///
/// The volume has not been made.
pub(crate) fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_YSERVER_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("yserver")?,
    };
    let image = directory.join("yserver.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: scripts/fetch/fetch-yserver.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// `test-yserver` or `test-xwindow`, by the command's name.
///
/// # Errors
///
/// As [`test_yserver`] and `crate::compositor::test_xwindow`.
pub(crate) fn run(command: &str, args: &Args) -> Result<()> {
    if command == "test-xwindow" {
        crate::compositor::test_xwindow(args)
    } else {
        test_yserver(args)
    }
}

/// Boot a shell whose script starts yserver and runs `xdpyinfo` against it.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails, or
/// `xdpyinfo` did not reach the server.
pub(crate) fn test_yserver(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-yserver runs on x86-64")),
    };
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }

    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell starts yserver from the volume");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let mut files = rustc::files(LINKS);
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(RUN.as_bytes().to_vec()),
    });
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: running yserver on Ferrix with {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let exited = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    match exited {
        Some(status) if status == STATUS.to_string() => {
            println!("  {arch}: xdpyinfo reached yserver on Ferrix");
            Ok(())
        }
        Some("3") => Err(Error::new(format!(
            "{arch}: /data/yserver/yserver is not there: is the volume attached?"
        ))),
        Some("4") => Err(Error::new(format!(
            "{arch}: xdpyinfo did not reach yserver; the `yserver-gate:` lines and the \
             server's log say why"
        ))),
        other => Err(Error::new(format!(
            "{arch}: the yserver script ended with {other:?}"
        ))),
    }
}

/// Where [`XWINDOW_SCRIPT`] is in `test-xwindow`'s image.
pub(crate) const XWINDOW_PATH: &str = "etc/xwindow.sh";

/// `test-xwindow`'s script, started by the compositor. yserver runs as its
/// client on `:0`, and then:
///
/// * `xdpyinfo`, whose screen line says whether the root took the
///   compositor's screen for its own (docs/YSERVER.md, Y2);
/// * `xev`, whose window must become one of the compositor's (Y3), at the
///   size the compositor tiles it at (Y5). xev names its window but gives
///   it no class, so the script sets `WM_CLASS` once it is up, which is
///   also how a program renaming its window reaches the compositor;
/// * a second `xev`, made a transient of the first -- unmapped, given
///   `WM_TRANSIENT_FOR` and its own size back, and mapped again with
///   `xdotool` -- which the compositor must float as a dialog at that size
///   (Y5);
/// * `hyprctl clients`, and each window's size as X has it;
/// * [`XWINDOW_LOOK`], after which xtask looks for xev's window on the
///   screen while the script waits;
/// * the compositor's `closewindow` on xev, which must end it (Y5).
///
/// Every line of its own starts `xwindow:`, and it ends with
/// `xwindow: end` whatever happened.
pub(crate) const XWINDOW_SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp RUST_LOG=info
echo "xwindow: start"
YSERVER_BACKEND=wayland YSERVER_ALLOW_SOFTWARE_VULKAN=1 /data/yserver/yserver :0 -nolisten tcp \
    > /tmp/yserver.log 2>&1 &
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xwindow: the socket was there after ${waited}s"
export DISPLAY=:0
xdpyinfo > /tmp/xdpyinfo.txt 2>&1
echo "xwindow: xdpyinfo exited $?"
grep dimensions: /tmp/xdpyinfo.txt | sed 's/^/xwindow: /'
# Wait up to 30 s for the window named $1 to be viewable.
viewable() {
    waited=0
    until xwininfo -name "$1" 2>/dev/null | grep -q IsViewable || [ $waited -ge 30 ]; do
        sleep 1
        waited=$((waited + 1))
    done
    echo "xwindow: $1 was viewable after ${waited}s"
}
# The window id xwininfo gives for the window named $1.
id_of() {
    xwininfo -name "$1" | sed -n 's/.*Window id: \(0x[0-9a-f]*\).*/\1/p'
}
xev > /tmp/xev.txt 2>&1 &
xev=$!
viewable "Event Tester"
xprop -name "Event Tester" -f WM_CLASS 8s -set WM_CLASS Xev
xprop -name "Event Tester" WM_NAME WM_CLASS | sed 's/^/xwindow: /'
xev -name "Xev Dialog" > /tmp/xev-dialog.txt 2>&1 &
viewable "Xev Dialog"
dialog=$(id_of "Xev Dialog")
xdotool windowunmap --sync $dialog
xprop -id $dialog -f WM_TRANSIENT_FOR 32x -set WM_TRANSIENT_FOR $(id_of "Event Tester")
# Back to its own size, which the tile it had until now changed.
xdotool windowsize --sync $dialog 178 178
xdotool windowmap --sync $dialog
xprop -id $dialog WM_TRANSIENT_FOR | sed 's/^/xwindow: dialog /'
sleep 2
/bin/hyprctl clients | sed 's/^/xwindow: clients: /'
for name in "Event Tester" "Xev Dialog"; do
    size=$(xwininfo -name "$name" | sed -n 's/^ *Width: \([0-9]*\)$/\1/p;s/^ *Height: \([0-9]*\)$/\1/p' | tr '\n' ' ')
    echo "xwindow: X size of $name: $size"
done
echo "xwindow: look"
sleep 8
/bin/hyprctl dispatch closewindow 'title:^Event Tester$' | sed 's/^/xwindow: close: /'
waited=0
while kill -0 $xev 2>/dev/null && [ $waited -lt 10 ]; do
    sleep 1
    waited=$((waited + 1))
done
if kill -0 $xev 2>/dev/null; then
    echo "xwindow: xev is still running after ${waited}s"
else
    echo "xwindow: xev exited after ${waited}s"
fi
sed 's/^/xwindow: yserver: /' /tmp/yserver.log
echo "xwindow: end"
"#;

/// The line after which xtask looks at the screen, which the script then
/// leaves alone for eight seconds.
pub(crate) const XWINDOW_LOOK: &str = "xwindow: look";

/// The line [`XWINDOW_SCRIPT`] ends with.
pub(crate) const XWINDOW_END: &str = "xwindow: end";

/// Whether `test-xwindow`'s lines say the root window is the compositor's
/// screen: `xdpyinfo` reached the server and gave a size that is not the
/// headless 0×0, and the server said it took that size from the compositor.
pub(crate) fn judge_xwindow(arch: Arch, lines: &[String]) -> Result<()> {
    let dimensions = lines.iter().find_map(|line| {
        let (_, rest) = line.split_once("xwindow:")?;
        let size = rest
            .trim()
            .strip_prefix("dimensions:")?
            .split_whitespace()
            .next()?;
        let (width, height) = size.split_once('x')?;
        Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?))
    });
    let took = lines
        .iter()
        .find_map(|line| line.split_once("the root window is the compositor's screen, "))
        .map(|(_, size)| size.trim().to_owned());
    match (dimensions, took) {
        (Some((width, height)), Some(said)) if width > 0 && said == format!("{width}x{height}") => {
            println!("  {arch}: yserver's root is the compositor's screen, {width}x{height}");
            Ok(())
        }
        (dimensions, said) => Err(Error::new(format!(
            "{arch}: xdpyinfo said the screen is {dimensions:?}, and yserver said it took {said:?} \
             from the compositor; the `xwindow:` lines say more"
        ))),
    }
}

/// What xev calls its window (`WM_NAME`) and the class the script gives it
/// (`WM_CLASS`), which the compositor's window must have for its title and
/// app id.
const XEV_TITLE: &str = "Event Tester";
/// See [`XEV_TITLE`].
const XEV_CLASS: &str = "Xev";
/// The second xev's name, the one made a transient of the first.
const XEV_DIALOG: &str = "Xev Dialog";

/// xev's window as X draws it: white, with a white 50×50 subwindow at
/// (10, 10) inside a black border 4 pixels wide. The subwindow is drawn into
/// the top-level's own image only when the server redirects the top-level,
/// so finding it says the whole subtree reached the compositor.
const XEV_INNER_AT: usize = 10;
/// See [`XEV_INNER_AT`].
const XEV_INNER: usize = 50;
/// See [`XEV_INNER_AT`].
const XEV_BORDER: usize = 4;

/// Where on `screen` xev's subwindow's border has its top left corner, if
/// xev's window is on it.
pub(crate) fn find_xev(screen: &crate::display::Image) -> Option<(usize, usize)> {
    let pixel = |x: usize, y: usize| -> Option<&[u8]> {
        let at = y
            .checked_mul(screen.width)?
            .checked_add(x)?
            .checked_mul(3)?;
        screen.pixels.get(at..at.checked_add(3)?)
    };
    let white = |x: usize, y: usize| pixel(x, y).is_some_and(|rgb| rgb.iter().all(|&c| c >= 0xf0));
    let black = |x: usize, y: usize| pixel(x, y).is_some_and(|rgb| rgb.iter().all(|&c| c <= 0x10));
    let ring = XEV_INNER + 2 * XEV_BORDER;
    let is_xev = |x: usize, y: usize| {
        black(x, y)
            && white(x - 1, y)
            && white(x, y - 1)
            && (0..ring).all(|along| {
                (0..XEV_BORDER).all(|across| {
                    black(x + along, y + across)
                        && black(x + along, y + ring - 1 - across)
                        && black(x + across, y + along)
                        && black(x + ring - 1 - across, y + along)
                })
            })
            && (XEV_BORDER..ring - XEV_BORDER)
                .all(|row| (XEV_BORDER..ring - XEV_BORDER).all(|column| white(x + column, y + row)))
            && (1..=XEV_INNER_AT).all(|out| white(x - out, y) && white(x, y - out))
    };
    (XEV_INNER_AT..screen.height.saturating_sub(ring))
        .flat_map(|y| (XEV_INNER_AT..screen.width.saturating_sub(ring)).map(move |x| (x, y)))
        .find(|&(x, y)| is_xev(x, y))
}

/// Whether xev's window is one of the compositor's: `hyprctl clients` lists
/// it with xev's title and class, and the screen shows it, subwindow and all
/// (docs/YSERVER.md, Y3).
pub(crate) fn judge_xev(
    arch: Arch,
    lines: &[String],
    screen: Option<&crate::display::Image>,
    dump: &std::path::Path,
) -> Result<()> {
    let clients: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.split_once("xwindow: clients:"))
        .map(|(_, rest)| rest.trim())
        .collect();
    let has = |key: &str, value: &str| {
        clients.iter().any(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|rest| rest.trim() == value)
        })
    };
    if !has("title", XEV_TITLE) || !has("class", XEV_CLASS) {
        return Err(Error::new(format!(
            "{arch}: `hyprctl clients` has no window titled {XEV_TITLE:?} of class \
             {XEV_CLASS:?}; it said:\n{}",
            clients.join("\n")
        )));
    }
    let Some(screen) = screen else {
        return Err(Error::new(format!("{arch}: the boot took no picture")));
    };
    match find_xev(screen) {
        Some((x, y)) => {
            println!(
                "  {arch}: xev's window is the compositor's, {XEV_TITLE:?} of {XEV_CLASS:?}, \
                 its subwindow on the screen at ({x}, {y})"
            );
            Ok(())
        }
        None => Err(Error::new(format!(
            "{arch}: xev's window is not on the screen; the last picture is {}",
            dump.display()
        ))),
    }
}

/// `hyprctl clients`' windows among the script's lines: each one's title,
/// from its `Window … -> TITLE:` line, and the `key: value` lines under it.
fn listed_windows(lines: &[String]) -> Vec<(String, Vec<(String, String)>)> {
    let mut windows: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in lines {
        let Some((_, rest)) = line.split_once("xwindow: clients:") else {
            continue;
        };
        if let Some((_, title)) = rest.trim().split_once(" -> ") {
            windows.push((title.trim_end_matches(':').to_owned(), Vec::new()));
        } else if let (Some((key, value)), Some((_, fields))) =
            (rest.trim().split_once(':'), windows.last_mut())
        {
            fields.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    windows
}

/// Whether the compositor's sizes, floating and closing reached the X
/// windows (docs/YSERVER.md, Y5): xev's X size is the size hyprix tiled it
/// at, and its white background reaches the far corner of the tile on the
/// screen, so the area the grow exposed was painted; the transient second
/// xev floats at its own size; and hyprix's `closewindow` ended xev.
pub(crate) fn judge_windows(
    arch: Arch,
    lines: &[String],
    screen: Option<&crate::display::Image>,
) -> Result<()> {
    let windows = listed_windows(lines);
    let field = |title: &str, key: &str| {
        windows
            .iter()
            .find(|(named, _)| named == title)
            .and_then(|(_, fields)| fields.iter().find(|(name, _)| name == key))
            .map(|(_, value)| value.clone())
    };
    let x_size = |title: &str| {
        let prefix = format!("xwindow: X size of {title}:");
        lines.iter().find_map(|line| {
            let (_, size) = line.split_once(prefix.as_str())?;
            let mut numbers = size.split_whitespace();
            Some(format!("{},{}", numbers.next()?, numbers.next()?))
        })
    };
    let fail = |why: String| {
        Err(Error::new(format!(
            "{arch}: {why}; the `xwindow:` lines say more"
        )))
    };

    let tiled = field(XEV_TITLE, "size");
    if tiled.is_none() || tiled != x_size(XEV_TITLE) {
        return fail(format!(
            "hyprix tiled xev at {tiled:?}, and X has it at {:?}",
            x_size(XEV_TITLE)
        ));
    }
    // The tile's far corner, a few pixels in from hyprix's border.
    let corner = field(XEV_TITLE, "at")
        .zip(tiled.clone())
        .and_then(|(at, size)| {
            let (x, y) = at.split_once(',')?;
            let (width, height) = size.split_once(',')?;
            let x = x.trim().parse::<usize>().ok()? + width.trim().parse::<usize>().ok()?;
            let y = y.trim().parse::<usize>().ok()? + height.trim().parse::<usize>().ok()?;
            Some((x.checked_sub(5)?, y.checked_sub(5)?))
        });
    let white = |(x, y): (usize, usize)| {
        let screen = screen?;
        let at = (y.checked_mul(screen.width)?.checked_add(x)?).checked_mul(3)?;
        let rgb = screen.pixels.get(at..at.checked_add(3)?)?;
        Some(rgb.iter().all(|&c| c >= 0xf0))
    };
    if corner.and_then(white) != Some(true) {
        return fail(format!(
            "xev's tile is not white at its far corner {corner:?}: the area its grow exposed \
             was not painted with its background"
        ));
    }
    if field(XEV_DIALOG, "floating").as_deref() != Some("1") {
        return fail(format!(
            "the transient {XEV_DIALOG:?} is not floating: {:?}",
            field(XEV_DIALOG, "floating")
        ));
    }
    let floated = field(XEV_DIALOG, "size");
    if floated.as_deref() != Some("178,178") || floated != x_size(XEV_DIALOG) {
        return fail(format!(
            "the transient floats at {floated:?}, not at its own 178,178 ({:?} in X)",
            x_size(XEV_DIALOG)
        ));
    }
    if !lines
        .iter()
        .any(|line| line.contains("xwindow: xev exited after"))
    {
        return fail("hyprix's closewindow did not end xev".to_owned());
    }
    println!(
        "  {arch}: xev is X's size of its tile ({}), the transient floats at its own \
         178x178, and closing xev in hyprix ended it",
        tiled.unwrap_or_default()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Image;

    /// A screen of `width` × `height` grey with xev's window drawn at
    /// (`left`, `top`).
    fn screen_with_xev(width: usize, height: usize, left: usize, top: usize) -> Image {
        let mut pixels = vec![0x40; width * height * 3];
        let mut paint = |x: usize, y: usize, value: u8| {
            let at = (y * width + x) * 3;
            pixels[at..at + 3].fill(value);
        };
        for y in 0..178 {
            for x in 0..178 {
                paint(left + x, top + y, 0xff);
            }
        }
        let ring = XEV_INNER + 2 * XEV_BORDER;
        for y in 0..ring {
            for x in 0..ring {
                let edge = x < XEV_BORDER
                    || y < XEV_BORDER
                    || x >= ring - XEV_BORDER
                    || y >= ring - XEV_BORDER;
                if edge {
                    paint(left + XEV_INNER_AT + x, top + XEV_INNER_AT + y, 0);
                }
            }
        }
        Image {
            width,
            height,
            pixels,
        }
    }

    #[test]
    fn xev_is_found_where_it_is_drawn() {
        let screen = screen_with_xev(400, 300, 30, 40);
        assert_eq!(find_xev(&screen), Some((40, 50)));
    }

    #[test]
    fn a_white_window_without_the_subwindow_is_not_xev() {
        let mut screen = screen_with_xev(400, 300, 30, 40);
        for pixel in screen.pixels.chunks_exact_mut(3) {
            if pixel == [0, 0, 0] {
                pixel.fill(0xff);
            }
        }
        assert_eq!(find_xev(&screen), None);
    }

    #[test]
    fn the_windows_follow_the_compositor() {
        let mut lines: Vec<String> = [
            "Window 1 -> Event Tester:",
            "\tat: 21,21",
            "\tsize: 982,726",
            "\tfloating: 0",
            "Window 2 -> Xev Dialog:",
            "\tsize: 178,178",
            "\tfloating: 1",
        ]
        .iter()
        .map(|line| format!("xwindow: clients: {line}"))
        .collect();
        lines.push("xwindow: X size of Event Tester: 982 726 ".to_owned());
        lines.push("xwindow: X size of Xev Dialog: 178 178 ".to_owned());
        let mut closed = lines.clone();
        closed.push("xwindow: xev exited after 1s".to_owned());
        let white = Image {
            width: 1024,
            height: 768,
            pixels: vec![0xff; 1024 * 768 * 3],
        };
        let screen = Some(&white);
        assert!(judge_windows(Arch::X86_64, &closed, screen).is_ok());
        assert!(
            judge_windows(Arch::X86_64, &lines, screen).is_err(),
            "xev must have exited"
        );
        let black = Image {
            width: 1024,
            height: 768,
            pixels: vec![0; 1024 * 768 * 3],
        };
        assert!(
            judge_windows(Arch::X86_64, &closed, Some(&black)).is_err(),
            "the grown area must be painted"
        );
        let mut untiled = closed.clone();
        untiled.retain(|line| !line.contains("X size of Event Tester"));
        untiled.push("xwindow: X size of Event Tester: 178 178 ".to_owned());
        assert!(
            judge_windows(Arch::X86_64, &untiled, screen).is_err(),
            "xev must take the tile's size"
        );
        let tiled_dialog: Vec<String> = closed
            .iter()
            .map(|line| line.replace("floating: 1", "floating: 0"))
            .collect();
        assert!(
            judge_windows(Arch::X86_64, &tiled_dialog, screen).is_err(),
            "the dialog must float"
        );
    }

    #[test]
    fn the_clients_must_name_xev() {
        let dump = std::path::Path::new("xwindow.ppm");
        let screen = screen_with_xev(400, 300, 30, 40);
        let listed = |lines: &[&str]| -> Vec<String> {
            lines
                .iter()
                .map(|line| format!("xwindow: clients: {line}"))
                .collect()
        };
        let good = listed(&[
            "Window 1 -> Event Tester:",
            "\tclass: Xev",
            "\ttitle: Event Tester",
        ]);
        assert!(judge_xev(Arch::X86_64, &good, Some(&screen), dump).is_ok());
        let untitled = listed(&["\tclass: Xev", "\ttitle: "]);
        assert!(judge_xev(Arch::X86_64, &untitled, Some(&screen), dump).is_err());
        assert!(judge_xev(Arch::X86_64, &good, None, dump).is_err());
    }
}
