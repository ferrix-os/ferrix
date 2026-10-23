//! `settings.conf`: the settings a person chose in Ferrix's Settings app,
//! kept so they survive a restart (`docs/RUNTIME-SETTINGS.md` §3.3).
//!
//! The file is hyprlang, read after the configuration as if a `source`
//! line ended it -- unless the configuration sources it itself, which
//! puts it where that line is -- so a choice made in Settings wins over
//! the same option in `hyprland.conf`. It lives at
//! `$XDG_CONFIG_HOME/hypr/settings.conf`, else
//! `~/.config/hypr/settings.conf`, beside the configuration a person
//! edits by hand.
//!
//! The Settings app does not write it. It asks the compositor:
//!
//! * `hyprctl setting KEY VALUE` checks the value as `hyprctl keyword`
//!   would read it, writes `KEY = VALUE` into the file in place of the line
//!   for the same key, and then puts it in force at once, as `keyword`
//!   does. An answer other than `ok` is what was wrong, and nothing was
//!   written or changed.
//! * `hyprctl setting reset KEY` takes the key's line out of the file and
//!   reads the configuration again, so the key is back to what
//!   `hyprland.conf` or Hyprland's default says.
//! * `hyprctl settings` lists the file's settings; `-j` as JSON.
//!
//! A key is an option (`general:gaps_in`) or a `wallpaper` line, which is
//! one setting for each output: `wallpaper DP-1` is the key of
//! `wallpaper = DP-1, …`, and `wallpaper` alone that of the line for every
//! screen. Lines the compositor did not write -- comments, a person's own
//! -- are kept as they are.

use std::io;
use std::path::{Path, PathBuf};

use compositor_config::{Config, Wallpaper};
use compositor_ipc::{Format, Reply, Request};

/// Where the file is when the compositor is started from the command line;
/// `None` with neither `$XDG_CONFIG_HOME` nor `$HOME`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(config).join("hypr/settings.conf"));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/hypr/settings.conf"))
}

/// What the file starts with when the compositor writes it first.
const HEADER: &str = "\
# Ferrix's Settings: what `hyprctl setting KEY VALUE` keeps, read by hyprix
# after hyprland.conf, so these win. Change them in Settings, or with
# `hyprctl setting`; a line edited here takes at the next `hyprctl reload`.
";

/// A `hyprctl setting` or `settings` request answered: what to write back,
/// and what the compositor is to do after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    /// The answer: `ok`, or what was wrong.
    pub answer: String,
    /// What the compositor does: the keyword put in force, or a reload.
    pub then: Option<Reply>,
}

/// Answer `request` when it is one of this module's, with the file at
/// `path`; `None` for any other request.
#[must_use]
pub fn answer(request: &Request, path: Option<&Path>) -> Option<Answered> {
    let said = |answer: String| Answered { answer, then: None };
    match request.command.as_str() {
        "settings" => Some(said(match path {
            Some(path) => listed(path, request.flags.format()),
            None => "settings: no settings file (no $XDG_CONFIG_HOME or $HOME)\n".to_owned(),
        })),
        "setting" => {
            let Some(path) = path else {
                return Some(said(
                    "setting: no settings file (no $XDG_CONFIG_HOME or $HOME)\n".to_owned(),
                ));
            };
            let argument = request.argument.trim();
            if let Some(key) = argument.strip_prefix("reset ") {
                return Some(match forget(path, key.trim()) {
                    Ok(_) => Answered {
                        answer: "ok\n".to_owned(),
                        then: Some(Reply::Reload),
                    },
                    Err(error) => said(format!("setting reset: {}: {error}\n", path.display())),
                });
            }
            let (name, value) = argument
                .split_once(char::is_whitespace)
                .map_or((argument, ""), |(name, value)| (name, value.trim()));
            Some(match checked(name, value) {
                Err(why) => said(format!("setting {name}: {why}\n")),
                Ok(value) => match save(path, name, &value) {
                    Ok(()) => Answered {
                        answer: "ok\n".to_owned(),
                        then: Some(Reply::Keyword {
                            name: name.to_owned(),
                            value,
                        }),
                    },
                    Err(error) => said(format!("setting {name}: {}: {error}\n", path.display())),
                },
            })
        }
        _ => None,
    }
}

/// `value` for key `name` as it is to be written, once it has been read as
/// `hyprctl keyword` would read it: a `wallpaper` line as the compositor
/// writes one back, anything else as given.
///
/// # Errors
///
/// What is wrong with the key or the value.
pub fn checked(name: &str, value: &str) -> Result<String, String> {
    if name.is_empty() || value.is_empty() {
        return Err("setting KEY VALUE, or setting reset KEY".to_owned());
    }
    if name == "wallpaper" {
        return match Wallpaper::parse(value)? {
            (_, Some(line)) => Ok(line.line()),
            (output, None) => Ok(format!("{output}, off")),
        };
    }
    if compositor_config::default_of(name).is_none() {
        return Err(format!(
            "only options and wallpaper lines are kept here; {name} is neither \
             (monitors are kept by `keyword monitor`)"
        ));
    }
    Config::default().keyword(name, value)?;
    Ok(value.to_owned())
}

/// The key a line of the file is, `key = value` with no category around
/// it: the option's name, or `wallpaper OUTPUT` for a wallpaper line;
/// `None` for anything else.
fn key_of(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let (name, value) = line.split_once('=')?;
    key(name.trim(), value.trim())
}

/// The key of setting `name` with value `value`.
fn key(name: &str, value: &str) -> Option<String> {
    if name == "wallpaper" {
        let output = value.split(',').next().unwrap_or_default().trim();
        return Some(if output.is_empty() {
            "wallpaper".to_owned()
        } else {
            format!("wallpaper {output}")
        });
    }
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_owned())
}

/// Write `name = value` into the file at `path`, in place of the line with
/// the same key, or at its end; made with its directory if it is not
/// there. Written whole beside it and renamed over it, so a crash leaves
/// the old file or the new one, never half of one.
///
/// # Errors
///
/// What the file system said.
pub fn save(path: &Path, name: &str, value: &str) -> io::Result<()> {
    let wanted = key(name, value);
    // A `#` in a value is a comment to hyprlang unless doubled.
    let written = format!("{name} = {}", value.replace('#', "##"));
    let text = read(path)?;
    let mut lines: Vec<String> = Vec::new();
    let mut placed = false;
    for line in text.lines() {
        if wanted.is_some() && key_of(line) == wanted {
            if !placed {
                lines.push(written.clone());
                placed = true;
            }
            continue;
        }
        lines.push(line.to_owned());
    }
    if !placed {
        lines.push(written);
    }
    write(path, &lines)
}

/// Take the line with key `wanted` out of the file at `path`. Gives
/// whether there was one.
///
/// # Errors
///
/// What the file system said.
pub fn forget(path: &Path, wanted: &str) -> io::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let kept: Vec<String> = text
        .lines()
        .filter(|line| key_of(line).as_deref() != Some(wanted))
        .map(str::to_owned)
        .collect();
    if kept.len() == text.lines().count() {
        return Ok(false);
    }
    write(path, &kept)?;
    Ok(true)
}

/// The settings in the file, `key = value` a line, or as JSON
/// `[{"key": …, "name": …, "value": …}]`.
#[must_use]
pub fn listed(path: &Path, format: Format) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let settings: Vec<(String, String, String)> = text
        .lines()
        .filter_map(|line| {
            let key = key_of(line)?;
            let (name, value) = line.split_once('=')?;
            Some((key, name.trim().to_owned(), value.trim().replace("##", "#")))
        })
        .collect();
    match format {
        Format::Json => {
            let mut out = String::from("[");
            for (index, (key, name, value)) in settings.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{{\"key\": {}, \"name\": {}, \"value\": {}}}",
                    json(key),
                    json(name),
                    json(value)
                ));
            }
            out.push_str("]\n");
            out
        }
        Format::Readable => settings
            .iter()
            .map(|(_, name, value)| format!("{name} = {value}\n"))
            .collect(),
    }
}

/// `text` as a JSON string.
fn json(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The file's text, or the header for a file that is not there yet.
fn read(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(HEADER.to_owned()),
        Err(error) => Err(error),
    }
}

/// `lines` written to `path` whole: beside it, then renamed over it.
fn write(path: &Path, lines: &[String]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".new");
    let partial = PathBuf::from(partial);
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hyprix-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("hypr/settings.conf")
    }

    /// A key written twice is one line, the second value; other keys,
    /// comments and a person's own lines stay; reset takes the line out.
    #[test]
    fn a_key_is_one_line_and_the_rest_is_kept() {
        let path = scratch("one-line");
        save(&path, "general:gaps_in", "5").expect("written");
        save(&path, "decoration:rounding", "8").expect("written");
        save(&path, "general:gaps_in", "10").expect("written");
        save(&path, "wallpaper", ", colour ff000000").expect("written");
        save(&path, "wallpaper", "DP-1, off").expect("written");
        save(
            &path,
            "wallpaper",
            ", picture /a.png, fit fill, colour ff000000",
        )
        .expect("written");
        let text = std::fs::read_to_string(&path).expect("there");
        assert!(text.starts_with("# Ferrix's Settings"), "{text}");
        let settings: Vec<&str> = text.lines().filter(|line| !line.starts_with('#')).collect();
        assert_eq!(
            settings,
            [
                "general:gaps_in = 10",
                "decoration:rounding = 8",
                "wallpaper = , picture /a.png, fit fill, colour ff000000",
                "wallpaper = DP-1, off",
            ]
        );
        assert_eq!(forget(&path, "wallpaper DP-1").ok(), Some(true));
        assert_eq!(forget(&path, "wallpaper DP-1").ok(), Some(false));
        assert_eq!(
            listed(&path, Format::Readable),
            "general:gaps_in = 10\ndecoration:rounding = 8\n\
             wallpaper = , picture /a.png, fit fill, colour ff000000\n"
        );
        assert!(listed(&path, Format::Json).starts_with(
            "[{\"key\": \"general:gaps_in\", \"name\": \"general:gaps_in\", \"value\": \"10\"}"
        ));
        let _ = std::fs::remove_dir_all(path.parent().and_then(Path::parent).expect("a dir"));
    }

    /// A value is checked as `keyword` reads it, and refused before
    /// anything is written; a wallpaper line is written as read.
    #[test]
    fn a_value_is_checked_before_it_is_written() {
        assert!(checked("general:gaps_in", "ten").is_err());
        assert!(checked("general:no_such_option", "1").is_err());
        assert!(checked("bind", "SUPER, Q, exit").is_err());
        assert!(checked("general:gaps_in", "").is_err());
        assert_eq!(checked("input:repeat_rate", "40").as_deref(), Ok("40"));
        assert_eq!(
            checked("wallpaper", ", colour #102030").as_deref(),
            Ok(", colour ff102030")
        );
        assert_eq!(
            checked("wallpaper", "DP-1, off").as_deref(),
            Ok("DP-1, off")
        );
        let path = scratch("refused");
        let request = Request::parse("setting general:gaps_in ten");
        let answered = answer(&request, Some(&path)).expect("ours");
        assert!(
            answered.answer.starts_with("setting general:gaps_in:"),
            "{answered:?}"
        );
        assert_eq!(answered.then, None);
        assert!(!path.exists(), "a refused value was written");
    }
}
