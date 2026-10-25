//! The look a watched desktop is drawn in (`docs/DESKTOP.md` §6): Yoru,
//! the customer's design and the default since 2026-10-24; Ferrix light
//! (§2); or none, hyprix's own defaults.
//!
//! A theme is a configuration file hyprix ships (`hyprix/themes/`), put in
//! the image at `/usr/share/hyprix/themes/` with what it names -- Yoru's
//! wallpaper, Inter and the Phosphor icons -- and `source`d *first*, so
//! that every line after it wins: the keyboard layout and the network
//! `run-compositor` appends, and a person's own `hyprland.conf`. A desktop
//! with no configuration of its own takes the theme's keys too
//! (`yoru-keys.conf`); one with its own takes the look alone, so that its
//! own binds are the only ones.
//!
//! Only the desktops a person uses are themed -- `run-compositor`, `flash
//! --compositor` and `release-image` set [`Args::theme`] -- never a judged
//! boot, whose pictures stay what they were.

use crate::args::Args;
use crate::ports::{Content, File};
use crate::{Error, Result};

/// Where the themes go in the image.
const THEMES: &str = "usr/share/hyprix/themes";

/// Yoru's wallpaper, which `yoru.conf`'s `wallpaper =` line names.
pub(super) const YORU_WALLPAPER: &str = "usr/share/backgrounds/yoru/yoru-nocturne.svg";

/// The faces Yoru draws in, and where: Inter for its text, Phosphor for
/// its icons, each with its licence (`assets/fonts/README.md`).
const FACES: [(&str, &str); 4] = [
    (
        "assets/fonts/inter/InterVariable.ttf",
        "usr/share/fonts/inter/InterVariable.ttf",
    ),
    (
        "assets/fonts/inter/LICENSE",
        "usr/share/fonts/inter/LICENSE",
    ),
    (
        "assets/fonts/phosphor/Phosphor.ttf",
        "usr/share/fonts/phosphor/Phosphor.ttf",
    ),
    (
        "assets/fonts/phosphor/LICENSE",
        "usr/share/fonts/phosphor/LICENSE",
    ),
];

/// Yoru's dock, the `yoru-shell` app, which every desktop carries
/// (`desktop::DESKTOP_APPS`).
pub(super) const DOCK: &str = "bin/yoru-shell";

/// Whether `config` starts a bar of its own: an `exec-once` of waybar or
/// of Yoru's shell.
fn starts_a_bar(config: &str) -> bool {
    config.lines().any(|line| {
        line.split_once('=').is_some_and(|(key, value)| {
            matches!(key.trim(), "exec-once" | "exec")
                && (value.contains("waybar") || value.contains("yoru-shell"))
        })
    })
}

/// A desktop's look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Theme {
    /// The customer's design, on Nocturne.
    Yoru,
    /// GNOME-like and light.
    FerrixLight,
    /// hyprix's own defaults: nothing sourced.
    None,
}

impl Theme {
    /// What `--theme` says, Yoru when it says nothing.
    ///
    /// # Errors
    ///
    /// A name that is none of them.
    pub(super) fn of(args: &Args) -> Result<Option<Self>> {
        match args.theme.as_deref() {
            None => Ok(None),
            Some("yoru") => Ok(Some(Self::Yoru)),
            Some("ferrix-light") => Ok(Some(Self::FerrixLight)),
            Some("none") => Ok(Some(Self::None)),
            Some(other) => Err(Error::new(format!(
                "--theme {other}: yoru, ferrix-light or none"
            ))),
        }
    }
}

/// Give a desktop a person uses its theme: `--theme`, or Yoru.
pub(super) fn default_for_a_person(args: &mut Args) {
    if args.theme.is_none() {
        args.theme = Some("yoru".to_owned());
    }
}

/// Whether the shipped Ferrix wallpaper should give way to the theme's
/// own: under Yoru, which has one, unless `--wallpaper` named it.
pub(super) fn has_own_wallpaper(args: &Args) -> bool {
    matches!(Theme::of(args), Ok(Some(Theme::Yoru)))
        && args
            .wallpaper
            .as_deref()
            .is_none_or(|name| name == crate::wallpaper::BRAND && args.release)
}

/// `config` with the theme `args` names sourced before it, and the files
/// it needs added to `files`. A configuration of the person's own
/// (`--config`, or `--everything`'s) takes the theme's look and not its
/// keys.
///
/// # Errors
///
/// An unknown `--theme`, or a theme's file missing from the tree.
pub(super) fn apply(config: String, args: &Args, files: &mut Vec<File>) -> Result<String> {
    let Some(theme) = Theme::of(args)? else {
        return Ok(config);
    };
    let own = args.config.is_some();
    let sourced: &[&str] = match (theme, own) {
        (Theme::None, _) => return Ok(config),
        (Theme::Yoru, false) => &["yoru.conf", "yoru-keys.conf"],
        (Theme::Yoru, true) => &["yoru.conf"],
        (Theme::FerrixLight, _) => &["ferrix-light.conf"],
    };
    let root = crate::paths::workspace_root();
    let themes = root.join("src/user/system/linux/compositor/hyprix/themes");
    let mut head = format!(
        "# The desktop's look, `--theme {}` (docs/DESKTOP.md §6): first, so every line \
         after it wins.\n",
        args.theme.as_deref().unwrap_or("yoru")
    );
    let mut carry = |from: std::path::PathBuf, to: String| -> Result<()> {
        let bytes = std::fs::read(&from)
            .map_err(|error| Error::new(format!("{}: {error}", from.display())))?;
        if !files.iter().any(|file| file.path == to) {
            files.push(File {
                path: to,
                mode: 0o644,
                content: Content::Bytes(bytes),
            });
        }
        Ok(())
    };
    for name in sourced {
        carry(themes.join(name), format!("{THEMES}/{name}"))?;
        head.push_str(&format!("source = /{THEMES}/{name}\n"));
    }
    // Yoru's dock and its two widget sidebars, unless the configuration
    // starts a bar of its own.
    if theme == Theme::Yoru && !starts_a_bar(&config) {
        head.push_str(&format!("exec-once = /{DOCK}\n"));
        head.push_str(&format!("exec-once = /{DOCK} --sidebar left\n"));
        head.push_str(&format!("exec-once = /{DOCK} --sidebar right\n"));
    }
    if theme == Theme::Yoru {
        // The launcher's look, which Yoru's keys and its dock start fuzzel
        // with (`--config`).
        carry(
            themes.join("yoru-fuzzel.ini"),
            format!("{THEMES}/yoru-fuzzel.ini"),
        )?;
        carry(
            root.join("docs/brand/yoru/yoru-nocturne.svg"),
            YORU_WALLPAPER.to_owned(),
        )?;
        for (from, to) in FACES {
            carry(root.join(from), to.to_owned())?;
        }
    }
    println!(
        "  theme: {} ({})",
        args.theme.as_deref().unwrap_or("yoru"),
        if own {
            "its look under this configuration's own lines"
        } else {
            "its look and its keys"
        }
    );
    Ok(format!("{head}{config}"))
}

#[cfg(test)]
mod tests {
    use super::{Theme, apply};
    use crate::args::Args;

    /// Yoru, on a desktop with no configuration of its own: both files
    /// sourced before everything else, and the wallpaper and faces carried.
    #[test]
    fn yoru_comes_first_with_its_keys() {
        let args = Args {
            theme: Some("yoru".to_owned()),
            ..Args::default()
        };
        let mut files = Vec::new();
        let config = apply(
            "bind = SUPER, Q, killactive\n".to_owned(),
            &args,
            &mut files,
        )
        .expect("themed");
        let yoru = config
            .find("source = /usr/share/hyprix/themes/yoru.conf")
            .expect("look");
        let keys = config
            .find("source = /usr/share/hyprix/themes/yoru-keys.conf")
            .expect("keys");
        let own = config.find("bind = SUPER, Q").expect("own line");
        assert!(yoru < keys && keys < own);
        assert!(config.contains("exec-once = /bin/yoru-shell"), "the dock");
        for path in [
            "usr/share/hyprix/themes/yoru.conf",
            "usr/share/hyprix/themes/yoru-fuzzel.ini",
            "usr/share/backgrounds/yoru/yoru-nocturne.svg",
            "usr/share/fonts/phosphor/Phosphor.ttf",
            "usr/share/fonts/phosphor/LICENSE",
            "usr/share/fonts/inter/InterVariable.ttf",
        ] {
            assert!(files.iter().any(|file| file.path == path), "{path}");
        }
    }

    /// A person's own configuration keeps its own keys.
    #[test]
    fn an_own_configuration_takes_the_look_alone() {
        let args = Args {
            theme: Some("yoru".to_owned()),
            config: Some("/home/u/.config/hypr/hyprland.conf".to_owned()),
            ..Args::default()
        };
        let mut files = Vec::new();
        let config = apply(String::new(), &args, &mut files).expect("themed");
        assert!(config.contains("yoru.conf"));
        assert!(!config.contains("yoru-keys.conf"));
        assert!(
            config.contains("yoru-shell"),
            "no bar of its own: Yoru's dock"
        );
        let with_waybar =
            apply("exec-once = waybar\n".to_owned(), &args, &mut files).expect("themed");
        assert!(
            !with_waybar.contains("yoru-shell"),
            "its own bar, not the dock"
        );
    }

    /// No `--theme` is no theme (a judged boot's), `none` the same, and an
    /// unknown one is refused.
    #[test]
    fn unthemed_and_unknown() {
        let mut files = Vec::new();
        let plain = Args::default();
        assert_eq!(
            apply("x\n".to_owned(), &plain, &mut files).expect("same"),
            "x\n"
        );
        let none = Args {
            theme: Some("none".to_owned()),
            ..Args::default()
        };
        assert_eq!(
            apply("x\n".to_owned(), &none, &mut files).expect("same"),
            "x\n"
        );
        assert!(files.is_empty());
        let odd = Args {
            theme: Some("mauve".to_owned()),
            ..Args::default()
        };
        assert!(Theme::of(&odd).is_err());
    }
}
