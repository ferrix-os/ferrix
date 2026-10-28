//! The [`Backend`] that writes virgl streams: a render node's in a guest,
//! virglrenderer's test server on a host.
//!
//! Everything the frame's shapes are made of -- the quads, the constants,
//! which texture is read and which is drawn into -- is [`super::Renderer`]'s.
//! What is here is how those become words: the state objects a context
//! numbers, the sampler views, the vertex buffer the quads are written into,
//! and when a stream is submitted.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};

use compositor_virgl::{
    Blend as VirglBlend, Device, Layout, Rasterizer, Region, Sampler, Stream, Texture,
    VertexBuffer, VertexElement, View, pipe, shaders,
};

use super::{Backend, Blend, Filter, Program, Use};

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

/// The handle a fragment shader is made under.
const fn shader_handle(program: Program) -> u32 {
    match program {
        Program::Solid => 10,
        Program::Surface => 11,
        Program::Gradient => 12,
        Program::Shadow => 13,
        Program::BlurPrepare => 14,
        Program::BlurDown => 15,
        Program::BlurUp => 16,
        Program::BlurFinish => 17,
    }
}

/// The TGSI text of a fragment shader.
const fn shader_text(program: Program) -> &'static str {
    match program {
        Program::Solid => shaders::SOLID,
        Program::Surface => shaders::SURFACE,
        Program::Gradient => shaders::GRADIENT,
        Program::Shadow => shaders::SHADOW,
        Program::BlurPrepare => shaders::BLUR_PREPARE,
        Program::BlurDown => shaders::BLUR_DOWN,
        Program::BlurUp => shaders::BLUR_UP,
        Program::BlurFinish => shaders::BLUR_FINISH,
    }
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

/// A texture's handles in the stream: the surface it is drawn into through
/// (0 for one never drawn into), and its two sampler views.
#[derive(Clone, Copy, Debug)]
struct Views {
    surface: u32,
    /// Read alpha and all.
    view: u32,
    /// Read with alpha as one.
    opaque_view: u32,
}

/// A virgl context on `D`, as a [`Backend`].
#[derive(Debug)]
pub struct Virgl<D: Device> {
    device: D,
    stream: Stream,
    vertices: u32,
    vertex_at: u32,
    next_handle: u32,
    /// The views of every texture made or imported, by resource.
    views: BTreeMap<u32, Views>,
    /// What is bound, so that nothing is bound twice.
    bound_target: Option<u32>,
    bound_program: Option<Program>,
    bound_blend: Option<Blend>,
    bound_view: Option<(u32, Filter)>,
    /// The resources the draws written since the last submission read from.
    ///
    /// Moving pixels into a texture is not part of the command stream: it
    /// happens when it is asked for, while what is in the stream happens
    /// when the stream is submitted. So a draw that is written but not yet
    /// submitted would read pixels that arrived after it -- but only if it
    /// reads *that* texture, and this is how that is known.
    sampled: Vec<u32>,
    /// The first submission the device refused, until [`Backend::flush`]
    /// says so.
    failed: Option<io::Error>,
}

impl<D: Device> Virgl<D> {
    /// A context on `device`, with the state every draw shares made.
    ///
    /// # Errors
    ///
    /// The device's, for the vertex buffer.
    pub fn new(mut device: D) -> io::Result<Self> {
        let vertices = device.buffer(VERTEX_BYTES)?;
        let mut virgl = Self {
            device,
            stream: Stream::new(),
            vertices,
            vertex_at: 0,
            next_handle: FIRST_MADE,
            views: BTreeMap::new(),
            bound_target: None,
            bound_program: None,
            bound_blend: None,
            bound_view: None,
            sampled: Vec::new(),
            failed: None,
        };
        virgl.begin();
        Ok(virgl)
    }

    /// The device.
    pub const fn device(&mut self) -> &mut D {
        &mut self.device
    }

    /// The state every draw shares, made once.
    fn begin(&mut self) {
        let stream = &mut self.stream;
        stream.create_blend(BLEND_OVER, VirglBlend::PREMULTIPLIED_OVER);
        stream.create_blend(BLEND_REPLACE, VirglBlend::REPLACE);
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
        for program in Program::ALL {
            let _ = self.stream.create_shader(
                shader_handle(program),
                pipe::SHADER_FRAGMENT,
                shader_text(program),
                shaders::TOKENS,
            );
            self.submit();
        }
    }

    /// Remember the first failure.
    fn fail(&mut self, error: io::Error) {
        if self.failed.is_none() {
            self.failed = Some(error);
        }
    }

    /// Submit the stream so far; a failure is kept for [`Backend::flush`].
    fn submit(&mut self) {
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
    /// The ordering [`Virgl::sampled`] describes. In a frame as this
    /// compositor draws one each surface is brought up to date and then
    /// drawn, so the texture being written is one nothing waiting reads and
    /// this costs nothing -- which is the point, since a submission is a
    /// round trip through the driver and the device.
    fn flush_readers_of(&mut self, resource: u32) {
        if self.sampled.contains(&resource) {
            self.submit();
        }
    }

    /// Submit if the stream is getting full.
    fn room(&mut self) {
        if self.stream.len_bytes() > FLUSH_BYTES {
            self.submit();
        }
    }

    /// A new state object's handle.
    fn handle(&mut self) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(FIRST_MADE);
        handle
    }

    /// The two sampler views of `resource`, made.
    fn sampler_views(&mut self, resource: u32) -> (u32, u32) {
        let (view, opaque_view) = (self.handle(), self.handle());
        for (handle, opaque) in [(view, false), (opaque_view, true)] {
            self.stream.create_sampler_view(
                handle,
                View {
                    resource,
                    format: pipe::FORMAT_B8G8R8A8_UNORM,
                    opaque,
                },
            );
        }
        (view, opaque_view)
    }
}

impl<D: Device> Backend for Virgl<D> {
    fn image(&mut self, width: u32, height: u32, usage: Use) -> io::Result<u32> {
        let bind = if usage.drawn {
            pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW
        } else {
            pipe::BIND_SAMPLER_VIEW
        };
        let resource = self.device.texture(Texture {
            width,
            height,
            format: pipe::FORMAT_B8G8R8A8_UNORM,
            bind,
            moved: usage.moved,
            scanout: usage.scanout,
        })?;
        let surface = if usage.drawn { self.handle() } else { 0 };
        if usage.drawn {
            self.stream
                .create_surface(surface, resource, pipe::FORMAT_B8G8R8A8_UNORM);
        }
        let (view, opaque_view) = self.sampler_views(resource);
        let _ = self.views.insert(
            resource,
            Views {
                surface,
                view,
                opaque_view,
            },
        );
        Ok(resource)
    }

    fn import(&mut self, fd: BorrowedFd<'_>, layout: &Layout) -> io::Result<Option<u32>> {
        let Some(resource) = self.device.import(fd, layout)? else {
            return Ok(None);
        };
        let (view, opaque_view) = self.sampler_views(resource);
        let _ = self.views.insert(
            resource,
            Views {
                surface: 0,
                view,
                opaque_view,
            },
        );
        // Its views are written outside any frame: submitted if many
        // buffers arrive before the next one.
        self.room();
        Ok(Some(resource))
    }

    fn upload(&mut self, image: u32, region: Region, stride: u32, data: &[u8]) -> io::Result<()> {
        // The pixels must be there before the draw that reads them, and a
        // transfer is not part of the stream: anything waiting to be
        // submitted that reads this texture goes first.
        self.flush_readers_of(image);
        self.device.upload(image, region, stride, data)
    }

    fn release(&mut self, image: u32) -> io::Result<()> {
        let _ = self.views.remove(&image);
        self.device.release(image)
    }

    fn aim(&mut self, image: u32, width: u32, height: u32) {
        if self.bound_target == Some(image) {
            return;
        }
        let Some(views) = self.views.get(&image).copied() else {
            return;
        };
        self.bound_target = Some(image);
        self.stream.set_framebuffer(views.surface);
        self.stream.set_viewport(width, height);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a texture's size, at most 16384, exact in f32"
        )]
        let (wide, tall) = (width as f32, height as f32);
        self.stream
            .set_constants(pipe::SHADER_VERTEX, &[2.0 / wide, 2.0 / tall, -1.0, -1.0]);
    }

    fn program(&mut self, program: Program, blend: Blend, constants: &[f32]) {
        if self.bound_program != Some(program) {
            self.bound_program = Some(program);
            self.stream
                .bind_shader(shader_handle(program), pipe::SHADER_FRAGMENT);
        }
        if self.bound_blend != Some(blend) {
            self.bound_blend = Some(blend);
            let object = match blend {
                Blend::Over => BLEND_OVER,
                Blend::Replace => BLEND_REPLACE,
            };
            self.stream.bind_object(pipe::object::BLEND, object);
        }
        self.stream.set_constants(pipe::SHADER_FRAGMENT, constants);
    }

    fn sample(&mut self, image: u32, opaque: bool, filter: Filter) {
        // Written down even when the view is bound already: what matters is
        // that the draws waiting to be submitted read this resource, and a
        // second draw through the same view is one more of them.
        if !self.sampled.contains(&image) {
            self.sampled.push(image);
        }
        let Some(views) = self.views.get(&image).copied() else {
            return;
        };
        let view = if opaque {
            views.opaque_view
        } else {
            views.view
        };
        if self.bound_view == Some((view, filter)) {
            return;
        }
        self.bound_view = Some((view, filter));
        let sampler = match filter {
            Filter::Nearest => SAMPLER_NEAREST,
            Filter::Linear => SAMPLER_LINEAR,
        };
        self.stream
            .set_sampler_views(pipe::SHADER_FRAGMENT, &[view]);
        self.stream
            .bind_sampler_states(pipe::SHADER_FRAGMENT, &[sampler]);
    }

    fn draw(&mut self, vertices: &[[f32; 4]]) {
        for chunk in vertices.chunks(QUADS_A_DRAW * 6) {
            let data: Vec<u8> = chunk
                .iter()
                .flatten()
                .flat_map(|value| value.to_le_bytes())
                .collect();
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

    fn flush(&mut self) -> io::Result<()> {
        self.submit();
        self.failed.take().map_or(Ok(()), Err)
    }

    fn read(&mut self, image: u32, region: Region) -> io::Result<Vec<u8>> {
        self.flush()?;
        self.device.read(image, region)
    }

    fn read_into(
        &mut self,
        image: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        self.flush()?;
        self.device.read_into(image, region, into, stride)
    }

    fn export(&mut self, image: u32) -> io::Result<Option<OwnedFd>> {
        self.device.export(image)
    }
}
