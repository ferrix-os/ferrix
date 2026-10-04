//! The expected images the frame tests compare against, byte for byte, and
//! their file format.
//!
//! An image is `tests/data/<name>.xrle`: the frame as `XRGB8888` values,
//! run-length encoded, which keeps two tiled pattern clients at 1024x768 to
//! a few kilobytes where a PPM would be 2.3 MB.
//!
//! ```text
//! magic   "ferrix-xrgb-rle\n"          16 bytes
//! width   u32 little-endian
//! height  u32 little-endian
//! rows    height times:
//!           0x00                       the row is the row above's (not first)
//!           0x01 then runs             runs whose counts add up to width:
//!             count u16 little-endian  1 or more
//!             pixel u32 little-endian  the XRGB8888 value, X byte included
//! ```
//!
//! Nothing may follow the last row. The images are written only when
//! `COMPOSITOR_RENDER_BLESS` is set, and a test that writes one fails, so an
//! image is never replaced by a run that merely happened to have the
//! variable set: look at what was written (`COMPOSITOR_RENDER_PPM=<dir>`
//! writes each rendered frame there as a PPM too), commit it, and run the
//! tests again without the variable.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test's expected-image checker: its job is to stop the test with a clear message, \
              and every caller is a test"
)]

use std::path::PathBuf;

/// The file's first bytes.
const MAGIC: &[u8; 16] = b"ferrix-xrgb-rle\n";

/// The variable that makes the tests write their expected images.
pub const BLESS: &str = "COMPOSITOR_RENDER_BLESS";

/// The variable naming a directory to write every rendered frame to as PPM.
const PPM: &str = "COMPOSITOR_RENDER_PPM";

/// A pixel that is not what the expected image holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mismatch {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) expected: u32,
    pub(crate) actual: u32,
}

/// The pixels of `width`-wide `XRGB8888` bytes with no padding, as values.
fn values(data: &[u8]) -> impl Iterator<Item = u32> + '_ {
    data.chunks_exact(4)
        .map(|pixel| u32::from_le_bytes(pixel.try_into().unwrap()))
}

/// Encode a `width` × `height` frame of `XRGB8888` bytes with no padding.
pub(crate) fn encode(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
    assert_eq!(data.len(), width as usize * height as usize * 4);
    let mut out = MAGIC.to_vec();
    out.extend(width.to_le_bytes());
    out.extend(height.to_le_bytes());
    let mut previous: Option<&[u8]> = None;
    for row in data.chunks_exact(width as usize * 4) {
        if previous == Some(row) {
            out.push(0);
            continue;
        }
        out.push(1);
        let mut pixels = values(row).peekable();
        while let Some(pixel) = pixels.next() {
            let mut count: u16 = 1;
            while pixels.peek() == Some(&pixel) {
                let _ = pixels.next();
                count += 1;
            }
            out.extend(count.to_le_bytes());
            out.extend(pixel.to_le_bytes());
        }
        previous = Some(row);
    }
    out
}

/// The first `n` bytes of `rest`, which then starts after them.
fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if rest.len() < n {
        return Err(String::from("truncated"));
    }
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Ok(head)
}

/// Decode an image into its width, height and `XRGB8888` bytes.
pub(crate) fn decode(file: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut rest = file.strip_prefix(MAGIC).ok_or("no magic")?;
    let width = u32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap());
    let height = u32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap());
    let row_len = width as usize * 4;
    let mut data = Vec::with_capacity(row_len * height as usize);
    for y in 0..height {
        match take(&mut rest, 1)? {
            [0] if y > 0 => data.extend_from_within(data.len() - row_len..),
            [1] => {
                let mut filled = 0u32;
                while filled < width {
                    let count = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
                    let pixel: [u8; 4] = take(&mut rest, 4)?.try_into().unwrap();
                    if count == 0 || filled + u32::from(count) > width {
                        return Err(format!("row {y}: a run of {count} does not fit"));
                    }
                    for _ in 0..count {
                        data.extend(pixel);
                    }
                    filled += u32::from(count);
                }
            }
            tag => return Err(format!("row {y}: bad tag {tag:?}")),
        }
    }
    if !rest.is_empty() {
        return Err(String::from("bytes after the last row"));
    }
    Ok((width, height, data))
}

/// Every pixel where two `width`-wide frames differ, in row order.
pub(crate) fn compare(expected: &[u8], actual: &[u8], width: u32) -> Vec<Mismatch> {
    assert_eq!(expected.len(), actual.len(), "the frames are the same size");
    values(expected)
        .zip(values(actual))
        .enumerate()
        .filter(|(_, (expected, actual))| expected != actual)
        .map(|(index, (expected, actual))| Mismatch {
            x: (index % width as usize) as u32,
            y: (index / width as usize) as u32,
            expected,
            actual,
        })
        .collect()
}

/// Write a frame as a binary PPM, for looking at.
fn write_ppm(path: &std::path::Path, width: u32, height: u32, data: &[u8]) {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    for pixel in values(data) {
        out.extend([(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
    }
    std::fs::write(path, out).unwrap();
}

/// Where the expected image `name` is kept.
#[cfg(test)]
pub(crate) fn path(name: &str) -> PathBuf {
    in_dir(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("data"),
        name,
    )
}

/// The same, in a directory the caller names: the expected images live
/// together whichever crate blesses one.
fn in_dir(directory: &std::path::Path, name: &str) -> PathBuf {
    directory.join(format!("{name}.xrle"))
}

/// Require a rendered frame to be the expected image `name`, byte for byte;
/// or, with [`BLESS`] set, write it as that image and fail.
#[cfg(test)]
pub(crate) fn check(name: &str, width: u32, height: u32, data: &[u8]) {
    check_at(&path(name), name, width, height, data);
}

/// The same, for a crate whose expected images are somewhere else: the
/// terminal's, which are drawn by the term app and kept here with the
/// rest.
///
/// # Panics
///
/// As [`check`]: a frame that is not the expected image, or a blessing.
pub fn check_in(directory: &std::path::Path, name: &str, width: u32, height: u32, data: &[u8]) {
    check_at(&in_dir(directory, name), name, width, height, data);
}

/// Require `data` to be the image at `path`, or write it there.
fn check_at(path: &std::path::Path, name: &str, width: u32, height: u32, data: &[u8]) {
    if let Some(dir) = std::env::var_os(PPM) {
        write_ppm(
            &PathBuf::from(dir).join(format!("{name}.ppm")),
            width,
            height,
            data,
        );
    }
    if std::env::var_os(BLESS).is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, encode(width, height, data)).unwrap();
        panic!(
            "wrote {}: look at it, commit it, and run the tests again without {BLESS}",
            path.display()
        );
    }
    let file = std::fs::read(path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; write it deliberately with {BLESS}=1 cargo test -p compositor-render",
            path.display()
        )
    });
    let (expected_width, expected_height, expected) = decode(&file).unwrap();
    assert_eq!(
        (expected_width, expected_height),
        (width, height),
        "{name} is another size"
    );
    let mismatches = compare(&expected, data, width);
    assert!(
        mismatches.is_empty(),
        "{} pixels differ from {name}; the first: {:x?}",
        mismatches.len(),
        mismatches.get(..mismatches.len().min(8))
    );
}
