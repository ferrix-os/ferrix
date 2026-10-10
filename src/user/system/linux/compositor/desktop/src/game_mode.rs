//! Game mode: which applications the user starts as games, and the mark a
//! launcher gives them (`docs/SETTINGS.md`).
//!
//! # The file
//!
//! `$XDG_CONFIG_HOME/ferrix/game-mode.conf`, which is
//! `~/.config/ferrix/game-mode.conf` where that is not set ([`path`]). Plain
//! text, one desktop-file ID a line -- the name a launcher knows an
//! application by, `steam.desktop` -- in any order:
//!
//! ```text
//! # Applications started in game mode, one desktop-file ID a line.
//! pattern.desktop
//! steam.desktop
//! ```
//!
//! A line that starts with `#`, and an empty one, is skipped. A line that
//! cannot be an ID (it has a space or a `/` in it) is skipped and complained
//! of. No file is no application in game mode. An ID nothing installed has
//! is kept: the application may come back.
//!
//! The Settings app writes it ([`GameMode::save`]): the whole set, sorted,
//! into a new file beside it that then takes its name, so a reader sees the
//! old set or the new one and never half of either. Anybody may write it by
//! hand; the next start of an application reads it again.
//!
//! # The mark
//!
//! A launcher asks [`mark_for_launch`] what to do to the environment of the
//! program it is about to start, and does it ([`Mark::apply`]). For an
//! application in the set that is `FERRIX_GAME_MODE=1` and
//! `FERRIX_GAME_MODE_APP=<its ID>`; for any other, both are taken *out*, so
//! that a launcher which was itself started from a game does not hand the
//! mark on to something the user never switched on. What a marked program
//! starts inherits the mark, which is how a game Steam starts has Steam's.
//!
//! That is the one hook. Whatever game mode comes to do to a program is done
//! where the mark is given or by something that reads it; nothing else
//! decides who is a game.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The variable a program started in game mode has, set to [`ON`].
pub const MARK: &str = "FERRIX_GAME_MODE";

/// The variable that names the application the mark was given for: its
/// desktop-file ID.
pub const MARK_APP: &str = "FERRIX_GAME_MODE_APP";

/// What [`MARK`] is set to.
pub const ON: &str = "1";

/// The file's place under the configuration directory.
const RELATIVE: &str = "ferrix/game-mode.conf";

/// What the file starts with when Settings writes it.
const HEADER: &str = "\
# Ferrix game mode: the applications started in game mode, one desktop-file
# ID a line. Written by Settings (/bin/settings); edit it freely. A launcher
# reads it each time it starts an application (docs/SETTINGS.md).
";

/// Where the user's file is: under `XDG_CONFIG_HOME`, or `~/.config` without
/// one. `None` when the environment `var` answers for has neither, which is
/// a program with no user to ask.
#[must_use]
pub fn path(var: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = match var("XDG_CONFIG_HOME").filter(|base| !base.is_empty()) {
        Some(base) => PathBuf::from(base),
        None => PathBuf::from(var("HOME").filter(|home| !home.is_empty())?).join(".config"),
    };
    Some(base.join(RELATIVE))
}

/// Whether `id` can be a line of the file: a desktop-file ID is a file's
/// name, so it has no `/`, and a line is one ID, so it has no white space
/// and does not look like a comment.
#[must_use]
pub fn is_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('#')
        && !id
            .chars()
            .any(|c| c == '/' || c.is_whitespace() || c.is_control())
}

/// The applications in game mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameMode {
    /// Their desktop-file IDs.
    apps: BTreeSet<String>,
}

impl GameMode {
    /// Read the file's text: the set, and a complaint for each line that is
    /// not an ID, naming the line.
    #[must_use]
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut apps = BTreeSet::new();
        let mut complaints = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if is_id(line) {
                let _ = apps.insert(line.to_owned());
            } else {
                complaints.push(format!(
                    "line {}: {line:?} is not a desktop-file ID, and is skipped",
                    index.saturating_add(1)
                ));
            }
        }
        (Self { apps }, complaints)
    }

    /// The file's text for this set: the header, then each ID on a line of
    /// its own, sorted, so the same set is always the same bytes.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = HEADER.to_owned();
        for id in &self.apps {
            text.push_str(id);
            text.push('\n');
        }
        text
    }

    /// Read the file at `path`. A file that is not there is the empty set.
    ///
    /// # Errors
    ///
    /// The file is there and cannot be read, or is not text.
    pub fn load(path: &Path) -> std::io::Result<(Self, Vec<String>)> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok((Self::default(), Vec::new()))
            }
            Err(error) => Err(error),
        }
    }

    /// Write the set to `path`, whole or not at all: the text goes into a
    /// new file in the same directory, is synchronised, and is then renamed
    /// over `path`. The directory is made if it is missing. The file is
    /// `0644`: other programs of the user's, and other users, may read which
    /// applications are games; only the user writes it.
    ///
    /// # Errors
    ///
    /// The directory cannot be made, or the new file cannot be written or
    /// renamed; `path` is then what it was.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let directory = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the file has no directory",
            )
        })?;
        std::fs::create_dir_all(directory)?;
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "the file has no name")
        })?;
        let mut hidden = std::ffi::OsString::from(".");
        hidden.push(name);
        hidden.push(format!(".{}.new", std::process::id()));
        let new = directory.join(hidden);
        // Left by a writer that died with this process's number.
        let _ = std::fs::remove_file(&new);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&new)
            .and_then(|mut file| {
                file.write_all(self.render().as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&new, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&new);
        }
        written?;
        // The rename itself, kept across a power failure where the
        // filesystem can: a directory that cannot be synchronised (tmpfs)
        // loses nothing by it.
        if let Ok(directory) = std::fs::File::open(directory) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    /// Whether `id` is in game mode.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.apps.contains(id)
    }

    /// Put `id` in game mode, or take it out. Answers whether the set
    /// changed; an `id` that cannot be a line of the file changes nothing.
    pub fn set(&mut self, id: &str, on: bool) -> bool {
        if on {
            is_id(id) && self.apps.insert(id.to_owned())
        } else {
            self.apps.remove(id)
        }
    }

    /// The IDs, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.apps.iter().map(String::as_str)
    }

    /// How many applications are in game mode.
    #[must_use]
    pub fn len(&self) -> usize {
        self.apps.len()
    }

    /// Whether none is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// What a launcher does to the environment of a program it starts for
    /// the application `id` (`None` for a command that is no application's).
    #[must_use]
    pub fn mark(&self, id: Option<&str>) -> Mark {
        match id.filter(|id| self.contains(id)) {
            Some(id) => Mark {
                app: Some(id.to_owned()),
            },
            None => Mark { app: None },
        }
    }
}

/// What starting one program in or out of game mode does to its environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    /// The application the program is started in game mode for, or `None`
    /// for a program that is not.
    app: Option<String>,
}

impl Mark {
    /// Whether the program is started in game mode.
    #[must_use]
    pub fn is_game(&self) -> bool {
        self.app.is_some()
    }

    /// The application it is in game mode for.
    #[must_use]
    pub fn app(&self) -> Option<&str> {
        self.app.as_deref()
    }

    /// The variables the program is given.
    #[must_use]
    pub fn set(&self) -> Vec<(&'static str, String)> {
        match &self.app {
            Some(app) => vec![(MARK, ON.to_owned()), (MARK_APP, app.clone())],
            None => Vec::new(),
        }
    }

    /// The variables taken out of what the program would inherit: both, for
    /// a program not in game mode, so a mark is never passed on by accident.
    #[must_use]
    pub fn unset(&self) -> &'static [&'static str] {
        match self.app {
            Some(_) => &[],
            None => &[MARK, MARK_APP],
        }
    }

    /// Do both to `command`.
    pub fn apply(&self, command: &mut std::process::Command) {
        for name in self.unset() {
            let _ = command.env_remove(name);
        }
        let _ = command.envs(self.set());
    }
}

/// The mark for starting application `id` now: the user's file is read
/// afresh, so a switch turned in Settings counts from the next start on.
/// Also answers what there is to complain of: lines of the file that are no
/// ID, or the reason the file could not be read -- in which case nothing is
/// in game mode, since a launcher that cannot read a preference must still
/// start the program.
#[must_use]
pub fn mark_for_launch(
    id: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
) -> (Mark, Vec<String>) {
    let Some(path) = path(var) else {
        return (Mark::default(), Vec::new());
    };
    match GameMode::load(&path) {
        Ok((set, complaints)) => (
            set.mark(id),
            complaints
                .into_iter()
                .map(|complaint| format!("{}: {complaint}", path.display()))
                .collect(),
        ),
        Err(error) => (
            Mark::default(),
            vec![format!(
                "{}: {error}; nothing is started in game mode",
                path.display()
            )],
        ),
    }
}

/// Whether the environment `var` answers for is a program's that was started
/// in game mode, and for which application: what a program, or something
/// acting for it, reads to learn it is a game.
#[must_use]
pub fn marked(var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    (var(MARK).as_deref() == Some(ON)).then(|| var(MARK_APP).unwrap_or_default())
}

#[cfg(test)]
mod tests;
