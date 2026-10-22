//! A `monitor =` line, read into what the compositor does with it.
//!
//! Hyprland's form is `monitor = name, resolution, position, scale`, with
//! further keyword pairs after it (`ConfigManager::handleMonitor`). An empty
//! name is the rule for every monitor no other rule names, which is what
//! `monitor = , preferred, auto, 1` in every example configuration is.
//!
//! What is read here: the name, `disable`, the resolution as `preferred` or
//! `WIDTHxHEIGHT` with an optional `@REFRESH`, the position as `auto` or
//! `XxY`, the scale as `auto` or a number, and the `transform, N` pair after
//! them. `mirror`, `bitdepth`, `vrr` and the rest are not done, and a line
//! that carries one says so rather than being read as if it were not there.
//!
//! `monitor = NAME, addreserved, TOP, BOTTOM, LEFT, RIGHT` is not a rule for
//! the monitor but strips of it kept free of windows, beside what the layer
//! surfaces reserve: [`AddedReserved`]. The Pixel 7's VM sets it while
//! Android's keyboard covers the bottom of the screen, so the windows are
//! laid out above it as on a shorter screen.

use crate::Gaps;

/// What a `monitor =` line asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorRule {
    /// The monitor it names, or empty for every monitor no other rule
    /// names.
    pub name: String,
    /// `monitor = name, disable`: the monitor is left dark.
    pub disabled: bool,
    /// The mode it asks for.
    pub mode: Mode,
    /// Where the monitor goes.
    pub position: Position,
    /// How many buffer pixels a logical one is.
    pub scale: Scale,
    /// `transform, N`: how the monitor is turned, [`Transform::Normal`]
    /// for a line that does not say.
    pub transform: Transform,
}

/// How a monitor is turned: `monitor = ..., transform, N`, whose `N` is a
/// `wl_output.transform` value, 0 to 7.
///
/// The protocol's word for `1` is "90 degrees counter-clockwise", and it is
/// what Hyprland does to the picture it lays out to make the buffer the
/// connector scans out: the desktop's top left goes to the buffer's bottom
/// left. A person reads that upright on a monitor turned clockwise, onto
/// its right-hand edge; `3` is the other way round, for a monitor turned
/// onto its left-hand edge.
///
/// The flipped four mirror the picture left to right first and then turn
/// it as the first four do, which is the protocol's own statement of them.
/// `compositor_render::transform` is where a pixel is actually moved, and
/// derives each place from Hyprland's matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transform {
    /// 0: as it comes.
    #[default]
    Normal,
    /// 1: turned 90 degrees.
    Rotated90,
    /// 2: upside down.
    Rotated180,
    /// 3: turned 270 degrees, which is 90 the other way.
    Rotated270,
    /// 4: mirrored left to right.
    Flipped,
    /// 5: mirrored, then turned 90 degrees.
    Flipped90,
    /// 6: mirrored, then upside down, which is mirrored top to bottom.
    Flipped180,
    /// 7: mirrored, then turned 270 degrees.
    Flipped270,
}

impl Transform {
    /// Every transform, in the protocol's order, so that `ALL[n]` is `n`'s.
    pub const ALL: [Self; 8] = [
        Self::Normal,
        Self::Rotated90,
        Self::Rotated180,
        Self::Rotated270,
        Self::Flipped,
        Self::Flipped90,
        Self::Flipped180,
        Self::Flipped270,
    ];

    /// The transform whose `wl_output.transform` value is `value`, if there
    /// is one.
    #[must_use]
    pub fn from_value(value: u32) -> Option<Self> {
        Self::ALL.get(usize::try_from(value).ok()?).copied()
    }

    /// Its `wl_output.transform` value: what `hyprctl monitors` prints and
    /// `wl_output.geometry` carries.
    #[must_use]
    pub const fn value(self) -> u32 {
        match self {
            Self::Normal => 0,
            Self::Rotated90 => 1,
            Self::Rotated180 => 2,
            Self::Rotated270 => 3,
            Self::Flipped => 4,
            Self::Flipped90 => 5,
            Self::Flipped180 => 6,
            Self::Flipped270 => 7,
        }
    }

    /// Whether it turns a quarter, so that a monitor's width is the height
    /// of what is laid out on it: 1, 3, 5 and 7.
    #[must_use]
    pub const fn swaps(self) -> bool {
        matches!(
            self,
            Self::Rotated90 | Self::Rotated270 | Self::Flipped90 | Self::Flipped270
        )
    }

    /// A mode's `(width, height)` as it is laid out: the two exchanged for a
    /// quarter turn, and as they are otherwise. Hyprland's
    /// `m_transformedSize`.
    #[must_use]
    pub const fn size<T: Copy>(self, (width, height): (T, T)) -> (T, T) {
        if self.swaps() {
            (height, width)
        } else {
            (width, height)
        }
    }
}

/// The resolution field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// `preferred` (and `maxwidth`, which Hyprland treats so): the mode the
    /// connector marks preferred.
    Preferred,
    /// `highres`: the listed mode with the most pixels, the fastest of
    /// them.
    HighRes,
    /// `highrr`: the listed mode with the fastest refresh, the largest of
    /// them.
    HighRr,
    /// `WIDTHxHEIGHT[@REFRESH]`.
    Fixed {
        /// In pixels.
        width: u32,
        /// In pixels.
        height: u32,
        /// In hertz, if the line gave one.
        refresh: Option<f64>,
    },
}

/// The position field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// `auto`: to the right of the monitors already placed.
    Auto,
    /// `XxY`: where the line says, in logical pixels.
    At(i64, i64),
}

/// The scale field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scale {
    /// `auto`: the compositor chooses, which here is 1.
    Auto,
    /// A number.
    Fixed(f64),
}

/// The keywords Hyprland reads after the scale that this compositor does
/// not do yet, which a line is refused for naming rather than read as if it
/// did not.
const NOT_DONE: [&str; 7] = [
    "mirror",
    "bitdepth",
    "cm",
    "sdrsaturation",
    "sdrbrightness",
    "vrr",
    "icc",
];

/// `monitor = NAME, addreserved, TOP, BOTTOM, LEFT, RIGHT`: strips of a
/// monitor, in logical pixels, that windows are not tiled into, added to
/// what the layer surfaces' exclusive zones reserve. `hyprctl keyword
/// monitor NAME,addreserved,0,0,0,0` gives them back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedReserved {
    /// The monitor, named as a [`MonitorRule`] names one; empty for every
    /// monitor.
    pub name: String,
    /// The strips.
    pub reserved: Gaps,
}

impl AddedReserved {
    /// Every `addreserved` line among the `monitor =` values, in the order
    /// the file and the `hyprctl keyword`s after it have them. Lines that
    /// are not `addreserved`, or whose numbers cannot be read, are skipped:
    /// [`MonitorRule::read_all`] is what says why.
    #[must_use]
    pub fn read_all<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Self> {
        values
            .into_iter()
            .filter_map(|value| {
                let fields: Vec<&str> = value.split(',').map(str::trim).collect();
                match fields.as_slice() {
                    [name, "addreserved", rest @ ..] => Some(Self {
                        name: (*name).to_owned(),
                        reserved: parse_reserved(rest).ok()?,
                    }),
                    _ => None,
                }
            })
            .collect()
    }

    /// What the last of `added` that names this monitor reserves, as a
    /// later line wins; nothing when none does.
    #[must_use]
    pub fn for_monitor(added: &[Self], name: &str, description: &str) -> Gaps {
        added
            .iter()
            .rev()
            .find(|added| names(&added.name, name, description))
            .map_or(Gaps::all(0), |added| added.reserved)
    }
}

/// Whether a line's `named` monitor is the one called `name`, described as
/// `description`: every monitor when it is empty, by description after
/// `desc:`, and by connector otherwise.
fn names(named: &str, name: &str, description: &str) -> bool {
    if named.is_empty() {
        return true;
    }
    match named.strip_prefix("desc:") {
        Some(wanted) => {
            let wanted = wanted.trim();
            !wanted.is_empty() && description.starts_with(wanted)
        }
        None => named == name,
    }
}

/// `TOP, BOTTOM, LEFT, RIGHT`, each a number of logical pixels from 0.
fn parse_reserved(fields: &[&str]) -> Result<Gaps, String> {
    let [top, bottom, left, right] = fields else {
        return Err("addreserved wants four numbers: top, bottom, left, right".to_owned());
    };
    let side = |field: &str| {
        field
            .parse::<i64>()
            .ok()
            .filter(|pixels| *pixels >= 0)
            .ok_or_else(|| format!("addreserved: `{field}` is not a number of pixels"))
    };
    Ok(Gaps {
        top: side(top)?,
        right: side(right)?,
        bottom: side(bottom)?,
        left: side(left)?,
    })
}

impl MonitorRule {
    /// Read every `monitor =` line's value, in the order the file has them:
    /// the rules, and each line that could not be read with why.
    ///
    /// One form is not a rule of its own. `monitor = NAME, transform, N`
    /// turns the monitor an earlier line named `NAME` -- exactly, as
    /// Hyprland compares it -- by adding that rule again with the new
    /// transform, which wins because a later rule does. A name no earlier
    /// line gave does nothing and is no error. Both are
    /// `CConfigManager::handleMonitor`'s.
    #[must_use]
    pub fn read_all<'a>(
        values: impl IntoIterator<Item = &'a str>,
    ) -> (Vec<Self>, Vec<(&'a str, String)>) {
        let mut rules: Vec<Self> = Vec::new();
        let mut refused = Vec::new();
        for value in values {
            let fields: Vec<&str> = value.split(',').map(str::trim).collect();
            let read = match fields.as_slice() {
                [name, "transform", rest @ ..] => Self::turned_again(&rules, name, rest),
                [_, "addreserved", rest @ ..] => parse_reserved(rest).map(|_| None),
                _ => Self::parse(value).map(Some),
            };
            match read {
                Ok(Some(rule)) => rules.push(rule),
                Ok(None) => {}
                Err(why) => refused.push((value, why)),
            }
        }
        (rules, refused)
    }

    /// `NAME, transform, N`: the last rule named `NAME` with the transform
    /// changed, or `None` when no rule is.
    fn turned_again(rules: &[Self], name: &str, rest: &[&str]) -> Result<Option<Self>, String> {
        let transform = parse_transform(rest.first().copied().unwrap_or(""))?;
        Ok(rules
            .iter()
            .rev()
            .find(|rule| rule.name == name)
            .map(|rule| Self {
                transform,
                ..rule.clone()
            }))
    }

    /// Read a `monitor =` line's value.
    ///
    /// # Errors
    ///
    /// Hyprland's own wording for a field it cannot read, and a sentence for
    /// the fields this compositor does not do yet.
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut fields = value.split(',').map(str::trim);
        let name = fields.next().unwrap_or("").to_owned();
        let resolution = fields.next().unwrap_or("preferred");
        if resolution == "transform" {
            return Err(format!(
                "monitor: `{name}, transform, N` turns an earlier rule's monitor and is read with \
                 the other lines"
            ));
        }
        if resolution.eq_ignore_ascii_case("disable") || resolution.eq_ignore_ascii_case("disabled")
        {
            return Ok(Self {
                name,
                disabled: true,
                mode: Mode::Preferred,
                position: Position::Auto,
                scale: Scale::Auto,
                transform: Transform::Normal,
            });
        }
        let mode = parse_mode(resolution)?;
        let position = parse_position(fields.next().unwrap_or("auto"))?;
        let scale = parse_scale(fields.next().unwrap_or("auto"))?;
        // After the four come keyword pairs in any order, which is how
        // `handleMonitor` reads them: a keyword, then its value, and a
        // later pair of the same keyword over an earlier one.
        let mut transform = Transform::Normal;
        let rest: Vec<&str> = fields.filter(|field| !field.is_empty()).collect();
        for pair in rest.chunks(2) {
            match pair {
                ["transform", value] => transform = parse_transform(value)?,
                ["transform"] => return Err("invalid transform".to_owned()),
                [keyword, ..] if NOT_DONE.contains(keyword) => {
                    return Err(format!(
                        "monitor: {} is not done yet; the name, resolution, position, scale \
                         and transform are",
                        pair.join(", ")
                    ));
                }
                // Hyprland's own words for a field that is no keyword of
                // its.
                [other, ..] => return Err(format!("invalid syntax at \"{other}\"")),
                [] => {}
            }
        }
        Ok(Self {
            name,
            disabled: false,
            mode,
            position,
            scale,
            transform,
        })
    }

    /// Whether this rule is the one for a monitor called `name` that
    /// describes itself as `description`.
    ///
    /// Three forms, which are `CMonitor::matchesStaticSelector`'s: a rule
    /// with no name at all is every monitor's, a rule beginning `desc:`
    /// matches the *start* of the description, and any other name is the
    /// connector's, exactly.
    ///
    /// The prefix is Hyprland's and it matters: a description is the make,
    /// the model and the serial, and `desc:Dell Inc. DELL P2418D` names
    /// every Dell P2418D on the machine while the serial after it names
    /// one.
    #[must_use]
    pub fn matches(&self, name: &str, description: &str) -> bool {
        names(&self.name, name, description)
    }

    /// The rule of `rules`, in the order the file has them, that is the one
    /// for a monitor called `name` that describes itself as `description`:
    /// the last that names it, by its connector or by `desc:`; or, where none
    /// does, the last with no name at all.
    ///
    /// Hyprland's `CMonitorRuleManager::get`: it walks the rules backwards
    /// for one that `matchesStaticSelector` -- which a rule with no name never
    /// is -- and only then falls back to the rule with no name, of which it
    /// keeps the last written, since adding a rule drops any other of the same
    /// name. So `monitor = desc:…` above a catch-all `monitor = , …` is still
    /// that monitor's rule, as a configuration written for several monitors
    /// has it.
    #[must_use]
    pub fn for_monitor<'a>(rules: &'a [Self], name: &str, description: &str) -> Option<&'a Self> {
        rules
            .iter()
            .rev()
            .find(|rule| !rule.name.is_empty() && rule.matches(name, description))
            .or_else(|| rules.iter().rev().find(|rule| rule.name.is_empty()))
    }

    /// The scale as a number: `auto` is 1, and a scale at or below zero is
    /// not one, so it is 1 as well.
    #[must_use]
    pub fn scale_factor(&self) -> f64 {
        match self.scale {
            Scale::Auto => 1.0,
            Scale::Fixed(scale) if scale > 0.0 => scale,
            Scale::Fixed(_) => 1.0,
        }
    }
}

/// The resolution field, which is a size or a word.
fn parse_mode(text: &str) -> Result<Mode, String> {
    // Hyprland's words for "let the connector decide", and the two that
    // choose among the modes it lists (`CMonitor::applyMonitorRule`).
    match text.to_ascii_lowercase().as_str() {
        "" | "preferred" | "maxwidth" => return Ok(Mode::Preferred),
        "highres" => return Ok(Mode::HighRes),
        "highrr" => return Ok(Mode::HighRr),
        _ => {}
    }
    let (size, refresh) = match text.split_once('@') {
        Some((size, rate)) => {
            let rate: f64 = rate
                .trim()
                .parse()
                .map_err(|_| format!("Invalid refresh rate {rate}"))?;
            (size, Some(rate))
        }
        None => (text, None),
    };
    let (width, height) = size
        .split_once('x')
        .ok_or_else(|| format!("Invalid resolution {text}"))?;
    let width: u32 = width
        .trim()
        .parse()
        .map_err(|_| format!("Invalid resolution {text}"))?;
    let height: u32 = height
        .trim()
        .parse()
        .map_err(|_| format!("Invalid resolution {text}"))?;
    Ok(Mode::Fixed {
        width,
        height,
        refresh,
    })
}

/// The position field.
fn parse_position(text: &str) -> Result<Position, String> {
    let lower = text.to_ascii_lowercase();
    if lower.is_empty() || lower == "auto" {
        return Ok(Position::Auto);
    }
    if lower.starts_with("auto-") {
        return Err(format!(
            "monitor: {text} is not done yet; `auto` puts a monitor to the right of the last"
        ));
    }
    let (x, y) = text
        .split_once('x')
        .ok_or_else(|| format!("Invalid position {text}"))?;
    let x: i64 = x
        .trim()
        .parse()
        .map_err(|_| format!("Invalid position {text}"))?;
    let y: i64 = y
        .trim()
        .parse()
        .map_err(|_| format!("Invalid position {text}"))?;
    Ok(Position::At(x, y))
}

/// The value after `transform`: a whole number from 0 to 7, which is
/// Hyprland's `CMonitorRuleParser::parseTransform` and its words for one that
/// is not.
fn parse_transform(text: &str) -> Result<Transform, String> {
    let value: i64 = text
        .trim()
        .parse()
        .map_err(|_| format!("invalid transform {text}"))?;
    u32::try_from(value)
        .ok()
        .and_then(Transform::from_value)
        .ok_or_else(|| format!("invalid transform {text}"))
}

/// The scale field.
fn parse_scale(text: &str) -> Result<Scale, String> {
    let lower = text.to_ascii_lowercase();
    if lower.is_empty() || lower == "auto" {
        return Ok(Scale::Auto);
    }
    let scale: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("Invalid scale {text}"))?;
    // A scale that is zero, negative or not a number at all is not one; a
    // comparison that says so without a negation reads the same and keeps
    // NaN on the refusing side.
    if scale <= 0.0 || scale.is_nan() {
        return Err(format!("Invalid scale {text}"));
    }
    Ok(Scale::Fixed(scale))
}
