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
//! `tools/common/fetch/fetch-yserver.sh` builds yserver from the customer's fork
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
    ("etc/X11", "/data/etc/X11"),
    ("usr/share/fonts", "/data/usr/share/fonts"),
    ("usr/share/fontconfig", "/data/usr/share/fontconfig"),
    ("usr/share/vulkan", "/data/usr/share/vulkan"),
    ("usr/share/X11", "/data/usr/share/X11"),
    ("usr/share/drirc.d", "/data/usr/share/drirc.d"),
];

/// Where [`DESKTOP_SCRIPT`] is in the `--everything` desktop's image.
const DESKTOP_PATH: &str = "etc/yserver.sh";

/// yserver as the `--everything` desktop's X server on `:0`, a client of
/// hyprix's (docs/YSERVER.md, Y7), started by the compositor's `exec-once`.
///
/// It runs on the volume's own glibc through its loader, named here, rather
/// than through `/lib64`: that is ferrousli's loader on a desktop whose Chrome
/// runs on ferrousli, and yserver is built against Debian's glibc. The
/// `--library-path` takes the place of the `LD_LIBRARY_PATH` that desktop
/// gives its clients for ferrousli. Its log is `/tmp/yserver.log`, at the
/// `RUST_LOG` the compositor's configuration gives, or `info`.
///
/// It renders on lavapipe even where the desktop has Venus. Every frame
/// yserver hands hyprix is read back to the CPU, and on Venus that read,
/// and every client image written, crosses memory Ferrix maps uncached
/// (the device calls it write-combining): Steam's maximized window took
/// 22 ms a frame to read back and its uploads half the server's time. On
/// lavapipe both are copies in RAM. Venus pays once hyprix takes dmabufs.
const DESKTOP_SCRIPT: &str = r#"export YSERVER_BACKEND=wayland YSERVER_ALLOW_SOFTWARE_VULKAN=1
export RUST_LOG="${RUST_LOG:-info}" VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/lvp_icd.json
unset LD_LIBRARY_PATH
exec /data/usr/lib64/ld-linux-x86-64.so.2 --library-path /data/usr/lib/x86_64-linux-gnu \
    /data/yserver/yserver :0 -nolisten tcp > /tmp/yserver.log 2>&1
"#;

/// What the `--everything` desktop's configuration gains for yserver: the
/// server started with the compositor, `DISPLAY` for every program started
/// from it and its terminals, and Steam's small windows floated. Steam's main
/// window tiles well; its friends list, settings and offers are better
/// floated, as Hyprland users float them (docs/YSERVER.md §5).
pub(crate) fn desktop_config() -> String {
    format!(
        "# Added by `cargo xtask run-compositor --everything`: the X server, yserver.\n\
         env = DISPLAY,:0\n\
         exec-once = /bin/busybox sh /{DESKTOP_PATH}\n\
         windowrule = float, match:class ^([Ss]team)$, match:title ^(Friends List|Steam Settings|Special Offers)$\n"
    )
}

/// [`DESKTOP_SCRIPT`] on the RTX 3060 (`run-compositor --nvidia`), whose
/// volume has no lavapipe: yserver renders on NVIDIA's Vulkan, and with a
/// GPU and its render node it offers DRI3 and Present, through which
/// NVIDIA's libGLX and its Vulkan X11 surfaces hand it finished frames as
/// dmabufs (docs/NVIDIA.md §4.6). [`DEV_SERVER_PATH`], when the image
/// carries it, runs in place of the volume's server.
const NVIDIA_DESKTOP_SCRIPT: &str = r#"export YSERVER_BACKEND=wayland
export RUST_LOG="${RUST_LOG:-info}" VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/nvidia_icd.json
unset LD_LIBRARY_PATH
server=/data/yserver/yserver
[ -x /yserver-dev/yserver ] && server=/yserver-dev/yserver
exec /data/usr/lib64/ld-linux-x86-64.so.2 --library-path /data/usr/lib/x86_64-linux-gnu \
    $server :0 -nolisten tcp > /tmp/yserver.log 2>&1
"#;

/// Where `FERRIX_YSERVER_DEV`'s server is in the image.
const DEV_SERVER_PATH: &str = "yserver-dev/yserver";

/// The server `FERRIX_YSERVER_DEV` names on the host, a yserver built from a
/// fork branch (stripped: the image is in memory), carried to
/// [`DEV_SERVER_PATH`] for [`NVIDIA_DESKTOP_SCRIPT`] to start in place of
/// the volume's.
fn dev_server() -> Option<crate::ports::File> {
    let path = std::env::var_os("FERRIX_YSERVER_DEV")?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            println!(
                "  yserver: {} in place of the volume's (FERRIX_YSERVER_DEV)",
                std::path::Path::new(&path).display()
            );
            Some(crate::ports::File {
                path: DEV_SERVER_PATH.to_owned(),
                mode: 0o755,
                content: crate::ports::Content::Bytes(bytes),
            })
        }
        Err(error) => {
            println!("  yserver: FERRIX_YSERVER_DEV unreadable ({error}); the volume's server");
            None
        }
    }
}

/// What `run-compositor --everything` adds to the archive for yserver:
/// [`DESKTOP_SCRIPT`], and [`LINKS`] less any path `carried` already has --
/// Chrome's and the compiler's name the same Debian's paths.
pub(crate) fn desktop_files(carried: &[crate::ports::File]) -> Vec<crate::ports::File> {
    desktop_files_on(carried, false)
}

/// [`desktop_files`], with [`NVIDIA_DESKTOP_SCRIPT`] on the RTX 3060.
pub(crate) fn desktop_files_on(
    carried: &[crate::ports::File],
    nvidia: bool,
) -> Vec<crate::ports::File> {
    let taken = |path: &str| {
        carried.iter().any(|file| {
            file.path == path
                || file
                    .path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let links: Vec<(&str, &str)> = LINKS
        .iter()
        .copied()
        .filter(|(path, _)| !taken(path))
        .collect();
    let mut files = rustc::files(&links);
    let script = if nvidia {
        files.extend(dev_server());
        NVIDIA_DESKTOP_SCRIPT
    } else {
        DESKTOP_SCRIPT
    };
    files.push(crate::ports::File {
        path: DESKTOP_PATH.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(script.as_bytes().to_vec()),
    });
    files
}

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

/// The script that makes the volume, for the commit of the fork it pins.
const FETCH: &str = include_str!("../../fetch/fetch-yserver.sh");

/// The commit of the fork `tools/common/fetch/fetch-yserver.sh` builds, which
/// it also writes beside the image it made from it.
fn pinned() -> &'static str {
    FETCH
        .lines()
        .find_map(|line| line.strip_prefix("YSERVER_COMMIT="))
        .unwrap_or_default()
}

/// Where `tools/common/fetch/fetch-yserver.sh` writes, unless
/// `FERRIX_YSERVER_VOLUME` names another directory.
///
/// The image must hold the yserver the script pins: one made before the pin
/// moved runs, and answers `xdpyinfo`, but lacks what later commits of the
/// fork gave it, such as X input from hyprix's seat and the clipboard, so
/// `test-xwindow` failed on it for want of input rather than for a fault.
///
/// # Errors
///
/// The volume has not been made, or was made from another commit.
pub(crate) fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_YSERVER_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("yserver")?,
    };
    let image = directory.join("yserver.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-yserver.sh makes it",
            image.display()
        )));
    }
    let stamp = directory.join("yserver.commit");
    let built = std::fs::read_to_string(&stamp).unwrap_or_default();
    let built = match built.trim() {
        "" => format!(
            "does not say which yserver it holds ({} is not there)",
            stamp.display()
        ),
        commit if commit == pinned() => return Ok(image),
        commit => format!("holds yserver {commit}"),
    };
    Err(Error::new(format!(
        "{} {built}: tools/common/fetch/fetch-yserver.sh makes it again from {}, the commit it pins",
        image.display(),
        pinned()
    )))
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

/// `test-xwindow`'s script, started by the compositor beside yserver, which
/// the compositor starts on `:0` as the `--everything` desktop does
/// ([`desktop_config`], Y7). It waits for the server's socket, says the
/// `DISPLAY` the compositor gave it, then runs `xdpyinfo`, whose screen line
/// says whether the root took the compositor's screen for its own
/// (docs/YSERVER.md, Y2), then `xprop`'s view of the window manager EWMH
/// clients find (`xwindow: wm:` lines), then `xev`, whose
/// window must become one of the compositor's (Y3): `hyprctl clients` lists
/// it, and xtask looks for it on the screen once the script has ended, while
/// `xev` still runs. xev names its window but gives it no class, so the
/// script sets `WM_CLASS` once it is up, which is also how a program
/// renaming its window reaches the compositor.
///
/// Then it says `xwindow: input` and waits for xtask to point at xev's
/// window, click, turn the wheel and type `a` and `z` there (Y4), until xev
/// has reported the `z`. `xwininfo` then asks for a window to be picked,
/// which grabs the pointer with a cross for a cursor, and it says
/// `xwindow: pick` for xtask to click xev's window again. xev's and
/// xwininfo's reports come out as `xwindow: xev:` and `xwindow: pick:`
/// lines, and the server's log, whose seat module says each cursor it gives
/// the compositor, as `xwindow: yserver:` lines.
///
/// Before those lines come Y5's: a second `xev`, made a transient of the
/// first -- unmapped, given `WM_TRANSIENT_FOR` and its own size back, and
/// mapped again with `xdotool` -- which the compositor must float as a
/// dialog at that size; `hyprctl clients` again, and each window's size as
/// X has it; a drag of the floating dialog by its own request, between
/// [`XWINDOW_DRAG`] and [`XWINDOW_DRAGGED`]: xtask presses on it and holds,
/// `/bin/xmoveresize` sends `_NET_WM_MOVERESIZE` as Steam's login window
/// does, xtask moves the pointer and lets go, and the dialog is listed again
/// and closed, for its xev's count of presses and releases; the
/// compositor's `closewindow` on xev, which must end it; and
/// `xfontsel` with its field menu held open through XTEST, between
/// [`XWINDOW_MENU`] and [`XWINDOW_MENU_OPEN`], at each of which xtask looks
/// at the screen.
/// Last, the clipboard both ways for both selections (Y6): `xclip` copies
/// and `/bin/clip` pastes, then `/bin/clip` copies and `xclip` pastes.
/// Every line of its own starts `xwindow:`, and it ends with `xwindow: end`
/// whatever happened.
pub(crate) const XWINDOW_SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp
echo "xwindow: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xwindow: the socket was there after ${waited}s"
echo "xwindow: DISPLAY is $DISPLAY"
xdpyinfo > /tmp/xdpyinfo.txt 2>&1
echo "xwindow: xdpyinfo exited $?"
grep dimensions: /tmp/xdpyinfo.txt | sed 's/^/xwindow: /'
# The window manager, as EWMH clients such as Steam find it: the root names
# a window, which names itself and carries the manager's name.
xprop -root _NET_SUPPORTING_WM_CHECK _NET_SUPPORTED | sed 's/^/xwindow: wm: root /'
wm=$(xprop -root _NET_SUPPORTING_WM_CHECK | sed -n 's/.*window id # \(0x[0-9a-f]*\).*/\1/p')
[ -n "$wm" ] && xprop -id $wm _NET_SUPPORTING_WM_CHECK _NET_WM_NAME | sed 's/^/xwindow: wm: check /'
xev > /tmp/xev.txt 2>&1 &
waited=0
until xwininfo -name "Event Tester" 2>/dev/null | grep -q IsViewable || [ $waited -ge 30 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xwindow: xev's window was viewable after ${waited}s"
xprop -name "Event Tester" -f WM_CLASS 8s -set WM_CLASS Xev
xprop -name "Event Tester" WM_NAME WM_CLASS | sed 's/^/xwindow: /'
sleep 2
/bin/hyprctl clients | sed 's/^/xwindow: clients: /'
echo "xwindow: input"
waited=0
until [ "$(grep -c 'keysym 0x7a, z' /tmp/xev.txt)" -ge 2 ] || [ $waited -ge 60 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xwindow: xev had the keys after ${waited}s"
xwininfo > /tmp/pick.txt 2>&1 &
pick=$!
sleep 2
echo "xwindow: pick"
waited=0
while kill -0 $pick 2>/dev/null && [ $waited -lt 30 ]; do
    sleep 1
    waited=$((waited + 1))
done
kill $pick 2>/dev/null
sed 's/^/xwindow: pick: /' /tmp/pick.txt
# A dialog: a second xev, made a transient of the first -- unmapped, given
# WM_TRANSIENT_FOR and its own size back, and mapped again -- which the
# compositor must float at that size. After the input, whose keys a newly
# mapped window would take.
# The window id xwininfo gives for the window named $1.
id_of() {
    xwininfo -name "$1" | sed -n 's/.*Window id: \(0x[0-9a-f]*\).*/\1/p'
}
xev -name "Xev Dialog" > /tmp/xev-dialog.txt 2>&1 &
waited=0
until xwininfo -name "Xev Dialog" 2>/dev/null | grep -q IsViewable || [ $waited -ge 30 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xwindow: the dialog was viewable after ${waited}s"
dialog=$(id_of "Xev Dialog")
xdotool windowunmap --sync $dialog
xprop -id $dialog -f WM_TRANSIENT_FOR 32x -set WM_TRANSIENT_FOR $(id_of "Event Tester")
xdotool windowsize --sync $dialog 178 178
xdotool windowmap --sync $dialog
xprop -id $dialog WM_TRANSIENT_FOR | sed 's/^/xwindow: dialog /'
sleep 2
/bin/hyprctl clients | sed 's/^/xwindow: clients: /'
for name in "Event Tester" "Xev Dialog"; do
    size=$(xwininfo -name "$name" | sed -n 's/^ *Width: \([0-9]*\)$/\1/p;s/^ *Height: \([0-9]*\)$/\1/p' | tr '\n' ' ')
    echo "xwindow: X size of $name: $size"
done
# A drag by the dialog's own title bar, as Steam's login window asks for one:
# xtask presses on the dialog and holds the button, xmoveresize hands the
# press to the window manager with _NET_WM_MOVERESIZE, and xtask moves the
# pointer and lets go. The dialog must have followed, and its client must
# have had the release.
echo "xwindow: drag"
sleep 3
/bin/xmoveresize $dialog 2>&1 | sed 's/^/xwindow: drag: /'
echo "xwindow: drag asked"
sleep 4
echo "xwindow: dragged"
/bin/hyprctl clients | sed 's/^/xwindow: clients: /'
# xev writes its report when it exits, so the dialog is closed to count.
/bin/hyprctl dispatch closewindow 'title:^Xev Dialog$' > /dev/null
waited=0
while xwininfo -name "Xev Dialog" > /dev/null 2>&1 && [ $waited -lt 10 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xwindow: dialog presses: $(grep -c ButtonPress /tmp/xev-dialog.txt)"
echo "xwindow: dialog releases: $(grep -c ButtonRelease /tmp/xev-dialog.txt)"
# The compositor closes xev, which xev hears as WM_DELETE_WINDOW.
/bin/hyprctl dispatch closewindow 'title:^Event Tester$' | sed 's/^/xwindow: close: /'
waited=0
while xwininfo -name "Event Tester" > /dev/null 2>&1 && [ $waited -lt 10 ]; do
    sleep 1
    waited=$((waited + 1))
done
if xwininfo -name "Event Tester" > /dev/null 2>&1; then
    echo "xwindow: xev is still running after ${waited}s"
else
    echo "xwindow: xev exited after ${waited}s"
fi
# A menu (Y5b): xfontsel's field menu, an override-redirect window, which
# the compositor must show as a popup where X put it. xdotool holds button 1
# down on the menu's button through XTEST, as a hand would, since the menu
# is up only while the button is; xtask looks before and while it is.
xfontsel > /tmp/xfontsel.txt 2>&1 &
waited=0
until xwininfo -name xfontsel 2>/dev/null | grep -q IsViewable || [ $waited -ge 30 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xwindow: xfontsel was viewable after ${waited}s"
sleep 2
/bin/hyprctl clients | sed 's/^/xwindow: clients: /'
echo "xwindow: menu"
sleep 3
xdotool mousemove --window $(id_of xfontsel) 15 40 mousedown 1
sleep 2
xwininfo -root -children | grep -E '^ +0x[0-9a-f]+ \(has no name\)' | grep -v ' 1x1+' | sed 's/^/xwindow: menu window: /'
echo "xwindow: menu open"
sleep 4
xdotool mouseup 1
sleep 1
# The clipboard (Y6), both selections: an X client's copy pasted by a
# Wayland one, then the reverse. xclip -i serves the selection from the
# background until the Wayland copy takes it.
for which in clipboard primary; do
    flag=""
    [ $which = primary ] && flag=--primary
    printf 'x-%s-to-wayland' $which | xclip -selection $which -i
    sleep 1
    /bin/clip $flag paste 2>&1 | sed "s/^/xwindow: clip $which: /"
    /bin/clip $flag copy wayland-$which-to-x > /dev/null 2>&1 &
    sleep 1
    echo "xwindow: xclip $which: $(xclip -selection $which -o 2>&1)"
done
sed 's/^/xwindow: xev: /' /tmp/xev.txt
sed 's/^/xwindow: yserver: /' /tmp/yserver.log
echo "xwindow: end"
"#;

/// The line [`XWINDOW_SCRIPT`] ends with.
pub(crate) const XWINDOW_END: &str = "xwindow: end";

/// The line [`XWINDOW_SCRIPT`] says when xev's window is up and it waits
/// for the input.
pub(crate) const XWINDOW_INPUT: &str = "xwindow: input";

/// The line before xfontsel's menu is opened, and the one while it is open:
/// the script waits three and four seconds after each for xtask to look.
pub(crate) const XWINDOW_MENU: &str = "xwindow: menu";
/// See [`XWINDOW_MENU`].
pub(crate) const XWINDOW_MENU_OPEN: &str = "xwindow: menu open";

/// The line [`XWINDOW_SCRIPT`] says before it asks for the dialog to be
/// dragged, the one after, and the one once the drag is over.
const XWINDOW_DRAG: &str = "xwindow: drag";
/// See [`XWINDOW_DRAG`].
const XWINDOW_DRAG_ASKED: &str = "xwindow: drag asked";
/// See [`XWINDOW_DRAG`].
const XWINDOW_DRAGGED: &str = "xwindow: dragged";

/// How far [`drive_drag`] moves the pointer once the dialog asked to be
/// dragged.
const DRAG_BY: (i64, i64) = (120, 80);

/// Where `p` is on a screen `across` wide, as QEMU's tablet has it.
fn tablet(at: i64, across: usize) -> i32 {
    let across = i64::try_from(across.max(1)).unwrap_or(1);
    i32::try_from(at * 0x7FFF / across).unwrap_or(0)
}

/// A window's `at` or `size` from `hyprctl clients`, `x,y`.
fn pair(text: &str) -> Option<(i64, i64)> {
    let (x, y) = text.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// The dialog's `at` and `size` in the last listing among `lines`.
fn dialog_place(lines: &[String]) -> Option<((i64, i64), (i64, i64))> {
    let windows = listed_windows(lines);
    let (_, fields) = windows
        .iter()
        .rev()
        .find(|(title, _)| title == XEV_DIALOG)?;
    let field = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .and_then(|(_, value)| pair(value))
    };
    Some((field("at")?, field("size")?))
}

/// Drag the floating dialog by its own request, as a hand on Steam's title
/// bar would (`XWINDOW_SCRIPT`'s drag): at [`XWINDOW_DRAG`], press the left
/// button in the dialog's middle and hold it; once the script has sent
/// `_NET_WM_MOVERESIZE` ([`XWINDOW_DRAG_ASKED`]), move the pointer by
/// [`DRAG_BY`] in steps, and let go. `size` is the screen's.
///
/// # Errors
///
/// A QMP command that fails.
pub(crate) fn drive_drag(
    qmp: &mut crate::display::Qmp,
    watching: &mut qemu::Watching<'_>,
    size: (usize, usize),
) -> Result<()> {
    use crate::compositor::{absolute, button_event};
    use std::time::{Duration, Instant};

    let pause = |millis| std::thread::sleep(Duration::from_millis(millis));
    let said = |marker: &'static str| {
        move |lines: &[String]| {
            lines
                .iter()
                .any(|line| line.trim_end().ends_with(marker) || line.contains(XWINDOW_END))
        }
    };
    if !watching.read_more(Instant::now() + Duration::from_secs(90), said(XWINDOW_DRAG))? {
        return Ok(());
    }
    let Some(((x, y), (width, height))) = dialog_place(watching.after()) else {
        println!("  the dialog was not listed before its drag; nothing was pressed");
        return Ok(());
    };
    let (x, y) = (x + width / 2, y + height / 2);
    let to = |qmp: &mut crate::display::Qmp, (x, y): (i64, i64)| {
        qmp.input_send_event(&[
            absolute("x", tablet(x, size.0)),
            absolute("y", tablet(y, size.1)),
        ])
    };
    to(qmp, (x, y))?;
    pause(300);
    qmp.input_send_event(&[button_event("left", true)])?;
    let asked = watching.read_more(
        Instant::now() + Duration::from_secs(30),
        said(XWINDOW_DRAG_ASKED),
    );
    if asked.is_ok() {
        pause(300);
        for step in 1..=8 {
            to(qmp, (x + DRAG_BY.0 * step / 8, y + DRAG_BY.1 * step / 8))?;
            pause(50);
        }
        pause(300);
    }
    // Let go even when the script never asked, or the button stays down.
    qmp.input_send_event(&[button_event("left", false)])?;
    asked.map(|_| ())
}

/// Whether the dialog followed the pointer by its own request, and its
/// client had the release that ended the drag: its place in the listing
/// after [`XWINDOW_DRAGGED`] is its place before plus [`DRAG_BY`], give or
/// take the tablet's rounding.
pub(crate) fn judge_drag(arch: Arch, lines: &[String]) -> Result<()> {
    let fail = |why: String| {
        Err(Error::new(format!(
            "{arch}: {why}; the `xwindow:` lines say more"
        )))
    };
    let Some(split) = lines
        .iter()
        .position(|line| line.trim_end().ends_with(XWINDOW_DRAGGED))
    else {
        return fail("the script never dragged the dialog".to_owned());
    };
    let (before, after) = lines.split_at(split);
    let (Some((from, _)), Some((to, _))) = (dialog_place(before), dialog_place(after)) else {
        return fail("the dialog is not listed both before and after the drag".to_owned());
    };
    let moved = (to.0 - from.0, to.1 - from.1);
    if (moved.0 - DRAG_BY.0).abs() > 2 || (moved.1 - DRAG_BY.1).abs() > 2 {
        return fail(format!(
            "the dialog moved by {moved:?} from {from:?}, where the pointer dragged it by \
             {DRAG_BY:?}"
        ));
    }
    let released = lines.iter().find_map(|line| {
        let (_, count) = line.split_once("xwindow: dialog releases: ")?;
        count.trim().parse::<u32>().ok()
    });
    if released.unwrap_or(0) == 0 {
        return fail(format!(
            "the dialog's client never had the release that ended its drag ({released:?})"
        ));
    }
    println!(
        "  {arch}: the floating dialog asked for its own drag and followed the pointer by \
         {moved:?}, and its client had the release"
    );
    Ok(())
}

/// The line it says when `xwininfo` waits for a window to be picked.
const XWINDOW_PICK: &str = "xwindow: pick";

/// Where in xev's window the pointer is put, in the window's own
/// coordinates: right and below the subwindow, on the window itself.
const XEV_POINT: (usize, usize) = (120, 120);

/// Whether `test-xwindow`'s lines say the desktop's X server is up for its
/// clients and its root window is the compositor's screen: the compositor
/// gave the script `DISPLAY` `:0` ([`desktop_config`]), `xdpyinfo` reached
/// the server and gave a size that is not the headless 0×0, and the server
/// said it took that size from the compositor.
pub(crate) fn judge_xwindow(arch: Arch, lines: &[String]) -> Result<()> {
    let display = lines
        .iter()
        .find_map(|line| line.split_once("xwindow: DISPLAY is"))
        .map(|(_, display)| display.trim());
    if display != Some(":0") {
        return Err(Error::new(format!(
            "{arch}: the compositor gave its clients DISPLAY {display:?}, not :0; \
             the desktop's `env = DISPLAY` did not reach them"
        )));
    }
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

/// The window manager's name the server announces for the compositor.
const WINDOW_MANAGER: &str = "hyprix";

/// Whether `test-xwindow`'s `xwindow: wm:` lines say the server announces
/// a window manager as EWMH clients look for one (Steam's "X Window
/// Manager"): the root's `_NET_SUPPORTING_WM_CHECK` names a window, that
/// window's names itself, and its `_NET_WM_NAME` is [`WINDOW_MANAGER`].
pub(crate) fn judge_window_manager(arch: Arch, lines: &[String]) -> Result<()> {
    let check = |prefix: &str| {
        lines.iter().find_map(|line| {
            let (_, rest) = line.split_once(prefix)?;
            let (_, id) = rest.split_once("_NET_SUPPORTING_WM_CHECK(WINDOW): window id # ")?;
            Some(id.trim().to_owned())
        })
    };
    let name = lines.iter().find_map(|line| {
        let (_, rest) = line.split_once("xwindow: wm: check ")?;
        let (_, name) = rest.split_once("_NET_WM_NAME(UTF8_STRING) = ")?;
        Some(name.trim().trim_matches('"').to_owned())
    });
    match (
        check("xwindow: wm: root "),
        check("xwindow: wm: check "),
        name,
    ) {
        (Some(root), Some(own), Some(name)) if root == own && name == WINDOW_MANAGER => {
            println!("  {arch}: X clients find the window manager {name}, window {own}");
            Ok(())
        }
        (root, own, name) => Err(Error::new(format!(
            "{arch}: the root names window manager window {root:?}, which names {own:?} and \
             is called {name:?}, not {WINDOW_MANAGER:?}; the `xwindow: wm:` lines say more"
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

/// Put input into xev's window, whose subwindow's ring [`find_xev`] found
/// at `ring` on a screen of `size`: the pointer to [`XEV_POINT`], a left
/// click, a wheel click down, and the keys `a` and `z`, which the script
/// waits for. Then, when the script says `xwininfo` waits, click there
/// again.
///
/// # Errors
///
/// What QMP says.
pub(crate) fn drive_xev(
    qmp: &mut crate::display::Qmp,
    watching: &mut qemu::Watching<'_>,
    ring: (usize, usize),
    size: (usize, usize),
) -> Result<()> {
    use crate::compositor::{absolute, button_event, press};
    use std::time::{Duration, Instant};

    let tablet = |at: usize, across: usize| {
        let at = i64::try_from(at).unwrap_or(0);
        let across = i64::try_from(across.max(1)).unwrap_or(1);
        i32::try_from(at * 0x7FFF / across).unwrap_or(0)
    };
    let x = ring.0 - XEV_INNER_AT + XEV_POINT.0;
    let y = ring.1 - XEV_INNER_AT + XEV_POINT.1;
    let pause = |millis| std::thread::sleep(Duration::from_millis(millis));
    let click = |qmp: &mut crate::display::Qmp, button: &str| -> Result<()> {
        qmp.input_send_event(&[button_event(button, true)])?;
        pause(100);
        qmp.input_send_event(&[button_event(button, false)])
    };
    qmp.input_send_event(&[
        absolute("x", tablet(x, size.0)),
        absolute("y", tablet(y, size.1)),
    ])?;
    pause(500);
    click(qmp, "left")?;
    pause(300);
    click(qmp, "wheel-down")?;
    pause(300);
    press(qmp, &["a"])?;
    pause(300);
    press(qmp, &["z"])?;
    let picking = watching.read_more(Instant::now() + Duration::from_secs(90), |lines| {
        lines
            .iter()
            .any(|line| line.contains(XWINDOW_PICK) || line.contains(XWINDOW_END))
    })?;
    if picking {
        click(qmp, "left")?;
    }
    Ok(())
}

/// xev's report, one event a paragraph, each with its first line's name:
/// the `xwindow: xev:` lines, each event starting at a line that does not
/// begin with a space.
fn xev_events(lines: &[String]) -> Vec<(String, String)> {
    let mut events: Vec<(String, String)> = Vec::new();
    for line in lines {
        let Some((_, rest)) = line.split_once("xwindow: xev: ") else {
            continue;
        };
        let rest = rest.trim_end();
        if rest.starts_with(' ') {
            if let Some((_, text)) = events.last_mut() {
                text.push(' ');
                text.push_str(rest.trim());
            }
        } else if let Some((name, _)) = rest.split_once(" event,") {
            events.push((name.to_owned(), rest.to_owned()));
        }
    }
    events
}

/// The window-relative position an xev event reports, `(x,y)`.
fn xev_position(text: &str) -> Option<(i64, i64)> {
    let (_, rest) = text.split_once(", (")?;
    let (inside, _) = rest.split_once(')')?;
    let (x, y) = inside.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Whether xev reported the input [`drive_xev`] put in, at the place it was
/// put, and `xwininfo`'s pick found xev's window; and the server gave the
/// compositor a cursor (docs/YSERVER.md, Y4).
pub(crate) fn judge_xev_input(arch: Arch, lines: &[String]) -> Result<()> {
    let events = xev_events(lines);
    let near = |text: &str| {
        xev_position(text).is_some_and(|(x, y)| {
            let (want_x, want_y) = (XEV_POINT.0 as i64, XEV_POINT.1 as i64);
            (x - want_x).abs() <= 2 && (y - want_y).abs() <= 2
        })
    };
    let has = |name: &str, detail: &str, placed: bool| {
        events
            .iter()
            .any(|(event, text)| event == name && text.contains(detail) && (!placed || near(text)))
    };
    let wanted: [(&str, &str, &str, bool); 8] = [
        ("the keyboard focus", "FocusIn", "window", false),
        ("the pointer coming in", "EnterNotify", "window", false),
        (
            "the pointer where it was put",
            "MotionNotify",
            "window",
            true,
        ),
        ("the left button", "ButtonPress", "button 1,", true),
        ("the left button let go", "ButtonRelease", "button 1,", true),
        ("the wheel", "ButtonPress", "button 5,", true),
        ("the key a", "KeyPress", "(keysym 0x61, a)", true),
        ("the key a let go", "KeyRelease", "(keysym 0x61, a)", true),
    ];
    let missing: Vec<&str> = wanted
        .iter()
        .filter(|(_, name, detail, placed)| !has(name, detail, *placed))
        .map(|(what, ..)| *what)
        .collect();
    let picked = lines.iter().any(|line| {
        line.split_once("xwindow: pick:").is_some_and(|(_, rest)| {
            rest.contains("Window id:") && rest.contains("\"Event Tester\"")
        })
    });
    // The server's cursor, then xwininfo's cross while it grabs the pointer.
    let mut cursors: Vec<String> = lines
        .iter()
        .filter_map(|line| line.split_once("wayland: the cursor is "))
        .map(|(_, rest)| rest.trim().to_owned())
        .collect();
    let given = cursors.len();
    cursors.dedup();
    if missing.is_empty() && picked && cursors.len() >= 2 {
        println!(
            "  {arch}: xev reported {} events of the input put in, at ({}, {}) in its window; \
             xwininfo picked xev's window by a click; the compositor was given {given} cursors: \
             {}",
            events.len(),
            XEV_POINT.0,
            XEV_POINT.1,
            cursors.join(", ")
        );
        return Ok(());
    }
    let report: Vec<String> = events
        .iter()
        .map(|(_, text)| text.clone())
        .take(40)
        .collect();
    Err(Error::new(format!(
        "{arch}: xev did not report {missing:?}; xwininfo {} xev's window; the compositor was \
         given the cursors {cursors:?}, where xwininfo's grab should have changed it. xev \
         said:\n{}",
        if picked { "picked" } else { "did not pick" },
        report.join("\n")
    )))
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
    // The latest listing of a window is the one that counts.
    let field = |title: &str, key: &str| {
        windows
            .iter()
            .rev()
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

/// The screen before xfontsel's menu is opened and while it is, taken at
/// [`XWINDOW_MENU`] and [`XWINDOW_MENU_OPEN`].
///
/// # Errors
///
/// A screendump that fails.
pub(crate) fn watch_menu(
    qmp: &mut crate::display::Qmp,
    watching: &mut qemu::Watching<'_>,
    dump: &std::path::Path,
) -> Result<(Option<crate::display::Image>, Option<crate::display::Image>)> {
    use std::time::{Duration, Instant};
    let mut look = |line: &'static str, kept: &str| -> Result<Option<crate::display::Image>> {
        let seen = watching.read_more(Instant::now() + Duration::from_secs(120), |lines| {
            lines
                .iter()
                .any(|said| said.trim_end().ends_with(line) || said.contains(XWINDOW_END))
        })?;
        if !seen {
            return Ok(None);
        }
        // Kept beside the main picture, for a person reading a failure.
        let kept = dump.with_file_name(kept);
        qmp.screendump(Some(crate::display::DEVICE_ID), &kept)?;
        let bytes = std::fs::read(&kept)
            .map_err(|error| Error::new(format!("reading {}: {error}", kept.display())))?;
        crate::display::parse_ppm(&bytes).map(Some)
    };
    let before = look(XWINDOW_MENU, "xwindow-menu-before.ppm")?;
    let open = look(XWINDOW_MENU_OPEN, "xwindow-menu-open.ppm")?;
    Ok((before, open))
}

/// Where the server says it made its last popup, from its log: its size
/// and where it hangs from on its parent.
fn popup_placed(lines: &[String]) -> Option<((i32, i32), (usize, usize))> {
    lines.iter().rev().find_map(|line| {
        let (_, rest) = line.split_once("override-redirect window ")?;
        let (_, rest) = rest.split_once('(')?;
        let (size, rest) = rest.split_once(')')?;
        let (width, height) = size.split_once('x')?;
        let (_, at) = rest.split_once(" at (")?;
        let (x, rest) = at.split_once(", ")?;
        let (y, _) = rest.split_once(')')?;
        Some((
            (x.trim().parse().ok()?, y.trim().parse().ok()?),
            (width.parse().ok()?, height.parse().ok()?),
        ))
    })
}

/// How much of a rectangle's outline on `screen` is dark: an Athena menu's
/// one-pixel black border.
fn outline_dark(
    screen: &crate::display::Image,
    (x, y): (usize, usize),
    (w, h): (usize, usize),
) -> f64 {
    let dark = |px: usize, py: usize| {
        let at = (py * screen.width + px) * 3;
        screen
            .pixels
            .get(at..at + 3)
            .is_some_and(|rgb| rgb.iter().all(|&c| c <= 0x40))
    };
    let mut points = Vec::new();
    for px in x..x + w {
        points.push((px, y));
        points.push((px, y + h - 1));
    }
    for py in y..y + h {
        points.push((x, py));
        points.push((x + w - 1, py));
    }
    let found = points.iter().filter(|&&(px, py)| dark(px, py)).count();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a count of pixels on an outline, far below 2^52"
    )]
    let share = found as f64 / points.len().max(1) as f64;
    share
}

/// Whether xfontsel's menu showed as a popup where X put it (docs/YSERVER.md,
/// Y5b): the server made one, and the screen has its one-pixel black border
/// at xfontsel's place on the screen plus the popup's place on xfontsel,
/// while it is open and not before.
pub(crate) fn judge_menu(
    arch: Arch,
    lines: &[String],
    (before, open): (
        Option<&crate::display::Image>,
        Option<&crate::display::Image>,
    ),
) -> Result<()> {
    let fail = |why: String| {
        Err(Error::new(format!(
            "{arch}: {why}; the `xwindow:` lines say more"
        )))
    };
    if !lines
        .iter()
        .any(|line| line.contains("xwindow: menu window:"))
    {
        return fail("xfontsel's menu never opened in X".to_owned());
    }
    let Some(((x, y), size)) = popup_placed(lines) else {
        return fail("the server made no popup for xfontsel's menu".to_owned());
    };
    let windows = listed_windows(lines);
    let tile = windows
        .iter()
        .rev()
        .find(|(title, _)| title == "xfontsel")
        .and_then(|(_, fields)| fields.iter().find(|(key, _)| key == "at"))
        .and_then(|(_, at)| {
            let (left, top) = at.split_once(',')?;
            Some((
                left.trim().parse::<i32>().ok()?,
                top.trim().parse::<i32>().ok()?,
            ))
        });
    let Some((left, top)) = tile else {
        return fail("`hyprctl clients` did not place xfontsel".to_owned());
    };
    let (Ok(sx), Ok(sy)) = (usize::try_from(left + x), usize::try_from(top + y)) else {
        return fail(format!(
            "the popup would be off the screen at ({}, {})",
            left + x,
            top + y
        ));
    };
    let (Some(before), Some(open)) = (before, open) else {
        return fail("the boot took no picture of the menu".to_owned());
    };
    let (was, is) = (
        outline_dark(before, (sx, sy), size),
        outline_dark(open, (sx, sy), size),
    );
    if is < 0.9 || was > 0.5 {
        return fail(format!(
            "the menu's {}x{} outline at ({sx}, {sy}) is {:.0}% dark while open and {:.0}% before",
            size.0,
            size.1,
            is * 100.0,
            was * 100.0
        ));
    }
    println!(
        "  {arch}: xfontsel's menu is a popup, its {}x{} border on the screen at ({sx}, {sy}), \
         where X put it on xfontsel",
        size.0, size.1
    );
    Ok(())
}

/// Whether the clipboard went both ways for both selections
/// (docs/YSERVER.md, Y6): what `xclip` copied, `clip` pasted, and what
/// `clip` copied, `xclip` pasted.
pub(crate) fn judge_clipboard(arch: Arch, lines: &[String]) -> Result<()> {
    let said = |prefix: &str| {
        lines
            .iter()
            .find_map(|line| line.split_once(prefix))
            .map(|(_, rest)| rest.trim().to_owned())
    };
    let mut wrong = Vec::new();
    for which in ["clipboard", "primary"] {
        let pasted = said(&format!("xwindow: clip {which}:"));
        let wanted = format!("clip: pasted x-{which}-to-wayland");
        if pasted.as_deref() != Some(wanted.as_str()) {
            wrong.push(format!(
                "X's {which} copy reached Wayland as {pasted:?}, not {wanted:?}"
            ));
        }
        let pasted = said(&format!("xwindow: xclip {which}:"));
        let wanted = format!("wayland-{which}-to-x");
        if pasted.as_deref() != Some(wanted.as_str()) {
            wrong.push(format!(
                "Wayland's {which} copy reached X as {pasted:?}, not {wanted:?}"
            ));
        }
    }
    if wrong.is_empty() {
        println!("  {arch}: the clipboard and the primary selection go from X to Wayland and back");
        return Ok(());
    }
    Err(Error::new(format!(
        "{arch}: {}; the `xwindow:` lines and the server's log say more",
        wrong.join("; ")
    )))
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

    /// xev's report of the input `drive_xev` puts in, as the script prints
    /// it, with the pick and the server's cursor.
    fn reported() -> Vec<String> {
        let xev = "FocusIn event, serial 12, synthetic NO, window 0x200001,
    mode NotifyNormal, detail NotifyNonlinear

EnterNotify event, serial 13, synthetic NO, window 0x200001,
    root 0x3c9, subw 0x0, time 100, (119,121), root:(119,121),
    mode NotifyNormal, detail NotifyNonlinear, same_screen YES,
    focus YES, state 0

MotionNotify event, serial 13, synthetic NO, window 0x200001,
    root 0x3c9, subw 0x0, time 101, (120,120), root:(120,120),
    state 0x0, is_hint 0, same_screen YES

ButtonPress event, serial 13, synthetic NO, window 0x200001,
    root 0x3c9, subw 0x0, time 102, (120,120), root:(120,120),
    state 0x0, button 1, same_screen YES

ButtonRelease event, serial 13, synthetic NO, window 0x200001,
    root 0x3c9, subw 0x0, time 103, (120,120), root:(120,120),
    state 0x100, button 1, same_screen YES

ButtonPress event, serial 13, synthetic NO, window 0x200001,
    root 0x3c9, subw 0x0, time 104, (120,120), root:(120,120),
    state 0x0, button 5, same_screen YES

KeyPress event, serial 13, synthetic NO, window 0x200001,
    root 0x3c9, subw 0x0, time 105, (120,120), root:(120,120),
    state 0x0, keycode 38 (keysym 0x61, a), same_screen YES,
    XLookupString gives 1 bytes: (61) \"a\"

KeyRelease event, serial 13, synthetic NO, window 0x200001,
    root 0x3c9, subw 0x0, time 106, (120,120), root:(120,120),
    state 0x0, keycode 38 (keysym 0x61, a), same_screen YES,
    XLookupString gives 1 bytes: (61) \"a\"";
        let mut lines: Vec<String> = xev
            .lines()
            .map(|line| format!("xwindow: xev: {line}"))
            .collect();
        lines.push("xwindow: pick: xwininfo: Window id: 0x200001 \"Event Tester\"".to_owned());
        for hot in [8, 7, 8] {
            lines.push(format!(
                "xwindow: yserver: [DEBUG yserver::wayland::input] wayland: the cursor is 16x16 \
                 at ({hot}, {hot})"
            ));
        }
        lines
    }

    #[test]
    fn xevs_report_is_read_one_event_a_paragraph() {
        let events = xev_events(&reported());
        assert_eq!(events.len(), 8);
        assert_eq!(events[3].0, "ButtonPress");
        assert_eq!(xev_position(&events[3].1), Some((120, 120)));
        assert!(events[6].1.contains("(keysym 0x61, a)"));
        assert_eq!(xev_position(&events[0].1), None, "FocusIn has no place");
    }

    #[test]
    fn the_input_must_all_be_reported_where_it_was_put() {
        assert!(judge_xev_input(Arch::X86_64, &reported()).is_ok());
        let moved: Vec<String> = reported()
            .into_iter()
            .map(|line| line.replace("(120,120)", "(60,60)"))
            .collect();
        assert!(
            judge_xev_input(Arch::X86_64, &moved).is_err(),
            "a click in the wrong place"
        );
        let unpicked: Vec<String> = reported()
            .into_iter()
            .filter(|line| !line.contains("pick:"))
            .collect();
        assert!(judge_xev_input(Arch::X86_64, &unpicked).is_err());
        let no_wheel: Vec<String> = reported()
            .into_iter()
            .map(|line| line.replace("button 5", "button 4"))
            .collect();
        assert!(judge_xev_input(Arch::X86_64, &no_wheel).is_err());
        let one_cursor: Vec<String> = reported()
            .into_iter()
            .map(|line| line.replace("(7, 7)", "(8, 8)"))
            .collect();
        assert!(
            judge_xev_input(Arch::X86_64, &one_cursor).is_err(),
            "the grab's cursor never reached the compositor"
        );
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
    fn the_dialog_must_follow_its_own_drag() {
        let listing = |at: &str| {
            [
                "Window 2 -> Xev Dialog:".to_owned(),
                format!("\tat: {at}"),
                "\tsize: 178,178".to_owned(),
                "\tfloating: 1".to_owned(),
            ]
            .map(|line| format!("xwindow: clients: {line}"))
        };
        let lines = |after: &str, releases: u32| {
            let mut lines: Vec<String> = listing("423,295").to_vec();
            lines.push("xwindow: drag".to_owned());
            lines.push("xwindow: drag asked".to_owned());
            lines.push("xwindow: dragged".to_owned());
            lines.extend(listing(after));
            lines.push(format!("xwindow: dialog releases: {releases}"));
            lines
        };
        assert_eq!(
            dialog_place(&lines("543,375", 1)),
            Some(((543, 375), (178, 178)))
        );
        assert!(judge_drag(Arch::X86_64, &lines("543,375", 1)).is_ok());
        assert!(
            judge_drag(Arch::X86_64, &lines("544,374", 1)).is_ok(),
            "the tablet rounds"
        );
        assert!(
            judge_drag(Arch::X86_64, &lines("423,295", 1)).is_err(),
            "it never moved"
        );
        assert!(
            judge_drag(Arch::X86_64, &lines("543,375", 0)).is_err(),
            "the client must have had the release"
        );
        let undragged: Vec<String> = lines("543,375", 1)
            .into_iter()
            .filter(|line| !line.ends_with("dragged"))
            .collect();
        assert!(judge_drag(Arch::X86_64, &undragged).is_err());
    }

    #[test]
    fn the_menu_is_found_by_its_outline_where_the_server_put_it() {
        let lines: Vec<String> = [
            "xwindow: clients: Window 3 -> xfontsel:",
            "xwindow: clients: \tat: 21,21",
            "xwindow: menu window:      0x10004f (has no name): ()  68x448+7+46  +7+46",
            "xwindow: yserver: [..] wayland: override-redirect window 0x400048 (70x450) is a \
             popup on 0x40001a at (6, 45)",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert_eq!(popup_placed(&lines), Some(((6, 45), (70, 450))));
        let blank = Image {
            width: 200,
            height: 600,
            pixels: vec![0xff; 200 * 600 * 3],
        };
        let mut menu = Image {
            width: 200,
            height: 600,
            pixels: vec![0xff; 200 * 600 * 3],
        };
        for y in 66..66 + 450 {
            for x in 27..27 + 70 {
                if x == 27 || x == 27 + 69 || y == 66 || y == 66 + 449 {
                    let at = (y * 200 + x) * 3;
                    menu.pixels[at..at + 3].fill(0);
                }
            }
        }
        assert!(judge_menu(Arch::X86_64, &lines, (Some(&blank), Some(&menu))).is_ok());
        assert!(judge_menu(Arch::X86_64, &lines, (Some(&blank), Some(&blank))).is_err());
        assert!(judge_menu(Arch::X86_64, &lines, (Some(&menu), Some(&menu))).is_err());
    }

    #[test]
    fn the_clipboard_must_go_both_ways_for_both_selections() {
        let good: Vec<String> = [
            "xwindow: clip clipboard: clip: pasted x-clipboard-to-wayland",
            "xwindow: xclip clipboard: wayland-clipboard-to-x",
            "xwindow: clip primary: clip: pasted x-primary-to-wayland",
            "xwindow: xclip primary: wayland-primary-to-x",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert!(judge_clipboard(Arch::X86_64, &good).is_ok());
        // What a server without the bridge gives: xclip pastes its own
        // copy back, and clip finds nothing.
        let unbridged: Vec<String> = good
            .iter()
            .map(|line| {
                line.replace(
                    "clip: pasted x-clipboard-to-wayland",
                    "clip: failed: nothing",
                )
                .replace("wayland-primary-to-x", "x-primary-to-wayland")
            })
            .collect();
        assert!(judge_clipboard(Arch::X86_64, &unbridged).is_err());
        assert!(judge_clipboard(Arch::X86_64, &good[..3]).is_err());
    }

    #[test]
    fn the_root_is_judged_only_with_the_desktops_display() {
        let lines = |display: &str| -> Vec<String> {
            [
                display,
                "xwindow:   dimensions:    1280x800 pixels (338x211 millimeters)",
                "[INFO yserver::wayland] the root window is the compositor's screen, 1280x800",
            ]
            .iter()
            .map(|line| (*line).to_owned())
            .collect()
        };
        assert!(judge_xwindow(Arch::X86_64, &lines("xwindow: DISPLAY is :0")).is_ok());
        assert!(judge_xwindow(Arch::X86_64, &lines("xwindow: DISPLAY is ")).is_err());
    }

    #[test]
    fn the_window_manager_is_judged_by_its_round_trip_and_name() {
        let lines = |own: &str, name: &str| -> Vec<String> {
            [
                "xwindow: wm: root _NET_SUPPORTING_WM_CHECK(WINDOW): window id # 0x110".to_owned(),
                "xwindow: wm: root _NET_SUPPORTED(ATOM) = _NET_SUPPORTING_WM_CHECK, _NET_WM_NAME"
                    .to_owned(),
                format!("xwindow: wm: check _NET_SUPPORTING_WM_CHECK(WINDOW): window id # {own}"),
                format!("xwindow: wm: check _NET_WM_NAME(UTF8_STRING) = \"{name}\""),
            ]
            .to_vec()
        };
        assert!(judge_window_manager(Arch::X86_64, &lines("0x110", "hyprix")).is_ok());
        assert!(judge_window_manager(Arch::X86_64, &lines("0x111", "hyprix")).is_err());
        assert!(judge_window_manager(Arch::X86_64, &lines("0x110", "other")).is_err());
        // What a server without the announcement gives.
        let none = vec!["xwindow: wm: root _NET_SUPPORTING_WM_CHECK:  not found.".to_owned()];
        assert!(judge_window_manager(Arch::X86_64, &none).is_err());
    }

    #[test]
    fn the_desktop_gets_the_server_and_only_the_links_it_lacks() {
        let carried = vec![crate::ports::File {
            path: "usr/share/fonts/truetype".to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(Vec::new()),
        }];
        let files = desktop_files(&carried);
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert!(paths.contains(&DESKTOP_PATH));
        assert!(paths.contains(&"usr/share/X11"));
        assert!(
            !paths.contains(&"usr/share/fonts"),
            "fonts are Chrome's already"
        );
        let config = desktop_config();
        assert!(config.contains("env = DISPLAY,:0\n"));
        assert!(config.contains(&format!("exec-once = /bin/busybox sh /{DESKTOP_PATH}\n")));
        assert!(DESKTOP_SCRIPT.contains("/data/usr/lib64/ld-linux-x86-64.so.2 --library-path"));
    }

    #[test]
    fn the_pin_is_read_from_the_fetch_script() {
        let commit = pinned();
        assert_eq!(commit.len(), 40, "{commit:?}");
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(FETCH.contains(r#"echo "$YSERVER_COMMIT" > "$out/yserver.commit""#));
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
