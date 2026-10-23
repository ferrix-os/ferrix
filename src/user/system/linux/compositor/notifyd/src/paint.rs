//! Drawing the banners and the list (`docs/DESKTOP.md` §4.2): rounded
//! light cards with the app's name small, the summary bold, the body, and
//! the actions as buttons under them; a close cross in the corner.
//! Ferrix light's own colours.

use compositor_text::tiny_skia::{
    FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform,
};
use compositor_text::{Ellipsize, FontDescription, Fonts, Rgba, Size, Weight};

use crate::store::{Note, Urgency};

/// A banner's width, logical pixels.
pub const WIDTH: u32 = 400;
/// The surface's height, logical pixels: fixed, so that a banner coming or
/// going never resizes it (three banners at their tallest fit, and the
/// list shows as many as fit). What is not drawn on takes no input.
pub const HEIGHT: u32 = 560;
/// The space round and between banners.
pub const GAP: u32 = 10;
/// Inside a banner.
const PAD: f32 = 14.0;
/// A banner's corner radius.
const RADIUS: f32 = 14.0;
/// An action button's height.
const BUTTON: f32 = 30.0;

const CARD: Rgba = Rgba::rgb(0xfa, 0xfa, 0xfb);
const CARD_CRITICAL: Rgba = Rgba::rgb(0xff, 0xef, 0xec);
const TEXT: Rgba = Rgba::rgb(0x24, 0x24, 0x28);
const DIM: Rgba = Rgba::rgb(0x77, 0x77, 0x7d);
const BUTTON_FILL: Rgba = Rgba::rgb(0xe8, 0xe8, 0xeb);
const BORDER: Rgba = Rgba {
    r: 0,
    g: 0,
    b: 0,
    a: 0x24,
};

/// Where a click lands: a banner, its close cross, or one of its actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The banner itself: its `default` action.
    Banner(u32),
    /// Its close cross.
    Close(u32),
    /// The list, where nothing is a button.
    List,
    /// An action, by its key.
    Action(u32, String),
}

/// A drawn area and what a click there means, in logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Area {
    /// Left, top, width, height.
    pub rect: (f32, f32, f32, f32),
    /// What it is.
    pub hit: Hit,
}

/// The banners painted, stacked top to bottom, at `scale`: the pixmap, the
/// height in logical pixels, and where each click goes. `None` with no
/// banner.
pub fn banners(fonts: &mut Fonts, notes: &[Note], scale: u32) -> Option<(Pixmap, u32, Vec<Area>)> {
    if notes.is_empty() {
        return None;
    }
    let heights: Vec<f32> = notes.iter().map(|note| height(fonts, note)).collect();
    let total = heights.iter().sum::<f32>() + GAP as f32 * (notes.len() as f32 + 1.0);
    let s = scale.max(1) as f32;
    let mut pixmap = Pixmap::new(
        (WIDTH as f32 * s).ceil() as u32,
        (total * s).ceil().max(1.0) as u32,
    )?;
    let mut areas = Vec::new();
    let mut y = GAP as f32;
    for (note, height) in notes.iter().zip(heights) {
        draw_card(
            fonts,
            &mut pixmap,
            note,
            GAP as f32,
            y,
            WIDTH as f32 - 2.0 * GAP as f32,
            height,
            s,
            &mut areas,
        );
        y += height + GAP as f32;
    }
    Some((pixmap, total.ceil() as u32, areas))
}

/// The list of recent notifications, newest first, under a "Clear"
/// button: the pixmap, its height and its areas. The clear button's hit is
/// `Action(0, "clear")`.
pub fn list(fonts: &mut Fonts, notes: &[Note], scale: u32) -> Option<(Pixmap, u32, Vec<Area>)> {
    let s = scale.max(1) as f32;
    let inner = WIDTH as f32 - 2.0 * GAP as f32;
    let header = 40.0;
    let mut rows: Vec<f32> = Vec::new();
    let mut used = header + 2.0 * GAP as f32;
    for note in notes {
        let row = height(fonts, note) - button_room(note);
        if used + row + GAP as f32 > HEIGHT as f32 {
            break;
        }
        used += row + GAP as f32;
        rows.push(row);
    }
    let total = header + rows.iter().sum::<f32>() + GAP as f32 * (rows.len() as f32 + 2.0);
    let mut pixmap = Pixmap::new((WIDTH as f32 * s).ceil() as u32, (total * s).ceil() as u32)?;
    fill_round(
        &mut pixmap,
        0.0,
        0.0,
        WIDTH as f32,
        total,
        RADIUS,
        s,
        Rgba::rgb(0xf0, 0xf0, 0xf2),
    );
    let title = if notes.is_empty() {
        "No notifications"
    } else {
        "Notifications"
    };
    text_line(
        fonts,
        &mut pixmap,
        title,
        GAP as f32 + 4.0,
        26.0,
        inner,
        15.0,
        true,
        TEXT,
        s,
    );
    let mut areas = vec![Area {
        rect: (0.0, 0.0, WIDTH as f32, total),
        hit: Hit::List,
    }];
    if !notes.is_empty() {
        let (bx, by, bw, bh) = (WIDTH as f32 - GAP as f32 - 70.0, 8.0, 70.0, 26.0);
        fill_round(&mut pixmap, bx, by, bw, bh, 13.0, s, BUTTON_FILL);
        text_line(
            fonts,
            &mut pixmap,
            "Clear",
            bx + 16.0,
            by + 18.0,
            bw,
            13.0,
            true,
            TEXT,
            s,
        );
        areas.push(Area {
            rect: (bx, by, bw, bh),
            hit: Hit::Action(0, "clear".to_owned()),
        });
    }
    let mut y = header + GAP as f32;
    for (note, height) in notes.iter().zip(rows) {
        let plain = Note {
            actions: Vec::new(),
            ..note.clone()
        };
        let mut ignored = Vec::new();
        draw_card(
            fonts,
            &mut pixmap,
            &plain,
            GAP as f32,
            y,
            inner,
            height,
            s,
            &mut ignored,
        );
        y += height + GAP as f32;
    }
    Some((pixmap, total.ceil() as u32, areas))
}

/// The room the action buttons take under a card's text.
fn button_room(note: &Note) -> f32 {
    let shown = note
        .actions
        .iter()
        .filter(|(key, _)| key != "default")
        .count();
    if shown == 0 { 0.0 } else { BUTTON + 10.0 }
}

/// A card's height: the app line, the summary, up to three lines of body,
/// and the buttons.
fn height(fonts: &mut Fonts, note: &Note) -> f32 {
    let body_lines = if note.body.is_empty() {
        0.0
    } else {
        let font = font(fonts, 14.0, false);
        let width = fonts.measure(&font, &note.body);
        (width / (WIDTH as f32 - 2.0 * GAP as f32 - 2.0 * PAD))
            .ceil()
            .clamp(1.0, 3.0)
    };
    PAD + 16.0 + 22.0 + body_lines * 19.0 + button_room(note) + PAD
}

#[expect(
    clippy::too_many_arguments,
    reason = "a card's place and its size, each a number"
)]
fn draw_card(
    fonts: &mut Fonts,
    pixmap: &mut Pixmap,
    note: &Note,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    s: f32,
    areas: &mut Vec<Area>,
) {
    let fill = if note.urgency == Urgency::Critical {
        CARD_CRITICAL
    } else {
        CARD
    };
    fill_round(
        pixmap,
        x - 1.0,
        y - 1.0,
        width + 2.0,
        height + 2.0,
        RADIUS + 1.0,
        s,
        BORDER,
    );
    fill_round(pixmap, x, y, width, height, RADIUS, s, fill);
    areas.push(Area {
        rect: (x, y, width, height),
        hit: Hit::Banner(note.id),
    });
    let inner = width - 2.0 * PAD;
    let mut app = note.app.clone();
    if note.count > 1 {
        app = format!("{app} \u{b7} {} times", note.count);
    }
    text_line(
        fonts,
        pixmap,
        &app,
        x + PAD,
        y + PAD + 11.0,
        inner - 24.0,
        12.0,
        false,
        DIM,
        s,
    );
    // The close cross, top right.
    let (cx, cy) = (x + width - PAD - 6.0, y + PAD + 6.0);
    cross(pixmap, cx, cy, 5.0, s, DIM);
    areas.push(Area {
        rect: (cx - 12.0, cy - 12.0, 24.0, 24.0),
        hit: Hit::Close(note.id),
    });
    text_line(
        fonts,
        pixmap,
        &note.summary,
        x + PAD,
        y + PAD + 34.0,
        inner,
        15.0,
        true,
        TEXT,
        s,
    );
    let mut line_y = y + PAD + 56.0;
    if !note.body.is_empty() {
        for line in wrap(fonts, &note.body, inner, 3) {
            text_line(
                fonts,
                pixmap,
                &line,
                x + PAD,
                line_y,
                inner,
                14.0,
                false,
                TEXT,
                s,
            );
            line_y += 19.0;
        }
    }
    let actions: Vec<&(String, String)> = note
        .actions
        .iter()
        .filter(|(key, _)| key != "default")
        .collect();
    if !actions.is_empty() {
        let count = actions.len() as f32;
        let each = (inner - 8.0 * (count - 1.0)) / count;
        let by = y + height - PAD - BUTTON;
        for (index, (key, label)) in actions.into_iter().enumerate() {
            let bx = x + PAD + index as f32 * (each + 8.0);
            fill_round(pixmap, bx, by, each, BUTTON, BUTTON / 2.0, s, BUTTON_FILL);
            text_line(
                fonts,
                pixmap,
                label,
                bx + 12.0,
                by + 20.0,
                each - 24.0,
                13.0,
                true,
                TEXT,
                s,
            );
            areas.push(Area {
                rect: (bx, by, each, BUTTON),
                hit: Hit::Action(note.id, key.clone()),
            });
        }
    }
}

/// `text` broken at spaces into at most `lines` lines of `width`, the last
/// ellipsized by the drawing.
fn wrap(fonts: &mut Fonts, text: &str, width: f32, lines: usize) -> Vec<String> {
    let font = font(fonts, 14.0, false);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let tried = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if fonts.measure(&font, &tried) > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            if out.len() == lines {
                return out;
            }
            word.clone_into(&mut current);
        } else {
            current = tried;
        }
    }
    if !current.is_empty() && out.len() < lines {
        out.push(current);
    }
    out
}

fn font(fonts: &mut Fonts, px: f32, bold: bool) -> compositor_text::Font {
    let mut description = FontDescription::pango("sans-serif");
    description.size = Size::Pixels(px);
    if bold {
        description.weight = Weight::BOLD;
    }
    fonts.resolve(&description)
}

#[expect(clippy::too_many_arguments, reason = "a line's place, size and look")]
fn text_line(
    fonts: &mut Fonts,
    pixmap: &mut Pixmap,
    text: &str,
    x: f32,
    baseline: f32,
    width: f32,
    px: f32,
    bold: bool,
    color: Rgba,
    s: f32,
) {
    let font = font(fonts, px * s, bold);
    let run = fonts.shape_ellipsized(&font, text, width * s, Ellipsize::End);
    fonts.draw_run(&run, &mut pixmap.as_mut(), x * s, baseline * s, color);
}

#[expect(
    clippy::too_many_arguments,
    reason = "a rectangle, its corners and its colour"
)]
fn fill_round(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, s: f32, color: Rgba) {
    let (x, y, w, h, r) = (
        x * s,
        y * s,
        w * s,
        h * s,
        (r * s).min(w / 2.0).min(h / 2.0),
    );
    let mut path = PathBuilder::new();
    path.move_to(x + r, y);
    path.line_to(x + w - r, y);
    path.quad_to(x + w, y, x + w, y + r);
    path.line_to(x + w, y + h - r);
    path.quad_to(x + w, y + h, x + w - r, y + h);
    path.line_to(x + r, y + h);
    path.quad_to(x, y + h, x, y + h - r);
    path.line_to(x, y + r);
    path.quad_to(x, y, x + r, y);
    path.close();
    let Some(path) = path.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color.to_skia());
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn cross(pixmap: &mut Pixmap, cx: f32, cy: f32, half: f32, s: f32, color: Rgba) {
    let mut path = PathBuilder::new();
    path.move_to((cx - half) * s, (cy - half) * s);
    path.line_to((cx + half) * s, (cy + half) * s);
    path.move_to((cx + half) * s, (cy - half) * s);
    path.line_to((cx - half) * s, (cy + half) * s);
    let Some(path) = path.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color.to_skia());
    paint.anti_alias = true;
    let stroke = Stroke {
        width: 1.6 * s,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// The area under a click at `(x, y)`, the innermost: an action or the
/// cross before the banner.
#[must_use]
pub fn hit(areas: &[Area], x: f64, y: f64) -> Option<Hit> {
    let (x, y) = (x as f32, y as f32);
    areas
        .iter()
        .rev()
        .find(|area| {
            let (ax, ay, aw, ah) = area.rect;
            x >= ax && x < ax + aw && y >= ay && y < ay + ah
        })
        .map(|area| area.hit.clone())
}
