//! Which kinds of event the log takes, and at what level
//! (`docs/LOGGING.md` §18 "Events"; the customer, 2026-10-23: each new
//! kind can be switched, so not every program start or window is logged).
//!
//! `/etc/ferrix/lode.d/events.conf`, one kind a line, read by lode at start
//! and on `lode reload`, and by the programs that make the events -- hyprix
//! for windows and its programs' streams -- each time they would make one;
//! a person's `~/.config/ferrix/lode/events.conf` overrides the kinds made
//! in their session:
//!
//! ```text
//! window-opened   off             # every window that opens, with its title
//! app-error       on   warning    # an error dialog: a record and a screenshot
//! session-streams off             # a desktop program's output under its own name
//! crash-names     on              # a crash record names its process
//! ```
//!
//! Which records of a kind are kept is the rules' (`*.rules`): `drop where
//! event = window-opened and id = waybar` is one line. A kind's record says
//! its kind in `FERRIX_EVENT`.

use alloc::string::String;
use alloc::vec::Vec;

use crate::limit::RuleError;
use crate::record::level;

/// A kind of event: its name, whether it is on by default, its level, and
/// what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    /// As `events.conf` and `FERRIX_EVENT` name it.
    pub name: &'static str,
    /// On unless a file says otherwise.
    pub on: bool,
    /// The level its records get.
    pub level: u8,
    /// Who makes it.
    pub made_by: &'static str,
    /// What it is.
    pub what: &'static str,
}

/// Every kind, with its defaults: errors and crashes on, what could flood
/// off.
pub const KINDS: [Kind; 6] = [
    Kind {
        name: "window-opened",
        on: false,
        level: 6,
        made_by: "hyprix",
        what: "a window opened: its title, app id and process",
    },
    Kind {
        name: "app-error",
        on: true,
        level: 4,
        made_by: "hyprix",
        what: "a window that looks like an error dialog, with a screenshot when capture is on",
    },
    Kind {
        name: "session-streams",
        on: false,
        level: 6,
        made_by: "hyprix",
        what: "each program the desktop starts writes under its own name",
    },
    Kind {
        name: "crash-names",
        on: true,
        level: 3,
        made_by: "lode",
        what: "a crash record names its process: name, path, module, command line",
    },
    Kind {
        name: "process-names",
        on: true,
        level: 6,
        made_by: "lode",
        what: "a record with _PID gets _COMM, _EXE and _CMDLINE, read while the process writes",
    },
    Kind {
        name: "capture",
        on: true,
        level: 6,
        made_by: "lode",
        what: "crashes and app errors take a screenshot (capture.conf's Capture= too)",
    },
];

/// One kind's setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// The kind.
    pub name: String,
    /// On or off.
    pub on: bool,
    /// Its level.
    pub level: u8,
    /// Where it was set: `default`, or the file's path.
    pub from: String,
}

/// Every kind's setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Events {
    /// One a kind, in [`KINDS`]' order.
    pub settings: Vec<Setting>,
}

impl Default for Events {
    fn default() -> Self {
        Events {
            settings: KINDS
                .iter()
                .map(|kind| Setting {
                    name: String::from(kind.name),
                    on: kind.on,
                    level: kind.level,
                    from: String::from("default"),
                })
                .collect(),
        }
    }
}

impl Events {
    /// Apply a file's lines, `from` its path, over what is set: the
    /// defaults, or the system's under a person's.
    ///
    /// # Errors
    ///
    /// The first line that is not `KIND on|off [LEVEL]`; nothing of the
    /// file is applied then.
    pub fn apply(&mut self, text: &str, from: &str) -> Result<(), RuleError> {
        let mut changes = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
            if line.is_empty() {
                continue;
            }
            let error = |why: &str| RuleError {
                line: index + 1,
                why: String::from(why),
            };
            let words: Vec<&str> = line.split_whitespace().collect();
            let (name, state, given) = match words.as_slice() {
                [name, state] => (*name, *state, None),
                [name, state, given] => (*name, *state, Some(*given)),
                _ => return Err(error("KIND on|off [LEVEL]")),
            };
            if !KINDS.iter().any(|kind| kind.name == name) {
                return Err(error(
                    "not a kind of event lode knows (lode events lists them)",
                ));
            }
            let on = match state {
                "on" | "yes" | "true" => true,
                "off" | "no" | "false" => false,
                _ => return Err(error("on or off")),
            };
            let level = given
                .map(|word| {
                    level(word)
                        .filter(|&level| level < 8)
                        .ok_or_else(|| error("a level"))
                })
                .transpose()?;
            changes.push((String::from(name), on, level));
        }
        for (name, on, level) in changes {
            if let Some(setting) = self
                .settings
                .iter_mut()
                .find(|setting| setting.name == name)
            {
                setting.on = on;
                if let Some(level) = level {
                    setting.level = level;
                }
                setting.from = String::from(from);
            }
        }
        Ok(())
    }

    /// Whether `kind` is on.
    #[must_use]
    pub fn on(&self, kind: &str) -> bool {
        self.settings
            .iter()
            .find(|setting| setting.name == kind)
            .is_some_and(|setting| setting.on)
    }

    /// `kind`'s level.
    #[must_use]
    pub fn level(&self, kind: &str) -> u8 {
        self.settings
            .iter()
            .find(|setting| setting.name == kind)
            .map_or(6, |setting| setting.level)
    }

    /// `lode events`' table.
    #[must_use]
    pub fn table(&self) -> String {
        let mut text = alloc::format!(
            "{:<16} {:<4} {:<8} {:<8} {}\n",
            "KIND",
            "ON",
            "LEVEL",
            "BY",
            "SET IN"
        );
        for setting in &self.settings {
            let kind = KINDS.iter().find(|kind| kind.name == setting.name);
            text.push_str(&alloc::format!(
                "{:<16} {:<4} {:<8} {:<8} {}\n",
                setting.name,
                if setting.on { "on" } else { "off" },
                crate::record::LEVELS
                    .get(usize::from(setting.level))
                    .copied()
                    .unwrap_or("?"),
                kind.map_or("", |kind| kind.made_by),
                setting.from
            ));
        }
        text
    }
}
