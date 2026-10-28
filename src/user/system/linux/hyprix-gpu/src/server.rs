//! hyprix's side of the conversation, answered: canvases and their output
//! buffers, client textures, the arena, and frames drawn op by op
//! (`docs/GPU-COMPOSITOR.md` §4).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::rc::Rc;

use compositor_gpu_protocol::{
    self as protocol, ARGB8888, Message, Op, VERSION, XRGB8888, failure,
};
use compositor_render::gpu::{
    Backdrop, Backend, Blend, Filter, Image, Layout, Program, Region, Renderer, Use,
};
use compositor_render::{Blur, Color, Damage, Gradient, Painter, Rect, Rounding, Shadow};

use crate::vulkan::Vulkan;
use crate::wire::Wire;

/// The one device every canvas draws through.
#[derive(Debug, Clone)]
pub struct Shared(pub Rc<RefCell<Vulkan>>);

impl Backend for Shared {
    fn image(&mut self, width: u32, height: u32, usage: Use) -> io::Result<u32> {
        self.0.borrow_mut().image(width, height, usage)
    }
    fn import(&mut self, fd: BorrowedFd<'_>, layout: &Layout) -> io::Result<Option<u32>> {
        self.0.borrow_mut().import(fd, layout)
    }
    fn upload(&mut self, image: u32, region: Region, stride: u32, data: &[u8]) -> io::Result<()> {
        self.0.borrow_mut().upload(image, region, stride, data)
    }
    fn release(&mut self, image: u32) -> io::Result<()> {
        self.0.borrow_mut().release(image)
    }
    fn aim(&mut self, image: u32, width: u32, height: u32) {
        self.0.borrow_mut().aim(image, width, height);
    }
    fn program(&mut self, program: Program, blend: Blend, constants: &[f32]) {
        self.0.borrow_mut().program(program, blend, constants);
    }
    fn sample(&mut self, image: u32, opaque: bool, filter: Filter) {
        self.0.borrow_mut().sample(image, opaque, filter);
    }
    fn draw(&mut self, vertices: &[[f32; 4]]) {
        self.0.borrow_mut().draw(vertices);
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush()
    }
    fn read(&mut self, image: u32, region: Region) -> io::Result<Vec<u8>> {
        self.0.borrow_mut().read(image, region)
    }
    fn read_into(
        &mut self,
        image: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        self.0.borrow_mut().read_into(image, region, into, stride)
    }
    fn export(&mut self, image: u32) -> io::Result<Option<OwnedFd>> {
        self.0.borrow_mut().export(image)
    }
}

/// An output buffer: what the card is shown.
#[derive(Debug)]
struct Output {
    /// The image's number.
    id: u32,
    /// Bytes from one row to the next.
    stride: u32,
    /// Whether hyprix gave it back (the card is done reading it).
    released: bool,
    /// What changed on the screen since it was last written.
    missed: Damage,
}

/// A screen's frame, its backdrop and its outputs.
#[derive(Debug)]
struct Canvas {
    renderer: Renderer<Shared>,
    backdrop: Backdrop,
    outputs: Vec<Output>,
}

/// A texture hyprix named.
#[derive(Debug, Clone, Copy)]
struct Texture {
    image: Image,
    /// An XRGB8888 buffer's: its alpha is read as one.
    opaque: bool,
}

/// A memfd hyprix writes pixels into, mapped.
#[derive(Debug)]
struct Arena {
    ptr: *mut u8,
    len: usize,
    _fd: OwnedFd,
}

impl Arena {
    /// `fd`, `len` bytes, mapped shared for reading and writing.
    fn map(fd: OwnedFd, len: u64) -> io::Result<Self> {
        let len = usize::try_from(len).map_err(|_| io::Error::other("an arena too large"))?;
        if len == 0 {
            return Err(io::Error::other("an empty arena"));
        }
        // SAFETY: a fresh shared mapping of the descriptor; nothing else in
        // this process points there.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
            _fd: fd,
        })
    }

    /// Its bytes from `offset` on.
    fn bytes_from(&self, offset: u64) -> Option<&[u8]> {
        let offset = usize::try_from(offset).ok()?;
        if offset > self.len {
            return None;
        }
        // SAFETY: `offset` is inside the mapping.
        let start = unsafe { self.ptr.add(offset) };
        // SAFETY: mapped for `len` bytes; hyprix writes it only between
        // frames, while nothing here reads it.
        Some(unsafe { std::slice::from_raw_parts(start, self.len - offset) })
    }

    /// Its bytes from `offset` on, to write.
    fn bytes_from_mut(&mut self, offset: u64) -> Option<&mut [u8]> {
        let offset = usize::try_from(offset).ok()?;
        if offset > self.len {
            return None;
        }
        // SAFETY: `offset` is inside the mapping.
        let start = unsafe { self.ptr.add(offset) };
        // SAFETY: as above; hyprix reads the part after `ReadDone`.
        Some(unsafe { std::slice::from_raw_parts_mut(start, self.len - offset) })
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `map`, unmapped once.
        let _ = unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// The frame being drawn.
#[derive(Debug)]
struct Frame {
    canvas: u32,
    output: u32,
    damage: Damage,
    /// The first op that could not be drawn, and its code.
    bad: Option<(u32, String)>,
}

/// Why the conversation ends.
#[derive(Debug)]
pub enum End {
    /// hyprix closed its end.
    Closed,
    /// The device was lost.
    Lost(String),
    /// The socket failed.
    Socket(io::Error),
}

/// The helper's state.
#[derive(Debug)]
pub struct Server {
    device: Shared,
    wire: Wire,
    canvases: HashMap<u32, Canvas>,
    textures: HashMap<u64, Texture>,
    arenas: HashMap<u32, Arena>,
    frame: Option<Frame>,
    /// The driver's name, for `Ready`.
    driver: String,
    /// Whether a finished output's lines are flushed from the processor's
    /// cache (`HYPRIX_GPU_OUTPUT_FLUSH=1`).
    flush_outputs: bool,
}

/// A protocol rectangle as the renderer's.
fn rect(r: protocol::Rect) -> Rect {
    Rect::new(
        i64::from(r.x),
        i64::from(r.y),
        i64::from(r.width),
        i64::from(r.height),
    )
}

/// A protocol rounding as the renderer's.
fn rounding(r: protocol::Rounding) -> Rounding {
    Rounding {
        radius: i64::from(r.radius),
        power: r.power,
    }
}

/// A protocol gradient as the renderer's.
fn gradient(g: &protocol::Gradient) -> Gradient {
    let colors: Vec<Color> = g.colors.iter().map(|c| Color(*c)).collect();
    Gradient::new(&colors, g.degrees)
}

/// A protocol blur as the renderer's.
const fn blur(b: protocol::Blur) -> Blur {
    Blur {
        size: b.size as i64,
        passes: b.passes,
        noise: b.noise,
        contrast: b.contrast,
        brightness: b.brightness,
        vibrancy: b.vibrancy,
        vibrancy_darkness: b.vibrancy_darkness,
    }
}

/// `rect`, inside a texture, as a region of it.
fn region(r: protocol::Rect) -> Region {
    Region {
        x: u32::try_from(r.x.max(0)).unwrap_or(0),
        y: u32::try_from(r.y.max(0)).unwrap_or(0),
        width: r.width,
        height: r.height,
    }
}

/// `pixels` of a texture in screen pixels, drawn `drawn` wide for `of` of
/// them, to the nearest: how a crop's whole texture is laid around it.
fn scaled(pixels: i64, drawn: i64, of: i64) -> i64 {
    if of <= 0 {
        return pixels;
    }
    pixels.saturating_mul(drawn).saturating_add(of / 2) / of
}

impl Server {
    /// A server on `device`, speaking over `wire`; `driver` is what
    /// `Ready` says draws.
    #[must_use]
    pub fn new(device: Vulkan, wire: Wire, driver: String) -> Self {
        Self {
            device: Shared(Rc::new(RefCell::new(device))),
            wire,
            canvases: HashMap::new(),
            textures: HashMap::new(),
            arenas: HashMap::new(),
            frame: None,
            driver,
            flush_outputs: std::env::var_os("HYPRIX_GPU_OUTPUT_FLUSH").is_some_and(|v| v == "1"),
        }
    }

    /// Answer records until hyprix closes its end or the device is lost.
    pub fn run(&mut self) -> End {
        loop {
            let (record, fds) = match self.wire.receive() {
                Ok(Some(got)) => got,
                Ok(None) => return End::Closed,
                Err(error) => return End::Socket(error),
            };
            if let Err(end) = self.answer(&record, fds) {
                return end;
            }
        }
    }

    /// Send `message` with `fds`.
    fn send(&self, message: &Message, fds: &[BorrowedFd<'_>]) -> Result<(), End> {
        let record = message
            .encode(0)
            .map_err(|error| End::Socket(io::Error::other(error.to_string())))?;
        self.wire.send(&record, fds).map_err(End::Socket)
    }

    /// Answer one record.
    fn answer(&mut self, record: &[u8], fds: Vec<OwnedFd>) -> Result<(), End> {
        let (header, message) = match Message::decode(record) {
            Ok(decoded) => decoded,
            Err(error) => {
                return Err(End::Socket(io::Error::other(format!(
                    "a record that does not decode: {error}"
                ))));
            }
        };
        if fds.len() != message.fds() {
            return Err(End::Socket(io::Error::other(
                "a record with the wrong count of descriptors",
            )));
        }
        let mut fds = fds.into_iter();
        match message {
            Message::Hello { .. } => self.hello(),
            Message::Configure {
                canvas,
                width,
                height,
                outputs,
            } => self.configure(canvas, (width, height), outputs),
            Message::Drop { canvas } => {
                self.drop_canvas(canvas);
                Ok(())
            }
            Message::Arena { arena, size } => {
                if let Some(fd) = fds.next() {
                    match Arena::map(fd, size) {
                        Ok(mapped) => {
                            let _ = self.arenas.insert(arena, mapped);
                        }
                        Err(error) => warn(&format!("arena {arena} not mapped: {error}")),
                    }
                }
                Ok(())
            }
            Message::ImportDmabuf {
                texture,
                width,
                height,
                fourcc,
                modifier,
                planes,
            } => {
                if let (Some(fd), Some(plane)) = (fds.next(), planes.first()) {
                    self.import(
                        texture,
                        fd.as_fd(),
                        (width, height),
                        fourcc,
                        modifier,
                        *plane,
                    );
                }
                Ok(())
            }
            Message::CreateTexture {
                texture,
                width,
                height,
                fourcc,
            } => {
                self.forget(texture);
                let made = self.device.image(
                    width,
                    height,
                    Use {
                        moved: true,
                        drawn: false,
                        scanout: false,
                    },
                );
                match made {
                    Ok(id) => {
                        let _ = self.textures.insert(
                            texture,
                            Texture {
                                image: Image { id, width, height },
                                opaque: fourcc == XRGB8888,
                            },
                        );
                    }
                    Err(error) => warn(&format!("texture {texture} not made: {error}")),
                }
                Ok(())
            }
            Message::Forget { texture } => {
                self.forget(texture);
                Ok(())
            }
            Message::FrameBegin {
                canvas,
                frame,
                output,
            } => {
                let damage = self
                    .canvases
                    .get(&canvas)
                    .map(|c| Damage::full(c.renderer.width(), c.renderer.height()))
                    .unwrap_or_default();
                let _ = frame;
                self.frame = Some(Frame {
                    canvas,
                    output,
                    damage,
                    bad: None,
                });
                Ok(())
            }
            Message::Ops(body) => {
                match Op::decode_all(&body) {
                    Ok(ops) => {
                        for op in ops {
                            self.op(&op);
                        }
                    }
                    Err(error) => self.bad(failure::BAD_OP, format!("ops: {error}")),
                }
                Ok(())
            }
            Message::FrameEnd { frame, damage } => self.end_frame(frame, &damage),
            Message::Release { canvas, output } => {
                if let Some(out) = self
                    .canvases
                    .get_mut(&canvas)
                    .and_then(|c| c.outputs.get_mut(output as usize))
                {
                    out.released = true;
                }
                Ok(())
            }
            Message::Read {
                canvas,
                rect: r,
                arena,
                offset,
                stride,
            } => {
                self.read(canvas, r, arena, offset, stride);
                self.send(
                    &Message::ReadDone {
                        canvas,
                        serial: header.serial,
                    },
                    &[],
                )
            }
            // What only the helper sends.
            Message::Ready { .. }
            | Message::Modifiers { .. }
            | Message::Outputs { .. }
            | Message::Done { .. }
            | Message::Failed { .. }
            | Message::ReadDone { .. } => Err(End::Socket(io::Error::other(
                "hyprix sent a record only the helper sends",
            ))),
        }
    }

    /// `Ready`, and the modifiers clients' dmabufs are sampled with.
    fn hello(&self) -> Result<(), End> {
        let device = self.device.0.borrow().core().name().to_owned();
        self.send(
            &Message::Ready {
                version: VERSION,
                caps: 0,
                device,
                driver: self.driver.clone(),
            },
            &[],
        )?;
        let modifiers = self.device.0.borrow().sampled_modifiers();
        for fourcc in [ARGB8888, XRGB8888] {
            self.send(
                &Message::Modifiers {
                    fourcc,
                    modifiers: modifiers.clone(),
                },
                &[],
            )?;
        }
        Ok(())
    }

    /// Make canvas `canvas` (again) at `size`, with `count` outputs.
    fn configure(&mut self, canvas: u32, size: (u32, u32), count: u32) -> Result<(), End> {
        self.drop_canvas(canvas);
        let made = Renderer::with_backend(self.device.clone(), size.0, size.1);
        let renderer = match made {
            Ok(renderer) => renderer,
            Err(error) => {
                return self.send(
                    &Message::Failed {
                        canvas,
                        frame: 0,
                        code: failure::OUT_OF_MEMORY,
                        why: format!("the canvas: {error}"),
                    },
                    &[],
                );
            }
        };
        let mut outputs = Vec::new();
        let mut exported = Vec::new();
        for _ in 0..count.clamp(1, 3) {
            let made = self.device.0.borrow_mut().any_output(size);
            match made {
                Ok(output) => exported.push(output),
                Err(error) => {
                    warn(&format!("canvas {canvas}: no output buffer: {error}"));
                    break;
                }
            }
        }
        let Some(first) = exported.first() else {
            let _ = renderer.dispose(None);
            return self.send(
                &Message::Failed {
                    canvas,
                    frame: 0,
                    code: failure::OUT_OF_MEMORY,
                    why: "no output buffer".to_owned(),
                },
                &[],
            );
        };
        let modifier = first.modifier;
        warn(&format!(
            "canvas {canvas}: {} outputs of {}x{}, modifier {modifier:#x}, stride {}, memory {:?}",
            exported.len(),
            size.0,
            size.1,
            first.stride,
            first.memory
        ));
        let planes: Vec<protocol::Plane> = exported
            .iter()
            .map(|e| protocol::Plane {
                stride: e.stride,
                offset: e.offset,
            })
            .collect();
        for each in &exported {
            outputs.push(Output {
                id: each.id,
                stride: each.stride,
                released: true,
                missed: Damage::full(size.0, size.1),
            });
        }
        let fds: Vec<BorrowedFd<'_>> = exported.iter().map(|e| e.fd.as_fd()).collect();
        let sent = self.send(
            &Message::Outputs {
                canvas,
                width: size.0,
                height: size.1,
                fourcc: XRGB8888,
                modifier,
                planes,
            },
            &fds,
        );
        let _ = self.canvases.insert(
            canvas,
            Canvas {
                renderer,
                backdrop: Backdrop::new(size.0, size.1),
                outputs,
            },
        );
        sent
    }

    /// Let canvas `canvas` go, and everything of it.
    fn drop_canvas(&mut self, canvas: u32) {
        if let Some(mut gone) = self.canvases.remove(&canvas) {
            let mut device = gone.renderer.dispose(Some(&mut gone.backdrop));
            for output in gone.outputs {
                let _ = device.release(output.id);
            }
        }
    }

    /// Import a client's dmabuf as `texture`.
    fn import(
        &mut self,
        texture: u64,
        fd: BorrowedFd<'_>,
        (width, height): (u32, u32),
        fourcc: u32,
        modifier: u64,
        plane: protocol::Plane,
    ) {
        self.forget(texture);
        let layout = Layout {
            width,
            height,
            stride: plane.stride,
            offset: plane.offset,
            modifier,
        };
        match self.device.import(fd, &layout) {
            Ok(Some(id)) => {
                let _ = self.textures.insert(
                    texture,
                    Texture {
                        image: Image { id, width, height },
                        opaque: fourcc == XRGB8888,
                    },
                );
            }
            Ok(None) => warn(&format!("dmabuf {texture}: the device imports none")),
            Err(error) => warn(&format!("dmabuf {texture} not imported: {error}")),
        }
    }

    /// Let texture `texture` go.
    fn forget(&mut self, texture: u64) {
        if let Some(gone) = self.textures.remove(&texture) {
            let _ = self.device.release(gone.image.id);
        }
    }

    /// The frame's first failure.
    fn bad(&mut self, code: u32, why: String) {
        if let Some(frame) = self.frame.as_mut()
            && frame.bad.is_none()
        {
            frame.bad = Some((code, why));
        }
    }

    /// Draw `op` into the frame.
    fn op(&mut self, op: &Op) {
        let Some(frame) = self.frame.as_mut() else {
            return;
        };
        if frame.bad.is_some() {
            return;
        }
        if let Op::Damage(rects) = op {
            let mut damage = Damage::new();
            for r in rects {
                damage.add(rect(*r));
            }
            frame.damage = damage;
            return;
        }
        let damage = frame.damage.clone();
        if let Op::Upload {
            texture,
            rect: r,
            arena,
            offset,
            stride,
        } = op
        {
            let why = self.upload(*texture, *r, *arena, *offset, *stride).err();
            if let Some(why) = why {
                self.bad(failure::BAD_OP, why);
            }
            return;
        }
        let canvas_id = frame.canvas;
        // A texture the frame samples that is not there -- never made, or
        // an import the device refused -- fails the frame, so that hyprix
        // draws it in software rather than show nothing where it is.
        if let Op::Composite { texture, .. } | Op::CompositeScaled { texture, .. } = op
            && !self.textures.contains_key(texture)
        {
            self.bad(failure::BAD_OP, format!("texture {texture} is not there"));
            return;
        }
        let textures = &self.textures;
        let Some(canvas) = self.canvases.get_mut(&canvas_id) else {
            self.bad(failure::BAD_OP, format!("no canvas {canvas_id}"));
            return;
        };
        let r = &mut canvas.renderer;
        match op {
            Op::Damage(_) | Op::Upload { .. } => {}
            Op::Clear(c) => r.clear(Color(*c), &damage),
            Op::Fill(at, c) => r.fill(rect(*at), Color(*c), &damage),
            Op::FillRounded(at, round, c) => {
                r.fill_rounded(rect(*at), rounding(*round), Color(*c), &damage);
            }
            Op::FillRoundedGradient(at, round, g) => {
                r.fill_rounded_gradient(rect(*at), rounding(*round), &gradient(g), &damage);
            }
            Op::BorderGradient(at, width, g) => {
                r.border_gradient(rect(*at), i64::from(*width), &gradient(g), &damage);
            }
            Op::Shadow(at, s) => r.shadow(
                rect(*at),
                &Shadow {
                    rounding: rounding(s.rounding),
                    range: i64::from(s.range),
                    power: s.power,
                    color: Color(s.color),
                    offset: (i64::from(s.offset.0), i64::from(s.offset.1)),
                },
                &damage,
            ),
            Op::Blur(at, round, b) => r.blur(rect(*at), rounding(*round), &blur(*b), &damage),
            Op::Composite {
                texture,
                src,
                rect: at,
            } => {
                let Some(tex) = textures.get(texture).copied() else {
                    return;
                };
                let (src, at) = (rect(*src), rect(*at));
                // Pixel for pixel from the rectangle's corner, as the
                // software painter's `composite` draws.
                let across = Rect::new(
                    at.x.saturating_sub(src.x),
                    at.y.saturating_sub(src.y),
                    i64::from(tex.image.width),
                    i64::from(tex.image.height),
                );
                let shape = Rect::new(
                    at.x,
                    at.y,
                    src.width.min(at.width),
                    src.height.min(at.height),
                );
                r.composite_image(
                    tex.image,
                    tex.opaque,
                    across,
                    (shape, Rounding::none()),
                    1.0,
                    true,
                    &damage,
                );
            }
            Op::CompositeScaled {
                texture,
                src,
                rect: at,
                rounding: round,
                opacity,
                nearest,
            } => {
                let Some(tex) = textures.get(texture).copied() else {
                    return;
                };
                let (src, at) = (rect(*src), rect(*at));
                let across = Rect::new(
                    at.x.saturating_sub(scaled(src.x, at.width, src.width)),
                    at.y.saturating_sub(scaled(src.y, at.height, src.height)),
                    scaled(i64::from(tex.image.width), at.width, src.width),
                    scaled(i64::from(tex.image.height), at.height, src.height),
                );
                let same = at.width == src.width && at.height == src.height;
                r.composite_image(
                    tex.image,
                    tex.opaque,
                    across,
                    (at, rounding(*round)),
                    *opacity,
                    *nearest || same,
                    &damage,
                );
            }
            Op::KeepBackdrop => r.keep_backdrop(&mut canvas.backdrop, &damage),
            Op::BlurBackdrop(at, round, b) => r.blur_backdrop(
                &mut canvas.backdrop,
                rect(*at),
                rounding(*round),
                &blur(*b),
                &damage,
            ),
        }
    }

    /// Move `r` of texture `texture`'s pixels in from the arena.
    fn upload(
        &mut self,
        texture: u64,
        r: protocol::Rect,
        arena: u32,
        offset: u64,
        stride: u32,
    ) -> Result<(), String> {
        let tex = self
            .textures
            .get(&texture)
            .copied()
            .ok_or_else(|| format!("an upload into unknown texture {texture}"))?;
        let data = self
            .arenas
            .get(&arena)
            .and_then(|a| a.bytes_from(offset))
            .ok_or_else(|| format!("an upload from outside arena {arena}"))?;
        self.device
            .upload(tex.image.id, region(r), stride, data)
            .map_err(|error| format!("an upload: {error}"))
    }

    /// The frame's end: its present damage into the output, submitted and
    /// waited for, then `Done` or `Failed`.
    fn end_frame(&mut self, number: u64, present: &[protocol::Rect]) -> Result<(), End> {
        let Some(frame) = self.frame.take() else {
            return Ok(());
        };
        let (canvas_id, output) = (frame.canvas, frame.output);
        let mut why = frame.bad;
        if let Some(canvas) = self.canvases.get_mut(&canvas_id) {
            let flushed = into_output(&self.device, canvas, output, present, why.is_some())
                .map_err(|failed| why = why.clone().or(Some(failed)))
                .ok()
                .flatten();
            if let Err(error) = canvas.renderer.finish() {
                why = why.or(Some((failure::BAD_OP, error.to_string())));
            }
            if let Some((id, rows, stride)) = flushed
                && self.flush_outputs
            {
                self.device.0.borrow().flush_output_lines(id, rows, stride);
            }
        } else {
            why = why.or(Some((failure::BAD_OP, format!("no canvas {canvas_id}"))));
        }
        let lost = self.device.0.borrow().is_lost();
        if lost {
            let why = why.map_or_else(|| "the device was lost".to_owned(), |(_, why)| why);
            let _ = self.send(
                &Message::Failed {
                    canvas: canvas_id,
                    frame: number,
                    code: failure::DEVICE_LOST,
                    why: why.clone(),
                },
                &[],
            );
            return Err(End::Lost(why));
        }
        match why {
            None => self.send(
                &Message::Done {
                    canvas: canvas_id,
                    frame: number,
                    output,
                    flags: 0,
                },
                &[],
            ),
            Some((code, why)) => self.send(
                &Message::Failed {
                    canvas: canvas_id,
                    frame: number,
                    code,
                    why,
                },
                &[],
            ),
        }
    }

    /// Copy `r` of canvas `canvas`'s frame into the arena.
    fn read(&mut self, canvas: u32, r: protocol::Rect, arena: u32, offset: u64, stride: u32) {
        let Some(target) = self.canvases.get(&canvas).map(|c| c.renderer.target()) else {
            return;
        };
        let pixels = self.device.read(target.id, region(r));
        let Some(into) = self
            .arenas
            .get_mut(&arena)
            .and_then(|a| a.bytes_from_mut(offset))
        else {
            return;
        };
        let Ok(pixels) = pixels else {
            return;
        };
        let row = r.width as usize * 4;
        if row == 0 {
            return;
        }
        for (index, from) in pixels.chunks_exact(row).enumerate() {
            let start = index * stride as usize;
            if let Some(to) = into.get_mut(start..start + row) {
                to.copy_from_slice(from);
            }
        }
    }
}

/// The rows of an output written, its number and its stride.
type Written = (u32, std::ops::Range<u32>, u32);

/// Add `present` to what every output of `canvas` missed, and copy what
/// output `output` missed into it from the frame -- unless `skip`, for a
/// frame that failed. The output is then the card's until released.
fn into_output(
    device: &Shared,
    canvas: &mut Canvas,
    output: u32,
    present: &[protocol::Rect],
    skip: bool,
) -> Result<Option<Written>, (u32, String)> {
    for out in &mut canvas.outputs {
        for r in present {
            out.missed.add(rect(*r));
        }
    }
    let target = canvas.renderer.target();
    let Some(out) = canvas.outputs.get_mut(output as usize) else {
        return Err((failure::BAD_OP, format!("no output {output}")));
    };
    if !out.released {
        return Err((failure::BAD_OP, format!("output {output} was not released")));
    }
    if skip {
        return Ok(None);
    }
    let regions: Vec<Region> = out
        .missed
        .rects()
        .iter()
        .map(|r| Region {
            x: u32::try_from(r.x.max(0)).unwrap_or(0),
            y: u32::try_from(r.y.max(0)).unwrap_or(0),
            width: u32::try_from(r.width.max(0)).unwrap_or(0),
            height: u32::try_from(r.height.max(0)).unwrap_or(0),
        })
        .collect();
    device
        .0
        .borrow_mut()
        .copy(target.id, out.id, &regions)
        .map_err(|error| (failure::BAD_OP, error.to_string()))?;
    let written = out.missed.bounds().map(|bounds| {
        let top = u32::try_from(bounds.y.max(0)).unwrap_or(0);
        let bottom = u32::try_from(bounds.bottom().max(0)).unwrap_or(0);
        (out.id, top..bottom, out.stride)
    });
    out.missed.clear();
    out.released = false;
    Ok(written)
}

/// Say `line` on standard error, the helper's log (hyprix's).
pub fn warn(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(io::stderr(), "hyprix-gpu: {line}");
}
