//! What hyprix and hyprix-gpu say to each other (`docs/GPU-COMPOSITOR.md`).
//!
//! hyprix draws a frame as a sequence of `compositor_render::Painter` calls.
//! When a Vulkan helper process draws it, each call becomes an [`Op`], the
//! ops travel in [`Message::Ops`] records over a `SOCK_SEQPACKET` socket
//! pair, and the helper answers with [`Message::Done`] once the GPU has put
//! the frame in an output dmabuf. Descriptors (dmabufs, the arena memfd,
//! outputs) ride on the records by `SCM_RIGHTS`; this crate says how many
//! each record carries ([`Message::fds`]) and leaves the socket to each
//! side.
//!
//! Every record is a [`Header`] and a body of at most [`MAX_BODY`] bytes,
//! all integers and floats little-endian. A record that does not decode is
//! a reason to close the connection: [`Error`] says why.
//!
//! No `std` and no dependencies: hyprix is a static musl program and the
//! helper a glibc one, and both build this crate as it is.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// The first four bytes of every record: `HXG1`.
pub const MAGIC: u32 = u32::from_le_bytes(*b"HXG1");

/// The version this crate speaks, which [`Message::Hello`] and
/// [`Message::Ready`] carry.
pub const VERSION: u32 = 1;

/// Bytes of a [`Header`].
pub const HEADER: usize = 16;

/// The most a record may be, header included. Under the 64 KiB a socket
/// buffer is sure to take in one piece.
pub const MAX_RECORD: usize = 60 * 1024;

/// The most a record's body may be.
pub const MAX_BODY: usize = MAX_RECORD - HEADER;

/// The most rectangles one [`Op::Damage`] or [`Message::FrameEnd`] carries.
/// A frame whose damage has more is drawn with fewer, larger ones by the
/// sender (every op of the frame cut to the same, larger damage).
pub const MAX_RECTS: usize = 1024;

/// The most colours a gradient has: `compositor_config::MAX_GRADIENT_COLORS`
/// and more.
pub const MAX_COLORS: usize = 16;

/// The most planes a dmabuf or an output set has.
pub const MAX_PLANES: usize = 4;

/// The most bytes of a string in a record.
pub const MAX_STRING: usize = 4096;

/// [`Message::Ready`]'s `caps`: the helper can hand a `sync_file` with
/// [`Message::Done`].
pub const CAP_SYNC_FILE: u32 = 1;
/// [`Message::Ready`]'s `caps`: the helper samples the arena where it lies
/// (`VK_EXT_external_memory_host`) rather than copying it.
pub const CAP_HOST_MEMORY: u32 = 1 << 1;

/// [`Message::Done`]'s `flags`: a `sync_file` rides on the record, signalled
/// when the GPU has finished. Without it the GPU has finished already.
pub const DONE_SYNC_FILE: u32 = 1;

/// DRM's `ARGB8888`: premultiplied alpha, as `wl_shm` defines it.
pub const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// DRM's `XRGB8888`: opaque, the X byte read as 255.
pub const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// [`Message::Outputs`]' modifier when the outputs are memfds, not dmabufs
/// (`DRM_FORMAT_MOD_INVALID`): the compositor maps and copies them, as a
/// `wl_shm` buffer. A device that exports no dmabufs -- lavapipe on Ferrix
/// -- draws into these.
pub const MODIFIER_MEMFD: u64 = 0x00ff_ffff_ffff_ffff;

/// [`Message::Failed`]'s codes.
pub mod failure {
    /// An op the helper could not draw: a texture it does not have, a
    /// rectangle out of range.
    pub const BAD_OP: u32 = 1;
    /// The device's memory ran out.
    pub const OUT_OF_MEMORY: u32 = 2;
    /// The device is lost; the helper exits after saying so.
    pub const DEVICE_LOST: u32 = 3;
}

/// Why a record or an op did not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than the header or a field needs.
    Short,
    /// Bytes left over after the last field.
    Long,
    /// The record does not start with [`MAGIC`].
    Magic,
    /// The header's length is not the body's.
    Length,
    /// A record, string or list over its limit.
    TooBig,
    /// A message kind this version does not have.
    Kind(u16),
    /// An op tag this version does not have.
    Tag(u8),
    /// The header names a different count of descriptors than the body
    /// needs.
    Fds,
    /// A string that is not UTF-8.
    Utf8,
    /// A field that cannot be what it says: an op's padding not zero.
    Value,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short => f.write_str("a record shorter than its fields"),
            Self::Long => f.write_str("bytes after a record's last field"),
            Self::Magic => f.write_str("a record without the magic"),
            Self::Length => f.write_str("a header whose length is not the body's"),
            Self::TooBig => f.write_str("a record or list over its limit"),
            Self::Kind(kind) => write!(f, "message kind {kind}"),
            Self::Tag(tag) => write!(f, "op tag {tag}"),
            Self::Fds => f.write_str("a record with the wrong count of descriptors"),
            Self::Utf8 => f.write_str("a string that is not UTF-8"),
            Self::Value => f.write_str("a field out of range"),
        }
    }
}

/// A rectangle in a canvas's pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    /// The left edge.
    pub x: i32,
    /// The top edge.
    pub y: i32,
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
}

impl Rect {
    /// A rectangle from its position and size.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// How a shape's corners are cut: `compositor_render::Rounding`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rounding {
    /// The corner's radius in pixels.
    pub radius: i32,
    /// The superellipse's power: 2 is a circle.
    pub power: f32,
}

/// A gradient run across a rectangle: `compositor_render::Gradient`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gradient {
    /// Its angle in degrees.
    pub degrees: i32,
    /// Its colours, `0xAARRGGBB`, at most [`MAX_COLORS`].
    pub colors: Vec<u32>,
}

/// A window's shadow: `compositor_render::Shadow`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shadow {
    /// The corners it is cut with.
    pub rounding: Rounding,
    /// How far it reaches.
    pub range: i32,
    /// How fast it falls off.
    pub power: u32,
    /// Its colour.
    pub color: u32,
    /// How far it is moved from the window.
    pub offset: (i32, i32),
}

/// How a blur is drawn: `compositor_render::Blur`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Blur {
    /// The kernel's size.
    pub size: i32,
    /// How many passes.
    pub passes: u32,
    /// Noise added.
    pub noise: f32,
    /// Contrast.
    pub contrast: f32,
    /// Brightness.
    pub brightness: f32,
    /// Vibrancy.
    pub vibrancy: f32,
    /// Vibrancy's darkness.
    pub vibrancy_darkness: f32,
}

/// One plane of a dmabuf: where its rows start and how far apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plane {
    /// Bytes from one row's start to the next.
    pub stride: u32,
    /// Where the first row starts, in bytes.
    pub offset: u32,
}

/// One of the `Painter` calls, or a copy of pixels the calls after it read
/// (`docs/GPU-COMPOSITOR.md` §4.5). Colours are `0xAARRGGBB`, not
/// premultiplied; every op is cut to the last [`Op::Damage`].
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// What every following op is cut to: disjoint rectangles.
    Damage(Vec<Rect>),
    /// Replace the damage with a colour, made opaque.
    Clear(u32),
    /// Blend a colour over a rectangle.
    Fill(Rect, u32),
    /// The same with its corners cut.
    FillRounded(Rect, Rounding, u32),
    /// A gradient over a rectangle with its corners cut.
    FillRoundedGradient(Rect, Rounding, Gradient),
    /// A square border this wide around the rectangle.
    BorderGradient(Rect, i32, Gradient),
    /// A window's shadow.
    Shadow(Rect, Shadow),
    /// The rectangle, corners cut, replaced with the blur of the frame as
    /// it stands.
    Blur(Rect, Rounding, Blur),
    /// A texture's `src` part blended at `rect`, pixel for pixel.
    Composite {
        /// The texture.
        texture: u64,
        /// The part of it drawn.
        src: Rect,
        /// Where.
        rect: Rect,
    },
    /// A texture's `src` part stretched to `rect`, corners cut.
    CompositeScaled {
        /// The texture.
        texture: u64,
        /// The part of it drawn.
        src: Rect,
        /// Where.
        rect: Rect,
        /// The corners.
        rounding: Rounding,
        /// How opaque, 0 to 1.
        opacity: f32,
        /// Sampled nearest rather than linear.
        nearest: bool,
    },
    /// Keep what has been drawn, inside the damage, as the backdrop.
    KeepBackdrop,
    /// The rectangle, corners cut, replaced with the blur of the backdrop.
    BlurBackdrop(Rect, Rounding, Blur),
    /// Copy `rect` of a texture made by [`Message::CreateTexture`] from the
    /// arena, rows `stride` bytes apart from `offset`.
    Upload {
        /// The texture.
        texture: u64,
        /// The part of it written.
        rect: Rect,
        /// Which arena.
        arena: u32,
        /// Where its first pixel is.
        offset: u64,
        /// Bytes from one row to the next.
        stride: u32,
    },
}

/// A record's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The message.
    pub kind: u16,
    /// How many descriptors ride on the record.
    pub fds: u16,
    /// Bytes of body.
    pub len: u32,
    /// The sender's count, from 1.
    pub serial: u32,
}

impl Header {
    /// Read a header off the front of `record`.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] or [`Error::Magic`]; [`Error::Length`] when the
    /// record is not header and body exactly.
    pub fn read(record: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(record);
        if reader.u32()? != MAGIC {
            return Err(Error::Magic);
        }
        let header = Self {
            kind: reader.u16()?,
            fds: reader.u16()?,
            len: reader.u32()?,
            serial: reader.u32()?,
        };
        if usize::try_from(header.len).ok() != Some(reader.rest().len()) {
            return Err(Error::Length);
        }
        if record.len() > MAX_RECORD {
            return Err(Error::TooBig);
        }
        Ok(header)
    }
}

/// A record's message (`docs/GPU-COMPOSITOR.md` §4).
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// → The first record: the version hyprix speaks.
    Hello {
        /// [`VERSION`].
        version: u32,
    },
    /// ← The device is up.
    Ready {
        /// The version the helper speaks.
        version: u32,
        /// [`CAP_SYNC_FILE`], [`CAP_HOST_MEMORY`].
        caps: u32,
        /// The device's name, as Vulkan says it.
        device: String,
        /// The driver's.
        driver: String,
    },
    /// ← The modifiers a client's dmabuf of `fourcc` is sampled with.
    Modifiers {
        /// [`ARGB8888`] or [`XRGB8888`].
        fourcc: u32,
        /// Each modifier, and whether it is external only.
        modifiers: Vec<(u64, bool)>,
    },
    /// → Make canvas `canvas`, or make it again at a new size.
    Configure {
        /// hyprix's number for it.
        canvas: u32,
        /// Its width.
        width: u32,
        /// Its height.
        height: u32,
        /// How many output buffers, 1 to 3.
        outputs: u32,
    },
    /// ← The canvas's output dmabufs, one descriptor each.
    Outputs {
        /// Whose.
        canvas: u32,
        /// Their width.
        width: u32,
        /// Their height.
        height: u32,
        /// Their format.
        fourcc: u32,
        /// Their modifier: 0 for linear.
        modifier: u64,
        /// Each one's single plane, in order.
        planes: Vec<Plane>,
    },
    /// → The screen is gone.
    Drop {
        /// Which.
        canvas: u32,
    },
    /// → A memfd hyprix writes pixels into: one descriptor.
    Arena {
        /// hyprix's number for it.
        arena: u32,
        /// Its bytes.
        size: u64,
    },
    /// → Sample this client dmabuf as `texture`: one descriptor a plane.
    ImportDmabuf {
        /// hyprix's name for it.
        texture: u64,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Its format.
        fourcc: u32,
        /// Its modifier.
        modifier: u64,
        /// Its planes.
        planes: Vec<Plane>,
    },
    /// → An image of the helper's own, filled by [`Op::Upload`].
    CreateTexture {
        /// hyprix's name for it.
        texture: u64,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Its format.
        fourcc: u32,
    },
    /// → hyprix is done with the texture.
    Forget {
        /// Which.
        texture: u64,
    },
    /// → A frame of `canvas`, to be put in `output`.
    FrameBegin {
        /// Which canvas.
        canvas: u32,
        /// The frame's number, counting up.
        frame: u64,
        /// The output its damage goes into.
        output: u32,
    },
    /// → Part of the frame's display list: [`Op`]s, encoded.
    Ops(Vec<u8>),
    /// → The frame's present damage; draw it.
    FrameEnd {
        /// The frame.
        frame: u64,
        /// What changed on the screen.
        damage: Vec<Rect>,
    },
    /// ← The frame is in its output.
    Done {
        /// Which canvas.
        canvas: u32,
        /// The frame.
        frame: u64,
        /// The output.
        output: u32,
        /// [`DONE_SYNC_FILE`].
        flags: u32,
    },
    /// ← The frame was not drawn.
    Failed {
        /// Which canvas.
        canvas: u32,
        /// The frame.
        frame: u64,
        /// One of [`failure`]'s.
        code: u32,
        /// What went wrong, for the log.
        why: String,
    },
    /// → The card is done reading that output.
    Release {
        /// Which canvas.
        canvas: u32,
        /// Which output.
        output: u32,
    },
    /// → Copy `rect` of the canvas's frame into the arena.
    Read {
        /// Which canvas.
        canvas: u32,
        /// What part.
        rect: Rect,
        /// Which arena.
        arena: u32,
        /// Where the first pixel goes.
        offset: u64,
        /// Bytes from one row to the next.
        stride: u32,
    },
    /// ← The pixels a [`Message::Read`] asked for are in the arena.
    ReadDone {
        /// Which canvas.
        canvas: u32,
        /// The read's serial.
        serial: u32,
    },
}

/// Message kinds, as the header carries them.
pub mod kind {
    #![allow(missing_docs, reason = "each is its Message variant's number")]
    pub const HELLO: u16 = 1;
    pub const READY: u16 = 2;
    pub const MODIFIERS: u16 = 3;
    pub const CONFIGURE: u16 = 4;
    pub const OUTPUTS: u16 = 5;
    pub const DROP: u16 = 6;
    pub const ARENA: u16 = 7;
    pub const IMPORT_DMABUF: u16 = 8;
    pub const CREATE_TEXTURE: u16 = 9;
    pub const FORGET: u16 = 10;
    pub const FRAME_BEGIN: u16 = 11;
    pub const OPS: u16 = 12;
    pub const FRAME_END: u16 = 13;
    pub const DONE: u16 = 14;
    pub const FAILED: u16 = 15;
    pub const RELEASE: u16 = 16;
    pub const READ: u16 = 17;
    pub const READ_DONE: u16 = 18;
}

impl Message {
    /// The kind the header carries.
    #[must_use]
    pub const fn kind(&self) -> u16 {
        match self {
            Self::Hello { .. } => kind::HELLO,
            Self::Ready { .. } => kind::READY,
            Self::Modifiers { .. } => kind::MODIFIERS,
            Self::Configure { .. } => kind::CONFIGURE,
            Self::Outputs { .. } => kind::OUTPUTS,
            Self::Drop { .. } => kind::DROP,
            Self::Arena { .. } => kind::ARENA,
            Self::ImportDmabuf { .. } => kind::IMPORT_DMABUF,
            Self::CreateTexture { .. } => kind::CREATE_TEXTURE,
            Self::Forget { .. } => kind::FORGET,
            Self::FrameBegin { .. } => kind::FRAME_BEGIN,
            Self::Ops(_) => kind::OPS,
            Self::FrameEnd { .. } => kind::FRAME_END,
            Self::Done { .. } => kind::DONE,
            Self::Failed { .. } => kind::FAILED,
            Self::Release { .. } => kind::RELEASE,
            Self::Read { .. } => kind::READ,
            Self::ReadDone { .. } => kind::READ_DONE,
        }
    }

    /// How many descriptors ride on this message's record, in the order
    /// the message names them.
    #[must_use]
    pub fn fds(&self) -> usize {
        match self {
            Self::Outputs { planes, .. } | Self::ImportDmabuf { planes, .. } => planes.len(),
            Self::Arena { .. } => 1,
            Self::Done { flags, .. } => usize::from(flags & DONE_SYNC_FILE != 0),
            _ => 0,
        }
    }

    /// The whole record for this message: header and body.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] for a message over a limit.
    pub fn encode(&self, serial: u32) -> Result<Vec<u8>, Error> {
        let mut out = Vec::with_capacity(64);
        put_u32(&mut out, MAGIC);
        put_u16(&mut out, self.kind());
        put_u16(
            &mut out,
            u16::try_from(self.fds()).map_err(|_| Error::TooBig)?,
        );
        put_u32(&mut out, 0);
        put_u32(&mut out, serial);
        self.body(&mut out)?;
        let len = out.len().saturating_sub(HEADER);
        if len > MAX_BODY {
            return Err(Error::TooBig);
        }
        let len = u32::try_from(len).map_err(|_| Error::TooBig)?;
        if let Some(field) = out.get_mut(8..12) {
            field.copy_from_slice(&len.to_le_bytes());
        }
        Ok(out)
    }

    fn body(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Self::Hello { version } => put_u32(out, *version),
            Self::Ready {
                version,
                caps,
                device,
                driver,
            } => {
                put_u32(out, *version);
                put_u32(out, *caps);
                put_str(out, device)?;
                put_str(out, driver)?;
            }
            Self::Modifiers { fourcc, modifiers } => {
                put_u32(out, *fourcc);
                put_len(out, modifiers.len(), MAX_BODY / 12)?;
                for &(modifier, external) in modifiers {
                    put_u64(out, modifier);
                    put_u32(out, u32::from(external));
                }
            }
            Self::Configure {
                canvas,
                width,
                height,
                outputs,
            } => {
                for value in [*canvas, *width, *height, *outputs] {
                    put_u32(out, value);
                }
            }
            Self::Outputs {
                canvas,
                width,
                height,
                fourcc,
                modifier,
                planes,
            } => {
                for value in [*canvas, *width, *height, *fourcc] {
                    put_u32(out, value);
                }
                put_u64(out, *modifier);
                put_planes(out, planes)?;
            }
            Self::Drop { canvas } => put_u32(out, *canvas),
            Self::Arena { arena, size } => {
                put_u32(out, *arena);
                put_u64(out, *size);
            }
            Self::ImportDmabuf {
                texture,
                width,
                height,
                fourcc,
                modifier,
                planes,
            } => {
                put_u64(out, *texture);
                for value in [*width, *height, *fourcc] {
                    put_u32(out, value);
                }
                put_u64(out, *modifier);
                put_planes(out, planes)?;
            }
            Self::CreateTexture {
                texture,
                width,
                height,
                fourcc,
            } => {
                put_u64(out, *texture);
                for value in [*width, *height, *fourcc] {
                    put_u32(out, value);
                }
            }
            Self::Forget { texture } => put_u64(out, *texture),
            Self::FrameBegin {
                canvas,
                frame,
                output,
            } => {
                put_u32(out, *canvas);
                put_u64(out, *frame);
                put_u32(out, *output);
            }
            Self::Ops(ops) => {
                if ops.len() > MAX_BODY {
                    return Err(Error::TooBig);
                }
                out.extend_from_slice(ops);
            }
            Self::FrameEnd { frame, damage } => {
                put_u64(out, *frame);
                put_rects(out, damage)?;
            }
            Self::Done {
                canvas,
                frame,
                output,
                flags,
            } => {
                put_u32(out, *canvas);
                put_u64(out, *frame);
                put_u32(out, *output);
                put_u32(out, *flags);
            }
            Self::Failed {
                canvas,
                frame,
                code,
                why,
            } => {
                put_u32(out, *canvas);
                put_u64(out, *frame);
                put_u32(out, *code);
                put_str(out, why)?;
            }
            Self::Release { canvas, output } => {
                put_u32(out, *canvas);
                put_u32(out, *output);
            }
            Self::Read {
                canvas,
                rect,
                arena,
                offset,
                stride,
            } => {
                put_u32(out, *canvas);
                put_rect(out, *rect);
                put_u32(out, *arena);
                put_u64(out, *offset);
                put_u32(out, *stride);
            }
            Self::ReadDone { canvas, serial } => {
                put_u32(out, *canvas);
                put_u32(out, *serial);
            }
        }
        Ok(())
    }

    /// Read a whole record: its header and its message.
    ///
    /// The caller checks that as many descriptors arrived as the header
    /// says; this checks that the header says as many as the message needs.
    ///
    /// # Errors
    ///
    /// Any [`Error`]: the record is not one this version sends.
    pub fn decode(record: &[u8]) -> Result<(Header, Self), Error> {
        let header = Header::read(record)?;
        let body = record.get(HEADER..).ok_or(Error::Short)?;
        let mut r = Reader::new(body);
        let message = match header.kind {
            kind::HELLO => Self::Hello { version: r.u32()? },
            kind::READY => Self::Ready {
                version: r.u32()?,
                caps: r.u32()?,
                device: r.string()?,
                driver: r.string()?,
            },
            kind::MODIFIERS => {
                let fourcc = r.u32()?;
                let n = r.len(MAX_BODY / 12)?;
                let mut modifiers = Vec::with_capacity(n);
                for _ in 0..n {
                    modifiers.push((r.u64()?, r.bool()?));
                }
                Self::Modifiers { fourcc, modifiers }
            }
            kind::CONFIGURE => Self::Configure {
                canvas: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                outputs: r.u32()?,
            },
            kind::OUTPUTS => Self::Outputs {
                canvas: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                fourcc: r.u32()?,
                modifier: r.u64()?,
                planes: r.planes()?,
            },
            kind::DROP => Self::Drop { canvas: r.u32()? },
            kind::ARENA => Self::Arena {
                arena: r.u32()?,
                size: r.u64()?,
            },
            kind::IMPORT_DMABUF => Self::ImportDmabuf {
                texture: r.u64()?,
                width: r.u32()?,
                height: r.u32()?,
                fourcc: r.u32()?,
                modifier: r.u64()?,
                planes: r.planes()?,
            },
            kind::CREATE_TEXTURE => Self::CreateTexture {
                texture: r.u64()?,
                width: r.u32()?,
                height: r.u32()?,
                fourcc: r.u32()?,
            },
            kind::FORGET => Self::Forget { texture: r.u64()? },
            kind::FRAME_BEGIN => Self::FrameBegin {
                canvas: r.u32()?,
                frame: r.u64()?,
                output: r.u32()?,
            },
            kind::OPS => {
                let ops = r.rest().to_vec();
                r = Reader::new(&[]);
                Self::Ops(ops)
            }
            kind::FRAME_END => Self::FrameEnd {
                frame: r.u64()?,
                damage: r.rects()?,
            },
            kind::DONE => Self::Done {
                canvas: r.u32()?,
                frame: r.u64()?,
                output: r.u32()?,
                flags: r.u32()?,
            },
            kind::FAILED => Self::Failed {
                canvas: r.u32()?,
                frame: r.u64()?,
                code: r.u32()?,
                why: r.string()?,
            },
            kind::RELEASE => Self::Release {
                canvas: r.u32()?,
                output: r.u32()?,
            },
            kind::READ => Self::Read {
                canvas: r.u32()?,
                rect: r.rect()?,
                arena: r.u32()?,
                offset: r.u64()?,
                stride: r.u32()?,
            },
            kind::READ_DONE => Self::ReadDone {
                canvas: r.u32()?,
                serial: r.u32()?,
            },
            other => return Err(Error::Kind(other)),
        };
        r.end()?;
        if usize::from(header.fds) != message.fds() {
            return Err(Error::Fds);
        }
        Ok((header, message))
    }
}

mod tag {
    pub(crate) const DAMAGE: u8 = 1;
    pub(crate) const CLEAR: u8 = 2;
    pub(crate) const FILL: u8 = 3;
    pub(crate) const FILL_ROUNDED: u8 = 4;
    pub(crate) const FILL_ROUNDED_GRADIENT: u8 = 5;
    pub(crate) const BORDER_GRADIENT: u8 = 6;
    pub(crate) const SHADOW: u8 = 7;
    pub(crate) const BLUR: u8 = 8;
    pub(crate) const COMPOSITE: u8 = 9;
    pub(crate) const COMPOSITE_SCALED: u8 = 10;
    pub(crate) const KEEP_BACKDROP: u8 = 11;
    pub(crate) const BLUR_BACKDROP: u8 = 12;
    pub(crate) const UPLOAD: u8 = 13;
}

impl Op {
    /// Append the op's bytes: its tag, three zero bytes, its fields.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] for a damage or gradient over its limit.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let tag = match self {
            Self::Damage(_) => tag::DAMAGE,
            Self::Clear(_) => tag::CLEAR,
            Self::Fill(..) => tag::FILL,
            Self::FillRounded(..) => tag::FILL_ROUNDED,
            Self::FillRoundedGradient(..) => tag::FILL_ROUNDED_GRADIENT,
            Self::BorderGradient(..) => tag::BORDER_GRADIENT,
            Self::Shadow(..) => tag::SHADOW,
            Self::Blur(..) => tag::BLUR,
            Self::Composite { .. } => tag::COMPOSITE,
            Self::CompositeScaled { .. } => tag::COMPOSITE_SCALED,
            Self::KeepBackdrop => tag::KEEP_BACKDROP,
            Self::BlurBackdrop(..) => tag::BLUR_BACKDROP,
            Self::Upload { .. } => tag::UPLOAD,
        };
        out.extend_from_slice(&[tag, 0, 0, 0]);
        match self {
            Self::Damage(rects) => put_rects(out, rects)?,
            Self::Clear(color) => put_u32(out, *color),
            Self::Fill(rect, color) => {
                put_rect(out, *rect);
                put_u32(out, *color);
            }
            Self::FillRounded(rect, rounding, color) => {
                put_rect(out, *rect);
                put_rounding(out, *rounding);
                put_u32(out, *color);
            }
            Self::FillRoundedGradient(rect, rounding, gradient) => {
                put_rect(out, *rect);
                put_rounding(out, *rounding);
                put_gradient(out, gradient)?;
            }
            Self::BorderGradient(rect, width, gradient) => {
                put_rect(out, *rect);
                put_i32(out, *width);
                put_gradient(out, gradient)?;
            }
            Self::Shadow(rect, shadow) => {
                put_rect(out, *rect);
                put_rounding(out, shadow.rounding);
                put_i32(out, shadow.range);
                put_u32(out, shadow.power);
                put_u32(out, shadow.color);
                put_i32(out, shadow.offset.0);
                put_i32(out, shadow.offset.1);
            }
            Self::Blur(rect, rounding, blur) | Self::BlurBackdrop(rect, rounding, blur) => {
                put_rect(out, *rect);
                put_rounding(out, *rounding);
                put_blur(out, *blur);
            }
            Self::Composite { texture, src, rect } => {
                put_u64(out, *texture);
                put_rect(out, *src);
                put_rect(out, *rect);
            }
            Self::CompositeScaled {
                texture,
                src,
                rect,
                rounding,
                opacity,
                nearest,
            } => {
                put_u64(out, *texture);
                put_rect(out, *src);
                put_rect(out, *rect);
                put_rounding(out, *rounding);
                put_f32(out, *opacity);
                put_u32(out, u32::from(*nearest));
            }
            Self::KeepBackdrop => {}
            Self::Upload {
                texture,
                rect,
                arena,
                offset,
                stride,
            } => {
                put_u64(out, *texture);
                put_rect(out, *rect);
                put_u32(out, *arena);
                put_u64(out, *offset);
                put_u32(out, *stride);
            }
        }
        Ok(())
    }

    /// Every op in an [`Message::Ops`] body, in order.
    ///
    /// # Errors
    ///
    /// The first op that does not decode; nothing of the record is drawn.
    pub fn decode_all(body: &[u8]) -> Result<Vec<Self>, Error> {
        let mut reader = Reader::new(body);
        let mut ops = Vec::new();
        while !reader.rest().is_empty() {
            ops.push(Self::decode(&mut reader)?);
        }
        Ok(ops)
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        let tag = r.u8()?;
        if [r.u8()?, r.u8()?, r.u8()?] != [0, 0, 0] {
            return Err(Error::Value);
        }
        Ok(match tag {
            tag::DAMAGE => Self::Damage(r.rects()?),
            tag::CLEAR => Self::Clear(r.u32()?),
            tag::FILL => Self::Fill(r.rect()?, r.u32()?),
            tag::FILL_ROUNDED => Self::FillRounded(r.rect()?, r.rounding()?, r.u32()?),
            tag::FILL_ROUNDED_GRADIENT => {
                Self::FillRoundedGradient(r.rect()?, r.rounding()?, r.gradient()?)
            }
            tag::BORDER_GRADIENT => Self::BorderGradient(r.rect()?, r.i32()?, r.gradient()?),
            tag::SHADOW => Self::Shadow(
                r.rect()?,
                Shadow {
                    rounding: r.rounding()?,
                    range: r.i32()?,
                    power: r.u32()?,
                    color: r.u32()?,
                    offset: (r.i32()?, r.i32()?),
                },
            ),
            tag::BLUR => Self::Blur(r.rect()?, r.rounding()?, r.blur()?),
            tag::COMPOSITE => Self::Composite {
                texture: r.u64()?,
                src: r.rect()?,
                rect: r.rect()?,
            },
            tag::COMPOSITE_SCALED => Self::CompositeScaled {
                texture: r.u64()?,
                src: r.rect()?,
                rect: r.rect()?,
                rounding: r.rounding()?,
                opacity: r.f32()?,
                nearest: r.bool()?,
            },
            tag::KEEP_BACKDROP => Self::KeepBackdrop,
            tag::BLUR_BACKDROP => Self::BlurBackdrop(r.rect()?, r.rounding()?, r.blur()?),
            tag::UPLOAD => Self::Upload {
                texture: r.u64()?,
                rect: r.rect()?,
                arena: r.u32()?,
                offset: r.u64()?,
                stride: r.u32()?,
            },
            other => return Err(Error::Tag(other)),
        })
    }
}

/// A frame's display list, split into [`Message::Ops`] bodies that each fit
/// a record. An op is never split between two.
#[derive(Debug, Default)]
pub struct OpsWriter {
    done: Vec<Vec<u8>>,
    current: Vec<u8>,
    scratch: Vec<u8>,
}

impl OpsWriter {
    /// An empty list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            done: Vec::new(),
            current: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Add `op` at the end.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] for an op over its own limits; the list is as it
    /// was.
    pub fn push(&mut self, op: &Op) -> Result<(), Error> {
        self.scratch.clear();
        op.encode(&mut self.scratch)?;
        if self.current.len() + self.scratch.len() > MAX_BODY {
            self.done.push(core::mem::take(&mut self.current));
        }
        self.current.extend_from_slice(&self.scratch);
        Ok(())
    }

    /// Whether nothing has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.current.is_empty()
    }

    /// The bodies, in order, each one record's; the writer is empty again.
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut bodies = core::mem::take(&mut self.done);
        if !self.current.is_empty() {
            bodies.push(core::mem::take(&mut self.current));
        }
        bodies
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_len(out: &mut Vec<u8>, len: usize, most: usize) -> Result<(), Error> {
    if len > most {
        return Err(Error::TooBig);
    }
    put_u32(out, u32::try_from(len).map_err(|_| Error::TooBig)?);
    Ok(())
}

fn put_str(out: &mut Vec<u8>, text: &str) -> Result<(), Error> {
    put_len(out, text.len(), MAX_STRING)?;
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

fn put_rect(out: &mut Vec<u8>, rect: Rect) {
    put_i32(out, rect.x);
    put_i32(out, rect.y);
    put_u32(out, rect.width);
    put_u32(out, rect.height);
}

fn put_rects(out: &mut Vec<u8>, rects: &[Rect]) -> Result<(), Error> {
    put_len(out, rects.len(), MAX_RECTS)?;
    for &rect in rects {
        put_rect(out, rect);
    }
    Ok(())
}

fn put_planes(out: &mut Vec<u8>, planes: &[Plane]) -> Result<(), Error> {
    put_len(out, planes.len(), MAX_PLANES)?;
    for plane in planes {
        put_u32(out, plane.stride);
        put_u32(out, plane.offset);
    }
    Ok(())
}

fn put_rounding(out: &mut Vec<u8>, rounding: Rounding) {
    put_i32(out, rounding.radius);
    put_f32(out, rounding.power);
}

fn put_gradient(out: &mut Vec<u8>, gradient: &Gradient) -> Result<(), Error> {
    put_i32(out, gradient.degrees);
    put_len(out, gradient.colors.len(), MAX_COLORS)?;
    for &color in &gradient.colors {
        put_u32(out, color);
    }
    Ok(())
}

fn put_blur(out: &mut Vec<u8>, blur: Blur) {
    put_i32(out, blur.size);
    put_u32(out, blur.passes);
    for value in [
        blur.noise,
        blur.contrast,
        blur.brightness,
        blur.vibrancy,
        blur.vibrancy_darkness,
    ] {
        put_f32(out, value);
    }
}

/// Fields read off the front of a body.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    const fn rest(&self) -> &'a [u8] {
        self.bytes
    }

    fn end(&self) -> Result<(), Error> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(Error::Long)
        }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let (head, tail) = self.bytes.split_first_chunk::<N>().ok_or(Error::Short)?;
        self.bytes = tail;
        Ok(*head)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(u8::from_le_bytes(self.take()?))
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.take()?))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn f32(&mut self) -> Result<f32, Error> {
        Ok(f32::from_le_bytes(self.take()?))
    }

    fn bool(&mut self) -> Result<bool, Error> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Value),
        }
    }

    fn len(&mut self, most: usize) -> Result<usize, Error> {
        let len = usize::try_from(self.u32()?).map_err(|_| Error::TooBig)?;
        if len > most {
            return Err(Error::TooBig);
        }
        Ok(len)
    }

    fn string(&mut self) -> Result<String, Error> {
        let len = self.len(MAX_STRING)?;
        if self.bytes.len() < len {
            return Err(Error::Short);
        }
        let (text, tail) = self.bytes.split_at(len);
        self.bytes = tail;
        core::str::from_utf8(text)
            .map(String::from)
            .map_err(|_| Error::Utf8)
    }

    fn rect(&mut self) -> Result<Rect, Error> {
        Ok(Rect {
            x: self.i32()?,
            y: self.i32()?,
            width: self.u32()?,
            height: self.u32()?,
        })
    }

    fn rects(&mut self) -> Result<Vec<Rect>, Error> {
        let n = self.len(MAX_RECTS)?;
        let mut rects = Vec::with_capacity(n);
        for _ in 0..n {
            rects.push(self.rect()?);
        }
        Ok(rects)
    }

    fn planes(&mut self) -> Result<Vec<Plane>, Error> {
        let n = self.len(MAX_PLANES)?;
        let mut planes = Vec::with_capacity(n);
        for _ in 0..n {
            planes.push(Plane {
                stride: self.u32()?,
                offset: self.u32()?,
            });
        }
        Ok(planes)
    }

    fn rounding(&mut self) -> Result<Rounding, Error> {
        Ok(Rounding {
            radius: self.i32()?,
            power: self.f32()?,
        })
    }

    fn gradient(&mut self) -> Result<Gradient, Error> {
        let degrees = self.i32()?;
        let n = self.len(MAX_COLORS)?;
        let mut colors = Vec::with_capacity(n);
        for _ in 0..n {
            colors.push(self.u32()?);
        }
        Ok(Gradient { degrees, colors })
    }

    fn blur(&mut self) -> Result<Blur, Error> {
        Ok(Blur {
            size: self.i32()?,
            passes: self.u32()?,
            noise: self.f32()?,
            contrast: self.f32()?,
            brightness: self.f32()?,
            vibrancy: self.f32()?,
            vibrancy_darkness: self.f32()?,
        })
    }
}
