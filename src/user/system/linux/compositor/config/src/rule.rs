//! A `windowrule =` line, read into what the compositor does with it.
//!
//! Hyprland 0.56's form is a list of comma-separated fields, each a name and
//! a value with a space between them; a field whose name begins `match:` is
//! something the window must be, and every other field is something to do to
//! it (`CConfigManager::handleWindowrule`):
//!
//! ```text
//! windowrule = float, match:class ^(foot)$
//! windowrule = size 800 600, match:title ^(Save.*)$, match:float 1
//! ```
//!
//! `windowrulev2` is refused, with Hyprland's own words, because that is
//! what Hyprland 0.56 does with it: the two syntaxes were merged and the old
//! one taken away.
//!
//! What a rule matches on is a regular expression for the four names a
//! window has and a yes-or-no for the states it can be in;
//! `src/user/system/linux/compositor/regex` says which patterns are understood. What a rule does
//! is the list on [`Effect`] -- the parts of Hyprland's that the layout and
//! the renderer here can carry out.

use compositor_regex::Regex;

use crate::value::Gradient;

/// What a window must be for a rule to apply to it.
#[derive(Clone, Debug)]
pub enum Matcher {
    /// `match:class`: the application id, which Hyprland calls the class.
    Class(Regex),
    /// `match:title`.
    Title(Regex),
    /// `match:initial_class`: the class it had when it opened.
    InitialClass(Regex),
    /// `match:initial_title`.
    InitialTitle(Regex),
    /// `match:float`: whether it floats.
    Floating(bool),
    /// `match:fullscreen`.
    Fullscreen(bool),
    /// `match:focus`: whether it is the focused window.
    Focused(bool),
    /// `match:pin`: whether it is pinned across workspaces.
    Pinned(bool),
    /// `match:modal`: whether `xdg_dialog_v1` called it modal.
    Modal(bool),
    /// `match:group`: whether it is in a group.
    Grouped(bool),
    /// `match:xwayland`: whether it is an X11 window. No window here is --
    /// there is no `XWayland` -- so a rule asking for one matches nothing.
    Xwayland(bool),
    /// `match:tag`: one of the tags a `tag` effect or `tagwindow` gave it.
    Tag(Regex),
    /// `match:workspace`: the workspace it is on, by name.
    Workspace(Regex),
    /// `match:namespace`: the namespace, for the layer surfaces Hyprland
    /// matches with the same engine.
    Namespace(Regex),
    /// `match:xdg_tag`: the name `xdg_toplevel_tag_manager_v1` gave it.
    XdgTag(Regex),
    /// `match:content`: what `wp_content_type_v1` says it is showing.
    Content(Regex),
    /// `match:fullscreen_state_internal`: the fullscreen state the
    /// compositor put it in, as a number.
    FullscreenStateInternal(i64),
    /// `match:fullscreen_state_client`: the fullscreen state the client
    /// asked for, as a number.
    FullscreenStateClient(i64),
}

/// Every `match:` property Hyprland's rule engine knows.
///
/// `Rule.cpp`'s `MATCH_PROP_STRINGS`. A `layerrule` may name any of them
/// and Hyprland reads all of them, even though a layer surface only ever
/// matches on `namespace`; [`is_prop`] is how the layer half asks.
const PROPS: [&str; 18] = [
    "class",
    "title",
    "initial_class",
    "initial_title",
    "float",
    "tag",
    "xwayland",
    "fullscreen",
    "pin",
    "focus",
    "group",
    "modal",
    "fullscreen_state_internal",
    "fullscreen_state_client",
    "workspace",
    "content",
    "xdg_tag",
    "namespace",
];

/// Whether `name` is a `match:` property Hyprland knows.
#[must_use]
pub fn is_prop(name: &str) -> bool {
    PROPS.contains(&name)
}

/// What a rule does to a window it matches.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// `float`: take it out of the tiling.
    Float,
    /// `tile`: put it back in, which is what a window is by default.
    Tile,
    /// `size <width> <height>`, each in pixels or as a percentage of the
    /// monitor.
    Size(Length, Length),
    /// `move <x> <y>`, the same, or `center`.
    Move(Length, Length),
    /// `center`: put it in the middle of the monitor.
    Center,
    /// `workspace <id>`, and whether the focus follows it there.
    Workspace {
        /// The workspace, as a `workspace` dispatcher would name it.
        target: String,
        /// `silent`: the window goes and the focus stays.
        silent: bool,
    },
    /// `fullscreen`.
    Fullscreen,
    /// `maximize`.
    Maximize,
    /// `no_focus`: it opens without taking the focus.
    NoFocus,
    /// `opacity <a>`: how much of it shows.
    Opacity(f32),
    /// `rounding_power <p>`: the curve its corners are cut by, which is a
    /// superellipse's exponent. Two is a circle.
    RoundingPower(f32),
    /// `border_color <gradient>`: its border, instead of the focused and
    /// unfocused ones.
    BorderColor(Gradient),
    /// `decorate <yes-or-no>`: whether it gets a border and a shadow at
    /// all.
    Decorate(bool),
    /// `opaque <yes-or-no>`: it is drawn as if every pixel were opaque,
    /// whatever its buffer's alpha says.
    Opaque(bool),
    /// `nearest_neighbor <yes-or-no>`: a stretched window is sampled
    /// nearest rather than bilinear, for pixel art.
    NearestNeighbor(bool),
    /// `monitor <name>`: the window opens on that monitor.
    Monitor(String),
    /// `min_size <w> <h>`: never smaller than this.
    MinSize(i64, i64),
    /// `max_size <w> <h>`: never larger.
    MaxSize(i64, i64),
    /// `no_max_size`: whatever maximum was set is cleared, which is what a
    /// person writes for a window whose own protocol maximum is wrong.
    NoMaxSize,
    /// `keep_aspect_ratio`: the shape it was first given is the shape it
    /// keeps.
    KeepAspectRatio(bool),
    /// `fullscreen_state <internal> [client]`: the fullscreen state it
    /// opens in, as two numbers.
    FullscreenState(i64, i64),
    /// `scrolling_width <fraction>`: how much of the screen its column
    /// takes in the scrolling layout.
    ScrollingWidth(f32),
    /// `group [set|new|lock|barred|invade|deny|override|unset] [always]`:
    /// what the window does about groups when it opens.
    Group(GroupRules),
    /// `no_close_for <milliseconds>`: `killactive` will not close it until
    /// that long after it opened, which is what a person writes for a
    /// window they keep shutting by accident.
    NoCloseFor(i64),
    /// `dim_around <yes-or-no>`: everything behind it is darkened by
    /// `decoration:dim_around` while it is up.
    DimAround(bool),
    /// `xray <yes-or-no>`: the blur behind it is of the wallpaper, or --
    /// `xray 0` -- never of the wallpaper alone, whatever
    /// `decoration:blur:xray` and the tiling would have chosen.
    Xray(bool),
    /// `no_screen_share <yes-or-no>`: a screenshot or a screen being shared
    /// shows a black box where it is.
    NoScreenShare(bool),
    /// `rounding <n>`: how far its corners are cut.
    Rounding(i64),
    /// `border_size <n>`.
    BorderSize(i64),
    /// `no_blur`, `no_shadow`, `no_dim`: a decoration this window does not
    /// get.
    Without(Decoration),
    /// `pin`: it stays on the screen across workspaces.
    Pin,
    /// `pseudo`: it keeps its own size inside its tiled slot.
    Pseudo,
    /// `tag <name>`: a tag that a later `match:tag` finds it by.
    Tag(String),
    /// `suppress_event <event> ...`: what the window asks for and does not
    /// get.
    Suppress(Vec<String>),
    /// `persistent_size <yes-or-no>`: a floating window opens at the size
    /// the last window of the same name closed at.
    PersistentSize(bool),
    /// An effect Hyprland has that this compositor does not carry out,
    /// kept by name rather than refused.
    Unhandled(String),
}

/// Every effect name Hyprland's own table has.
///
/// `WindowRuleEffectContainer.cpp`'s list, in its order. A name in it that
/// this compositor does not carry out becomes [`Effect::Unhandled`]; a name
/// that is not in it at all is a typo, and is refused so a person hears
/// about it rather than watching a rule quietly do nothing.
const KNOWN: [&str; 55] = [
    "float",
    "tile",
    "fullscreen",
    "maximize",
    "fullscreen_state",
    "move",
    "size",
    "center",
    "pseudo",
    "monitor",
    "workspace",
    "no_initial_focus",
    "pin",
    "group",
    "suppress_event",
    "content",
    "no_close_for",
    "scrolling_width",
    "rounding",
    "rounding_power",
    "persistent_size",
    "animation",
    "border_color",
    "idle_inhibit",
    "opacity",
    "tag",
    "max_size",
    "min_size",
    "border_size",
    "allows_input",
    "dim_around",
    "decorate",
    "focus_on_activate",
    "keep_aspect_ratio",
    "nearest_neighbor",
    "no_anim",
    "no_blur",
    "no_dim",
    "no_focus",
    "no_follow_mouse",
    "no_max_size",
    "no_shadow",
    "no_shortcuts_inhibit",
    "opaque",
    "force_rgbx",
    "sync_fullscreen",
    "immediate",
    "xray",
    "render_unfocused",
    "no_screen_share",
    "no_vrr",
    "no_auto_hdr",
    "tonemap",
    "scroll_mouse",
    "scroll_touchpad",
];

/// What `windowrule = group ...` asks for.
///
/// `CWindow::applyDynamicRules`' `m_groupRules`, read the same way: a list
/// of words, each turning one flag on, with `always` qualifying whichever
/// word came before it. `override` and `unset` clear everything set so
/// far, which is how a later rule undoes an earlier one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupRules {
    /// `set`: the window becomes a group of one when it opens, so the next
    /// window opened on top of it joins it rather than splitting the
    /// workspace.
    pub set: bool,
    /// `lock`: the group it is in or makes is locked, so nothing new joins
    /// it.
    pub lock: bool,
    /// `barred`: it does not join a group it is opened onto.
    pub barred: bool,
    /// `invade`: it joins a *locked* group anyway.
    pub invade: bool,
    /// `deny`: nothing may be added to its group by being dropped on it.
    pub deny: bool,
    /// `set always` and `lock always`: the flag holds for every window
    /// opened after it and not only the next.
    pub always: bool,
    /// `override` or `unset`: every flag an earlier rule set is cleared.
    pub cleared: bool,
}

impl GroupRules {
    /// Read the words after `group`.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut held = Self::default();
        let mut before = "";
        for word in text.split_whitespace() {
            match word {
                "group" => {}
                "set" => held.set = true,
                // `new` is Hyprland's shorthand for `barred set`.
                "new" => {
                    held.set = true;
                    held.barred = true;
                }
                "lock" => held.lock = true,
                "invade" => held.invade = true,
                "barred" => held.barred = true,
                "deny" => held.deny = true,
                "override" => {
                    held = Self {
                        cleared: true,
                        ..Self::default()
                    }
                }
                "unset" => {
                    held = Self {
                        cleared: true,
                        ..Self::default()
                    };
                    break;
                }
                // `always` qualifies the word before it, and Hyprland
                // accepts it only after `set` or `lock`.
                "always" if matches!(before, "set" | "lock" | "group") => held.always = true,
                _ => {}
            }
            before = word;
        }
        held
    }
}

/// A decoration a rule can take away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoration {
    /// `no_blur`.
    Blur,
    /// `no_shadow`.
    Shadow,
    /// `no_dim`.
    Dim,
}

/// A length a rule gives: pixels, or a percentage of the monitor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    /// A number of pixels.
    Pixels(i64),
    /// A share of the monitor's own width or height, from zero to one.
    Share(f64),
}

impl Length {
    /// The length in pixels, against a monitor `whole` pixels long.
    #[must_use]
    pub fn against(self, whole: i64) -> i64 {
        match self {
            Self::Pixels(pixels) => pixels,
            Self::Share(share) => {
                #[expect(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    reason = "a monitor's pixels are far inside f64's exact range"
                )]
                let scaled = (whole as f64 * share).round() as i64;
                scaled
            }
        }
    }

    /// Read `50%` or `500`.
    fn parse(text: &str) -> Result<Self, String> {
        if let Some(number) = text.strip_suffix('%') {
            let share: f64 = number
                .trim()
                .parse()
                .map_err(|_| format!("`{text}` is not a percentage"))?;
            return Ok(Self::Share(share / 100.0));
        }
        text.trim()
            .parse()
            .map(Self::Pixels)
            .map_err(|_| format!("`{text}` is not a length"))
    }
}

/// One `windowrule =` line.
#[derive(Clone, Debug)]
pub struct WindowRule {
    /// What it does, in the order the line gave them.
    pub effects: Vec<Effect>,
    /// What the window must be. A rule with none matches every window,
    /// which is what a line with no `match:` field means.
    pub matchers: Vec<Matcher>,
}

impl WindowRule {
    /// Read a `windowrule =` line's value.
    ///
    /// # Errors
    ///
    /// Hyprland's own wording for a field it cannot read.
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut effects = Vec::new();
        let mut matchers = Vec::new();
        for field in value.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            match field.strip_prefix("match:") {
                Some(rest) => matchers.push(matcher(rest)?),
                None => effects.push(effect(field)?),
            }
        }
        if effects.is_empty() {
            return Err(format!("windowrule: `{value}` does nothing"));
        }
        Ok(Self { effects, matchers })
    }

    /// Whether this rule applies to a window that is `what`.
    #[must_use]
    pub fn matches(&self, what: &Window<'_>) -> bool {
        self.matchers.iter().all(|matcher| match matcher {
            Matcher::Class(pattern) => pattern.matches(what.class),
            Matcher::Title(pattern) => pattern.matches(what.title),
            Matcher::InitialClass(pattern) => pattern.matches(what.initial_class),
            Matcher::InitialTitle(pattern) => pattern.matches(what.initial_title),
            Matcher::Floating(wanted) => what.floating == *wanted,
            Matcher::Fullscreen(wanted) => what.fullscreen == *wanted,
            Matcher::Focused(wanted) => what.focused == *wanted,
            Matcher::Pinned(wanted) => what.pinned == *wanted,
            Matcher::Modal(wanted) => what.modal == *wanted,
            Matcher::Grouped(wanted) => what.grouped == *wanted,
            Matcher::Xwayland(wanted) => !*wanted,
            Matcher::Tag(pattern) => what.tags.iter().any(|tag| pattern.matches(tag)),
            Matcher::Workspace(pattern) => pattern.matches(what.workspace),
            Matcher::Namespace(pattern) => pattern.matches(what.namespace),
            Matcher::XdgTag(pattern) => pattern.matches(what.xdg_tag),
            Matcher::Content(pattern) => pattern.matches(what.content),
            Matcher::FullscreenStateInternal(wanted) => what.fullscreen_state_internal == *wanted,
            Matcher::FullscreenStateClient(wanted) => what.fullscreen_state_client == *wanted,
        })
    }
}

/// A window, as a rule sees it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Window<'a> {
    /// `xdg_toplevel.set_app_id`, which Hyprland calls the class.
    pub class: &'a str,
    /// `xdg_toplevel.set_title`.
    pub title: &'a str,
    /// The class it opened with.
    pub initial_class: &'a str,
    /// The title it opened with.
    pub initial_title: &'a str,
    /// Whether it floats.
    pub floating: bool,
    /// Whether it is fullscreen.
    pub fullscreen: bool,
    /// Whether it has the focus.
    pub focused: bool,
    /// Whether it is pinned across workspaces.
    pub pinned: bool,
    /// Whether `xdg_dialog_v1` called it modal.
    pub modal: bool,
    /// Whether it is in a group.
    pub grouped: bool,
    /// The tags it has been given.
    pub tags: &'a [String],
    /// The workspace it is on, by name.
    pub workspace: &'a str,
    /// Its namespace, which only a layer surface has.
    pub namespace: &'a str,
    /// The name `xdg_toplevel_tag_manager_v1` gave it.
    pub xdg_tag: &'a str,
    /// What `wp_content_type_v1` says it is showing.
    pub content: &'a str,
    /// The fullscreen state the compositor put it in.
    pub fullscreen_state_internal: i64,
    /// The fullscreen state the client asked for.
    pub fullscreen_state_client: i64,
}

/// One `match:` field.
fn matcher(field: &str) -> Result<Matcher, String> {
    let (name, value) = field
        .split_once(' ')
        .ok_or_else(|| format!("invalid field {field}: missing a value"))?;
    let value = value.trim();
    let pattern = || Regex::new(value).map_err(|why| format!("invalid prop {name}: {why}"));
    let number = || {
        value
            .parse()
            .map_err(|_| format!("invalid prop {name}: `{value}` is not a number"))
    };
    let yes_or_no = || match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!(
            "invalid prop {name}: `{other}` is not a yes or a no"
        )),
    };
    match name {
        "class" => pattern().map(Matcher::Class),
        "title" => pattern().map(Matcher::Title),
        "initial_class" => pattern().map(Matcher::InitialClass),
        "initial_title" => pattern().map(Matcher::InitialTitle),
        "float" => yes_or_no().map(Matcher::Floating),
        "fullscreen" => yes_or_no().map(Matcher::Fullscreen),
        "focus" => yes_or_no().map(Matcher::Focused),
        "pin" => yes_or_no().map(Matcher::Pinned),
        "modal" => yes_or_no().map(Matcher::Modal),
        "group" => yes_or_no().map(Matcher::Grouped),
        // No window here is XWayland's -- there is no XWayland -- so the
        // rule matches only when it asks for a native one. A real
        // configuration uses this the way example's does, to keep a rule
        // off Wayland windows, and a compositor that refused the matcher
        // would cost a person the whole line.
        "xwayland" => yes_or_no().map(Matcher::Xwayland),
        "tag" => pattern().map(Matcher::Tag),
        "workspace" => pattern().map(Matcher::Workspace),
        "namespace" => pattern().map(Matcher::Namespace),
        "xdg_tag" => pattern().map(Matcher::XdgTag),
        "content" => pattern().map(Matcher::Content),
        "fullscreen_state_internal" => number().map(Matcher::FullscreenStateInternal),
        "fullscreen_state_client" => number().map(Matcher::FullscreenStateClient),
        other => Err(format!("invalid prop {other}")),
    }
}

/// One effect field.
fn effect(field: &str) -> Result<Effect, String> {
    let (name, value) = match field.split_once(' ') {
        Some((name, value)) => (name, value.trim()),
        None => (field, ""),
    };
    let two = || -> Result<(Length, Length), String> {
        let (first, second) = value
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("invalid field {name}: it takes two lengths"))?;
        Ok((Length::parse(first)?, Length::parse(second.trim())?))
    };
    // Hyprland's boolean effects take `truthy`, and a bare word is true:
    // `windowrule = opaque` and `opaque true` mean the same thing.
    let yes = |value: &str| -> bool {
        if value.is_empty() || value == "1" {
            return true;
        }
        let lowered = value.to_lowercase();
        ["true", "yes", "on"]
            .iter()
            .any(|word| lowered.starts_with(word))
    };
    let number = |what: &str| -> Result<i64, String> {
        value
            .parse()
            .map_err(|_| format!("invalid field {name}: `{value}` is not {what}"))
    };
    match name {
        "float" => Ok(Effect::Float),
        "tile" => Ok(Effect::Tile),
        "center" => Ok(Effect::Center),
        "fullscreen" => Ok(Effect::Fullscreen),
        "maximize" => Ok(Effect::Maximize),
        "no_focus" => Ok(Effect::NoFocus),
        "no_blur" => Ok(Effect::Without(Decoration::Blur)),
        "no_shadow" => Ok(Effect::Without(Decoration::Shadow)),
        "no_dim" => Ok(Effect::Without(Decoration::Dim)),
        "size" => two().map(|(width, height)| Effect::Size(width, height)),
        "move" => {
            if value == "center" {
                return Ok(Effect::Center);
            }
            two().map(|(x, y)| Effect::Move(x, y))
        }
        "workspace" => {
            let (target, silent) = match value.strip_suffix("silent") {
                Some(head) => (head.trim(), true),
                None => (value, false),
            };
            if target.is_empty() {
                return Err("invalid field workspace: it takes a workspace".to_owned());
            }
            Ok(Effect::Workspace {
                target: target.to_owned(),
                silent,
            })
        }
        "opacity" => {
            // Hyprland takes one number, or two for the focused and
            // unfocused states; the first is what this carries.
            let first = value.split_whitespace().next().unwrap_or("");
            let opacity: f32 = first
                .parse()
                .map_err(|_| format!("invalid field opacity: `{value}` is not a number"))?;
            if !(0.0..=1.0).contains(&opacity) {
                return Err(format!("invalid field opacity: {opacity} is not a share"));
            }
            Ok(Effect::Opacity(opacity))
        }
        "rounding" => number("a number of pixels").map(Effect::Rounding),
        "rounding_power" => {
            let power: f32 = value
                .parse()
                .map_err(|_| format!("invalid field rounding_power: `{value}` is not a number"))?;
            Ok(Effect::RoundingPower(power.clamp(1.0, 10.0)))
        }
        // Hyprland's rule takes one gradient or two -- the second for the
        // unfocused state -- and this carries the first, as the renderer
        // draws one border.
        "border_color" => crate::value::parse_gradient(value)
            .map(Effect::BorderColor)
            .map_err(|why| format!("invalid field border_color: {why}")),
        "persistent_size" => Ok(Effect::PersistentSize(yes(value))),
        "decorate" => Ok(Effect::Decorate(yes(value))),
        "opaque" => Ok(Effect::Opaque(yes(value))),
        "nearest_neighbor" => Ok(Effect::NearestNeighbor(yes(value))),
        "monitor" => {
            if value.is_empty() {
                return Err("invalid field monitor: it takes a monitor".to_owned());
            }
            Ok(Effect::Monitor(value.to_owned()))
        }
        "min_size" | "max_size" => {
            let (wide, tall) = two()?;
            // Pixels, always: a size limit as a share of the monitor would
            // change when a window moved between screens, which is not
            // what a limit is for, and Hyprland reads two integers.
            let (wide, tall) = (wide.against(0), tall.against(0));
            if name == "min_size" {
                Ok(Effect::MinSize(wide.max(0), tall.max(0)))
            } else {
                Ok(Effect::MaxSize(wide.max(0), tall.max(0)))
            }
        }
        "no_max_size" => Ok(Effect::NoMaxSize),
        "keep_aspect_ratio" => Ok(Effect::KeepAspectRatio(yes(value))),
        "fullscreen_state" => {
            let mut fields = value.split_whitespace();
            let internal: i64 =
                fields.next().unwrap_or("").parse().map_err(|_| {
                    format!("invalid field fullscreen_state: `{value}` is not a state")
                })?;
            // Hyprland's second number is the state the *client* is told,
            // and it defaults to the first.
            let client = fields
                .next()
                .and_then(|text| text.parse().ok())
                .unwrap_or(internal);
            Ok(Effect::FullscreenState(internal, client))
        }
        "scrolling_width" => {
            let width: f32 = value
                .parse()
                .map_err(|_| format!("invalid field scrolling_width: `{value}` is not a share"))?;
            Ok(Effect::ScrollingWidth(width.clamp(0.1, 1.0)))
        }
        "group" => Ok(Effect::Group(GroupRules::parse(value))),
        "no_close_for" => {
            number("a number of milliseconds").map(|held| Effect::NoCloseFor(held.max(0)))
        }
        "dim_around" => Ok(Effect::DimAround(yes(value))),
        "xray" => Ok(Effect::Xray(yes(value))),
        "no_screen_share" => Ok(Effect::NoScreenShare(yes(value))),
        // `no_initial_focus` is `no_focus` by another name: Hyprland keeps
        // them apart because one is checked when the window maps and the
        // other whenever it would be focused, and this compositor applies
        // its rules when a window maps, which is the same moment.
        "no_initial_focus" => Ok(Effect::NoFocus),
        "border_size" => number("a number of pixels").map(Effect::BorderSize),
        "pin" => Ok(Effect::Pin),
        "pseudo" => Ok(Effect::Pseudo),
        "tag" => Ok(Effect::Tag(value.to_owned())),
        // `suppress_event <event>`: a window that asks to be maximized or
        // fullscreen is not. A tiling compositor decides both, and a
        // configuration says this so that an application which asks on
        // startup does not fight the layout.
        "suppress_event" => Ok(Effect::Suppress(
            value.split_whitespace().map(ToOwned::to_owned).collect(),
        )),
        // Every other effect Hyprland has is read and kept without being
        // acted on, rather than refused. One unsupported word in a line
        // must not cost a person the matchers beside it, and a rule this
        // compositor cannot carry out is still a rule it can report.
        other if KNOWN.contains(&other) => Ok(Effect::Unhandled(other.to_owned())),
        other => Err(format!("invalid field type {other}")),
    }
}
