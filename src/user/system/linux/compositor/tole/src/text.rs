//! Text through the desktop's own `compositor-text` (`docs/UI-FRAMEWORK.md`
//! §4.5): fonts resolved once per font and scale, and runs shaped once per
//! font, scale and text, as the lode viewer's `draw::Text` keeps them.

use std::collections::HashMap;
use std::rc::Rc;

use compositor_text::{Ellipsize, FontDescription, Fonts, Run, Size, Weight};

use crate::value::Font;

/// How many shaped runs are kept before the cache starts again.
const RUNS_MAX: usize = 6000;

/// A font at a scale, as a cache key: families, size and scale in
/// quarters, weight.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    /// The families.
    families: Rc<str>,
    /// Quarter logical pixels.
    quarters: u32,
    /// The weight.
    weight: u16,
    /// Quarter buffer pixels to a logical one.
    scale: u32,
}

impl Key {
    /// The key of `font` at `scale`.
    fn of(font: &Font, scale: f32) -> Self {
        Key {
            families: Rc::clone(&font.families),
            quarters: (font.px * 4.0).max(0.0) as u32,
            weight: font.weight,
            scale: (scale * 4.0).round().max(1.0) as u32,
        }
    }

    /// The size in buffer pixels.
    fn px(&self) -> f32 {
        self.quarters as f32 / 4.0 * (self.scale as f32 / 4.0)
    }
}

/// Fonts and what was shaped with them.
pub(crate) struct TextEngine {
    /// Every face found.
    pub(crate) fonts: Fonts,
    /// Resolved fonts.
    resolved: HashMap<Key, compositor_text::Font>,
    /// Shaped runs, by font and text.
    runs: HashMap<(Key, Rc<str>), Rc<Run>>,
}

impl std::fmt::Debug for TextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEngine")
            .field("faces", &self.fonts.faces().len())
            .field("runs", &self.runs.len())
            .finish_non_exhaustive()
    }
}

impl TextEngine {
    /// Text drawn with `fonts`.
    pub(crate) fn new(fonts: Fonts) -> Self {
        TextEngine {
            fonts,
            resolved: HashMap::new(),
            runs: HashMap::new(),
        }
    }

    /// How many faces there are.
    pub(crate) fn faces(&self) -> usize {
        self.fonts.faces().len()
    }

    /// The resolved font for `key`.
    fn font(&mut self, key: &Key) -> compositor_text::Font {
        if let Some(font) = self.resolved.get(key) {
            return font.clone();
        }
        let families: Vec<&str> = key
            .families
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        let mut description = FontDescription::new(&families, Size::Pixels(key.px()));
        description.weight = Weight(key.weight);
        let font = self.fonts.resolve(&description);
        let _ = self.resolved.insert(key.clone(), font.clone());
        font
    }

    /// `text` in `font`, shaped at `scale`: in buffer pixels.
    pub(crate) fn run(&mut self, font: &Font, scale: f32, text: &Rc<str>) -> Rc<Run> {
        let key = Key::of(font, scale);
        let cache_key = (key, Rc::clone(text));
        if let Some(run) = self.runs.get(&cache_key) {
            return Rc::clone(run);
        }
        if self.runs.len() >= RUNS_MAX {
            self.runs.clear();
        }
        let resolved = self.font(&cache_key.0);
        let run = Rc::new(self.fonts.shape(&resolved, text));
        let _ = self.runs.insert(cache_key, Rc::clone(&run));
        run
    }

    /// The same, cut with `…` to `max` buffer pixels when wider.
    pub(crate) fn run_within(
        &mut self,
        font: &Font,
        scale: f32,
        text: &Rc<str>,
        max: f32,
    ) -> Rc<Run> {
        let run = self.run(font, scale, text);
        if run.width <= max {
            return run;
        }
        let resolved = self.font(&Key::of(font, scale));
        Rc::new(
            self.fonts
                .shape_ellipsized(&resolved, text, max.max(1.0), Ellipsize::End),
        )
    }

    /// `font`'s ascent and descent at `scale`, in buffer pixels.
    pub(crate) fn extent(&mut self, font: &Font, scale: f32) -> (f32, f32) {
        let resolved = self.font(&Key::of(font, scale));
        let metrics = self.fonts.metrics(&resolved);
        (metrics.ascent, metrics.descent)
    }
}
