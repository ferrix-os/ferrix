//! A screen's frame drawn by hyprix-gpu: a [`Painter`] that writes ops.
//!
//! [`compositor_render::render_onto`] draws a frame as `Painter` calls; this
//! painter turns each into an [`Op`] (`docs/GPU-COMPOSITOR.md` §4.5) and
//! [`Canvas::finish`] sends them and waits for the helper to say the frame
//! is in its output. A client's shared-memory pixels under the damage are
//! copied into the arena as they are drawn; a client's dmabuf is imported
//! once and sampled where it lies.
//!
//! As the virgl painter does, a failure is remembered rather than answered:
//! every call after it is skipped and [`Canvas::finish`] says what went
//! wrong, which the compositor answers with the software painter.

use std::cell::RefCell;
use std::io;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use compositor_gpu_protocol as proto;
use compositor_render::{Blur, Color, Damage, Gradient, Painter, Rect, Rounding, Shadow, Surface};
use proto::{Message, Op, OpsWriter};

use super::link::Link;

/// How long a frame may take on the GPU before it counts as failed: the
/// compositor's hold after a dropped frame.
pub(crate) const FRAME_DEADLINE: Duration = Duration::from_millis(250);

/// How long the helper has to make a canvas's outputs.
const CONFIGURE_DEADLINE: Duration = Duration::from_secs(2);

/// Frames a texture may go unsampled before the helper is told to forget
/// it.
const FORGET_AFTER: u64 = 120;

/// Names of textures made for a surface that has no name of its own, which
/// live one frame: above every name a surface or a dmabuf is given.
const TRANSIENT: u64 = 1 << 63;

/// Names of client dmabufs, apart from shared-memory surfaces' names.
const DMABUF: u64 = 1 << 62;

/// What a canvas keeps behind its windows: in the helper, so here only its
/// size, which is what [`Painter::backdrop_fits`] asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backdrop {
    width: u32,
    height: u32,
}

impl Backdrop {
    /// A backdrop for a canvas of this size.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// One output buffer the helper draws a frame into.
#[derive(Debug)]
struct Output {
    fd: OwnedFd,
    plane: proto::Plane,
}

/// A screen's frame, drawn by hyprix-gpu.
#[derive(Debug)]
pub struct Canvas {
    link: Rc<RefCell<Link>>,
    canvas: u32,
    width: u32,
    height: u32,
    outputs: Vec<Output>,
    modifier: u64,
    /// The frame's ops so far.
    ops: OpsWriter,
    /// The damage the last op sent was cut to.
    damage: Vec<proto::Rect>,
    /// Every rectangle drawn in this frame: what goes into the output.
    present: Damage,
    /// What went wrong in this frame, which [`Canvas::finish`] says.
    error: Option<String>,
    /// Textures made for this frame alone.
    transient: Vec<u64>,
    /// Whether the card still reads the output: it is released before the
    /// next frame is drawn into it.
    owed_release: bool,
}

impl Canvas {
    /// A `width` × `height` canvas numbered `canvas` on the helper behind
    /// `link`, and its output.
    ///
    /// # Errors
    ///
    /// A helper that has gone, or did not make the outputs in time.
    pub(crate) fn new(
        link: Rc<RefCell<Link>>,
        canvas: u32,
        width: u32,
        height: u32,
    ) -> io::Result<Self> {
        let (outputs, modifier) = {
            let mut held = link.borrow_mut();
            held.send(
                &Message::Configure {
                    canvas,
                    width,
                    height,
                    outputs: 1,
                },
                &[],
            )?;
            let deadline = Instant::now() + CONFIGURE_DEADLINE;
            loop {
                let received = held.receive(deadline)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "hyprix-gpu made no outputs in time",
                    )
                })?;
                match received.message {
                    Message::Outputs {
                        canvas: of,
                        width: w,
                        height: h,
                        modifier,
                        planes,
                        ..
                    } if of == canvas => {
                        break outputs_of((w, h), (width, height), planes, received.fds)
                            .map(|outputs| (outputs, modifier))?;
                    }
                    Message::Failed { why, .. } => {
                        return Err(io::Error::other(format!("hyprix-gpu: {why}")));
                    }
                    _ => {}
                }
            }
        };
        Ok(Self {
            link,
            canvas,
            width,
            height,
            outputs,
            modifier,
            ops: OpsWriter::new(),
            damage: Vec::new(),
            present: Damage::new(),
            error: None,
            transient: Vec::new(),
            owed_release: false,
        })
    }

    /// The output the screen is pointed at, to adopt: a linear buffer of
    /// four bytes a pixel, rows packed, or `None`, and the frame is read
    /// back instead.
    ///
    /// # Errors
    ///
    /// The descriptor would not duplicate.
    pub fn export(&self) -> io::Result<Option<OwnedFd>> {
        let Some(output) = self.outputs.first() else {
            return Ok(None);
        };
        if self.modifier != 0
            || output.plane.offset != 0
            || output.plane.stride != self.width.saturating_mul(4)
        {
            return Ok(None);
        }
        output.fd.try_clone().map(Some)
    }

    /// Whether a client's dmabuf `surface` is sampled where it lies.
    pub fn imports(&self, surface: &Surface<'_>) -> bool {
        surface.device().is_some_and(|on| {
            self.link
                .borrow()
                .samples(surface.format().fourcc(), on.modifier)
        })
    }

    /// Send the frame and wait for the helper to say it is in the output.
    ///
    /// # Errors
    ///
    /// What went wrong while the frame was recorded, a helper that has
    /// gone, a frame it could not draw, or one that took longer than
    /// [`FRAME_DEADLINE`].
    pub fn finish(&mut self) -> io::Result<()> {
        let bodies = self.ops.finish();
        let present = core::mem::take(&mut self.present);
        self.damage.clear();
        let transient = core::mem::take(&mut self.transient);
        let error = self.error.take();
        let mut link = self.link.borrow_mut();
        if let Some(error) = error {
            link.reset_arena();
            forget(&mut link, transient);
            return Err(io::Error::other(error));
        }
        if bodies.is_empty() {
            link.reset_arena();
            forget(&mut link, transient);
            return Ok(());
        }
        if core::mem::take(&mut self.owed_release) {
            link.send(
                &Message::Release {
                    canvas: self.canvas,
                    output: 0,
                },
                &[],
            )?;
        }
        let frame = link.next_frame();
        link.send(
            &Message::FrameBegin {
                canvas: self.canvas,
                frame,
                output: 0,
            },
            &[],
        )?;
        for body in bodies {
            link.send(&Message::Ops(body), &[])?;
        }
        link.send(
            &Message::FrameEnd {
                frame,
                damage: rects(&present),
            },
            &[],
        )?;
        let deadline = Instant::now() + FRAME_DEADLINE;
        let answer = loop {
            let Some(received) = link.receive(deadline)? else {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "hyprix-gpu did not finish a frame in {} ms",
                        FRAME_DEADLINE.as_millis()
                    ),
                ));
            };
            match received.message {
                Message::Done {
                    canvas, frame: of, ..
                } if canvas == self.canvas && of == frame => break Ok(()),
                Message::Failed {
                    canvas, code, why, ..
                } if canvas == self.canvas || code == proto::failure::DEVICE_LOST => {
                    if code == proto::failure::DEVICE_LOST {
                        link.lose(format!("hyprix-gpu: the device is lost: {why}"));
                    }
                    break Err(io::Error::other(format!("hyprix-gpu: {why}")));
                }
                _ => {}
            }
        };
        self.owed_release = true;
        link.reset_arena();
        forget(&mut link, transient);
        link.forget_unused(FORGET_AFTER);
        answer
    }

    /// The pixels of `rect` of the last frame, rows packed.
    ///
    /// # Errors
    ///
    /// As [`Canvas::finish`]'s.
    pub fn read(&mut self, rect: Rect) -> io::Result<Vec<u8>> {
        let row = usize::try_from(rect.width).unwrap_or(0).saturating_mul(4);
        let mut pixels = vec![0; row.saturating_mul(usize::try_from(rect.height).unwrap_or(0))];
        self.read_into_packed(rect, &mut pixels)?;
        Ok(pixels)
    }

    /// The pixels of `rect` of the last frame, written into `buffer` at the
    /// same place, rows `stride` bytes apart.
    ///
    /// # Errors
    ///
    /// As [`Canvas::finish`]'s.
    pub fn read_into(&mut self, rect: Rect, buffer: &mut [u8], stride: u32) -> io::Result<()> {
        let pixels = self.read(rect)?;
        let row = usize::try_from(rect.width).unwrap_or(0).saturating_mul(4);
        let (Ok(x), Ok(y)) = (usize::try_from(rect.x), usize::try_from(rect.y)) else {
            return Ok(());
        };
        for (at, from) in pixels.chunks(row.max(1)).enumerate() {
            let start = (y + at)
                .saturating_mul(stride as usize)
                .saturating_add(x.saturating_mul(4));
            if let Some(to) = buffer.get_mut(start..start.saturating_add(from.len())) {
                to.copy_from_slice(from);
            }
        }
        Ok(())
    }

    fn read_into_packed(&mut self, rect: Rect, pixels: &mut [u8]) -> io::Result<()> {
        if pixels.is_empty() {
            return Ok(());
        }
        let mut link = self.link.borrow_mut();
        link.reset_arena();
        let (arena, offset) = link.reserve(pixels.len())?;
        let wire = to_rect(rect);
        link.send(
            &Message::Read {
                canvas: self.canvas,
                rect: wire,
                arena,
                offset: offset as u64,
                stride: wire.width.saturating_mul(4),
            },
            &[],
        )?;
        let deadline = Instant::now() + FRAME_DEADLINE;
        loop {
            let received = link.receive(deadline)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "hyprix-gpu did not read in time")
            })?;
            match received.message {
                Message::ReadDone { canvas, .. } if canvas == self.canvas => break,
                Message::Failed { canvas, why, .. } if canvas == self.canvas => {
                    return Err(io::Error::other(format!("hyprix-gpu: {why}")));
                }
                _ => {}
            }
        }
        link.copy_out(arena, offset, pixels);
        Ok(())
    }

    /// Append `op`, cut to `damage`.
    fn op(&mut self, op: Op, damage: &Damage) {
        if self.error.is_some() || damage.is_empty() {
            return;
        }
        let wanted = rects(damage);
        if wanted != self.damage {
            if let Err(error) = self.ops.push(&Op::Damage(wanted.clone())) {
                self.error = Some(format!("a damage hyprix-gpu cannot take: {error}"));
                return;
            }
            self.damage = wanted;
            for &rect in damage.rects() {
                self.present.add(rect);
            }
        }
        if let Err(error) = self.ops.push(&op) {
            self.error = Some(format!("an op hyprix-gpu cannot take: {error}"));
        }
    }

    /// The texture `surface` is drawn from, and the part of it that is the
    /// surface, its pixels under `need` (the surface's own pixels) uploaded
    /// first; `None` for a surface with nothing to draw.
    fn texture(&mut self, surface: &Surface<'_>, need: &[Rect]) -> Option<(u64, proto::Rect)> {
        let mut link = self.link.borrow_mut();
        let fourcc = surface.format().fourcc();
        if let Some(on) = surface.device()
            && link.samples(fourcc, on.modifier)
        {
            let name = DMABUF | (on.key & !(TRANSIENT | DMABUF));
            if let Err(error) = link.import(surface, on.fd, name, fourcc) {
                self.error = Some(format!("a client's dmabuf: {error}"));
                return None;
            }
            link.used(name);
            return Some((
                name,
                proto::Rect::new(
                    i32::try_from(on.x).unwrap_or(0),
                    i32::try_from(on.y).unwrap_or(0),
                    surface.width(),
                    surface.height(),
                ),
            ));
        }
        if !surface.has_pixels() {
            return None;
        }
        let name = match surface.name() {
            0 => {
                let name = link.transient_name(TRANSIENT);
                self.transient.push(name);
                name
            }
            name => name & !(TRANSIENT | DMABUF),
        };
        let made = link.ensure_texture(name, surface.width(), surface.height(), fourcc);
        if let Err(error) = made {
            self.error = Some(format!("a texture: {error}"));
            return None;
        }
        // A texture just made holds nothing, so all of it is uploaded.
        let whole = [Rect::new(
            0,
            0,
            i64::from(surface.width()),
            i64::from(surface.height()),
        )];
        let need = if made.unwrap_or(false) {
            &whole[..]
        } else {
            need
        };
        for &part in need {
            let Some(part) = compositor_render::intersect(part, whole[0]) else {
                continue;
            };
            match link.upload(surface, part) {
                Ok(upload) => {
                    if let Err(error) = self.ops.push(&Op::Upload {
                        texture: name,
                        rect: to_rect(part),
                        arena: upload.0,
                        offset: upload.1 as u64,
                        stride: upload.2,
                    }) {
                        self.error = Some(format!("an upload: {error}"));
                        return None;
                    }
                }
                Err(error) => {
                    self.error = Some(format!("an upload: {error}"));
                    return None;
                }
            }
        }
        link.used(name);
        Some((
            name,
            proto::Rect::new(0, 0, surface.width(), surface.height()),
        ))
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        if let Ok(mut link) = self.link.try_borrow_mut() {
            let _ = link.send(
                &Message::Drop {
                    canvas: self.canvas,
                },
                &[],
            );
        }
    }
}

/// The outputs an `Outputs` record named, if they are the size asked for.
fn outputs_of(
    made: (u32, u32),
    asked: (u32, u32),
    planes: Vec<proto::Plane>,
    fds: Vec<OwnedFd>,
) -> io::Result<Vec<Output>> {
    if made != asked || planes.is_empty() {
        return Err(io::Error::other("hyprix-gpu made outputs of another size"));
    }
    Ok(fds
        .into_iter()
        .zip(planes)
        .map(|(fd, plane)| Output { fd, plane })
        .collect())
}

/// Tell the helper to forget the textures of one frame.
fn forget(link: &mut Link, names: Vec<u64>) {
    for name in names {
        link.forget(name);
    }
}

/// `damage` as the protocol's rectangles, at most [`proto::MAX_RECTS`]: more
/// become their bounds, which only the present damage may be.
fn rects(damage: &Damage) -> Vec<proto::Rect> {
    if damage.rects().len() > proto::MAX_RECTS {
        return damage.bounds().map(to_rect).into_iter().collect();
    }
    damage.rects().iter().copied().map(to_rect).collect()
}

/// A rectangle of the layout's as the protocol's: negative sizes are
/// empty, coordinates clamped to `i32`.
pub(crate) fn to_rect(rect: Rect) -> proto::Rect {
    let clamp =
        |value: i64| i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX });
    let size = |value: i64| u32::try_from(value.max(0)).unwrap_or(u32::MAX);
    proto::Rect::new(
        clamp(rect.x),
        clamp(rect.y),
        size(rect.width),
        size(rect.height),
    )
}

fn to_rounding(rounding: Rounding) -> proto::Rounding {
    proto::Rounding {
        radius: i32::try_from(rounding.radius).unwrap_or(i32::MAX),
        power: rounding.power,
    }
}

fn to_gradient(gradient: &Gradient) -> proto::Gradient {
    proto::Gradient {
        degrees: gradient.angle_degrees(),
        colors: gradient.colors().iter().map(|color| color.0).collect(),
    }
}

fn to_blur(blur: &Blur) -> proto::Blur {
    proto::Blur {
        size: i32::try_from(blur.size).unwrap_or(i32::MAX),
        passes: blur.passes,
        noise: blur.noise,
        contrast: blur.contrast,
        brightness: blur.brightness,
        vibrancy: blur.vibrancy,
        vibrancy_darkness: blur.vibrancy_darkness,
    }
}

/// The parts of a surface laid 1:1 at `rect` that `damage` reaches, in the
/// surface's own pixels.
fn under(rect: Rect, damage: &Damage) -> Vec<Rect> {
    damage
        .rects()
        .iter()
        .filter_map(|&cut| compositor_render::intersect(cut, rect))
        .map(|part| Rect::new(part.x - rect.x, part.y - rect.y, part.width, part.height))
        .collect()
}

impl Painter for Canvas {
    type Backdrop = Backdrop;

    fn bounds(&self) -> Rect {
        Rect::new(0, 0, i64::from(self.width), i64::from(self.height))
    }

    fn clear(&mut self, color: Color, damage: &Damage) {
        self.op(Op::Clear(color.0), damage);
    }

    fn fill(&mut self, rect: Rect, color: Color, damage: &Damage) {
        self.op(Op::Fill(to_rect(rect), color.0), damage);
    }

    fn fill_rounded(&mut self, rect: Rect, rounding: Rounding, color: Color, damage: &Damage) {
        self.op(
            Op::FillRounded(to_rect(rect), to_rounding(rounding), color.0),
            damage,
        );
    }

    fn fill_rounded_gradient(
        &mut self,
        rect: Rect,
        rounding: Rounding,
        gradient: &Gradient,
        damage: &Damage,
    ) {
        self.op(
            Op::FillRoundedGradient(to_rect(rect), to_rounding(rounding), to_gradient(gradient)),
            damage,
        );
    }

    fn border_gradient(&mut self, rect: Rect, width: i64, gradient: &Gradient, damage: &Damage) {
        self.op(
            Op::BorderGradient(
                to_rect(rect),
                i32::try_from(width).unwrap_or(i32::MAX),
                to_gradient(gradient),
            ),
            damage,
        );
    }

    fn shadow(&mut self, rect: Rect, shadow: &Shadow, damage: &Damage) {
        let wire = proto::Shadow {
            rounding: to_rounding(shadow.rounding),
            range: i32::try_from(shadow.range).unwrap_or(i32::MAX),
            power: shadow.power,
            color: shadow.color.0,
            offset: (
                i32::try_from(shadow.offset.0).unwrap_or(0),
                i32::try_from(shadow.offset.1).unwrap_or(0),
            ),
        };
        self.op(Op::Shadow(to_rect(rect), wire), damage);
    }

    fn blur(&mut self, rect: Rect, rounding: Rounding, blur: &Blur, damage: &Damage) {
        self.op(
            Op::Blur(to_rect(rect), to_rounding(rounding), to_blur(blur)),
            damage,
        );
    }

    fn composite(&mut self, surface: &Surface<'_>, rect: Rect, damage: &Damage) {
        if self.error.is_some() || damage.is_empty() {
            return;
        }
        let need = under(rect, damage);
        if need.is_empty() {
            return;
        }
        let Some((texture, src)) = self.texture(surface, &need) else {
            return;
        };
        self.op(
            Op::Composite {
                texture,
                src,
                rect: to_rect(rect),
            },
            damage,
        );
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
        if self.error.is_some() || damage.is_empty() || under(rect, damage).is_empty() {
            return;
        }
        // Stretched, any pixel of it may be sampled: all of it goes.
        let whole = [Rect::new(
            0,
            0,
            i64::from(surface.width()),
            i64::from(surface.height()),
        )];
        let Some((texture, src)) = self.texture(surface, &whole) else {
            return;
        };
        self.op(
            Op::CompositeScaled {
                texture,
                src,
                rect: to_rect(rect),
                rounding: to_rounding(rounding),
                opacity,
                nearest,
            },
            damage,
        );
    }

    fn backdrop_fits(&self, backdrop: &Self::Backdrop) -> bool {
        backdrop.width == self.width && backdrop.height == self.height
    }

    fn keep_backdrop(&mut self, _backdrop: &mut Self::Backdrop, damage: &Damage) {
        self.op(Op::KeepBackdrop, damage);
    }

    fn blur_backdrop(
        &mut self,
        _backdrop: &mut Self::Backdrop,
        rect: Rect,
        rounding: Rounding,
        blur: &Blur,
        damage: &Damage,
    ) {
        self.op(
            Op::BlurBackdrop(to_rect(rect), to_rounding(rounding), to_blur(blur)),
            damage,
        );
    }
}
