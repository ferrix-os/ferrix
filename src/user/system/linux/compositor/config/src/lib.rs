//! `hyprland.conf`, parsed into the compositor's configuration.
//!
//! Hyprland's configuration language is hyprlang: one `key = value` per line,
//! `#` comments with `##` for a literal `#`, `category { … }` blocks that may
//! nest and the `category:key = value` shorthand for them, `$variables`
//! expanded in values, keywords such as `bind`, `windowrule` and `exec-once`
//! that add to a list rather than set an option, and `source` to read
//! another file in place.
//!
//! As Hyprland does, a line that fails does not stop the file: it becomes a
//! [`Diagnostic`] carrying Hyprland's wording, and every other line applies.
//! [`parse`] therefore never fails; it returns the configuration and the
//! diagnostics side by side.
//!
//! Nothing here touches a file system except [`FsSources`], so the parser is
//! a pure function of the text and of what `source` resolves to, which the
//! tests and the fuzz target give it from memory.
//!
//! What is not handled yet, each reported as a diagnostic rather than
//! misread: hyprlang's `# hyprlang` directives, `{{ }}` expressions, globs in
//! `source`, and keyed categories such as `device[name] { … }`.

mod bind;
mod layer;
mod monitor;
mod options;
mod parse;
mod rule;
mod value;
mod wallpaper;
mod workspace;

#[cfg(test)]
mod tests;

use core::fmt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use bind::{Bind, BindFlags, Key, Mods};
pub use layer::{LayerEffect, LayerRule, Layered};
pub use monitor::{AddedReserved, Mode, MonitorRule, Position, Scale, Transform};
pub use options::{OptionValue, default_of};
pub use parse::{parse, parse_over};
pub use rule::{Decoration, Effect, GroupRules, Length, Matcher, Window, WindowRule, is_prop};
pub use value::{
    Color, Gaps, Gradient, MAX_GRADIENT_COLORS, parse_color, parse_float, parse_gaps,
    parse_gradient, parse_int,
};
pub use wallpaper::{SLIDESHOW_EVERY, WALLPAPER_COLOUR, Wallpaper, WallpaperFit, WallpaperSource};
pub use workspace::{Which, WorkspaceRule, rules_for};

/// A keyword line kept as written, for the parts of the compositor that
/// interpret it: window and layer rules, monitors, workspaces, animations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
    /// The keyword, such as `windowrulev2`.
    pub keyword: String,
    /// The value, variables expanded, trimmed.
    pub value: String,
}

/// The compositor's configuration: options, and the lists keywords build.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    options: BTreeMap<&'static str, OptionValue>,
    /// `$name = value` definitions, by name without the `$`.
    pub variables: BTreeMap<String, String>,
    /// Key bindings in the order they were written, `unbind` applied.
    pub binds: Vec<Bind>,
    /// `windowrule` and `windowrulev2` lines, in order.
    pub window_rules: Vec<Raw>,
    /// `layerrule` lines, in order.
    pub layer_rules: Vec<Raw>,
    /// `plugin` lines, in order: the programs the compositor starts and
    /// gives its control socket to.
    pub plugins: Vec<String>,
    /// `monitor` lines, in order.
    pub monitors: Vec<Raw>,
    /// `workspace` lines, in order.
    pub workspaces: Vec<Raw>,
    /// `animation` and `bezier` lines, in order.
    pub animations: Vec<Raw>,
    /// `exec-once` commands: run once, at startup.
    pub exec_once: Vec<String>,
    /// `exec` commands: run at startup and on every reload.
    pub exec: Vec<String>,
    /// `exec-shutdown` commands: run on exit.
    pub exec_shutdown: Vec<String>,
    /// `env = NAME, value` pairs, in order.
    pub env: Vec<(String, String)>,
    /// `wallpaper` lines, Ferrix's: at most one for each output, the last
    /// written, and `off` taking one away ([`Wallpaper`]).
    pub wallpapers: Vec<Wallpaper>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            options: options::OPTIONS
                .iter()
                .map(|&(name, default)| (name, default.value()))
                .collect(),
            variables: BTreeMap::new(),
            binds: Vec::new(),
            window_rules: Vec::new(),
            layer_rules: Vec::new(),
            plugins: Vec::new(),
            monitors: Vec::new(),
            workspaces: Vec::new(),
            animations: Vec::new(),
            exec_once: Vec::new(),
            exec: Vec::new(),
            exec_shutdown: Vec::new(),
            env: Vec::new(),
            wallpapers: Vec::new(),
        }
    }
}

impl Config {
    /// The value of option `name`, such as `general:gaps_in`, if the
    /// compositor has that option.
    #[must_use]
    pub fn option(&self, name: &str) -> Option<&OptionValue> {
        self.options.get(name)
    }

    /// Every option and its value, sorted by name.
    pub fn options(&self) -> impl Iterator<Item = (&'static str, &OptionValue)> {
        self.options.iter().map(|(&name, value)| (name, value))
    }

    /// Option `name` as an integer, if it is one.
    #[must_use]
    pub fn int(&self, name: &str) -> Option<i64> {
        match self.option(name) {
            Some(OptionValue::Int(value)) => Some(*value),
            _ => None,
        }
    }

    /// Option `name` as a boolean: an integer other than zero.
    #[must_use]
    pub fn bool(&self, name: &str) -> Option<bool> {
        self.int(name).map(|value| value != 0)
    }

    /// Option `name` as a float, if it is one.
    #[must_use]
    pub fn float(&self, name: &str) -> Option<f64> {
        match self.option(name) {
            Some(OptionValue::Float(value)) => Some(*value),
            _ => None,
        }
    }

    /// Option `name` as a string, if it is one.
    #[must_use]
    pub fn str(&self, name: &str) -> Option<&str> {
        match self.option(name) {
            Some(OptionValue::Str(value)) => Some(value),
            _ => None,
        }
    }

    /// Option `name` as a gradient, if it is one.
    #[must_use]
    pub fn gradient(&self, name: &str) -> Option<&Gradient> {
        match self.option(name) {
            Some(OptionValue::Gradient(value)) => Some(value),
            _ => None,
        }
    }

    /// Option `name` as gaps, if it is one.
    #[must_use]
    pub fn gaps(&self, name: &str) -> Option<Gaps> {
        match self.option(name) {
            Some(OptionValue::Gaps(value)) => Some(*value),
            _ => None,
        }
    }

    /// Apply one `key = value` as a line of the file at top level would,
    /// which is what `hyprctl keyword` does. `source` is refused here: a
    /// keyword sent over IPC names no file to resolve a path against.
    pub fn keyword(&mut self, key: &str, value: &str) -> Result<(), String> {
        parse::apply(self, key.trim(), value.trim(), None)
    }
}

/// A line that did not apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The file, as the caller or `source` named it.
    pub file: String,
    /// The line, counting from 1.
    pub line: usize,
    /// What went wrong, in Hyprland's words where it has them.
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Config error in file {} at line {}: {}",
            self.file, self.line, self.message
        )
    }
}

/// A parsed configuration and what did not apply.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// The configuration, defaults and every line that applied.
    pub config: Config,
    /// Every line that did not, in the order they were read.
    pub diagnostics: Vec<Diagnostic>,
}

/// A file `source` read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Its name, for diagnostics and for resolving its own `source` lines.
    pub name: String,
    /// Its text.
    pub text: String,
}

/// What `source = …` reads.
pub trait Sources {
    /// The files `spec` names, as written in file `from`, in the order they
    /// are to be read.
    fn resolve(&mut self, spec: &str, from: &str) -> Result<Vec<SourceFile>, String>;
}

/// Sources that refuse every `source` line.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSources;

impl Sources for NoSources {
    fn resolve(&mut self, spec: &str, _from: &str) -> Result<Vec<SourceFile>, String> {
        Err(format!("source file {spec} cannot be read here"))
    }
}

/// Sources read from the file system: `~/` is the home directory given, and
/// a relative path is relative to the directory of the file naming it.
#[derive(Debug, Clone, Default)]
pub struct FsSources {
    /// What `~` stands for, or `None` to refuse paths starting with it.
    pub home: Option<PathBuf>,
}

impl Sources for FsSources {
    fn resolve(&mut self, spec: &str, from: &str) -> Result<Vec<SourceFile>, String> {
        if spec.contains(['*', '?', '[']) {
            return Err(format!("source file {spec}: globs are not supported yet"));
        }
        let path = if let Some(rest) = spec.strip_prefix("~/") {
            let home = self
                .home
                .as_ref()
                .ok_or_else(|| format!("source file {spec}: no home directory"))?;
            home.join(rest)
        } else {
            let path = Path::new(spec);
            match Path::new(from).parent() {
                Some(directory) if path.is_relative() => directory.join(path),
                _ => path.to_path_buf(),
            }
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("source file {} not found: {error}", path.display()))?;
        Ok(vec![SourceFile {
            name: path.display().to_string(),
            text,
        }])
    }
}
