//! `hyprctl`'s requests and answers.
//!
//! The JSON is checked by parsing it back with a parser written here, which
//! is the only way to say "this is JSON" without taking a dependency the
//! compositor does not want. The field names and their order are Hyprland
//! 0.56.2's, read from `src/debug/HyprCtl.cpp`, because a bar reads them by
//! name and a missing one is a crash in somebody else's program.

use crate::json::Json;
use crate::reply::{Reply, Version, answer};
use crate::request::{Flags, Format, Request};
use crate::state::{
    Animation, Bezier, Bind, Device, Devices, Keyboard, Layer, Monitor, Opt, Shortcut, Snapshot,
    Style, System, Window, Workspace,
};

/// A small parser, so a test can say "this answer is JSON" and look inside
/// it without the compositor taking a JSON crate.
mod parse {
    /// A parsed value.
    #[derive(Clone, Debug, PartialEq)]
    pub(super) enum Value {
        /// `null`.
        Null,
        /// `true` or `false`.
        Bool(bool),
        /// A number, which JSON has only one kind of.
        Number(f64),
        /// A string, with its escapes undone.
        Text(String),
        /// An array.
        List(Vec<Value>),
        /// An object, in the order its fields were written.
        Map(Vec<(String, Value)>),
    }

    impl Value {
        /// The field `name`, if this is a map with one.
        pub(super) fn get(&self, name: &str) -> Option<&Self> {
            match self {
                Self::Map(fields) => fields
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value),
                _ => None,
            }
        }

        /// The items, if this is a list.
        pub(super) fn items(&self) -> &[Self] {
            match self {
                Self::List(items) => items,
                _ => &[],
            }
        }

        /// The field names, in order, if this is a map.
        pub(super) fn keys(&self) -> Vec<&str> {
            match self {
                Self::Map(fields) => fields.iter().map(|(key, _)| key.as_str()).collect(),
                _ => Vec::new(),
            }
        }

        /// The string, if this is one.
        pub(super) fn text(&self) -> Option<&str> {
            match self {
                Self::Text(value) => Some(value),
                _ => None,
            }
        }

        /// The number, if this is one.
        pub(super) fn number(&self) -> Option<f64> {
            match self {
                Self::Number(value) => Some(*value),
                _ => None,
            }
        }

        /// The boolean, if this is one.
        pub(super) fn boolean(&self) -> Option<bool> {
            match self {
                Self::Bool(value) => Some(*value),
                _ => None,
            }
        }
    }

    /// Parse `text` whole, or say where it stopped making sense.
    pub(super) fn parse(text: &str) -> Result<Value, String> {
        let bytes: Vec<char> = text.chars().collect();
        let mut at = 0;
        let value = value(&bytes, &mut at)?;
        skip(&bytes, &mut at);
        if at != bytes.len() {
            return Err(format!("{} characters left over at {at}", bytes.len() - at));
        }
        Ok(value)
    }

    fn skip(bytes: &[char], at: &mut usize) {
        while bytes.get(*at).is_some_and(|c| c.is_whitespace()) {
            *at += 1;
        }
    }

    fn value(bytes: &[char], at: &mut usize) -> Result<Value, String> {
        skip(bytes, at);
        match bytes.get(*at) {
            Some('{') => map(bytes, at),
            Some('[') => list(bytes, at),
            Some('"') => text(bytes, at).map(Value::Text),
            Some('t') => word(bytes, at, "true").map(|()| Value::Bool(true)),
            Some('f') => word(bytes, at, "false").map(|()| Value::Bool(false)),
            Some('n') => word(bytes, at, "null").map(|()| Value::Null),
            Some(_) => number(bytes, at),
            None => Err("nothing at all".to_owned()),
        }
    }

    fn word(bytes: &[char], at: &mut usize, want: &str) -> Result<(), String> {
        for expected in want.chars() {
            if bytes.get(*at) != Some(&expected) {
                return Err(format!("expected {want} at {at}"));
            }
            *at += 1;
        }
        Ok(())
    }

    fn number(bytes: &[char], at: &mut usize) -> Result<Value, String> {
        let start = *at;
        while bytes
            .get(*at)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
        {
            *at += 1;
        }
        let text: String = bytes.get(start..*at).unwrap_or(&[]).iter().collect();
        text.parse()
            .map(Value::Number)
            .map_err(|_| format!("{text:?} at {start} is not a number"))
    }

    fn text(bytes: &[char], at: &mut usize) -> Result<String, String> {
        if bytes.get(*at) != Some(&'"') {
            return Err(format!("expected a string at {at}"));
        }
        *at += 1;
        let mut out = String::new();
        loop {
            match bytes.get(*at) {
                None => return Err("a string that never ends".to_owned()),
                Some('"') => {
                    *at += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    *at += 1;
                    let escape = *bytes.get(*at).ok_or("an escape that never ends")?;
                    *at += 1;
                    out.push(match escape {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        'u' => {
                            let digits: String = bytes
                                .get(*at..*at + 4)
                                .ok_or("a short unicode escape")?
                                .iter()
                                .collect();
                            *at += 4;
                            let code = u32::from_str_radix(&digits, 16)
                                .map_err(|_| format!("{digits:?} is not hex"))?;
                            char::from_u32(code).ok_or("not a character")?
                        }
                        other => other,
                    });
                }
                Some(other) => {
                    // A raw control character inside a string is exactly what
                    // an unescaped title would put here, and what a bar's own
                    // parser would refuse.
                    if (*other as u32) < 0x20 {
                        return Err(format!("a raw control character {:#04x}", *other as u32));
                    }
                    out.push(*other);
                    *at += 1;
                }
            }
        }
    }

    fn list(bytes: &[char], at: &mut usize) -> Result<Value, String> {
        *at += 1;
        let mut items = Vec::new();
        loop {
            skip(bytes, at);
            if bytes.get(*at) == Some(&']') {
                *at += 1;
                return Ok(Value::List(items));
            }
            if !items.is_empty() {
                if bytes.get(*at) != Some(&',') {
                    return Err(format!("expected a comma at {at}"));
                }
                *at += 1;
            }
            skip(bytes, at);
            if bytes.get(*at) == Some(&']') {
                return Err(format!("a trailing comma at {at}"));
            }
            items.push(value(bytes, at)?);
        }
    }

    fn map(bytes: &[char], at: &mut usize) -> Result<Value, String> {
        *at += 1;
        let mut fields = Vec::new();
        loop {
            skip(bytes, at);
            if bytes.get(*at) == Some(&'}') {
                *at += 1;
                return Ok(Value::Map(fields));
            }
            if !fields.is_empty() {
                if bytes.get(*at) != Some(&',') {
                    return Err(format!("expected a comma at {at}"));
                }
                *at += 1;
            }
            skip(bytes, at);
            if bytes.get(*at) == Some(&'}') {
                return Err(format!("a trailing comma at {at}"));
            }
            let name = text(bytes, at)?;
            skip(bytes, at);
            if bytes.get(*at) != Some(&':') {
                return Err(format!("expected a colon at {at}"));
            }
            *at += 1;
            fields.push((name, value(bytes, at)?));
        }
    }
}

/// A snapshot with two windows on one monitor, as the compositor's own test
/// has.
fn snapshot() -> Snapshot {
    Snapshot {
        plugins: Vec::new(),
        workspace_rules: Vec::new(),
        monitors: vec![Monitor {
            id: 0,
            name: "HEADLESS-1".to_owned(),
            width: 1024,
            height: 768,
            refresh: 60.0,
            at: (0, 0),
            active_workspace: 1,
            active_workspace_name: "1".to_owned(),
            special_workspace: None,
            scale: 1.0,
            transform: compositor_config::Transform::Normal,
            focused: true,
            description: "Headless output 1".to_owned(),
            make: "Ferrix".to_owned(),
            model: "hyprix".to_owned(),
            serial: String::new(),
            reserved: (0, 0, 0, 0),
            dpms: true,
        }],
        workspaces: vec![Workspace {
            id: 1,
            name: "1".to_owned(),
            monitor: "HEADLESS-1".to_owned(),
            windows: 2,
            has_fullscreen: false,
        }],
        windows: vec![
            Window {
                address: 1,
                mapped: true,
                visible: true,
                accepts_input: true,
                at: (10, 10),
                size: (485, 726),
                workspace: 1,
                workspace_name: "1".to_owned(),
                floating: false,
                fullscreen: false,
                fullscreen_mode: 0,
                monitor: 0,
                class: "rocks.magical.pattern".to_owned(),
                title: "one".to_owned(),
                pid: 1234,
                focus_history: 0,
                hidden: false,
                grouped: Vec::new(),
                style: Style::default(),
            },
            Window {
                address: 2,
                mapped: false,
                visible: true,
                accepts_input: true,
                at: (515, 10),
                size: (485, 726),
                workspace: 1,
                workspace_name: "1".to_owned(),
                floating: true,
                fullscreen: false,
                fullscreen_mode: 0,
                monitor: 0,
                class: "rocks.magical.pattern".to_owned(),
                // Everything an unescaped answer would break on, because a
                // title is whatever a program chose to call itself.
                title: "a \"quoted\" \\ title\nwith a newline".to_owned(),
                pid: 5678,
                focus_history: 1,
                hidden: false,
                grouped: Vec::new(),
                style: Style::default(),
            },
        ],
        active_window: Some(1),
        active_workspace: 1,
        submap: String::new(),
        // One of each of the things a bar reads that is not a window, so
        // that every answer this crate writes has something in it.
        binds: vec![Bind {
            release: true,
            modmask: 64,
            submap: "resize".to_owned(),
            submap_universal: true,
            key: "Q".to_owned(),
            dispatcher: "killactive".to_owned(),
            ..Bind::default()
        }],
        // The names are the normalised ones, because a device has no other:
        // Hyprland keeps `m_hlName` and prints that, and a selector matches
        // against it. `QEMU Virtio Keyboard` is what the kernel said.
        devices: Devices {
            mice: vec![Device {
                address: 1,
                name: "qemu-virtio-tablet".to_owned(),
            }],
            keyboards: vec![Keyboard {
                device: Device {
                    address: 0,
                    name: "qemu-virtio-keyboard".to_owned(),
                },
                layout: "us".to_owned(),
                active_layout_index: Some(0),
                active_keymap: "English (US)".to_owned(),
                groups: 1,
                main: true,
                ..Keyboard::default()
            }],
            ..Devices::default()
        },
        layers: vec![Layer {
            monitor: "HEADLESS-1".to_owned(),
            level: 2,
            address: 7,
            at: (0, 0),
            size: (1024, 32),
            namespace: "bar".to_owned(),
            pid: 0,
        }],
        cursor: (512, 384),
        locked: false,
        options: Vec::new(),
        animations: Vec::new(),
        beziers: Vec::new(),
        errors: Vec::new(),
        log: Vec::new(),
        shortcuts: Vec::new(),
        system: System::default(),
    }
}

/// Answer one request written as a client would send it.
fn ask(line: &str) -> Reply {
    let request = Request::parse(line);
    answer(&request, &snapshot(), Version::default())
}

/// The text of an answer, requiring it to be one.
fn text(line: &str) -> String {
    match ask(line) {
        Reply::Text(text) => text,
        other => panic!("{line:?} asked for {other:?}"),
    }
}

/// The parsed JSON of an answer.
fn json(line: &str) -> parse::Value {
    let answer = text(line);
    parse::parse(&answer).unwrap_or_else(|error| panic!("{line:?}: {error}\n{answer}"))
}

#[test]
fn the_flags_in_front_of_a_request_are_read() {
    assert_eq!(Request::parse("clients").flags, Flags::default());
    assert!(Request::parse("j/clients").flags.json);
    assert_eq!(Request::parse("j/clients").command, "clients");
    let both = Request::parse("jr/clients").flags;
    assert!(both.json && both.raw);
    assert!(Request::parse("a/clients").flags.all);
    assert!(Request::parse("-j/clients").flags.no_newline);
    // An unknown letter is ignored rather than refused, so a client built
    // against a newer Hyprland is still answered.
    assert_eq!(Request::parse("jz/clients").command, "clients");
    assert!(Request::parse("jz/clients").flags.json);
}

#[test]
fn a_command_keeps_its_argument_and_a_slash_in_one_is_left_alone() {
    let request = Request::parse("dispatch movefocus l");
    assert_eq!(request.command, "dispatch");
    assert_eq!(request.argument, "movefocus l");

    // A path in a `keyword` has slashes, and they are past the first space,
    // so they are not flags.
    let request = Request::parse("keyword misc:font_family /usr/share/fonts/x.ttf");
    assert_eq!(request.command, "keyword");
    assert_eq!(request.argument, "misc:font_family /usr/share/fonts/x.ttf");

    // The name is lower-cased, as Hyprland lower-cases it.
    assert_eq!(Request::parse("CLIENTS").command, "clients");
    // And a trailing newline, which a socket carries, is not part of it.
    assert_eq!(Request::parse("clients\n").command, "clients");
}

#[test]
fn a_batch_runs_several_and_they_share_the_flags() {
    let batch = Request::parse_batch("j/[[BATCH]]dispatch workspace 2;dispatch killactive");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].command, "dispatch");
    assert_eq!(batch[0].argument, "workspace 2");
    assert_eq!(batch[1].argument, "killactive");
    assert!(batch.iter().all(|request| request.flags.json));

    // One request is a batch of one.
    let single = Request::parse_batch("clients");
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].command, "clients");
    // And an empty part is dropped, which a trailing `;` makes.
    assert_eq!(Request::parse_batch("[[BATCH]]clients;").len(), 1);
}

#[test]
fn clients_names_every_field_hyprland_names_in_the_order_it_names_them() {
    // A bar reads these by name, and Hyprland's own order is what a person
    // comparing two outputs expects. From `CHyprCtl::getWindowData`.
    let value = json("j/clients");
    let windows = value.items();
    assert_eq!(windows.len(), 1, "only mapped windows without `a`");
    assert_eq!(
        windows[0].keys(),
        [
            "address",
            "mapped",
            "hidden",
            "visible",
            "acceptsInput",
            "at",
            "size",
            "workspace",
            "floating",
            "monitor",
            "class",
            "title",
            "initialClass",
            "initialTitle",
            "pid",
            "xwayland",
            "pinned",
            "pinFullscreened",
            "fullscreen",
            "fullscreenClient",
            "fullscreenHandler",
            "allowedOverFullscreen",
            "grouped",
            "tags",
            "swallowing",
            "focusHistoryID",
            "inhibitingIdle",
            "xdgTag",
            "xdgDescription",
            "contentType",
            "tearingHint",
            "stableId",
        ]
    );
    assert_eq!(
        windows[0].get("address").and_then(parse::Value::text),
        Some("0x1")
    );
    assert_eq!(
        windows[0].get("title").and_then(parse::Value::text),
        Some("one")
    );
    assert_eq!(
        windows[0].get("class").and_then(parse::Value::text),
        Some("rocks.magical.pattern")
    );
    assert_eq!(windows[0].get("at").expect("at").items().len(), 2);
    assert_eq!(
        windows[0]
            .get("size")
            .and_then(|size| size.items().first())
            .and_then(parse::Value::number),
        Some(485.0)
    );
    assert_eq!(
        windows[0]
            .get("workspace")
            .and_then(|workspace| workspace.get("name"))
            .and_then(parse::Value::text),
        Some("1")
    );
}

#[test]
fn a_window_that_is_not_mapped_is_shown_only_with_the_all_flag() {
    assert_eq!(json("j/clients").items().len(), 1);
    assert_eq!(json("ja/clients").items().len(), 2);
    assert!(text("clients").contains("one"));
    assert!(!text("clients").contains("quoted"));
    assert!(text("a/clients").contains("quoted"));
}

#[test]
fn a_title_a_program_chose_cannot_break_the_json_a_bar_parses() {
    // The second window's title holds a quote, a backslash and a newline,
    // which are exactly what an unescaped answer would break on. The parser
    // above refuses a raw control character in a string, so this fails rather
    // than quietly producing something a bar cannot read.
    let value = json("ja/clients");
    let windows = value.items();
    assert_eq!(windows.len(), 2);
    assert_eq!(
        windows[1].get("title").and_then(parse::Value::text),
        Some("a \"quoted\" \\ title\nwith a newline"),
        "the title did not come back as it went in"
    );
}

#[test]
fn activewindow_is_the_focused_one_and_an_empty_object_when_there_is_none() {
    let value = json("j/activewindow");
    assert_eq!(
        value.get("address").and_then(parse::Value::text),
        Some("0x1")
    );

    let mut empty = snapshot();
    empty.active_window = None;
    let request = Request::parse("j/activewindow");
    let Reply::Text(written) = answer(&request, &empty, Version::default()) else {
        panic!("not an answer");
    };
    // Hyprland answers `{}`, and every bar is written for that.
    assert_eq!(written.trim(), "{}");
    let request = Request::parse("activewindow");
    let Reply::Text(written) = answer(&request, &empty, Version::default()) else {
        panic!("not an answer");
    };
    assert_eq!(written.trim(), "Invalid");
}

#[test]
fn monitors_and_workspaces_carry_what_a_bar_reads() {
    let value = json("j/monitors");
    let monitors = value.items();
    assert_eq!(monitors.len(), 1);
    assert_eq!(
        monitors[0].get("name").and_then(parse::Value::text),
        Some("HEADLESS-1")
    );
    assert_eq!(
        monitors[0].get("width").and_then(parse::Value::number),
        Some(1024.0)
    );
    assert_eq!(
        monitors[0]
            .get("refreshRate")
            .and_then(parse::Value::number),
        Some(60.0)
    );
    assert_eq!(
        monitors[0].get("focused").and_then(parse::Value::boolean),
        Some(true)
    );
    // `activeWorkspace` is an object and there is only one field with that
    // name: two fields of one key is a document where which a reader takes
    // is its own business.
    assert_eq!(
        monitors[0]
            .keys()
            .iter()
            .filter(|key| **key == "activeWorkspace")
            .count(),
        1
    );
    assert_eq!(
        monitors[0]
            .get("activeWorkspace")
            .and_then(|workspace| workspace.get("id"))
            .and_then(parse::Value::number),
        Some(1.0)
    );

    let value = json("j/workspaces");
    assert_eq!(value.items().len(), 1);
    assert_eq!(
        value.items()[0]
            .get("windows")
            .and_then(parse::Value::number),
        Some(2.0)
    );
    // waybar's hyprland/window takes the focused workspace's last window
    // and its title from here: a fixed "0x0" is a bar with no title.
    assert_eq!(
        value.items()[0]
            .get("lastwindow")
            .and_then(parse::Value::text),
        Some("0x1")
    );
    assert_eq!(
        value.items()[0]
            .get("lastwindowtitle")
            .and_then(parse::Value::text),
        Some("one")
    );

    let value = json("j/activeworkspace");
    assert_eq!(value.get("id").and_then(parse::Value::number), Some(1.0));
    assert_eq!(
        value.get("lastwindowtitle").and_then(parse::Value::text),
        Some("one")
    );
}

/// What a bar and a script read that is not a window: the bindings, the
/// input devices, the layer surfaces, the pointer and the lock.
///
/// Each field name is Hyprland's, because a script reads them by name and a
/// close-enough name is a script that prints nothing.
#[test]
fn the_rest_of_what_hyprctl_answers_carries_hyprlands_own_fields() {
    let ask = |line: &str| text(line);

    // `binds`: the letters after `bind` are the flags, and the fields are
    // Hyprland's own.
    let readable = ask("binds");
    assert!(
        readable.starts_with(
            "bindr
"
        ),
        "{readable}"
    );
    for wanted in [
        "	modmask: 64",
        "	submap: resize",
        "	key: Q",
        "	dispatcher: killactive",
    ] {
        assert!(readable.contains(wanted), "binds: {readable}");
    }
    let json = parse::parse(&ask("j/binds")).expect("binds is json");
    let first = &json.items()[0];
    assert_eq!(first.get("key").and_then(parse::Value::text), Some("Q"));
    assert_eq!(
        first.get("modmask").and_then(parse::Value::number),
        Some(64.0)
    );
    assert_eq!(
        first.get("submap").and_then(parse::Value::text),
        Some("resize")
    );
    assert_eq!(
        first.get("release").and_then(parse::Value::boolean),
        Some(true)
    );
    assert_eq!(
        first.get("mouse").and_then(parse::Value::boolean),
        Some(false)
    );

    // `devices`: the five groups, in Hyprland's order and under its names.
    let json = parse::parse(&ask("j/devices")).expect("devices is json");
    for group in ["mice", "keyboards", "tablets", "touch", "switches"] {
        assert!(json.get(group).is_some(), "devices has no {group}");
    }
    let keyboard = &json.get("keyboards").expect("keyboards").items()[0];
    assert_eq!(
        keyboard.get("name").and_then(parse::Value::text),
        Some("qemu-virtio-keyboard")
    );
    assert_eq!(
        keyboard.get("layout").and_then(parse::Value::text),
        Some("us")
    );
    assert_eq!(
        keyboard.get("main").and_then(parse::Value::boolean),
        Some(true)
    );
    let readable = ask("devices");
    assert!(
        readable.starts_with(
            "mice:
"
        ),
        "{readable}"
    );
    assert!(readable.contains("Keyboards:"), "{readable}");
    assert!(readable.contains("			main: yes"), "{readable}");

    // `layers`: by monitor, then by level, which is how a bar finds its own.
    let json = parse::parse(&ask("j/layers")).expect("layers is json");
    let levels = json
        .get("HEADLESS-1")
        .and_then(|monitor| monitor.get("levels"))
        .expect("the monitor's levels");
    assert_eq!(levels.get("0").map(|level| level.items().len()), Some(0));
    let top = &levels.get("2").expect("the top level").items()[0];
    assert_eq!(
        top.get("namespace").and_then(parse::Value::text),
        Some("bar")
    );
    assert_eq!(top.get("h").and_then(parse::Value::number), Some(32.0));
    let readable = ask("layers");
    assert!(readable.contains("Monitor HEADLESS-1:"), "{readable}");
    assert!(readable.contains("	Layer level 2 (top):"), "{readable}");
    assert!(readable.contains("namespace: bar"), "{readable}");

    // `cursorpos` and `locked`, which a script asks for on one line.
    assert_eq!(ask("cursorpos"), "512, 384\n");
    let json = parse::parse(&ask("j/cursorpos")).expect("cursorpos is json");
    assert_eq!(json.get("x").and_then(parse::Value::number), Some(512.0));
    assert_eq!(ask("locked"), "false\n");

    // And the two that have nothing to list are empty rather than unknown.
    for command in ["workspacerules", "globalshortcuts"] {
        let json = parse::parse(&ask(&format!("j/{command}")))
            .unwrap_or_else(|error| panic!("{command}: {error}"));
        assert_eq!(json.items().len(), 0, "{command} listed something");
    }
}

/// `submapRequest`: the name on a line, `default` for the global map, and a
/// bare JSON string rather than an object.
#[test]
fn the_submap_is_asked_for_by_name() {
    assert_eq!(text("submap"), "default\n");
    assert_eq!(text("j/submap"), "\"default\"\n");

    let mut inside = snapshot();
    inside.submap = "resize".to_owned();
    let asked = |line: &str| match answer(&Request::parse(line), &inside, Version::default()) {
        Reply::Text(text) => text,
        other => panic!("{line:?} asked for {other:?}"),
    };
    assert_eq!(asked("submap"), "resize\n");
    assert_eq!(asked("j/submap"), "\"resize\"\n");
}

#[test]
fn the_compact_form_has_no_newlines_in_it() {
    let pretty = text("j/clients");
    let compact = text("jr/clients");
    assert!(pretty.contains("\n    "), "`j` alone is pretty-printed");
    assert_eq!(
        compact.trim().lines().count(),
        1,
        "`r` asks for it on one line: {compact}"
    );
    // Both are the same document.
    assert_eq!(
        parse::parse(&pretty).expect("pretty is json"),
        parse::parse(&compact).expect("compact is json")
    );
    // And `-` leaves the trailing newline off.
    assert!(text("j/clients").ends_with('\n'));
    assert!(!text("-j/clients").ends_with('\n'));
}

#[test]
fn what_changes_the_compositor_comes_back_for_it_to_do() {
    assert_eq!(
        ask("dispatch movefocus l"),
        Reply::Dispatch {
            name: "movefocus".to_owned(),
            argument: "l".to_owned()
        }
    );
    assert_eq!(
        ask("dispatch killactive"),
        Reply::Dispatch {
            name: "killactive".to_owned(),
            argument: String::new()
        }
    );
    assert_eq!(
        ask("keyword general:gaps_in 10"),
        Reply::Keyword {
            name: "general:gaps_in".to_owned(),
            value: "10".to_owned()
        }
    );
    assert_eq!(ask("reload"), Reply::Reload);
}

#[test]
fn an_unknown_request_is_answered_rather_than_refused() {
    // Hyprland answers with a line and keeps the connection, so a program
    // asking for something newer keeps working for everything else it asks.
    let written = text("nonsense");
    assert!(written.contains("unknown request nonsense"), "{written}");
    assert!(!text("").is_empty(), "even an empty request is answered");
}

#[test]
fn version_says_what_this_compositor_is() {
    let readable = text("version");
    assert!(readable.contains("hyprix"), "{readable}");
    let value = json("j/version");
    // A bar reads `tag`; Hyprland's other fields are there so nothing looking
    // for them crashes.
    assert!(value.get("tag").and_then(parse::Value::text).is_some());
    assert!(value.get("commit").is_some());
    assert!(value.get("flags").is_some());
}

#[test]
fn the_json_writer_escapes_what_json_requires() {
    let mut out = Json::new(false);
    out.object();
    out.string("text", "a\"b\\c\nd\te\u{1}f");
    out.end('}');
    let written = out.finish();
    assert_eq!(
        written, "{\"text\":\"a\\\"b\\\\c\\nd\\te\\u0001f\"}",
        "an escape is missing"
    );
    let value = parse::parse(&written).expect("still json");
    assert_eq!(
        value.get("text").and_then(parse::Value::text),
        Some("a\"b\\c\nd\te\u{1}f")
    );
}

#[test]
fn every_answer_this_crate_writes_is_json_when_json_was_asked_for() {
    for command in [
        "version",
        "monitors",
        "workspaces",
        "clients",
        "activewindow",
        "activeworkspace",
        "binds",
        "devices",
        "layers",
        "cursorpos",
        "locked",
        "submap",
    ] {
        for flags in ["j", "jr", "ja"] {
            let line = format!("{flags}/{command}");
            let written = text(&line);
            let _ =
                parse::parse(&written).unwrap_or_else(|error| panic!("{line}: {error}\n{written}"));
        }
        // And the readable form is not empty, which is what `hyprctl` without
        // `-j` prints.
        assert!(!text(command).is_empty(), "{command} said nothing");
    }
    assert_eq!(Flags::default().format(), Format::Readable);
}

// ---------------------------------------------------------------------------
// The event stream
//
// The line is `CEventManager::formatEvent`'s, and each payload is the one
// Hyprland's own `postEvent` call builds; the variants cite the file and the
// line. A bar reads these, so a shape that is nearly right is a bar that
// shows the wrong thing.
// ---------------------------------------------------------------------------

use crate::{Event, MAX_DATA, Watcher, WindowRef};

fn lines(event: &Event) -> Vec<String> {
    event.lines()
}

#[test]
fn an_event_is_its_name_then_two_angles_then_its_data() {
    assert_eq!(
        lines(&Event::Workspace {
            id: 2,
            name: "2".to_owned()
        }),
        ["workspace>>2\n", "workspacev2>>2,2\n"]
    );
    assert_eq!(
        lines(&Event::ConfigReloaded),
        ["configreloaded>>\n"],
        "an event with no data is still a line"
    );
    assert_eq!(lines(&Event::Fullscreen(true)), ["fullscreen>>1\n"]);
    assert_eq!(lines(&Event::Fullscreen(false)), ["fullscreen>>0\n"]);
}

/// Hyprland prints an address as lower-case hexadecimal with no `0x`, because
/// it prints a pointer with `{:x}`.
#[test]
fn an_address_is_bare_lower_case_hexadecimal() {
    assert_eq!(
        lines(&Event::CloseWindow {
            address: 0xDEAD_BEEF
        }),
        ["closewindow>>deadbeef\n"]
    );
    assert_eq!(lines(&Event::Urgent { address: 0x2A }), ["urgent>>2a\n"]);
}

#[test]
fn the_focused_window_is_named_by_class_and_title_and_by_address() {
    let window = WindowRef {
        address: 0x1234,
        class: "rocks.magical.pattern".to_owned(),
        title: "one".to_owned(),
    };
    assert_eq!(
        lines(&Event::ActiveWindow(Some(window))),
        [
            "activewindow>>rocks.magical.pattern,one\n",
            "activewindowv2>>1234\n"
        ]
    );
}

/// With nothing focused Hyprland sends a bare comma and an empty payload,
/// which is what its `FocusState.cpp:243` writes. A reader splits on the
/// comma and gets two empty fields, and that is the signal.
#[test]
fn nothing_focused_is_a_comma_and_nothing() {
    assert_eq!(
        lines(&Event::ActiveWindow(None)),
        ["activewindow>>,\n", "activewindowv2>>\n"]
    );
}

#[test]
fn every_event_that_has_a_v2_form_sends_both() {
    let both = [
        Event::Workspace {
            id: 1,
            name: "1".to_owned(),
        },
        Event::CreateWorkspace {
            id: 1,
            name: "1".to_owned(),
        },
        Event::DestroyWorkspace {
            id: 1,
            name: "1".to_owned(),
        },
        Event::FocusedMonitor {
            monitor: "HEADLESS-1".to_owned(),
            workspace: (1, "1".to_owned()),
        },
        Event::ActiveWindow(None),
        Event::MoveWindow {
            address: 1,
            workspace: (2, "2".to_owned()),
        },
        Event::WindowTitle {
            address: 1,
            title: "t".to_owned(),
        },
        Event::MonitorAdded {
            id: 0,
            name: "HEADLESS-1".to_owned(),
            description: String::new(),
        },
        Event::MonitorRemoved {
            id: 0,
            name: "HEADLESS-1".to_owned(),
            description: String::new(),
        },
    ];
    for event in both {
        let sent = lines(&event);
        assert_eq!(sent.len(), 2, "{event:?} sent {}", sent.len());
        let names: Vec<&str> = sent
            .iter()
            .filter_map(|line| line.split_once(">>").map(|(name, _)| name))
            .collect();
        assert_eq!(
            names.get(1).map(|name| name.ends_with("v2")),
            Some(true),
            "{event:?}: {names:?}"
        );
    }

    // And the ones Hyprland sends once are sent once.
    for event in [
        Event::OpenWindow {
            address: 1,
            workspace: "1".to_owned(),
            class: "c".to_owned(),
            title: "t".to_owned(),
        },
        Event::CloseWindow { address: 1 },
        Event::Fullscreen(true),
        Event::ConfigReloaded,
        Event::Submap(String::new()),
        Event::OpenLayer("bar".to_owned()),
        Event::CloseLayer("bar".to_owned()),
        Event::Urgent { address: 1 },
        Event::FloatingMode {
            address: 1,
            floating: true,
        },
    ] {
        assert_eq!(lines(&event).len(), 1, "{event:?}");
    }
}

/// One event is one line, whatever a client called its window. A title with
/// a newline in it would otherwise be two lines and a reader would take the
/// second for another event.
#[test]
fn a_newline_in_the_data_becomes_a_space() {
    let sent = lines(&Event::WindowTitle {
        address: 1,
        title: "two\nlines".to_owned(),
    });
    assert_eq!(
        sent.get(1).map(String::as_str),
        Some("windowtitlev2>>1,two lines\n")
    );
    for line in &sent {
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
    }
}

/// Hyprland cuts the payload at 1024 bytes, so a client with an enormous
/// title cannot make an enormous line.
#[test]
fn a_payload_is_cut_at_a_kilobyte() {
    let long = "x".repeat(MAX_DATA * 2);
    let sent = lines(&Event::OpenLayer(long));
    let line = sent.first().map(String::as_str).unwrap_or("");
    assert_eq!(line.len(), "openlayer>>".len() + MAX_DATA + 1);
    // The cut is on a character boundary: a multi-byte character straddling
    // the limit is dropped rather than halved, which would be invalid UTF-8.
    let wide = "\u{00e9}".repeat(MAX_DATA);
    let sent = lines(&Event::OpenLayer(wide));
    let line = sent.first().map(String::as_str).unwrap_or("");
    assert!(line.is_char_boundary(line.len()));
    assert_eq!(line.len(), "openlayer>>".len() + MAX_DATA + 1);
}

// -- What changed -----------------------------------------------------------

fn watched_window(address: u64, title: &str) -> Window {
    Window {
        address,
        mapped: true,
        visible: true,
        workspace: 1,
        workspace_name: "1".to_owned(),
        class: "rocks.magical.pattern".to_owned(),
        title: title.to_owned(),
        ..Window::default()
    }
}

fn watched(windows: &[Window], active: Option<u64>) -> Snapshot {
    Snapshot {
        plugins: Vec::new(),
        workspace_rules: Vec::new(),
        monitors: vec![Monitor {
            id: 0,
            name: "HEADLESS-1".to_owned(),
            width: 1024,
            height: 768,
            active_workspace: 1,
            active_workspace_name: "1".to_owned(),
            focused: true,
            ..Monitor::default()
        }],
        workspaces: vec![Workspace {
            id: 1,
            name: "1".to_owned(),
            monitor: "HEADLESS-1".to_owned(),
            windows: windows.len() as u32,
            has_fullscreen: false,
        }],
        windows: windows.to_vec(),
        active_window: active,
        active_workspace: 1,
        submap: String::new(),
        // One of each of the things a bar reads that is not a window, so
        // that every answer this crate writes has something in it.
        binds: vec![Bind {
            release: true,
            modmask: 64,
            submap: "resize".to_owned(),
            submap_universal: true,
            key: "Q".to_owned(),
            dispatcher: "killactive".to_owned(),
            ..Bind::default()
        }],
        // The names are the normalised ones, because a device has no other:
        // Hyprland keeps `m_hlName` and prints that, and a selector matches
        // against it. `QEMU Virtio Keyboard` is what the kernel said.
        devices: Devices {
            mice: vec![Device {
                address: 1,
                name: "qemu-virtio-tablet".to_owned(),
            }],
            keyboards: vec![Keyboard {
                device: Device {
                    address: 0,
                    name: "qemu-virtio-keyboard".to_owned(),
                },
                layout: "us".to_owned(),
                active_layout_index: Some(0),
                active_keymap: "English (US)".to_owned(),
                groups: 1,
                main: true,
                ..Keyboard::default()
            }],
            ..Devices::default()
        },
        layers: vec![Layer {
            monitor: "HEADLESS-1".to_owned(),
            level: 2,
            address: 7,
            at: (0, 0),
            size: (1024, 32),
            namespace: "bar".to_owned(),
            pid: 0,
        }],
        cursor: (512, 384),
        locked: false,
        options: Vec::new(),
        animations: Vec::new(),
        beziers: Vec::new(),
        errors: Vec::new(),
        log: Vec::new(),
        shortcuts: Vec::new(),
        system: System::default(),
    }
}

/// Entering a submap and leaving it are each one event, and the name is the
/// payload -- empty for the global map, which is how a bar knows the mode
/// has ended.
#[test]
fn entering_and_leaving_a_submap_is_each_one_event() {
    let mut watcher = Watcher::new();
    let _ = watcher.changed(&watched(&[watched_window(1, "one")], Some(1)));

    let mut inside = watched(&[watched_window(1, "one")], Some(1));
    inside.submap = "resize".to_owned();
    let events = watcher.changed(&inside);
    assert_eq!(events, vec![Event::Submap("resize".to_owned())]);
    assert_eq!(
        events.first().map(Event::lines),
        Some(vec!["submap>>resize\n".to_owned()])
    );

    assert!(watcher.changed(&inside).is_empty(), "nothing changed");

    let out = watched(&[watched_window(1, "one")], Some(1));
    assert_eq!(
        watcher.changed(&out),
        vec![Event::Submap(String::new())],
        "leaving says so with an empty payload"
    );
}

/// A bar that connected before the compositor finished starting has to be
/// told what already exists, or it shows an empty screen until something
/// moves.
#[test]
fn the_first_look_reports_everything_as_new() {
    let mut watcher = Watcher::new();
    let events = watcher.changed(&watched(&[watched_window(1, "one")], Some(1)));
    assert!(
        matches!(events.first(), Some(Event::MonitorAdded { .. })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::CreateWorkspace { id: 1, .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::OpenWindow { address: 1, .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ActiveWindow(Some(_))))
    );

    // Nothing changed, nothing said.
    assert!(
        watcher
            .changed(&watched(&[watched_window(1, "one")], Some(1)))
            .is_empty()
    );
}

#[test]
fn a_window_arriving_leaving_and_being_renamed_is_each_one_event() {
    let mut watcher = Watcher::new();
    let _ = watcher.changed(&watched(&[watched_window(1, "one")], Some(1)));

    let events = watcher.changed(&watched(
        &[watched_window(1, "one"), watched_window(2, "two")],
        Some(2),
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::OpenWindow { address: 2, .. }))
            .count(),
        1,
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ActiveWindow(Some(window)) if window.address == 2
    )));

    let events = watcher.changed(&watched(
        &[watched_window(1, "renamed"), watched_window(2, "two")],
        Some(2),
    ));
    assert_eq!(
        events,
        vec![Event::WindowTitle {
            address: 1,
            title: "renamed".to_owned()
        }]
    );

    let events = watcher.changed(&watched(&[watched_window(1, "renamed")], None));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::CloseWindow { address: 2 }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ActiveWindow(None)))
    );
}

#[test]
fn a_window_moving_workspace_and_floating_are_each_one_event() {
    let mut watcher = Watcher::new();
    let _ = watcher.changed(&watched(&[watched_window(1, "one")], Some(1)));

    let mut moved = watched_window(1, "one");
    moved.workspace = 2;
    moved.workspace_name = "2".to_owned();
    moved.floating = true;
    let events = watcher.changed(&watched(&[moved], Some(1)));
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::MoveWindow { address: 1, workspace } if workspace.0 == 2
        )),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::FloatingMode {
            address: 1,
            floating: true
        }
    )));
}

/// Hyprland's `fullscreen` event names no window: it is the focused one's
/// state, so a focus change onto a fullscreen window is one too.
#[test]
fn fullscreen_follows_the_focused_window() {
    let mut watcher = Watcher::new();
    let _ = watcher.changed(&watched(&[watched_window(1, "one")], Some(1)));

    let mut full = watched_window(1, "one");
    full.fullscreen = true;
    let events = watcher.changed(&watched(&[full.clone()], Some(1)));
    assert!(events.contains(&Event::Fullscreen(true)), "{events:?}");
    let events = watcher.changed(&watched(&[watched_window(1, "one")], Some(1)));
    assert!(events.contains(&Event::Fullscreen(false)), "{events:?}");
}

/// `hyprctl monitors` prints the scratchpad over a monitor, and prints a
/// zero and an empty name for one showing none: a bar reads that field to
/// know whether the scratchpad is up.
#[test]
fn a_monitor_says_which_scratchpad_is_over_it() {
    let mut state = snapshot();
    let readable = |state: &Snapshot| {
        let Reply::Text(text) = answer(&Request::parse("monitors"), state, Version::default())
        else {
            panic!("monitors is text");
        };
        text
    };
    assert!(
        readable(&state).contains("special workspace: 0 ()"),
        "{}",
        readable(&state)
    );

    if let Some(monitor) = state.monitors.first_mut() {
        monitor.special_workspace = Some((-99, "special:special".to_owned()));
    }
    assert!(
        readable(&state).contains("special workspace: -99 (special:special)"),
        "{}",
        readable(&state)
    );

    // And in JSON, where a bar reads it by name. The state is the one
    // `json` answers from, so the scratchpad is put there too.
    let Reply::Text(text) = answer(&Request::parse("j/monitors"), &state, Version::default())
    else {
        panic!("monitors is text");
    };
    let parsed = parse::parse(&text).expect("the answer is JSON");
    let special = parsed.items()[0]
        .get("specialWorkspace")
        .expect("a specialWorkspace object");
    assert_eq!(
        special.get("name").and_then(parse::Value::text),
        Some("special:special")
    );
    assert_eq!(
        special.get("id").and_then(parse::Value::number),
        Some(-99.0)
    );
}

/// A window in a group: `grouped` holds every member's address in the
/// group's order, and the members a group does not draw are listed as
/// hidden rather than left out, which is what a bar drawing tabs reads.
/// From `getGroupedData` in `src/debug/HyprCtl.cpp`.
#[test]
fn a_grouped_window_names_its_group() {
    let mut snap = snapshot();
    for (index, window) in snap.windows.iter_mut().enumerate() {
        window.grouped = vec![1, 2];
        window.hidden = index > 0;
        window.visible = index == 0;
        window.mapped = true;
    }
    let request = Request::parse("j/clients");
    let Reply::Text(json) = answer(&request, &snap, Version::default()) else {
        panic!("clients answered with no text");
    };
    let value = parse::parse(&json).unwrap_or_else(|error| panic!("{error}\n{json}"));
    let windows = value.items();
    assert_eq!(windows.len(), 2, "a hidden member is still listed");
    let addresses = |at: usize| -> Vec<String> {
        windows[at]
            .get("grouped")
            .expect("grouped")
            .items()
            .iter()
            .filter_map(|item| item.text().map(str::to_owned))
            .collect()
    };
    assert_eq!(addresses(0), ["0x1", "0x2"]);
    assert_eq!(addresses(1), ["0x1", "0x2"]);
    assert_eq!(
        windows[1].get("hidden").and_then(parse::Value::boolean),
        Some(true)
    );

    // And the readable form prints them without `0x`, as Hyprland does.
    let request = Request::parse("clients");
    let Reply::Text(readable) = answer(&request, &snap, Version::default()) else {
        panic!("clients answered with no text");
    };
    assert!(readable.contains("\tgrouped: 1,2\n"), "{readable}");
    assert!(readable.contains("\thidden: 1\n"), "{readable}");
}

/// The readable form prints what `-j` does, field for field and in the same
/// order, as `CHyprCtl::getWindowData`'s two formats do -- with Hyprland's
/// one difference of spelling, `stableID` against `stableId` -- and its
/// flags as `0` and `1`, not `true` and `false`.
#[test]
fn the_readable_window_has_the_json_fields_in_order() {
    let readable = text("clients");
    let first = readable.split("\n\n").next().unwrap_or("");
    // The address is the header's, `Window 1 -> one:`.
    let names: Vec<String> = std::iter::once("address".to_owned())
        .chain(
            first
                .lines()
                .skip(1)
                .filter_map(|line| line.trim_start_matches('\t').split_once(':'))
                .map(|(name, _)| name.to_ascii_lowercase()),
        )
        .collect();
    assert!(first.starts_with("Window 1 -> one:\n"), "{readable}");
    let keys: Vec<String> = json("j/clients").items()[0]
        .keys()
        .iter()
        .map(|key| key.to_ascii_lowercase())
        .collect();
    assert_eq!(names, keys, "{readable}");
    assert!(first.contains("\n\tmapped: 1\n"), "{readable}");
    assert!(first.contains("\n\tacceptsInput: 1\n"), "{readable}");
}

/// A window in no group prints Hyprland's empty array, and its `0` in the
/// readable form.
#[test]
fn an_ungrouped_window_says_so() {
    let value = json("j/clients");
    assert!(
        value.items()[0]
            .get("grouped")
            .expect("grouped")
            .items()
            .is_empty()
    );
    assert!(text("clients").contains("\tgrouped: 0\n"));
}

/// The four states of a group, each the event Hyprland posts for it: made,
/// joined, left, dissolved. From `CGroup::create`, `CGroup::destroy` and the
/// two dispatchers in `ConfigActions.cpp`.
#[test]
fn a_group_being_made_joined_left_and_dissolved_is_four_events() {
    let grouped = |members: &[u64], of: &[u64]| -> Vec<Window> {
        members
            .iter()
            .map(|address| Window {
                grouped: of.to_vec(),
                ..watched_window(*address, if *address == 1 { "one" } else { "two" })
            })
            .collect()
    };
    let mut watcher = Watcher::new();
    // Two windows, neither grouped.
    let _ = watcher.changed(&watched(
        &[watched_window(1, "one"), watched_window(2, "two")],
        Some(1),
    ));

    // `togglegroup` on window 1: a group of one, headed by it.
    let mut windows = grouped(&[1], &[1]);
    windows.push(watched_window(2, "two"));
    let made = watcher.changed(&watched(&windows, Some(1)));
    assert_eq!(
        made.iter().flat_map(Event::lines).collect::<Vec<_>>(),
        ["togglegroup>>1,1\n"]
    );

    // `moveintogroup`: window 2 joins the group window 1 heads.
    let joined = watcher.changed(&watched(&grouped(&[1, 2], &[1, 2]), Some(1)));
    assert_eq!(
        joined.iter().flat_map(Event::lines).collect::<Vec<_>>(),
        ["moveintogroup>>2\n"]
    );

    // `moveoutofgroup`: it leaves, and the group is still there.
    let mut windows = grouped(&[1], &[1]);
    windows.push(watched_window(2, "two"));
    let left = watcher.changed(&watched(&windows, Some(1)));
    assert_eq!(
        left.iter().flat_map(Event::lines).collect::<Vec<_>>(),
        ["moveoutofgroup>>2\n"]
    );

    // `togglegroup` again: the group goes, and its head is named.
    let gone = watcher.changed(&watched(
        &[watched_window(1, "one"), watched_window(2, "two")],
        Some(1),
    ));
    assert_eq!(
        gone.iter().flat_map(Event::lines).collect::<Vec<_>>(),
        ["togglegroup>>0,1\n"]
    );
}

/// A batch makes a group and fills it in one pass, so the watcher sees both
/// at once: the head made it, and every other member joined it.
#[test]
fn a_group_made_and_joined_in_one_pass_is_both_events() {
    let mut watcher = Watcher::new();
    let _ = watcher.changed(&watched(
        &[watched_window(1, "one"), watched_window(2, "two")],
        Some(1),
    ));
    let members: Vec<Window> = [(2_u64, "two"), (1, "one")]
        .into_iter()
        .map(|(address, title)| Window {
            grouped: vec![2, 1],
            style: Style::default(),
            ..watched_window(address, title)
        })
        .collect();
    let events = watcher.changed(&watched(&members, Some(1)));
    assert_eq!(
        events.iter().flat_map(Event::lines).collect::<Vec<_>>(),
        ["togglegroup>>1,2\n", "moveintogroup>>1\n"]
    );
}

/// `hyprctl plugin list`, in the shape Hyprland prints it: the name, the
/// author, the handle, the version and the description, and the JSON with
/// those fields under those names. From `HyprCtl.cpp`'s `dispatchPlugin`.
#[test]
fn the_plugins_are_listed_as_hyprland_lists_them() {
    let mut snap = snapshot();
    snap.plugins = vec![crate::Plugin {
        name: "swap".to_owned(),
        author: "ferrix".to_owned(),
        version: "1.0".to_owned(),
        description: "swaps the two windows".to_owned(),
        handle: 0x2a,
        dispatchers: vec!["swapthem".to_owned()],
    }];
    let ask = |snap: &Snapshot, line: &str| -> String {
        match answer(&Request::parse(line), snap, Version::default()) {
            Reply::Text(text) => text,
            other => panic!("{line:?} asked for {other:?}"),
        }
    };
    let readable = ask(&snap, "plugin list");
    assert!(readable.contains("Plugin swap by ferrix:"), "{readable}");
    assert!(readable.contains("\tHandle: 2a\n"), "{readable}");
    assert!(readable.contains("\tVersion: 1.0\n"), "{readable}");
    assert!(
        readable.contains("\tDescription: swaps the two windows\n"),
        "{readable}"
    );
    // What Hyprland has no line for, because its plugins add dispatchers to
    // the compositor's own table and this one's keep them.
    assert!(readable.contains("\tDispatchers: swapthem\n"), "{readable}");

    let value = parse::parse(&ask(&snap, "j/plugin list")).expect("the JSON parses");
    let listed = value.items();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].get("name").and_then(parse::Value::text),
        Some("swap")
    );
    assert_eq!(
        listed[0].get("handle").and_then(parse::Value::text),
        Some("2a")
    );

    // No plugins is Hyprland's own sentence, and an unknown option its own
    // word.
    snap.plugins.clear();
    assert_eq!(ask(&snap, "plugin list"), "no plugins loaded\n");
    assert_eq!(ask(&snap, "plugin load /tmp/x.so"), "unknown opt\n");
}

// -- The rest of what `hyprctl` reads ----------------------------------------

/// A snapshot that also holds what the later commands read.
fn told() -> Snapshot {
    let mut snapshot = snapshot();
    snapshot.options = vec![
        Opt {
            name: "general:gaps_in".to_owned(),
            value: "4 4 4 4".to_owned(),
            kind: "custom",
            set: true,
        },
        Opt {
            name: "general:border_size".to_owned(),
            value: "1".to_owned(),
            kind: "int",
            set: false,
        },
        Opt {
            name: "general:layout".to_owned(),
            value: "dwindle".to_owned(),
            kind: "str",
            set: false,
        },
    ];
    snapshot.animations = vec![Animation {
        name: "windows".to_owned(),
        overridden: true,
        bezier: "myBezier".to_owned(),
        enabled: true,
        speed: 7.0,
        style: "slide".to_owned(),
    }];
    snapshot.beziers = vec![Bezier {
        name: "myBezier".to_owned(),
        first: (0.05, 0.9),
        second: (0.1, 1.05),
    }];
    snapshot.errors = vec!["general:gaps_in = x: not a number".to_owned()];
    snapshot.log = vec![
        "hyprix: 1 monitor".to_owned(),
        "hyprix: frames 1".to_owned(),
    ];
    snapshot.shortcuts = vec![Shortcut {
        name: "rocks.magical.cast:record".to_owned(),
        description: "Start recording".to_owned(),
    }];
    snapshot.system = System {
        os: "Ferrix".to_owned(),
        kernel: "0.1.0".to_owned(),
        counts: (1, 2, 3),
        uptime: 42,
    };
    snapshot
}

/// Answer one request against that snapshot.
fn told_text(line: &str) -> String {
    match answer(&Request::parse(line), &told(), Version::default()) {
        Reply::Text(text) => text,
        other => panic!("{line:?} asked for {other:?}"),
    }
}

/// `getoption` puts the value under the key its *type* names, which is what
/// a script reads, and says whether the configuration set it.
#[test]
fn getoption_names_the_value_after_its_type() {
    assert_eq!(
        told_text("getoption general:border_size"),
        "int: 1\nset: false\n"
    );
    assert_eq!(
        told_text("getoption general:layout"),
        "str: dwindle\nset: false\n"
    );
    // Hyprland writes a complex value as `custom type:` and its JSON key as
    // `custom`.
    assert_eq!(
        told_text("getoption general:gaps_in"),
        "custom type: 4 4 4 4\nset: true\n"
    );
    assert_eq!(told_text("getoption nothing:here"), "no such option\n");

    let json = parse::parse(&told_text("j/getoption general:border_size")).expect("JSON");
    assert_eq!(
        json.get("option"),
        Some(&parse::Value::Text("general:border_size".to_owned()))
    );
    assert_eq!(json.get("int"), Some(&parse::Value::Number(1.0)));
    assert_eq!(json.get("set"), Some(&parse::Value::Bool(false)));
    // A string goes quoted under `str`, a number bare under `int`.
    let json = parse::parse(&told_text("j/getoption general:layout")).expect("JSON");
    assert_eq!(
        json.get("str"),
        Some(&parse::Value::Text("dwindle".to_owned()))
    );
}

/// `animations` prints the tree and then the beziers, in Hyprland's shape.
#[test]
fn animations_prints_the_tree_and_the_beziers() {
    let written = told_text("animations");
    for wanted in [
        "animations:",
        "name: windows",
        "overridden: 1",
        "bezier: myBezier",
        "speed: 7.00",
        "style: slide",
        "beziers:",
        "X0: 0.05",
        "Y1: 1.05",
    ] {
        assert!(written.contains(wanted), "{wanted:?} in {written:?}");
    }

    // The JSON is two arrays: the animations, then the beziers.
    let json = parse::parse(&told_text("j/animations")).expect("JSON");
    let parse::Value::List(both) = &json else {
        panic!("an array of two");
    };
    assert_eq!(both.len(), 2);
    assert_eq!(
        both.first()
            .and_then(|list| match list {
                parse::Value::List(items) => items.first(),
                _ => None,
            })
            .and_then(|first| first.get("bezier")),
        Some(&parse::Value::Text("myBezier".to_owned()))
    );
}

/// `configerrors`, `rollinglog`, `systeminfo` and `globalshortcuts`.
#[test]
fn the_compositor_says_what_it_is_and_what_went_wrong() {
    assert_eq!(
        told_text("configerrors"),
        "general:gaps_in = x: not a number\n"
    );
    assert_eq!(
        told_text("rollinglog"),
        "hyprix: 1 monitor\nhyprix: frames 1\n"
    );
    let written = told_text("systeminfo");
    for wanted in ["os: Ferrix", "monitors: 1", "windows: 2", "uptime: 42 s"] {
        assert!(written.contains(wanted), "{wanted:?} in {written:?}");
    }
    // `status` is the same answer under another name, as Hyprland's is.
    assert_eq!(told_text("status"), written);
    assert_eq!(
        told_text("globalshortcuts"),
        "rocks.magical.cast:record -> Start recording\n"
    );

    let json = parse::parse(&told_text("j/configerrors")).expect("JSON");
    assert_eq!(
        json,
        parse::Value::List(vec![parse::Value::Text(
            "general:gaps_in = x: not a number".to_owned()
        )])
    );
}

/// `notify`, `dismissnotify` and `seterror` come back for the compositor to
/// say, with the message the person is to see.
#[test]
fn a_notification_comes_back_with_its_message() {
    assert_eq!(
        ask("notify 1 3000 rgb(ff0000) something happened"),
        Reply::Notify {
            message: "something happened".to_owned(),
            error: false
        }
    );
    assert_eq!(
        ask("dismissnotify"),
        Reply::Notify {
            message: String::new(),
            error: false
        }
    );
    assert_eq!(
        ask("seterror rgb(ff0000) the configuration is wrong"),
        Reply::Notify {
            message: "the configuration is wrong".to_owned(),
            error: true
        }
    );
    assert_eq!(
        ask("seterror disable"),
        Reply::Notify {
            message: String::new(),
            error: true
        }
    );
}

/// The commands that act on something this compositor does not have say so,
/// rather than pretending or refusing.
#[test]
fn the_commands_with_nothing_to_act_on_say_so() {
    for (line, wanted) in [
        ("output create headless", "the card's"),
        ("setcursor Adwaita 24", "no theme"),
        ("kill", "click-to-kill"),
    ] {
        let written = told_text(line);
        assert!(written.contains(wanted), "{line:?} said {written:?}");
    }
    // And a window that is there has a border and nothing else.
    assert!(told_text("decorations one").contains("border"));
    assert_eq!(told_text("decorations nothing"), "");
}

/// `getprop` answers one property of one window, bare in the readable form
/// and under its own key in JSON, which is what a script reads.
#[test]
fn getprop_answers_one_property_of_one_window() {
    let mut snapshot = told();
    if let Some(window) = snapshot.windows.first_mut() {
        window.style = Style {
            alpha: Some(0.5),
            rounding: Some(8),
            no_blur: true,
            ..Style::default()
        };
    }
    let ask = |line: &str| match answer(&Request::parse(line), &snapshot, Version::default()) {
        Reply::Text(text) => text,
        other => panic!("{line:?} asked for {other:?}"),
    };
    assert_eq!(ask("getprop one alpha"), "0.5\n");
    assert_eq!(ask("getprop one rounding"), "8\n");
    assert_eq!(ask("getprop one noblur"), "true\n");
    // A property nothing set is the compositor's own, which is what a
    // person asking wants to know.
    assert_eq!(ask("getprop one bordersize"), "-1\n");
    assert_eq!(ask("getprop one floating"), "false\n");
    assert_eq!(ask("getprop nothing alpha"), "window not found\n");
    assert_eq!(ask("getprop one wobble"), "prop not found\n");
    assert_eq!(ask("getprop one"), "not enough args\n");

    let json = parse::parse(&ask("j/getprop one rounding")).expect("JSON");
    assert_eq!(json.get("rounding"), Some(&parse::Value::Number(8.0)));
    let json = parse::parse(&ask("j/getprop one title")).expect("JSON");
    assert_eq!(
        json.get("title"),
        Some(&parse::Value::Text("one".to_owned()))
    );
}

/// `hyprctl workspacerules` prints every `workspace =` line, and prints a
/// field the line did not set as unset rather than as false.
///
/// `border:` unset and `border:false` are different things -- the first
/// leaves `general:border_size` alone and the second insists the workspace
/// has none -- so a reader that could not tell them apart would have to
/// guess, and a bar drawing a workspace's state would guess wrong.
#[test]
fn the_workspace_rules_are_listed() {
    let mut snapshot = snapshot();
    snapshot.workspace_rules = [
        "1, monitor:DP-1, gapsout:0, border:false",
        "name:code, default:1",
    ]
    .iter()
    .map(|line| compositor_config::WorkspaceRule::parse(line).expect("a rule"))
    .collect();
    let ask = |line: &str| match answer(&Request::parse(line), &snapshot, Version::default()) {
        Reply::Text(text) => text,
        other => panic!("{line:?} asked for {other:?}"),
    };

    let json = parse::parse(&ask("j/workspacerules")).expect("workspacerules is json");
    let items = json.items();
    assert_eq!(items.len(), 2);
    let first = &items[0];
    assert_eq!(
        first.get("workspaceString").and_then(parse::Value::text),
        Some("1")
    );
    assert_eq!(
        first.get("monitor").and_then(parse::Value::text),
        Some("DP-1")
    );
    assert_eq!(
        first.get("border").and_then(parse::Value::boolean),
        Some(false),
        "`border:false` prints as border false"
    );
    assert!(
        first.get("persistent").is_none(),
        "a field the line did not set is left out"
    );
    assert_eq!(
        items[1].get("default").and_then(parse::Value::boolean),
        Some(true)
    );

    let readable = ask("workspacerules");
    assert!(readable.contains("Workspace rule 1:"), "{readable}");
    assert!(readable.contains("\tmonitor: DP-1\n"), "{readable}");
    assert!(readable.contains("\tpersistent: <unset>\n"), "{readable}");
    assert!(readable.contains("\tborder: false\n"), "{readable}");
}

/// `eval` and `repl` are answered with the sentence Hyprland answers them
/// with when it was not started from a Lua configuration.
///
/// The two are the last of `hyprctl`'s commands. They run a line in the
/// configuration's own Lua interpreter, and Hyprland has one only when the
/// configuration was written in Lua; against the file format this
/// compositor reads, Hyprland says this and nothing else. A script that
/// asks is entitled to the answer it would get from Hyprland, which is not
/// the same as being told the command does not exist.
#[test]
fn eval_and_repl_say_what_hyprland_says_without_lua() {
    for line in ["eval print(1)", "repl print(1)", "eval", "repl"] {
        assert_eq!(
            text(line),
            "eval is only supported with the lua config manager\n",
            "{line}"
        );
    }
}

// ---------------------------------------------------------------------------
// `switchxkblayout`
//
// `switchXKBLayoutRequest` (`src/debug/HyprCtl.cpp:1359`). Every answer here
// is Hyprland's own string, character for character, because a script reads
// them: `hyprctl switchxkblayout … | grep -q ok` is how people check whether
// a switch worked, and a reworded answer is a script that stops working.
// ---------------------------------------------------------------------------

/// One keyboard with `groups` layouts, sitting in group `active`.
fn keyboard(address: u64, name: &str, groups: u32, active: Option<u32>) -> Keyboard {
    Keyboard {
        device: Device {
            address,
            name: name.to_owned(),
        },
        layout: "de,us,fr".to_owned(),
        active_layout_index: active,
        active_keymap: match active {
            Some(0) => "German".to_owned(),
            Some(1) => "English (US)".to_owned(),
            Some(2) => "French".to_owned(),
            _ => "none".to_owned(),
        },
        groups,
        main: address == 0,
        ..Keyboard::default()
    }
}

/// Answer `switchxkblayout <argument>` against `keyboards` and nothing else.
fn switch(argument: &str, keyboards: &[Keyboard]) -> Reply {
    let request = Request::parse(&format!("switchxkblayout {argument}"));
    let snapshot = Snapshot {
        devices: Devices {
            keyboards: keyboards.to_vec(),
            ..Devices::default()
        },
        ..Snapshot::default()
    };
    answer(&request, &snapshot, Version::default())
}

/// What the answer says, for a reply that carries one.
fn says(argument: &str, keyboards: &[Keyboard]) -> String {
    switch(argument, keyboards)
        .said()
        .unwrap_or_else(|| panic!("{argument:?} said nothing"))
        .to_owned()
}

/// Which keyboards the answer moves, and to which group.
fn moves(argument: &str, keyboards: &[Keyboard]) -> Vec<(u64, u32)> {
    match switch(argument, keyboards) {
        Reply::SwitchLayout { groups, .. } => groups,
        _ => Vec::new(),
    }
}

/// `main`, `active` and `current` all mean the keyboard that last typed.
///
/// Three words for one thing in Hyprland (`HyprCtl.cpp:1394`), and a person
/// who learned one of them from a wiki page should not find that the other
/// two do nothing.
#[test]
fn main_active_and_current_all_name_the_seats_keyboard() {
    let keyboards = [
        keyboard(1, "other-keyboard", 3, Some(0)),
        keyboard(0, "qemu-virtio-keyboard", 3, Some(0)),
    ];
    for selector in ["main", "active", "current"] {
        let line = format!("{selector} next");
        assert_eq!(says(&line, &keyboards), "ok\n", "{selector}");
        // Address 0 is the one with `main` set, and it is second in the
        // list: the answer is the seat's keyboard and not the first one.
        assert_eq!(moves(&line, &keyboards), [(0, 1)], "{selector}");
    }
}

/// A seat with keyboards but none active is `no device`, which is a
/// different sentence from a name that matched nothing.
#[test]
fn a_seat_with_no_active_keyboard_says_no_device() {
    let keyboards = [Keyboard {
        main: false,
        ..keyboard(3, "some-keyboard", 2, Some(0))
    }];
    assert_eq!(says("main next", &keyboards), "no device\n");
    assert!(moves("main next", &keyboards).is_empty());
}

/// A device is named by its normalised name, whatever case and spacing the
/// person wrote.
#[test]
fn a_device_is_named_by_its_normalised_name() {
    let keyboards = [keyboard(0, "qemu-virtio-keyboard", 3, Some(0))];
    // The name as `hyprctl devices` prints it, and the name as the kernel
    // said it with the spaces that `deviceNameToInternalString` replaces --
    // except that a space splits the arguments, so what a person can write
    // is the dashed form in any case they like.
    for written in ["qemu-virtio-keyboard", "QEMU-Virtio-Keyboard"] {
        assert_eq!(
            says(&format!("{written} 2"), &keyboards),
            "ok\n",
            "{written}"
        );
        assert_eq!(moves(&format!("{written} 2"), &keyboards), [(0, 2)]);
    }
}

/// A name nothing has is `device not found`.
#[test]
fn a_name_no_keyboard_has_is_not_found() {
    let keyboards = [keyboard(0, "qemu-virtio-keyboard", 3, Some(0))];
    assert_eq!(
        says("no-such-keyboard next", &keyboards),
        "device not found\n"
    );
    assert!(moves("no-such-keyboard next", &keyboards).is_empty());
}

/// An empty device is a device, and it is not `all`.
///
/// `CVarList` keeps its empty fields, so `switchxkblayout  next` written
/// with two spaces asks for a keyboard called nothing and is answered
/// `device not found` -- not "every keyboard", which is the answer a reader
/// might expect from a missing argument and which would switch the layout
/// of a machine whose script had a typo in it.
#[test]
fn an_empty_device_is_not_every_device() {
    let keyboards = [keyboard(0, "qemu-virtio-keyboard", 3, Some(0))];
    for argument in ["", " next", "next"] {
        assert_eq!(
            says(argument, &keyboards),
            "device not found\n",
            "{argument:?}"
        );
        assert!(moves(argument, &keyboards).is_empty(), "{argument:?}");
    }
}

/// `all` moves every keyboard and answers `ok`.
#[test]
fn all_moves_every_keyboard() {
    let keyboards = [
        keyboard(0, "one-keyboard", 3, Some(0)),
        keyboard(1, "two-keyboard", 3, Some(2)),
    ];
    assert_eq!(says("all next", &keyboards), "ok\n");
    assert_eq!(moves("all next", &keyboards), [(0, 1), (1, 0)]);
    // A seat with no keyboard at all: nothing failed, so `ok`.
    assert_eq!(says("all next", &[]), "ok\n");
}

/// `all` where one keyboard cannot take the command moves the rest and
/// answers with the reasons, a line each.
///
/// Hyprland concatenates them and answers `ok` only when every keyboard
/// took it (`HyprCtl.cpp:1405`). The keyboards that could be moved are
/// moved either way, which is the behaviour worth keeping: a second
/// keyboard with a one-layout keymap should not stop the first from
/// switching.
#[test]
fn all_answers_with_the_keyboards_that_failed_and_moves_the_rest() {
    let keyboards = [
        keyboard(0, "three-layouts", 3, Some(0)),
        keyboard(1, "one-layout", 1, Some(0)),
    ];
    assert_eq!(
        says("all 2", &keyboards),
        "layout idx out of range of 1\n",
        "the one-layout keyboard is the only failure"
    );
    assert_eq!(moves("all 2", &keyboards), [(0, 2)]);

    // Two failures are two lines, and then nothing is moved at all.
    let both = [
        keyboard(0, "one-layout", 1, Some(0)),
        keyboard(1, "another-one-layout", 1, Some(0)),
    ];
    assert_eq!(
        says("all 2", &both),
        "layout idx out of range of 1\nlayout idx out of range of 1\n"
    );
    assert!(moves("all 2", &both).is_empty());
}

/// `next` runs off the last group and back to the first.
#[test]
fn next_wraps_off_the_last_group() {
    let last = [keyboard(0, "three-layouts", 3, Some(2))];
    assert_eq!(moves("main next", &last), [(0, 0)]);
    let middle = [keyboard(0, "three-layouts", 3, Some(1))];
    assert_eq!(moves("main next", &middle), [(0, 2)]);
    // One layout is a keyboard `next` cannot move, and Hyprland does not
    // range-check `next`: it answers `ok` and the group stays where it is.
    let one = [keyboard(0, "one-layout", 1, Some(0))];
    assert_eq!(says("main next", &one), "ok\n");
    assert_eq!(moves("main next", &one), [(0, 0)]);
}

/// `prev` runs off the first group and round to the last.
#[test]
fn prev_wraps_off_the_first_group() {
    let first = [keyboard(0, "three-layouts", 3, Some(0))];
    assert_eq!(moves("main prev", &first), [(0, 2)]);
    let middle = [keyboard(0, "three-layouts", 3, Some(1))];
    assert_eq!(moves("main prev", &middle), [(0, 0)]);
    assert_eq!(says("main prev", &first), "ok\n");
}

/// A keyboard with no active group at all is moved as Hyprland moves it.
///
/// Its scan for the active group runs off the end and leaves the count,
/// which its wrapping then reads as the first group, so `next` lands on the
/// second and `prev` on the last. Reachable only for a keyboard whose state
/// has no group; the arithmetic is written down because it is the one place
/// where Hyprland's answer is not the obvious one.
#[test]
fn a_keyboard_with_no_active_group_is_moved_from_the_first() {
    let none = [keyboard(0, "three-layouts", 3, None)];
    assert_eq!(moves("main next", &none), [(0, 1)]);
    assert_eq!(moves("main prev", &none), [(0, 2)]);
}

/// A numeric argument counts from zero and is range-checked.
#[test]
fn a_numeric_argument_is_an_index_from_zero() {
    let keyboards = [keyboard(0, "three-layouts", 3, Some(0))];
    for (written, group) in [("0", 0), ("1", 1), ("2", 2)] {
        assert_eq!(says(&format!("main {written}"), &keyboards), "ok\n");
        assert_eq!(moves(&format!("main {written}"), &keyboards), [(0, group)]);
    }
}

/// An index past the end names the count, not the last index.
///
/// `layout idx out of range of 3` for a keyboard with three layouts, whose
/// last index is two: the sentence is Hyprland's and tells a person how
/// many there are rather than what the largest index is.
#[test]
fn an_index_out_of_range_names_how_many_layouts_there_are() {
    let keyboards = [keyboard(0, "three-layouts", 3, Some(0))];
    for written in ["3", "4", "-1", "2147483647"] {
        assert_eq!(
            says(&format!("main {written}"), &keyboards),
            "layout idx out of range of 3\n",
            "{written}"
        );
        assert!(
            moves(&format!("main {written}"), &keyboards).is_empty(),
            "{written}"
        );
    }
}

/// Anything that is not `next`, `prev` or a number is `invalid arg 2`.
///
/// Including nothing at all, which is what `switchxkblayout main` on its
/// own leaves: Hyprland's `CVarList` answers an absent field with an empty
/// string and `std::stoi` throws on it.
#[test]
fn an_argument_that_is_no_command_and_no_number_is_invalid() {
    let keyboards = [keyboard(0, "three-layouts", 3, Some(0))];
    for written in ["", "forward", "NEXT", "x2", "99999999999999999999"] {
        let line = format!("main {written}");
        assert_eq!(says(&line, &keyboards), "invalid arg 2\n", "{written:?}");
        assert!(moves(&line, &keyboards).is_empty(), "{written:?}");
    }
    // `std::stoi` reads the digits it finds and stops, so a number with
    // something after it is that number. Written down because it is a
    // reading a person might rely on by accident and the answer must not
    // change under them.
    assert_eq!(moves("main 2x", &keyboards), [(0, 2)]);
}

/// The names a device is given, and what happens to two of the same.
#[test]
fn a_device_name_is_lowercased_and_its_spaces_commas_and_newlines_dashed() {
    assert_eq!(
        crate::device_name("QEMU Virtio Keyboard"),
        "qemu-virtio-keyboard"
    );
    // The three that are replaced are the three that would make the name
    // unusable: a space splits the arguments, a comma splits the
    // `activelayout` payload, and a newline ends an event line.
    assert_eq!(crate::device_name("a b,c\nd"), "a-b-c-d");
    // Everything else is left alone, lower-cased where ASCII says so.
    assert_eq!(
        crate::device_name("Logitech_K280e-2.4"),
        "logitech_k280e-2.4"
    );

    // Two of the same model get told apart, as `getNameForNewDevice` tells
    // them apart, or `switchxkblayout <device>` could name only the first.
    let mut taken: Vec<String> = Vec::new();
    for wanted in ["keychron-k2", "keychron-k2-1", "keychron-k2-2"] {
        let given = crate::new_device_name("Keychron K2", &taken);
        assert_eq!(given, wanted);
        taken.push(given);
    }
    // A device that gave no name at all still gets one.
    assert_eq!(crate::new_device_name("", &[]), "unknown-device");
    assert_eq!(
        crate::new_device_name("", &["unknown-device".to_owned()]),
        "unknown-device-1"
    );
}

/// `hyprctl devices` carries every keyboard field Hyprland's does, under
/// its own name and in its own order.
#[test]
fn devices_carries_hyprlands_own_keyboard_fields() {
    let devices = json("j/devices");
    let keyboard = &devices.get("keyboards").expect("keyboards").items()[0];
    assert_eq!(
        keyboard.keys(),
        [
            "address",
            "name",
            "rules",
            "model",
            "layout",
            "variant",
            "options",
            "active_layout_index",
            "active_keymap",
            "capsLock",
            "numLock",
            "main",
        ]
    );
    assert_eq!(
        keyboard
            .get("active_layout_index")
            .and_then(parse::Value::number),
        Some(0.0)
    );
    assert_eq!(
        keyboard.get("active_keymap").and_then(parse::Value::text),
        Some("English (US)")
    );
    let readable = text("devices");
    assert!(readable.contains("active layout index: 0"), "{readable}");
    assert!(
        readable.contains("active keymap: English (US)"),
        "{readable}"
    );
}

/// A keyboard with no active group is still an answer a bar can parse.
///
/// Hyprland writes the word `none` into the `active_layout_index` field
/// without quoting it (`HyprCtl.cpp:793`), which makes the whole document
/// invalid JSON: a bar does not lose the layout field, it loses every
/// field. Quoted here, so the one case Hyprland breaks is the one case a
/// bar reads a string.
#[test]
fn a_keyboard_with_no_active_group_is_still_json() {
    let request = Request::parse("j/devices");
    let snapshot = Snapshot {
        devices: Devices {
            keyboards: vec![keyboard(0, "three-layouts", 3, None)],
            ..Devices::default()
        },
        ..Snapshot::default()
    };
    let written = match answer(&request, &snapshot, Version::default()) {
        Reply::Text(written) => written,
        other => panic!("devices asked for {other:?}"),
    };
    let parsed = parse::parse(&written).unwrap_or_else(|error| panic!("{error}\n{written}"));
    let listed = &parsed.get("keyboards").expect("keyboards").items()[0];
    assert_eq!(
        listed
            .get("active_layout_index")
            .and_then(parse::Value::text),
        Some("none")
    );
    assert_eq!(
        listed.get("active_keymap").and_then(parse::Value::text),
        Some("none")
    );

    let request = Request::parse("devices");
    let readable = match answer(&request, &snapshot, Version::default()) {
        Reply::Text(written) => written,
        other => panic!("devices asked for {other:?}"),
    };
    assert!(readable.contains("active layout index: none"), "{readable}");
}

/// The event a switch puts on the socket: `activelayout`, the keyboard's
/// name, a comma, the layout's name.
#[test]
fn a_layout_switch_is_one_activelayout_line() {
    assert_eq!(
        lines(&Event::ActiveLayout {
            keyboard: "qemu-virtio-keyboard".to_owned(),
            layout: "German".to_owned(),
        }),
        ["activelayout>>qemu-virtio-keyboard,German\n"]
    );
}

/// The watcher says `activelayout` when a keyboard's group changed, and
/// when a keyboard arrived.
///
/// Hyprland posts it from `onKeyboardMod` when the group differs from the
/// last one sent (`InputManager.cpp:1731`) and from the keymap listener
/// while a keyboard is being set up (`:1189`). Both are a difference
/// between two snapshots here, which is how every other event on this
/// socket is found.
#[test]
fn the_watcher_says_activelayout_for_a_group_that_moved() {
    let with = |keyboards: Vec<Keyboard>| Snapshot {
        devices: Devices {
            keyboards,
            ..Devices::default()
        },
        ..Snapshot::default()
    };
    let mut watcher = Watcher::new();

    // A keyboard that has just appeared is a layout a bar has not been told
    // about, so it is told.
    let events = watcher.changed(&with(vec![keyboard(0, "one-keyboard", 3, Some(0))]));
    assert_eq!(
        events,
        [Event::ActiveLayout {
            keyboard: "one-keyboard".to_owned(),
            layout: "German".to_owned(),
        }]
    );

    // Nothing moved, nothing said.
    assert!(
        watcher
            .changed(&with(vec![keyboard(0, "one-keyboard", 3, Some(0))]))
            .is_empty()
    );

    // The group moved: one line, naming the keyboard and the layout it is
    // in now.
    let events = watcher.changed(&with(vec![keyboard(0, "one-keyboard", 3, Some(1))]));
    assert_eq!(
        events,
        [Event::ActiveLayout {
            keyboard: "one-keyboard".to_owned(),
            layout: "English (US)".to_owned(),
        }]
    );

    // A second keyboard appearing is one line for it and none for the one
    // that was already there.
    let events = watcher.changed(&with(vec![
        keyboard(0, "one-keyboard", 3, Some(1)),
        keyboard(1, "two-keyboard", 3, Some(2)),
    ]));
    assert_eq!(
        events,
        [Event::ActiveLayout {
            keyboard: "two-keyboard".to_owned(),
            layout: "French".to_owned(),
        }]
    );

    // A reload that rebuilt the keymap under a keyboard leaves it in the
    // same group with another layout in it, which is a switch to a reader.
    let renamed = Keyboard {
        active_keymap: "Russian".to_owned(),
        ..keyboard(0, "one-keyboard", 3, Some(1))
    };
    let events = watcher.changed(&with(vec![
        renamed,
        keyboard(1, "two-keyboard", 3, Some(2)),
    ]));
    assert_eq!(
        events,
        [Event::ActiveLayout {
            keyboard: "one-keyboard".to_owned(),
            layout: "Russian".to_owned(),
        }]
    );
}

/// Two windows mapped in one pass each took the focus, one after the other.
/// The state after says only the second has it; the trail says both went
/// by, and each is said, as Hyprland posts `activewindow` at each.
#[test]
fn every_window_the_focus_went_to_in_one_pass_is_said() {
    let mut watcher = Watcher::new();
    let _ = watcher.changed(&watched(&[], None));

    let both = watched(
        &[watched_window(1, "one"), watched_window(2, "two")],
        Some(2),
    );
    let focused: Vec<Option<u64>> = watcher
        .changed_through(&both, &[Some(1), Some(2)])
        .into_iter()
        .filter_map(|event| match event {
            Event::ActiveWindow(window) => Some(window.map(|window| window.address)),
            _ => None,
        })
        .collect();
    assert_eq!(focused, [Some(1), Some(2)]);

    // Where the focus went and came back from in one pass is still two
    // moves; a window that has gone again is not said at all, and a trail
    // that stayed where it began says nothing.
    let events = watcher.changed_through(&both, &[Some(1), Some(3), Some(2)]);
    let focused: Vec<Option<u64>> = events
        .into_iter()
        .filter_map(|event| match event {
            Event::ActiveWindow(window) => Some(window.map(|window| window.address)),
            _ => None,
        })
        .collect();
    assert_eq!(focused, [Some(1), Some(2)]);
    assert!(watcher.changed_through(&both, &[Some(2)]).is_empty());
}
