//! The applications: `.desktop` files, found and read as fuzzel's `xdg.c`
//! finds and reads them. The fuzzel app's `desktop` module until 2026-10-11,
//! here since the Settings app lists the same applications.
//!
//! The Desktop Entry specification says what the keys mean; fuzzel decides
//! which of them it reads and how forgiving it is, and it is fuzzel's
//! behaviour that a person's launcher has, so it is fuzzel's that is here:
//! which directories, in which order, which file wins when two have the same
//! desktop-file ID, how a localised key is chosen, what hides an entry, and
//! what an entry's title is when actions are shown.
//!
//! Two places where fuzzel's code does something its own comments do not
//! mean are *not* copied. With `show-actions`, it lowercases the exec line,
//! generic name and comment of the *last* action in a file only, and uses
//! that action's lengths for all of them -- so an action entry's generic name
//! is matched case-sensitively and against the wrong length. Here every
//! entry's fields are lowercased and measured as their own. Without
//! `show-actions` (the user's file) the two agree exactly.

use std::path::{Path, PathBuf};

/// A string as fuzzel matches it: one element a code point, so positions in
/// it are the positions fuzzel's `char32_t` arrays have.
pub type Text = Vec<char>;

/// `towlower`, one code point to one: a character whose lowercase is more
/// than one code point keeps its own, so positions do not move.
#[must_use]
pub fn lower_char(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(one), None) => one,
        _ => c,
    }
}

/// `s` lowercased, one code point to one.
#[must_use]
pub fn lower(s: &str) -> Text {
    s.chars().map(lower_char).collect()
}

/// One entry of the list: an application, one of its actions, or a line of
/// dmenu input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Application {
    /// The desktop-file ID, such as `org.gnome.Nautilus.desktop`.
    pub id: Option<String>,
    /// `Path`: where it is started.
    pub path: Option<String>,
    /// `Exec`, with the terminal put in front for `Terminal=true`.
    pub exec: Option<String>,
    /// `StartupWMClass`.
    pub app_id: Option<String>,
    /// What is shown: `Name`, or `Name — action` for an action.
    pub title: Text,
    /// `title`, lowercased.
    pub title_lower: Text,
    /// The file's name without `.desktop`, lowercased.
    pub basename: Text,
    /// `Exec` as written, lowercased.
    pub wexec: Text,
    /// `GenericName`, lowercased.
    pub generic_name: Option<Text>,
    /// `Comment`, lowercased.
    pub comment: Option<Text>,
    /// `Keywords`, lowercased.
    pub keywords: Vec<Text>,
    /// `Categories`, lowercased.
    pub categories: Vec<Text>,
    /// The whole dmenu line, which is what is printed.
    pub dmenu_input: Option<String>,
    /// What `--match-nth` matches against, lowercased.
    pub dmenu_match_nth: Option<Text>,
    /// Its position in the dmenu input.
    pub index: usize,
    /// `Icon`, or the dmenu line's `\0icon\x1f` name.
    pub icon_name: Option<String>,
    /// Whether it is listed: not `Hidden`, not `NoDisplay`, its `TryExec`
    /// found, and shown in this desktop.
    pub visible: bool,
    /// `StartupNotify`, as fuzzel reads it.
    pub startup_notify: bool,
    /// How many times it has been started, from the cache.
    pub count: u32,
    /// The `.desktop` file's full path.
    pub desktop_file_path: Option<String>,
    /// The action's identifier, for an action.
    pub action_id: Option<String>,
    /// The entry's `Name` (for an action, the application's).
    pub original_name: Option<String>,
    /// This entry's own `Name`.
    pub localized_name: Option<String>,
    /// The action's `Name`, for an action.
    pub action_name: Option<String>,
    /// The application's `GenericName`, as written.
    pub original_generic_name: Option<String>,
    /// This entry's `GenericName`, as written.
    pub localized_generic_name: Option<String>,
    /// `Comment` as written, which `launch-prefix` passes on.
    pub comment_text: Option<String>,
}

impl Application {
    /// The title as a string.
    #[must_use]
    pub fn title_string(&self) -> String {
        self.title.iter().collect()
    }
}

/// `LC_MESSAGES` split the four ways a localised key is matched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Locale {
    /// `lang_COUNTRY@MODIFIER`.
    pub lang_country_modifier: Option<String>,
    /// `lang_COUNTRY`.
    pub lang_country: Option<String>,
    /// `lang@MODIFIER`.
    pub lang_modifier: Option<String>,
    /// `lang`.
    pub lang: Option<String>,
}

impl Locale {
    /// Split a locale name such as `de_DE.UTF-8@euro`, as fuzzel's
    /// `scan_dir` splits what `setlocale(LC_MESSAGES, NULL)` returns.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        // fuzzel writes a NUL over the first `_`, the first `.` and the
        // first `@`: each part runs to the next NUL written, or the end.
        let cuts: Vec<usize> = ['_', '.', '@']
            .iter()
            .filter_map(|c| name.find(*c))
            .collect();
        let end_of = |start: usize| {
            cuts.iter()
                .copied()
                .filter(|at| *at > start)
                .min()
                .unwrap_or(name.len())
        };
        let lang_end = cuts.iter().copied().min().unwrap_or(name.len());
        let lang = name.get(..lang_end).unwrap_or(name).to_owned();
        let part = |c: char| {
            name.find(c)
                .and_then(|at| name.get(at + 1..end_of(at)).map(str::to_owned))
        };
        let country = part('_');
        let modifier = part('@');
        let lang_country = country.as_ref().map(|country| format!("{lang}_{country}"));
        let lang_modifier = modifier
            .as_ref()
            .map(|modifier| format!("{lang}@{modifier}"));
        let lang_country_modifier = match (&lang_country, &modifier) {
            (Some(lc), Some(modifier)) => Some(format!("{lc}@{modifier}")),
            _ => None,
        };
        Self {
            lang_country_modifier,
            lang_country,
            lang_modifier,
            lang: Some(lang),
        }
    }

    /// How well a key's `[locale]` suits: 5 to 2 for the four forms, and -1
    /// for a locale that is not this one.
    fn score(&self, locale: &str) -> i32 {
        let is = |wanted: &Option<String>| wanted.as_deref() == Some(locale);
        if is(&self.lang_country_modifier) {
            5
        } else if is(&self.lang_country) {
            4
        } else if is(&self.lang_modifier) {
            3
        } else if is(&self.lang) {
            2
        } else {
            -1
        }
    }
}

/// What reading the files needs from the environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Search {
    /// The data directories, in order, from [`data_dirs`].
    pub data_dirs: Vec<PathBuf>,
    /// `terminal`, for `Terminal=true`.
    pub terminal: Option<String>,
    /// `show-actions`.
    pub include_actions: bool,
    /// `filter-desktop`.
    pub filter_desktop: bool,
    /// `XDG_CURRENT_DESKTOP`, split at `:`.
    pub desktops: Vec<String>,
    /// `LC_MESSAGES`.
    pub locale: Locale,
    /// `PATH`, for `TryExec`.
    pub path: Option<String>,
}

impl Search {
    /// The search that lists "all applications": what fuzzel lists with a
    /// `fuzzel.ini` that changes nothing, read from the environment `var`
    /// answers. Each application once, no actions, none filtered by desktop,
    /// with `LC_ALL`, `LC_MESSAGES` or `LANG` choosing the localised names.
    ///
    /// The Settings app lists with this, so that the applications it offers
    /// are the ones a launcher starts: the system's entries, an app's own
    /// (`docs/APPS.md`), and whatever a program wrote under the user's
    /// `~/.local/share/applications`, Steam's and Wine's among them.
    #[must_use]
    pub fn listing(var: &dyn Fn(&str) -> Option<String>) -> Self {
        Self {
            data_dirs: data_dirs(
                var("XDG_DATA_HOME").as_deref(),
                var("HOME").as_deref(),
                var("XDG_DATA_DIRS").as_deref(),
            ),
            locale: Locale::parse(
                &var("LC_ALL")
                    .or_else(|| var("LC_MESSAGES"))
                    .or_else(|| var("LANG"))
                    .unwrap_or_else(|| "C".to_owned()),
            ),
            path: var("PATH"),
            ..Self::default()
        }
    }
}

/// fuzzel's `xdg_data_dirs`: `XDG_DATA_HOME` (or `~/.local/share`), then
/// `XDG_DATA_DIRS` (or `/usr/local/share:/usr/share`), each only if it is a
/// directory.
#[must_use]
pub fn data_dirs(
    xdg_data_home: Option<&str>,
    home: Option<&str>,
    xdg_data_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let is_dir = |path: &Path| path.is_dir();
    match (
        xdg_data_home.filter(|s| !s.is_empty()),
        home.filter(|s| !s.is_empty()),
    ) {
        (Some(data_home), _) => {
            let path = PathBuf::from(data_home);
            if is_dir(&path) {
                out.push(path);
            }
        }
        (None, Some(home)) => {
            let path = PathBuf::from(format!("{home}/.local/share"));
            if is_dir(&path) {
                out.push(path);
            }
        }
        (None, None) => {}
    }
    match xdg_data_dirs {
        Some(dirs) => out.extend(
            dirs.split(':')
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .filter(|path| is_dir(path)),
        ),
        None => out.extend(
            ["/usr/local/share", "/usr/share"]
                .into_iter()
                .map(PathBuf::from)
                .filter(|path| is_dir(path)),
        ),
    }
    out
}

/// Every application in `search.data_dirs`' `applications` directories,
/// sorted by title, as fuzzel's `xdg_find_programs` lists them.
#[must_use]
pub fn find_programs(search: &Search) -> Vec<Application> {
    let mut apps = Vec::new();
    for dir in &search.data_dirs {
        let applications = dir.join("applications");
        if applications.is_dir() {
            scan_dir(&applications, None, search, &mut apps);
        }
    }
    sort_by_title(&mut apps);
    apps
}

/// fuzzel's `path_find_programs`, for `list-executables-in-path`: every
/// executable regular file in each `PATH` directory, named by its file name,
/// the first of a name kept. They are added after the sorted entries,
/// unsorted, as fuzzel adds them. Also answers the directories fuzzel would
/// warn it could not open.
#[must_use]
pub fn path_programs(path: &str) -> (Vec<Application>, Vec<String>) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut out: Vec<Application> = Vec::new();
    let mut warnings = Vec::new();
    for dir in path.split(':').filter(|dir| !dir.is_empty()) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!("failed to open {dir} from PATH: {error}"));
                continue;
            }
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
            .collect();
        names.sort();
        for name in names {
            let full = Path::new(dir).join(&name);
            let Ok(meta) = std::fs::metadata(&full) else {
                continue;
            };
            // `S_IXUSR`: executable by its owner.
            if !meta.is_file() || meta.permissions().mode() & 0o100 == 0 {
                continue;
            }
            let title: Text = name.chars().collect();
            if out.iter().any(|app| app.title == title) {
                continue;
            }
            out.push(Application {
                title_lower: lower(&name),
                title,
                exec: Some(format!("{dir}/{name}")),
                visible: true,
                startup_notify: true,
                ..Application::default()
            });
        }
    }
    (out, warnings)
}

/// `sort_application_by_title`: `c32casecmp` on the titles. Stable, where
/// fuzzel's `qsort` leaves equal titles in whatever order it leaves them.
pub fn sort_by_title(apps: &mut [Application]) {
    apps.sort_by(|a, b| {
        let a = a.title.iter().map(|c| lower_char(*c));
        let b = b.title.iter().map(|c| lower_char(*c));
        a.cmp(b)
    });
}

/// fuzzel's `scan_dir`: every regular `.desktop` file under `dir`, the ID
/// of one in a subdirectory joined with `-`, and the first file with an ID
/// the one that counts.
fn scan_dir(dir: &Path, base_id: Option<&str>, search: &Search, apps: &mut Vec<Application>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<std::ffi::OsString> = entries
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    // readdir's order is the file system's; sorting makes the ID a file wins
    // with the same on every machine, which fuzzel leaves to the disk.
    names.sort();
    for name in names {
        let Some(name) = name.to_str().map(str::to_owned) else {
            continue;
        };
        let full = dir.join(&name);
        let Ok(meta) = std::fs::metadata(&full) else {
            continue;
        };
        let id = match base_id {
            Some(base) => format!("{base}-{name}"),
            None => name.clone(),
        };
        if meta.is_dir() {
            scan_dir(&full, Some(&id), search, apps);
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if !name.ends_with(".desktop") {
            continue;
        }
        // The basename is cut at the *last* dot, which for `a.b.desktop` is
        // the `.desktop` one.
        let basename = name
            .rsplit_once('.')
            .map_or(name.as_str(), |(before, _)| before);
        if apps
            .iter()
            .any(|app| app.id.as_deref() == Some(id.as_str()))
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        apps.extend(parse_desktop_file(
            &text,
            &id,
            &lower(basename),
            &full.display().to_string(),
            search,
        ));
    }
}

/// One `Desktop Action` (or the entry itself) as it is read.
#[derive(Clone, Debug, Default)]
struct Action {
    name: Option<String>,
    generic_name: Option<String>,
    app_id: Option<String>,
    comment: Option<String>,
    keywords: Vec<String>,
    categories: Vec<String>,
    only_show_in: Vec<String>,
    not_show_in: Vec<String>,
    name_score: i32,
    generic_name_score: i32,
    comment_score: i32,
    keywords_score: i32,
    categories_score: i32,
    icon: Option<String>,
    exec: Option<String>,
    path: Option<String>,
    visible: bool,
    use_terminal: bool,
    no_startup_notify: bool,
    action_id: Option<String>,
}

/// `isspace` in the C locale.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// Split at `;`, dropping empty parts, as `strtok` does.
fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split(';').filter(|part| !part.is_empty())
}

/// Whether `bin` is an executable file, by path or on `PATH`.
fn is_executable(bin: &str, path: Option<&str>) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let executable = |candidate: &Path| {
        std::fs::metadata(candidate).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    };
    if bin.starts_with('/') {
        return executable(Path::new(bin));
    }
    path.is_some_and(|path| {
        path.split(':')
            .filter(|dir| !dir.is_empty())
            .any(|dir| executable(&Path::new(dir).join(bin)))
    })
}

/// Whether an entry is shown in `desktops`, as fuzzel's
/// `filter_desktop_entry` decides: the first current desktop found in
/// `OnlyShowIn` shows it and one found in `NotShowIn` hides it; with
/// neither, it is shown unless it has an `OnlyShowIn` at all.
fn shown_in(action: &Action, desktops: &[String]) -> bool {
    for current in desktops {
        if action.only_show_in.iter().any(|d| d == current) {
            return true;
        }
        if action.not_show_in.iter().any(|d| d == current) {
            return false;
        }
    }
    action.only_show_in.is_empty()
}

/// `terminal` in front of `exec`: in place of every `{cmd}`, or before it.
#[must_use]
pub fn with_terminal(terminal: &str, exec: &str) -> String {
    if terminal.contains("{cmd}") {
        terminal.replace("{cmd}", exec)
    } else {
        format!("{terminal} {exec}")
    }
}

/// fuzzel's `parse_desktop_file`: the entries one file gives -- the
/// application, and with `show-actions` each action it declares.
#[must_use]
pub fn parse_desktop_file(
    text: &str,
    id: &str,
    basename_lower: &[char],
    file_path: &str,
    search: &Search,
) -> Vec<Application> {
    let mut is_desktop_entry = false;
    let mut action_names: Vec<String> = Vec::new();
    let mut actions: Vec<Action> = vec![Action {
        visible: true,
        ..Action::default()
    }];

    for raw in text.split('\n') {
        let line = raw.trim_matches(is_space);
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // `strncasecmp(&line[1], "desktop entry", len - 2)`: a header
            // that is any case-insensitive prefix of it counts.
            let lowered = inner.to_ascii_lowercase();
            if lowered.len() <= "desktop entry".len() && "desktop entry".starts_with(&lowered) {
                is_desktop_entry = true;
                continue;
            }
            if search.include_actions
                && line.len() >= 16
                && line
                    .get(1..16)
                    .is_some_and(|word| word.eq_ignore_ascii_case("desktop action "))
            {
                let name = line.get(16..line.len() - 1).unwrap_or_default();
                if !action_names.iter().any(|known| known == name) {
                    break;
                }
                let base = actions.first().cloned().unwrap_or_default();
                actions.push(Action {
                    action_id: Some(name.to_owned()),
                    generic_name: base.generic_name,
                    comment: base.comment,
                    path: base.path,
                    icon: base.icon,
                    visible: base.visible,
                    use_terminal: base.use_terminal,
                    keywords: base.keywords,
                    categories: base.categories,
                    ..Action::default()
                });
                continue;
            }
            // Any other group ends the file, as far as fuzzel reads it.
            break;
        }

        // `strtok_r(line, "=")` then `strtok_r(NULL, "\n")`: leading `=`s
        // are skipped, the key runs to the next `=`, and an empty value is
        // no value.
        let stripped = line.trim_start_matches('=');
        let Some((key, value)) = stripped.split_once('=') else {
            continue;
        };
        if key.is_empty() || value.is_empty() {
            continue;
        }
        // Trailing space off the key (never below one character), leading
        // space off the value.
        let mut key = key;
        while key.chars().count() > 1 && key.ends_with(is_space) {
            let mut chars = key.chars();
            let _ = chars.next_back();
            key = chars.as_str();
        }
        let value = value.trim_start_matches(is_space);
        let mut score = 1;
        let mut key_name = key;
        if key.ends_with(']')
            && let Some((name, rest)) = key.split_once('[')
        {
            let locale = rest.strip_suffix(']').unwrap_or(rest);
            key_name = name;
            score = search.locale.score(locale);
        }
        let Some(action) = actions.last_mut() else {
            break;
        };
        match key_name {
            "Name" => {
                if score > action.name_score {
                    action.name = Some(value.to_owned());
                    action.name_score = score;
                }
            }
            "Exec" => action.exec = Some(value.to_owned()),
            "TryExec" => {
                if !is_executable(value, search.path.as_deref()) {
                    action.visible = false;
                }
            }
            "Path" => action.path = Some(value.to_owned()),
            "GenericName" => {
                if score > action.generic_name_score {
                    action.generic_name = Some(value.to_owned());
                    action.generic_name_score = score;
                }
            }
            // fuzzel's test is `strcmp(value, "false")`, so anything *but*
            // `false` turns startup notification off. That only decides
            // whether an activation token is asked for.
            "StartupNotify" => {
                if value != "false" {
                    action.no_startup_notify = true;
                }
            }
            "StartupWMClass" => action.app_id = Some(value.to_owned()),
            "Comment" => {
                if score > action.comment_score {
                    action.comment = Some(value.to_owned());
                    action.comment_score = score;
                }
            }
            // A better-localised `Keywords` is added to the ones already
            // read rather than replacing them, as fuzzel does.
            "Keywords" => {
                if score > action.keywords_score {
                    action.keywords.extend(list(value).map(str::to_owned));
                    action.keywords_score = score;
                }
            }
            "Categories" => {
                if score > action.categories_score {
                    action.categories.extend(list(value).map(str::to_owned));
                    action.categories_score = score;
                }
            }
            "Actions" => action_names.extend(list(value).map(str::to_owned)),
            "OnlyShowIn" => action.only_show_in.extend(list(value).map(str::to_owned)),
            "NotShowIn" => action.not_show_in.extend(list(value).map(str::to_owned)),
            "Icon" => action.icon = Some(value.to_owned()),
            "Hidden" | "NoDisplay" if value == "true" => action.visible = false,
            "Terminal" if value == "true" => action.use_terminal = true,
            _ => {}
        }
    }

    if !is_desktop_entry {
        return Vec::new();
    }
    let default_name = actions
        .first()
        .and_then(|a| a.name.clone())
        .unwrap_or_else(|| "<no title>".to_owned());
    let default_generic = actions.first().and_then(|a| a.generic_name.clone());
    let mut out = Vec::new();
    for (at, a) in actions.into_iter().enumerate() {
        let name = a.name.clone().unwrap_or_else(|| "<no title>".to_owned());
        let exec = match (&a.exec, &search.terminal) {
            (Some(exec), Some(terminal)) if a.use_terminal => Some(with_terminal(terminal, exec)),
            _ => a.exec.clone(),
        };
        let title = if at == 0 {
            name.clone()
        } else {
            format!("{default_name} — {name}")
        };
        let visible = a.visible && (!search.filter_desktop || shown_in(&a, &search.desktops));
        let is_action = a.action_id.is_some();
        out.push(Application {
            id: Some(id.to_owned()),
            path: a.path.clone(),
            exec,
            app_id: a.app_id.clone(),
            title_lower: lower(&title),
            title: title.chars().collect(),
            basename: basename_lower.to_vec(),
            wexec: a.exec.as_deref().map(lower).unwrap_or_default(),
            generic_name: a.generic_name.as_deref().map(lower),
            comment: a.comment.as_deref().map(lower),
            keywords: a.keywords.iter().map(|k| lower(k)).collect(),
            categories: a.categories.iter().map(|k| lower(k)).collect(),
            dmenu_input: None,
            dmenu_match_nth: None,
            index: 0,
            icon_name: a.icon.clone(),
            visible,
            startup_notify: !a.no_startup_notify,
            count: 0,
            desktop_file_path: Some(file_path.to_owned()),
            action_id: a.action_id.clone(),
            original_name: Some(default_name.clone()),
            localized_name: Some(name.clone()),
            action_name: is_action.then(|| name.clone()),
            original_generic_name: default_generic.clone(),
            localized_generic_name: a.generic_name.clone(),
            comment_text: a.comment.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests;
