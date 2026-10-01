//! The user's own desktop configuration, carried into a desktop image.
//!
//! `cargo xtask run-compositor --config ~/.config/hypr/hyprland.conf` boots
//! the compositor on that file, and the file starts `waybar` and `hypridle`
//! and binds fuzzel and hyprlock -- programs that read files of their own
//! from beside it: `~/.config/hypr/hyprlock.conf` and `hypridle.conf`,
//! `~/.config/waybar/config.jsonc`, `style.css` and its icons,
//! `~/.config/fuzzel/fuzzel.ini`. So those directories go into the image
//! too, and the fonts the files name.
//!
//! **Where they go.** Into `/.config`: the desktop's `HOME` is `/`. Since
//! init L10 the compositor is `hyprix.service` under `/sbin/init`, whose
//! unit sets `HOME=/` (5fa34300), the environment hyprix had as
//! pid 1, and every client it starts inherits it.
//!
//! **Which directories.** The ones beside the directory `--config` is in,
//! named [`CARRIED`]: for `~/.config/hypr/hyprland.conf` that is
//! `~/.config/{hypr,waybar,fuzzel}`. A directory that is not there is
//! skipped, and so is a file larger than [`LARGEST`], which is a picture
//! somebody keeps there rather than configuration.
//!
//! **Fonts.** Every family the files name -- `font_family` in the hyprlang
//! files (with `$variables` expanded), `font=` in `fuzzel.ini`,
//! `font-family` in waybar's CSS -- is resolved on this machine with
//! `fc-match`, and every file of that family (`fc-list`) goes to
//! `/usr/share/fonts/host/`, where `src/user/system/linux/compositor/text` looks. They are read
//! from the host when the image is built and never committed: they are the
//! user's own and their licences are theirs. A family the host cannot
//! resolve is said and skipped; a host with no fontconfig carries none.
//!
//! `--no-dotfiles` carries none of it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::ports::{Content, File};

/// The directories carried from beside `--config`'s.
pub(crate) const CARRIED: [&str; 3] = ["hypr", "waybar", "fuzzel"];

/// Where they go in the image: `$HOME/.config`, and `HOME` is `/`.
pub(crate) const CONFIG_HOME: &str = ".config";

/// Where the resolved fonts go.
pub(crate) const FONT_DIR: &str = "usr/share/fonts/host";

/// The largest file carried from a configuration directory.
pub(crate) const LARGEST: u64 = 4 << 20;

/// The families every desktop carries whatever the files say, so that a
/// generic `sans-serif` and a glyph the named font lacks both have a face.
const ALWAYS: [&str; 2] = ["sans-serif", "monospace"];

/// The dotfiles beside `config` and the fonts they name, as image files.
///
/// # Errors
///
/// A directory that exists and cannot be read.
pub(crate) fn carried(config: &Path) -> Result<Vec<File>> {
    let Some(root) = config_root(config) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for name in CARRIED {
        let dir = root.join(name);
        if dir.is_dir() {
            walk(&dir, &format!("{CONFIG_HOME}/{name}"), &mut files)?;
        }
    }
    let texts: Vec<(String, String)> = files
        .iter()
        .filter_map(|file| match &file.content {
            Content::Bytes(bytes) => core::str::from_utf8(bytes)
                .ok()
                .map(|text| (file.path.clone(), text.to_owned())),
            _ => None,
        })
        .collect();
    let configured = files
        .iter()
        .filter(|file| matches!(file.content, Content::Bytes(_)))
        .count();
    println!(
        "  dotfiles: {configured} files from {} into /{CONFIG_HOME}",
        root.display()
    );
    let mut families: BTreeSet<String> = ALWAYS.iter().map(|name| (*name).to_owned()).collect();
    for (path, text) in &texts {
        families.extend(families_named(path, text));
    }
    files.extend(fonts(&families));
    if let Ok(text) = std::fs::read_to_string(config) {
        files.extend(launchers(&text));
    }
    Ok(files)
}

/// The programs the desktop image carries at `/bin/<name>` that a user's
/// script may name by the path of its build on their own machine.
const PORTED: [&str; 4] = ["fuzzel", "waybar", "hyprlock", "hypridle"];

/// The commands `bind` lines run with `exec`, `$variables` expanded, as
/// hyprlang expands them (longest name first).
fn bound_commands(text: &str) -> Vec<String> {
    let mut variables: Vec<(String, String)> = Vec::new();
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let line = line.split(" #").next().unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if let Some(variable) = name.strip_prefix('$') {
            variables.push((variable.to_owned(), value.to_owned()));
            variables.sort_by_key(|(name, _)| core::cmp::Reverse(name.len()));
            continue;
        }
        if !name.starts_with("bind") {
            continue;
        }
        // `MODS, key, dispatcher, argument`: the argument may hold commas.
        let mut fields = value.splitn(4, ',');
        let (Some(_), Some(_), Some(dispatcher), Some(argument)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if dispatcher.trim() != "exec" {
            continue;
        }
        let mut command = argument.trim().to_owned();
        for (variable, replacement) in &variables {
            command = command.replace(&format!("${variable}"), replacement);
        }
        found.push(command);
    }
    found
}

/// The user's own launcher scripts a bind runs, carried unchanged at the
/// absolute path the bind names -- `bind = $mainMod, R, exec, $menu` with
/// `$menu = /home/<user>/.local/bin/hypr-launcher` -- when the program is a
/// `#!/bin/sh` script, which busybox's `sh` runs as it is. Every absolute
/// path the script names whose file name is one of [`PORTED`] (its
/// `FUZZEL=/home/<user>/.local/bin/fuzzel`, a build of their own) becomes a
/// link to the image's `/bin/<name>`. Nothing else the script names is
/// carried: a helper that is not there fails as a missing program does,
/// which the design says for each one.
fn launchers(config: &str) -> Vec<File> {
    let mut out: Vec<File> = Vec::new();
    for command in bound_commands(config) {
        let Some(program) = command.split_whitespace().next() else {
            continue;
        };
        let Some(relative) = program.strip_prefix('/') else {
            continue;
        };
        let Some(bytes) = std::fs::read(program)
            .ok()
            .or_else(|| std::fs::read(in_this_home(program)?).ok())
        else {
            continue;
        };
        if !bytes.starts_with(b"#!/bin/sh\n") || out.iter().any(|file| file.path == relative) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        for path in text.split(|c: char| c.is_whitespace() || "\"'=;()<>&|$`".contains(c)) {
            let Some(name) = Path::new(path).file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(linked) = path.strip_prefix('/') else {
                continue;
            };
            if PORTED.contains(&name) && !out.iter().any(|file| file.path == linked) {
                out.push(File {
                    path: linked.to_owned(),
                    mode: 0o777,
                    content: Content::Link(format!("/bin/{name}")),
                });
            }
        }
        println!("  dotfiles: {program}, which a bind runs, carried unchanged");
        out.push(File {
            path: relative.to_owned(),
            mode: 0o755,
            content: Content::Bytes(bytes),
        });
    }
    out
}

/// This machine's home: `HOME`, or on Windows, where PowerShell and `cmd`
/// set none, `USERPROFILE`.
pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `/home/<user>/<rest>` as `<rest>` under this machine's home: where a
/// config copied from a Linux machine finds, on Windows, the scripts it
/// names by their Linux path.
fn in_this_home(path: &str) -> Option<PathBuf> {
    let (_user, rest) = path.strip_prefix("/home/")?.split_once('/')?;
    Some(home()?.join(rest))
}

/// The font the user's lock screen names first -- `font_family` in
/// `~/.config/hypr/hyprlock.conf`, `$variables` expanded -- which is the
/// face the `caption` boot draws in. `None` without such a file.
pub(crate) fn users_font() -> Option<String> {
    let path = home()?.join(".config/hypr/hyprlock.conf");
    let text = std::fs::read_to_string(path).ok()?;
    hyprlang_families(&text).into_iter().next()
}

/// Every file of each of `families` on this machine, as image files under
/// [`FONT_DIR`].
pub(crate) fn font_files(families: &[String]) -> Vec<File> {
    fonts(&families.iter().cloned().collect())
}

/// The directory `config`'s directory is in: `~/.config` for
/// `~/.config/hypr/hyprland.conf`.
fn config_root(config: &Path) -> Option<PathBuf> {
    let absolute = std::fs::canonicalize(config).ok()?;
    Some(absolute.parent()?.parent()?.to_path_buf())
}

/// Every file under `dir`, as `name/…`, symlinks followed.
fn walk(dir: &Path, name: &str, out: &mut Vec<File>) -> Result<()> {
    let mut children: Vec<_> = std::fs::read_dir(dir)
        .map_err(|error| crate::Error::new(format!("reading {}: {error}", dir.display())))?
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect();
    children.sort();
    for child in children {
        let path = dir.join(&child);
        let child = child.to_string_lossy();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let inside = format!("{name}/{child}");
        if meta.is_dir() {
            walk(&path, &inside, out)?;
        } else if meta.len() > LARGEST {
            println!(
                "  dotfiles: {} is {} KiB, left out",
                path.display(),
                meta.len() / 1024
            );
        } else if meta.is_file() {
            let bytes = std::fs::read(&path).map_err(|error| {
                crate::Error::new(format!("reading {}: {error}", path.display()))
            })?;
            let mode = if executable(&meta) { 0o755 } else { 0o644 };
            out.push(File {
                path: inside,
                mode,
                content: Content::Bytes(bytes),
            });
        }
    }
    Ok(())
}

/// The font families a configuration file names, by what kind of file its
/// path says it is.
pub(crate) fn families_named(path: &str, text: &str) -> Vec<String> {
    if path.ends_with(".css") {
        css_families(text)
    } else if path.ends_with(".ini") {
        fuzzel_families(text)
    } else if path.ends_with(".conf") {
        hyprlang_families(text)
    } else {
        Vec::new()
    }
}

/// `font-family: "Ubuntu", "DejaVu Sans", sans-serif;` in CSS.
fn css_families(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("font-family") {
        rest = rest.get(at + "font-family".len()..).unwrap_or("");
        let Some(value) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let end = value.find([';', '}']).unwrap_or(value.len());
        for family in value.get(..end).unwrap_or("").split(',') {
            let family = family.trim().trim_matches(['"', '\'']).trim();
            if !family.is_empty() {
                found.push(family.to_owned());
            }
        }
    }
    found
}

/// `font=GFS Didot:size=16,Noto Sans` in `fuzzel.ini`: fontconfig patterns,
/// comma-separated, each a family and then its `:` properties.
fn fuzzel_families(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix("font").map(str::trim_start) else {
            continue;
        };
        let Some(value) = value.strip_prefix('=') else {
            continue;
        };
        for pattern in value.split(',') {
            let family = pattern.split(':').next().unwrap_or("").trim();
            if !family.is_empty() {
                found.push(family.to_owned());
            }
        }
    }
    found
}

/// `font_family = $font Light` in a hyprlang file, `$font` expanded from the
/// file's own `$font = Ubuntu` and the value read as Pango reads a font
/// description (the families, which is what a file is resolved by; the
/// style words stay, and [`resolve`] takes them off when they are not part
/// of a family's name).
fn hyprlang_families(text: &str) -> Vec<String> {
    let mut variables: Vec<(String, String)> = Vec::new();
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let line = line.split(" #").next().unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if let Some(variable) = name.strip_prefix('$') {
            variables.push((variable.to_owned(), value.to_owned()));
            variables.sort_by_key(|(name, _)| core::cmp::Reverse(name.len()));
            continue;
        }
        if !(name == "font_family" || name.ends_with(":font_family")) {
            continue;
        }
        let mut value = value.to_owned();
        for (variable, replacement) in &variables {
            value = value.replace(&format!("${variable}"), replacement);
        }
        for family in value.split(',') {
            let family = family.trim();
            if !family.is_empty() {
                found.push(family.to_owned());
            }
        }
    }
    found
}

/// The words Pango takes off the end of a description as a style, weight,
/// or stretch rather than part of the family.
const STYLE_WORDS: [&str; 24] = [
    "thin",
    "ultra-light",
    "extra-light",
    "light",
    "semi-light",
    "book",
    "regular",
    "normal",
    "medium",
    "semi-bold",
    "demi-bold",
    "bold",
    "ultra-bold",
    "extra-bold",
    "heavy",
    "black",
    "italic",
    "oblique",
    "condensed",
    "expanded",
    "semi-condensed",
    "semi-expanded",
    "small-caps",
    "roman",
];

/// Every file of each family, resolved by the host's fontconfig.
fn fonts(families: &BTreeSet<String>) -> Vec<File> {
    if run("fc-match", &["--version"]).is_none() {
        println!("  fonts: this machine has no fontconfig; no fonts carried");
        return Vec::new();
    }
    let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
    for family in families {
        match resolve(family) {
            Some((resolved, files)) => {
                println!("  fonts: {family} -> {resolved}, {} files", files.len());
                paths.extend(files);
            }
            None => println!("  fonts: {family}: this machine has no such family; left out"),
        }
    }
    let mut out = Vec::new();
    let mut names = BTreeSet::new();
    for path in paths {
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        // Two families' files with one name: the first is kept.
        if !names.insert(name.clone()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        out.push(File {
            path: format!("{FONT_DIR}/{name}"),
            mode: 0o644,
            content: Content::Bytes(bytes),
        });
    }
    let bytes: usize = out
        .iter()
        .map(|file| match &file.content {
            Content::Bytes(bytes) => bytes.len(),
            _ => 0,
        })
        .sum();
    println!(
        "  fonts: {} files, {} KiB, into /{FONT_DIR}",
        out.len(),
        bytes / 1024
    );
    out
}

/// The family fontconfig gives `name`, and every file of it. A name whose
/// match is some other family is tried again with its trailing style words
/// taken off, as Pango reads `Ubuntu Light` (family Ubuntu, weight Light);
/// a generic name takes whatever it matches.
fn resolve(name: &str) -> Option<(String, Vec<PathBuf>)> {
    let generic = matches!(
        name.to_ascii_lowercase().as_str(),
        "sans-serif"
            | "sans"
            | "serif"
            | "monospace"
            | "mono"
            | "system-ui"
            | "cursive"
            | "fantasy"
    );
    let mut words: Vec<&str> = name.split_whitespace().collect();
    loop {
        if words.is_empty() {
            return None;
        }
        let family = words.join(" ");
        let matched = run("fc-match", &["-f", "%{family}\n", &family])?;
        let matched = matched.lines().next().unwrap_or("").to_owned();
        let same = matched
            .split(',')
            .any(|candidate| same_family(candidate, &family));
        if same || generic {
            let first = matched.split(',').next().unwrap_or("").to_owned();
            let listed = run("fc-list", &["-f", "%{file}\n", &first]).unwrap_or_default();
            let mut files: Vec<PathBuf> = listed
                .lines()
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect();
            files.sort();
            files.dedup();
            return (!files.is_empty()).then_some((first, files));
        }
        let last = words.last().map(|word| word.to_ascii_lowercase())?;
        if !STYLE_WORDS.contains(&last.as_str()) {
            return None;
        }
        let _ = words.pop();
    }
}

/// Whether two family names are one, as fontconfig compares them: case and
/// spaces ignored.
fn same_family(one: &str, two: &str) -> bool {
    let squash = |name: &str| {
        name.chars()
            .filter(|character| !character.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    squash(one) == squash(two)
}

/// A host program's standard output, or `None` if it would not run or
/// failed.
fn run(program: &str, arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_families_each_kind_of_file_names_are_found() {
        let css = "window#waybar {\n    font-family: \"Ubuntu\", 'DejaVu Sans', sans-serif;\n    font-size: 15px;\n}";
        assert_eq!(
            families_named("waybar/style.css", css),
            ["Ubuntu", "DejaVu Sans", "sans-serif"]
        );
        let ini = "[main]\nfont=GFS Didot:size=16,Noto Sans:weight=bold\nterminal=foot\n";
        assert_eq!(
            families_named("fuzzel/fuzzel.ini", ini),
            ["GFS Didot", "Noto Sans"]
        );
        let lock = "$font = Ubuntu\n# font_family = Commented Out\nlabel {\n    font_family = $font Light\n}\ninput-field {\n    font_family = $font # the same\n}\n";
        assert_eq!(
            families_named("hypr/hyprlock.conf", lock),
            ["Ubuntu Light", "Ubuntu"]
        );
        let hypr = "group {\n    groupbar {\n        font_family = GFS Didot\n    }\n}\nmisc:font_family = Inter\n";
        assert_eq!(
            families_named("hypr/hyprland.conf", hypr),
            ["GFS Didot", "Inter"]
        );
        assert!(families_named("waybar/config.jsonc", "{}").is_empty());
    }

    #[test]
    fn a_bind_that_runs_a_variable_is_expanded() {
        let conf = "$mainMod = SUPER\n$menu = /home/u/.local/bin/hypr-launcher\n\
                    bind = $mainMod, R, exec, $menu\nbindr = $mainMod, SUPER_L, exec, $menu --x\n\
                    bind = $mainMod, Q, killactive,\nexec-once = /home/u/other\n";
        assert_eq!(
            bound_commands(conf),
            vec![
                "/home/u/.local/bin/hypr-launcher".to_owned(),
                "/home/u/.local/bin/hypr-launcher --x".to_owned()
            ]
        );
    }

    #[test]
    fn a_linux_home_path_is_found_in_this_home() {
        let home = home().unwrap();
        assert_eq!(
            in_this_home("/home/u/.local/bin/hypr-launcher"),
            Some(home.join(".local/bin/hypr-launcher"))
        );
        assert_eq!(in_this_home("/usr/bin/fuzzel"), None);
    }

    #[test]
    fn a_family_name_is_compared_as_fontconfig_does() {
        assert!(same_family("DejaVu Sans", "dejavusans"));
        assert!(!same_family("Ubuntu", "Ubuntu Light"));
    }
}

#[cfg(test)]
mod probe {
    /// What `--config ~/.config/hypr/hyprland.conf` would carry on this
    /// machine, run by hand (`cargo test -p xtask dotfiles -- --ignored`).
    #[test]
    #[ignore = "reads this machine's ~/.config and fontconfig"]
    fn this_machines_dotfiles() {
        let Some(home) = super::home() else {
            return;
        };
        let config = home.join(".config/hypr/hyprland.conf");
        if !config.is_file() {
            return;
        }
        let files = super::carried(&config).unwrap_or_default();
        assert!(
            files
                .iter()
                .any(|file| file.path == ".config/hypr/hyprlock.conf")
        );
        assert!(
            files
                .iter()
                .any(|file| file.path.ends_with(".local/bin/hypr-launcher"))
        );
        // A host with no fontconfig carries no fonts, and says so.
        if super::run("fc-match", &["--version"]).is_some() {
            assert!(
                files
                    .iter()
                    .any(|file| file.path.starts_with("usr/share/fonts/host/"))
            );
        }
    }
}

/// Whether any execute bit is set: on a Unix host, from the file's mode; on
/// any other there are none to read, and a dotfile is taken as not a
/// program.
#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

/// As above: no mode bits off a Unix host.
#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    false
}
