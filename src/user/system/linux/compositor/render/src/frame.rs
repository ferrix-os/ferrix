//! A monitor's frame, drawn from the layout's answer and the clients'
//! buffers.

use std::collections::{BTreeMap, BTreeSet};

use compositor_config::Config;
use compositor_layout::{MonitorLayout, Placed, WindowId};

use crate::damage::intersect;
use crate::timing::{Phase, Timer, timed};
use crate::{
    Backdrop, Blur, Canvas, Color, Damage, Format, Gradient, Painter, Rect, Rounding, Shadow,
    Surface,
};

/// The colours a frame is drawn in, and the decorations it is drawn with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    /// What shows where no window is: Hyprland's `misc:background_color`.
    pub background: Color,
    /// The focused window's border: `general:col.active_border`, whole.
    ///
    /// A gradient, because Hyprland's is: `col.active_border = rgba(33ccffee)
    /// rgba(00ff99ee) 45deg` is two colours running across the window, and a
    /// compositor that took the first of them would draw a configuration
    /// nobody wrote.
    pub active_border: Gradient,
    /// Every other window's border: `general:col.inactive_border`.
    pub inactive_border: Gradient,
    /// `decoration:rounding` and `decoration:rounding_power`: how far a
    /// window's corners are cut and by what curve. Zero is a square
    /// window, which is Hyprland's default.
    pub rounding: Rounding,
    /// `decoration:active_opacity`: how much of the focused window shows.
    pub active_opacity: f32,
    /// `decoration:inactive_opacity`: the same for every other window.
    pub inactive_opacity: f32,
    /// `decoration:fullscreen_opacity`: the same for a fullscreen one, which
    /// Hyprland keeps apart because a translucent fullscreen window shows
    /// the background and nothing else.
    pub fullscreen_opacity: f32,
    /// `decoration:shadow:*`, or `None` when `shadow:enabled` is off.
    pub shadow: Option<Shadow>,
    /// `decoration:dim_strength` when `decoration:dim_inactive` is on: how
    /// much black is laid over a window that is not focused. Zero when it is
    /// off.
    pub dim: f32,
    /// `decoration:dim_around`: how much black is laid over everything
    /// *behind* a window or layer surface whose rule asked for it.
    pub dim_around: f32,
    /// `decoration:blur:*`, or `None` when `blur:enabled` is off: how much
    /// of what is behind a translucent window is blurred, how many times,
    /// and the colour grading over it.
    pub blur: Option<Blur>,
    /// `decoration:blur:xray`: every window takes its blur from what is
    /// behind the windows -- the wallpaper and the layers under them --
    /// rather than from the windows it floats over. Off by default, as in
    /// Hyprland; a window's own `xray` rule says otherwise for that one.
    pub xray: bool,
}

impl Style {
    /// Hyprland's `misc:background_color` default.
    pub const BACKGROUND: Color = Color(0xFF11_1111);

    /// The same style on a monitor at `scale`: every length in buffer
    /// pixels rather than logical ones.
    ///
    /// Hyprland scales its decorations by the monitor's scale -- a rounding
    /// of 12 on a screen at `scale = 2` cuts 24 buffer pixels -- and a
    /// renderer that scaled the windows and not the decorations would draw
    /// a hairline border round a doubled window.
    #[must_use]
    pub fn at_scale(&self, scale: f64) -> Self {
        if (scale - 1.0).abs() < f64::EPSILON {
            return *self;
        }
        let grow = |value: i64| -> i64 {
            #[expect(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                reason = "a decoration's pixels are far inside f64's exact range"
            )]
            let scaled = (value as f64 * scale).round() as i64;
            scaled
        };
        let scaled = |rounding: Rounding| Rounding {
            radius: grow(rounding.radius),
            ..rounding
        };
        Self {
            rounding: scaled(self.rounding),
            shadow: self.shadow.map(|shadow| Shadow {
                rounding: scaled(shadow.rounding),
                range: grow(shadow.range),
                offset: (grow(shadow.offset.0), grow(shadow.offset.1)),
                ..shadow
            }),
            blur: self.blur.map(|blur| Blur {
                size: grow(blur.size),
                ..blur
            }),
            ..*self
        }
    }

    /// The colours and the decorations `config` gives.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        // `decoration:shadow:color` is a gradient option in Hyprland too,
        // because every colour option is; the shadow shader takes one
        // colour, so this takes the first of them, as Hyprland's
        // `CHyprDropShadowDecoration` does.
        let first = |name: &str, default: u32| {
            config
                .gradient(name)
                .and_then(|gradient| gradient.colors.first().copied())
                .unwrap_or(Color(default))
        };
        let border = |name: &str, default: u32| {
            config
                .gradient(name)
                .map_or_else(|| Gradient::solid(Color(default)), Gradient::from)
        };
        let opacity = |name: &str| {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "an opacity is between zero and one; `f32` holds it"
            )]
            let value = config.float(name).unwrap_or(1.0) as f32;
            value.clamp(0.0, 1.0)
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a rounding power is between one and ten; `f32` holds it"
        )]
        let power = config
            .float("decoration:rounding_power")
            .unwrap_or(f64::from(Rounding::POWER)) as f32;
        let rounding = Rounding {
            radius: config.int("decoration:rounding").unwrap_or(0).max(0),
            power: power.clamp(1.0, 10.0),
        };
        Self {
            // Hyprland stores a colour as an integer and reads this one
            // as a colour rather than a gradient, so the first stop of
            // whatever was written is what it is.
            background: config
                .gradient("misc:background_color")
                .and_then(|written| written.colors.first().copied())
                .unwrap_or(Self::BACKGROUND),
            active_border: border("general:col.active_border", 0xFFFF_FFFF),
            inactive_border: border("general:col.inactive_border", 0xFF44_4444),
            rounding,
            active_opacity: opacity("decoration:active_opacity"),
            inactive_opacity: opacity("decoration:inactive_opacity"),
            fullscreen_opacity: opacity("decoration:fullscreen_opacity"),
            shadow: config
                .bool("decoration:shadow:enabled")
                .unwrap_or(true)
                .then(|| Shadow {
                    rounding,
                    range: config.int("decoration:shadow:range").unwrap_or(4).max(0),
                    power: u32::try_from(config.int("decoration:shadow:render_power").unwrap_or(3))
                        .unwrap_or(3),
                    color: first("decoration:shadow:color", 0xEE1A_1A1A),
                    offset: offset(config.str("decoration:shadow:offset").unwrap_or("")),
                }),
            dim: if config.bool("decoration:dim_inactive").unwrap_or(false) {
                opacity("decoration:dim_strength")
            } else {
                0.0
            },
            // Not gated on `dim_inactive`: this one is a *rule*'s, and the
            // option is only how strong it is. Hyprland's default is 0.4.
            dim_around: {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a share between zero and one; `f32` holds it"
                )]
                let value = config.float("decoration:dim_around").unwrap_or(0.4) as f32;
                value.clamp(0.0, 1.0)
            },
            blur: config
                .bool("decoration:blur:enabled")
                .unwrap_or(true)
                .then(|| blur_of(config)),
            xray: config.bool("decoration:blur:xray").unwrap_or(false),
        }
    }

    /// How much of a window shows, by what it is.
    #[must_use]
    pub const fn opacity(&self, focused: bool, fullscreen: bool) -> f32 {
        if fullscreen {
            self.fullscreen_opacity
        } else if focused {
            self.active_opacity
        } else {
            self.inactive_opacity
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

impl Style {
    /// The same style with the blur's dither off.
    ///
    /// For the expected images, and for nothing else. They are run-length
    /// encoded, and a dither that moves every pixel by a step or two is the
    /// one thing a run of pixels cannot survive: blessed from Hyprland's
    /// own `decoration:blur:noise` the committed images grow from one
    /// megabyte to eight and a half, and the format's reason for existing
    /// goes with them.
    ///
    /// What is given up is a picture *of* a dither of about one part in
    /// 170 in most of the images. What is kept is the dither itself,
    /// exactly as `blurFinish.glsl` computes it, drawn for any
    /// configuration that asks for one and held by an image of its own.
    #[must_use]
    pub fn undithered(mut self) -> Self {
        if let Some(blur) = self.blur.as_mut() {
            blur.noise = 0.0;
        }
        self
    }
}

impl Eq for Style {}

/// `decoration:blur:*`: the shape of the blur and the five values that
/// grade it.
///
/// Each grading value falls back to Hyprland's own default rather than to
/// the one that does nothing, because Hyprland's defaults are not nothing --
/// `contrast` is 0.8916 and `vibrancy` 0.1696 out of the box -- so a
/// configuration that turns the blur on and says no more asks for a graded
/// blur, dither and all.
///
/// Each is clamped to the range Hyprland's `ConfigValues.cpp` gives it, so
/// a value outside it is the nearest one inside rather than a picture
/// nobody has seen.
fn blur_of(config: &Config) -> Blur {
    let graded = |name: &str, default: f32, top: f32| {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a grading value between zero and two; `f32` holds it"
        )]
        let value = config
            .float(name)
            .map_or(default, |value| value as f32)
            .clamp(0.0, top);
        value
    };
    Blur {
        size: config.int("decoration:blur:size").unwrap_or(8).max(0),
        passes: u32::try_from(config.int("decoration:blur:passes").unwrap_or(1)).unwrap_or(1),
        noise: graded("decoration:blur:noise", Blur::NOISE, 1.0),
        contrast: graded("decoration:blur:contrast", Blur::CONTRAST, 2.0),
        brightness: graded("decoration:blur:brightness", Blur::BRIGHTNESS, 2.0),
        vibrancy: graded("decoration:blur:vibrancy", Blur::VIBRANCY, 1.0),
        vibrancy_darkness: graded(
            "decoration:blur:vibrancy_darkness",
            Blur::VIBRANCY_DARKNESS,
            1.0,
        ),
    }
}

/// `shadow:offset`, which Hyprland reads as a vector: two numbers separated
/// by a space or a comma. Anything else is no offset, which is its default.
fn offset(text: &str) -> (i64, i64) {
    let mut parts = text
        .split([' ', ','])
        .map(str::trim)
        .filter(|part| !part.is_empty());
    let number = |part: Option<&str>| {
        part.and_then(|text| text.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .map_or(0, |value| {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "Hyprland clamps the offset to ±250; an i64 holds it"
                )]
                let whole = value.round() as i64;
                whole.clamp(-250, 250)
            })
    };
    (number(parts.next()), number(parts.next()))
}

/// A window's client rectangle with its border around it, in the layout's
/// global coordinates.
#[must_use]
pub fn outer(placed: &Placed) -> Rect {
    let border = placed.border.max(0);
    Rect::new(
        placed.rect.x.saturating_sub(border),
        placed.rect.y.saturating_sub(border),
        placed.rect.width.saturating_add(border.saturating_mul(2)),
        placed.rect.height.saturating_add(border.saturating_mul(2)),
    )
}

/// One layer surface to draw: where it is, whether it is above the windows,
/// and its pixels.
///
/// The compositor works out the rectangle from the protocol's anchor rules
/// (`compositor_layout::layers`); this crate only draws it.
#[derive(Debug)]
pub struct LayerFrame<'pixels> {
    /// Where it is, in the layout's global coordinates.
    pub rect: Rect,
    /// Whether it is drawn above the windows: `top` and `overlay` are,
    /// `background` and `bottom` are not.
    pub above: bool,
    /// Its pixels, or `None` for one that has not drawn yet.
    pub surface: Option<Surface<'pixels>>,
    /// Whether everything behind it is darkened while it is up:
    /// `layerrule = dim_around`. What a launcher does to the desktop.
    pub dim_around: bool,
    /// Whether what is behind it is blurred: `layerrule = blur, waybar`.
    ///
    /// This is what makes a bar with a translucent background look like
    /// Hyprland's, and it is a rule rather than the default because
    /// blurring behind an opaque bar costs a pyramid of passes and changes
    /// not one pixel.
    pub blur: bool,
    /// Whether that blur is of the *wallpaper* rather than of what is
    /// directly behind it: `layerrule = xray`.
    ///
    /// A bar over a window blurs the window, and a person who wants the
    /// desktop showing through their bar however many windows are under it
    /// asks for this. It is the same picture a tiled window's blur is taken
    /// from -- everything behind the windows, kept and blurred -- so it
    /// costs nothing a frame was not paying already, and a bar that reads it
    /// is one the windows moving underneath no longer redraws.
    ///
    /// Only read for a surface drawn *above* the windows. One drawn below
    /// them is part of what the backdrop is a copy of, and a blur of the
    /// backdrop there would be a blur of itself.
    pub xray: bool,
}

/// What one window is drawn with, where a `windowrule` asked for something
/// other than the style every window has.
///
/// Hyprland's rules change a single window's decorations -- `opacity 0.8`,
/// `rounding 0`, `no_blur` -- and a compositor that read those and drew
/// every window the same would be one whose rules do nothing. Each field is
/// "as the style says" until a rule fills it in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowStyle {
    /// `opacity`: how much of the window shows, over the style's own.
    pub opacity: Option<f32>,
    /// `rounding`: how far its corners are cut.
    pub rounding: Option<i64>,
    /// `border_size`: how wide its border is.
    pub border: Option<i64>,
    /// `no_blur`: whether what is behind it is blurred.
    pub blur: bool,
    /// `no_shadow`.
    pub shadow: bool,
    /// `no_dim`: whether it is dimmed when it is not focused.
    pub dim: bool,
    /// `rounding_power`: the curve its corners are cut by.
    pub rounding_power: Option<f32>,
    /// `border_color`: its border, instead of the focused and unfocused
    /// ones. Hyprland's rule takes one or two gradients -- the second for
    /// the unfocused state -- and this carries the first.
    pub border_color: Option<Gradient>,
    /// `decorate`: whether it is drawn with a border and a shadow at all.
    /// `decorate false` is what a person writes for a window that draws
    /// its own frame.
    pub decorate: bool,
    /// `opaque`: it is drawn as if every pixel were opaque, whatever its
    /// buffer's alpha says. Hyprland's rule for a client that leaves
    /// rubbish in its alpha channel, and it also takes the blur out of the
    /// frame, since there is nothing to see behind an opaque window.
    pub opaque: bool,
    /// `nearest_neighbor`: a stretched window is sampled nearest rather
    /// than bilinear, which is what a person writes for pixel art -- a
    /// sprite must stay a sprite and not become a smear.
    pub nearest: bool,
    /// `dim_around`: everything behind this window is darkened by
    /// `decoration:dim_around` while it is up, which is what a launcher or
    /// a confirmation dialog does to the desktop behind it.
    pub dim_around: bool,
    /// `xray`: `Some(true)` takes the blur behind this window from the
    /// [`Backdrop`] -- the wallpaper, whatever windows it is over --
    /// `Some(false)` never does, and `None` leaves it to
    /// `decoration:blur:xray` and the tiling, as [`reads_backdrop`] says.
    pub xray: Option<bool>,
    /// `no_screen_share`: a screenshot shows a black box where this window
    /// is, rounded as it is. The screen itself shows it as ever.
    pub no_screen_share: bool,
}

impl Default for WindowStyle {
    /// Everything as the style says, which is what a window with no rule
    /// gets.
    fn default() -> Self {
        Self {
            opacity: None,
            rounding: None,
            border: None,
            blur: true,
            shadow: true,
            dim: true,
            rounding_power: None,
            border_color: None,
            decorate: true,
            opaque: false,
            nearest: false,
            dim_around: false,
            xray: None,
            no_screen_share: false,
        }
    }
}

/// The style a frame is drawn with: the one every window has, and the
/// windows a rule gave something else.
#[derive(Clone, Copy, Debug)]
pub struct Styles<'a> {
    /// What every window is drawn with.
    pub base: &'a Style,
    /// What a rule changed, by window.
    pub windows: &'a BTreeMap<WindowId, WindowStyle>,
}

impl<'a> Styles<'a> {
    /// A style with no window given anything of its own.
    #[must_use]
    pub fn plain(base: &'a Style) -> Self {
        Self {
            base,
            windows: Self::none(),
        }
    }

    /// The empty map, which a style with no rules borrows.
    fn none() -> &'static BTreeMap<WindowId, WindowStyle> {
        static NONE: std::sync::OnceLock<BTreeMap<WindowId, WindowStyle>> =
            std::sync::OnceLock::new();
        NONE.get_or_init(BTreeMap::new)
    }

    /// What `window` is drawn with.
    #[must_use]
    pub fn of(&self, window: WindowId) -> WindowStyle {
        self.windows.get(&window).copied().unwrap_or_default()
    }
}

/// Draw `output`, the layout of the monitor whose top-left corner is at
/// `origin` in the layout's global coordinates, into `canvas` within
/// `damage`.
///
/// In order: the background; the layer surfaces that are under the windows;
/// each window bottom to top, its border in [`Style`]'s active colour if it
/// has focus and the inactive one if not, then its surface from `surfaces`
/// inside the border; and the layer surfaces that are above them. A window
/// or a layer surface with no pixels yet shows what is under it.
///
/// Returns the damage the frame produced, which is `damage` on the canvas,
/// since the background covers all of it.
pub fn render(
    canvas: &mut Canvas,
    output: &MonitorLayout,
    origin: (i64, i64),
    style: &Style,
    surfaces: &BTreeMap<WindowId, Surface<'_>>,
    damage: &Damage,
) -> Damage {
    render_with_layers(
        canvas,
        output,
        origin,
        &Styles::plain(style),
        surfaces,
        &[],
        damage,
    )
}

/// A monitor's layout in the buffer pixels a scaled screen draws.
///
/// Hyprland lays a scaled monitor out in logical pixels -- a 1024x768 screen
/// at `scale = 2` tiles its windows in 512x384 -- and draws every one of
/// them as `scale` buffer pixels. Everything above the renderer therefore
/// works in logical pixels, and this is where they become the screen's own:
/// each rectangle grows away from the monitor's corner, so a window at the
/// monitor's top left stays there.
///
/// The decorations scale with it, which is what [`Style::at_scale`] is for:
/// a border one logical pixel wide is two buffer pixels at `scale = 2`, as
/// it is in Hyprland.
#[must_use]
pub fn scaled(output: &MonitorLayout, origin: (i64, i64), scale: f64) -> MonitorLayout {
    if (scale - 1.0).abs() < f64::EPSILON {
        return output.clone();
    }
    let grow = |value: i64| -> i64 {
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "a screen's pixels are far inside f64's exact range, and a scaled length is                       rounded to the pixel it lands on"
        )]
        let scaled = (value as f64 * scale).round() as i64;
        scaled
    };
    let at = |value: i64, from: i64| from.saturating_add(grow(value.saturating_sub(from)));
    let mut out = output.clone();
    for placed in &mut out.windows {
        placed.rect = Rect::new(
            at(placed.rect.x, origin.0),
            at(placed.rect.y, origin.1),
            grow(placed.rect.width),
            grow(placed.rect.height),
        );
        placed.border = grow(placed.border);
    }
    out
}

/// [`render`], with the layer surfaces `zwlr_layer_shell_v1` put on the
/// monitor.
pub fn render_with_layers(
    canvas: &mut Canvas,
    output: &MonitorLayout,
    origin: (i64, i64),
    styles: &Styles<'_>,
    surfaces: &BTreeMap<WindowId, Surface<'_>>,
    layers: &[LayerFrame<'_>],
    damage: &Damage,
) -> Damage {
    // With a backdrop of its own, made for the one frame: a tiled window
    // takes its blur from what is behind the windows whoever draws it, so
    // that a frame drawn here from nothing and the frame a compositor has
    // kept up to date with [`render_onto`] are one picture. Only where a
    // blur is asked for, since a backdrop is two canvases.
    let mut behind = styles
        .base
        .blur
        .and_then(|_| Backdrop::new(canvas.width(), canvas.height()).ok());
    // A backdrop made now has seen no frame, and a blur reads further than
    // the damage: so it is shown the whole of what is behind the windows,
    // drawn aside, which is what one kept from frame to frame would hold.
    if let Some(behind) = behind.as_mut()
        && let Ok(mut aside) = Canvas::new(canvas.width(), canvas.height())
    {
        let whole = Damage::from(aside.bounds());
        behind_windows(&mut aside, origin, styles.base, layers, &whole);
        behind.take(&aside, &whole);
    }
    render_onto(
        canvas,
        behind.as_mut(),
        output,
        origin,
        styles,
        surfaces,
        layers,
        damage,
    )
}

/// The same, with somewhere to keep the blur's backdrop.
///
/// `backdrop` is a [`Backdrop`]: everything *behind* the windows, kept
/// between frames, and the blur of it. A window [`reads_backdrop`] says yes
/// to takes its blur from there, which costs a copy; every other blurred
/// surface blurs the frame as it stands, as all of them do without one.
///
/// The two are not the same picture, and the backdrop's is Hyprland's. A
/// blur of the frame as it stands has the window's own shadow in it --
/// the shadow is drawn whole, under the window
/// (`CHyprDropShadowDecoration::render`) -- and whatever of its neighbour
/// is within the kernel's reach; `m_blurFB` has neither, and is drawn over
/// both. So [`render_with_layers`] makes a backdrop for the frame it draws
/// rather than passing none.
///
/// A blur of the frame as it stands reads a kernel's reach outside what it
/// writes, and outside the damage the canvas holds the *last* frame with the
/// surface drawn over its own blur. So the caller owes such a surface a
/// whole redraw whenever its damage touches it, and owes a window that
/// reads the backdrop only the damage -- grown by [`Blur::reach`] where
/// what is *behind* the windows changed, since that is how far a changed
/// pixel shows in the blur.
#[expect(
    clippy::too_many_arguments,
    reason = "a frame is its canvas, its backdrop, its layout, its styles, its pixels and its damage"
)]
pub fn render_onto<P: Painter>(
    canvas: &mut P,
    mut backdrop: Option<&mut P::Backdrop>,
    output: &MonitorLayout,
    origin: (i64, i64),
    styles: &Styles<'_>,
    surfaces: &BTreeMap<WindowId, Surface<'_>>,
    layers: &[LayerFrame<'_>],
    damage: &Damage,
) -> Damage {
    let style = styles.base;
    let local = |rect: Rect| rect.translate(origin.0.saturating_neg(), origin.1.saturating_neg());
    timed(Phase::Behind, || {
        behind_windows(canvas, origin, style, layers, damage);
    });
    // Everything behind the windows is drawn; that is the backdrop, and it
    // is taken now, before a window goes over it.
    if let Some(behind) = backdrop.as_deref_mut() {
        timed(Phase::Backdrop, || canvas.keep_backdrop(behind, damage));
    }
    for (at, placed) in output.windows.iter().enumerate() {
        if styles.of(placed.window).dim_around {
            dim_behind(canvas, style, damage);
        }
        window(
            canvas,
            backdrop
                .as_deref_mut()
                .filter(|_| reads_backdrop(&output.windows, at, styles)),
            placed,
            local(placed.rect),
            styles,
            surfaces,
            damage,
        );
    }
    for layer in layers.iter().filter(|layer| layer.above) {
        if layer.dim_around {
            dim_behind(canvas, style, damage);
        }
        timed(Phase::Over, || {
            draw_layer(
                canvas,
                backdrop.as_deref_mut().filter(|_| layer.xray),
                layer,
                local(layer.rect),
                style,
                damage,
            );
        });
    }
    damage.clipped(canvas.bounds())
}

/// Everything behind the windows: the background, and the layer surfaces
/// under them. What a [`Backdrop`] is a copy of.
fn behind_windows<P: Painter>(
    canvas: &mut P,
    origin: (i64, i64),
    style: &Style,
    layers: &[LayerFrame<'_>],
    damage: &Damage,
) {
    canvas.clear(style.background, damage);
    for layer in layers.iter().filter(|layer| !layer.above) {
        if layer.dim_around {
            dim_behind(canvas, style, damage);
        }
        let rect = layer
            .rect
            .translate(origin.0.saturating_neg(), origin.1.saturating_neg());
        // No backdrop: this surface is part of what the backdrop is a copy
        // of, so `xray` here would be a blur of itself.
        draw_layer(canvas, None, layer, rect, style, damage);
    }
}

/// `dim_around`: black over everything drawn *so far*, laid down just
/// before the window or surface that asked for it. Hyprland dims what is
/// behind such a thing; drawing in order means "behind" is "already drawn",
/// so one fill in the right place is the whole of it -- no second pass and
/// no second canvas.
fn dim_behind<P: Painter>(canvas: &mut P, style: &Style, damage: &Damage) {
    if style.dim_around <= 0.0 {
        return;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a share between zero and one becomes a byte of alpha"
    )]
    let alpha = (style.dim_around.clamp(0.0, 1.0) * 255.0).round() as u32;
    canvas.fill(canvas.bounds(), Color(alpha << 24), damage);
}

/// Whether the blur behind `windows[at]` is taken from the [`Backdrop`]
/// rather than from the frame as it stands.
///
/// Hyprland's rule is `IHyprRenderer::shouldUseNewBlurOptimizations` in
/// `src/render/Renderer.cpp`: a window that is not floating and not on a
/// special workspace. Both of those are ways of saying *nothing but the
/// desktop is behind it*, and a layout's answer carries the first and not
/// the second, so this asks the question itself: a tiled window with no
/// window drawn under it. A scratchpad's window over the workspace it was
/// called up on has one, and blurs it, as Hyprland's does.
///
/// A window a `dim_around` rule darkened the desktop for, or one drawn after
/// such a window, is behind a fill the backdrop does not hold.
///
/// `xray` comes first, as it does there: a window's own `xray 0` never
/// reads the backdrop, and `decoration:blur:xray` or the window's `xray 1`
/// always does -- floating, over other windows or behind a dim, since what
/// it asks for is the blur of the wallpaper and not of whatever is between.
///
/// The caller's damage has to know the answer ([`render_onto`] says what
/// each kind is owed), which is why this is not the renderer's own secret.
#[must_use]
pub fn reads_backdrop(windows: &[Placed], at: usize, styles: &Styles<'_>) -> bool {
    let Some(placed) = windows.get(at) else {
        return false;
    };
    match styles.of(placed.window).xray {
        Some(own) => return own,
        None if styles.base.xray => return true,
        None => {}
    }
    !placed.floating
        && windows
            .iter()
            .take(at.saturating_add(1))
            .all(|drawn| !styles.of(drawn.window).dim_around)
        && windows
            .iter()
            .take(at)
            .all(|under| intersect(outer(under), placed.rect).is_none())
}

/// One layer surface: the blur behind it, where a `layerrule` asked for
/// one, and then its pixels.
///
/// The blur is of the frame as it stands, whichever side of the windows the
/// surface is on: a menu over a window blurs the window, and Hyprland keeps
/// its `m_blurFB` from a layer surface unless a `layerrule = xray` asks.
///
/// No border, no rounding and no shadow: a bar draws its own corners, and a
/// compositor that put a border round a wallpaper would be drawing a line
/// across the screen.
fn draw_layer<P: Painter>(
    canvas: &mut P,
    backdrop: Option<&mut P::Backdrop>,
    layer: &LayerFrame<'_>,
    rect: Rect,
    style: &Style,
    damage: &Damage,
) {
    let Some(surface) = layer.surface.as_ref() else {
        return;
    };
    // Only for a surface that can be seen through: blurring behind an
    // opaque bar costs a pyramid of passes and changes nothing.
    if let Some(blur) = style.blur.filter(|_| layer.blur)
        && surface.format() == Format::Argb8888
    {
        // `layerrule = xray`: the kept blur of what is behind the windows,
        // which is the wallpaper and whatever is under them, rather than a
        // blur of the frame as it stands with the windows in it. The caller
        // hands over a backdrop only for a surface that asked and is drawn
        // above the windows.
        match backdrop {
            Some(behind) => canvas.blur_backdrop(behind, rect, Rounding::none(), &blur, damage),
            None => canvas.blur(rect, Rounding::none(), &blur, damage),
        }
    }
    // Stretched to its rectangle, as a window is: a client that drew at
    // scale 1 on a screen at scale 2 -- every one the compositor has not
    // told its scale -- has a buffer half the rectangle's size, and drawn
    // pixel for pixel it covered a quarter of it, under a reserved zone of
    // the whole. The exact path when the sizes agree.
    canvas.composite_scaled(surface, rect, Rounding::none(), 1.0, false, damage);
}

/// One window: its shadow, its border, the blur behind it, its own pixels
/// and the dim over them, with whatever a `windowrule` changed.
fn window<P: Painter>(
    canvas: &mut P,
    backdrop: Option<&mut P::Backdrop>,
    placed: &Placed,
    rect: Rect,
    styles: &Styles<'_>,
    surfaces: &BTreeMap<WindowId, Surface<'_>>,
    damage: &Damage,
) {
    let style = styles.base;
    let own = styles.of(placed.window);
    // A rule's `rounding` and `rounding_power` each stand in for the
    // style's own, and either may be set without the other.
    let rounding = Rounding {
        radius: own.rounding.unwrap_or(style.rounding.radius).max(0),
        power: own.rounding_power.unwrap_or(style.rounding.power),
    };
    let width = own.border.unwrap_or(placed.border).max(0);
    let opacity = own
        .opacity
        .unwrap_or_else(|| style.opacity(placed.focused, placed.fullscreen));
    let gradient = own.border_color.as_ref().unwrap_or(if placed.focused {
        &style.active_border
    } else {
        &style.inactive_border
    });
    // `decorate false`: no border and no shadow, for a window that draws
    // its own frame.
    let width = if own.decorate { width } else { 0 };
    // What is behind a window that can be seen through, blurred. Only for a
    // window that can be: blurring behind an opaque one costs a pyramid of
    // passes and changes not one pixel of the frame. A surface in a format
    // with alpha may be translucent anywhere, and a window drawn at less
    // than full opacity is translucent everywhere.
    // `opaque`: the window is drawn as if every pixel were opaque, so
    // there is nothing to see behind it and nothing to blur.
    let translucent = !own.opaque
        && (surfaces
            .get(&placed.window)
            .is_some_and(|surface| surface.format() == Format::Argb8888)
            || opacity < 1.0);
    let blur = style.blur.filter(|_| own.blur && translucent);
    // The shadow first, under the border and the window: Hyprland draws it
    // as a decoration behind them and does not cut the window's own shape
    // out of it.
    //
    // Neither does this, where any of it can be seen. But a shadow is a
    // power and a blend a pixel over a box larger than the window, twelve
    // milliseconds of a full-screen one, and under two kinds of window all
    // of that but the rim is written over before the frame is done: one
    // whose blur is copied out of the backdrop, which replaces every pixel
    // inside the window's shape, and one whose own pixels are copied in
    // opaque. There the shadow is drawn where it shows -- outside the
    // window's rectangle drawn in by its corners' radius, which is inside
    // its shape whatever the corners' curve.
    let overwritten = (blur.is_some_and(|blur| blur.size > 0 && blur.passes > 0)
        && backdrop
            .as_deref()
            .is_some_and(|behind| canvas.backdrop_fits(behind)))
        || surfaces.get(&placed.window).is_some_and(|surface| {
            (own.opaque || surface.format() == Format::Xrgb8888)
                && opacity >= 1.0
                && i64::from(surface.width()) == rect.width
                && i64::from(surface.height()) == rect.height
        });
    let seen;
    let shadowed = if overwritten {
        let inset = rounding
            .radius
            .min(rect.width / 2)
            .min(rect.height / 2)
            .max(0);
        seen = damage.without(Rect::new(
            rect.x.saturating_add(inset),
            rect.y.saturating_add(inset),
            rect.width.saturating_sub(inset.saturating_mul(2)),
            rect.height.saturating_sub(inset.saturating_mul(2)),
        ));
        &seen
    } else {
        damage
    };
    if let Some(shadow) = style.shadow.as_ref().filter(|_| own.shadow && own.decorate) {
        let _shadow = Timer::start(Phase::Shadow);
        canvas.shadow(
            Rect::new(
                rect.x.saturating_sub(width),
                rect.y.saturating_sub(width),
                rect.width.saturating_add(width.saturating_mul(2)),
                rect.height.saturating_add(width.saturating_mul(2)),
            ),
            &Shadow {
                rounding,
                ..*shadow
            },
            shadowed,
        );
    }
    // A rounded window's border follows its corners, so it cannot be four
    // strips: Hyprland draws the outer rounding as the window's plus the
    // border's width, and the surface goes inside it. A square window keeps
    // the four strips, which blend a translucent border once at the corners.
    //
    // Either way the gradient runs across the border's whole box, which is
    // the box `renderBorder` gives the shader: the rounded path fills that
    // box and the square one draws four windows onto it, so a window's
    // corner is the same colour whichever path drew it.
    let border = Timer::start(Phase::Border);
    if !rounding.is_square() {
        let outer = Rect::new(
            rect.x.saturating_sub(width),
            rect.y.saturating_sub(width),
            rect.width.saturating_add(width.saturating_mul(2)),
            rect.height.saturating_add(width.saturating_mul(2)),
        );
        canvas.fill_rounded_gradient(
            outer,
            Rounding {
                radius: rounding.radius.saturating_add(width),
                ..rounding
            },
            gradient,
            damage,
        );
    } else {
        canvas.border_gradient(rect, width, gradient, damage);
    }
    drop(border);
    if let Some(blur) = blur {
        let _blur = Timer::start(Phase::Blur);
        match backdrop {
            Some(behind) => canvas.blur_backdrop(behind, rect, rounding, &blur, damage),
            None => canvas.blur(rect, rounding, &blur, damage),
        }
    }
    if let Some(surface) = surfaces.get(&placed.window) {
        let _surface = Timer::start(Phase::Surface);
        // Scaled, which is the exact path when the surface is already the
        // rectangle's size -- which it is for every window that is not
        // part-way through an animation.
        let surface = if own.opaque {
            (*surface).as_opaque()
        } else {
            *surface
        };
        canvas.composite_scaled(&surface, rect, rounding, opacity, own.nearest, damage);
    }
    // `decoration:dim_inactive`: black over a window that is not focused, at
    // `dim_strength`. Over the surface, because it dims the window and not
    // the background behind it.
    if style.dim > 0.0 && !placed.focused && own.dim {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "an opacity between zero and one becomes a byte of alpha"
        )]
        let alpha = (style.dim.clamp(0.0, 1.0) * 255.0).round() as u32;
        canvas.fill_rounded(rect, rounding, Color(alpha << 24), damage);
    }
}

/// The part of a monitor whose top-left corner is at `origin` that differs
/// between layouts `old` and `new`, in the monitor's coordinates: the
/// border-and-client rectangle of each window that appeared, went, moved,
/// changed its border or its focus, both where it was and where it is. If the
/// stacking order changed, every window in either layout counts.
///
/// Changes to a surface's own pixels are the caller's to add.
#[must_use]
pub fn damage_between(old: &MonitorLayout, new: &MonitorLayout, origin: (i64, i64)) -> Damage {
    let local = |rect: Rect| rect.translate(origin.0.saturating_neg(), origin.1.saturating_neg());
    let order = |layout: &MonitorLayout| -> Vec<WindowId> {
        layout.windows.iter().map(|placed| placed.window).collect()
    };
    let restacked = {
        let (before, after) = (order(old), order(new));
        let common = |a: &[WindowId], b: &[WindowId]| -> Vec<WindowId> {
            a.iter().copied().filter(|id| b.contains(id)).collect()
        };
        common(&before, &after) != common(&after, &before)
    };
    let find = |layout: &MonitorLayout, window: WindowId| {
        layout
            .windows
            .iter()
            .find(|placed| placed.window == window)
            .copied()
    };
    let mut damage = Damage::new();
    for placed in &old.windows {
        if restacked || find(new, placed.window) != Some(*placed) {
            damage.add(local(outer(placed)));
        }
    }
    for placed in &new.windows {
        if restacked || find(old, placed.window) != Some(*placed) {
            damage.add(local(outer(placed)));
        }
    }
    damage
}

/// [`damage_between`], knowing how each window is drawn: a window whose
/// focus is all that changed is owed only its border, and the part of its
/// box its corners cut, rather than all of it.
///
/// Focus decides three things about a window's pixels: its border's colour,
/// how much of it shows (`active_opacity` and `inactive_opacity`), and
/// whether it is dimmed (`dim_inactive`). Where the second and third come
/// out the same either way -- which they do unless a configuration sets
/// them -- moving the focus between two windows changes their borders and
/// nothing inside them. Redrawing all of both was most of what a focus
/// change cost on a slow machine: two windows' worth of pixels drawn and
/// copied to the screen, turned for a monitor standing on its edge, for a
/// frame whose only change was two coloured rings.
///
/// The ring is the border's box less the window's rectangle drawn in by its
/// rounding's radius: under a rounded corner the border's fill shows, so
/// that corner is part of what changed colour.
///
/// A window that takes the focus is also raised, which [`damage_between`]
/// answers with every window whole, since which of two is drawn over the
/// other is a change wherever they overlap. Here it is answered with that:
/// where they overlap, their shadows' reach included -- which for tiled
/// windows is nowhere, and on the DK1 was the whole of each focus change
/// the ring had been meant to save.
#[must_use]
pub fn damage_between_styled(
    old: &MonitorLayout,
    new: &MonitorLayout,
    origin: (i64, i64),
    styles: &Styles<'_>,
) -> Damage {
    let (mut damage, rings) = damage_between_parts(old, new, origin, styles);
    damage.extend(&rings);
    damage
}

/// [`damage_between_styled`] in its two parts: what moved, came, went or
/// was restacked, and the rings of windows whose focus alone changed.
///
/// Apart because a compositor grows the first by how far a window's shadow
/// reaches, which a window that moved has moved too, and a ring needs no
/// such growing: its shadow is where it was. Grown, two thin rings were
/// ten times their own pixels, most of them the shadow's fading rim.
#[must_use]
pub fn damage_between_parts(
    old: &MonitorLayout,
    new: &MonitorLayout,
    origin: (i64, i64),
    styles: &Styles<'_>,
) -> (Damage, Damage) {
    let local = |rect: Rect| rect.translate(origin.0.saturating_neg(), origin.1.saturating_neg());
    let find = |layout: &MonitorLayout, window: WindowId| {
        layout
            .windows
            .iter()
            .position(|placed| placed.window == window)
            .and_then(|at| Some((at, *layout.windows.get(at)?)))
    };
    let mut damage = Damage::new();
    let mut rings = Damage::new();

    // Each window by what it is, wherever it is in either list: one that
    // came or went, moved or changed its border is owed all of its box
    // where it was and where it is; one whose focus is all that changed,
    // where focus changes nothing inside it, only its ring.
    let ids = old
        .windows
        .iter()
        .chain(&new.windows)
        .map(|placed| placed.window)
        .collect::<BTreeSet<_>>();
    for &window in &ids {
        let (was, now) = (find(old, window), find(new, window));
        match (was, now) {
            (Some((_, was)), Some((_, now))) if was == now => {}
            (Some((_, was)), Some((_, now)))
                if Placed {
                    focused: now.focused,
                    ..was
                } == now
                    && focus_is_border_only(styles, &now) =>
            {
                for part in ring(&now, styles) {
                    rings.add(local(part));
                }
            }
            _ => {
                for placed in [was, now].into_iter().flatten() {
                    damage.add(local(outer(&placed.1)));
                }
            }
        }
    }

    // And the stacking order: which of two windows is drawn over the other
    // shows only where they overlap, shadows included. A focused window is
    // raised, so a focus change restacks; tiled windows do not overlap, and
    // for them the order changes no pixel at all.
    let reach = styles.base.shadow.map_or(0, |shadow| {
        shadow
            .range
            .saturating_add(shadow.offset.0.abs().max(shadow.offset.1.abs()))
            .max(0)
    });
    let reached = |placed: &Placed| {
        let box_ = outer(placed);
        Rect::new(
            box_.x.saturating_sub(reach),
            box_.y.saturating_sub(reach),
            box_.width.saturating_add(reach.saturating_mul(2)),
            box_.height.saturating_add(reach.saturating_mul(2)),
        )
    };
    let ids: Vec<WindowId> = ids.into_iter().collect();
    for (at, &one) in ids.iter().enumerate() {
        for &other in ids.iter().skip(at.saturating_add(1)) {
            let (
                Some((one_was, a)),
                Some((other_was, b)),
                Some((one_now, c)),
                Some((other_now, d)),
            ) = (
                find(old, one),
                find(old, other),
                find(new, one),
                find(new, other),
            )
            else {
                continue;
            };
            if (one_was < other_was) == (one_now < other_now) {
                continue;
            }
            for (first, second) in [(a, b), (c, d)] {
                if let Some(overlap) = intersect(reached(&first), reached(&second)) {
                    damage.add(local(overlap));
                }
            }
        }
    }
    (damage, rings)
}

/// Whether focusing or unfocusing `placed` changes nothing of it but its
/// border: it shows as much of itself and is dimmed alike either way.
fn focus_is_border_only(styles: &Styles<'_>, placed: &Placed) -> bool {
    let style = styles.base;
    let own = styles.of(placed.window);
    let opacity = |focused: bool| {
        own.opacity
            .unwrap_or_else(|| style.opacity(focused, placed.fullscreen))
    };
    let dimmed = style.dim > 0.0 && own.dim;
    opacity(true).to_bits() == opacity(false).to_bits() && !dimmed
}

/// The part of `placed`'s box a change of its border's colour repaints:
/// the border, and the corners its rounding cuts out of the window.
fn ring(placed: &Placed, styles: &Styles<'_>) -> [Rect; 4] {
    let own = styles.of(placed.window);
    let border = if own.decorate {
        own.border.unwrap_or(placed.border).max(0)
    } else {
        0
    };
    let rect = placed.rect;
    let radius = own
        .rounding
        .unwrap_or(styles.base.rounding.radius)
        .max(0)
        .min(rect.width / 2)
        .min(rect.height / 2);
    let outer = Rect::new(
        rect.x.saturating_sub(border),
        rect.y.saturating_sub(border),
        rect.width.saturating_add(border.saturating_mul(2)),
        rect.height.saturating_add(border.saturating_mul(2)),
    );
    let inner = Rect::new(
        rect.x.saturating_add(radius),
        rect.y.saturating_add(radius),
        rect.width.saturating_sub(radius.saturating_mul(2)),
        rect.height.saturating_sub(radius.saturating_mul(2)),
    );
    let above = inner.y.saturating_sub(outer.y);
    let below = outer.bottom().saturating_sub(inner.bottom());
    [
        Rect::new(outer.x, outer.y, outer.width, above),
        Rect::new(outer.x, inner.bottom(), outer.width, below),
        Rect::new(
            outer.x,
            inner.y,
            inner.x.saturating_sub(outer.x),
            inner.height,
        ),
        Rect::new(
            inner.right(),
            inner.y,
            outer.right().saturating_sub(inner.right()),
            inner.height,
        ),
    ]
}
