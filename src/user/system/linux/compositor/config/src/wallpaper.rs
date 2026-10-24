//! `wallpaper = <output>, <field>, <field>…`: what is behind the windows,
//! a Ferrix keyword (`docs/RUNTIME-SETTINGS.md` §3.3) the compositor
//! carries out by starting the wallpaper client with the lines in force.
//!
//! Each field is a word and its value, the value being everything after
//! the first space, so a path may hold spaces (but not a comma):
//!
//! ```text
//! wallpaper = , picture /usr/share/ferrix/wallpapers/ferrix.png, fit fill, colour 1d2027
//! wallpaper = DP-1, slideshow ~/Pictures/Wallpapers, every 1800, shuffle 1
//! wallpaper = HDMI-A-1, colour rgb(203040)
//! wallpaper = DP-1, off
//! ```
//!
//! An empty output is every screen that has no line of its own. A line for
//! an output takes the place of the one before it for the same output, so
//! a file that says it twice, or a keyword after the file, means the last;
//! `off` takes the output's line away.

use crate::value;

/// How a picture is fitted to a screen of another shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WallpaperFit {
    /// Scaled to cover the screen, the overflow cut evenly: what the
    /// wallpaper client always did.
    #[default]
    Fill,
    /// Scaled to fit inside the screen, the colour around it.
    Fit,
    /// Scaled to the screen's shape, whatever that does to the picture.
    Stretch,
    /// Repeated at its own size from the top left.
    Tile,
    /// At its own size in the middle, the colour around it.
    Centre,
}

impl WallpaperFit {
    /// The word a line writes.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Fit => "fit",
            Self::Stretch => "stretch",
            Self::Tile => "tile",
            Self::Centre => "centre",
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "fill" | "cover" => Ok(Self::Fill),
            "fit" | "contain" => Ok(Self::Fit),
            "stretch" => Ok(Self::Stretch),
            "tile" => Ok(Self::Tile),
            "centre" | "center" => Ok(Self::Centre),
            other => Err(format!(
                "wallpaper fit {other}: fill, fit, stretch, tile or centre"
            )),
        }
    }
}

/// What is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WallpaperSource {
    /// The colour alone.
    Colour,
    /// One picture: a PNG, JPEG or SVG.
    Picture(String),
    /// The pictures in a directory, one after another.
    Slideshow {
        /// The directory.
        folder: String,
        /// Seconds each is shown.
        every: u32,
        /// In a random order rather than by name.
        shuffle: bool,
    },
    /// A video, played over and over.
    Video(String),
}

/// One `wallpaper =` line, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallpaper {
    /// The output's name, empty for every screen with no line of its own.
    pub output: String,
    /// What is shown.
    pub source: WallpaperSource,
    /// How a picture is fitted.
    pub fit: WallpaperFit,
    /// The colour shown alone, and around a picture that does not cover
    /// the screen, as `0xAARRGGBB`.
    pub colour: u32,
    /// One picture across every screen as they are placed, rather than one
    /// on each: the empty output's line only.
    pub span: bool,
    /// A slideshow or a video waits while a window covers the screen.
    pub pause_covered: bool,
    /// And while the machine runs on its battery.
    pub pause_battery: bool,
}

/// The colour a wallpaper has when its line names none: Ferrix's dark.
pub const WALLPAPER_COLOUR: u32 = 0xff1d_2027;

/// The seconds a slideshow shows each picture when its line names none.
pub const SLIDESHOW_EVERY: u32 = 1800;

impl Wallpaper {
    /// Read the value of a `wallpaper =` line. `Ok(None)` is `off`: the
    /// output's line taken away. The output is `Some` for `off` too, in
    /// the second of the pair.
    ///
    /// # Errors
    ///
    /// A sentence naming the field that is wrong.
    pub fn parse(value: &str) -> Result<(String, Option<Self>), String> {
        let mut fields = value.split(',').map(str::trim);
        let output = fields.next().unwrap_or_default().to_owned();
        if output.contains(char::is_whitespace) {
            return Err(format!(
                "wallpaper: the first field is the output's name, not \"{output}\""
            ));
        }
        let mut source = None;
        let mut fit = WallpaperFit::default();
        let mut colour = None;
        let (mut span, mut pause_covered, mut pause_battery) = (false, true, true);
        let (mut every, mut shuffle) = (SLIDESHOW_EVERY, true);
        let mut off = false;
        for field in fields.filter(|field| !field.is_empty()) {
            let (word, rest) = field
                .split_once(char::is_whitespace)
                .map_or((field, ""), |(word, rest)| (word, rest.trim()));
            let wants = |what: &str| -> Result<(), String> {
                if rest.is_empty() {
                    Err(format!("wallpaper {word} needs {what}"))
                } else {
                    Ok(())
                }
            };
            let mut once = |chosen: WallpaperSource| -> Result<(), String> {
                if source.replace(chosen).is_some() {
                    Err("wallpaper: one of picture, slideshow and video".to_owned())
                } else {
                    Ok(())
                }
            };
            match word {
                "off" | "none" if rest.is_empty() => off = true,
                "picture" => {
                    wants("a file")?;
                    once(WallpaperSource::Picture(rest.to_owned()))?;
                }
                "video" => {
                    wants("a file")?;
                    once(WallpaperSource::Video(rest.to_owned()))?;
                }
                "slideshow" => {
                    wants("a directory")?;
                    once(WallpaperSource::Slideshow {
                        folder: rest.to_owned(),
                        every: 0,
                        shuffle: false,
                    })?;
                }
                "colour" | "color" => {
                    wants("a colour")?;
                    colour = Some(colour_of(rest)?);
                }
                "fit" => fit = WallpaperFit::parse(rest)?,
                "every" => {
                    every = seconds(rest)?;
                }
                "shuffle" => shuffle = flag(word, rest)?,
                "span" => span = flag(word, rest)?,
                "pause_covered" => pause_covered = flag(word, rest)?,
                "pause_battery" => pause_battery = flag(word, rest)?,
                other => {
                    return Err(format!(
                        "wallpaper: unknown field {other}; picture, colour, slideshow, video, \
                         fit, every, shuffle, span, pause_covered, pause_battery or off"
                    ));
                }
            }
        }
        if off {
            return Ok((output, None));
        }
        if span && !output.is_empty() {
            return Err("wallpaper span is for the line with no output".to_owned());
        }
        let source = match source {
            Some(WallpaperSource::Slideshow { folder, .. }) => WallpaperSource::Slideshow {
                folder,
                every,
                shuffle,
            },
            Some(chosen) => chosen,
            None if colour.is_some() => WallpaperSource::Colour,
            None => {
                return Err(
                    "wallpaper needs a picture, a slideshow, a video, a colour or off".to_owned(),
                );
            }
        };
        Ok((
            output.clone(),
            Some(Self {
                output,
                source,
                fit,
                colour: colour.unwrap_or(WALLPAPER_COLOUR),
                span,
                pause_covered,
                pause_battery,
            }),
        ))
    }

    /// The line's value as [`Self::parse`] reads it back: what the
    /// compositor hands the wallpaper client, one argument a line.
    #[must_use]
    pub fn line(&self) -> String {
        let mut fields = vec![self.output.clone()];
        match &self.source {
            WallpaperSource::Colour => {}
            WallpaperSource::Picture(path) => fields.push(format!("picture {path}")),
            WallpaperSource::Video(path) => fields.push(format!("video {path}")),
            WallpaperSource::Slideshow {
                folder,
                every,
                shuffle,
            } => {
                fields.push(format!("slideshow {folder}"));
                fields.push(format!("every {every}"));
                fields.push(format!("shuffle {}", u8::from(*shuffle)));
            }
        }
        if !matches!(self.source, WallpaperSource::Colour) {
            fields.push(format!("fit {}", self.fit.word()));
        }
        fields.push(format!("colour {:08x}", self.colour));
        if self.span {
            fields.push("span 1".to_owned());
        }
        if matches!(
            self.source,
            WallpaperSource::Slideshow { .. } | WallpaperSource::Video(_)
        ) {
            fields.push(format!("pause_covered {}", u8::from(self.pause_covered)));
            fields.push(format!("pause_battery {}", u8::from(self.pause_battery)));
        }
        fields.join(", ")
    }
}

/// A colour as hyprlang writes one -- `rgb(…)`, `rgba(…)`, `0xAARRGGBB` --
/// or as a colour picker does, `#rrggbb` or six bare hex digits, opaque.
fn colour_of(text: &str) -> Result<u32, String> {
    let bare = text.strip_prefix('#').unwrap_or(text);
    if bare.len() == 6 && bare.chars().all(|c| c.is_ascii_hexdigit()) {
        return u32::from_str_radix(bare, 16)
            .map(|rgb| 0xff00_0000 | rgb)
            .map_err(|error| error.to_string());
    }
    if bare.len() == 8 && bare.chars().all(|c| c.is_ascii_hexdigit()) {
        return u32::from_str_radix(bare, 16).map_err(|error| error.to_string());
    }
    value::parse_color(text).map(|colour| colour.0)
}

/// Seconds, at least one: `1800`, or with a unit, `30m`, `1h`, `1d`.
fn seconds(text: &str) -> Result<u32, String> {
    let (number, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, ""),
    };
    let scale = match unit.trim() {
        "" | "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        other => return Err(format!("wallpaper every: unknown unit {other}")),
    };
    number
        .parse::<u32>()
        .ok()
        .and_then(|count| count.checked_mul(scale))
        .filter(|&total| total > 0)
        .ok_or_else(|| format!("wallpaper every {text}: a number of seconds above 0"))
}

/// `1`, `0`, `true`, `false`, `yes`, `no`, `on`, `off`.
fn flag(word: &str, text: &str) -> Result<bool, String> {
    match text {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!("wallpaper {word} {other}: 1 or 0")),
    }
}
