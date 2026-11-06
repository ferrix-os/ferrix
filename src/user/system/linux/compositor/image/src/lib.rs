//! Pictures for Ferrix's desktop clients.
//!
//! waybar draws its chips from SVG `background-image`s, fuzzel draws each
//! application's icon (PNG or SVG, from the icon theme), hyprlock draws a
//! background and an `image { }` from a PNG or JPEG. All of them come back
//! from here as a `tiny_skia::Pixmap`: premultiplied RGBA, ready to be drawn
//! onto a surface with `draw_pixmap` or used as a pattern.
//!
//! SVG is resvg's, which renders with the same tiny-skia the rest of the
//! desktop draws with. See `Cargo.toml` for why a whole renderer rather
//! than a subset.

use std::path::Path;

pub use tiny_skia;

/// What a picture file is, by its first bytes (not its name).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `\x89PNG`.
    Png,
    /// `\xFF\xD8\xFF`.
    Jpeg,
    /// An XML document with an `<svg` root, or gzip (`.svgz`) -- which is
    /// refused, since the decompressor is not carried.
    Svg,
}

/// Why a picture could not be had.
#[derive(Debug)]
pub enum Error {
    /// The file could not be read.
    Io(std::io::Error),
    /// Not a format this reads.
    Unknown,
    /// The decoder refused it, in its words.
    Decode(String),
    /// A size of zero, or one too large to allocate.
    Size(u32, u32),
}

impl core::fmt::Display for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Unknown => formatter.write_str("not a PNG, JPEG or SVG"),
            Self::Decode(why) => formatter.write_str(why),
            Self::Size(width, height) => write!(formatter, "a {width}x{height} picture"),
        }
    }
}

impl std::error::Error for Error {}

/// How big to make an SVG, which has no pixels of its own.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fit {
    /// Its own `width` and `height` (or its `viewBox`'s).
    Natural,
    /// Scaled to fit inside this box, keeping its shape; the pixmap is the
    /// scaled size.
    Within(u32, u32),
    /// Exactly this size. How the drawing fills it is the SVG's own
    /// `preserveAspectRatio`: `none` stretches it (waybar's chip caps), the
    /// default centres it.
    Exactly(u32, u32),
}

/// What a file is, by its first bytes.
#[must_use]
pub fn sniff(bytes: &[u8]) -> Option<Kind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Kind::Png);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Kind::Jpeg);
    }
    if bytes.starts_with(&[0x1F, 0x8B]) {
        return Some(Kind::Svg);
    }
    // An XML document whose root is `<svg`: a BOM, a declaration, comments
    // and a doctype may come first, and gdk-pixbuf's "first few hundred
    // bytes" is not a rule this needs to copy.
    let text = core::str::from_utf8(bytes.get(..bytes.len().min(64 * 1024))?).ok()?;
    root_tag(text).map(|_| Kind::Svg)
}

/// Read a file of any kind this knows. A raster picture comes back at its
/// own size, whatever `fit` says (scale it when drawing); an SVG at `fit`.
///
/// # Errors
///
/// As [`Error`].
pub fn load(path: &Path, fit: Fit) -> Result<tiny_skia::Pixmap, Error> {
    let bytes = std::fs::read(path).map_err(Error::Io)?;
    decode(&bytes, fit)
}

/// Decode bytes of any kind this knows, as [`load`].
///
/// # Errors
///
/// As [`Error`].
pub fn decode(bytes: &[u8], fit: Fit) -> Result<tiny_skia::Pixmap, Error> {
    match sniff(bytes).ok_or(Error::Unknown)? {
        Kind::Png => png(bytes),
        Kind::Jpeg => jpeg(bytes),
        Kind::Svg => svg(bytes, fit),
    }
}

/// Decode a PNG.
///
/// # Errors
///
/// The decoder's.
pub fn png(bytes: &[u8]) -> Result<tiny_skia::Pixmap, Error> {
    tiny_skia::Pixmap::decode_png(bytes).map_err(|error| Error::Decode(format!("PNG: {error}")))
}

/// Decode a JPEG.
///
/// # Errors
///
/// The decoder's.
pub fn jpeg(bytes: &[u8]) -> Result<tiny_skia::Pixmap, Error> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder =
        zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let pixels = decoder
        .decode()
        .map_err(|error| Error::Decode(format!("JPEG: {error:?}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| Error::Decode("JPEG: no header".to_owned()))?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    let size = tiny_skia::IntSize::from_wh(width, height).ok_or(Error::Size(width, height))?;
    // Opaque, so RGBA is already premultiplied.
    tiny_skia::Pixmap::from_vec(pixels, size).ok_or(Error::Size(width, height))
}

/// Render an SVG.
///
/// # Errors
///
/// usvg's refusal, or a size of zero.
pub fn svg(bytes: &[u8], fit: Fit) -> Result<tiny_skia::Pixmap, Error> {
    let options = resvg::usvg::Options::default();
    let tree = parse(bytes, &options)?;
    let natural = tree.size();
    let (width, height) = (natural.width(), natural.height());
    match fit {
        Fit::Natural => render(&tree, pixels(width), pixels(height), 1.0, 1.0),
        Fit::Within(most_width, most_height) => {
            let scale = (most_width as f32 / width).min(most_height as f32 / height);
            let (out_width, out_height) = (pixels(width * scale), pixels(height * scale));
            render(&tree, out_width, out_height, scale, scale)
        }
        Fit::Exactly(out_width, out_height) => {
            // The drawing is laid into the box by the file's own
            // `preserveAspectRatio`, which usvg applies between the viewBox
            // and the root's width and height: so the root is given the
            // box's size and parsed again. A file with no viewBox is given
            // one of its own size first, or the new size would crop it.
            let text = core::str::from_utf8(bytes)
                .map_err(|_| Error::Decode("SVG: not UTF-8".to_owned()))?;
            let resized = resize_root(text, out_width, out_height, (width, height))
                .ok_or_else(|| Error::Decode("SVG: no <svg> root".to_owned()))?;
            let tree = parse(resized.as_bytes(), &options)?;
            let size = tree.size();
            render(
                &tree,
                out_width,
                out_height,
                out_width as f32 / size.width(),
                out_height as f32 / size.height(),
            )
        }
    }
}

/// An SVG's own size in its user units: `width` × `height`, or its
/// `viewBox`'s.
///
/// # Errors
///
/// usvg's refusal.
pub fn svg_size(bytes: &[u8]) -> Result<(f32, f32), Error> {
    let tree = parse(bytes, &resvg::usvg::Options::default())?;
    Ok((tree.size().width(), tree.size().height()))
}

/// Copy a pixmap into `wl_shm` `ARGB8888` bytes (premultiplied, little
/// endian: B, G, R, A), `stride` bytes a row.
pub fn to_argb8888(pixmap: &tiny_skia::PixmapRef<'_>, out: &mut [u8], stride: usize) {
    let row = pixmap.width() as usize * 4;
    if row == 0 {
        return;
    }
    for (from, to) in pixmap
        .data()
        .chunks_exact(row)
        .zip(out.chunks_mut(stride.max(1)))
    {
        for (&[r, g, b, a], target) in from
            .as_chunks::<4>()
            .0
            .iter()
            .zip(to.as_chunks_mut::<4>().0)
        {
            *target = [b, g, r, a];
        }
    }
}

fn parse(bytes: &[u8], options: &resvg::usvg::Options<'_>) -> Result<resvg::usvg::Tree, Error> {
    if bytes.starts_with(&[0x1F, 0x8B]) {
        return Err(Error::Decode(
            "SVG: compressed (.svgz), which this does not read".to_owned(),
        ));
    }
    resvg::usvg::Tree::from_data(bytes, options)
        .map_err(|error| Error::Decode(format!("SVG: {error}")))
}

fn render(
    tree: &resvg::usvg::Tree,
    width: u32,
    height: u32,
    scale_x: f32,
    scale_y: f32,
) -> Result<tiny_skia::Pixmap, Error> {
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or(Error::Size(width, height))?;
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale_x, scale_y),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

/// A size in user units as whole pixels, at least one.
fn pixels(value: f32) -> u32 {
    let rounded = value.ceil();
    if rounded.is_nan() || rounded < 1.0 {
        1
    } else if rounded > 16384.0 {
        16384
    } else {
        rounded as u32
    }
}

/// Where the root `<svg ...>` start tag is: its byte range, `<` to `>`.
fn root_tag(text: &str) -> Option<(usize, usize)> {
    let mut at = 0;
    loop {
        let rest = text.get(at..)?;
        let open = at + rest.find('<')?;
        let after = text.get(open..)?;
        if after.starts_with("<!--") {
            at = open + after.find("-->")? + 3;
        } else if after.starts_with("<?") || after.starts_with("<!") {
            at = open + after.find('>')? + 1;
        } else if after.starts_with("<svg") {
            return Some((open, open + tag_end(after)? + 1));
        } else {
            // Another root element: not an SVG.
            return None;
        }
    }
}

/// The offset of the `>` that ends the tag `tag` starts with, quotes
/// respected.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (at, character) in tag.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), close) if open == close => quote = None,
            (None, '>') => return Some(at),
            _ => {}
        }
    }
    None
}

/// `text` with the root tag's `width` and `height` replaced by the given
/// size, and a `viewBox` of `natural` added where it had none.
fn resize_root(text: &str, width: u32, height: u32, natural: (f32, f32)) -> Option<String> {
    let (start, end) = root_tag(text)?;
    let tag = text.get(start..end)?;
    let inner = tag.strip_prefix("<svg")?.strip_suffix('>')?;
    let (inner, closed) = match inner.strip_suffix('/') {
        Some(inner) => (inner, "/>"),
        None => (inner, ">"),
    };
    let mut attributes = Vec::new();
    let mut has_view_box = false;
    for (name, value) in attributes_of(inner) {
        match name {
            "width" | "height" => {}
            _ => {
                has_view_box |= name == "viewBox";
                attributes.push(format!("{name}=\"{value}\""));
            }
        }
    }
    if !has_view_box {
        attributes.push(format!("viewBox=\"0 0 {} {}\"", natural.0, natural.1));
    }
    attributes.push(format!("width=\"{width}\""));
    attributes.push(format!("height=\"{height}\""));
    let before = text.get(..start)?;
    let after = text.get(end..)?;
    Some(format!(
        "{before}<svg {}{closed}{after}",
        attributes.join(" ")
    ))
}

/// A start tag's attributes, as `name="value"` pairs (either quote).
fn attributes_of(inner: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    let mut rest = inner;
    loop {
        rest = rest.trim_start();
        let Some(equals) = rest.find('=') else {
            break;
        };
        let name = rest.get(..equals).unwrap_or("").trim();
        let after = rest.get(equals + 1..).unwrap_or("").trim_start();
        let Some(quote) = after
            .chars()
            .next()
            .filter(|quote| *quote == '"' || *quote == '\'')
        else {
            break;
        };
        let Some(close) = after.get(1..).and_then(|value| value.find(quote)) else {
            break;
        };
        found.push((name, after.get(1..=close).unwrap_or("")));
        rest = after.get(close + 2..).unwrap_or("");
    }
    found
}

#[cfg(test)]
mod tests;
