//! The pointer on a cursor plane of its own.
//!
//! Drawn into the frame, the pointer costs a frame every time it moves: the
//! damage round the old place and the new, a flush to the host, and -- on a
//! screen served over VNC -- an update the viewer gets only when the server
//! next looks, which is every 30 ms. A card with a cursor plane shows a small
//! image over the frame and moves it for the asking. virtio-gpu has one: the
//! image goes to the host once, and every move after that is a command on
//! the cursor queue that nobody waits for. A host that serves the screen
//! over VNC hands the image to the viewer, which draws it where its own
//! mouse is, and the pointer a person watching sees has no lag at all
//! (`docs/GPU.md` §3.9).
//!
//! So a screen whose backend has a plane shows the pointer on it, and its
//! frames neither draw the pointer nor count its moves as damage. What does
//! not fit -- a client's cursor larger than the plane -- is drawn into the
//! frame as before, and so is everything on a screen with no plane.
//! Hyprland's `cursor:no_hardware_cursors = 1` asks for that everywhere.
//!
//! A turned monitor (`monitor = ..., transform, N`) has its frame drawn
//! upright and turned on the way into the buffer the card scans out
//! (`compositor_render::transform`); the card knows nothing of the turn.
//! So the plane is told in the buffer's terms, as the frame is: the image
//! turned the same way, its hotspot with it, and its place where the
//! transform sends the pointer ([`turned`]). The DK1's monitor stands on
//! its edge, and its frames are the slowest there are.

use compositor_render::transform::{self, Transform};
use compositor_render::{Format, Surface};

/// Whether `cursor:no_hardware_cursors` leaves the pointer to a plane:
/// Hyprland's 0 and its default 2 do, where a screen has one, and 1 never
/// does.
#[must_use]
pub const fn wanted(no_hardware_cursors: i64) -> bool {
    no_hardware_cursors != 1
}

/// An image for a plane `size` big: `surface` in its top-left corner and
/// nothing around it, premultiplied `ARGB8888` with rows packed, which is
/// what the plane shows. No surface is an image of nothing, which is how a
/// hidden pointer is hidden: a host that serves the screen keeps the last
/// image it was given, and only a clear one clears it.
///
/// `None` when the surface is larger than the plane: that pointer is the
/// frame's to draw.
#[must_use]
pub fn image(surface: Option<&Surface<'_>>, size: (u32, u32)) -> Option<Vec<u8>> {
    let (width, height) = (size.0 as usize, size.1 as usize);
    let mut image = vec![0u8; width * height * 4];
    let Some(surface) = surface else {
        return Some(image);
    };
    let (wide, tall) = (surface.width() as usize, surface.height() as usize);
    if wide > width || tall > height {
        return None;
    }
    let stride = surface.stride() as usize;
    let opaque = surface.format() == Format::Xrgb8888;
    for row in 0..tall {
        let from = surface.data().get(row * stride..row * stride + wide * 4)?;
        let to = image.get_mut(row * width * 4..(row * width + wide) * 4)?;
        to.copy_from_slice(from);
        if opaque {
            for pixel in to.as_chunks_mut::<4>().0 {
                if let Some(alpha) = pixel.get_mut(3) {
                    *alpha = 0xFF;
                }
            }
        }
    }
    Some(image)
}

/// An image for the plane, its hotspot, and where its top-left corner goes.
pub type Placed = (Vec<u8>, (i32, i32), (i32, i32));

/// An image for a plane `size` big, drawn upright with its hotspot at
/// `hot` and its top-left corner at `at` on a turned monitor whose frame is
/// `frame` pixels as it is read -- as the card is to be told it: the image
/// turned by `transform` as every pixel of the frame is, the hotspot the
/// same pixel of the turned image, and the place the turned image's
/// top-left corner goes in the buffer.
///
/// The transform is a fixed map of the frame's pixels onto the buffer's
/// that moves a step in the frame by a step in the buffer, so a pixel
/// `(x, y)` of the image, which is the frame's pixel `at + (x, y)`, goes to
/// the buffer's `point(at) + (point(x, y) - point(0, 0))` -- the second
/// point taken on the image's own size. That is the turned image, its
/// top-left corner at `point(at) - point(0, 0)`: the pointer lands where the
/// frame would have drawn it, pixel for pixel, including a pointer across an
/// edge whose place the turn makes negative.
///
/// `None` for a quarter turn of a plane that is not square, whose image
/// turned would not be the plane's size; cursor planes are square.
#[must_use]
pub fn turned(
    transform: Transform,
    frame: (u32, u32),
    image: Vec<u8>,
    size: (u32, u32),
    hot: (i32, i32),
    at: (i32, i32),
) -> Option<Placed> {
    if transform == Transform::Normal {
        return Some((image, hot, at));
    }
    if transform.size(size) != size {
        return None;
    }
    let side = (i64::from(size.0), i64::from(size.1));
    let width = usize::try_from(size.0).ok()?;
    let mut out = vec![0u8; image.len()];
    for (index, pixel) in image.as_chunks::<4>().0.iter().enumerate() {
        let (x, y) = (
            i64::try_from(index % width).ok()?,
            i64::try_from(index / width).ok()?,
        );
        let (x, y) = transform::point(transform, side, (x, y));
        let to = usize::try_from(y)
            .ok()?
            .checked_mul(width)?
            .checked_add(usize::try_from(x).ok()?)?
            .checked_mul(4)?;
        out.get_mut(to..to.checked_add(4)?)?.copy_from_slice(pixel);
    }
    let wide = |(x, y): (i32, i32)| (i64::from(x), i64::from(y));
    let narrow = |(x, y): (i64, i64)| Some((i32::try_from(x).ok()?, i32::try_from(y).ok()?));
    let hot = transform::point(transform, side, wide(hot));
    let corner = transform::point(
        transform,
        (i64::from(frame.0), i64::from(frame.1)),
        wide(at),
    );
    let origin = transform::point(transform, side, (0, 0));
    let at = (
        corner.0.checked_sub(origin.0)?,
        corner.1.checked_sub(origin.1)?,
    );
    Some((out, narrow(hot)?, narrow(at)?))
}

/// What a screen's plane is to be told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tell {
    /// Nothing: it shows this already.
    Nothing,
    /// The same image, its top-left corner here.
    Move((i32, i32)),
    /// This image with its hotspot here, its top-left corner there.
    Set {
        /// The image, the plane's size.
        image: Vec<u8>,
        /// Its hotspot.
        hot: (i32, i32),
        /// Its top-left corner.
        at: (i32, i32),
    },
}

/// What a screen's plane was last told, and whether the pointer is on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plane {
    /// Whether this screen's pointer is on the plane, rather than drawn into
    /// its frames.
    pub on: bool,
    /// A digest of the image and hotspot last set, so that one unchanged is
    /// not set again: setting an image waits for it to reach the host, and
    /// moving one waits for nothing.
    shown: Option<u64>,
    /// Where its top-left corner last went.
    at: Option<(i32, i32)>,
}

impl Plane {
    /// What to tell the plane so that it shows `image` with its hotspot at
    /// `hot` and its top-left corner at `at`. A clear image is not moved:
    /// there is nothing to see go anywhere.
    pub fn tell(&mut self, image: Vec<u8>, hot: (i32, i32), at: (i32, i32)) -> Tell {
        let digest = digest(&image, hot);
        if self.shown != Some(digest) {
            self.shown = Some(digest);
            self.at = Some(at);
            return Tell::Set { image, hot, at };
        }
        let clear = image.iter().all(|&byte| byte == 0);
        if clear || self.at == Some(at) {
            return Tell::Nothing;
        }
        self.at = Some(at);
        Tell::Move(at)
    }

    /// Forget what the plane was told: the next image is set whatever it
    /// is. For a screen whose card came back, which has seen none of it,
    /// and for one whose last telling failed.
    pub fn forget(&mut self) {
        self.shown = None;
        self.at = None;
    }
}

/// FNV-1a over the image and its hotspot: enough to tell one pointer's
/// image from the next, which is all it is asked.
fn digest(image: &[u8], hot: (i32, i32)) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in image
        .iter()
        .chain(&hot.0.to_le_bytes())
        .chain(&hot.1.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_option_is_hyprlands() {
        assert!(wanted(0));
        assert!(wanted(2), "the default leaves it to the compositor");
        assert!(!wanted(1));
    }

    #[test]
    fn an_image_is_the_surface_in_the_corner_and_nothing_else() {
        // Three by two, padded rows, one pixel translucent.
        let mut pixels = vec![0u8; 16 * 2];
        pixels[..4].copy_from_slice(&[1, 2, 3, 0x80]);
        pixels[16 + 8..16 + 12].copy_from_slice(&[4, 5, 6, 0xFF]);
        let surface = Surface::new(&pixels, 3, 2, 16, Format::Argb8888).expect("a surface");
        let image = image(Some(&surface), (4, 4)).expect("it fits");
        assert_eq!(image.len(), 4 * 4 * 4);
        assert_eq!(image[..4], [1, 2, 3, 0x80]);
        assert_eq!(image[16 + 8..16 + 12], [4, 5, 6, 0xFF]);
        assert_eq!(image.iter().filter(|&&byte| byte != 0).count(), 8);

        // An opaque format is opaque on the plane, whatever its padding
        // byte says.
        let opaque = Surface::new(&pixels, 3, 2, 16, Format::Xrgb8888).expect("a surface");
        let image = super::image(Some(&opaque), (4, 4)).expect("it fits");
        assert_eq!(image[3], 0xFF);
        assert_eq!(image[16 + 11], 0xFF);

        // Nothing is a clear image; too large is the frame's to draw.
        assert!(
            super::image(None, (4, 4))
                .expect("clear")
                .iter()
                .all(|&byte| byte == 0)
        );
        assert_eq!(super::image(Some(&surface), (2, 2)), None);
    }

    /// A `side`-square image with a pixel of its own at each place, so that
    /// where one went says where it came from.
    fn numbered(side: u32) -> Vec<u8> {
        (0..side * side)
            .flat_map(|n| {
                let [a, b, ..] = n.to_le_bytes();
                [a, b, 0x5A, 0xFF]
            })
            .collect()
    }

    /// The pixel of the turned image at `(x, y)`.
    fn pixel(image: &[u8], side: u32, (x, y): (i64, i64)) -> Option<&[u8]> {
        let at = usize::try_from(y * i64::from(side) + x).ok()? * 4;
        image.get(at..at + 4)
    }

    #[test]
    fn a_turned_image_lands_where_the_frame_would_have_drawn_it() {
        // The DK1's monitor, 1280x720 turned onto its edge, and every other
        // transform on it; the pointer in the middle and across each edge.
        let side = 64;
        let image = numbered(side);
        for transform in Transform::ALL {
            // The frame as it is read: the mode's size, exchanged for a
            // quarter turn.
            let frame = transform.size((1280, 720));
            let size = (i64::from(frame.0), i64::from(frame.1));
            for at in [(300, 500), (-20, -9), (700, 1270), (-63, 1279)] {
                let hot = (5, 11);
                let (shown, shown_hot, place) =
                    turned(transform, frame, image.clone(), (side, side), hot, at)
                        .expect("a square plane turns");
                // Every pixel of the image is where the transform sends the
                // frame's pixel it covers.
                for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63), (5, 11), (40, 7)] {
                    let on_frame = (i64::from(at.0 + x), i64::from(at.1 + y));
                    let in_buffer = transform::point(transform, size, on_frame);
                    let in_image = (
                        in_buffer.0 - i64::from(place.0),
                        in_buffer.1 - i64::from(place.1),
                    );
                    assert_eq!(
                        pixel(&shown, side, in_image),
                        pixel(&image, side, (i64::from(x), i64::from(y))),
                        "{transform:?} at {at:?}: the image's ({x}, {y})"
                    );
                }
                // And the hotspot is the pixel the hotspot was.
                assert_eq!(
                    pixel(&shown, side, (shown_hot.0.into(), shown_hot.1.into())),
                    pixel(&image, side, (5, 11)),
                    "{transform:?}: the hotspot"
                );
            }
        }
    }

    #[test]
    fn the_dk1s_turned_pointer_is_turned_clockwise_into_the_buffer() {
        // `transform, 3`: the frame's top left is the buffer's top right, so
        // the arrow's tip at the image's top left is at the turned image's
        // top right, and a pointer at the frame's top-left corner is at the
        // buffer's top-right one.
        let mut image = vec![0u8; 64 * 64 * 4];
        image[..4].copy_from_slice(&[1, 2, 3, 0xFF]);
        let (shown, hot, at) = turned(
            Transform::Rotated270,
            (720, 1280),
            image,
            (64, 64),
            (0, 0),
            (0, 0),
        )
        .expect("square");
        assert_eq!(hot, (63, 0), "the tip, turned");
        assert_eq!(pixel(&shown, 64, (63, 0)), Some(&[1, 2, 3, 0xFF][..]));
        assert_eq!(
            (at.0 + hot.0, at.1 + hot.1),
            (1279, 0),
            "the buffer's top-right pixel"
        );
        // Upright is left alone, and a plane that is not square is not
        // turned a quarter.
        let upright = turned(
            Transform::Normal,
            (1280, 720),
            vec![7; 16],
            (2, 2),
            (1, 1),
            (4, 5),
        );
        assert_eq!(upright, Some((vec![7; 16], (1, 1), (4, 5))));
        let oblong = vec![0; 4 * 2 * 4];
        assert_eq!(
            turned(
                Transform::Rotated90,
                (720, 1280),
                oblong,
                (4, 2),
                (0, 0),
                (0, 0)
            ),
            None
        );
    }

    #[test]
    fn an_image_is_set_once_and_then_only_moved() {
        let mut plane = Plane::default();
        let arrow = vec![7u8; 16];
        assert!(matches!(
            plane.tell(arrow.clone(), (0, 0), (5, 5)),
            Tell::Set { at: (5, 5), .. }
        ));
        assert_eq!(plane.tell(arrow.clone(), (0, 0), (5, 5)), Tell::Nothing);
        assert_eq!(
            plane.tell(arrow.clone(), (0, 0), (6, 5)),
            Tell::Move((6, 5))
        );
        // A new hotspot is a new image, and so is a clear one -- which is
        // then not moved about.
        assert!(matches!(
            plane.tell(arrow.clone(), (1, 1), (6, 5)),
            Tell::Set { .. }
        ));
        assert!(matches!(
            plane.tell(vec![0; 16], (0, 0), (6, 5)),
            Tell::Set { .. }
        ));
        assert_eq!(plane.tell(vec![0; 16], (0, 0), (9, 9)), Tell::Nothing);
        // A plane that forgot is told again.
        plane.forget();
        assert!(matches!(
            plane.tell(vec![0; 16], (0, 0), (9, 9)),
            Tell::Set { .. }
        ));
    }
}
