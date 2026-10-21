//! Dual-Kawase blur, on the CPU, with the colour grading Hyprland grades it
//! with.
//!
//! Hyprland blurs what is *behind* a translucent window: the frame so far is
//! taken, blurred, and written back under the window before the window is
//! drawn over it. The blur itself is four shaders, run in this order over
//! the frame:
//!
//! 1. `blurprepare.glsl` once, over the whole region: `blur:contrast`
//!    through the `gain` curve, then `blur:brightness` where it brightens.
//! 2. `blur1.glsl`'s five-tap downsample, `decoration:blur:passes` times,
//!    each pass halving the size and each ending in the `blur:vibrancy`
//!    saturation boost.
//! 3. `blur2.glsl`'s eight-tap upsample, the same number of times back up.
//! 4. `blurFinish.glsl` once: a hash dither at `blur:noise`, then
//!    `blur:brightness` where it darkens.
//!
//! The two kernels are, exactly:
//!
//! ```text
//! down(uv) = (4·t(2uv) + t(2uv − h·r) + t(2uv + h·r)
//!             + t(2uv + (hx, −hy)·r) + t(2uv − (hx, −hy)·r)) / 8
//!
//! up(uv)   = ( t(uv/2 + (−2hx, 0)·r) + 2·t(uv/2 + (−hx,  hy)·r)
//!            + t(uv/2 + (0,  2hy)·r) + 2·t(uv/2 + ( hx,  hy)·r)
//!            + t(uv/2 + ( 2hx, 0)·r) + 2·t(uv/2 + ( hx, −hy)·r)
//!            + t(uv/2 + (0, −2hy)·r) + 2·t(uv/2 + (−hx, −hy)·r) ) / 12
//! ```
//!
//! with `h` the half-pixel of the buffer being written and `r` the radius.
//! Sampling is bilinear with the edges clamped, which is what
//! `GL_CLAMP_TO_EDGE` and `GL_LINEAR` give the shader.
//!
//! # The grading is not decoration
//!
//! Hyprland's defaults grade every blur -- `contrast` is 0.8916 and
//! `vibrancy` 0.1696 out of the box -- so a blur drawn without them is not a
//! plainer blur, it is the wrong colour. The five values are in [`Blur`],
//! each with Hyprland's own default, and each applied at the point in the
//! chain its shader applies it: `contrast` before the first downsample and
//! `noise` after the last upsample are different pictures from doing either
//! in the middle.
//!
//! The noise is a hash of the pixel's position on the monitor, not a random
//! number: `blurFinish.glsl`'s `hash(v_texcoord)`, fed the same coordinate
//! the shader is fed. Two frames of the same scene are therefore the same
//! bytes, which is what lets an expected image hold it.
//!
//! # What it costs
//!
//! Each pass halves the size, so the whole chain touches at most twice the
//! source's pixels going down and twice going up: about `4 × w × h` samples
//! of five and eight taps. The eight-tap upsample at full resolution is
//! three quarters of them -- 16 million of a screen's 25 million at three
//! passes -- and that is where a blurred frame's time goes.
//!
//! A full screen at 1920x1080, `size = 8`, `passes = 3`: **85 ms**, from
//! 304 ms when the pyramid was first written.
//! `a_full_screen_blur_is_inside_the_stated_bound` holds the number. What
//! bought it, none of it changing a single byte of any expected image:
//!
//! * The bilinear sample's rows, its vertical weight and its clamps come
//!   out of the pixel loop and are worked out once an output row a tap
//!   ([`Plane::blend_row`]), so a tap is one mix across rather than three.
//! * `f32::floor` is a call out to the C library on a baseline x86-64,
//!   where the instruction that would do it in one go is an extension the
//!   target does not assume. A truncating cast and a step back for the
//!   negatives is the same answer in one instruction, and was a fifth of
//!   the whole blur ([`floor`]).
//! * `blurprepare.glsl`'s contrast curve is a power of each channel, and it
//!   runs over the block before anything else has touched it, so every
//!   channel is still one of a byte's 256 values: a table of 256 rather
//!   than six million calls to `powf` ([`prepared`]).
//!
//! What is left is the tap itself, and a software dual-Kawase cannot go
//! much under 16 million of them for a screen. Past that the levers were a
//! renderer that spreads its rows over cores, explicit vector arithmetic,
//! or the GPU. The first is taken: every stage here writes its rows through
//! [`crate::cores`], which is 85 ms down to about 20 on twelve cores with
//! the same bytes out, and one thread and the old 85 where there is one
//! core.

use crate::cores::bands;
use crate::scratch;

/// What `decoration:blur` asks for: the shape of the blur, and the grading
/// over it.
///
/// The defaults are Hyprland's, from `ConfigValues.cpp`, so a compositor
/// whose configuration says nothing but `blur { enabled = true }` draws
/// Hyprland's blur rather than an ungraded one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blur {
    /// `blur:size`: the radius the taps are spread over.
    pub size: i64,
    /// `blur:passes`: how many times down, and the same back up.
    pub passes: u32,
    /// `blur:noise`: how much of a per-pixel dither is added, 0 to 1.
    pub noise: f32,
    /// `blur:contrast`: the `gain` curve's exponent, 0 to 2. One leaves the
    /// colours alone; above one pushes them away from the middle grey.
    pub contrast: f32,
    /// `blur:brightness`, 0 to 2. Applied twice, as Hyprland applies it:
    /// `max(1, brightness)` before the passes and `min(1, brightness)`
    /// after them, so one of the two is always the identity.
    pub brightness: f32,
    /// `blur:vibrancy`: how much the saturation of a blurred colour is
    /// boosted, 0 to 1.
    pub vibrancy: f32,
    /// `blur:vibrancy_darkness`: how much of that boost dark colours get,
    /// 0 to 1.
    pub vibrancy_darkness: f32,
}

impl Blur {
    /// Hyprland's `decoration:blur:noise`, which is what a blur starts
    /// with here as well.
    pub const NOISE: f32 = 0.0117;
    /// Hyprland's `decoration:blur:contrast`.
    pub const CONTRAST: f32 = 0.8916;
    /// Hyprland's `decoration:blur:brightness`.
    pub const BRIGHTNESS: f32 = 1.0;
    /// Hyprland's `decoration:blur:vibrancy`.
    pub const VIBRANCY: f32 = 0.1696;
    /// Hyprland's `decoration:blur:vibrancy_darkness`.
    pub const VIBRANCY_DARKNESS: f32 = 0.0;

    /// A blur of `size` and `passes`, graded the way Hyprland grades one
    /// that says nothing else.
    #[must_use]
    pub const fn new(size: i64, passes: u32) -> Self {
        Self {
            size,
            passes,
            noise: Self::NOISE,
            contrast: Self::CONTRAST,
            brightness: Self::BRIGHTNESS,
            vibrancy: Self::VIBRANCY,
            vibrancy_darkness: Self::VIBRANCY_DARKNESS,
        }
    }

    /// How far from a pixel anything that changes its blur can be, in
    /// pixels of the canvas.
    ///
    /// Each level of the pyramid is half the one above it, so a tap at
    /// `size` on level *k* is `size * 2^k` source pixels. Going down that
    /// sums to `size * (2^passes - 1)` and coming back up to the same, so
    /// the kernel reaches `2 * size * (2^passes - 1)`, which is inside
    /// `2 * size * 2^passes`. Twice the lattice the pyramid halves on is
    /// added, because what is read is snapped out to it at either end.
    #[must_use]
    pub const fn reach(&self) -> i64 {
        let lattice = 1_i64 << if self.passes < 6 { self.passes } else { 6 };
        self.size
            .saturating_mul(lattice)
            .saturating_mul(2)
            .saturating_add(lattice.saturating_mul(2))
    }

    /// The same shape with every grading value at the one that does
    /// nothing: the two kernels and no colour change at all.
    ///
    /// Not a configuration anybody writes -- it is `noise = 0`,
    /// `contrast = 1`, `brightness = 1` and `vibrancy = 0` together -- but
    /// it is what a test that wants to see the grading's effect compares
    /// against.
    #[must_use]
    pub const fn ungraded(size: i64, passes: u32) -> Self {
        Self {
            size,
            passes,
            noise: 0.0,
            contrast: 1.0,
            brightness: 1.0,
            vibrancy: 0.0,
            vibrancy_darkness: 0.0,
        }
    }
}

/// Where the block being blurred sits, which is what the noise needs.
///
/// `blurFinish.glsl` hashes `v_texcoord`, and its quad is the whole monitor,
/// so the dither is fixed to the screen rather than to the window: a window
/// that moves moves through the pattern instead of carrying it along. The
/// block knows where it is on the canvas so the same is true here.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Block {
    /// The block's width in pixels.
    pub(crate) width: usize,
    /// Its height in pixels.
    pub(crate) height: usize,
    /// Its top-left corner on the canvas.
    pub(crate) origin: (i64, i64),
    /// The canvas's own size, which the noise coordinate is relative to.
    pub(crate) screen: (u32, u32),
    /// Whether the last step back up is [`doubled`]: decided by the size of
    /// what is blurred, the window, not of the block read for it, so that a
    /// damaged strip of a large window is blurred as the whole of it is.
    pub(crate) doubled: bool,
}

/// A buffer of premultiplied `RGBA` floats, as the blur works on it.
///
/// Floats, not bytes: the chain writes and reads its own output up to twenty
/// times, and rounding to a byte each time loses the low bits that make a
/// blur smooth rather than banded.
#[derive(Clone, Debug)]
struct Plane {
    width: usize,
    height: usize,
    pixels: Vec<[f32; 4]>,
}

impl Plane {
    /// A plane, from the ones kept ([`crate::scratch`]): every pixel of it
    /// is written by the pass that fills it before anything reads one, so
    /// what it held before does not matter.
    fn new(width: usize, height: usize) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            pixels: scratch::planes(width.max(1) * height.max(1)),
        }
    }

    /// Keep the plane's memory for the next one.
    fn back(self) {
        scratch::planes_back(self.pixels);
    }

    /// One row, with the edges clamped.
    fn row(&self, y: isize) -> &[[f32; 4]] {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a plane is at most a screen tall, far inside isize"
        )]
        let last = self.height.saturating_sub(1) as isize;
        let y = y.clamp(0, last).unsigned_abs();
        let start = y.saturating_mul(self.width);
        self.pixels
            .get(start..start.saturating_add(self.width))
            .unwrap_or(&[])
    }

    /// The two rows a bilinear sample at height `y` reads, mixed into one
    /// row of their own.
    ///
    /// This is the pass's arithmetic turned on its side. A bilinear sample
    /// mixes four texels, two across and two down; the two it mixes down
    /// are the same two for every pixel of an output row, and several taps
    /// read at the same height. Mixing them once a row leaves each tap of
    /// each pixel one mix -- across -- where the four-texel sample was
    /// three, and it is what takes the clamping and the row arithmetic off
    /// the innermost loop of the whole effect.
    ///
    /// Mixing down first is the same arithmetic in the other order, not a
    /// different kernel: bilinear interpolation is symmetric in its two
    /// axes. The two orders may differ in the last bit a float holds, so
    /// this is one of the few changes in this crate that could have moved a
    /// pixel; not one of the committed images moved by a byte.
    fn blend_row(&self, y: f32, into: &mut [[f32; 4]]) {
        let fy = y - 0.5;
        let (iy, weight) = floor(fy);
        let (top, bottom) = (self.row(iy), self.row(iy.saturating_add(1)));
        for ((slot, &above), &below) in into.iter_mut().zip(top).zip(bottom) {
            *slot = mix(above, below, weight);
        }
    }
}

/// The sample at `x` of a row already mixed down, with the edges clamped as
/// `GL_CLAMP_TO_EDGE` clamps them.
fn sample_row(row: &[[f32; 4]], x: f32) -> [f32; 4] {
    let fx = x - 0.5;
    let (ix, tx) = floor(fx);
    #[expect(
        clippy::cast_possible_wrap,
        reason = "a row is at most a screen wide, far inside isize"
    )]
    let last = row.len().saturating_sub(1) as isize;
    let (left, right) = (
        ix.clamp(0, last).unsigned_abs(),
        ix.saturating_add(1).clamp(0, last).unsigned_abs(),
    );
    let zero = [0.0_f32; 4];
    let read = |at: usize| row.get(at).copied().unwrap_or(zero);
    mix(read(left), read(right), tx)
}

/// A position's whole part and what is left of it, as a bilinear sample
/// splits one.
///
/// `f32::floor` and not a cast, in a shape that is a cast: on a baseline
/// x86-64 the rounding instruction that would do it in one go is not there,
/// so `floor` is a call out to the C library -- once a tap, twenty million
/// times a blur. A truncating cast is one instruction everywhere, and a
/// step back for a negative that lost its fraction makes it the floor.
fn floor(value: f32) -> (isize, f32) {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a position inside a buffer, which is far inside i32"
    )]
    let truncated = value as i32;
    #[expect(
        clippy::cast_precision_loss,
        reason = "a whole number a buffer's width can hold, exact in f32"
    )]
    let whole = if (truncated as f32) > value {
        truncated - 1
    } else {
        truncated
    };
    #[expect(clippy::cast_precision_loss, reason = "as above")]
    let fraction = value - whole as f32;
    (whole as isize, fraction)
}

/// One colour `t` of the way towards another, channel by channel.
///
/// Four expressions rather than a loop over `get`: this is the arithmetic
/// underneath every tap of every pass, and written this way it is four
/// multiplications a compiler can put in one instruction rather than four
/// `Option`s it cannot.
fn mix(one: [f32; 4], other: [f32; 4], t: f32) -> [f32; 4] {
    [
        one[0] + (other[0] - one[0]) * t,
        one[1] + (other[1] - one[1]) * t,
        one[2] + (other[2] - one[2]) * t,
        one[3] + (other[3] - one[3]) * t,
    ]
}

/// One tap of a pass: how far across to read, which of the pass's mixed
/// rows to read it from, and how much of what it finds goes into the sum.
#[derive(Clone, Copy, Debug)]
struct Tap {
    offset: f32,
    row: usize,
    weight: f32,
}

/// One pass of the pyramid: every pixel of `to` is the weighted sum of
/// `taps` read from `from` around its own position at `scale`, over `total`.
///
/// The taps of a pass read at only a few distinct heights -- three for
/// `blur1`, five for `blur2` -- and every pixel of an output row reads at
/// the same ones, so each is mixed down to a row of its own once and the
/// taps then only mix across. That is the whole of why a blurred frame is
/// milliseconds rather than a third of a second; [`Plane::blend_row`] says
/// what it costs in exactness.
///
/// `each` is what the pass does with a pixel once it has it: `blur1.glsl`
/// ends in the vibrancy boost and `blur2.glsl` ends.
fn pass<const HEIGHTS: usize, const TAPS: usize>(
    from: &Plane,
    to: &mut Plane,
    heights: &[f32; HEIGHTS],
    taps: &[Tap; TAPS],
    scale: f32,
    each: impl Fn([f32; 4]) -> [f32; 4] + Sync,
) {
    let width = to.width;
    bands(&mut to.pixels, width, |first, band| {
        pass_rows(from, band, (first, width), heights, taps, scale, &each);
    });
}

/// The rows of a pass from `first` on, `band.len() / width` of them: what
/// one thread of [`pass`] does.
fn pass_rows<const HEIGHTS: usize, const TAPS: usize>(
    from: &Plane,
    band: &mut [[f32; 4]],
    (first, width): (usize, usize),
    heights: &[f32; HEIGHTS],
    taps: &[Tap; TAPS],
    scale: f32,
    each: &impl Fn([f32; 4]) -> [f32; 4],
) {
    let total: f32 = taps.iter().map(|tap| tap.weight).sum();
    // The mixed rows, in one block rather than a vector of vectors: the
    // taps read them at every pixel, and a row behind a pointer of its own
    // is a pointer followed twenty million times.
    let stride = from.width.max(1);
    let mut mixed = scratch::planes(stride.saturating_mul(heights.len()));
    for (y, row) in (first..).zip(band.chunks_exact_mut(width.max(1))) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a position inside a buffer at most a screen tall"
        )]
        let sy = (y as f32 + 0.5) * scale;
        for (row, &dy) in mixed.chunks_exact_mut(stride).zip(heights) {
            from.blend_row(sy + dy, row);
        }
        let mut sources: [&[[f32; 4]]; HEIGHTS] = [&[]; HEIGHTS];
        for (slot, row) in sources.iter_mut().zip(mixed.chunks_exact(stride)) {
            *slot = row;
        }
        for (x, slot) in row.iter_mut().enumerate() {
            #[expect(clippy::cast_precision_loss, reason = "as above")]
            let sx = (x as f32 + 0.5) * scale;
            let mut sum = [0.0_f32; 4];
            for tap in taps {
                let Some(source) = sources.get(tap.row) else {
                    continue;
                };
                let sample = sample_row(source, sx + tap.offset);
                // Written out for the same reason `mix` is: this runs once
                // a tap a pixel a pass.
                sum[0] += sample[0] * tap.weight;
                sum[1] += sample[1] * tap.weight;
                sum[2] += sample[2] * tap.weight;
                sum[3] += sample[3] * tap.weight;
            }
            *slot = each([
                sum[0] / total,
                sum[1] / total,
                sum[2] / total,
                sum[3] / total,
            ]);
        }
    }
    scratch::planes_back(mixed);
}

/// `blur1.glsl`'s five taps: read `from` at twice this buffer's scale, and
/// then its vibrancy boost.
///
/// In pixels rather than in the shader's normalised coordinates, which is
/// the same kernel written the way a software renderer can check it: the
/// target pixel `(x, y)` reads the source around `(2x + 1, 2y + 1)`, its own
/// centre, with four taps a `radius` away on the diagonals.
fn down(from: &Plane, radius: f32, settings: &Blur) -> Plane {
    let mut to = Plane::new(from.width.div_ceil(2), from.height.div_ceil(2));
    // The three heights the five taps read at, and the taps over them: the
    // centre row of the sample, and a radius above and below it.
    let heights = [0.0, -radius, radius];
    let taps = [
        Tap {
            offset: 0.0,
            row: 0,
            weight: 4.0,
        },
        Tap {
            offset: -radius,
            row: 1,
            weight: 1.0,
        },
        Tap {
            offset: radius,
            row: 2,
            weight: 1.0,
        },
        Tap {
            offset: radius,
            row: 1,
            weight: 1.0,
        },
        Tap {
            offset: -radius,
            row: 2,
            weight: 1.0,
        },
    ];
    let vibrancy = Vibrancy::of(settings);
    pass(from, &mut to, &heights, &taps, 2.0, |color| {
        vibrancy.as_ref().map_or(color, |boost| boost.apply(color))
    });
    to
}

/// `blur2.glsl`'s eight taps: read `from` at half this buffer's scale.
fn up(from: &Plane, width: usize, height: usize, radius: f32) -> Plane {
    let mut to = Plane::new(width, height);
    // The two passes do not use the same offsets. `blur1`'s `halfpixel` is
    // `0.5 / (size / 2)` and `blur2`'s is `0.5 / (size * 2)`, both of the
    // monitor's size: four times smaller. In the sampled plane's pixels that
    // is `radius` going down and a quarter of it coming back up, and the
    // ratio is what makes the eight taps a reconstruction rather than a
    // ring -- a ring of that width turns a bright block into a dark hole
    // with a bright halo, which is what this looked like when both were
    // `radius`.
    let r = radius / 4.0;
    // The five heights the eight taps read at, in the order `blur2.glsl`
    // writes its taps: the centre, a quarter-radius either side and half a
    // radius either side.
    let heights = [0.0, r, 2.0 * r, -r, -2.0 * r];
    let taps = [
        Tap {
            offset: -2.0 * r,
            row: 0,
            weight: 1.0,
        },
        Tap {
            offset: -r,
            row: 1,
            weight: 2.0,
        },
        Tap {
            offset: 0.0,
            row: 2,
            weight: 1.0,
        },
        Tap {
            offset: r,
            row: 1,
            weight: 2.0,
        },
        Tap {
            offset: 2.0 * r,
            row: 0,
            weight: 1.0,
        },
        Tap {
            offset: r,
            row: 3,
            weight: 2.0,
        },
        Tap {
            offset: 0.0,
            row: 4,
            weight: 1.0,
        },
        Tap {
            offset: -r,
            row: 3,
            weight: 2.0,
        },
    ];
    pass(from, &mut to, &heights, &taps, 0.5, |color| color);
    to
}

/// Above this many pixels a block's last step back up to its own size is
/// [`doubled`] rather than [`up`]'s eight taps.
///
/// That step is three quarters of the whole blur's taps, and over a block
/// the size of a screen it is what a slow machine cannot pay: 750 to 850 ms
/// a frame on the N150 for a 1921x1201 window floating over the desktop.
/// What is lost is the last reconstruction of a picture that is a blur
/// already, a softer edge at the finest scale; every block of a screen
/// under about 1440x1040, which is every window of the committed images,
/// is drawn exactly as before.
const LARGE: usize = 1_500_000;

/// Whether a blur over `rect` is large enough for [`LARGE`]'s cheaper
/// last step.
pub(crate) fn large(rect: crate::Rect) -> bool {
    usize::try_from(rect.width.max(0).saturating_mul(rect.height.max(0))).unwrap_or(usize::MAX)
        > LARGE
}

/// A plain bilinear doubling of `from` to `width` x `height`: one tap where
/// [`up`] has eight.
fn doubled(from: &Plane, width: usize, height: usize) -> Plane {
    let mut to = Plane::new(width, height);
    let taps = [Tap {
        offset: 0.0,
        row: 0,
        weight: 1.0,
    }];
    pass(from, &mut to, &[0.0], &taps, 0.5, |color| color);
    to
}

/// `blurprepare.glsl`: the contrast curve and the brightening half of
/// `brightness`, as a table of what each byte a colour channel can hold
/// becomes.
///
/// A table because the pass runs over the block exactly once, before any
/// other pass has touched it, so every channel it sees is still one of the
/// 256 values a byte holds. Two hundred and fifty-six powers rather than
/// three a pixel: at 1920x1080 that is six million calls to `powf` turned
/// into 256, and about a sixth of the whole blur.
fn prepared(settings: &Blur) -> [f32; 256] {
    let contrast = settings.contrast.clamp(0.0, 2.0);
    let brightness = settings.brightness.clamp(0.0, 2.0).max(1.0);
    core::array::from_fn(|byte| {
        let value = f32::from(u8::try_from(byte).unwrap_or(u8::MAX)) / 255.0;
        let value = if contrast == 1.0 {
            value
        } else {
            gain(value, contrast)
        };
        value * brightness
    })
}

/// `blurFinish.glsl`: the dither and the darkening half of `brightness`,
/// over the block after the last pass.
fn finish(plane: &mut Plane, block: &Block, settings: &Blur) {
    let noise = settings.noise.clamp(0.0, 1.0);
    let brightness = settings.brightness.clamp(0.0, 2.0).min(1.0);
    if noise == 0.0 && brightness == 1.0 {
        return;
    }
    let width = plane.width;
    bands(&mut plane.pixels, width, |first, band| {
        finish_rows(
            band,
            first.saturating_mul(width),
            width,
            block,
            (noise, brightness),
        );
    });
}

/// [`finish`] over the pixels from `start` on, counted along the plane.
fn finish_rows(
    band: &mut [[f32; 4]],
    start: usize,
    width: usize,
    block: &Block,
    (noise, brightness): (f32, f32),
) {
    // A canvas is at most `MAX_SIZE` pixels on a side, so its size is exact
    // in the `u16` this reads it through and in the float that divides by
    // it.
    let screen = (
        f64::from(block.screen.0.max(1)),
        f64::from(block.screen.1.max(1)),
    );
    for (index, pixel) in (start..).zip(band.iter_mut()) {
        // The shader's `v_texcoord` at this fragment: where the pixel's
        // centre is on the monitor, from 0 to 1.
        let coord = |along: usize, from: i64, size: f64| -> f32 {
            let whole = from.saturating_add(i64::try_from(along).unwrap_or(0));
            #[expect(
                clippy::cast_precision_loss,
                reason = "a position on a canvas at most 16384 pixels wide"
            )]
            let centre = whole as f64 + 0.5;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a texture coordinate between zero and one, as the shader holds it"
            )]
            let coord = (centre / size) as f32;
            coord
        };
        let dither = (hash(
            coord(index % width, block.origin.0, screen.0),
            coord(index / width, block.origin.1, screen.1),
        ) - 0.5)
            * noise;
        for channel in pixel.iter_mut().take(3) {
            *channel = (*channel + dither) * brightness;
        }
    }
}

/// `gain` from `gain.glsl`: a symmetric contrast curve about the middle,
/// with `k` its exponent.
fn gain(value: f32, k: f32) -> f32 {
    let x = value.clamp(0.0, 1.0);
    let past_middle = x >= 0.5;
    let y = if past_middle { 1.0 - x } else { x };
    let a = 0.5 * (2.0 * y).powf(k);
    if past_middle { 1.0 - a } else { a }
}

/// What `blur1.glsl`'s vibrancy needs that does not change from pixel to
/// pixel.
struct Vibrancy {
    /// `vibrancy` divided by the number of passes: what one pass of the
    /// chain is allowed to add.
    boost: f32,
    /// `1 - vibrancy_darkness`, inverted so that it maps to the config
    /// setting, as the shader says.
    darkness: f32,
    /// `cos(a)` and `sin(a)` with the shader's `a = 0.93`, which weighs
    /// saturation against brightness. Worked out once: a cosine is a call
    /// out to the C library, and this would otherwise be two of them a
    /// pixel of every level of the pyramid.
    cosine: f32,
    sine: f32,
}

impl Vibrancy {
    /// What `settings` asks for, or `None` where `blur1.glsl` returns the
    /// colour it was given.
    fn of(settings: &Blur) -> Option<Self> {
        let vibrancy = settings.vibrancy.clamp(0.0, 1.0);
        if vibrancy == 0.0 || settings.passes == 0 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "at most eight passes, which Hyprland clamps to"
        )]
        let passes = settings.passes as f32;
        let a = 0.93_f32;
        Some(Self {
            boost: vibrancy / passes,
            darkness: 1.0 - settings.vibrancy_darkness.clamp(0.0, 1.0),
            cosine: a.cos(),
            sine: a.sin(),
        })
    }

    /// The colour with its saturation boosted, which is what `blur1.glsl`
    /// ends each downsample with.
    ///
    /// The constants are the shader's own -- `a = 0.93` weighs saturation
    /// against brightness, `b = 0.11` and `c = 0.66` place and smooth the
    /// step between boosted and unboosted -- and so is the division of the
    /// boost by the number of passes, which keeps a three-pass blur from
    /// being three times as saturated as a one-pass one.
    fn apply(&self, color: [f32; 4]) -> [f32; 4] {
        let (r, g, b) = (color[0], color[1], color[2]);
        let (hue, saturation, lightness) = rgb_to_hsl(r, g, b);
        // The "perceived brightness" of <http://alienryderflex.com/hsp.html>,
        // so that a deep blue is not boosted as hard as an equally saturated
        // yellow.
        let perceived = double_circle_sigmoid(
            (r * r * 0.299 + g * g * 0.587 + b * b * 0.114).sqrt(),
            0.8 * self.darkness,
        );
        let b1 = 0.11 * self.darkness;
        let boost = if saturation > 0.0 {
            let distance =
                (1.0 - saturation * self.cosine).powi(2) + (1.0 - perceived * self.sine).powi(2);
            smoothstep(b1 - 0.33, b1 + 0.33, 1.0 - distance)
        } else {
            0.0
        };
        let saturation = (saturation + boost * self.boost).clamp(0.0, 1.0);
        let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
        [r, g, b, color[3]]
    }
}

/// GLSL's `smoothstep`.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let span = edge1 - edge0;
    if span == 0.0 {
        return f32::from(u8::from(x >= edge1));
    }
    let t = ((x - edge0) / span).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The shaper of <http://www.flong.com/archive/texts/code/shapers_circ/>,
/// as `blur1.glsl` carries it.
fn double_circle_sigmoid(x: f32, a: f32) -> f32 {
    let a = a.clamp(0.0, 1.0);
    if x <= a {
        a - (a * a - x * x).max(0.0).sqrt()
    } else {
        a + ((1.0 - a).powi(2) - (x - 1.0).powi(2)).max(0.0).sqrt()
    }
}

/// `blur1.glsl`'s `rgb2hsl`, in its own order: hue, saturation, lightness.
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let smallest = r.min(g).min(b);
    let largest = r.max(g).max(b);
    let delta = largest - smallest;
    let lightness = (smallest + largest) * 0.5;
    let saturation = if lightness > 0.0 && lightness < 1.0 {
        let half = if lightness < 0.5 {
            lightness
        } else {
            1.0 - lightness
        };
        delta / (half * 2.0)
    } else {
        0.0
    };
    if delta <= 0.0 {
        return (0.0, saturation, lightness);
    }
    // The shader's masks: the channel that is the largest, unless the next
    // one round is equally large, which is how it breaks a tie.
    let masks = [
        r == largest && g != largest,
        g == largest && b != largest,
        b == largest && r != largest,
    ];
    let adds = [
        (g - b) / delta,
        2.0 + (b - r) / delta,
        4.0 + (r - g) / delta,
    ];
    let mut hue = 0.0;
    for (add, mask) in adds.into_iter().zip(masks) {
        if mask {
            hue += add;
        }
    }
    hue /= 6.0;
    if hue < 0.0 {
        hue += 1.0;
    }
    (hue, saturation, lightness)
}

/// `blur1.glsl`'s `hsl2rgb`, which is not the usual one: it builds the
/// colour from three ramps rather than from hue sectors.
fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (f32, f32, f32) {
    const THIRD: f32 = 1.0 / 3.0;
    const TWO_THIRDS: f32 = 2.0 / 3.0;
    const SIXTH: f32 = 6.0;
    let xt = if hue < THIRD {
        [SIXTH * (THIRD - hue), SIXTH * hue, 0.0]
    } else if hue < TWO_THIRDS {
        [0.0, SIXTH * (TWO_THIRDS - hue), SIXTH * (hue - THIRD)]
    } else {
        [SIXTH * (hue - TWO_THIRDS), 0.0, SIXTH * (1.0 - hue)]
    };
    let channel = |at: usize| {
        let x = xt.get(at).copied().unwrap_or(0.0).min(1.0);
        let ct = 2.0 * saturation * x + (1.0 - saturation);
        if lightness >= 0.5 {
            (1.0 - lightness) * ct + (2.0 * lightness - 1.0)
        } else {
            lightness * ct
        }
    };
    (channel(0), channel(1), channel(2))
}

/// `blurFinish.glsl`'s `hash`, tap for tap.
///
/// A hash and not a random number: an expected image has to be the same
/// bytes every run, and the shader's dither is a function of where the pixel
/// is and nothing else.
fn hash(x: f32, y: f32) -> f32 {
    let fract = |value: f32| value - value.floor();
    let seeded = [
        fract(x * 1689.1984),
        fract(y * 1689.1984),
        fract(x * 1689.1984),
    ];
    let (a, b, c) = (seeded[0], seeded[1], seeded[2]);
    // `p3 += dot(p3, p3.yzx + 33.33)`, which adds one scalar to all three.
    let scalar = a * (b + 33.33) + b * (c + 33.33) + c * (a + 33.33);
    fract((a + scalar + (b + scalar)) * (c + scalar))
}

/// Blur `pixels`, a tight `block.width × block.height` block of
/// premultiplied `RGBA` bytes, in place, and grade it as `settings` says.
///
/// Zero passes or zero size leaves the block alone, which is what
/// Hyprland's own zero does: the whole chain, grading included, is skipped
/// rather than run with no radius.
pub(crate) fn blur(pixels: &mut [u8], block: &Block, settings: &Blur) {
    let (width, height) = (block.width, block.height);
    if settings.passes == 0 || settings.size <= 0 || width == 0 || height == 0 {
        return;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a blur size is at most 100, which `f32` holds exactly"
    )]
    let radius = settings.size as f32;

    // The block, as floats, with `blurprepare.glsl` applied as it is read:
    // the contrast and the brightening half of `brightness` are functions
    // of one channel of one pixel, so they are a table rather than a pass.
    let graded = prepared(settings);
    let mut plane = Plane::new(width, height);
    let source: &[u8] = pixels;
    bands(&mut plane.pixels, width, |first, band| {
        let from = first.saturating_mul(width).saturating_mul(4);
        let bytes = source.get(from..).unwrap_or(&[]);
        for (slot, bytes) in band.iter_mut().zip(bytes.chunks_exact(4)) {
            let colour = |at: usize| {
                let byte = bytes.get(at).copied().unwrap_or(0);
                graded.get(usize::from(byte)).copied().unwrap_or(0.0)
            };
            let alpha = f32::from(bytes.get(3).copied().unwrap_or(0)) / 255.0;
            *slot = [colour(0), colour(1), colour(2), alpha];
        }
    });

    // Down, keeping each level's size so the way back up lands on it.
    // Each level's plane goes back to the kept ones as soon as the next has
    // been made from it, and comes out again for the level of the same
    // size on the way up: a frame's blur maps no memory once the first
    // frame has.
    let mut sizes = Vec::with_capacity(settings.passes as usize);
    for _ in 0..settings.passes {
        sizes.push((plane.width, plane.height));
        let next = down(&plane, radius, settings);
        std::mem::replace(&mut plane, next).back();
    }
    // And back up, in reverse: the last step to the block's own size a
    // plain doubling where the block is large ([`LARGE`]).
    let last = sizes.first().copied();
    for (wide, tall) in sizes.into_iter().rev() {
        let next = if Some((wide, tall)) == last && block.doubled {
            doubled(&plane, wide, tall)
        } else {
            up(&plane, wide, tall, radius)
        };
        std::mem::replace(&mut plane, next).back();
    }
    finish(&mut plane, block, settings);

    let blurred = &plane.pixels;
    bands(pixels, width.saturating_mul(4), |first, band| {
        let values = blurred.get(first.saturating_mul(width)..).unwrap_or(&[]);
        for (value, bytes) in values.iter().zip(band.chunks_exact_mut(4)) {
            written(*value, bytes);
        }
    });
    plane.back();
}

/// One blurred pixel as the four bytes it is written back as.
fn written(value: [f32; 4], bytes: &mut [u8]) {
    for (channel, slot) in value.into_iter().zip(bytes.iter_mut()) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a channel between zero and one becomes a byte"
        )]
        let byte = (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
        *slot = byte;
    }
}

#[cfg(test)]
mod tests;
