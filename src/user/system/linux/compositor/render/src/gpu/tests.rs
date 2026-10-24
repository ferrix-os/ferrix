//! The GPU's frame against the software one.
//!
//! The same [`render_onto`], the same layout, styles and pixels, drawn once
//! by [`crate::Canvas`] and once by [`Canvas`] on virglrenderer's test
//! server -- the renderer a guest's frames reach through QEMU, run on this
//! host's GL. The software frame is the reference, byte-exact against its
//! own expected images elsewhere in this crate; what is asked here is that
//! the GPU's is the same picture.
//!
//! "The same" is not byte for byte, and the module's own comment says why.
//! So a frame is judged by how many of its pixels are further than a small
//! step from the reference's, and how far the furthest is: a shape cut a
//! pixel differently at a corner is a handful of pixels, and a wrong colour,
//! a missing decoration or a flipped texture is thousands.
//!
//! A host with no `virgl_test_server` runs none of this, and says so.

use std::collections::BTreeMap;

use compositor_layout::{MonitorLayout, Settings, WindowId};
use compositor_virgl::vtest::Vtest;

use super::{Backdrop, Canvas};
use crate::tests::{client_buffers, decorated_style, plain_style, surfaces, two_clients_on};
use crate::{
    Blur, Damage, Format, LayerFrame, Painter, Pattern, Rect, Style, Styles, Surface, render_onto,
};

const SIZE: (u32, u32) = (512, 384);

/// How far apart a channel may be and the two frames still be one picture.
///
/// On the host these were first run on, no channel of any frame here is
/// more than two apart, corners and blur included: the shapes are cut at
/// the same pixels and the rest is a GPU's rounding. Three leaves another
/// host's GPU a step of its own, and anything wrong is tens.
const STEP: u8 = 3;

/// A server, or `None` with a line saying the test was skipped.
#[expect(
    clippy::print_stderr,
    reason = "a test that did not run says so where a person reading the run will see it"
)]
fn server(name: &str) -> Option<Vtest> {
    match Vtest::start(name) {
        Ok(Some(server)) => Some(server),
        Ok(None) => {
            eprintln!("{name}: no virgl_test_server on this host; skipped");
            None
        }
        Err(error) => {
            eprintln!("{name}: virgl_test_server would not start ({error}); skipped");
            None
        }
    }
}

/// How two frames differ: how many pixels have a colour channel more than
/// `step` apart, and the furthest any channel is.
fn difference(one: &[u8], other: &[u8], step: u8) -> (usize, u8) {
    assert_eq!(one.len(), other.len());
    let mut count = 0;
    let mut furthest = 0;
    for (a, b) in one.chunks_exact(4).zip(other.chunks_exact(4)) {
        // Blue, green and red; the fourth byte is not part of the picture.
        let apart = a
            .iter()
            .zip(b)
            .take(3)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        furthest = furthest.max(apart);
        if apart > step {
            count += 1;
        }
    }
    (count, furthest)
}

/// Both frames of one scene: the software one's bytes and the GPU's.
/// `kept` is whether a backdrop is handed over, which decides where a tiled
/// window's blur is read from.
fn both(
    name: &str,
    layout: &MonitorLayout,
    style: &Style,
    surfaces: &BTreeMap<WindowId, Surface<'_>>,
    layers: &[LayerFrame<'_>],
    kept: bool,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let server = server(name)?;
    let whole = Damage::full(SIZE.0, SIZE.1);
    let styles = Styles::plain(style);

    let mut software = crate::Canvas::new(SIZE.0, SIZE.1).expect("a canvas");
    let mut behind = crate::Backdrop::new(SIZE.0, SIZE.1).expect("a backdrop");
    let _ = render_onto(
        &mut software,
        kept.then_some(&mut behind),
        layout,
        (0, 0),
        &styles,
        surfaces,
        layers,
        &whole,
    );

    let mut gpu = Canvas::new(server, SIZE.0, SIZE.1).expect("a GPU canvas");
    let mut gpu_behind = Backdrop::new(SIZE.0, SIZE.1);
    let _ = render_onto(
        &mut gpu,
        kept.then_some(&mut gpu_behind),
        layout,
        (0, 0),
        &styles,
        surfaces,
        layers,
        &whole,
    );
    gpu.finish().expect("the frame was drawn");
    let drawn = gpu
        .read(Rect::new(0, 0, i64::from(SIZE.0), i64::from(SIZE.1)))
        .expect("read back");
    Some((software.data().to_vec(), drawn))
}

#[test]
fn two_plain_clients_are_the_same_picture() {
    let (_, layout) = two_clients_on(SIZE, Settings::default());
    let buffers = client_buffers(&layout);
    let Some((software, gpu)) = both(
        "plain",
        &layout,
        &plain_style(),
        &surfaces(&buffers),
        &[],
        true,
    ) else {
        return;
    };
    let (count, furthest) = difference(&software, &gpu, STEP);
    assert!(
        count == 0,
        "{count} pixels differ by more than {STEP}, the furthest by {furthest}"
    );
}

#[test]
fn rounding_opacity_shadow_and_dim_are_the_same_picture() {
    let (_, layout) = two_clients_on(SIZE, Settings::default());
    let buffers = client_buffers(&layout);
    let Some((software, gpu)) = both(
        "decorated",
        &layout,
        &decorated_style(),
        &surfaces(&buffers),
        &[],
        true,
    ) else {
        return;
    };
    let (count, furthest) = difference(&software, &gpu, STEP);
    assert!(
        count == 0,
        "{count} pixels differ by more than {STEP}, the furthest by {furthest}"
    );
}

/// Two translucent windows over a wallpaper that is a checkerboard: a blur
/// of one colour is that colour, and would prove nothing.
fn blurred_scene(name: &str, kept: bool) -> Option<(Vec<u8>, Vec<u8>)> {
    let (_, layout) = two_clients_on(SIZE, Settings::default());
    let mut buffers = client_buffers(&layout);
    for (data, _, _, format) in buffers.values_mut() {
        *format = Format::Argb8888;
        // One tint at a third, premultiplied: whatever edges show through
        // the window are then the wallpaper's and not its own.
        for pixel in data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[40, 20, 10, 85]);
        }
    }
    let wallpaper = Pattern::Checkerboard.draw(SIZE.0, SIZE.1);
    let layers = [LayerFrame {
        rect: Rect::new(0, 0, i64::from(SIZE.0), i64::from(SIZE.1)),
        above: false,
        surface: Some(
            Surface::new(
                &wallpaper,
                SIZE.0,
                SIZE.1,
                SIZE.0 * 4,
                Pattern::Checkerboard.format(),
            )
            .expect("a wallpaper"),
        ),
        dim_around: false,
        blur: false,
        xray: false,
    }];
    let style = Style {
        blur: Some(Blur {
            noise: 0.0,
            ..Blur::new(4, 2)
        }),
        ..plain_style()
    };
    both(name, &layout, &style, &surfaces(&buffers), &layers, kept)
}

/// The blur is a blur: the checkerboard's hard edges, seen through a window,
/// are gone from both frames. Without this the two could agree by neither
/// having blurred anything.
fn edges_through_the_window(frame: &[u8]) -> usize {
    let (wide, tall) = (SIZE.0 as usize, SIZE.1 as usize);
    let row = tall / 2;
    (wide / 8..wide / 2 - wide / 8)
        .filter(|x| {
            let at = (row * wide + x) * 4;
            match (frame.get(at), frame.get(at + 4)) {
                (Some(here), Some(next)) => here.abs_diff(*next) > 24,
                _ => false,
            }
        })
        .count()
}

#[test]
fn a_blur_of_the_backdrop_is_the_same_picture() {
    let Some((software, gpu)) = blurred_scene("blurred-backdrop", true) else {
        return;
    };
    assert_eq!(edges_through_the_window(&software), 0, "the reference");
    assert_eq!(edges_through_the_window(&gpu), 0, "the GPU's");
    let (count, furthest) = difference(&software, &gpu, STEP);
    assert!(
        count == 0,
        "{count} pixels differ by more than {STEP}, the furthest by {furthest}"
    );
}

#[test]
fn a_blur_of_the_frame_so_far_is_the_same_picture() {
    let Some((software, gpu)) = blurred_scene("blurred-frame", false) else {
        return;
    };
    assert_eq!(edges_through_the_window(&software), 0, "the reference");
    assert_eq!(edges_through_the_window(&gpu), 0, "the GPU's");
    let (count, furthest) = difference(&software, &gpu, STEP);
    assert!(
        count == 0,
        "{count} pixels differ by more than {STEP}, the furthest by {furthest}"
    );
}

/// A second frame, drawn only inside its damage, is the software's second
/// frame: part of a named surface is moved and no more, the backdrop's
/// tiles outside the damage are not blurred again, and nothing outside the
/// damage is touched.
#[test]
fn a_frame_drawn_inside_its_damage_is_the_same_picture() {
    let Some(server) = server("damaged") else {
        return;
    };
    let (_, layout) = two_clients_on(SIZE, Settings::default());
    let mut buffers = client_buffers(&layout);
    for (data, _, _, format) in buffers.values_mut() {
        *format = Format::Argb8888;
        for pixel in data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[40, 20, 10, 85]);
        }
    }
    let wallpaper = Pattern::Checkerboard.draw(SIZE.0, SIZE.1);
    let style = Style {
        blur: Some(Blur {
            noise: 0.0,
            ..Blur::new(4, 2)
        }),
        ..decorated_style()
    };
    let styles = Styles::plain(&style);
    let mut software = crate::Canvas::new(SIZE.0, SIZE.1).expect("a canvas");
    let mut behind = crate::Backdrop::new(SIZE.0, SIZE.1).expect("a backdrop");
    let mut gpu = Canvas::new(server, SIZE.0, SIZE.1).expect("a GPU canvas");
    let mut gpu_behind = Backdrop::new(SIZE.0, SIZE.1);

    let mut before = Vec::new();
    let first = layout.windows.first().expect("a window");
    // A patch in the first window, which is what its client draws next.
    let patch = Rect::new(first.rect.x + 30, first.rect.y + 40, 50, 20);
    for (frame, damage) in [Damage::full(SIZE.0, SIZE.1), {
        let mut damage = Damage::default();
        damage.add(patch);
        damage
    }]
    .into_iter()
    .enumerate()
    {
        if frame == 1 {
            let (data, wide, _, _) = buffers.get_mut(&first.window).expect("its buffer");
            let wide = *wide as usize;
            for row in data.chunks_exact_mut(wide * 4).skip(40).take(20) {
                for pixel in row.chunks_exact_mut(4).skip(30).take(50) {
                    pixel.copy_from_slice(&[0, 0, 200, 255]);
                }
            }
        }
        let named: BTreeMap<WindowId, Surface<'_>> = surfaces(&buffers)
            .into_iter()
            .map(|(window, surface)| (window, surface.named(window.0)))
            .collect();
        let layers = [LayerFrame {
            rect: Rect::new(0, 0, i64::from(SIZE.0), i64::from(SIZE.1)),
            above: false,
            surface: Some(
                Surface::new(
                    &wallpaper,
                    SIZE.0,
                    SIZE.1,
                    SIZE.0 * 4,
                    Pattern::Checkerboard.format(),
                )
                .expect("a wallpaper")
                .named(1 << 32),
            ),
            dim_around: false,
            blur: false,
            xray: false,
        }];
        let _ = render_onto(
            &mut software,
            Some(&mut behind),
            &layout,
            (0, 0),
            &styles,
            &named,
            &layers,
            &damage,
        );
        let _ = render_onto(
            &mut gpu,
            Some(&mut gpu_behind),
            &layout,
            (0, 0),
            &styles,
            &named,
            &layers,
            &damage,
        );
        gpu.finish().expect("the frame was drawn");
        if frame == 0 {
            before = gpu
                .read(Rect::new(0, 0, i64::from(SIZE.0), i64::from(SIZE.1)))
                .expect("read back");
        }
    }
    let drawn = gpu
        .read(Rect::new(0, 0, i64::from(SIZE.0), i64::from(SIZE.1)))
        .expect("read back");
    let (count, furthest) = difference(software.data(), &drawn, STEP);
    assert!(
        count == 0,
        "{count} pixels differ by more than {STEP}, the furthest by {furthest}"
    );
    // And the second frame changed the patch and nothing but the patch:
    // outside the damage the GPU's frame is the first frame's bytes.
    let mut inside = 0;
    for (at, (was, is)) in before
        .chunks_exact(4)
        .zip(drawn.chunks_exact(4))
        .enumerate()
    {
        let (x, y) = (at % SIZE.0 as usize, at / SIZE.0 as usize);
        let place = (i64::try_from(x).unwrap_or(0), i64::try_from(y).unwrap_or(0));
        let within = (patch.x..patch.right()).contains(&place.0)
            && (patch.y..patch.bottom()).contains(&place.1);
        if within {
            inside += usize::from(was != is);
        } else {
            assert_eq!(was, is, "({x}, {y}) is outside the damage");
        }
    }
    assert!(inside > 500, "only {inside} pixels of the patch changed");
}

/// A renderer that has seen many sizes keeps only a few of their textures,
/// and still draws after letting the rest go.
///
/// Two things, and the second is the one worth a test: that what is kept is
/// bounded, and that the renderer goes on drawing afterwards -- which it
/// would not if it let go of a texture something still names, since
/// virglrenderer refuses a resource it has unreferenced.
#[test]
fn textures_of_sizes_long_out_of_use_are_let_go_of() {
    let Some(server) = server("retired") else {
        return;
    };
    let mut gpu = Canvas::new(server, SIZE.0, SIZE.1).expect("a GPU canvas");
    let whole = Damage::full(SIZE.0, SIZE.1);
    // Each frame a surface of its own size, named so that it is kept, and
    // never drawn again.
    for step in 0..(super::MAX_SPARE * 3) as u32 {
        let (wide, tall) = (16 + step, 16 + step);
        let data = vec![0x80_u8; (wide * tall * 4) as usize];
        let surface = Surface::new(&data, wide, tall, wide * 4, Format::Argb8888)
            .expect("a surface")
            .named(u64::from(step) + 1);
        Painter::composite(
            &mut gpu,
            &surface,
            Rect::new(0, 0, i64::from(wide), i64::from(tall)),
            &whole,
        );
        gpu.finish().expect("the frame was drawn");
        // Each frame counts against what a surface is kept for, so nothing
        // retires until they have all been seen; wind the clock instead.
        gpu.frame = gpu.frame.saturating_add(super::KEPT_FRAMES);
        gpu.retire();
    }
    assert!(
        gpu.spare.len() <= super::MAX_SPARE,
        "{} textures kept for the next surface of their size",
        gpu.spare.len()
    );
    // And the canvas still draws: a clear, read back.
    Painter::clear(&mut gpu, crate::Color(0xff20_4060), &whole);
    gpu.finish().expect("the frame was drawn");
    let drawn = gpu.read(Rect::new(0, 0, 4, 1)).expect("read back");
    assert_eq!(drawn.get(..3), Some(&[0x60, 0x40, 0x20][..]));
}

/// A draw waiting to be submitted keeps the pixels it was written with, even
/// when the texture it reads is written again before the frame ends.
///
/// The renderer batches a frame into one submission, which it can only do
/// because moving pixels into a texture nothing waiting reads changes
/// nothing. This is the case where something waiting *does* read it: one
/// surface drawn twice in a frame, with different pixels each time. The
/// first draw must show the first pixels -- which it does only if the
/// second upload submitted what was waiting first.
#[test]
fn a_waiting_draw_keeps_the_pixels_it_was_written_with() {
    let Some(server) = server("reused") else {
        return;
    };
    let (wide, tall) = (64, 32);
    let mut gpu = Canvas::new(server, wide, tall).expect("a GPU canvas");
    let whole = Damage::full(wide, tall);
    Painter::clear(&mut gpu, crate::Color(0xff00_0000), &whole);

    // One surface by one name, drawn twice, its pixels changed between.
    let side = 16;
    let bytes = (side * side * 4) as usize;
    let red = vec![[0x00, 0x00, 0xff, 0xff]; bytes / 4].concat();
    let blue = vec![[0xff, 0x00, 0x00, 0xff]; bytes / 4].concat();
    // One name, so the renderer keeps one texture for both.
    let first = Surface::new(&red, side, side, side * 4, Format::Xrgb8888)
        .expect("a surface")
        .named(7);
    let second = Surface::new(&blue, side, side, side * 4, Format::Xrgb8888)
        .expect("a surface")
        .named(7);
    Painter::composite(
        &mut gpu,
        &first,
        Rect::new(0, 0, i64::from(side), i64::from(side)),
        &whole,
    );
    Painter::composite(
        &mut gpu,
        &second,
        Rect::new(32, 0, i64::from(side), i64::from(side)),
        &whole,
    );
    gpu.finish().expect("the frame was drawn");

    let drawn = gpu
        .read(Rect::new(0, 0, i64::from(wide), i64::from(tall)))
        .expect("read back");
    let at = |x: u32, y: u32| {
        let start = ((y * wide + x) * 4) as usize;
        drawn.get(start..start + 3).map(<[u8]>::to_vec)
    };
    assert_eq!(at(8, 8), Some(vec![0x00, 0x00, 0xff]), "the first draw");
    assert_eq!(at(40, 8), Some(vec![0xff, 0x00, 0x00]), "the second");
}

/// Yoru's decorations (Ferrix's docs/DESKTOP.md §6): the focused window's
/// glow over its deep shadow, a lighter shadow under the other, drawn the
/// same by the GPU as by the software renderer.
fn yoru_style() -> Style {
    let config = compositor_config::parse(
        "test",
        "decoration:rounding = 8\n\
         decoration:shadow:range = 40\n\
         decoration:shadow:render_power = 2\n\
         decoration:shadow:offset = 0 16\n\
         decoration:shadow:color = rgba(0000008c)\n\
         decoration:shadow:color_inactive = rgba(00000066)\n\
         decoration:glow:enabled = 1\n\
         decoration:glow:range = 36\n\
         decoration:glow:render_power = 3\n\
         decoration:glow:color = rgba(9184d940)\n",
        &mut compositor_config::NoSources,
    )
    .config;
    let style = Style::from_config(&config).undithered();
    assert!(style.glow.is_some() && style.shadow_inactive.is_some());
    style
}

#[test]
fn yoru_glow_and_shadows_are_the_same_picture() {
    let (_, layout) = two_clients_on(SIZE, Settings::default());
    let buffers = client_buffers(&layout);
    let Some((software, gpu)) = both(
        "yoru",
        &layout,
        &yoru_style(),
        &surfaces(&buffers),
        &[],
        true,
    ) else {
        return;
    };
    let (count, furthest) = difference(&software, &gpu, STEP);
    assert!(
        count == 0,
        "{count} pixels differ by more than {STEP}, the furthest by {furthest}"
    );
}

/// What Yoru's decorations cost a 1920x1080 frame on the GPU path, against
/// the same frame with none (`cargo test -p compositor-render --release
/// -- --ignored yoru_frame_cost --nocapture`): two windows, then with the
/// shadows, with the glow too, and with the blur behind a translucent bar
/// across the foot, as the dock is. Measured on virglrenderer's test
/// server over this host's GL, which is the path a guest's frames take
/// through QEMU; each the median of twenty whole frames after five warm.
#[test]
#[ignore = "a measurement, not a check"]
#[expect(clippy::print_stderr, reason = "the measurement is the output")]
fn yoru_frame_cost() {
    const BIG: (u32, u32) = (1920, 1080);
    let (_, layout) = two_clients_on(BIG, Settings::default());
    let buffers = client_buffers(&layout);
    let surfaces = surfaces(&buffers);
    let wallpaper = Pattern::Checkerboard.draw(BIG.0, BIG.1);
    let bar: Vec<u8> = [0x26, 0x18, 0x16, 0xc7].repeat((BIG.0 * 60) as usize);
    let full = Rect::new(0, 0, i64::from(BIG.0), i64::from(BIG.1));
    let wall = || LayerFrame {
        rect: full,
        above: false,
        surface: Some(
            Surface::new(
                &wallpaper,
                BIG.0,
                BIG.1,
                BIG.0 * 4,
                Pattern::Checkerboard.format(),
            )
            .expect("a wallpaper"),
        ),
        dim_around: false,
        blur: false,
        xray: false,
    };
    let dock = |blur: bool| LayerFrame {
        rect: Rect::new(12, i64::from(BIG.1) - 66, i64::from(BIG.0) - 24, 60),
        above: true,
        surface: Some(Surface::new(&bar, BIG.0, 60, BIG.0 * 4, Format::Argb8888).expect("a bar")),
        dim_around: false,
        blur,
        xray: false,
    };
    let none = Style {
        shadow: None,
        glow: None,
        blur: None,
        ..yoru_style()
    };
    let shadows = Style {
        glow: None,
        blur: None,
        ..yoru_style()
    };
    let glow = Style {
        blur: None,
        ..yoru_style()
    };
    let blurred = Style {
        blur: Some(Blur {
            noise: 0.0,
            ..Blur::new(6, 2)
        }),
        ..yoru_style()
    };
    let cases: [(&str, &Style, bool); 4] = [
        ("no decorations", &none, false),
        ("+ shadows", &shadows, false),
        ("+ glow", &glow, false),
        ("+ the dock's blur", &blurred, true),
    ];
    for (what, style, blur) in cases {
        let Some(server) = server("yoru-cost") else {
            return;
        };
        let mut gpu = Canvas::new(server, BIG.0, BIG.1).expect("a GPU canvas");
        let mut behind = Backdrop::new(BIG.0, BIG.1);
        let styles = Styles::plain(style);
        let layers = [wall(), dock(blur)];
        let whole = Damage::full(BIG.0, BIG.1);
        let mut times = Vec::new();
        for round in 0..25 {
            let started = std::time::Instant::now();
            let _ = render_onto(
                &mut gpu,
                Some(&mut behind),
                &layout,
                (0, 0),
                &styles,
                &surfaces,
                &layers,
                &whole,
            );
            gpu.finish().expect("the frame was drawn");
            let _ = gpu.read(Rect::new(0, 0, 1, 1)).expect("waited for");
            if round >= 5 {
                times.push(started.elapsed());
            }
        }
        times.sort();
        let median = times[times.len() / 2];
        // The same frame in software, for the frames a guest without a 3D
        // card draws.
        let mut software = crate::Canvas::new(BIG.0, BIG.1).expect("a canvas");
        let mut kept = crate::Backdrop::new(BIG.0, BIG.1).expect("a backdrop");
        let mut cpu = Vec::new();
        for _ in 0..7 {
            let started = std::time::Instant::now();
            let _ = render_onto(
                &mut software,
                Some(&mut kept),
                &layout,
                (0, 0),
                &styles,
                &surfaces,
                &layers,
                &whole,
            );
            cpu.push(started.elapsed());
        }
        cpu.sort();
        let on_cpu = cpu[cpu.len() / 2];
        eprintln!("yoru frame cost, 1920x1080: {what}: GPU {median:?}, software {on_cpu:?}");
    }
}
