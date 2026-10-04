//! fuzzel on the desktop: the applications it lists, their icons, and the
//! boot that drives it.
//!
//! fuzzel lists `.desktop` files, and Ferrix's image has almost no programs
//! that come with one. So the image carries entries of the tree's own for
//! what the desktop does have -- [`APPLICATIONS`], one small list: the
//! terminal running zinc, the test pattern, busybox's `top`, Chrome when
//! `--chrome` carries it, and vkgears when the image has it and the card
//! offers Venus -- each with an icon of the tree's own in a
//! `hicolor` theme, so the icon lookup and the large picture of the selected
//! entry have something to find. The files are in
//! `src/user/system/linux/compositor/desktop/`, where the fuzzel app's host test reads the same ones.
//! These are the system's entries; an app ships its own in its package
//! (`docs/APPS.md`), so the `--everything` desktop lists btop and
//! ferrofetch beside them.
//!
//! `top` is `Terminal=true` on purpose. fuzzel starts such an entry through
//! `terminal=`, and the user's `fuzzel.ini` says `terminal=foot`, which is not
//! in the image: choosing it fails the way fuzzel says a program that is not
//! there fails (`foot top: failed to execute: No such file or directory
//! (2)`), and fuzzel exits 1. With `terminal=/bin/term` it opens in the
//! terminal.

use crate::ports::{Content, File};
use crate::{Result, paths};

/// The applications the image carries, by their `.desktop` file's name.
const APPLICATIONS: [&str; 3] = ["terminal", "pattern", "top"];

/// The icons, by name.
const ICONS: [&str; 5] = [
    "ferrix-terminal",
    "ferrix-pattern",
    "ferrix-monitor",
    "ferrix-browser",
    "ferrix-gears",
];

/// The entry for vkgears, which only a desktop with Venus lists: on any other
/// card it finds no Vulkan device and exits. Listed, never started -- it is
/// there to be chosen. Its `Exec` sets `MESA_VK_WSI_DEBUG=sw` for the reason
/// `test-vkgears` does (`docs/GPU.md` §6.1), for vkgears alone rather than as
/// an `env =` line every client would inherit.
const VKGEARS: &str = "vkgears";

/// Where the tree keeps them.
const DATA: &str = "src/user/system/linux/compositor/desktop";

/// Where the entries and the icons go in the image.
const INSTALLED_APPLICATIONS: &str = "usr/share/applications";
const HICOLOR: &str = "usr/share/icons/hicolor";

/// A file of the tree's, read.
fn read(relative: &str) -> Result<Vec<u8>> {
    let path = paths::workspace_root().join(relative);
    std::fs::read(&path).map_err(|error| crate::Error::new(format!("{}: {error}", path.display())))
}

/// Every file fuzzel's applications need in the image: the entries, the
/// `hicolor` theme and its icons, and with `chrome` (the command that opens
/// its window) an entry for Chrome, and with `vkgears` one for it.
pub(crate) fn files(chrome: Option<&str>, vkgears: bool) -> Result<Vec<File>> {
    let mut out = Vec::new();
    let listed = vkgears.then_some(VKGEARS);
    for name in APPLICATIONS.into_iter().chain(listed) {
        out.push(File {
            path: format!("{INSTALLED_APPLICATIONS}/{name}.desktop"),
            mode: 0o644,
            content: Content::Bytes(read(&format!("{DATA}/applications/{name}.desktop"))?),
        });
    }
    if let Some(command) = chrome {
        // Written here rather than kept, because its command line is
        // xtask's: the one `run-compositor --chrome` starts.
        let text = format!(
            "[Desktop Entry]\nType=Application\nName=Chrome for Testing\nGenericName=Web Browser\n\
             Comment=Chrome for Testing, from the data disk\nExec={command}\n\
             Icon=ferrix-browser\nTerminal=false\nKeywords=web;internet;browser;\n\
             Categories=Network;WebBrowser;\n"
        );
        out.push(File {
            path: format!("{INSTALLED_APPLICATIONS}/chrome.desktop"),
            mode: 0o644,
            content: Content::Bytes(text.into_bytes()),
        });
    }
    // `~/.cache`, which is where fuzzel keeps how often each entry was
    // started. The desktop's home is `/`, and a home with no cache
    // directory is one fuzzel complains about on every run
    // (`/.cache: failed to open`), as it does on Linux.
    out.push(File {
        path: ".cache".to_owned(),
        mode: 0o755,
        content: Content::Directory,
    });
    out.push(File {
        path: format!("{HICOLOR}/index.theme"),
        mode: 0o644,
        content: Content::Bytes(read(&format!("{DATA}/icons/index.theme"))?),
    });
    for icon in ICONS {
        out.push(File {
            path: format!("{HICOLOR}/scalable/apps/{icon}.svg"),
            mode: 0o644,
            content: Content::Bytes(read(&format!("{DATA}/icons/{icon}.svg"))?),
        });
    }
    Ok(out)
}

/// The boot's `hyprland.conf`: fuzzel as the desktop comes up, and two keys
/// that have `/bin/vkbd` type into it -- a program acting as a keyboard,
/// which is how a key reaches fuzzel's exclusive keyboard grab from outside
/// the guest.
pub(crate) const BOOT_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/fuzzel --log-level=info --print-timing-info
bind = , F1, exec, /bin/vkbd p a t
bind = , F2, exec, /bin/vkbd Return
";

/// The pictures the boot requires, in order.
pub(crate) const EXPECTED: [(&str, &str); 3] = [
    (
        "fuzzel over the empty screen, every entry listed and the first selected",
        "src/user/system/linux/compositor/render/tests/data/fuzzel-listed.xrle",
    ),
    (
        "fuzzel with `pat` typed and the test pattern ranked first",
        "src/user/system/linux/compositor/render/tests/data/fuzzel-typed-pat.xrle",
    ),
    (
        "the test pattern fuzzel started, alone on the screen",
        "src/user/system/linux/compositor/render/tests/data/one-client-alone.xrle",
    ),
];

/// The keys between the pictures. No modifier, for the reason
/// `TASKBAR_BINDS` gives.
pub(crate) const BINDS: [(&str, &[&str]); 2] = [
    ("F1, which has vkbd type `pat`", &["f1"]),
    ("F2, which has vkbd press Return", &["f2"]),
];

/// What the boot waits for before its transcript is judged.
pub(crate) const AWAITING: [&str; 2] = ["vkbd: typed Return", "executing pattern.desktop"];

/// Where the boot's `fuzzel.ini` is kept, and the font it names.
/// The boot's `fuzzel.ini`, in the fuzzel app's folder.
const BOOT_INI: &str = "data/boot/fuzzel.ini";
const BOOT_FONT: &str = "assets/fonts/liberation/LiberationSerif-Regular.ttf";

/// Everything the fuzzel boot carries: the program, the entries and icons,
/// the `fuzzel.ini` in the home directory the compositor's children have
/// (`/`), and the font it names where `src/user/system/linux/compositor/text` looks.
pub(crate) fn boot_files(program: &std::path::Path) -> Result<Vec<File>> {
    let read = |path: &std::path::Path| {
        std::fs::read(path)
            .map_err(|error| crate::Error::new(format!("{}: {error}", path.display())))
    };
    let root = paths::workspace_root();
    let mut out = vec![
        File {
            path: "bin/fuzzel".to_owned(),
            mode: 0o755,
            content: Content::Bytes(read(program)?),
        },
        File {
            path: ".config/fuzzel/fuzzel.ini".to_owned(),
            mode: 0o644,
            content: Content::Bytes(read(&crate::apps::folder("fuzzel")?.join(BOOT_INI))?),
        },
        File {
            path: "usr/share/ferrix/fonts/liberation/LiberationSerif-Regular.ttf".to_owned(),
            mode: 0o644,
            content: Content::Bytes(read(&root.join(BOOT_FONT))?),
        },
    ];
    out.extend(files(None, false)?);
    Ok(out)
}

/// What the fuzzel boot requires of its transcript, once its pictures
/// matched.
pub(crate) fn judge(arch: paths::Arch, pictures: usize, said: &[String]) -> Result<()> {
    if pictures != EXPECTED.len() {
        return Err(crate::Error::new(format!(
            "{arch}: {pictures} of {} pictures were taken",
            EXPECTED.len()
        )));
    }
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for wanted in [
        "fuzzel: 3 entries",
        "namespace launcher",
        "vkbd: typed p a t",
        "vkbd: typed Return",
        "executing pattern.desktop: \"/bin/pattern gradient launched\"",
    ] {
        if !has(wanted) {
            return Err(crate::Error::new(format!(
                "{arch}: the fuzzel boot never said `{wanted}`"
            )));
        }
    }
    // fuzzel says what it could not do as ` err:`; a boot that matched its
    // pictures and complained is not one that worked.
    if let Some(line) = said.iter().find(|line| line.contains(" err: ")) {
        return Err(crate::Error::new(format!(
            "{arch}: fuzzel complained: {}",
            line.trim()
        )));
    }
    println!(
        "  {arch}: fuzzel read its fuzzel.ini, listed the image's applications with their icons, \
         ranked the one `pat` names first when another program typed it, and started it on Return"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{APPLICATIONS, VKGEARS, files};

    #[test]
    fn the_carried_entries_are_desktop_files() {
        let carried = files(Some("/data/chrome --flag"), true).unwrap_or_default();
        for name in APPLICATIONS.into_iter().chain([VKGEARS]) {
            let file = carried
                .iter()
                .find(|file| file.path.ends_with(&format!("/{name}.desktop")));
            let Some(crate::ports::Content::Bytes(bytes)) = file.map(|file| &file.content) else {
                panic!("{name}.desktop is not carried");
            };
            assert!(bytes.starts_with(b"[Desktop Entry]\n"), "{name}");
        }
        assert!(
            carried
                .iter()
                .any(|file| file.path.ends_with("/chrome.desktop"))
        );
        assert!(
            carried
                .iter()
                .any(|file| file.path.ends_with("hicolor/index.theme"))
        );
    }

    #[test]
    fn vkgears_is_listed_only_when_asked() {
        let entry = format!("/{VKGEARS}.desktop");
        let without = files(None, false).unwrap_or_default();
        assert!(!without.iter().any(|file| file.path.ends_with(&entry)));
        let with = files(None, true).unwrap_or_default();
        assert!(with.iter().any(|file| file.path.ends_with(&entry)));
    }
}
