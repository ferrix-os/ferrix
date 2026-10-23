//! QOI, "the Quite OK Image format" (qoiformat.org, specification 1.0):
//! lossless, and simple enough that every provider can write it with no
//! library. A provider hands its screen as `0xAARRGGBB` words or as XRGB
//! bytes in memory order (B, G, R, X); [`encode`] writes RGB, three
//! channels, sRGB.

use alloc::vec::Vec;

/// The file's first bytes.
const MAGIC: &[u8; 4] = b"qoif";
/// The end marker: seven zero bytes and a one.
const END: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];

/// One pixel.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct Pixel {
    /// Red.
    r: u8,
    /// Green.
    g: u8,
    /// Blue.
    b: u8,
    /// Alpha.
    a: u8,
}

impl Pixel {
    /// Its slot in the running index.
    fn hash(self) -> usize {
        (usize::from(self.r) * 3
            + usize::from(self.g) * 5
            + usize::from(self.b) * 7
            + usize::from(self.a) * 11)
            % 64
    }
}

/// Encode a `width` by `height` image whose rows are `stride` bytes apart
/// in `xrgb`, each pixel four bytes B, G, R, X (little-endian XRGB8888, a
/// `wl_shm` and DRM screen's format). `None` for a size the bytes do not hold.
#[must_use]
pub fn encode(width: u32, height: u32, stride: usize, xrgb: &[u8]) -> Option<Vec<u8>> {
    let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    if w == 0 || h == 0 || stride < w * 4 || xrgb.len() < stride * (h - 1) + w * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(14 + w * h + 8);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.push(3);
    out.push(0);
    let mut index = [Pixel::default(); 64];
    let mut previous = Pixel {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    let mut run: u8 = 0;
    for row in 0..h {
        let line = xrgb.get(row * stride..row * stride + w * 4)?;
        for quad in line.chunks_exact(4) {
            let [b, g, r, _] = <[u8; 4]>::try_from(quad).ok()?;
            let pixel = Pixel { r, g, b, a: 255 };
            if pixel == previous {
                run += 1;
                if run == 62 {
                    out.push(0xc0 | (run - 1));
                    run = 0;
                }
                continue;
            }
            if run > 0 {
                out.push(0xc0 | (run - 1));
                run = 0;
            }
            let slot = pixel.hash();
            if index.get(slot) == Some(&pixel) {
                out.push(u8::try_from(slot).unwrap_or(0));
            } else {
                if let Some(entry) = index.get_mut(slot) {
                    *entry = pixel;
                }
                push_difference(&mut out, previous, pixel);
            }
            previous = pixel;
        }
    }
    if run > 0 {
        out.push(0xc0 | (run - 1));
    }
    out.extend_from_slice(&END);
    Some(out)
}

/// `pixel` after `previous`, as the smallest of DIFF, LUMA and RGB.
fn push_difference(out: &mut Vec<u8>, previous: Pixel, pixel: Pixel) {
    let dr = pixel.r.wrapping_sub(previous.r) as i8;
    let dg = pixel.g.wrapping_sub(previous.g) as i8;
    let db = pixel.b.wrapping_sub(previous.b) as i8;
    let small = |d: i8| (-2..=1).contains(&d);
    if small(dr) && small(dg) && small(db) {
        out.push(0x40 | (((dr + 2) as u8) << 4) | (((dg + 2) as u8) << 2) | ((db + 2) as u8));
        return;
    }
    let dr_dg = dr.wrapping_sub(dg);
    let db_dg = db.wrapping_sub(dg);
    if (-32..=31).contains(&dg) && (-8..=7).contains(&dr_dg) && (-8..=7).contains(&db_dg) {
        out.push(0x80 | ((dg + 32) as u8));
        out.push((((dr_dg + 8) as u8) << 4) | ((db_dg + 8) as u8));
        return;
    }
    out.extend_from_slice(&[0xfe, pixel.r, pixel.g, pixel.b]);
}

/// Decode a QOI image of three or four channels: its width, height and
/// RGBA pixels. For tests, and for a reader that wants to look.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.get(..4)? != MAGIC {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(8..12)?.try_into().ok()?);
    let count = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    let mut pixels = Vec::with_capacity(count.checked_mul(4)?);
    let mut index = [Pixel::default(); 64];
    let mut pixel = Pixel {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    let mut at = 14;
    let end = bytes.len().checked_sub(END.len())?;
    let mut produced = 0;
    while produced < count {
        let mut run = 1;
        if at < end {
            let tag = *bytes.get(at)?;
            at += 1;
            match tag {
                0xfe => {
                    pixel.r = *bytes.get(at)?;
                    pixel.g = *bytes.get(at + 1)?;
                    pixel.b = *bytes.get(at + 2)?;
                    at += 3;
                }
                0xff => {
                    pixel = Pixel {
                        r: *bytes.get(at)?,
                        g: *bytes.get(at + 1)?,
                        b: *bytes.get(at + 2)?,
                        a: *bytes.get(at + 3)?,
                    };
                    at += 4;
                }
                _ => match tag >> 6 {
                    0 => pixel = *index.get(usize::from(tag & 0x3f))?,
                    1 => {
                        pixel.r = pixel.r.wrapping_add(((tag >> 4) & 3).wrapping_sub(2));
                        pixel.g = pixel.g.wrapping_add(((tag >> 2) & 3).wrapping_sub(2));
                        pixel.b = pixel.b.wrapping_add((tag & 3).wrapping_sub(2));
                    }
                    2 => {
                        let second = *bytes.get(at)?;
                        at += 1;
                        let dg = (tag & 0x3f).wrapping_sub(32);
                        pixel.r = pixel
                            .r
                            .wrapping_add(dg.wrapping_add((second >> 4).wrapping_sub(8)));
                        pixel.g = pixel.g.wrapping_add(dg);
                        pixel.b = pixel
                            .b
                            .wrapping_add(dg.wrapping_add((second & 15).wrapping_sub(8)));
                    }
                    _ => run = usize::from(tag & 0x3f) + 1,
                },
            }
            if let Some(entry) = index.get_mut(pixel.hash()) {
                *entry = pixel;
            }
        }
        for _ in 0..run {
            pixels.extend_from_slice(&[pixel.r, pixel.g, pixel.b, pixel.a]);
        }
        produced += run;
    }
    pixels.truncate(count * 4);
    Some((width, height, pixels))
}
