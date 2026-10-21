//! The N150 box's desktop (`docs/N150.md` §9, N3): the image
//! `n150-install` puts on the box's EFI system partition by default, and
//! the one `test-n150-desktop` boots the way the box will.
//!
//! hyprix on the firmware's framebuffer (`ferrix.efifb`, H6: the box's Intel
//! graphics have no driver), with the boot console drawn on that screen
//! until hyprix takes it (`ferrix.fbcon`: the box has no serial port), and
//! the self-checks skipped, as on every desktop a person switches on.
//! yserver for X11, Wine's and Proton's links into the program tree at
//! `/data`, and `gryphlink`, which starts GRYPHLINK, in fuzzel's list and
//! a terminal away.
//!
//! The desktop is a session of the user `ferrix`, started by `sessiond`
//! (`crate::session`), not root's: the compositor and everything it starts,
//! GRYPHLINK and Wine's prefix in `/home/ferrix` among them, run as that
//! user. What has to stay root is init's: a script, a unit of
//! `multi-user.target` and so not the compositor's, brings the wired
//! interface up -- the YT6801's on the box, QEMU's under the test -- and
//! says what it found, whether hyprix runs or not.
//!
//! The launcher's background video is blocked, as
//! `FERRIX_ENDFIELD_NO_VIDEO=1` blocks it for `test-endfield-proton`
//! (`docs/WINE.md` §10): played in software it took 3.2 of 4 processors. Its
//! host resolves nowhere in `/etc/hosts`, which the image carries, and in
//! the Wine prefix's own hosts file, which `/bin/gryphlink` writes before
//! it starts the volume's `gryphlink`.

use std::path::PathBuf;

use super::boot::build_desktop_image;
use super::desktop::{Backdrop, desktop};
use super::{Programs, gates_busybox};
use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::{Error, Result};

/// The launcher's background video's host (`compositor::endfield`).
pub(crate) const VIDEO_HOST: &str = super::endfield::VIDEO_HOST;

/// What the image's loader adds to the kernel's command line
/// (`FERRIX/DEFAULTS.TXT`), before any `--kernel-option`.
pub(crate) const BOX_OPTIONS: &str = "ferrix.checks=skip ferrix.efifb ferrix.fbcon";

/// Where [`SCRIPT`] is in the image.
const SCRIPT_PATH: &str = "etc/n150-desktop.sh";

/// The line [`SCRIPT`] ends with, whatever it found.
pub(crate) const READY: &str = "n150-desktop: ready";

/// The script [`UNIT`] runs as root: the wired interface's lease (tried for
/// a minute: on the box the YT6801's driver may come up after the
/// compositor), the video host's block read back, the launcher's start
/// looked for, and the session's X server waited for. Every line starts
/// `n150-desktop:`, and it ends with [`READY`].
const SCRIPT: &str = r#"echo "n150-desktop: start"
if grep -q '@HOST@' /etc/hosts; then
    echo "n150-desktop: the launcher's video host @HOST@ resolves nowhere"
else
    echo "n150-desktop: /etc/hosts does not block @HOST@"
fi
if [ -x /data/usr/bin/gryphlink ]; then
    echo "n150-desktop: gryphlink is on /data"
else
    echo "n150-desktop: no gryphlink on /data"
fi
if lease=$(dhcpc -i eth0 -n -q -t 30 -T 2 2> /dev/null); then
    echo "n150-desktop: a lease for eth0: $lease"
else
    echo "n150-desktop: no lease for eth0 (dhcpc -i eth0 -q from a terminal tries again)"
fi
route=$(ip route 2> /dev/null | grep '^default')
if [ -n "$route" ]; then
    echo "n150-desktop: the default route: $route"
else
    echo "n150-desktop: no default route"
fi
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
if [ -S /tmp/.X11-unix/X0 ]; then
    echo "n150-desktop: yserver is up on :0 after ${waited}s"
else
    echo "n150-desktop: no X server after ${waited}s; /tmp/yserver.log says:"
    tail -n 5 /tmp/yserver.log 2> /dev/null | sed 's/^/n150-desktop:   /'
fi
echo "n150-desktop: ready"
"#;

/// Where [`UNIT`] is in the image, and the link that makes
/// `multi-user.target` want it.
const UNIT_PATH: &str = "etc/ferrix/units/n150-desktop.service";
const UNIT_WANTED: &str = "etc/ferrix/units/multi-user.target.wants/n150-desktop.service";

/// [`SCRIPT`]'s unit: root's, of `multi-user.target`, so the lease does not
/// wait on the compositor's card (`docs/BACKLOG.md`: the box was
/// unreachable whenever hyprix did not run). `simple`, not `oneshot`:
/// `graphical.target` comes after `multi-user.target`, and a oneshot would
/// hold the compositor back while the script waits for its X server.
const UNIT: &str = "# The N150 box's root part of its desktop, which runs as ferrix \
                    (tools/common/xtask/src/compositor/n150_box.rs).\n\
                    [Unit]\n\
                    Description=The N150 box's network and launcher checks\n\
                    \n\
                    [Service]\n\
                    Type=simple\n\
                    ExecStart=/bin/busybox sh /etc/n150-desktop.sh\n\
                    StandardOutput=console\n\
                    StandardError=console\n";

/// `/bin/gryphlink`: the video host blocked in the prefix's hosts file,
/// which Chromium reads on Windows, then the volume's own start. On the
/// first start the prefix does not exist yet; the file is written ahead of
/// `wineboot`, and if `wineboot` replaces it the next start writes it again.
/// `/etc/hosts` blocks the host either way.
const GRYPHLINK: &str = r#"#!/bin/busybox sh
# The N150 box's start of GRYPHLINK (cargo xtask n150-install, docs/N150.md):
# the launcher's background video blocked in the Wine prefix's hosts file
# too, as FERRIX_ENDFIELD_NO_VIDEO=1 does for test-endfield-proton
# (docs/WINE.md section 10), then the volume's own start.
prefix="${WINEPREFIX:-$HOME/.local/share/gryphlink/prefix}"
hosts="$prefix/drive_c/windows/system32/drivers/etc/hosts"
if ! grep -q '@HOST@' "$hosts" 2>/dev/null; then
    mkdir -p "${hosts%/*}"
    echo "0.0.0.0 @HOST@" >> "$hosts"
fi
exec /data/usr/bin/gryphlink "$@"
"#;

/// The image's `/etc/hosts`: the machine's own name, `ferrix`
/// (`/etc/hostname`), on 127.0.1.1 as Debian puts it, so a program that
/// resolves the host it runs on finds it -- the box needed it by hand for
/// the launcher (2026-10-18) -- and the video host blocked.
const HOSTS: &str = "127.0.0.1 localhost\n127.0.1.1 ferrix\n::1 localhost\n0.0.0.0 @HOST@\n";

/// The compositor's configuration ahead of what [`desktop`] adds: a
/// terminal to start with and one a key away, fuzzel a key away, the
/// layout's keys, and X11's display for every program. `HOME` is the
/// session's own, which `sessiond` sets.
fn config(layout: &str) -> String {
    format!(
        "# Written into the initramfs by `cargo xtask n150-install` (docs/N150.md).\n\
         input:kb_layout = {layout}\n\
         {}\
         exec-once = /bin/term /bin/zinc\n\
         bind = SUPER, RETURN, exec, /bin/term /bin/zinc\n\
         bind = SUPER, D, exec, /bin/fuzzel\n\
         bind = SUPER, G, exec, /bin/gryphlink\n\
         bind = SUPER, Q, killactive\n\
         bind = SUPER, F, fullscreen\n\
         bind = SUPER, V, togglefloating\n\
         bind = SUPER, 1, workspace, 1\n\
         bind = SUPER, 2, workspace, 2\n\
         bind = SUPER SHIFT, 1, movetoworkspace, 1\n\
         bind = SUPER SHIFT, 2, movetoworkspace, 2\n",
        crate::yserver::desktop_config()
    )
}

/// The box's keyboard layout unless `--layout` names one: the German
/// keyboard the customer types on (the DK1's desktop takes it too).
const LAYOUT: &str = "de";

/// The files the box's desktop carries beyond [`desktop`]'s: yserver's,
/// Wine's and Proton's links into `/data` and fuzzel's GRYPHLINK entry
/// (`endfield::desktop_files`), with `/bin/gryphlink` the start of
/// [`GRYPHLINK`] rather than a link; [`HOSTS`]; and [`SCRIPT`] with its
/// [`UNIT`].
fn files(carried: &[File]) -> Vec<File> {
    let text = |path: &str, mode: u32, body: &str| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(body.replace("@HOST@", VIDEO_HOST).into_bytes()),
    };
    let mut out = crate::yserver::desktop_files(carried);
    let mut launcher = super::endfield::desktop_files(carried);
    launcher.retain(|file| file.path != "bin/gryphlink");
    out.extend(launcher);
    out.push(text("bin/gryphlink", 0o755, GRYPHLINK));
    out.push(text("etc/hosts", 0o644, HOSTS));
    out.push(text(SCRIPT_PATH, 0o644, SCRIPT));
    out.push(text(UNIT_PATH, 0o644, UNIT));
    out.push(File {
        path: UNIT_WANTED.to_owned(),
        mode: 0o777,
        content: Content::Link(format!("/{UNIT_PATH}")),
    });
    out
}

/// The box's desktop image and its kernel's ELF: built in release, on
/// x86-64, with [`BOX_OPTIONS`] and every `--kernel-option` in its
/// `DEFAULTS.TXT`.
///
/// # Errors
///
/// No static busybox on this host, or a build that failed.
pub(crate) fn image(args: &Args) -> Result<(PathBuf, PathBuf)> {
    let arch = Arch::X86_64;
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new(
            "the N150 desktop needs a static busybox: Alpine's busybox-static at \
             ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from \
             `cargo xtask busybox`",
        )
    })?;
    // `net` off for `desktop`: the script's unit, not an `exec-once`, asks
    // for the lease. No wallpaper unless one is named: each frame is drawn
    // in software on four small cores. The desktop is `ferrix`'s session,
    // never root's.
    let args = &Args {
        release: true,
        net: false,
        init: Some(busybox),
        wallpaper: args.wallpaper.clone().or_else(|| Some("none".to_owned())),
        session_user: Some(crate::session::USER.to_owned()),
        ..args.clone()
    };
    // `--ssh` with no sshdt package made an image without a server, said in
    // one line of the build's output, and the box was out of reach until
    // somebody came to it. sshdt is a script app, which an image takes only
    // once built, so it is built here.
    if args.ssh.is_some() {
        let package = crate::apps::build_unless_built(arch, "sshdt", false)?;
        println!("  n150: --ssh: sshdt from {}", package.display());
    }
    let programs = Programs::build(arch)?;
    let layout = args.layout.clone().unwrap_or_else(|| LAYOUT.to_owned());
    let size = args.size.unwrap_or(crate::wallpaper::SCREEN);
    let (config, mut carried) = desktop(arch, config(&layout), size, true, Backdrop::Any, args)?;
    // What `desktop` started as root becomes init's, and the dotfiles seed
    // the user's home, as `run-compositor --everything`'s session does.
    println!(
        "  n150: the desktop runs as {}, started by sessiond",
        crate::session::USER
    );
    let config = crate::session::for_session(&config, &mut carried.ports, None);
    if args.ssh.is_some() && !carried.ports.iter().any(|file| file.path == "bin/sshdt") {
        return Err(Error::new(
            "n150: --ssh, and the image has no /bin/sshdt (built, but not taken: --no-apps?); \
             no image is written that the box cannot be reached on",
        ));
    }
    let more = files(&carried.ports);
    carried.ports.extend(more);
    let mut options = BOX_OPTIONS.to_owned();
    for option in &args.kernel_options {
        options.push(' ');
        options.push_str(option);
    }
    println!("  n150: the box's desktop, its loader adding `{options}`");
    build_desktop_image(
        arch,
        &programs,
        &config,
        carried,
        args,
        &format!("{options}\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        GRYPHLINK, HOSTS, SCRIPT, SCRIPT_PATH, UNIT_PATH, UNIT_WANTED, VIDEO_HOST, config, files,
    };

    /// The video host is blocked in both hosts files, and `/bin/gryphlink`
    /// is the start that blocks it, not a link past it.
    #[test]
    fn the_video_host_is_blocked_before_the_launcher_starts() {
        let carried = files(&[]);
        let find = |path: &str| {
            carried
                .iter()
                .find(|file| file.path == path)
                .map(|file| match &file.content {
                    crate::ports::Content::Bytes(bytes) => {
                        String::from_utf8_lossy(bytes).into_owned()
                    }
                    _ => String::new(),
                })
        };
        let hosts = find("etc/hosts").expect("the image's /etc/hosts");
        assert!(hosts.contains(&format!("0.0.0.0 {VIDEO_HOST}")));
        assert!(hosts.lines().any(|line| line == "127.0.1.1 ferrix"));
        let start = find("bin/gryphlink").expect("/bin/gryphlink");
        let block = start
            .find("drivers/etc/hosts")
            .expect("the prefix's hosts file");
        let exec = start
            .find("exec /data/usr/bin/gryphlink")
            .expect("the volume's start");
        assert!(block < exec);
        assert!(!start.contains("@HOST@") && !hosts.contains("@HOST@"));
        assert_eq!(
            carried
                .iter()
                .filter(|file| file.path == "bin/gryphlink")
                .count(),
            1
        );
        assert!(find(SCRIPT_PATH).is_some());
        assert!(HOSTS.contains("localhost") && GRYPHLINK.starts_with("#!/bin/busybox sh"));
    }

    /// The configuration starts yserver and leaves the script to init, as
    /// root's unit of `multi-user.target`; nothing in it is root's, and the
    /// script ends with its ready line whatever it found.
    #[test]
    fn the_desktop_starts_the_x_server_and_init_starts_the_script() {
        let text = config("de");
        assert!(!text.contains(SCRIPT_PATH));
        assert!(!text.contains("/root"));
        let carried = files(&[]);
        let unit = carried
            .iter()
            .find(|file| file.path == UNIT_PATH)
            .expect("the script's unit");
        let crate::ports::Content::Bytes(unit) = &unit.content else {
            panic!("the unit is a file");
        };
        let unit = String::from_utf8_lossy(unit);
        assert!(unit.contains(&format!("ExecStart=/bin/busybox sh /{SCRIPT_PATH}\n")));
        assert!(unit.contains("Type=simple\n") && !unit.contains("User="));
        assert!(carried.iter().any(|file| file.path == UNIT_WANTED
            && matches!(&file.content, crate::ports::Content::Link(to) if *to == format!("/{UNIT_PATH}"))));
        assert!(text.contains("env = DISPLAY,:0"));
        assert!(text.contains("input:kb_layout = de"));
        assert!(
            SCRIPT
                .trim_end()
                .ends_with(&format!("echo \"{}\"", super::READY))
        );
    }
}
