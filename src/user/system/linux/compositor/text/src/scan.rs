//! Reading a font file's faces for matching: the name, `OS/2`, `post`,
//! `fvar` and `cmap` tables, and nothing of the outlines.
//!
//! A scan of `/usr/share/fonts` meets hundreds of megabytes of files, so a
//! file is not read whole here: its table directory is, and then only the
//! tables matching needs, which [`ttf_parser::Face::from_raw_tables`]
//! takes one by one. The outlines are read when a glyph is first wanted.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use ttf_parser::{Face, PlatformId, RawFaceTables, Tag};

use crate::{Style, Weight};

/// What matching needs of one face, as read from its tables.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scanned {
    /// Family names, typographic then legacy, English first.
    pub(crate) families: Vec<String>,
    /// The subfamily.
    pub(crate) style_name: String,
    /// The full name.
    pub(crate) full_name: String,
    /// `OS/2.usWeightClass`.
    pub(crate) weight: Weight,
    /// Italic, oblique or upright.
    pub(crate) style: Style,
    /// `post.isFixedPitch`.
    pub(crate) monospace: bool,
    /// The `wght` axis: least, default, most.
    pub(crate) weight_axis: Option<(f32, f32, f32)>,
    /// The characters the face maps, as inclusive ranges, sorted.
    pub(crate) coverage: Vec<(u32, u32)>,
    /// Its index in its file.
    pub(crate) index: u32,
}

/// The largest table read while scanning. A `cmap` of a CJK face is a few
/// hundred kilobytes; this bounds what a corrupt directory can allocate.
const LARGEST_TABLE: u32 = 32 << 20;

/// The tables a scan reads.
const WANTED: [&[u8; 4]; 8] = [
    b"head", b"hhea", b"maxp", b"cmap", b"name", b"OS/2", b"post", b"fvar",
];

/// Every face in the file at `path`; none for a file that is not a font.
pub(crate) fn scan_file(path: &Path) -> Vec<Scanned> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Some(header) = read_at(&mut file, 0, 12) else {
        return Vec::new();
    };
    let offsets: Vec<u64> = if header.get(0..4) == Some(b"ttcf".as_slice()) {
        let count = be_u32(&header, 8).unwrap_or(0).min(1024);
        let Some(table) = read_at(&mut file, 12, count.saturating_mul(4)) else {
            return Vec::new();
        };
        (0..count)
            .filter_map(|face| be_u32(&table, face as usize * 4).map(u64::from))
            .collect()
    } else {
        vec![0]
    };
    let mut faces = Vec::new();
    for (index, offset) in offsets.into_iter().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            break;
        };
        if let Some(face) = scan_face(&mut file, offset, index) {
            faces.push(face);
        }
    }
    faces
}

/// One face of a file, from its table directory at `offset`.
fn scan_face(file: &mut File, offset: u64, index: u32) -> Option<Scanned> {
    let header = read_at(file, offset, 12)?;
    let version = header.get(0..4)?;
    if ![b"\x00\x01\x00\x00".as_slice(), b"OTTO", b"true"].contains(&version) {
        return None;
    }
    let tables = u32::from(be_u16(&header, 4)?);
    let directory = read_at(file, offset + 12, tables * 16)?;
    let mut read: Vec<(&[u8; 4], Vec<u8>)> = Vec::new();
    for record in directory.as_chunks::<16>().0 {
        let Some(tag) = WANTED
            .iter()
            .find(|wanted| record.get(0..4) == Some(wanted.as_slice()))
        else {
            continue;
        };
        let at = be_u32(record, 8)?;
        let length = be_u32(record, 12)?;
        if length > LARGEST_TABLE {
            continue;
        }
        read.push((tag, read_at(file, u64::from(at), length)?));
    }
    let table = |tag: &[u8; 4]| {
        read.iter()
            .find(|(each, _)| *each == tag)
            .map(|(_, data)| data.as_slice())
    };
    let raw = RawFaceTables {
        head: table(b"head")?,
        hhea: table(b"hhea")?,
        maxp: table(b"maxp")?,
        cmap: table(b"cmap"),
        name: table(b"name"),
        os2: table(b"OS/2"),
        post: table(b"post"),
        fvar: table(b"fvar"),
        ..RawFaceTables::default()
    };
    let face = Face::from_raw_tables(raw).ok()?;
    Some(describe(&face, index))
}

/// What a parsed face says of itself.
pub(crate) fn describe(face: &Face<'_>, index: u32) -> Scanned {
    let families = names(face, &[16, 1]);
    let style_name = names(face, &[17, 2]).into_iter().next().unwrap_or_default();
    let full_name = names(face, &[4]).into_iter().next().unwrap_or_default();
    let mut weight = face.weight().to_number();
    // A handful of old faces write the weight on a 1-9 scale.
    if (1..10).contains(&weight) {
        weight *= 100;
    }
    if weight == 0 {
        weight = 400;
    }
    let lower = style_name.to_ascii_lowercase();
    let style = match face.style() {
        ttf_parser::Style::Italic => Style::Italic,
        ttf_parser::Style::Oblique => Style::Oblique,
        ttf_parser::Style::Normal if lower.contains("italic") => Style::Italic,
        ttf_parser::Style::Normal if lower.contains("oblique") => Style::Oblique,
        ttf_parser::Style::Normal => Style::Normal,
    };
    let weight_axis = face
        .variation_axes()
        .into_iter()
        .find(|axis| axis.tag == Tag::from_bytes(b"wght"))
        .map(|axis| (axis.min_value, axis.def_value, axis.max_value));
    Scanned {
        families,
        style_name,
        full_name,
        weight: Weight(weight.min(1000)),
        style,
        monospace: face.is_monospaced(),
        weight_axis,
        coverage: coverage(face),
        index,
    }
}

/// The names with the ids `ids`, in that order of id, English first in
/// each, each once.
fn names(face: &Face<'_>, ids: &[u16]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for &id in ids {
        let mut english = Vec::new();
        let mut other = Vec::new();
        for name in face.names() {
            if name.name_id != id {
                continue;
            }
            let Some(text) = decode(&name) else {
                continue;
            };
            let text = text.trim().to_owned();
            if text.is_empty() {
                continue;
            }
            let is_english = match name.platform_id {
                PlatformId::Windows => name.language_id & 0xff == 0x09,
                PlatformId::Macintosh => name.language_id == 0,
                _ => true,
            };
            if is_english {
                english.push(text);
            } else {
                other.push(text);
            }
        }
        for text in english.into_iter().chain(other) {
            if !found.contains(&text) {
                found.push(text);
            }
        }
    }
    found
}

/// A name as text: UTF-16 where the platform says Unicode, and the ASCII
/// part of Mac Roman otherwise.
fn decode(name: &ttf_parser::name::Name<'_>) -> Option<String> {
    if name.is_unicode() {
        return name.to_string();
    }
    if name.platform_id == PlatformId::Macintosh && name.encoding_id == 0 {
        return Some(
            name.name
                .iter()
                .map(|&byte| {
                    if byte.is_ascii() {
                        char::from(byte)
                    } else {
                        '?'
                    }
                })
                .collect(),
        );
    }
    None
}

/// Every character a face's Unicode `cmap` subtables map, as ranges.
fn coverage(face: &Face<'_>) -> Vec<(u32, u32)> {
    let mut points: Vec<u32> = Vec::new();
    if let Some(cmap) = face.tables().cmap {
        for subtable in cmap.subtables {
            if subtable.is_unicode() {
                subtable.codepoints(|point| points.push(point));
            }
        }
    }
    points.sort_unstable();
    points.dedup();
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for point in points {
        match ranges.last_mut() {
            Some((_, end)) if end.checked_add(1) == Some(point) => *end = point,
            _ => ranges.push((point, point)),
        }
    }
    ranges
}

/// Whether `ranges` (from [`coverage`]) hold `point`.
pub(crate) fn covers(ranges: &[(u32, u32)], point: u32) -> bool {
    let after = ranges.partition_point(|&(start, _)| start <= point);
    after
        .checked_sub(1)
        .and_then(|at| ranges.get(at))
        .is_some_and(|&(_, end)| point <= end)
}

/// `length` bytes of `file` from `offset`.
fn read_at(file: &mut File, offset: u64, length: u32) -> Option<Vec<u8>> {
    let _ = file.seek(SeekFrom::Start(offset)).ok()?;
    let mut data = vec![0; usize::try_from(length).ok()?];
    file.read_exact(&mut data).ok()?;
    Some(data)
}

/// A big-endian `u32` at `at`.
fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// A big-endian `u16` at `at`.
fn be_u16(data: &[u8], at: usize) -> Option<u16> {
    let bytes: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
    Some(u16::from_be_bytes(bytes))
}
