//! `.desktop` files against fuzzel's `xdg.c`.

use super::{Locale, Search, data_dirs, find_programs, lower, parse_desktop_file, with_terminal};
use crate::testdir::TestDir;

fn search() -> Search {
    Search {
        terminal: Some("foot".to_owned()),
        locale: Locale::parse("de_DE.UTF-8"),
        ..Search::default()
    }
}

fn one(text: &str, search: &Search) -> Vec<super::Application> {
    parse_desktop_file(text, "x.desktop", &lower("x"), "/apps/x.desktop", search)
}

const FIREFOX: &str = "\
[Desktop Entry]
Version=1.0
Name=Firefox Web Browser
Name[de]=Firefox-Webbrowser
Name[fr]=Navigateur Firefox
GenericName=Web Browser
Comment=Browse the World Wide Web
Keywords=Internet;WWW;Browser;Web;Explorer
Exec=firefox %u
Icon=firefox
Terminal=false
Type=Application
Categories=GNOME;GTK;Network;WebBrowser;
Actions=new-window;new-private-window;

[Desktop Action new-window]
Name=Open a New Window
Exec=firefox -new-window

[Desktop Action new-private-window]
Name=Open a New Private Window
Exec=firefox -private-window
";

#[test]
fn an_application_and_its_localised_name() {
    let apps = one(FIREFOX, &search());
    assert_eq!(apps.len(), 1);
    let app = apps.first().cloned().unwrap_or_default();
    // `Name[de]` scores 2 for de_DE, above the plain key's 1.
    assert_eq!(app.title_string(), "Firefox-Webbrowser");
    assert_eq!(app.title_lower, lower("firefox-webbrowser"));
    assert_eq!(app.exec.as_deref(), Some("firefox %u"));
    assert_eq!(app.generic_name, Some(lower("web browser")));
    assert_eq!(app.keywords.len(), 5);
    assert_eq!(app.categories.first(), Some(&lower("gnome")));
    assert_eq!(app.icon_name.as_deref(), Some("firefox"));
    assert!(app.visible);
    // Anything but `false` turns it off, including no key at all being...
    // absent: absent is on.
    assert!(app.startup_notify);
    let english = one(FIREFOX, &Search::default());
    assert_eq!(
        english
            .first()
            .map(super::Application::title_string)
            .as_deref(),
        Some("Firefox Web Browser")
    );
}

#[test]
fn actions_are_listed_only_when_asked() {
    let with = Search {
        include_actions: true,
        ..search()
    };
    let apps = one(FIREFOX, &with);
    let titles: Vec<String> = apps.iter().map(super::Application::title_string).collect();
    assert_eq!(
        titles,
        vec![
            "Firefox-Webbrowser",
            "Firefox-Webbrowser — Open a New Window",
            "Firefox-Webbrowser — Open a New Private Window",
        ]
    );
    let action = apps.get(1).cloned().unwrap_or_default();
    assert_eq!(action.exec.as_deref(), Some("firefox -new-window"));
    assert_eq!(action.action_id.as_deref(), Some("new-window"));
    // Inherited from the entry.
    assert_eq!(action.icon_name.as_deref(), Some("firefox"));
    assert_eq!(action.keywords.len(), 5);
}

#[test]
fn an_undeclared_action_or_unknown_group_ends_the_file() {
    let text = "[Desktop Entry]\nName=A\n[Desktop Action nope]\nName=B\n[Other]\nName=C\n";
    let with = Search {
        include_actions: true,
        ..Search::default()
    };
    let apps = one(text, &with);
    assert_eq!(apps.len(), 1);
    let text = "[Desktop Entry]\nName=A\n[X-Extra]\nName=C\nNoDisplay=true\n";
    let apps = one(text, &Search::default());
    assert_eq!(
        apps.first().map(|a| (a.title_string(), a.visible)),
        Some(("A".to_owned(), true))
    );
}

#[test]
fn not_a_desktop_entry_is_nothing() {
    assert!(one("Name=A\nExec=a\n", &Search::default()).is_empty());
    // A header that is any prefix of "desktop entry", in any case, counts.
    assert_eq!(one("[DESKTOP]\nName=A\n", &Search::default()).len(), 1);
    assert!(one("[Desktop Entry Extra]\nName=A\n", &Search::default()).is_empty());
}

#[test]
fn hidden_nodisplay_and_tryexec() {
    let s = Search {
        path: Some("/nonexistent".to_owned()),
        ..Search::default()
    };
    for text in [
        "[Desktop Entry]\nName=A\nHidden=true\n",
        "[Desktop Entry]\nName=A\nNoDisplay=true\n",
        "[Desktop Entry]\nName=A\nTryExec=surely-not-a-program\n",
        "[Desktop Entry]\nName=A\nTryExec=/nonexistent/bin\n",
    ] {
        assert_eq!(
            one(text, &s).first().map(|a| a.visible),
            Some(false),
            "{text}"
        );
    }
    // `Hidden=True` is not `true`.
    let shown = one("[Desktop Entry]\nName=A\nHidden=True\n", &s);
    assert_eq!(shown.first().map(|a| a.visible), Some(true));
    let sh = one("[Desktop Entry]\nName=A\nTryExec=/bin/sh\n", &s);
    assert_eq!(
        sh.first().map(|a| a.visible),
        Some(std::path::Path::new("/bin/sh").exists())
    );
}

#[test]
fn only_show_in_and_not_show_in() {
    let s = |filter: bool| Search {
        filter_desktop: filter,
        desktops: vec!["Hyprland".to_owned()],
        ..Search::default()
    };
    let visible = |text: &str, filter: bool| one(text, &s(filter)).first().map(|a| a.visible);
    let gnome_only = "[Desktop Entry]\nName=A\nOnlyShowIn=GNOME;\n";
    assert_eq!(visible(gnome_only, true), Some(false));
    assert_eq!(visible(gnome_only, false), Some(true));
    assert_eq!(
        visible(
            "[Desktop Entry]\nName=A\nOnlyShowIn=GNOME;Hyprland;\n",
            true
        ),
        Some(true)
    );
    assert_eq!(
        visible("[Desktop Entry]\nName=A\nNotShowIn=Hyprland;\n", true),
        Some(false)
    );
    assert_eq!(
        visible("[Desktop Entry]\nName=A\nNotShowIn=KDE;\n", true),
        Some(true)
    );
}

#[test]
fn terminal_programs() {
    let text = "[Desktop Entry]\nName=Htop\nExec=htop\nTerminal=true\n";
    let apps = one(text, &search());
    assert_eq!(
        apps.first().and_then(|a| a.exec.clone()).as_deref(),
        Some("foot htop")
    );
    // The field used for matching is the exec line as written.
    assert_eq!(apps.first().map(|a| a.wexec.clone()), Some(lower("htop")));
    let placeholder = Search {
        terminal: Some("foot -a '{cmd}' {cmd}".to_owned()),
        ..Search::default()
    };
    let apps = one(text, &placeholder);
    assert_eq!(
        apps.first().and_then(|a| a.exec.clone()).as_deref(),
        Some("foot -a 'htop' htop")
    );
    // With no terminal, the command is run as it is.
    let none = one(text, &Search::default());
    assert_eq!(
        none.first().and_then(|a| a.exec.clone()).as_deref(),
        Some("htop")
    );
    assert_eq!(with_terminal("xterm -e", "vi"), "xterm -e vi");
}

#[test]
fn lines_fuzzel_reads_loosely() {
    let text = "  [Desktop Entry]  \n  Name  =  Spaced  \nComment[de]=Kommentar\nComment=Plain\n\
                Exec=\nIcon==odd\n=Name=Leading\n";
    let apps = one(text, &search());
    let app = apps.first().cloned().unwrap_or_default();
    // `strtok` skips the leading `=`, so the last line is `Name=Leading`,
    // at the same score as the first, which it does not beat.
    assert_eq!(app.title_string(), "Spaced");
    assert_eq!(app.comment, Some(lower("kommentar")));
    assert_eq!(app.exec, None);
    assert_eq!(app.icon_name.as_deref(), Some("=odd"));
    let untitled = one("[Desktop Entry]\nExec=a\n", &Search::default());
    assert_eq!(
        untitled
            .first()
            .map(super::Application::title_string)
            .as_deref(),
        Some("<no title>")
    );
}

#[test]
fn locale_forms() {
    let l = Locale::parse("sr_RS.UTF-8@latin");
    assert_eq!(l.lang.as_deref(), Some("sr"));
    assert_eq!(l.lang_country.as_deref(), Some("sr_RS"));
    assert_eq!(l.lang_modifier.as_deref(), Some("sr@latin"));
    assert_eq!(l.lang_country_modifier.as_deref(), Some("sr_RS@latin"));
    let text = "[Desktop Entry]\nName=A\nName[sr]=B\nName[sr@latin]=C\nName[sr_RS]=D\nName[sr_RS@latin]=E\nName[de]=F\n";
    let s = Search {
        locale: l,
        ..Search::default()
    };
    assert_eq!(
        one(text, &s)
            .first()
            .map(super::Application::title_string)
            .as_deref(),
        Some("E")
    );
    let c = Locale::parse("C");
    assert_eq!((c.lang.as_deref(), c.lang_country), (Some("C"), None));
}

#[test]
fn the_directories_and_ids() {
    let dir = TestDir::new("apps");
    let home = dir.path().join("home");
    let _ = dir.write(
        "home/.local/share/applications/zinc.desktop",
        "[Desktop Entry]\nName=Zinc (home)\nExec=zinc\n",
    );
    let _ = dir.write(
        "usr/share/applications/zinc.desktop",
        "[Desktop Entry]\nName=Zinc (system)\nExec=zinc\n",
    );
    let _ = dir.write(
        "usr/share/applications/kde/konsole.desktop",
        "[Desktop Entry]\nName=Konsole\nExec=konsole\n",
    );
    let _ = dir.write(
        "usr/share/applications/alpha.desktop",
        "[Desktop Entry]\nName=alpha\nExec=a\n",
    );
    let _ = dir.write(
        "usr/share/applications/notes.txt",
        "[Desktop Entry]\nName=Not me\n",
    );
    let system = dir.path().join("usr/share");
    let dirs = data_dirs(
        None,
        Some(&home.display().to_string()),
        Some(&format!("{}:/nonexistent-dir", system.display())),
    );
    assert_eq!(dirs, vec![home.join(".local/share"), system.clone()]);
    let apps = find_programs(&Search {
        data_dirs: dirs,
        ..Search::default()
    });
    let listed: Vec<(String, String)> = apps
        .iter()
        .map(|a| (a.id.clone().unwrap_or_default(), a.title_string()))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("alpha.desktop".to_owned(), "alpha".to_owned()),
            ("kde-konsole.desktop".to_owned(), "Konsole".to_owned()),
            ("zinc.desktop".to_owned(), "Zinc (home)".to_owned()),
        ]
    );
    assert_eq!(
        apps.get(1).map(|a| a.basename.clone()),
        Some(lower("konsole"))
    );
    // With no XDG_DATA_DIRS, fuzzel's two defaults.
    let defaults = data_dirs(Some(""), None, None);
    assert!(
        defaults
            .iter()
            .all(|d| d == std::path::Path::new("/usr/local/share")
                || d == std::path::Path::new("/usr/share"))
    );
}

#[test]
fn executables_on_the_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("path");
    let a = dir.write("a/run-me", "#!/bin/sh\n");
    let b = dir.write("b/run-me", "#!/bin/sh\n");
    let _ = dir.write("a/not-me", "data");
    for path in [&a, &b] {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    let path = format!(
        "{}:{}:/nonexistent-path-dir",
        dir.path().join("a").display(),
        dir.path().join("b").display()
    );
    let (programs, warnings) = super::path_programs(&path);
    assert_eq!(programs.len(), 1);
    assert_eq!(
        programs.first().and_then(|p| p.exec.clone()),
        Some(format!("{}/run-me", dir.path().join("a").display()))
    );
    assert_eq!(warnings.len(), 1);
}

/// "All applications" for Settings: the system's own entries as the tree
/// ships them, what a program wrote under the user's own directory, and
/// nothing hidden.
#[test]
fn the_listing_is_the_systems_entries_and_the_users_own() {
    let dir = TestDir::new("listing");
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("applications");
    for name in ["terminal", "pattern", "top"] {
        let text =
            std::fs::read_to_string(shipped.join(format!("{name}.desktop"))).unwrap_or_default();
        assert!(
            text.starts_with("[Desktop Entry]"),
            "{name}.desktop is shipped"
        );
        let _ = dir.write(&format!("usr/share/applications/{name}.desktop"), &text);
    }
    let _ = dir.write(
        "usr/share/applications/hidden.desktop",
        "[Desktop Entry]\nType=Application\nName=Hidden\nExec=/bin/true\nNoDisplay=true\n",
    );
    // As Steam writes one for itself, and Wine one for each program, in a
    // subdirectory whose name becomes part of the ID.
    let _ = dir.write(
        "home/.local/share/applications/steam.desktop",
        "[Desktop Entry]\nType=Application\nName=Steam\nExec=/data/steam/run.sh\nIcon=steam\n",
    );
    let _ = dir.write(
        "home/.local/share/applications/wine/Programs/notepad.desktop",
        "[Desktop Entry]\nType=Application\nName=Notepad\nExec=wine notepad.exe\n",
    );
    let home = dir.path().join("home").to_string_lossy().into_owned();
    let system = dir.path().join("usr/share").to_string_lossy().into_owned();
    let var = move |name: &str| match name {
        "HOME" => Some(home.clone()),
        "XDG_DATA_DIRS" => Some(system.clone()),
        "PATH" => Some("/bin:/usr/bin".to_owned()),
        _ => None,
    };
    let search = Search::listing(&var);
    assert!(!search.include_actions && !search.filter_desktop);
    assert_eq!(search.terminal, None);
    let listed: Vec<(String, String)> = find_programs(&search)
        .into_iter()
        .filter(|app| app.visible)
        .map(|app| (app.title_string(), app.id.unwrap_or_default()))
        .collect();
    let listed: Vec<(&str, &str)> = listed
        .iter()
        .map(|(title, id)| (title.as_str(), id.as_str()))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("Notepad", "wine-Programs-notepad.desktop"),
            ("Steam", "steam.desktop"),
            ("Terminal", "terminal.desktop"),
            ("Test Pattern", "pattern.desktop"),
            ("Top", "top.desktop"),
        ]
    );
}
