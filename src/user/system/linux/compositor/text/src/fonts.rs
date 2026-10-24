//! The faces this machine has, matched, shaped and drawn.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ttf_parser::Tag;

use crate::face::Instance;
use crate::markup::{Rgba, Span, Underline};
use crate::scan::{self, Scanned};
use crate::{
    FaceId, FaceInfo, Font, FontDescription, Glyph, Layout, LayoutOptions, Mask, Metrics, Run,
    Style, Weight,
};

/// A file faces were found in.
#[derive(Default)]
struct FontFile {
    /// Where it is, or the name [`Fonts::add_bytes`] was given.
    path: PathBuf,
    /// The whole file, once a glyph has been wanted from it.
    data: Option<Arc<[u8]>>,
    /// Whether reading it failed, so that it is not tried again.
    failed: bool,
}

/// What a face needs beyond its [`FaceInfo`].
struct Extra {
    /// Its file, an index into [`Fonts::files`].
    file: usize,
    /// The characters it maps, as inclusive ranges.
    coverage: Vec<(u32, u32)>,
    /// The default of its `wght` axis, its weight if it has none.
    default_weight: f32,
}

/// What a rasterised glyph is cached by. The face's id carries its
/// instance and synthesis.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct GlyphKey {
    face: u32,
    id: u16,
    px: u32,
    quarter: u8,
}

/// One glyph as the shaper placed it, before a [`Run`] is made of it.
#[derive(Clone, Copy, Debug)]
struct Shaped {
    face: FaceId,
    id: u16,
    cluster: usize,
    advance: f32,
    x_offset: f32,
    y_offset: f32,
}

/// A face's lines, in pixels: underline position and thickness, then
/// strike-through position and thickness, as [`Metrics`] has them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Decoration {
    pub(crate) underline_position: f32,
    pub(crate) underline_thickness: f32,
    pub(crate) strikeout_position: f32,
    pub(crate) strikeout_thickness: f32,
}

/// How many glyphs are kept before the cache is emptied and begun again.
const GLYPH_CACHE: usize = 8192;

/// `FreeType`'s `FT_GlyphSlot_Oblique` slant, which is what fontconfig's
/// synthetic italic is drawn with under Pango.
const OBLIQUE_SKEW: f32 = 0.2126;

/// The text Pango measures `approximate_char_width` over, for English.
const SAMPLE: &str = "The quick brown fox jumps over the lazy dog.";

/// The families a generic name stands for, first choice first.
const SANS: &[&str] = &[
    "DejaVu Sans",
    "Noto Sans",
    "Liberation Sans",
    "Inter",
    "Inter Variable",
    "Ubuntu",
];
/// `serif`.
const SERIF: &[&str] = &["DejaVu Serif", "Noto Serif", "Liberation Serif"];
/// `monospace`.
const MONO: &[&str] = &["DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"];
/// `emoji`.
const EMOJI: &[&str] = &["Noto Color Emoji"];

/// A family name as fontconfig compares them: case and spaces ignored.
fn normalize(family: &str) -> String {
    family
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The list a normalised generic name stands for.
fn generic_list(key: &str) -> Option<(&'static [&'static str], bool)> {
    Some(match key {
        "sans-serif" | "sans" | "system-ui" => (SANS, false),
        "serif" => (SERIF, false),
        "monospace" | "mono" => (MONO, true),
        "emoji" => (EMOJI, false),
        _ => return None,
    })
}

/// CSS's nearest-weight order: `(tier, distance)`, smaller better. Below
/// 400 lighter weights are tried first, above 500 heavier ones, and in
/// between the weights up to 500, then lighter, then heavier.
fn weight_rank(wanted: f32, have: f32) -> (u8, f32) {
    if (have - wanted).abs() < 0.5 {
        (0, 0.0)
    } else if wanted < 400.0 {
        if have < wanted {
            (1, wanted - have)
        } else {
            (2, have - wanted)
        }
    } else if wanted > 500.0 {
        if have > wanted {
            (1, have - wanted)
        } else {
            (2, wanted - have)
        }
    } else if have > wanted && have <= 500.0 {
        (1, have - wanted)
    } else if have < wanted {
        (2, wanted - have)
    } else {
        (3, have - wanted)
    }
}

/// The style order: what was asked for, then the nearest other slant.
fn style_rank(wanted: Style, have: Style) -> u8 {
    match (wanted, have) {
        (Style::Italic, Style::Italic)
        | (Style::Oblique, Style::Oblique)
        | (Style::Normal, Style::Normal) => 0,
        (Style::Italic, Style::Oblique)
        | (Style::Oblique, Style::Italic)
        | (Style::Normal, Style::Oblique) => 1,
        (Style::Italic | Style::Oblique, Style::Normal) | (Style::Normal, Style::Italic) => 2,
    }
}

/// Whether a file's name says it is a font.
fn is_font_name(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ttf" | "otf" | "ttc" | "otc"
            )
        })
}

/// A face parsed from `data`, instanced and ready to shape and draw.
fn open(data: &[u8], index: u32, instance: Instance) -> Option<rustybuzz::Face<'_>> {
    let mut face = ttf_parser::Face::parse(data, index).ok()?;
    if let Some(weight) = instance.weight {
        let _ = face.set_variation(Tag::from_bytes(b"wght"), f32::from(weight));
    }
    Some(rustybuzz::Face::from_face(face))
}

/// Pixels per font unit at `px`.
fn scale_of(face: &ttf_parser::Face<'_>, px: f32) -> f32 {
    px / f32::from(face.units_per_em().max(1))
}

/// Whether glyph positions are whole pixels, and so how a caller asks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Positions {
    /// Each advance and offset rounded to a whole pixel, as Pango places
    /// glyphs by default (`pango_context_set_round_glyph_positions`, on
    /// unless a program turns it off, which GTK3 and hyprlock do not).
    Whole,
    /// `HarfBuzz`'s own fractions, for Pango's `approximate_char_width`,
    /// which is measured without the rounding.
    Fractional,
}

/// `text` shaped in one face: glyph, cluster (a byte offset from `base`),
/// and advance and offsets in pixels.
///
/// Pango on example (1.57, GTK3's `font-size: 15px` on Ubuntu) measures
/// `vol 0%` 44 wide, `abc` 24 and a space 3: each glyph's advance rounded
/// on its own, which `HarfBuzz`'s 44.50, 23.46 and 3.44 become only when
/// every glyph is rounded first. So `Positions::Whole` rounds each glyph.
fn shape_in(
    face: &rustybuzz::Face<'_>,
    id: FaceId,
    text: &str,
    base: usize,
    px: f32,
    positions: Positions,
) -> Vec<Shaped> {
    let place = |value: f32| match positions {
        Positions::Whole => value.round(),
        Positions::Fractional => value,
    };
    let scale = scale_of(face, px);
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let output = rustybuzz::shape(face, &[], buffer);
    output
        .glyph_infos()
        .iter()
        .zip(output.glyph_positions())
        .map(|(info, position)| Shaped {
            face: id,
            id: u16::try_from(info.glyph_id).unwrap_or(0),
            cluster: base + info.cluster as usize,
            advance: place(position.x_advance as f32 * scale),
            x_offset: place(position.x_offset as f32 * scale),
            y_offset: place(position.y_offset as f32 * scale),
        })
        .collect()
}

/// A face's underline and strike-through at `px`, with fallbacks for a
/// face that gives none.
fn decoration_of(face: &ttf_parser::Face<'_>, px: f32) -> Decoration {
    let scale = scale_of(face, px);
    let fallback = (px / 14.0).max(1.0);
    let (underline_position, underline_thickness) = match face.underline_metrics() {
        Some(line) if line.thickness > 0 => (
            -f32::from(line.position) * scale,
            f32::from(line.thickness) * scale,
        ),
        _ => (px / 10.0, fallback),
    };
    let (strikeout_position, strikeout_thickness) = match face.strikeout_metrics() {
        Some(line) if line.thickness > 0 && line.position > 0 => (
            f32::from(line.position) * scale,
            f32::from(line.thickness) * scale,
        ),
        _ => (
            f32::from(face.ascender()) * scale * 0.3 + underline_thickness / 2.0,
            underline_thickness,
        ),
    };
    Decoration {
        underline_position,
        underline_thickness,
        strikeout_position,
        strikeout_thickness,
    }
}

/// Metrics for a font with no face at all.
fn fallback_metrics(px: f32) -> Metrics {
    Metrics {
        ascent: px * 0.8,
        descent: px * 0.2,
        line_gap: 0.0,
        height: px,
        underline_position: px / 10.0,
        underline_thickness: (px / 14.0).max(1.0),
        strikeout_position: px * 0.3,
        strikeout_thickness: (px / 14.0).max(1.0),
        approximate_char_width: px / 2.0,
        approximate_digit_width: px / 2.0,
    }
}

/// Every face found, the ones loaded, and the glyphs drawn from them.
///
/// Scanning reads each file's name, `OS/2`, `post`, `fvar` and `cmap`
/// tables and no more; a face's file is read whole the first time a glyph
/// is wanted from it, and kept.
#[derive(Default)]
pub struct Fonts {
    /// Every face, in the order found.
    faces: Vec<FaceInfo>,
    /// Beside each face, what matching and loading it need.
    extra: Vec<Extra>,
    /// The files the faces are in.
    files: Vec<FontFile>,
    /// The files added, by canonical path, so none is added twice.
    paths: HashSet<PathBuf>,
    /// Faces by each of their family names, normalised.
    families: HashMap<String, Vec<usize>>,
    /// Each face's first family, normalised, in the order first seen: the
    /// last resort of [`Fonts::resolve`].
    order: Vec<String>,
    /// Which of those are in `order`.
    ordered: HashSet<String>,
    /// Glyphs drawn.
    glyphs: HashMap<GlyphKey, Option<Mask>>,
    /// Ascent and descent by face and size.
    vertical: HashMap<(u32, u32), Option<(f32, f32)>>,
}

impl fmt::Debug for Fonts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Fonts")
            .field("faces", &self.faces.len())
            .field("files", &self.files.len())
            .field(
                "loaded",
                &self.files.iter().filter(|file| file.data.is_some()).count(),
            )
            .field("glyphs", &self.glyphs.len())
            .finish_non_exhaustive()
    }
}

impl Fonts {
    /// No faces at all. A test adds its own.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The faces under the directories fontconfig would look in on this
    /// machine, in this order: `$FERRIX_FONT_DIRS` (colon-separated, the
    /// image's own list), `$XDG_DATA_HOME/fonts` (`~/.local/share/fonts`),
    /// `~/.fonts`, `/usr/share/fonts`, `/usr/local/share/fonts`,
    /// `/usr/share/ferrix/fonts` (the tree's `assets/fonts/`, where an image
    /// carries it). Missing directories are skipped; subdirectories are
    /// read.
    #[must_use]
    pub fn system() -> Self {
        let mut fonts = Self::new();
        for dir in Self::system_dirs() {
            let _ = fonts.add_dir(&dir);
        }
        fonts
    }

    /// The directories [`Fonts::system`] reads, in order.
    #[must_use]
    pub fn system_dirs() -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(list) = std::env::var_os("FERRIX_FONT_DIRS") {
            dirs.extend(std::env::split_paths(&list).filter(|dir| !dir.as_os_str().is_empty()));
        }
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        match std::env::var_os("XDG_DATA_HOME").filter(|data| !data.is_empty()) {
            Some(data) => dirs.push(PathBuf::from(data).join("fonts")),
            None => dirs.extend(home.iter().map(|home| home.join(".local/share/fonts"))),
        }
        dirs.extend(home.iter().map(|home| home.join(".fonts")));
        for fixed in [
            "/usr/share/fonts",
            "/usr/local/share/fonts",
            "/usr/share/ferrix/fonts",
        ] {
            dirs.push(PathBuf::from(fixed));
        }
        let mut seen = HashSet::new();
        dirs.retain(|dir| seen.insert(dir.clone()));
        dirs
    }

    /// Add every face under `dir`, recursively: `.ttf`, `.otf`, `.ttc`,
    /// `.otc`. Answers how many.
    pub fn add_dir(&mut self, dir: &Path) -> usize {
        let mut visited = HashSet::new();
        self.add_tree(dir, &mut visited)
    }

    /// [`Fonts::add_dir`], each directory once however it is reached.
    fn add_tree(&mut self, dir: &Path, visited: &mut HashSet<PathBuf>) -> usize {
        let Ok(canonical) = dir.canonicalize() else {
            return 0;
        };
        if !visited.insert(canonical) {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        paths.sort();
        let mut added = 0;
        for path in paths {
            if path.is_dir() {
                added += self.add_tree(&path, visited);
            } else if is_font_name(&path) {
                added += self.add_file(&path);
            }
        }
        added
    }

    /// Add the faces in one file. Answers how many; a file that is not a
    /// font adds none, and nor does one already added.
    pub fn add_file(&mut self, path: &Path) -> usize {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if self.paths.contains(&canonical) {
            return 0;
        }
        let scanned = scan::scan_file(path);
        if scanned.is_empty() {
            return 0;
        }
        let _ = self.paths.insert(canonical.clone());
        let file = self.files.len();
        // The file itself, not a link to it: `Ubuntu-L.ttf` on Ubuntu is a
        // link to `Ubuntu[wdth,wght].ttf`.
        self.files.push(FontFile {
            path: canonical,
            data: None,
            failed: false,
        });
        self.add_scanned(file, scanned)
    }

    /// Add faces from bytes already in memory, named `origin` in
    /// [`FaceInfo::path`].
    pub fn add_bytes(&mut self, bytes: Vec<u8>, origin: &str) -> usize {
        let count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
        let scanned: Vec<Scanned> = (0..count)
            .filter_map(|index| {
                ttf_parser::Face::parse(&bytes, index)
                    .ok()
                    .map(|face| scan::describe(&face, index))
            })
            .collect();
        if scanned.is_empty() {
            return 0;
        }
        let file = self.files.len();
        self.files.push(FontFile {
            path: PathBuf::from(origin),
            data: Some(Arc::from(bytes)),
            failed: false,
        });
        self.add_scanned(file, scanned)
    }

    /// Record the faces of file `file`.
    fn add_scanned(&mut self, file: usize, scanned: Vec<Scanned>) -> usize {
        let path = self
            .files
            .get(file)
            .map(|file| file.path.clone())
            .unwrap_or_default();
        let mut added = 0;
        for face in scanned {
            let index = self.faces.len();
            if FaceId::of(Instance {
                index,
                weight: None,
                bold: false,
                oblique: false,
            })
            .is_none()
            {
                break;
            }
            for family in &face.families {
                let list = self.families.entry(normalize(family)).or_default();
                if !list.contains(&index) {
                    list.push(index);
                }
            }
            if let Some(first) = face.families.first() {
                let key = normalize(first);
                if self.ordered.insert(key.clone()) {
                    self.order.push(key);
                }
            }
            let default_weight = face
                .weight_axis
                .map_or(f32::from(face.weight.0), |(_, default, _)| default);
            self.faces.push(FaceInfo {
                families: face.families,
                style_name: face.style_name,
                full_name: face.full_name,
                weight: face.weight,
                style: face.style,
                monospace: face.monospace,
                variable_weight: face.weight_axis.map(|(least, _, most)| (least, most)),
                path: path.clone(),
                index: face.index,
            });
            self.extra.push(Extra {
                file,
                coverage: face.coverage,
                default_weight,
            });
            added += 1;
        }
        added
    }

    /// Every face, in the order found; a [`FaceId`] is an index here
    /// ([`FaceId::index`]).
    #[must_use]
    pub fn faces(&self) -> &[FaceInfo] {
        &self.faces
    }

    /// The face `face` is, whatever instance of it the id names.
    #[must_use]
    pub fn info(&self, face: FaceId) -> Option<&FaceInfo> {
        self.faces.get(face.index())
    }

    /// The best face of one family for `weight` and `style`, by
    /// fontconfig's rules: the family must match (case and spaces ignored,
    /// any of the face's family names); then the nearest weight (CSS's
    /// rule: below 400 look lighter first, above 500 heavier first); then
    /// the style. A variable face whose `wght` axis covers the weight
    /// matches it exactly. A generic family name matches what
    /// [`Fonts::generic`] gives it. `None` when no face has the family.
    ///
    /// The id is the face's plain index; [`Fonts::resolve`] is what adds
    /// the instance and synthesis to draw it with.
    #[must_use]
    pub fn find(&self, family: &str, weight: Weight, style: Style) -> Option<FaceId> {
        let key = normalize(family);
        if generic_list(&key).is_some() {
            return self
                .generic(family)
                .iter()
                .find_map(|name| self.find_named(&normalize(name), weight, style));
        }
        self.find_named(&key, weight, style)
    }

    /// [`Fonts::find`] for a family that is not generic, by its key.
    fn find_named(&self, key: &str, weight: Weight, style: Style) -> Option<FaceId> {
        let candidates = self.families.get(key)?;
        let best = candidates.iter().copied().min_by(|&one, &other| {
            self.rank(one, weight, style)
                .partial_cmp(&self.rank(other, weight, style))
                .unwrap_or(Ordering::Equal)
        })?;
        FaceId::of(Instance {
            index: best,
            weight: None,
            bold: false,
            oblique: false,
        })
    }

    /// How well face `index` answers `weight` and `style`, smaller better.
    fn rank(&self, index: usize, weight: Weight, style: Style) -> (u8, f32, u8) {
        let Some(info) = self.faces.get(index) else {
            return (u8::MAX, f32::MAX, u8::MAX);
        };
        let wanted = f32::from(weight.0);
        let have = match info.variable_weight {
            Some((least, most)) => wanted.max(least).min(most),
            None => f32::from(info.weight.0),
        };
        let (tier, distance) = weight_rank(wanted, have);
        (tier, distance, style_rank(style, info.style))
    }

    /// The families a generic name (`sans-serif`, `serif`, `monospace`,
    /// `system-ui`, `emoji`) stands for here, first choice first:
    /// fontconfig's own defaults (`DejaVu`, Liberation, Noto, the tree's
    /// Inter) as far as they are installed. `monospace` goes on to every
    /// other family whose faces say they are fixed-pitch.
    #[must_use]
    pub fn generic(&self, name: &str) -> Vec<String> {
        let Some((list, monospace)) = generic_list(&normalize(name)) else {
            return Vec::new();
        };
        let mut found: Vec<String> = list
            .iter()
            .filter(|family| self.families.contains_key(&normalize(family)))
            .map(|family| (*family).to_owned())
            .collect();
        if monospace {
            for info in self.faces.iter().filter(|info| info.monospace) {
                let Some(family) = info.families.first() else {
                    continue;
                };
                let key = normalize(family);
                if !found.iter().any(|seen| normalize(seen) == key) {
                    found.push(family.clone());
                }
            }
        }
        found
    }

    /// Resolve a description to the faces its glyphs come from: every
    /// family in order, then `sans-serif`'s, then every other family's
    /// best face as the last resort, so a character any installed face has
    /// is drawn.
    ///
    /// Each face carries how it is drawn at the description's weight and
    /// style: a variable face is instanced at the weight (clamped to its
    /// axis), a face at least 200 lighter than a weight of 600 or more is
    /// emboldened, and an upright face asked to be italic or oblique is
    /// slanted.
    #[must_use]
    pub fn resolve(&self, description: &FontDescription) -> Font {
        let (weight, style) = (description.weight, description.style);
        let mut chain: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        let mut push = |face: Option<FaceId>| {
            if let Some(face) = face
                && seen.insert(face.index())
            {
                chain.push(face.index());
            }
        };
        for family in &description.families {
            push(self.find(family, weight, style));
        }
        for family in self.generic("sans-serif") {
            push(self.find_named(&normalize(&family), weight, style));
        }
        for key in &self.order {
            push(self.find_named(key, weight, style));
        }
        let faces = chain
            .into_iter()
            .filter_map(|index| self.instance(index, weight, style))
            .collect();
        Font {
            faces,
            px: description.size.pixels(crate::DPI),
            weight,
            style,
        }
    }

    /// Face `index` as it is drawn for `weight` and `style`.
    fn instance(&self, index: usize, weight: Weight, style: Style) -> Option<FaceId> {
        let info = self.faces.get(index)?;
        let extra = self.extra.get(index)?;
        let wanted = f32::from(weight.0);
        let (instance_weight, effective) = match info.variable_weight {
            Some((least, most)) => {
                let at = wanted.max(least).min(most).max(1.0);
                let differs = (at - extra.default_weight).abs() >= 0.5;
                (differs.then(|| at.round() as u16), at)
            }
            None => (None, f32::from(info.weight.0)),
        };
        FaceId::of(Instance {
            index,
            weight: instance_weight,
            bold: weight.0 >= 600 && wanted - effective >= 200.0,
            oblique: style != Style::Normal && info.style == Style::Normal,
        })
    }

    /// Face `face`'s file and its index in it, read now if it was not.
    fn load(&mut self, face: FaceId) -> Option<(Arc<[u8]>, u32)> {
        let index = face.index();
        let in_file = self.faces.get(index)?.index;
        let file = self.files.get_mut(self.extra.get(index)?.file)?;
        if let Some(data) = &file.data {
            return Some((Arc::clone(data), in_file));
        }
        if file.failed {
            return None;
        }
        if let Ok(bytes) = std::fs::read(&file.path) {
            let data: Arc<[u8]> = Arc::from(bytes);
            file.data = Some(Arc::clone(&data));
            Some((data, in_file))
        } else {
            file.failed = true;
            None
        }
    }

    /// Whether face `face` maps `character`.
    fn covers(&self, face: FaceId, character: u32) -> bool {
        self.extra
            .get(face.index())
            .is_some_and(|extra| scan::covers(&extra.coverage, character))
    }

    /// Ascent and descent of face `face` at `px`.
    fn vertical(&mut self, face: FaceId, px: f32) -> Option<(f32, f32)> {
        let key = (face.0, px.to_bits());
        if let Some(known) = self.vertical.get(&key) {
            return *known;
        }
        let found = self.load(face).and_then(|(data, index)| {
            let parsed = open(&data, index, face.instance())?;
            let scale = scale_of(&parsed, px);
            Some((
                (f32::from(parsed.ascender()) * scale).round(),
                (-f32::from(parsed.descender()) * scale).round(),
            ))
        });
        let _ = self.vertical.insert(key, found);
        found
    }

    /// Where face `face` draws its underline and strike-through at `px`.
    pub(crate) fn decoration(&mut self, face: FaceId, px: f32) -> Decoration {
        self.load(face)
            .and_then(|(data, index)| {
                let parsed = open(&data, index, face.instance())?;
                Some(decoration_of(&parsed, px))
            })
            .unwrap_or_else(|| {
                let metrics = fallback_metrics(px);
                Decoration {
                    underline_position: metrics.underline_position,
                    underline_thickness: metrics.underline_thickness,
                    strikeout_position: metrics.strikeout_position,
                    strikeout_thickness: metrics.strikeout_thickness,
                }
            })
    }

    /// The vertical metrics of a font's first face at its size.
    pub fn metrics(&mut self, font: &Font) -> Metrics {
        let px = font.px;
        let Some(face) = font.primary() else {
            return fallback_metrics(px);
        };
        let Some((data, index)) = self.load(face) else {
            return fallback_metrics(px);
        };
        let Some(parsed) = open(&data, index, face.instance()) else {
            return fallback_metrics(px);
        };
        let scale = scale_of(&parsed, px);
        // Hinted metrics, as Pango's are under GTK3 and cairo's defaults:
        // whole pixels (Ubuntu at 15 px: 13.98 and 2.84 are 14 and 3).
        let ascent = (f32::from(parsed.ascender()) * scale).round();
        let descent = (-f32::from(parsed.descender()) * scale).round();
        let line_gap = (f32::from(parsed.line_gap()).max(0.0) * scale).round();
        let lines = decoration_of(&parsed, px);
        let sample: f32 = shape_in(&parsed, face, SAMPLE, 0, px, Positions::Fractional)
            .iter()
            .map(|glyph| glyph.advance)
            .sum();
        let digit = ('0'..='9')
            .filter_map(|character| parsed.glyph_index(character))
            .filter_map(|glyph| parsed.glyph_hor_advance(glyph))
            .map(|advance| (f32::from(advance) * scale).round())
            .fold(0.0_f32, f32::max);
        Metrics {
            ascent,
            descent,
            line_gap,
            height: ascent + descent + line_gap,
            underline_position: lines.underline_position,
            underline_thickness: lines.underline_thickness,
            strikeout_position: lines.strikeout_position,
            strikeout_thickness: lines.strikeout_thickness,
            approximate_char_width: sample / SAMPLE.chars().count() as f32,
            approximate_digit_width: digit,
        }
    }

    /// Shape one line of `text`: `HarfBuzz`'s rules through rustybuzz, each
    /// stretch of characters the first face lacks shaped again in the next
    /// face that has them. A newline is not a line break here; it shapes
    /// as whatever the face has for it.
    pub fn shape(&mut self, font: &Font, text: &str) -> Run {
        let shaped = self.shape_chain(&font.faces, text, 0, font.px);
        self.run_of(font, &shaped)
    }

    /// `text` in the first of `faces`, each stretch of clusters it has no
    /// glyph for shaped again in the first later face that has the
    /// characters. Clusters are byte offsets from `base`.
    fn shape_chain(&mut self, faces: &[FaceId], text: &str, base: usize, px: f32) -> Vec<Shaped> {
        let Some((&first, rest)) = faces.split_first() else {
            return Vec::new();
        };
        if text.is_empty() {
            return Vec::new();
        }
        let Some(shaped) = self.shape_face(first, text, base, px) else {
            return self.shape_chain(rest, text, base, px);
        };
        let missing: HashSet<usize> = shaped
            .iter()
            .filter(|glyph| glyph.id == 0)
            .map(|glyph| glyph.cluster)
            .collect();
        if rest.is_empty() || missing.is_empty() {
            return shaped;
        }
        let mut starts: Vec<usize> = shaped.iter().map(|glyph| glyph.cluster).collect();
        starts.sort_unstable();
        starts.dedup();
        let end = base + text.len();
        let end_of = |cluster: usize| {
            let after = starts.partition_point(|&start| start <= cluster);
            starts.get(after).copied().unwrap_or(end)
        };
        let mut out = Vec::with_capacity(shaped.len());
        let mut at = 0;
        while let Some(glyph) = shaped.get(at) {
            if !missing.contains(&glyph.cluster) {
                out.push(*glyph);
                at += 1;
                continue;
            }
            // The glyphs of a stretch of missing clusters, and the text
            // they came from.
            let run_start = at;
            let (mut low, mut high) = (usize::MAX, 0);
            while let Some(glyph) = shaped
                .get(at)
                .filter(|glyph| missing.contains(&glyph.cluster))
            {
                low = low.min(glyph.cluster);
                high = high.max(end_of(glyph.cluster));
                at += 1;
            }
            let piece = text
                .get(low.saturating_sub(base)..high.saturating_sub(base))
                .unwrap_or("");
            match self.fallback_face(rest, piece) {
                Some(next) => {
                    let later = rest.get(next..).unwrap_or(&[]);
                    out.extend(self.shape_chain(later, piece, low, px));
                }
                None => out.extend(shaped.get(run_start..at).unwrap_or(&[]).iter().copied()),
            }
        }
        out
    }

    /// Which of `faces` to shape `piece` in next: the first that has all
    /// of its characters, else the first that has any.
    fn fallback_face(&self, faces: &[FaceId], piece: &str) -> Option<usize> {
        let ignorable = |character: char| {
            matches!(
                u32::from(character),
                0x200B..=0x200F | 0x2060..=0x206F | 0xFE00..=0xFE0F | 0xE0000..=0xE0FFF
            ) || character.is_control()
        };
        let mut characters: Vec<u32> = piece
            .chars()
            .filter(|&character| !ignorable(character) && !character.is_whitespace())
            .map(u32::from)
            .collect();
        if characters.is_empty() {
            characters = piece.chars().map(u32::from).collect();
        }
        faces
            .iter()
            .position(|&face| {
                characters
                    .iter()
                    .all(|&character| self.covers(face, character))
            })
            .or_else(|| {
                faces.iter().position(|&face| {
                    characters
                        .iter()
                        .any(|&character| self.covers(face, character))
                })
            })
    }

    /// `text` shaped in one face; `None` when the face cannot be read.
    fn shape_face(
        &mut self,
        face: FaceId,
        text: &str,
        base: usize,
        px: f32,
    ) -> Option<Vec<Shaped>> {
        let (data, index) = self.load(face)?;
        let parsed = open(&data, index, face.instance())?;
        Some(shape_in(&parsed, face, text, base, px, Positions::Whole))
    }

    /// Shaped glyphs placed along a run.
    fn run_of(&mut self, font: &Font, shaped: &[Shaped]) -> Run {
        let mut glyphs = Vec::with_capacity(shaped.len());
        let mut pen = 0.0;
        let mut faces: Vec<FaceId> = font.primary().into_iter().collect();
        for each in shaped {
            glyphs.push(Glyph {
                face: each.face,
                id: each.id,
                x: pen + each.x_offset,
                y: each.y_offset,
                advance: each.advance,
                cluster: each.cluster,
            });
            pen += each.advance;
            if !faces.contains(&each.face) {
                faces.push(each.face);
            }
        }
        let mut vertical: Option<(f32, f32)> = None;
        for face in faces {
            if let Some((ascent, descent)) = self.vertical(face, font.px) {
                vertical = Some(vertical.map_or((ascent, descent), |(most_up, most_down)| {
                    (most_up.max(ascent), most_down.max(descent))
                }));
            }
        }
        let (ascent, descent) = vertical.unwrap_or((font.px * 0.8, font.px * 0.2));
        Run {
            glyphs,
            width: pen,
            ascent,
            descent,
            px: font.px,
        }
    }

    /// `…` in `font`, from whichever of its faces has it, or `...` where
    /// none does.
    pub(crate) fn ellipsis(&mut self, font: &Font) -> Run {
        let run = self.shape(font, "\u{2026}");
        if run.glyphs.is_empty() || run.glyphs.iter().any(|glyph| glyph.id == 0) {
            self.shape(font, "...")
        } else {
            run
        }
    }

    /// `text` shaped and cut to fit `max_width`, with `…` (from whichever
    /// face has it) where the cut is.
    pub fn shape_ellipsized(
        &mut self,
        font: &Font,
        text: &str,
        max_width: f32,
        at: crate::Ellipsize,
    ) -> Run {
        let run = self.shape(font, text);
        if at == crate::Ellipsize::None || run.width <= max_width {
            return run;
        }
        let ellipsis = self.ellipsis(font);
        let cut = crate::layout::ellipsize(&[run], |_| ellipsis.clone(), max_width, at, text.len());
        crate::layout::join(cut.pieces.into_iter().map(|(_, run)| run))
    }

    /// The width `text` would take in `font`: [`Fonts::shape`]'s width.
    pub fn measure(&mut self, font: &Font, text: &str) -> f32 {
        self.shape(font, text).width
    }

    /// One glyph's coverage at `px`, its pen `x_fraction` of a pixel right
    /// of a whole pixel (quantised to quarters). Cached. `None` for a glyph
    /// with no outline, a space.
    pub fn glyph(&mut self, face: FaceId, id: u16, px: f32, x_fraction: f32) -> Option<&Mask> {
        let quarter = (x_fraction * 4.0).round().clamp(0.0, 3.0) as u8;
        let key = GlyphKey {
            face: face.0,
            id,
            px: px.to_bits(),
            quarter,
        };
        if !self.glyphs.contains_key(&key) {
            let mask = self.load(face).and_then(|(data, index)| {
                let instance = face.instance();
                let parsed = open(&data, index, instance)?;
                crate::raster::rasterise(
                    &parsed,
                    id,
                    px,
                    f32::from(quarter) / 4.0,
                    instance.bold,
                    instance.oblique.then_some(OBLIQUE_SKEW),
                )
            });
            if self.glyphs.len() >= GLYPH_CACHE {
                self.glyphs.clear();
            }
            let _ = self.glyphs.insert(key, mask);
        }
        self.glyphs.get(&key)?.as_ref()
    }

    /// Draw `run` with its baseline starting at (`x`, `baseline`) in
    /// `color`, blended source-over into premultiplied `pixmap`.
    ///
    /// Each pen position is quantised to a quarter pixel across and the
    /// baseline rounded to a whole pixel down.
    pub fn draw_run(
        &mut self,
        run: &Run,
        pixmap: &mut tiny_skia::PixmapMut<'_>,
        x: f32,
        baseline: f32,
        color: Rgba,
    ) {
        self.draw_run_clipped(
            run,
            pixmap,
            (x, baseline),
            color,
            (i64::MIN, i64::MIN, i64::MAX, i64::MAX),
        );
    }

    /// [`Fonts::draw_run`], with only the pixels inside `clip` drawn:
    /// columns `clip.0..clip.2` and rows `clip.1..clip.3`, in the
    /// pixmap's pixels. A scrolled list's rows stop at its edge this way
    /// without a layer to draw them into.
    pub fn draw_run_clipped(
        &mut self,
        run: &Run,
        pixmap: &mut tiny_skia::PixmapMut<'_>,
        (x, baseline): (f32, f32),
        color: Rgba,
        clip: (i64, i64, i64, i64),
    ) {
        for glyph in &run.glyphs {
            let pen = x + glyph.x;
            let whole = pen.floor();
            let mut quarter = ((pen - whole) * 4.0).round();
            let mut column = whole as i64;
            if quarter >= 4.0 {
                quarter = 0.0;
                column += 1;
            }
            let row = (baseline - glyph.y).round() as i64;
            if let Some(mask) = self.glyph(glyph.face, glyph.id, run.px, quarter / 4.0) {
                crate::raster::blend_mask(
                    pixmap,
                    mask,
                    (column + i64::from(mask.left), row - i64::from(mask.top)),
                    color,
                    clip,
                );
            }
        }
    }

    /// Lay out `spans` as `options` say.
    pub fn layout(&mut self, spans: &[Span], options: &LayoutOptions) -> Layout {
        crate::layout::lay_out(self, spans, options)
    }

    /// Draw a layout with its top-left at (`x`, `y`): each piece's
    /// background, then its glyphs, underline and strike-through.
    ///
    /// Every background is drawn before any glyph, so that one piece's
    /// background does not cover the overhang of the glyph before it.
    pub fn draw_layout(
        &mut self,
        layout: &Layout,
        pixmap: &mut tiny_skia::PixmapMut<'_>,
        x: f32,
        y: f32,
    ) {
        for line in &layout.lines {
            for piece in &line.pieces {
                if let Some(background) = piece.background {
                    let left = x + line.x + piece.x;
                    let top = y + line.y;
                    crate::raster::fill_rect(
                        pixmap,
                        (left, top),
                        (left + piece.run.width, top + line.height),
                        background,
                    );
                }
            }
        }
        for line in &layout.lines {
            for piece in &line.pieces {
                let left = x + line.x + piece.x;
                let baseline = y + line.baseline - piece.rise;
                self.draw_run(&piece.run, pixmap, left, baseline, piece.color);
                self.draw_decorations(piece, pixmap, left, baseline);
            }
        }
    }

    /// A piece's underline and strike-through, at the pixel positions its
    /// first glyph's face asks for.
    fn draw_decorations(
        &mut self,
        piece: &crate::Piece,
        pixmap: &mut tiny_skia::PixmapMut<'_>,
        left: f32,
        baseline: f32,
    ) {
        if piece.underline == Underline::None && !piece.strikethrough {
            return;
        }
        let Some(first) = piece.run.glyphs.first() else {
            return;
        };
        let lines = self.decoration(first.face, piece.run.px);
        let right = left + piece.run.width;
        let baseline = baseline.round();
        let thick = |thickness: f32| thickness.round().max(1.0);
        if piece.underline != Underline::None {
            let color = piece.underline_color.unwrap_or(piece.color);
            let thickness = thick(lines.underline_thickness);
            let top = match piece.underline {
                Underline::Low => baseline + self.ink_bottom(&piece.run) + thickness,
                _ => baseline + lines.underline_position.round(),
            };
            crate::raster::fill_rect(pixmap, (left, top), (right, top + thickness), color);
            if piece.underline == Underline::Double {
                let second = top + 2.0 * thickness;
                crate::raster::fill_rect(
                    pixmap,
                    (left, second),
                    (right, second + thickness),
                    color,
                );
            }
        }
        if piece.strikethrough {
            let thickness = thick(lines.strikeout_thickness);
            let top = baseline - lines.strikeout_position.round();
            crate::raster::fill_rect(pixmap, (left, top), (right, top + thickness), piece.color);
        }
    }

    /// How far below the baseline a run's ink reaches, in whole pixels.
    fn ink_bottom(&mut self, run: &Run) -> f32 {
        let mut bottom = 0_i64;
        for glyph in &run.glyphs {
            if let Some(mask) = self.glyph(glyph.face, glyph.id, run.px, 0.0) {
                bottom = bottom.max(i64::from(mask.height) - i64::from(mask.top));
            }
        }
        bottom as f32
    }

    /// A layout drawn into a pixmap of its own logical size (at least one
    /// pixel each way), as hyprlock's label texture is.
    pub fn render(&mut self, layout: &Layout) -> Option<tiny_skia::Pixmap> {
        let (width, height) = layout.pixel_size();
        let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
        self.draw_layout(layout, &mut pixmap.as_mut(), 0.0, 0.0);
        Some(pixmap)
    }

    /// The glyph for `character` in the first face of `font` that has one.
    pub fn glyph_for(&mut self, font: &Font, character: char) -> Option<Glyph> {
        for &face in &font.faces {
            if !self.covers(face, u32::from(character)) {
                continue;
            }
            let Some((data, index)) = self.load(face) else {
                continue;
            };
            let Some(parsed) = open(&data, index, face.instance()) else {
                continue;
            };
            let Some(glyph) = parsed.glyph_index(character).filter(|glyph| glyph.0 != 0) else {
                continue;
            };
            let advance = parsed.glyph_hor_advance(glyph).unwrap_or(0);
            return Some(Glyph {
                face,
                id: glyph.0,
                x: 0.0,
                y: 0.0,
                advance: f32::from(advance) * scale_of(&parsed, font.px),
                cluster: 0,
            });
        }
        None
    }
}
