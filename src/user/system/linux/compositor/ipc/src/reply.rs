//! Answering a request.

use core::fmt::Write;

use crate::json::Json;
use crate::request::{Flags, Format, Request};
use crate::state::{
    Bind, Devices, Keyboard, Layer, Monitor, Plugin, Snapshot, Window, Workspace, device_name,
};

/// What the compositor calls itself in `hyprctl version`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Version {
    /// The compositor's name.
    pub name: &'static str,
    /// Its version.
    pub version: &'static str,
    /// The commit it was built from, or an empty string.
    pub commit: &'static str,
}

impl Default for Version {
    fn default() -> Self {
        Self {
            name: "hyprix",
            version: env!("CARGO_PKG_VERSION"),
            commit: "",
        }
    }
}

/// What a request asked the compositor to do that this crate cannot do
/// itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reply {
    /// The answer, written.
    Text(String),
    /// `dispatch <name> <argument>`: the compositor runs it and says what
    /// happened.
    Dispatch {
        /// The dispatcher's name.
        name: String,
        /// Its argument.
        argument: String,
    },
    /// `keyword <name> <value>`: the configuration is changed while it runs.
    Keyword {
        /// The option's name, `category:key`.
        name: String,
        /// Its new value, as written.
        value: String,
    },
    /// `reload`: read the configuration file again.
    Reload,
    /// `notify`, `dismissnotify` and `seterror`: a message for the person
    /// at the screen. The compositor says it and puts it on the event
    /// socket, where a notification daemon or a bar picks it up.
    Notify {
        /// What to say, empty for "take the last one away".
        message: String,
        /// Whether it is an error rather than a notification, which is
        /// what `seterror` sends.
        error: bool,
    },
    /// `switchxkblayout <device> <cmd>`: which layout group one or more
    /// keyboards are in.
    ///
    /// The group is worked out here, from the snapshot, rather than by the
    /// compositor: which keyboards a selector names, what `next` wraps to
    /// and whether an index is in range are all answers this crate already
    /// has the state for, and a second copy of the arithmetic in the
    /// compositor is a second copy to get wrong. What is left for the
    /// compositor is the one thing it alone can do -- put the keyboard in
    /// the group and tell the clients.
    SwitchLayout {
        /// Each keyboard to move, by the address `hyprctl devices` prints,
        /// and the group to put it in.
        groups: Vec<(u64, u32)>,
        /// What to answer, which is `ok` unless some of the keyboards
        /// `all` named could not be moved and some could.
        answer: String,
    },
}

impl Reply {
    /// What to write back, for a reply that carries its own answer.
    ///
    /// `serve` answers `ok` for every reply the compositor has to act on,
    /// because Hyprland does. `switchxkblayout all` is the one request that
    /// both acts and has something else to say: Hyprland moves the
    /// keyboards it can and answers with the reasons the others could not
    /// be moved (`src/debug/HyprCtl.cpp:1405`), so that reply carries the
    /// text and this hands it over.
    #[must_use]
    pub fn said(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::SwitchLayout { answer, .. } => Some(answer),
            _ => None,
        }
    }
}

/// Answer `request` from `snapshot`.
///
/// The commands that only read state are answered here; the ones that change
/// it come back for the compositor to run, because this crate holds no
/// compositor.
#[must_use]
pub fn answer(request: &Request, snapshot: &Snapshot, version: Version) -> Reply {
    let flags = request.flags;
    let text = |value: String| Reply::Text(value);
    match request.command.as_str() {
        "version" => text(self_version(flags, version)),
        "monitors" => text(monitors(flags, &snapshot.monitors)),
        "workspaces" => text(workspaces(flags, snapshot)),
        "clients" => text(clients(flags, snapshot)),
        "activewindow" => text(active_window(flags, snapshot)),
        "activeworkspace" => text(active_workspace(flags, snapshot)),
        // `submapRequest`: the name, or `default` for the global map, and in
        // JSON a bare string rather than an object.
        "submap" => {
            let name = if snapshot.submap.is_empty() {
                "default"
            } else {
                &snapshot.submap
            };
            text(if flags.format() == Format::Json {
                format!("\"{name}\"\n")
            } else {
                format!("{name}\n")
            })
        }
        "splash" => text("a compositor, on an operating system, in Rust\n".to_owned()),
        "binds" => text(binds(flags, &snapshot.binds)),
        "devices" => text(devices(flags, &snapshot.devices)),
        "layers" => text(layers(flags, snapshot)),
        "cursorpos" => text(cursor_position(flags, snapshot.cursor)),
        "locked" => text(if flags.format() == Format::Json {
            let mut out = Json::new(pretty(flags));
            out.object();
            out.boolean("locked", snapshot.locked);
            out.end('}');
            finish(out, flags)
        } else {
            format!("{}\n", snapshot.locked)
        }),
        "workspacerules" => text(workspace_rules(flags, &snapshot.workspace_rules)),
        "globalshortcuts" => text(global_shortcuts(flags, &snapshot.shortcuts)),
        // `eval` and `repl` run a line of Lua in the configuration's own
        // interpreter, which Hyprland has only when it was started with a
        // Lua configuration; against the file format everything else here
        // reads, Hyprland answers this sentence and so does this. The
        // commands are answered rather than refused because a script that
        // asks is entitled to the same answer it would get from Hyprland.
        "eval" | "repl" => text("eval is only supported with the lua config manager\n".to_owned()),
        "getoption" => text(option_named(
            flags,
            request.argument.trim(),
            &snapshot.options,
        )),
        "descriptions" => text(descriptions(flags, &snapshot.options)),
        "animations" => text(animations(flags, snapshot)),
        "configerrors" => text(lines(flags, &snapshot.errors)),
        "rollinglog" => text(rolling_log(flags, &snapshot.log)),
        "systeminfo" | "status" => text(system_info(flags, &snapshot.system)),
        "dispatch" => {
            let (name, argument) = split(&request.argument);
            Reply::Dispatch { name, argument }
        }
        "keyword" => {
            let (name, value) = split(&request.argument);
            Reply::Keyword { name, value }
        }
        "reload" => Reply::Reload,
        // `notify <icon> <time> <colour> <message...>`: the first three
        // are how Hyprland's own overlay draws it, and this compositor
        // draws no overlay, so what is kept is the message.
        "notify" => Reply::Notify {
            message: request
                .argument
                .splitn(4, ' ')
                .nth(3)
                .unwrap_or_default()
                .to_owned(),
            error: false,
        },
        "dismissnotify" => Reply::Notify {
            message: String::new(),
            error: false,
        },
        // `seterror <colour> <message...>`, or `seterror disable`.
        "seterror" => {
            let rest = request.argument.trim();
            let message = match rest.split_once(' ') {
                Some((_, message)) if !rest.starts_with("dis") => message.trim().to_owned(),
                _ => String::new(),
            };
            Reply::Notify {
                message,
                error: true,
            }
        }
        "decorations" => text(decorations(flags, &request.argument, snapshot)),
        "getprop" => text(window_property(flags, &request.argument, snapshot)),
        "switchxkblayout" => switch_xkb_layout(&request.argument, &snapshot.devices.keyboards),
        // Each of these acts on something this compositor does not have,
        // and says so rather than pretending. `output` creates and removes
        // headless screens, which this compositor's screens are not;
        // `setcursor` names a cursor theme, and the cursor here is drawn in
        // code; `kill` is Hyprland's click-to-kill mode, which needs a
        // pointer grab.
        "output" => text("the screens are the card's, not this compositor's to make\n".to_owned()),
        "setcursor" => {
            text("the cursor is drawn in code; there is no theme to choose\n".to_owned())
        }
        "kill" => {
            text("click-to-kill is not implemented; `closewindow` names a window\n".to_owned())
        }
        // `hyprctl plugin list`, and nothing else: Hyprland's `load` and
        // `unload` take a shared object, and this compositor's plugins are
        // programs it starts.
        "plugin" => text(match request.argument.trim() {
            "list" => plugins(flags, &snapshot.plugins),
            _ => "unknown opt\n".to_owned(),
        }),
        // Hyprland answers an unknown command with a line rather than by
        // closing the connection, so a program asking for something newer
        // keeps working for everything else it asks.
        other => text(format!("unknown request {other}\n")),
    }
}

/// `hyprctl switchxkblayout <device> <cmd>`: which layout a keyboard types.
///
/// `switchXKBLayoutRequest` (`src/debug/HyprCtl.cpp:1359`). A request, not a
/// dispatcher: a keybind reaches it through `exec, hyprctl switchxkblayout`,
/// and Hyprland's dispatcher table has no entry for it, so neither does this
/// compositor's.
///
/// What it changes is only the group. No new keymap goes to the clients --
/// the keymap holds every layout the configuration named and the group says
/// which of them is in force -- so what a client is told is one
/// `wl_keyboard.modifiers` with the new group in it, which is how a switch
/// costs nothing and loses no key.
///
/// The arguments are split on spaces and nothing else, and an empty device
/// is a device, not `all`: `switchxkblayout  next`, written with two spaces,
/// names no keyboard and is answered `device not found`. That is Hyprland's
/// own reading, since `CVarList` keeps its empty fields, and this reaches
/// the same answer from a request whose argument arrived trimmed -- with one
/// difference worth writing down: a *leading* run of spaces is gone by the
/// time the argument gets here, so `switchxkblayout  all next` is read as
/// `all` where Hyprland reads an empty device. Every other spacing agrees,
/// including the trailing and doubled spaces a script actually produces.
fn switch_xkb_layout(argument: &str, keyboards: &[Keyboard]) -> Reply {
    let mut words = argument.split(' ');
    let selector = words.next().unwrap_or_default().trim();
    let command = words.next().unwrap_or_default().trim();

    // One keyboard: the answer is the reason it could not be moved, or `ok`.
    let one = |keyboard: &Keyboard| match group_wanted(keyboard, command) {
        Ok(group) => Reply::SwitchLayout {
            groups: vec![(keyboard.device.address, group)],
            answer: "ok\n".to_owned(),
        },
        Err(why) => Reply::Text(format!("{why}\n")),
    };

    match selector {
        // The seat's keyboard: the one that last produced an event, which is
        // Hyprland's `m_active` and the `main` field of `hyprctl devices`.
        // A seat that has no keyboard yet is `no device`, which is a
        // different answer from a name nothing matches.
        "main" | "active" | "current" => keyboards
            .iter()
            .find(|keyboard| keyboard.main)
            .map_or_else(|| Reply::Text("no device\n".to_owned()), one),
        // Every keyboard: the ones that can be moved are, and the answer is
        // the reasons the others could not, a line each. `ok` when none
        // failed -- including when there is no keyboard at all, since then
        // nothing failed.
        "all" => {
            let mut groups = Vec::new();
            let mut answer = String::new();
            for keyboard in keyboards {
                match group_wanted(keyboard, command) {
                    Ok(group) => groups.push((keyboard.device.address, group)),
                    Err(why) => {
                        let _ = writeln!(answer, "{why}");
                    }
                }
            }
            if answer.is_empty() {
                answer.push_str("ok\n");
            }
            if groups.is_empty() {
                // Nothing to do: an answer and no work, so the compositor is
                // not woken for it.
                return Reply::Text(answer);
            }
            Reply::SwitchLayout { groups, answer }
        }
        // A name, matched against the normalised name every device has.
        name => keyboards
            .iter()
            .find(|keyboard| keyboard.device.name == device_name(name))
            .map_or_else(|| Reply::Text("device not found\n".to_owned()), one),
    }
}

/// Which group `command` asks a keyboard for, or why it asks for none.
///
/// `updateKeyboard` inside `switchXKBLayoutRequest`
/// (`src/debug/HyprCtl.cpp:1366`). `next` and `prev` are not range-checked
/// there: they are handed to `xkb_state_update_mask`, which wraps a group
/// into the keymap's range, so the wrapping is done here instead and the
/// compositor is handed a group it can use as it stands.
///
/// A numeric argument is range-checked and counts from zero. It is read the
/// way `std::stoi` reads it, because that is what Hyprland reads it with:
/// see [`stoi`].
fn group_wanted(keyboard: &Keyboard, command: &str) -> Result<u32, String> {
    // At least one, for the reason `Keyboard::groups` gives.
    let groups = keyboard.groups.max(1);
    // Hyprland's own scan for the active group runs off the end when none is
    // active and leaves the count itself, which its wrapping then turns into
    // the first group; taking the count modulo itself is the same zero.
    let active = keyboard.active_layout_index.unwrap_or(groups) % groups;
    match command {
        "next" => Ok((active + 1) % groups),
        "prev" => Ok(if active == 0 { groups - 1 } else { active - 1 }),
        _ => {
            let index = stoi(command).ok_or_else(|| "invalid arg 2".to_owned())?;
            match u32::try_from(index) {
                // Hyprland's own check is `> LAYOUTS - 1`, which for a count
                // of at least one is this.
                Ok(index) if index < groups => Ok(index),
                // A negative index and one past the end are the same answer,
                // which names the count rather than the last index: a person
                // reading it is being told how many layouts there are.
                _ => Err(format!("layout idx out of range of {groups}")),
            }
        }
    }
}

/// `std::stoi`'s reading of a string, which is what Hyprland reads
/// `switchxkblayout`'s numeric argument with.
///
/// A sign, then digits, then anything -- `2x` is two -- and no digits at all
/// is the failure that becomes `invalid arg 2`. A number too large for an
/// `int` is `std::out_of_range`, which the same `catch` takes
/// (`src/debug/HyprCtl.cpp:1384`), so it is the same answer and `None` here.
/// Leading whitespace, which `stoi` also skips, cannot reach this: the
/// argument was split on spaces and each field trimmed.
fn stoi(text: &str) -> Option<i32> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let value = digits.parse::<i64>().ok()?;
    i32::try_from(if negative { -value } else { value }).ok()
}

/// `hyprctl getprop <window> <property>`: one property of one window.
///
/// The value bare in the readable form and `{"<property>": <value>}` in
/// JSON, which is Hyprland's own shape and what a script reads. A property
/// nothing set is the compositor's own -- which is what a person asking
/// wants to know, since the question is "what is this window drawn with".
fn window_property(flags: Flags, argument: &str, snapshot: &Snapshot) -> String {
    let mut words = argument.split_whitespace();
    let (Some(which), Some(property)) = (words.next(), words.next()) else {
        return "not enough args\n".to_owned();
    };
    let Some(window) = snapshot
        .windows
        .iter()
        .find(|window| window.title == which || window.class == which)
    else {
        return "window not found\n".to_owned();
    };
    let style = &window.style;
    let (value, quoted) = match property {
        "alpha" | "alphaoverride" => (
            style
                .alpha
                .map_or_else(|| "1".to_owned(), |alpha| format!("{alpha}")),
            false,
        ),
        "rounding" => (
            style
                .rounding
                .map_or_else(|| "-1".to_owned(), |value| value.to_string()),
            false,
        ),
        "bordersize" => (
            style
                .border
                .map_or_else(|| "-1".to_owned(), |value| value.to_string()),
            false,
        ),
        "noblur" => (style.no_blur.to_string(), false),
        "noshadow" => (style.no_shadow.to_string(), false),
        "nodim" => (style.no_dim.to_string(), false),
        "floating" => (window.floating.to_string(), false),
        "fullscreen" => (window.fullscreen.to_string(), false),
        "pid" => (window.pid.to_string(), false),
        "class" => (window.class.clone(), true),
        "title" => (window.title.clone(), true),
        "workspace" => (window.workspace.to_string(), false),
        // Hyprland's own answer to a name it does not know.
        _ => return "prop not found\n".to_owned(),
    };
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        if quoted {
            out.string(property, &value);
        } else {
            out.bare(property, &value);
        }
        out.end('}');
        return finish(out, flags);
    }
    format!("{value}\n")
}

/// `hyprctl decorations <window>`: what is drawn around one window.
///
/// Hyprland lists a decoration a plugin may have added; this compositor
/// draws a border and nothing else, so that is what it lists -- and a
/// window it does not have is an empty list, as Hyprland's is.
fn decorations(flags: Flags, which: &str, snapshot: &Snapshot) -> String {
    let found = snapshot
        .windows
        .iter()
        .find(|window| window.title == which.trim() || window.class == which.trim());
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        if found.is_some() {
            out.object();
            out.string("decorationName", "border");
            out.number("priority", 0);
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    match found {
        Some(_) => "Decoration border:\n\tpriority: 0\n".to_owned(),
        None => String::new(),
    }
}

/// `hyprctl globalshortcuts`: every shortcut a program registered.
fn global_shortcuts(flags: Flags, shortcuts: &[crate::Shortcut]) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for shortcut in shortcuts {
            out.object();
            out.string("name", &shortcut.name);
            out.string("description", &shortcut.description);
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    if shortcuts.is_empty() {
        return "none\n".to_owned();
    }
    shortcuts
        .iter()
        .map(|shortcut| format!("{} -> {}\n", shortcut.name, shortcut.description))
        .collect()
}

/// `hyprctl workspacerules`: every `workspace =` line, read.
///
/// Hyprland prints a field only where the line set it, and the text form
/// prints `<unset>` for the rest. That is not decoration: `border:` unset
/// and `border:true` are different -- the first leaves `general:border_size`
/// alone and the second insists on it -- so a reader has to be able to tell
/// them apart.
fn workspace_rules(flags: Flags, rules: &[compositor_config::WorkspaceRule]) -> String {
    let yes_or_no = |value: bool| if value { "true" } else { "false" };
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for rule in rules {
            out.object();
            out.string("workspaceString", &rule.workspace);
            out.boolean("enabled", true);
            if let Some(monitor) = &rule.monitor {
                out.string("monitor", monitor);
            }
            if let Some(value) = rule.is_default {
                out.boolean("default", value);
            }
            if let Some(value) = rule.is_persistent {
                out.boolean("persistent", value);
            }
            for (name, gaps) in [("gapsIn", rule.gaps_in), ("gapsOut", rule.gaps_out)] {
                if let Some(gaps) = gaps {
                    out.bare(
                        name,
                        &format!(
                            "[{}, {}, {}, {}]",
                            gaps.top, gaps.right, gaps.bottom, gaps.left
                        ),
                    );
                }
            }
            if let Some(size) = rule.border_size {
                out.number("borderSize", size);
            }
            if let Some(value) = rule.no_border {
                out.boolean("border", !value);
            }
            if let Some(value) = rule.no_rounding {
                out.boolean("rounding", !value);
            }
            if let Some(value) = rule.decorate {
                out.boolean("decorate", value);
            }
            if let Some(value) = rule.no_shadow {
                out.boolean("shadow", !value);
            }
            if let Some(name) = &rule.default_name {
                out.string("defaultName", name);
            }
            if let Some(command) = &rule.on_created_empty {
                out.string("onCreatedEmpty", command);
            }
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    let unset = |value: Option<String>| value.unwrap_or_else(|| "<unset>".to_owned());
    rules
        .iter()
        .map(|rule| {
            let gaps = |gaps: Option<compositor_config::Gaps>| {
                unset(gaps.map(|gaps| {
                    format!("{} {} {} {}", gaps.top, gaps.right, gaps.bottom, gaps.left)
                }))
            };
            format!(
                "Workspace rule {}:\n\tenabled: true\n\tmonitor: {}\n\tdefault: {}\n\tpersistent: \
                 {}\n\tgapsIn: {}\n\tgapsOut: {}\n\tborderSize: {}\n\tborder: {}\n\trounding: \
                 {}\n\tdecorate: {}\n\tshadow: {}\n\tdefaultName: {}\n\tonCreatedEmpty: {}\n\n",
                rule.workspace,
                unset(rule.monitor.clone()),
                unset(rule.is_default.map(|value| yes_or_no(value).to_owned())),
                unset(rule.is_persistent.map(|value| yes_or_no(value).to_owned())),
                gaps(rule.gaps_in),
                gaps(rule.gaps_out),
                unset(rule.border_size.map(|size| size.to_string())),
                unset(rule.no_border.map(|value| yes_or_no(!value).to_owned())),
                unset(rule.no_rounding.map(|value| yes_or_no(!value).to_owned())),
                unset(rule.decorate.map(|value| yes_or_no(value).to_owned())),
                unset(rule.no_shadow.map(|value| yes_or_no(!value).to_owned())),
                unset(rule.default_name.clone()),
                unset(rule.on_created_empty.clone()),
            )
        })
        .collect()
}

/// `hyprctl getoption <name>`, in Hyprland's own shape: the value under the
/// key its *type* names, and whether the configuration set it.
fn option_named(flags: Flags, name: &str, options: &[crate::Opt]) -> String {
    let Some(option) = options.iter().find(|option| option.name == name) else {
        return "no such option\n".to_owned();
    };
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        out.string("option", &option.name);
        // Hyprland puts a number bare and a string quoted, under a key
        // named for the type; a script reads exactly that key.
        match option.kind {
            "str" | "custom" => out.string(option.kind, &option.value),
            _ => out.bare(option.kind, &option.value),
        }
        out.boolean("set", option.set);
        out.end('}');
        return finish(out, flags);
    }
    // Hyprland's readable form writes a complex value as `custom type:`
    // and its JSON key as `custom`; every other type is the same word in
    // both.
    let word = if option.kind == "custom" {
        "custom type"
    } else {
        option.kind
    };
    format!("{word}: {}\nset: {}\n", option.value, option.set)
}

/// `hyprctl descriptions`: every option the compositor has.
///
/// Hyprland prints a sentence about each, written beside its default in its
/// own configuration table. This compositor's table has no such sentences,
/// so what is printed is the name, the type and the value -- which is what
/// a script reading this command is after, and is the truth rather than a
/// row of empty strings.
fn descriptions(flags: Flags, options: &[crate::Opt]) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for option in options {
            out.object();
            out.string("value", &option.name);
            out.string("type", option.kind);
            out.string("data", &option.value);
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    options
        .iter()
        .map(|option| format!("{} ({}) = {}\n", option.name, option.kind, option.value))
        .collect()
}

/// `hyprctl animations`: the animations and the beziers, in that order,
/// which is how Hyprland prints them.
fn animations(flags: Flags, snapshot: &Snapshot) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        out.array();
        for animation in &snapshot.animations {
            out.object();
            out.string("name", &animation.name);
            out.boolean("overridden", animation.overridden);
            out.string("bezier", &animation.bezier);
            out.boolean("enabled", animation.enabled);
            out.bare("speed", &format!("{:.2}", animation.speed));
            out.string("style", &animation.style);
            out.end('}');
        }
        out.end(']');
        out.array();
        for bezier in &snapshot.beziers {
            out.object();
            out.string("name", &bezier.name);
            out.bare("X0", &format!("{:.2}", bezier.first.0));
            out.bare("Y0", &format!("{:.2}", bezier.first.1));
            out.bare("X1", &format!("{:.2}", bezier.second.0));
            out.bare("Y1", &format!("{:.2}", bezier.second.1));
            out.end('}');
        }
        out.end(']');
        out.end(']');
        return finish(out, flags);
    }
    let mut result = String::from("animations:\n");
    for animation in &snapshot.animations {
        result.push_str(&format!(
            "\n\tname: {}\n\t\toverridden: {}\n\t\tbezier: {}\n\t\tenabled: {}\n\t\tspeed: {:.2}\n\t\tstyle: {}\n",
            animation.name,
            u8::from(animation.overridden),
            animation.bezier,
            animation.enabled,
            animation.speed,
            animation.style
        ));
    }
    result.push_str("beziers:\n");
    for bezier in &snapshot.beziers {
        result.push_str(&format!(
            "\n\tname: {}\n\t\tX0: {:.2}\n\t\tY0: {:.2}\n\t\tX1: {:.2}\n\t\tY1: {:.2}",
            bezier.name, bezier.first.0, bezier.first.1, bezier.second.0, bezier.second.1
        ));
    }
    result
}

/// `hyprctl configerrors`: a line each, or an array of them.
fn lines(flags: Flags, said: &[String]) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for line in said {
            out.item(line);
        }
        out.end(']');
        return finish(out, flags);
    }
    said.iter().map(|line| format!("{line}\n")).collect()
}

/// `hyprctl rollinglog`: the last lines the compositor said, as one string.
fn rolling_log(flags: Flags, log: &[String]) -> String {
    let joined: String = log.iter().map(|line| format!("{line}\n")).collect();
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        out.string("log", &joined);
        out.end('}');
        return finish(out, flags);
    }
    joined
}

/// `hyprctl systeminfo` and `hyprctl status`.
fn system_info(flags: Flags, system: &crate::System) -> String {
    let (monitors, windows, clients) = system.counts;
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        out.string("os", &system.os);
        out.string("kernel", &system.kernel);
        out.number("monitors", i64::try_from(monitors).unwrap_or(0));
        out.number("windows", i64::try_from(windows).unwrap_or(0));
        out.number("clients", i64::try_from(clients).unwrap_or(0));
        out.number("uptime", i64::try_from(system.uptime).unwrap_or(0));
        out.end('}');
        return finish(out, flags);
    }
    format!(
        "os: {}\nkernel: {}\nmonitors: {monitors}\nwindows: {windows}\nclients: \
         {clients}\nuptime: {} s\n",
        system.os, system.kernel, system.uptime
    )
}

/// `hyprctl binds`, in Hyprland's own shape.
///
/// The letters after `bind` are the flags the line was written with, in
/// `bindsRequest`'s own order, and the fields under them are its.
fn binds(flags: Flags, binds: &[Bind]) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for bind in binds {
            out.object();
            out.boolean("locked", bind.locked);
            out.boolean("mouse", bind.mouse);
            out.boolean("release", bind.release);
            out.boolean("repeat", bind.repeat);
            out.boolean("longPress", bind.long_press);
            out.boolean("non_consuming", bind.non_consuming);
            out.boolean("has_description", bind.has_description);
            out.number("modmask", i64::from(bind.modmask));
            out.string("submap", &bind.submap);
            out.boolean("submap_universal", bind.submap_universal);
            out.string("key", &bind.key);
            out.number("keycode", i64::from(bind.keycode));
            out.boolean("catch_all", bind.catch_all);
            out.string("description", &bind.description);
            out.string("dispatcher", &bind.dispatcher);
            out.string("arg", &bind.arg);
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    let mut text = String::new();
    for bind in binds {
        let mut letters = String::from("bind");
        for (on, letter) in [
            (bind.locked, 'l'),
            (bind.mouse, 'm'),
            (bind.release, 'r'),
            (bind.repeat, 'e'),
            (bind.non_consuming, 'n'),
            (bind.has_description, 'd'),
        ] {
            if on {
                letters.push(letter);
            }
        }
        let _ = writeln!(
            text,
            "{letters}\n\tmodmask: {}\n\tsubmap: {}\n\tkey: {}\n\tkeycode: {}\n\tcatchall: \
             {}\n\tdescription: {}\n\tdispatcher: {}\n\targ: {}\n",
            bind.modmask,
            bind.submap,
            bind.key,
            bind.keycode,
            bind.catch_all,
            bind.description,
            bind.dispatcher,
            bind.arg,
        );
    }
    text
}

/// `hyprctl devices`, in Hyprland's own shape.
///
/// The five groups it prints, in its order. What this compositor has no
/// notion of -- a pointer's acceleration, a tablet's physical size -- is
/// left out of the readable form and given its zero in the JSON, because a
/// bar reading a field that is always missing is worse than one reading a
/// field that is always zero.
fn devices(flags: Flags, devices: &Devices) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        out.array_field("mice");
        for mouse in &devices.mice {
            out.object();
            out.string("address", &format!("0x{:x}", mouse.address));
            out.string("name", &mouse.name);
            out.end('}');
        }
        out.end(']');
        out.array_field("keyboards");
        for keyboard in &devices.keyboards {
            out.object();
            out.string("address", &format!("0x{:x}", keyboard.device.address));
            out.string("name", &keyboard.device.name);
            out.string("rules", &keyboard.rules);
            out.string("model", &keyboard.model);
            out.string("layout", &keyboard.layout);
            out.string("variant", &keyboard.variant);
            out.string("options", &keyboard.options);
            match keyboard.active_layout_index {
                Some(index) => out.number("active_layout_index", i64::from(index)),
                // Hyprland writes the word `none` here for a keyboard with
                // no active group, and writes it *bare*
                // (`src/debug/HyprCtl.cpp:793`): the answer is then not
                // JSON at all and a bar's parser throws on the whole
                // document. Quoted here. A bar that reads a number still
                // reads one whenever there is one, and the only case where
                // it reads a string is the case where Hyprland gives it
                // nothing it can read.
                None => out.string("active_layout_index", "none"),
            }
            out.string("active_keymap", &keyboard.active_keymap);
            out.boolean("capsLock", keyboard.caps_lock);
            out.boolean("numLock", keyboard.num_lock);
            out.boolean("main", keyboard.main);
            out.end('}');
        }
        out.end(']');
        for (name, group) in [
            ("tablets", &devices.tablets),
            ("touch", &devices.touch),
            ("switches", &devices.switches),
        ] {
            out.array_field(name);
            for device in group {
                out.object();
                out.string("address", &format!("0x{:x}", device.address));
                out.string("name", &device.name);
                out.end('}');
            }
            out.end(']');
        }
        out.end('}');
        return finish(out, flags);
    }
    let mut text = String::from("mice:\n");
    for mouse in &devices.mice {
        let _ = writeln!(text, "\tMouse at {:x}:\n\t\t{}", mouse.address, mouse.name);
    }
    let _ = write!(text, "\n\nKeyboards:\n");
    for keyboard in &devices.keyboards {
        let _ = writeln!(
            text,
            "\tKeyboard at {:x}:\n\t\t{}\n\t\t\trules: r \"{}\", m \"{}\", l \"{}\", v \"{}\", o \
             \"{}\"\n\t\t\tactive layout index: {}\n\t\t\tactive keymap: \
             {}\n\t\t\tcapsLock: {}\n\t\t\tnumLock: {}\n\t\t\tmain: {}",
            keyboard.device.address,
            keyboard.device.name,
            keyboard.rules,
            keyboard.model,
            keyboard.layout,
            keyboard.variant,
            keyboard.options,
            // The word `none` for a keyboard with no active group, as
            // Hyprland's readable form has it
            // (`src/debug/HyprCtl.cpp:889`).
            keyboard
                .active_layout_index
                .map_or_else(|| "none".to_owned(), |index| index.to_string()),
            keyboard.active_keymap,
            yes_no(keyboard.caps_lock),
            yes_no(keyboard.num_lock),
            yes_no(keyboard.main),
        );
    }
    for (heading, group, what) in [
        ("Tablets", &devices.tablets, "Tablet"),
        ("Touch", &devices.touch, "Touch Device"),
        ("Switches", &devices.switches, "Switch Device"),
    ] {
        let _ = write!(text, "\n\n{heading}:\n");
        for device in group {
            let _ = writeln!(
                text,
                "\t{what} at {:x}:\n\t\t{}",
                device.address, device.name
            );
        }
    }
    text
}

/// `hyprctl layers`: every layer surface, by monitor and then by level.
fn layers(flags: Flags, snapshot: &Snapshot) -> String {
    /// `zwlr_layer_shell_v1`'s four, in its order.
    const LEVELS: [&str; 4] = ["background", "bottom", "top", "overlay"];
    let monitors: Vec<&str> = snapshot
        .monitors
        .iter()
        .map(|monitor| monitor.name.as_str())
        .collect();
    fn on<'a>(snapshot: &'a Snapshot, monitor: &'a str, level: u32) -> Vec<&'a Layer> {
        snapshot
            .layers
            .iter()
            .filter(|layer| layer.monitor == monitor && layer.level == level)
            .collect()
    }
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        for monitor in &monitors {
            out.object_field(monitor);
            out.object_field("levels");
            for level in 0..4u32 {
                out.array_field(&level.to_string());
                for layer in on(snapshot, monitor, level) {
                    out.object();
                    out.string("address", &format!("0x{:x}", layer.address));
                    out.number("x", i64::from(layer.at.0));
                    out.number("y", i64::from(layer.at.1));
                    out.number("w", i64::from(layer.size.0));
                    out.number("h", i64::from(layer.size.1));
                    out.string("namespace", &layer.namespace);
                    out.number("pid", i64::from(layer.pid));
                    out.end('}');
                }
                out.end(']');
            }
            out.end('}');
            out.end('}');
        }
        out.end('}');
        return finish(out, flags);
    }
    let mut text = String::new();
    for monitor in &monitors {
        let _ = writeln!(text, "Monitor {monitor}:");
        for (level, name) in LEVELS.iter().enumerate() {
            let level = u32::try_from(level).unwrap_or(0);
            let _ = writeln!(text, "\tLayer level {level} ({name}):");
            for layer in on(snapshot, monitor, level) {
                let _ = writeln!(
                    text,
                    "\t\tLayer {:x}: xywh: {} {} {} {}, namespace: {}, pid: {}",
                    layer.address,
                    layer.at.0,
                    layer.at.1,
                    layer.size.0,
                    layer.size.1,
                    layer.namespace,
                    layer.pid,
                );
            }
        }
        let _ = write!(text, "\n\n");
    }
    text
}

/// `hyprctl cursorpos`: where the pointer is.
fn cursor_position(flags: Flags, at: (i32, i32)) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        out.number("x", i64::from(at.0));
        out.number("y", i64::from(at.1));
        out.end('}');
        return finish(out, flags);
    }
    format!("{}, {}\n", at.0, at.1)
}

/// `yes` or `no`, which is what the readable form prints for a flag.
const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// `hyprctl plugin list`, in Hyprland's own shape.
fn plugins(flags: Flags, plugins: &[Plugin]) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for plugin in plugins {
            out.object();
            out.string("name", &plugin.name);
            out.string("author", &plugin.author);
            out.string("handle", &format!("{:x}", plugin.handle));
            out.string("version", &plugin.version);
            out.string("description", &plugin.description);
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    if plugins.is_empty() {
        return "no plugins loaded\n".to_owned();
    }
    let mut text = String::new();
    for plugin in plugins {
        let _ = writeln!(
            text,
            "\nPlugin {} by {}:\n\tHandle: {:x}\n\tVersion: {}\n\tDescription: {}",
            plugin.name, plugin.author, plugin.handle, plugin.version, plugin.description
        );
        // What Hyprland has no line for, because its plugins add their
        // dispatchers to the compositor's own table and this one's keep
        // them: the dispatchers a `dispatch` is handed to this plugin for.
        if !plugin.dispatchers.is_empty() {
            let _ = writeln!(text, "\tDispatchers: {}", plugin.dispatchers.join(", "));
        }
    }
    text
}

/// A command's argument split into its first word and the rest.
fn split(argument: &str) -> (String, String) {
    match argument.split_once(char::is_whitespace) {
        Some((first, rest)) => (first.to_owned(), rest.trim().to_owned()),
        None => (argument.to_owned(), String::new()),
    }
}

/// Whether to pretty-print: `r` asks for compact JSON.
const fn pretty(flags: Flags) -> bool {
    !flags.raw
}

fn self_version(flags: Flags, version: Version) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        out.string("branch", "");
        out.string("commit", version.commit);
        out.boolean("dirty", false);
        out.string("commit_message", "");
        out.string("commit_date", "");
        out.string("tag", version.version);
        out.number("commits", 0);
        out.string("buildAquamarine", "");
        out.string("buildHyprlang", "");
        out.string("buildHyprutils", "");
        out.string("buildHyprcursor", "");
        out.string("buildHyprgraphics", "");
        out.empty_array("flags");
        out.end('}');
        return finish(out, flags);
    }
    format!(
        "{} {}\n\nno flags were set\n",
        version.name, version.version
    )
}

fn monitors(flags: Flags, monitors: &[Monitor]) -> String {
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for monitor in monitors {
            out.object();
            out.number("id", i64::from(monitor.id));
            out.string("name", &monitor.name);
            out.string("description", &monitor.description);
            out.string("make", &monitor.make);
            out.string("model", &monitor.model);
            out.string("serial", &monitor.serial);
            out.number("width", i64::from(monitor.width));
            out.number("height", i64::from(monitor.height));
            out.field_refresh(monitor.refresh);
            out.number("x", i64::from(monitor.at.0));
            out.number("y", i64::from(monitor.at.1));
            // Hyprland has one `activeWorkspace`, an object; writing a
            // number of the same name beside it makes two fields with one
            // key, and which a reader takes is its own business.
            out.object_field("activeWorkspace");
            out.number("id", i64::from(monitor.active_workspace));
            out.string("name", &monitor.active_workspace_name);
            out.end('}');
            // A monitor showing no scratchpad has id 0 and an empty name,
            // which is what Hyprland writes for one.
            out.object_field("specialWorkspace");
            out.number(
                "id",
                i64::from(monitor.special_workspace.as_ref().map_or(0, |(id, _)| *id)),
            );
            out.string(
                "name",
                monitor
                    .special_workspace
                    .as_ref()
                    .map_or("", |(_, name)| name.as_str()),
            );
            out.end('}');
            let (left, top, right, bottom) = monitor.reserved;
            out.bare("reserved", &format!("[{left}, {top}, {right}, {bottom}]"));
            out.field_scale(monitor.scale);
            // The `wl_output.transform` number, which is what Hyprland
            // writes: `(int)m_transform`.
            out.number("transform", i64::from(monitor.transform.value()));
            out.boolean("focused", monitor.focused);
            out.boolean("dpmsStatus", monitor.dpms);
            // Nothing here does variable refresh, tearing or direct
            // scanout, and a monitor is never disabled once it is in the
            // list: a disabled one is left out when the screens are made.
            out.boolean("vrr", false);
            out.boolean("activelyTearing", false);
            out.boolean("disabled", false);
            out.string("currentFormat", "XRGB8888");
            out.string("mirrorOf", "none");
            out.bare(
                "availableModes",
                &format!(
                    "[\"{}x{}@{:.5}Hz\"]",
                    monitor.width, monitor.height, monitor.refresh
                ),
            );
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    let mut text = String::new();
    for monitor in monitors {
        let _ = writeln!(
            text,
            "Monitor {} (ID {}):\n\t{}x{}@{:.5} at {}x{}\n\tdescription: {}\n\tmake: {}\n\tmodel: {}\n\tserial: {}\n\tactive workspace: {} ({})\n\tspecial workspace: {} ({})\n\treserved: {} {} {} {}\n\tscale: {:.2}\n\ttransform: {}\n\tfocused: {}\n\tdpmsStatus: {}\n\tvrr: false\n\tactivelyTearing: false\n\tdisabled: false\n\tcurrentFormat: XRGB8888\n\tmirrorOf: none\n",
            monitor.name,
            monitor.id,
            monitor.width,
            monitor.height,
            monitor.refresh,
            monitor.at.0,
            monitor.at.1,
            monitor.description,
            monitor.make,
            monitor.model,
            monitor.serial,
            monitor.active_workspace,
            monitor.active_workspace_name,
            monitor.special_workspace.as_ref().map_or(0, |(id, _)| *id),
            monitor
                .special_workspace
                .as_ref()
                .map_or("", |(_, name)| name.as_str()),
            monitor.reserved.0,
            monitor.reserved.1,
            monitor.reserved.2,
            monitor.reserved.3,
            monitor.scale,
            monitor.transform.value(),
            if monitor.focused { "yes" } else { "no" },
            monitor.dpms,
        );
    }
    text
}

fn workspaces(flags: Flags, snapshot: &Snapshot) -> String {
    let workspaces = &snapshot.workspaces;
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for workspace in workspaces {
            out.object();
            write_workspace(&mut out, workspace, &snapshot.windows);
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    let mut text = String::new();
    for workspace in workspaces {
        let _ = writeln!(
            text,
            "workspace ID {} ({}) on monitor {}:\n\twindows: {}\n\thasfullscreen: {}\n",
            workspace.id,
            workspace.name,
            workspace.monitor,
            workspace.windows,
            i32::from(workspace.has_fullscreen),
        );
    }
    text
}

/// The window a workspace last had focused, as Hyprland's
/// `m_lastFocusedWindow` is: the one of its windows most recently focused.
///
/// This compositor keeps no focus history beyond the focused window
/// (`focusHistoryID` is 0 for it and -1 for the rest), so a workspace has a
/// last window while it holds the focused one, and none otherwise -- where
/// Hyprland would name the window it had focused before focus moved away.
/// waybar's `hyprland/window` reads the focused workspace's, which this
/// always answers.
fn last_window<'a>(workspace: &Workspace, windows: &'a [Window]) -> Option<&'a Window> {
    windows
        .iter()
        .filter(|window| window.workspace == workspace.id && window.focus_history >= 0)
        .min_by_key(|window| window.focus_history)
}

fn write_workspace(out: &mut Json, workspace: &Workspace, windows: &[Window]) {
    out.number("id", i64::from(workspace.id));
    out.string("name", &workspace.name);
    out.string("monitor", &workspace.monitor);
    out.number("monitorID", 0);
    out.number("windows", i64::from(workspace.windows));
    out.boolean("hasfullscreen", workspace.has_fullscreen);
    let last = last_window(workspace, windows);
    out.string(
        "lastwindow",
        &last.map_or_else(
            || "0x0".to_owned(),
            |window| format!("0x{:x}", window.address),
        ),
    );
    out.string(
        "lastwindowtitle",
        last.map_or("", |window| window.title.as_str()),
    );
    out.boolean("ispersistent", false);
}

fn clients(flags: Flags, snapshot: &Snapshot) -> String {
    let shown: Vec<&Window> = snapshot
        .windows
        .iter()
        .filter(|window| flags.all || window.mapped)
        .collect();
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.array();
        for window in shown {
            out.object();
            write_window(&mut out, window);
            out.end('}');
        }
        out.end(']');
        return finish(out, flags);
    }
    let mut text = String::new();
    for window in shown {
        let _ = write!(text, "{}", readable_window(window));
    }
    text
}

fn active_window(flags: Flags, snapshot: &Snapshot) -> String {
    let Some(window) = snapshot.active() else {
        // Hyprland answers `{}` in JSON and nothing at all otherwise, and
        // every bar is written for that.
        return match flags.format() {
            Format::Json => "{}\n".to_owned(),
            Format::Readable => "Invalid\n".to_owned(),
        };
    };
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        write_window(&mut out, window);
        out.end('}');
        return finish(out, flags);
    }
    readable_window(window)
}

fn active_workspace(flags: Flags, snapshot: &Snapshot) -> String {
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == snapshot.active_workspace)
        .cloned()
        .unwrap_or_else(|| Workspace {
            id: snapshot.active_workspace,
            name: snapshot.active_workspace.to_string(),
            ..Workspace::default()
        });
    if flags.format() == Format::Json {
        let mut out = Json::new(pretty(flags));
        out.object();
        write_workspace(&mut out, &workspace, &snapshot.windows);
        out.end('}');
        return finish(out, flags);
    }
    format!(
        "workspace ID {} ({}) on monitor {}:\n\twindows: {}\n\thasfullscreen: {}\n\n",
        workspace.id,
        workspace.name,
        workspace.monitor,
        workspace.windows,
        i32::from(workspace.has_fullscreen),
    )
}

/// The fields `hyprctl clients` gives a window, in Hyprland's own order.
///
/// The ones this compositor has no answer for are written at the value
/// Hyprland gives a window that has none, rather than left out: a bar reads
/// them by name, and a missing name is a crash in somebody else's program.
fn write_window(out: &mut Json, window: &Window) {
    out.string("address", &format!("0x{:x}", window.address));
    out.boolean("mapped", window.mapped);
    out.boolean("hidden", window.hidden);
    out.boolean("visible", window.visible);
    out.boolean("acceptsInput", window.accepts_input);
    out.pair("at", i64::from(window.at.0), i64::from(window.at.1));
    out.pair("size", i64::from(window.size.0), i64::from(window.size.1));
    out.object_field("workspace");
    out.number("id", i64::from(window.workspace));
    out.string("name", &window.workspace_name);
    out.end('}');
    out.boolean("floating", window.floating);
    out.number("monitor", i64::from(window.monitor));
    out.string("class", &window.class);
    out.string("title", &window.title);
    out.string("initialClass", &window.class);
    out.string("initialTitle", &window.title);
    out.number("pid", i64::from(window.pid));
    out.boolean("xwayland", false);
    out.boolean("pinned", false);
    out.boolean("pinFullscreened", false);
    // Numbers, not booleans: Hyprland's internal and client modes.
    out.number("fullscreen", i64::from(window.fullscreen_mode));
    out.number("fullscreenClient", i64::from(window.fullscreen_mode));
    out.string("fullscreenHandler", "default");
    out.boolean("allowedOverFullscreen", false);
    if window.grouped.is_empty() {
        out.empty_array("grouped");
    } else {
        out.array_field("grouped");
        for member in &window.grouped {
            out.item(&format!("0x{member:x}"));
        }
        out.end(']');
    }
    out.empty_array("tags");
    out.string("swallowing", "0x0");
    out.number("focusHistoryID", i64::from(window.focus_history));
    out.boolean("inhibitingIdle", false);
    out.string("xdgTag", "");
    out.string("xdgDescription", "");
    out.string("contentType", "none");
    out.boolean("tearingHint", false);
    // Hyprland's per-window counter, printed bare; the address is this
    // compositor's, and as stable.
    out.string("stableId", &format!("{:x}", window.address));
}

/// The readable form, which `hyprctl clients` prints without `-j`: the
/// same fields as the JSON, in the same order, booleans as `0` and `1`
/// (`CHyprCtl::getWindowData`).
fn readable_window(window: &Window) -> String {
    let flag = |value: bool| i32::from(value);
    let mut text = String::new();
    let _ = writeln!(text, "Window {:x} -> {}:", window.address, window.title);
    let _ = writeln!(text, "\tmapped: {}", flag(window.mapped));
    let _ = writeln!(text, "\thidden: {}", flag(window.hidden));
    let _ = writeln!(text, "\tvisible: {}", flag(window.visible));
    let _ = writeln!(text, "\tacceptsInput: {}", flag(window.accepts_input));
    let _ = writeln!(text, "\tat: {},{}", window.at.0, window.at.1);
    let _ = writeln!(text, "\tsize: {},{}", window.size.0, window.size.1);
    let _ = writeln!(
        text,
        "\tworkspace: {} ({})",
        window.workspace, window.workspace_name
    );
    let _ = writeln!(text, "\tfloating: {}", flag(window.floating));
    let _ = writeln!(text, "\tmonitor: {}", window.monitor);
    let _ = writeln!(text, "\tclass: {}", window.class);
    let _ = writeln!(text, "\ttitle: {}", window.title);
    let _ = writeln!(text, "\tinitialClass: {}", window.class);
    let _ = writeln!(text, "\tinitialTitle: {}", window.title);
    let _ = writeln!(text, "\tpid: {}", window.pid);
    let _ = writeln!(text, "\txwayland: 0");
    let _ = writeln!(text, "\tpinned: 0");
    let _ = writeln!(text, "\tpinFullscreened: 0");
    let _ = writeln!(text, "\tfullscreen: {}", window.fullscreen_mode);
    let _ = writeln!(text, "\tfullscreenClient: {}", window.fullscreen_mode);
    let _ = writeln!(text, "\tfullscreenHandler: default");
    let _ = writeln!(text, "\tallowedOverFullscreen: 0");
    // Hyprland prints a lone `0` for a window in no group, and the members'
    // addresses without `0x` for one in a group.
    let _ = writeln!(text, "\tgrouped: {}", grouped(&window.grouped));
    let _ = writeln!(text, "\ttags: ");
    let _ = writeln!(text, "\tswallowing: 0");
    let _ = writeln!(text, "\tfocusHistoryID: {}", window.focus_history);
    let _ = writeln!(text, "\tinhibitingIdle: 0");
    let _ = writeln!(text, "\txdgTag: ");
    let _ = writeln!(text, "\txdgDescription: ");
    let _ = writeln!(text, "\tcontentType: none");
    let _ = writeln!(text, "\ttearingHint: 0");
    let _ = writeln!(text, "\tstableID: {:x}\n", window.address);
    text
}

/// The readable form's `grouped:` value: `0` for no group.
fn grouped(members: &[u64]) -> String {
    if members.is_empty() {
        return "0".to_owned();
    }
    members
        .iter()
        .map(|member| format!("{member:x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Add the trailing newline unless the request asked for none.
fn finish(out: Json, flags: Flags) -> String {
    let mut text = out.finish();
    if !flags.no_newline {
        text.push('\n');
    }
    text
}
