//! A saved arrangement for each set of monitors (`docs/DESKTOP.md` §3.2,
//! step 3d), as a floating desktop keeps one: the laptop's panel alone, the
//! panel with the office's two screens, each remembered apart.
//!
//! When a person arranges the screens -- through output management
//! (`wlr-randr`, a settings program) or `hyprctl keyword monitor` -- the
//! arrangement is written down under the set of monitors connected, each
//! named by what it says it is (its EDID description) or, with none, by
//! its connector. When that set is connected again its lines are put in
//! force after the configuration's own `monitor =` lines, which still win
//! where they name a monitor.
//!
//! The file is `$XDG_STATE_HOME/hyprix/monitors.conf`
//! (`~/.local/state/hyprix/monitors.conf`), in `monitor =` lines grouped
//! under `[set ...]` headers, so a person can read and edit it.

use std::path::{Path, PathBuf};

/// One monitor as the arrangement sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    /// Its connector's name.
    pub name: String,
    /// What it says it is; empty when it does not say.
    pub description: String,
    /// Its mode.
    pub size: (u32, u32),
    /// Its refresh in millihertz.
    pub refresh_mhz: i32,
    /// Where it is, in logical pixels.
    pub at: (i64, i64),
    /// Its scale.
    pub scale: f64,
}

impl Placed {
    /// What the monitor is called in a set and in its line: `desc:` and its
    /// description where it has one, its connector where it does not.
    #[must_use]
    pub fn selector(&self) -> String {
        if self.description.is_empty() {
            self.name.clone()
        } else {
            format!("desc:{}", self.description)
        }
    }

    /// Its `monitor =` line's value.
    #[must_use]
    pub fn rule(&self) -> String {
        format!(
            "{}, {}x{}@{}.{:03}, {}x{}, {}",
            self.selector(),
            self.size.0,
            self.size.1,
            self.refresh_mhz / 1000,
            self.refresh_mhz.rem_euclid(1000),
            self.at.0,
            self.at.1,
            self.scale
        )
    }
}

/// The key a set of monitors is saved under: their selectors, sorted, so
/// that the order the connectors come in does not matter.
#[must_use]
pub fn key(monitors: &[Placed]) -> String {
    let mut selectors: Vec<String> = monitors.iter().map(Placed::selector).collect();
    selectors.sort();
    selectors.join(" + ")
}

/// Every saved set: its key and its `monitor =` values, in file order.
#[must_use]
pub fn parse(text: &str) -> Vec<(String, Vec<String>)> {
    let mut sets: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(set) = line
            .strip_prefix("[set ")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            sets.push((set.trim().to_owned(), Vec::new()));
        } else if let Some(value) = line
            .strip_prefix("monitor")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('='))
            && let Some((_, rules)) = sets.last_mut()
        {
            rules.push(value.trim().to_owned());
        }
    }
    sets
}

/// The file's text for `sets`.
#[must_use]
pub fn render(sets: &[(String, Vec<String>)]) -> String {
    let mut out = String::from(
        "# Written by hyprix: the arrangement of each set of monitors, put in force\n\
         # when that set is connected (docs/DESKTOP.md §3.2). The configuration's\n\
         # own `monitor =` lines win where they name a monitor.\n",
    );
    for (set, rules) in sets {
        out.push_str(&format!("\n[set {set}]\n"));
        for rule in rules {
            out.push_str(&format!("monitor = {rule}\n"));
        }
    }
    out
}

/// `sets` with `monitors`' arrangement put under their key, replacing what
/// was there.
#[must_use]
pub fn remember(
    mut sets: Vec<(String, Vec<String>)>,
    monitors: &[Placed],
) -> Vec<(String, Vec<String>)> {
    let set = key(monitors);
    let rules: Vec<String> = monitors.iter().map(Placed::rule).collect();
    match sets.iter_mut().find(|(known, _)| *known == set) {
        Some((_, held)) => *held = rules,
        None => sets.push((set, rules)),
    }
    sets
}

/// The saved `monitor =` values for the set `monitors` is, if one was
/// saved.
#[must_use]
pub fn recall(sets: &[(String, Vec<String>)], monitors: &[Placed]) -> Option<Vec<String>> {
    let set = key(monitors);
    sets.iter()
        .find(|(known, _)| *known == set)
        .map(|(_, rules)| rules.clone())
}

/// Where the file is, from the environment: `$XDG_STATE_HOME/hyprix`, or
/// `~/.local/state/hyprix`. `None` with neither.
#[must_use]
pub fn default_dir() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|state| !state.is_empty()) {
        return Some(PathBuf::from(state).join("hyprix"));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/hyprix"))
}

/// The sets saved under `dir`; none for a file that is not there.
#[must_use]
pub fn load(dir: &Path) -> Vec<(String, Vec<String>)> {
    std::fs::read_to_string(dir.join("monitors.conf"))
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// Save `sets` under `dir`, made if it is not there.
///
/// # Errors
///
/// What the file system said.
pub fn save(dir: &Path, sets: &[(String, Vec<String>)]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join("monitors.conf");
    let partial = dir.join("monitors.conf.new");
    std::fs::write(&partial, render(sets))?;
    std::fs::rename(partial, file)
}

#[cfg(test)]
mod tests {
    use super::{Placed, key, parse, recall, remember, render};

    fn screen(name: &str, description: &str, at: (i64, i64)) -> Placed {
        Placed {
            name: name.to_owned(),
            description: description.to_owned(),
            size: (1920, 1080),
            refresh_mhz: 60_000,
            at,
            scale: 1.0,
        }
    }

    /// A set is the same set whatever order its monitors come in, and its
    /// arrangement comes back after a round trip through the file.
    #[test]
    fn a_set_is_remembered_and_recalled_whatever_its_order() {
        let panel = screen("eDP-1", "", (0, 0));
        let office = screen("DP-1", "Dell Inc. DELL U2415 XKV0", (1920, 0));
        assert_eq!(
            key(&[panel.clone(), office.clone()]),
            key(&[office.clone(), panel.clone()])
        );
        let sets = remember(Vec::new(), &[panel.clone(), office.clone()]);
        let sets = remember(sets, std::slice::from_ref(&panel));
        let read = parse(&render(&sets));
        assert_eq!(read, sets);
        assert_eq!(
            recall(&read, &[office, panel.clone()]),
            Some(vec![
                "eDP-1, 1920x1080@60.000, 0x0, 1".to_owned(),
                "desc:Dell Inc. DELL U2415 XKV0, 1920x1080@60.000, 1920x0, 1".to_owned(),
            ])
        );
        assert!(recall(&read, &[screen("HDMI-A-1", "", (0, 0))]).is_none());
    }

    /// A saved line is one the configuration's own parser reads.
    #[test]
    fn a_saved_line_is_a_monitor_rule() {
        let line = screen("DP-1", "Dell Inc. DELL U2415 XKV0", (1920, 0)).rule();
        let (rules, refused) = compositor_config::MonitorRule::read_all([line.as_str()]);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(rules.len(), 1);
        assert!(rules[0].matches("DP-1", "Dell Inc. DELL U2415 XKV0"));
    }
}
