//! Which file an entry's `Icon` is: fuzzel's `icon.c`. The fuzzel app's
//! `icon` module until 2026-10-11, here with [`crate::entry`].
//!
//! The freedesktop icon theme specification, as fuzzel implements it: the
//! theme `icon-theme` names (`default`, which on most systems inherits
//! Adwaita), everything it inherits, then `hicolor`; in each, the
//! directories whose size suits the row, an exact size first and else the
//! nearest; then the bare `pixmaps` directories. Outside dmenu mode only a
//! theme's `Applications`, `Apps` and `Legacy` directories are searched, and
//! directories for a scale above one are skipped, both as fuzzel does.
//!
//! This finds files; it does not read them. Drawing them is the window's.

use std::path::{Path, PathBuf};

/// What an icon file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A bitmap, scaled to the row.
    Png,
    /// A drawing, rasterised at the row's size -- and the only kind fuzzel
    /// draws large for `image-size-ratio`.
    Svg,
}

/// An icon file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Where.
    pub path: PathBuf,
    /// What it is.
    pub kind: Kind,
}

/// How a theme directory's icons are sized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirType {
    /// Exactly `size`.
    Fixed,
    /// Anything from `min` to `max`.
    Scalable,
    /// Within `threshold` of `size`.
    Threshold,
}

/// One directory of a theme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconDir {
    /// Relative to the theme's directory.
    pub path: String,
    /// `Size`.
    pub size: i32,
    /// `MinSize`, or `size`.
    pub min_size: i32,
    /// `MaxSize`, or `size`.
    pub max_size: i32,
    /// `Scale`.
    pub scale: i32,
    /// `Threshold`.
    pub threshold: i32,
    /// `Type`.
    pub kind: DirType,
}

/// A theme as one `index.theme` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Its directory name.
    pub name: String,
    /// Its directories, as listed.
    pub dirs: Vec<IconDir>,
}

/// Whether a directory's `Context` is one fuzzel searches for applications.
fn allowed(context: Option<&str>) -> bool {
    context.is_some_and(|context| {
        ["applications", "apps", "legacy"]
            .iter()
            .any(|allowed| context.eq_ignore_ascii_case(allowed))
    })
}

/// `sscanf("%d")`: leading space, a sign, digits; anything else leaves the
/// value as it was.
fn scan_int(value: &str) -> Option<i32> {
    let value = value.trim_start();
    let digits_end = value
        .char_indices()
        .find(|(at, c)| !(c.is_ascii_digit() || (*at == 0 && (*c == '-' || *c == '+'))))
        .map_or(value.len(), |(at, _)| at);
    value.get(..digits_end)?.parse().ok()
}

/// The settings a section has collected, applied to the directories named
/// like it.
#[derive(Clone, Debug)]
struct Pending {
    section: Option<String>,
    size: i32,
    min_size: i32,
    max_size: i32,
    scale: i32,
    threshold: i32,
    context: Option<String>,
    kind: DirType,
}

impl Pending {
    fn new(section: Option<String>) -> Self {
        Self {
            section,
            size: -1,
            min_size: -1,
            max_size: -1,
            scale: 1,
            threshold: 2,
            context: None,
            kind: DirType::Threshold,
        }
    }

    fn apply(&self, dirs: &mut [IconDir], filter_context: bool) {
        if filter_context && !allowed(self.context.as_deref()) {
            return;
        }
        for dir in dirs.iter_mut() {
            if self.section.as_deref() != Some(dir.path.as_str()) {
                continue;
            }
            dir.size = self.size;
            dir.min_size = if self.min_size >= 0 {
                self.min_size
            } else {
                self.size
            };
            dir.max_size = if self.max_size >= 0 {
                self.max_size
            } else {
                self.size
            };
            dir.scale = self.scale;
            dir.threshold = self.threshold;
            dir.kind = self.kind;
        }
    }
}

/// fuzzel's `parse_theme`: a theme's directories, and the themes it
/// inherits.
#[must_use]
pub fn parse_theme(text: &str, filter_context: bool) -> (Vec<IconDir>, Vec<String>) {
    let mut dirs: Vec<IconDir> = Vec::new();
    let mut inherits = Vec::new();
    let mut pending = Pending::new(None);
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            pending.apply(&mut dirs, filter_context);
            pending = Pending::new(Some(section.to_owned()));
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let key_end = trimmed
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(trimmed.len());
        if key_end == 0 {
            continue;
        }
        let (key, rest) = trimmed.split_at(key_end);
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        if key.eq_ignore_ascii_case("inherits") {
            inherits.extend(
                value
                    .split(',')
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned),
            );
        }
        if key.eq_ignore_ascii_case("directories") {
            dirs.extend(value.split(',').filter(|d| !d.is_empty()).map(|d| IconDir {
                path: d.to_owned(),
                size: 0,
                min_size: 0,
                max_size: 0,
                scale: 0,
                threshold: 0,
                kind: DirType::Threshold,
            }));
        } else if key.eq_ignore_ascii_case("size") {
            pending.size = scan_int(value).unwrap_or(pending.size);
        } else if key.eq_ignore_ascii_case("minsize") {
            pending.min_size = scan_int(value).unwrap_or(pending.min_size);
        } else if key.eq_ignore_ascii_case("maxsize") {
            pending.max_size = scan_int(value).unwrap_or(pending.max_size);
        } else if key.eq_ignore_ascii_case("scale") {
            pending.scale = scan_int(value).unwrap_or(pending.scale);
        } else if key.eq_ignore_ascii_case("context") {
            pending.context = Some(value.to_owned());
        } else if key.eq_ignore_ascii_case("threshold") {
            pending.threshold = scan_int(value).unwrap_or(pending.threshold);
        } else if key.eq_ignore_ascii_case("type") {
            if value.eq_ignore_ascii_case("fixed") {
                pending.kind = DirType::Fixed;
            } else if value.eq_ignore_ascii_case("scalable") {
                pending.kind = DirType::Scalable;
            } else if value.eq_ignore_ascii_case("threshold") {
                pending.kind = DirType::Threshold;
            }
        }
    }
    pending.apply(&mut dirs, filter_context);
    // A directory no section described (or whose context was filtered out)
    // keeps size zero and is dropped.
    dirs.retain(|dir| dir.size != 0);
    (dirs, inherits)
}

/// fuzzel's `get_icon_dirs`: `~/.icons`, each data directory's `icons`,
/// then the two `pixmaps` directories.
#[must_use]
pub fn icon_dirs(data_dirs: &[PathBuf], home: Option<&str>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home {
        let path = PathBuf::from(format!("{home}/.icons"));
        if path.is_dir() {
            out.push(path);
        }
    }
    out.extend(
        data_dirs
            .iter()
            .map(|dir| dir.join("icons"))
            .filter(|dir| dir.is_dir()),
    );
    out.extend(
        ["/usr/local/share/pixmaps", "/usr/share/pixmaps"]
            .into_iter()
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir()),
    );
    out
}

/// fuzzel's `icon_load_theme`: `name`, what it inherits breadth first, and
/// `hicolor` last -- one entry for every directory the theme is found in.
#[must_use]
pub fn load_themes(name: &str, dirs: &[PathBuf], filter_context: bool) -> Vec<Theme> {
    let mut themes: Vec<Theme> = Vec::new();
    let mut queue = std::collections::VecDeque::from([name.to_owned()]);
    let loaded =
        |themes: &[Theme], name: &str| themes.iter().any(|t| t.name.eq_ignore_ascii_case(name));
    let discover =
        |themes: &mut Vec<Theme>, name: &str, queue: &mut std::collections::VecDeque<String>| {
            for dir in dirs {
                let index = dir.join(name).join("index.theme");
                let Ok(text) = std::fs::read_to_string(&index) else {
                    continue;
                };
                let (icon_dirs, inherits) = parse_theme(&text, filter_context);
                queue.extend(inherits);
                themes.push(Theme {
                    name: name.to_owned(),
                    dirs: icon_dirs,
                });
            }
        };
    while let Some(next) = queue.pop_front() {
        if loaded(&themes, &next) {
            continue;
        }
        discover(&mut themes, &next, &mut queue);
    }
    if !loaded(&themes, "hicolor") {
        let mut ignored = std::collections::VecDeque::new();
        discover(&mut themes, "hicolor", &mut ignored);
    }
    themes
}

/// `name.png` or else `name.svg` in `dir`, as fuzzel's `icon_file_exists`
/// looks.
fn file_in(dir: &Path, name: &str) -> Option<Found> {
    let png = dir.join(format!("{name}.png"));
    if png.is_file() {
        return Some(Found {
            path: png,
            kind: Kind::Png,
        });
    }
    let svg = dir.join(format!("{name}.svg"));
    svg.is_file().then_some(Found {
        path: svg,
        kind: Kind::Svg,
    })
}

/// How far a directory's size is from `size`, and whether it is exact.
fn distance(dir: &IconDir, size: i32) -> (bool, i32) {
    match dir.kind {
        DirType::Fixed => (dir.size == size, (dir.size - size).abs()),
        DirType::Threshold => {
            let exact = dir.size - dir.threshold <= size && dir.size + dir.threshold >= size;
            let diff = if size < dir.size - dir.threshold {
                dir.min_size - size
            } else if size > dir.size + dir.threshold {
                size - dir.max_size
            } else {
                0
            };
            (exact, diff)
        }
        DirType::Scalable => {
            let exact = dir.min_size <= size && dir.max_size >= size;
            let diff = if size < dir.min_size {
                dir.min_size - size
            } else if size > dir.max_size {
                size - dir.max_size
            } else {
                0
            };
            (exact, diff)
        }
    }
}

/// An icon still being looked for.
#[derive(Debug)]
struct Want<'a> {
    /// Which of the names it is.
    at: usize,
    name: &'a str,
    /// The nearest non-exact size found in the current theme, and its
    /// distance.
    best: Option<(i32, Found)>,
}

/// Put `file` in `found[at]`.
fn store(found: &mut [Option<Found>], at: usize, file: Found) {
    if let Some(slot) = found.get_mut(at) {
        *slot = Some(file);
    }
}

/// Look in one theme directory for every icon still wanted: an exact size
/// is the answer; otherwise a nearer size than the best so far is kept.
fn visit(
    dir: &Path,
    exact: bool,
    diff: i32,
    wants: &mut Vec<Want<'_>>,
    found: &mut [Option<Found>],
) {
    let mut resolved = Vec::new();
    for (index, want) in wants.iter_mut().enumerate() {
        if !exact && want.best.as_ref().is_some_and(|(d, _)| *d <= diff) {
            continue;
        }
        let Some(file) = file_in(dir, want.name) else {
            continue;
        };
        if exact {
            store(found, want.at, file);
            resolved.push(index);
        } else {
            want.best = Some((diff, file));
        }
    }
    for index in resolved.into_iter().rev() {
        let _ = wants.remove(index);
    }
}

/// fuzzel's `lookup_icons`: the file for each of `names` at `size` pixels.
#[must_use]
pub fn lookup(
    themes: &[Theme],
    dirs: &[PathBuf],
    size: i32,
    names: &[Option<&str>],
) -> Vec<Option<Found>> {
    let mut found: Vec<Option<Found>> = vec![None; names.len()];
    let mut wants: Vec<Want<'_>> = Vec::new();
    for (at, name) in names.iter().enumerate() {
        let Some(name) = name else { continue };
        if !name.starts_with('/') {
            wants.push(Want {
                at,
                name,
                best: None,
            });
            continue;
        }
        // An absolute path is used as it is, if it says it is an SVG or a
        // PNG; anything else is no icon.
        let kind = if name.ends_with("svg") {
            Some(Kind::Svg)
        } else if name.ends_with("png") {
            Some(Kind::Png)
        } else {
            None
        };
        if let Some(kind) = kind {
            store(
                &mut found,
                at,
                Found {
                    path: PathBuf::from(name),
                    kind,
                },
            );
        }
    }

    for theme in themes {
        for icon_dir in theme.dirs.iter().filter(|d| d.scale <= 1) {
            let (exact, diff) = distance(icon_dir, size);
            for base in dirs {
                let dir = base.join(&theme.name).join(&icon_dir.path);
                if dir.is_dir() {
                    visit(&dir, exact, diff, &mut wants, &mut found);
                }
            }
        }
        // The nearest size this theme had, for the icons it had no exact
        // size of; the rest go on to the next theme with a clean slate.
        wants.retain_mut(|want| match want.best.take() {
            Some((_, file)) => {
                store(&mut found, want.at, file);
                false
            }
            None => true,
        });
    }

    for want in wants {
        if let Some(file) = dirs.iter().find_map(|dir| file_in(dir, want.name)) {
            store(&mut found, want.at, file);
        }
    }
    found
}

#[cfg(test)]
mod tests;
