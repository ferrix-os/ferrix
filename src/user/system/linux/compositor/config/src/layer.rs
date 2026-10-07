//! `layerrule = <effect> <value>, match:namespace <namespace>`: what a bar,
//! a wallpaper or a notification is drawn with.
//!
//! The `zwlr_layer_shell_v1` half of `windowrule`, and in Hyprland 0.56 the
//! same engine reading the same grammar: a list of comma-separated fields,
//! each a name and a value with a space between them, where a field whose
//! name begins `match:` is something the surface must be and every other
//! field is something to do to it
//! (`CConfigManager::handleLayerrule`):
//!
//! ```text
//! layerrule = blur true, match:namespace waybar
//! layerrule = ignore_alpha 0.5, match:namespace waybar
//! layerrule = order 10, match:namespace notifications
//! ```
//!
//! A layer surface has no title and no application id -- it has a
//! *namespace*, which is what it passed to `get_layer_surface`, and that is
//! the only property Hyprland's `CLayerRule::matches` looks at. Every other
//! `match:` a person writes is read and skipped, which is what Hyprland
//! does with it; refusing them here would cost a person a line Hyprland
//! accepts.
//!
//! The namespace is matched as a regular expression, as a window's class
//! is, so `^(waybar)$` and `waybar` both work and mean different things.
//!
//! # What is drawn and what is only recorded
//!
//! `blur` is drawn: `src/user/system/linux/compositor/render` blurs what is behind a translucent
//! layer surface, which is what makes a bar look like Hyprland's.
//! `above_lock` is drawn: the surface goes over the session lock, which is
//! the whole reason an on-screen keyboard can be used on a lock screen.
//! `order` is obeyed: it decides where a surface goes among its own
//! layer's.
//!
//! `dim_around` is drawn: everything already on the canvas is darkened by
//! `decoration:dim_around` just before the surface goes on top of it, which
//! is what a launcher does to the desktop. Drawing in order means "behind"
//! is "already drawn", so one fill in the right place is the whole of it.
//!
//! `xray` is acted on: the blur behind such a surface is taken from
//! `compositor_render::Backdrop` -- everything behind the windows, kept
//! and blurred, which is Hyprland's `m_blurFB` -- rather than from the
//! frame as it stands. It needed no second pass in the end, because the
//! backdrop the blur optimisation already keeps *is* the picture the rule
//! asks for. Only above the windows: a surface drawn below them is part of
//! what the backdrop is a copy of.
//!
//! `blur_popups` is drawn: the surface's popups -- a bar's tooltip, a tray
//! icon's menu -- are blurred behind as `blur` blurs the surface, with its
//! `xray`. Popups are drawn last, over everything, so drawing in order is
//! the second pass this was once thought to need. `no_screen_share` is
//! obeyed: a screenshot or a shared screen shows a black box where the
//! surface and its popups are, drawn over the copy as Hyprland's
//! `CScreenshareFrame::renderMonitor` does, and the screen itself is drawn
//! as ever.
//!
//! The rest are read, kept and not acted on. `no_anim` has nothing to turn
//! off, because a layer surface is not animated here; `ignore_alpha` and
//! `animation` likewise. They are parsed rather than refused so that a
//! person's configuration is not a wall of diagnostics, and recorded so
//! that the compositor can act on them when it can.

use compositor_regex::Regex;

/// One `layerrule` line.
#[derive(Clone, Debug)]
pub struct LayerRule {
    /// The namespace it matches. A line with no `match:namespace` field
    /// applies to every layer surface, which is what Hyprland's engine
    /// does with a rule that registered no match for that property.
    pub namespace: Option<Regex>,
    /// What it does, in the order the line gave them. One line may carry
    /// several effects, as `blur true, ignore_alpha 0.2` does.
    pub effects: Vec<LayerEffect>,
}

/// What a `layerrule` asks for.
#[derive(Clone, PartialEq, Debug)]
pub enum LayerEffect {
    /// `blur <yes-or-no>`: what is behind the surface is blurred, which is
    /// what makes a bar with a translucent background look like
    /// Hyprland's.
    Blur(bool),
    /// `blur_popups <yes-or-no>`: the same for the popups it opens.
    BlurPopups(bool),
    /// `ignore_alpha <fraction>`: a pixel less opaque than this is left out
    /// of what the blur is drawn under.
    IgnoreAlpha(f32),
    /// `dim_around <yes-or-no>`: everything else is dimmed while it is up,
    /// which is what a launcher does.
    DimAround(bool),
    /// `xray <yes-or-no>`: the blur reads the wallpaper rather than what is
    /// under it.
    Xray(bool),
    /// `no_anim <yes-or-no>`: it does not animate.
    NoAnim(bool),
    /// `order <n>`: where it goes among the surfaces of its own layer, a
    /// higher number nearer the top.
    Order(i64),
    /// `above_lock <0-2>`: it is drawn over the session lock, which is what
    /// an on-screen keyboard needs to be usable on a lock screen. Two
    /// means it takes input there as well.
    AboveLock(i64),
    /// `no_screen_share <yes-or-no>`: it is left out of a screenshot.
    NoScreenShare(bool),
    /// `animation <style>`: which animation it uses, recorded as written.
    Animation(String),
}

/// Hyprland's `truthy`: `1`, or a word beginning `true`, `yes` or `on`.
fn truthy(value: &str) -> bool {
    if value == "1" {
        return true;
    }
    let lowered = value.to_lowercase();
    ["true", "yes", "on"]
        .iter()
        .any(|word| lowered.starts_with(word))
}

impl LayerRule {
    /// Read one `layerrule =` line's value.
    ///
    /// # Errors
    ///
    /// Hyprland's own wording for a field it cannot read, which the caller
    /// reports as a diagnostic and carries on -- one bad line must not cost
    /// a person their whole configuration.
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut namespace = None;
        let mut effects = Vec::new();
        for field in value.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            let (name, rest) = field
                .split_once(' ')
                .ok_or_else(|| format!("invalid field {field}: missing a value"))?;
            let rest = rest.trim();
            match name.strip_prefix("match:") {
                Some("namespace") => {
                    namespace = Some(
                        Regex::new(rest).map_err(|why| format!("invalid prop namespace: {why}"))?,
                    );
                }
                // Every other property Hyprland knows is accepted and
                // skipped, because that is what `CLayerRule::matches` does
                // with one: a layer surface has only a namespace.
                Some(other) if crate::rule::is_prop(other) => {}
                Some(other) => return Err(format!("invalid prop {other}")),
                None => effects.push(Self::effect(name, rest)?),
            }
        }
        if effects.is_empty() {
            return Err(format!("layerrule: `{value}` does nothing"));
        }
        Ok(Self { namespace, effects })
    }

    /// One effect field.
    fn effect(name: &str, value: &str) -> Result<LayerEffect, String> {
        let number = |what: &str| -> Result<i64, String> {
            value
                .parse()
                .map_err(|_| format!("invalid field {name}: `{value}` is not {what}"))
        };
        match name {
            "blur" => Ok(LayerEffect::Blur(truthy(value))),
            "blur_popups" => Ok(LayerEffect::BlurPopups(truthy(value))),
            "dim_around" => Ok(LayerEffect::DimAround(truthy(value))),
            "xray" => Ok(LayerEffect::Xray(truthy(value))),
            "no_anim" => Ok(LayerEffect::NoAnim(truthy(value))),
            "no_screen_share" => Ok(LayerEffect::NoScreenShare(truthy(value))),
            "ignore_alpha" => {
                let fraction: f32 = value
                    .parse()
                    .map_err(|_| format!("invalid field ignore_alpha: `{value}` is not a share"))?;
                Ok(LayerEffect::IgnoreAlpha(fraction.clamp(0.0, 1.0)))
            }
            "order" => number("a number").map(LayerEffect::Order),
            // Hyprland clamps this to 0..2 rather than refusing it.
            "above_lock" => {
                number("a number").map(|level| LayerEffect::AboveLock(level.clamp(0, 2)))
            }
            "animation" => Ok(LayerEffect::Animation(value.to_owned())),
            other => Err(format!("invalid field type {other}")),
        }
    }

    /// Whether this rule is about a surface with that namespace.
    #[must_use]
    pub fn matches(&self, namespace: &str) -> bool {
        self.namespace
            .as_ref()
            .is_none_or(|pattern| pattern.matches(namespace))
    }
}

/// What every rule that matched one surface comes to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layered {
    /// Whether what is behind it is blurred.
    pub blur: bool,
    /// The same for its popups.
    pub blur_popups: bool,
    /// How opaque a pixel has to be to count, for the blur.
    pub ignore_alpha: Option<f32>,
    /// Whether everything else is dimmed while it is up.
    pub dim_around: bool,
    /// Whether the blur reads the wallpaper rather than what is under it.
    pub xray: bool,
    /// Whether it animates.
    pub animates: bool,
    /// Where it goes among its own layer's surfaces.
    pub order: i64,
    /// Whether it is drawn over the session lock.
    pub above_lock: bool,
    /// Whether a screenshot leaves it out.
    pub no_screen_share: bool,
    /// Which animation it uses.
    pub animation: String,
}

impl Layered {
    /// What `rules` say about a surface called `namespace`.
    ///
    /// Later lines win, as they do everywhere else in the configuration.
    #[must_use]
    pub fn of(rules: &[LayerRule], namespace: &str) -> Self {
        let mut out = Self {
            animates: true,
            ..Self::default()
        };
        for rule in rules.iter().filter(|rule| rule.matches(namespace)) {
            for effect in &rule.effects {
                match effect {
                    LayerEffect::Blur(on) => out.blur = *on,
                    LayerEffect::BlurPopups(on) => out.blur_popups = *on,
                    LayerEffect::IgnoreAlpha(fraction) => out.ignore_alpha = Some(*fraction),
                    LayerEffect::DimAround(on) => out.dim_around = *on,
                    LayerEffect::Xray(on) => out.xray = *on,
                    LayerEffect::NoAnim(on) => out.animates = !*on,
                    LayerEffect::Order(order) => out.order = *order,
                    LayerEffect::AboveLock(level) => out.above_lock = *level > 0,
                    LayerEffect::NoScreenShare(on) => out.no_screen_share = *on,
                    LayerEffect::Animation(style) => out.animation.clone_from(style),
                }
            }
        }
        out
    }
}
