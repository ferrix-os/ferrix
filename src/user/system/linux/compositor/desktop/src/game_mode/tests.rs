//! The game-mode file's format, its atomic write, and the mark.

use super::{GameMode, MARK, MARK_APP, is_id, mark_for_launch, marked, path};
use crate::testdir::TestDir;

/// An environment made of pairs.
fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    }
}

#[test]
fn the_file_is_under_the_configuration_directory() {
    assert_eq!(
        path(&env(&[("HOME", "/home/u")])),
        Some("/home/u/.config/ferrix/game-mode.conf".into())
    );
    assert_eq!(
        path(&env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/cfg")])),
        Some("/cfg/ferrix/game-mode.conf".into())
    );
    // An empty XDG_CONFIG_HOME is one that is not set.
    assert_eq!(
        path(&env(&[("HOME", "/"), ("XDG_CONFIG_HOME", "")])),
        Some("/.config/ferrix/game-mode.conf".into())
    );
    assert_eq!(path(&env(&[])), None);
}

#[test]
fn one_id_a_line_with_comments_and_blanks_skipped() {
    let (set, complaints) = GameMode::parse(
        "# games\n\nsteam.desktop\n  pattern.desktop  \r\n#chrome.desktop\nsteam.desktop\n",
    );
    assert_eq!(
        set.ids().collect::<Vec<_>>(),
        vec!["pattern.desktop", "steam.desktop"]
    );
    assert!(complaints.is_empty(), "{complaints:?}");
    assert!(set.contains("steam.desktop"));
    assert!(!set.contains("chrome.desktop"));
    assert_eq!(set.len(), 2);
}

#[test]
fn a_line_that_is_no_id_is_skipped_and_named() {
    let (set, complaints) = GameMode::parse("two words\nok.desktop\nsub/dir.desktop\n");
    assert_eq!(set.ids().collect::<Vec<_>>(), vec!["ok.desktop"]);
    assert_eq!(
        complaints,
        vec![
            "line 1: \"two words\" is not a desktop-file ID, and is skipped",
            "line 3: \"sub/dir.desktop\" is not a desktop-file ID, and is skipped",
        ]
    );
    assert!(is_id("org.gnome.Nautilus.desktop"));
    assert!(!is_id(""));
    assert!(!is_id("#x"));
    assert!(!is_id("a\tb"));
}

#[test]
fn what_is_written_reads_back_and_is_the_same_bytes_for_the_same_set() {
    let mut set = GameMode::default();
    assert!(set.is_empty());
    assert!(set.set("steam.desktop", true));
    assert!(set.set("a.desktop", true));
    // Already on, and not an ID: neither changes the set.
    assert!(!set.set("steam.desktop", true));
    assert!(!set.set("not an id", true));
    let text = set.render();
    assert!(text.starts_with("# Ferrix game mode"), "{text}");
    assert!(text.ends_with("a.desktop\nsteam.desktop\n"), "{text}");
    let (back, complaints) = GameMode::parse(&text);
    assert_eq!(back, set);
    assert!(complaints.is_empty());
    assert_eq!(back.render(), text);
    assert!(set.set("a.desktop", false));
    assert!(!set.set("a.desktop", false));
    assert_eq!(set.ids().collect::<Vec<_>>(), vec!["steam.desktop"]);
}

#[test]
fn no_file_is_nothing_in_game_mode() {
    let dir = TestDir::new("game-mode-missing");
    let (set, complaints) =
        GameMode::load(&dir.path().join("ferrix/game-mode.conf")).unwrap_or_default();
    assert!(set.is_empty());
    assert!(complaints.is_empty());
}

#[test]
fn a_save_replaces_the_file_whole_and_leaves_nothing_beside_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("game-mode-save");
    // The directory is not there yet: the first save makes it.
    let file = dir.path().join("cfg/ferrix/game-mode.conf");
    let mut set = GameMode::default();
    let _ = set.set("steam.desktop", true);
    assert!(set.save(&file).is_ok());
    assert_eq!(
        std::fs::read_to_string(&file).unwrap_or_default(),
        set.render()
    );
    let mode = std::fs::metadata(&file).map(|meta| meta.permissions().mode() & 0o777);
    assert_eq!(mode.ok(), Some(0o644));

    // A second save over a file somebody else wrote by hand.
    let _ = dir.write("cfg/ferrix/game-mode.conf", "hand.desktop\n");
    let _ = set.set("pattern.desktop", true);
    assert!(set.save(&file).is_ok());
    let (back, _) = GameMode::load(&file).unwrap_or_default();
    assert_eq!(back, set);
    let beside: Vec<String> = std::fs::read_dir(dir.path().join("cfg/ferrix"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(beside, vec!["game-mode.conf"], "no temporary file is left");
}

#[test]
fn a_save_that_cannot_be_made_leaves_the_file_as_it_was() {
    let dir = TestDir::new("game-mode-refused");
    // The directory's place is taken by a file, so nothing can be made in it.
    let _ = dir.write("cfg/ferrix", "in the way");
    let file = dir.path().join("cfg/ferrix/game-mode.conf");
    assert!(GameMode::default().save(&file).is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cfg/ferrix")).unwrap_or_default(),
        "in the way"
    );
}

#[test]
fn an_application_in_the_set_is_marked_and_any_other_has_the_mark_taken_out() {
    let (set, _) = GameMode::parse("steam.desktop\n");
    let game = set.mark(Some("steam.desktop"));
    assert!(game.is_game());
    assert_eq!(game.app(), Some("steam.desktop"));
    assert_eq!(
        game.set(),
        vec![
            (MARK, "1".to_owned()),
            (MARK_APP, "steam.desktop".to_owned())
        ]
    );
    assert!(game.unset().is_empty());

    for other in [set.mark(Some("term.desktop")), set.mark(None)] {
        assert!(!other.is_game());
        assert!(other.set().is_empty());
        assert_eq!(other.unset(), &[MARK, MARK_APP]);
    }
}

#[test]
fn the_mark_reaches_a_started_program_and_an_inherited_one_does_not() {
    let (set, _) = GameMode::parse("game.desktop\n");
    // The program prints its environment: `env` is on every host that runs
    // these tests.
    let run = |id: &str| {
        let mut command = std::process::Command::new("env");
        // As if the launcher had itself been started from a game.
        let _ = command.env(MARK, "1").env(MARK_APP, "parent.desktop");
        set.mark(Some(id)).apply(&mut command);
        let output = command
            .output()
            .map(|output| output.stdout)
            .unwrap_or_default();
        String::from_utf8_lossy(&output).into_owned()
    };
    let game = run("game.desktop");
    assert!(
        game.lines().any(|line| line == "FERRIX_GAME_MODE=1"),
        "{game}"
    );
    assert!(
        game.lines()
            .any(|line| line == "FERRIX_GAME_MODE_APP=game.desktop"),
        "{game}"
    );
    let other = run("other.desktop");
    assert!(
        !other.contains("FERRIX_GAME_MODE"),
        "a program that is no game inherited a mark: {other}"
    );
}

#[test]
fn a_launch_reads_the_users_file_afresh() {
    let dir = TestDir::new("game-mode-launch");
    let home = dir.path().to_string_lossy().into_owned();
    let pairs = [("HOME", home.as_str())];
    let var = env(&pairs);
    // No file: nothing is a game, and nothing is complained of.
    let (mark, complaints) = mark_for_launch(Some("steam.desktop"), &var);
    assert!(!mark.is_game());
    assert!(complaints.is_empty());

    let _ = dir.write(
        ".config/ferrix/game-mode.conf",
        "steam.desktop\nnot an id\n",
    );
    let (mark, complaints) = mark_for_launch(Some("steam.desktop"), &var);
    assert!(mark.is_game());
    assert_eq!(complaints.len(), 1);
    assert!(
        complaints
            .first()
            .is_some_and(|line| line.contains("game-mode.conf: line 2")),
        "{complaints:?}"
    );
    assert!(!mark_for_launch(Some("term.desktop"), &var).0.is_game());
    assert!(!mark_for_launch(None, &var).0.is_game());

    // A file that cannot be read starts the program all the same, unmarked.
    let unreadable = TestDir::new("game-mode-unreadable");
    let _ = std::fs::create_dir_all(unreadable.path().join(".config/ferrix/game-mode.conf"));
    let home = unreadable.path().to_string_lossy().into_owned();
    let pairs = [("HOME", home.as_str())];
    let (mark, complaints) = mark_for_launch(Some("steam.desktop"), &env(&pairs));
    assert!(!mark.is_game());
    assert!(
        complaints
            .first()
            .is_some_and(|line| line.ends_with("nothing is started in game mode")),
        "{complaints:?}"
    );
}

#[test]
fn a_program_reads_its_own_mark() {
    assert_eq!(
        marked(&env(&[(MARK, "1"), (MARK_APP, "steam.desktop")])),
        Some("steam.desktop".to_owned())
    );
    assert_eq!(
        marked(&env(&[(MARK, "0"), (MARK_APP, "steam.desktop")])),
        None
    );
    assert_eq!(marked(&env(&[])), None);
}
