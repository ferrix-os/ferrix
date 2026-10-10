//! The frame, drawn by a GPU: a [`Painter`] that writes virgl streams.
//!
//! [`crate::Canvas`] fills a frame's pixels itself, and a blurred window over
//! a moving wallpaper is as fast as that gets: 37 ms of arithmetic a frame
//! (`docs/GPU.md` §3.5). This is the same frame as draw calls. Each
//! operation of the trait is a quad or a few, a fragment shader that cuts
//! the shape `crate::Canvas` cuts, and blending the way it blends; the blur
//! is Hyprland's own four passes over a pyramid of textures rather than this
//! crate's port of them to floats. Nothing here draws a pixel. It writes
//! words, `src/user/system/linux/compositor/virgl`'s, and a [`Device`] runs them: the render node
//! in a guest, virglrenderer's test server on a host, which is where every
//! line of this is tested against the software frame.
//!
//! # What is the same as the software frame, and what is not
//!
//! The order, the shapes and the colours are the same, because the frame is
//! [`crate::render_onto`] either way and the shapes are cut by the same
//! superellipse at the same pixel centres. What differs is arithmetic: a GPU
//! blends in its own precision and rounds its own way, and its blur is
//! eight-bit textures where the software one is floats. So a GPU frame is
//! judged against the software one within a step or two of a channel, not
//! byte for byte; the bytes are the software renderer's to hold.
//!
//! # Damage
//!
//! Every operation draws only inside the damage it is given, as the trait
//! says, and here that is geometry rather than a scissor: the quads drawn
//! are the operation's rectangle cut by each rectangle of the damage, and a
//! shape is cut in the fragment shader from the pixel's own place, so a quad
//! can be any part of it. It has to be exact. A translucent fill drawn again
//! over a pixel that was not cleared this frame is a pixel twice as dark.
//!
//! # Surfaces
//!
//! A client's pixels are in its own memory and a GPU samples its own, so
//! each surface is a texture here, kept from frame to frame by the surface's
//! [`Surface::name`], and what is moved each frame is the part of it under
//! the damage -- which is all of it that can have changed on the screen, by
//! the same argument that lets the software frame redraw only the damage. A
//! surface with no name is moved whole whenever it is drawn.
//!
//! # Failure
//!
//! The trait's operations answer nothing, so a device that fails is
//! remembered: everything after it is skipped, and [`Canvas::finish`] says
//! what went wrong. The compositor's answer to that is the software
//! renderer.

mod backdrop;
mod blur;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use compositor_virgl::{
    Blend, Device, Rasterizer, Region, Sampler, Stream, Texture, VertexBuffer, VertexElement, View,
    pipe, shaders,
};

pub use self::backdrop::Backdrop;
use crate::damage::intersect;
use crate::{Blur, Color, Damage, Format, Gradient, Painter, Rect, Rounding, Shadow, Surface};

// State objects, which this context numbers. Fixed ones first; everything
// made as the frame needs it counts up from `FIRST_MADE`.
const BLEND_OVER: u32 = 1;
const BLEND_REPLACE: u32 = 2;
const DSA: u32 = 3;
const RASTERIZER: u32 = 4;
const ELEMENTS: u32 = 5;
const VERTEX: u32 = 6;
const SAMPLER_NEAREST: u32 = 7;
const SAMPLER_LINEAR: u32 = 8;
const FIRST_MADE: u32 = 100;

/// A fragment shader, by the handle it is made under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
enum Program {
    Solid = 10,
    Surface = 11,
    Gradient = 12,
    Shadow = 13,
    BlurPrepare = 14,
    BlurDown = 15,
    BlurUp = 16,
    BlurFinish = 17,
}

impl Program {
    const ALL: [(Self, &'static str); 8] = [
        (Self::Solid, shaders::SOLID),
        (Self::Surface, shaders::SURFACE),
        (Self::Gradient, shaders::GRADIENT),
        (Self::Shadow, shaders::SHADOW),
        (Self::BlurPrepare, shaders::BLUR_PREPARE),
        (Self::BlurDown, shaders::BLUR_DOWN),
        (Self::BlurUp, shaders::BLUR_UP),
        (Self::BlurFinish, shaders::BLUR_FINISH),
    ];
}

/// How many bytes the vertex buffer is. Vertices are written into it in
/// turn and it is begun again when it is full: a draw reads what was written
/// before it in the stream, whatever is written after.
const VERTEX_BYTES: u32 = 256 * 1024;

/// Bytes of one vertex: a place and a texture coordinate, two floats each.
const VERTEX_STRIDE: u32 = 16;

/// The most quads one draw carries, which keeps a draw's vertices well
/// inside one command and one command well inside one submission.
const QUADS_A_DRAW: usize = 128;

/// How full a stream gets before it is submitted, in bytes: under the render
/// node's 64 KiB with room for the longest single operation.
const FLUSH_BYTES: usize = 32 * 1024;

/// Frames a surface's texture is kept for without being drawn, before it is
/// handed to the next surface of its size.
const KEPT_FRAMES: u64 = 120;

/// How many textures are kept for the next surface of their size.
///
/// A desktop has a few sizes in play -- the windows, and each animation
/// step of one being resized -- and making a texture costs a message and a
/// backing the kernel pins, so a handful are worth keeping. Past that they
/// are let go of: a session of windows dragged by their corners would
/// otherwise keep a texture for every width they were ever at.
const MAX_SPARE: usize = 8;

/// How many refused imports are remembered before the memory of them is
/// let go of and the device asked again.
const REFUSED_MOST: usize = 256;

/// A texture with the two ways of using it: drawn into, and read from.
#[derive(Clone, Copy, Debug)]
struct Image {
    resource: u32,
    /// The surface object it is drawn into through.
    surface: u32,
    /// The sampler view it is read through, alpha and all.
    view: u32,
    /// The same, reading alpha as one.
    opaque_view: u32,
    width: u32,
    height: u32,
}

/// A surface's texture, and when it was last drawn.
#[derive(Clone, Copy, Debug)]
struct Kept {
    image: Image,
    used: u64,
}

/// A frame drawn on a GPU, the size of the output.
#[derive(Debug)]
pub struct Canvas<D: Device> {
    device: D,
    width: u32,
    height: u32,
    /// What the frame is drawn into, and what a window's blur is read from.
    target: Image,
    /// The blur's pyramid, level 0 the canvas's size; and where a finished
    /// blur waits to be copied into a window's shape.
    levels: Vec<Image>,
    blurred: Option<Image>,
    stream: Stream,
    vertices: u32,
    vertex_at: u32,
    next_handle: u32,
    /// What is bound, so that nothing is bound twice.
    bound_target: Option<u32>,
    bound_program: Option<Program>,
    bound_blend: Option<u32>,
    bound_view: Option<(u32, u32)>,
    surfaces: BTreeMap<u64, Kept>,
    /// Clients' GPU buffers this canvas imported, by the key their surface
    /// gave (`Surface::on_device`). Never handed to another surface: the
    /// pixels are the client's, and nothing is uploaded into them.
    imports: BTreeMap<u64, Kept>,
    /// Clients' GPU buffers the device would not import, by the same key:
    /// not asked again, which would be a round trip a frame for an answer
    /// that does not change.
    refused: BTreeSet<u64>,
    spare: Vec<Image>,
    ramps: Vec<(Gradient, Image)>,
    /// The resources the draws written since the last submission read from.
    ///
    /// Moving pixels into a texture is not part of the command stream: it
    /// happens when it is asked for, while what is in the stream happens
    /// when the stream is submitted. So a draw that is written but not yet
    /// submitted would read pixels that arrived after it -- but only if it
    /// reads *that* texture, and this is how that is known.
    sampled: Vec<u32>,
    frame: u64,
    failed: Option<io::Error>,
}

/// `value` as a float: a position on a screen, exact in `f32`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a position on a canvas at most 16384 pixels wide, exact in f32"
)]
const fn float(value: i64) -> f32 {
    value as f32
}

/// `value`, known to be inside a canvas, as a texture's coordinate.
fn unsigned(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(0)
}

/// A rectangle as a shader takes one: x, y, width, height.
const fn rect_floats(rect: Rect) -> [f32; 4] {
    [
        float(rect.x),
        float(rect.y),
        float(rect.width),
        float(rect.height),
    ]
}

/// A colour as a shader takes one: premultiplied, red first.
fn premultiplied(color: Color) -> [f32; 4] {
    let alpha = f32::from(color.alpha()) / 255.0;
    [
        f32::from(color.red()) / 255.0 * alpha,
        f32::from(color.green()) / 255.0 * alpha,
        f32::from(color.blue()) / 255.0 * alpha,
        alpha,
    ]
}

/// A rounding as a shader takes one: `(radius, power)`.
fn rounding_floats(rounding: Rounding) -> [f32; 2] {
    [
        float(rounding.radius.max(0)),
        rounding.power.clamp(1.0, 10.0),
    ]
}

impl<D: Device> Canvas<D> {
    /// An opaque black `width` × `height` frame on `device`.
    ///
    /// # Errors
    ///
    /// The device's, for the textures and the state every frame uses.
    pub fn new(device: D, width: u32, height: u32) -> io::Result<Self> {
        if width == 0 || height == 0 || width > crate::MAX_SIZE || height > crate::MAX_SIZE {
            return Err(io::Error::other("a canvas of no size, or too large"));
        }
        let mut canvas = Self {
            device,
            width,
            height,
            target: Image {
                resource: 0,
                surface: 0,
                view: 0,
                opaque_view: 0,
                width,
                height,
            },
            levels: Vec::new(),
            blurred: None,
            stream: Stream::new(),
            vertices: 0,
            vertex_at: 0,
            next_handle: FIRST_MADE,
            bound_target: None,
            bound_program: None,
            bound_blend: None,
            bound_view: None,
            surfaces: BTreeMap::new(),
            imports: BTreeMap::new(),
            refused: BTreeSet::new(),
            spare: Vec::new(),
            ramps: Vec::new(),
            sampled: Vec::new(),
            frame: 0,
            failed: None,
        };
        canvas.vertices = canvas.device.buffer(VERTEX_BYTES)?;
        canvas.begin();
        // The frame's own texture is one a screen may be pointed at, so that
        // a card with a screen beside this renderer can show what was drawn
        // rather than being handed a copy of it.
        canvas.target = canvas.made(width, height, true, true, true)?;
        let whole = Damage::full(width, height);
        Painter::clear(&mut canvas, Color(0xff00_0000), &whole);
        canvas.finish()?;
        Ok(canvas)
    }

    /// The frame's width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The frame's height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The texture the frame is drawn into, by the number a stream -- or a
    /// scanout -- names it by.
    #[must_use]
    pub const fn resource(&self) -> u32 {
        self.target.resource
    }

    /// The device, for whoever shows the frame.
    pub const fn device(&mut self) -> &mut D {
        &mut self.device
    }

    /// A descriptor for the frame's own texture that a card can be shown, if
    /// this device has one.
    ///
    /// A screen pointed at it shows what was drawn without the pixels
    /// leaving the device, which is what [`Canvas::read`] otherwise costs
    /// every frame.
    ///
    /// # Errors
    ///
    /// The device's.
    pub fn export(&mut self) -> io::Result<Option<std::os::fd::OwnedFd>> {
        let resource = self.target.resource;
        self.device.export(resource)
    }

    /// Submit what has been drawn, and say whether all of it could be.
    ///
    /// One frame is one call of this, after the last operation: it is also
    /// what counts the frames a surface's texture is kept by.
    ///
    /// # Errors
    ///
    /// The first thing the device refused since the last call. The frame is
    /// then not the one that was asked for, and the caller's answer is the
    /// software renderer.
    pub fn finish(&mut self) -> io::Result<()> {
        self.flush();
        self.frame = self.frame.saturating_add(1);
        self.retire();
        self.failed.take().map_or(Ok(()), Err)
    }

    /// Read `rect` of the frame back: four bytes a pixel, blue first, rows
    /// packed. It comes after everything drawn so far.
    ///
    /// # Errors
    ///
    /// The device's, or [`Canvas::finish`]'s.
    pub fn read(&mut self, rect: Rect) -> io::Result<Vec<u8>> {
        self.flush();
        if let Some(error) = self.failed.take() {
            return Err(error);
        }
        let Some(rect) = intersect(rect, Painter::bounds(self)) else {
            return Ok(Vec::new());
        };
        self.device.read(
            self.target.resource,
            Region {
                x: unsigned(rect.x),
                y: unsigned(rect.y),
                width: unsigned(rect.width),
                height: unsigned(rect.height),
            },
        )
    }

    /// Read `rect` of the frame straight into a screen's `buffer`, rows
    /// `stride` bytes apart, at `rect`'s own place in it: [`Canvas::read`]
    /// without the vector between, which a device with shared memory
    /// fills in one copy.
    ///
    /// # Errors
    ///
    /// The device's, or [`Canvas::finish`]'s; a buffer too short.
    pub fn read_into(&mut self, rect: Rect, buffer: &mut [u8], stride: u32) -> io::Result<()> {
        self.flush();
        if let Some(error) = self.failed.take() {
            return Err(error);
        }
        let Some(rect) = intersect(rect, Painter::bounds(self)) else {
            return Ok(());
        };
        let start = (unsigned(rect.y) as usize)
            .checked_mul(stride as usize)
            .and_then(|start| start.checked_add(unsigned(rect.x) as usize * 4))
            .ok_or_else(|| io::Error::other("a rectangle outside the buffer"))?;
        let into = buffer
            .get_mut(start..)
            .ok_or_else(|| io::Error::other("a rectangle outside the buffer"))?;
        self.device.read_into(
            self.target.resource,
            Region {
                x: unsigned(rect.x),
                y: unsigned(rect.y),
                width: unsigned(rect.width),
                height: unsigned(rect.height),
            },
            into,
            stride as usize,
        )
    }

    /// The state every draw shares, made once.
    fn begin(&mut self) {
        let stream = &mut self.stream;
        stream.create_blend(BLEND_OVER, Blend::PREMULTIPLIED_OVER);
        stream.create_blend(BLEND_REPLACE, Blend::REPLACE);
        stream.create_dsa(DSA);
        stream.bind_object(pipe::object::DSA, DSA);
        stream.create_rasterizer(RASTERIZER, Rasterizer { scissor: false });
        stream.bind_object(pipe::object::RASTERIZER, RASTERIZER);
        stream.create_vertex_elements(
            ELEMENTS,
            &[
                VertexElement {
                    offset: 0,
                    buffer: 0,
                    format: pipe::FORMAT_R32G32_FLOAT,
                },
                VertexElement {
                    offset: 8,
                    buffer: 0,
                    format: pipe::FORMAT_R32G32_FLOAT,
                },
            ],
        );
        stream.bind_object(pipe::object::VERTEX_ELEMENTS, ELEMENTS);
        let _ = stream.create_shader(
            VERTEX,
            pipe::SHADER_VERTEX,
            shaders::VERTEX,
            shaders::TOKENS,
        );
        stream.bind_shader(VERTEX, pipe::SHADER_VERTEX);
        stream.create_sampler_state(
            SAMPLER_NEAREST,
            Sampler {
                filter: pipe::TEX_FILTER_NEAREST,
            },
        );
        stream.create_sampler_state(
            SAMPLER_LINEAR,
            Sampler {
                filter: pipe::TEX_FILTER_LINEAR,
            },
        );
        // The shaders in a submission each: the longest is a few kilobytes
        // of text, and all of them at once is more than a submission holds.
        for (program, text) in Program::ALL {
            let _ = self.stream.create_shader(
                program as u32,
                pipe::SHADER_FRAGMENT,
                text,
                shaders::TOKENS,
            );
            self.flush();
        }
    }

    /// Remember the first failure; everything after it is skipped.
    fn fail(&mut self, error: io::Error) {
        if self.failed.is_none() {
            self.failed = Some(error);
        }
    }

    /// Submit the stream so far.
    fn flush(&mut self) {
        if self.stream.is_empty() {
            return;
        }
        if self.failed.is_none()
            && let Err(error) = self.device.submit(self.stream.words())
        {
            self.fail(error);
        }
        self.stream.reset();
        // Nothing is waiting to be submitted any more, so nothing waiting
        // can read a texture written after it.
        self.sampled.clear();
    }

    /// Submit what is waiting if any of it reads `resource`, which pixels
    /// are about to be moved into.
    ///
    /// The ordering [`Canvas::sampled`] describes. In a frame as this
    /// compositor draws one each surface is brought up to date and then
    /// drawn, so the texture being written is one nothing waiting reads and
    /// this costs nothing -- which is the point, since a submission is a
    /// round trip through the driver and the device.
    fn flush_readers_of(&mut self, resource: u32) {
        if self.sampled.contains(&resource) {
            self.flush();
        }
    }

    /// Submit if the stream is getting full. Between operations only: an
    /// operation's state and its draw go in one submission.
    fn room(&mut self) {
        if self.stream.len_bytes() > FLUSH_BYTES {
            self.flush();
        }
    }

    /// A new state object's handle.
    fn handle(&mut self) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(FIRST_MADE);
        handle
    }

    /// Make a texture with its surface and its views. `moved` is whether
    /// pixels go to or from it; `drawn` whether it is ever a target.
    fn image(&mut self, width: u32, height: u32, moved: bool, drawn: bool) -> io::Result<Image> {
        self.made(width, height, moved, drawn, false)
    }

    /// The same, saying whether a screen may be shown it.
    fn made(
        &mut self,
        width: u32,
        height: u32,
        moved: bool,
        drawn: bool,
        scanout: bool,
    ) -> io::Result<Image> {
        let bind = if drawn {
            pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW
        } else {
            pipe::BIND_SAMPLER_VIEW
        };
        let resource = self.device.texture(Texture {
            width,
            height,
            format: pipe::FORMAT_B8G8R8A8_UNORM,
            bind,
            moved,
            scanout,
        })?;
        let image = Image {
            resource,
            surface: if drawn { self.handle() } else { 0 },
            view: self.handle(),
            opaque_view: self.handle(),
            width,
            height,
        };
        if drawn {
            self.stream
                .create_surface(image.surface, resource, pipe::FORMAT_B8G8R8A8_UNORM);
        }
        for (handle, opaque) in [(image.view, false), (image.opaque_view, true)] {
            self.stream.create_sampler_view(
                handle,
                View {
                    resource,
                    format: pipe::FORMAT_B8G8R8A8_UNORM,
                    opaque,
                },
            );
        }
        Ok(image)
    }

    /// Draw into `image` from here on.
    fn aim(&mut self, image: Image) {
        if self.bound_target == Some(image.resource) {
            return;
        }
        self.bound_target = Some(image.resource);
        self.stream.set_framebuffer(image.surface);
        self.stream.set_viewport(image.width, image.height);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a texture's size, at most 16384, exact in f32"
        )]
        let (wide, tall) = (image.width as f32, image.height as f32);
        self.stream
            .set_constants(pipe::SHADER_VERTEX, &[2.0 / wide, 2.0 / tall, -1.0, -1.0]);
    }

    /// Draw with `program`, its constants `constants`, mixed as `blend`.
    fn program(&mut self, program: Program, blend: u32, constants: &[f32]) {
        if self.bound_program != Some(program) {
            self.bound_program = Some(program);
            self.stream
                .bind_shader(program as u32, pipe::SHADER_FRAGMENT);
        }
        if self.bound_blend != Some(blend) {
            self.bound_blend = Some(blend);
            self.stream.bind_object(pipe::object::BLEND, blend);
        }
        self.stream.set_constants(pipe::SHADER_FRAGMENT, constants);
    }

    /// Read `view`, which is a view of `resource`, through `sampler` from
    /// here on.
    fn sample(&mut self, view: u32, resource: u32, sampler: u32) {
        // Written down even when the view is bound already: what matters is
        // that the draws waiting to be submitted read this resource, and a
        // second draw through the same view is one more of them.
        if !self.sampled.contains(&resource) {
            self.sampled.push(resource);
        }
        if self.bound_view == Some((view, sampler)) {
            return;
        }
        self.bound_view = Some((view, sampler));
        self.stream
            .set_sampler_views(pipe::SHADER_FRAGMENT, &[view]);
        self.stream
            .bind_sampler_states(pipe::SHADER_FRAGMENT, &[sampler]);
    }

    /// The parts of `rect` inside `damage` and inside `bounds`.
    fn clips(rect: Rect, damage: &Damage, bounds: Rect) -> Vec<Rect> {
        let Some(rect) = intersect(rect, bounds) else {
            return Vec::new();
        };
        damage
            .rects()
            .iter()
            .filter_map(|&part| intersect(part, rect))
            .collect()
    }

    /// Draw `quads` with whatever is bound. Each corner's texture coordinate
    /// is where it lies in `across`, from 0 to 1, so a quad that is part of
    /// a surface's rectangle reads that part of the surface.
    fn draw(&mut self, quads: &[Rect], across: Rect) {
        let (wide, tall) = (float(across.width.max(1)), float(across.height.max(1)));
        for chunk in quads.chunks(QUADS_A_DRAW) {
            let mut data = Vec::with_capacity(chunk.len() * 6 * VERTEX_STRIDE as usize);
            for quad in chunk {
                let (x0, y0) = (float(quad.x), float(quad.y));
                let (x1, y1) = (float(quad.right()), float(quad.bottom()));
                // Two triangles, which is what a list of quads is drawn as.
                for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y0), (x1, y1), (x0, y1)] {
                    let u = (x - float(across.x)) / wide;
                    let v = (y - float(across.y)) / tall;
                    data.extend([x, y, u, v].into_iter().flat_map(f32::to_le_bytes));
                }
            }
            let Ok(len) = u32::try_from(data.len()) else {
                return;
            };
            if self.vertex_at.saturating_add(len) > VERTEX_BYTES {
                self.vertex_at = 0;
            }
            let _ = self
                .stream
                .write_buffer(self.vertices, self.vertex_at, &data);
            self.stream.set_vertex_buffers(&[VertexBuffer {
                stride: VERTEX_STRIDE,
                offset: self.vertex_at,
                resource: self.vertices,
            }]);
            self.stream
                .draw(pipe::PRIM_TRIANGLES, 0, len / VERTEX_STRIDE);
            self.vertex_at = self.vertex_at.saturating_add(len);
            self.room();
        }
    }

    /// A solid colour inside a rounded rectangle, over or instead of what
    /// is there.
    fn solid(
        &mut self,
        rect: Rect,
        rounding: Rounding,
        color: [f32; 4],
        blend: u32,
        damage: &Damage,
    ) {
        if self.failed.is_some() {
            return;
        }
        let quads = Self::clips(rect, damage, Painter::bounds(self));
        if quads.is_empty() {
            return;
        }
        self.aim(self.target);
        let [radius, power] = rounding_floats(rounding);
        let [x, y, wide, tall] = rect_floats(rect);
        self.program(
            Program::Solid,
            blend,
            &[
                color[0], color[1], color[2], color[3], x, y, wide, tall, radius, power, 0.0, 0.0,
            ],
        );
        self.draw(&quads, rect);
    }

    /// Draw `image` stretched over `rect`, cut to `shape`, inside `quads`.
    #[expect(
        clippy::too_many_arguments,
        reason = "a textured draw is its texture, its place, its shape and how it is mixed"
    )]
    fn textured(
        &mut self,
        view: u32,
        resource: u32,
        sampler: u32,
        rect: Rect,
        shape: (Rect, Rounding),
        opacity: f32,
        blend: u32,
        quads: &[Rect],
    ) {
        let [radius, power] = rounding_floats(shape.1);
        let [x, y, wide, tall] = rect_floats(shape.0);
        self.sample(view, resource, sampler);
        self.program(
            Program::Surface,
            blend,
            &[
                opacity, 0.0, 0.0, 0.0, x, y, wide, tall, radius, power, 0.0, 0.0,
            ],
        );
        self.draw(quads, rect);
    }

    /// The texture `surface` is kept in, with `part` of it -- or all of it,
    /// for one not seen before -- brought up to date.
    fn surface_image(&mut self, surface: &Surface<'_>, part: Option<Rect>) -> Option<Image> {
        if let Some(image) = self.imported_image(surface) {
            return Some(image);
        }
        // Video memory this device would not import has no pixels to
        // upload instead: nothing is drawn for it, and nothing has failed.
        if !surface.has_pixels() {
            return None;
        }
        let (wide, tall) = (surface.width(), surface.height());
        // A surface with no name is known by where its pixels are, which
        // says nothing about whether they changed: it is moved whole.
        let (key, named) = match surface.name() {
            0 => ((surface.data().as_ptr() as usize as u64) | (1 << 63), false),
            name => (name & !(1 << 63), true),
        };
        let kept = self
            .surfaces
            .get(&key)
            .copied()
            .filter(|kept| (kept.image.width, kept.image.height) == (wide, tall));
        let (image, fresh) = match kept {
            Some(kept) => (kept.image, false),
            None => {
                if let Some(old) = self.surfaces.remove(&key) {
                    self.spare.push(old.image);
                }
                let spare = self
                    .spare
                    .iter()
                    .position(|image| (image.width, image.height) == (wide, tall));
                let image = match spare {
                    Some(at) => self.spare.swap_remove(at),
                    None => match self.image(wide, tall, true, false) {
                        Ok(image) => image,
                        Err(error) => {
                            self.fail(error);
                            return None;
                        }
                    },
                };
                (image, true)
            }
        };
        let _ = self.surfaces.insert(
            key,
            Kept {
                image,
                used: self.frame,
            },
        );
        let whole = Rect::new(0, 0, i64::from(wide), i64::from(tall));
        let moved = if fresh || !named {
            Some(whole)
        } else {
            part.and_then(|part| intersect(part, whole))
        };
        if let Some(moved) = moved {
            let start = unsigned(moved.y) as usize * surface.stride() as usize
                + unsigned(moved.x) as usize * 4;
            let data = surface.data().get(start..).unwrap_or(&[]);
            // The pixels must be there before the draw that reads them, and
            // a transfer is not part of the stream: anything waiting to be
            // submitted that reads this texture goes first.
            self.flush_readers_of(image.resource);
            if let Err(error) = self.device.upload(
                image.resource,
                Region {
                    x: unsigned(moved.x),
                    y: unsigned(moved.y),
                    width: unsigned(moved.width),
                    height: unsigned(moved.height),
                },
                surface.stride(),
                data,
            ) {
                self.fail(error);
                return None;
            }
        }
        Some(image)
    }

    /// The client's own GPU buffer behind `surface`, imported once and
    /// sampled where it lies, if it has one and this device can import it
    /// (`docs/GPU.md` §3.13).
    ///
    /// `None` sends the caller to the upload, which is also what a buffer
    /// that cannot be imported gets: its pixels came with it.
    fn imported_image(&mut self, surface: &Surface<'_>) -> Option<Image> {
        let on = surface.device()?;
        let (fd, key) = (on.fd, on.key);
        // The whole buffer is what is imported, whatever part of it the
        // surface is.
        let (wide, tall) = (on.width, on.height);
        if let Some(kept) = self.imports.get_mut(&key)
            && (kept.image.width, kept.image.height) == (wide, tall)
        {
            kept.used = self.frame;
            return Some(kept.image);
        }
        if self.refused.contains(&key) {
            return None;
        }
        if let Some(old) = self.imports.remove(&key) {
            let _ = self.device.release(old.image.resource);
        }
        let layout = compositor_virgl::Layout {
            width: wide,
            height: tall,
            stride: surface.stride(),
            offset: on.offset,
            modifier: on.modifier,
        };
        let resource = match self.device.import(fd, &layout) {
            Ok(Some(resource)) => resource,
            Ok(None) => return None,
            Err(_) => {
                // A key is a buffer's for its life and no other's after, so
                // the set only grows by buffers refused; it is emptied
                // rather than let grow without end.
                if self.refused.len() >= REFUSED_MOST {
                    self.refused.clear();
                }
                let _ = self.refused.insert(key);
                return None;
            }
        };
        let image = Image {
            resource,
            surface: 0,
            view: self.handle(),
            opaque_view: self.handle(),
            width: wide,
            height: tall,
        };
        for (handle, opaque) in [(image.view, false), (image.opaque_view, true)] {
            self.stream.create_sampler_view(
                handle,
                View {
                    resource,
                    format: pipe::FORMAT_B8G8R8A8_UNORM,
                    opaque,
                },
            );
        }
        let _ = self.imports.insert(
            key,
            Kept {
                image,
                used: self.frame,
            },
        );
        Some(image)
    }

    /// Hand the textures of surfaces not drawn for a while to whatever
    /// surface of their size comes next, and let go of the ones beyond what
    /// is worth keeping.
    fn retire(&mut self) {
        let frame = self.frame;
        let gone: Vec<u64> = self
            .surfaces
            .iter()
            .filter(|(_, kept)| frame.saturating_sub(kept.used) > KEPT_FRAMES)
            .map(|(key, _)| *key)
            .collect();
        for key in gone {
            if let Some(kept) = self.surfaces.remove(&key) {
                self.spare.push(kept.image);
            }
        }
        // A client's buffer is never another surface's texture: it is let
        // go of, which the client's own handle outlives.
        let stale: Vec<u64> = self
            .imports
            .iter()
            .filter(|(_, kept)| frame.saturating_sub(kept.used) > KEPT_FRAMES)
            .map(|(key, _)| *key)
            .collect();
        for key in stale {
            if let Some(kept) = self.imports.remove(&key) {
                let _ = self.device.release(kept.image.resource);
            }
        }
        // The oldest first, which are the sizes longest out of use. A
        // device that will not let go is one that keeps them, which costs
        // memory and nothing else, so the answer is dropped.
        while self.spare.len() > MAX_SPARE {
            let image = self.spare.remove(0);
            let _ = self.device.release(image.resource);
        }
    }

    /// The ramp texture for `gradient`, made the first time it is drawn.
    fn ramp(&mut self, gradient: &Gradient) -> Option<(Image, u32)> {
        if let Some((_, image)) = self.ramps.iter().find(|(held, _)| held == gradient) {
            return Some((*image, image.width));
        }
        let colors = gradient.ramp();
        let len = u32::try_from(colors.len()).ok().filter(|len| *len > 0)?;
        let image = match self.image(len, 1, true, false) {
            Ok(image) => image,
            Err(error) => {
                self.fail(error);
                return None;
            }
        };
        // Premultiplied, blue first, as every texture here is.
        let mut data = Vec::with_capacity(colors.len() * 4);
        for color in &colors {
            let alpha = u32::from(color.alpha());
            let scaled = |channel: u8| {
                u8::try_from((u32::from(channel) * alpha + 127) / 255).unwrap_or(u8::MAX)
            };
            data.extend_from_slice(&[
                scaled(color.blue()),
                scaled(color.green()),
                scaled(color.red()),
                color.alpha(),
            ]);
        }
        // A ramp is made the first time its gradient is drawn, so nothing
        // waiting reads it; the rule is the same either way.
        self.flush_readers_of(image.resource);
        if let Err(error) = self.device.upload(
            image.resource,
            Region {
                x: 0,
                y: 0,
                width: len,
                height: 1,
            },
            len.saturating_mul(4),
            &data,
        ) {
            self.fail(error);
            return None;
        }
        // A configuration has a few gradients; one that changes every frame
        // is an animation, and the oldest make way.
        if self.ramps.len() >= 16 {
            let (_, old) = self.ramps.remove(0);
            self.spare.push(old);
        }
        self.ramps.push((*gradient, image));
        Some((image, len))
    }

    /// `gradient` run across `across`, inside `rect` with its corners cut.
    fn gradient(
        &mut self,
        across: Rect,
        rect: Rect,
        rounding: Rounding,
        gradient: &Gradient,
        damage: &Damage,
    ) {
        if self.failed.is_some() {
            return;
        }
        let quads = Self::clips(rect, damage, Painter::bounds(self));
        if quads.is_empty() {
            return;
        }
        let Some((ramp, len)) = self.ramp(gradient) else {
            return;
        };
        self.aim(self.target);
        self.sample(ramp.view, ramp.resource, SAMPLER_NEAREST);
        let (sine, flip_x, flip_y) = gradient.axis().parts();
        let [radius, power] = rounding_floats(rounding);
        let [ax, ay, aw, ah] = rect_floats(across);
        let [x, y, wide, tall] = rect_floats(rect);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a ramp of a few thousand colours"
        )]
        let len = len as f32;
        self.program(
            Program::Gradient,
            BLEND_OVER,
            &[
                ax,
                ay,
                aw.max(1.0),
                ah.max(1.0),
                sine,
                f32::from(u8::from(flip_x)),
                f32::from(u8::from(flip_y)),
                0.0,
                x,
                y,
                wide,
                tall,
                radius,
                power,
                (len - 1.0) / len,
                0.5 / len,
            ],
        );
        self.draw(&quads, rect);
    }
}

impl<D: Device> Painter for Canvas<D> {
    type Backdrop = Backdrop;

    fn bounds(&self) -> Rect {
        Rect::new(0, 0, i64::from(self.width), i64::from(self.height))
    }

    fn clear(&mut self, color: Color, damage: &Damage) {
        let opaque = Color(color.0 | 0xff00_0000);
        self.solid(
            Painter::bounds(self),
            Rounding::none(),
            premultiplied(opaque),
            BLEND_REPLACE,
            damage,
        );
    }

    fn fill(&mut self, rect: Rect, color: Color, damage: &Damage) {
        self.fill_rounded(rect, Rounding::none(), color, damage);
    }

    fn fill_rounded(&mut self, rect: Rect, rounding: Rounding, color: Color, damage: &Damage) {
        if color.alpha() == 0 {
            return;
        }
        self.solid(rect, rounding, premultiplied(color), BLEND_OVER, damage);
    }

    fn fill_rounded_gradient(
        &mut self,
        rect: Rect,
        rounding: Rounding,
        gradient: &Gradient,
        damage: &Damage,
    ) {
        if gradient.is_solid() {
            self.fill_rounded(rect, rounding, gradient.first(), damage);
        } else {
            self.gradient(rect, rect, rounding, gradient, damage);
        }
    }

    fn border_gradient(&mut self, rect: Rect, width: i64, gradient: &Gradient, damage: &Damage) {
        if width <= 0 || rect.width <= 0 || rect.height <= 0 {
            return;
        }
        let outer = Rect::new(
            rect.x.saturating_sub(width),
            rect.y.saturating_sub(width),
            rect.width.saturating_add(width.saturating_mul(2)),
            rect.height.saturating_add(width.saturating_mul(2)),
        );
        // Four strips, as the software border is, so that a translucent
        // border blends once at its corners.
        let strips = [
            Rect::new(outer.x, outer.y, outer.width, width),
            Rect::new(outer.x, rect.bottom(), outer.width, width),
            Rect::new(outer.x, rect.y, width, rect.height),
            Rect::new(rect.right(), rect.y, width, rect.height),
        ];
        for strip in strips {
            if gradient.is_solid() {
                self.fill(strip, gradient.first(), damage);
            } else {
                self.gradient(outer, strip, Rounding::none(), gradient, damage);
            }
        }
    }

    fn shadow(&mut self, rect: Rect, shadow: &Shadow, damage: &Damage) {
        if self.failed.is_some()
            || shadow.range <= 0
            || shadow.color.alpha() == 0
            || rect.width <= 0
            || rect.height <= 0
        {
            return;
        }
        let full = Rect::new(
            rect.x
                .saturating_sub(shadow.range)
                .saturating_add(shadow.offset.0),
            rect.y
                .saturating_sub(shadow.range)
                .saturating_add(shadow.offset.1),
            rect.width.saturating_add(shadow.range.saturating_mul(2)),
            rect.height.saturating_add(shadow.range.saturating_mul(2)),
        );
        let quads = Self::clips(full, damage, Painter::bounds(self));
        if quads.is_empty() {
            return;
        }
        self.aim(self.target);
        let color = premultiplied(shadow.color);
        let [x, y, wide, tall] = rect_floats(full);
        let inset = float(shadow.range.saturating_add(shadow.rounding.radius.max(0)));
        #[expect(clippy::cast_precision_loss, reason = "a power between one and four")]
        let power = shadow.power.clamp(1, 4) as f32;
        self.program(
            Program::Shadow,
            BLEND_OVER,
            &[
                color[0],
                color[1],
                color[2],
                color[3],
                x,
                y,
                wide,
                tall,
                inset,
                float(shadow.range),
                power,
                0.0,
            ],
        );
        self.draw(&quads, full);
    }

    fn blur(&mut self, rect: Rect, rounding: Rounding, blur: &Blur, damage: &Damage) {
        self.blur_behind(None, rect, rounding, blur, damage);
    }

    fn composite(&mut self, surface: &Surface<'_>, rect: Rect, damage: &Damage) {
        // Pixel for pixel from the rectangle's corner, whatever the
        // rectangle's size: what `Canvas::composite` draws.
        let exact = Rect::new(
            rect.x,
            rect.y,
            i64::from(surface.width()).min(rect.width),
            i64::from(surface.height()).min(rect.height),
        );
        let whole = Rect::new(
            rect.x,
            rect.y,
            i64::from(surface.width()),
            i64::from(surface.height()),
        );
        self.surface(surface, whole, (exact, Rounding::none()), 1.0, true, damage);
    }

    fn composite_scaled(
        &mut self,
        surface: &Surface<'_>,
        rect: Rect,
        rounding: Rounding,
        opacity: f32,
        nearest: bool,
        damage: &Damage,
    ) {
        let same =
            rect.width == i64::from(surface.width()) && rect.height == i64::from(surface.height());
        self.surface(
            surface,
            rect,
            (rect, rounding),
            opacity,
            nearest || same,
            damage,
        );
    }

    fn backdrop_fits(&self, backdrop: &Backdrop) -> bool {
        backdrop.fits(self.width, self.height)
    }

    fn keep_backdrop(&mut self, backdrop: &mut Backdrop, damage: &Damage) {
        self.keep(backdrop, damage);
    }

    fn blur_backdrop(
        &mut self,
        backdrop: &mut Backdrop,
        rect: Rect,
        rounding: Rounding,
        blur: &Blur,
        damage: &Damage,
    ) {
        self.blur_behind(Some(backdrop), rect, rounding, blur, damage);
    }
}

/// Where the whole of `image` lies when `surface`, a part of it, lies over
/// `across`: a texture coordinate is a place in what is drawn across, so a
/// part of an imported buffer is drawn by laying the whole buffer where its
/// part comes out over the part's rectangle. `across` itself for a surface
/// that is all of its image, which every uploaded one is.
fn spread(across: Rect, surface: &Surface<'_>, image: Image) -> Rect {
    let Some(on) = surface.device() else {
        return across;
    };
    let part = (surface.width(), surface.height());
    if (image.width, image.height) != (on.width, on.height)
        || ((on.x, on.y) == (0, 0) && part == (on.width, on.height))
        || part.0 == 0
        || part.1 == 0
    {
        return across;
    }
    // `pixels` of the buffer in screen pixels, at the stretch the part is
    // drawn with, to the nearest.
    let scaled = |pixels: u32, drawn: i64, of: u32| -> i64 {
        let of = i64::from(of);
        (i64::from(pixels)
            .saturating_mul(drawn)
            .saturating_add(of / 2))
            / of
    };
    Rect::new(
        across.x.saturating_sub(scaled(on.x, across.width, part.0)),
        across.y.saturating_sub(scaled(on.y, across.height, part.1)),
        scaled(on.width, across.width, part.0),
        scaled(on.height, across.height, part.1),
    )
}

impl<D: Device> Canvas<D> {
    /// Whether this canvas's device takes `surface`'s GPU buffer
    /// ([`Surface::on_device`]) to sample where it lies: asked when a
    /// client hands a buffer over whose pixels cannot be read here, which
    /// is then either shown this way or not at all. The import is kept for
    /// the frames that follow.
    pub fn imports(&mut self, surface: &Surface<'_>) -> bool {
        let imported = self.imported_image(surface).is_some();
        // Its views are written outside any frame: submitted if many
        // buffers arrive before the next one.
        self.room();
        imported
    }

    /// Blend `surface`, stretched over `across`, inside `shape`.
    fn surface(
        &mut self,
        surface: &Surface<'_>,
        across: Rect,
        shape: (Rect, Rounding),
        opacity: f32,
        nearest: bool,
        damage: &Damage,
    ) {
        let opacity = opacity.clamp(0.0, 1.0);
        if self.failed.is_some()
            || opacity == 0.0
            || surface.width() == 0
            || surface.height() == 0
            || across.width <= 0
            || across.height <= 0
        {
            return;
        }
        let quads = Self::clips(shape.0, damage, Painter::bounds(self));
        if quads.is_empty() {
            return;
        }
        // The part of the surface under what is drawn, when it is drawn
        // pixel for pixel; a stretched one is an animation, and is moved
        // whole for the frames it lasts.
        let same = across.width == i64::from(surface.width())
            && across.height == i64::from(surface.height());
        let part = if same {
            crate::damage::bounding(&quads).map(|bounds| {
                bounds.translate(across.x.saturating_neg(), across.y.saturating_neg())
            })
        } else {
            Some(Rect::new(
                0,
                0,
                i64::from(surface.width()),
                i64::from(surface.height()),
            ))
        };
        let Some(image) = self.surface_image(surface, part) else {
            return;
        };
        let across = spread(across, surface, image);
        self.aim(self.target);
        let view = if surface.format() == Format::Xrgb8888 {
            image.opaque_view
        } else {
            image.view
        };
        let sampler = if nearest {
            SAMPLER_NEAREST
        } else {
            SAMPLER_LINEAR
        };
        self.textured(
            view,
            image.resource,
            sampler,
            across,
            shape,
            opacity,
            BLEND_OVER,
            &quads,
        );
    }
}
