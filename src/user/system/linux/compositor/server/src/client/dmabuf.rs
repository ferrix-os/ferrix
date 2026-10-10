//! `zwp_linux_dmabuf_v1`: a buffer that lives on the GPU.
//!
//! A client that draws on the GPU has its frame in a buffer object there,
//! and hands the compositor a descriptor for it -- a dmabuf -- rather than
//! shared memory. The compositor imports it into its own open of the render
//! node and samples it where it lies (`docs/GPU.md` §3.13).
//!
//! What is here is the protocol's bookkeeping: a `zwp_linux_buffer_params_v1`
//! gathers one plane, `create` or `create_immed` checks it against the
//! protocol's own errors, and the compositor above is told to import it
//! ([`Event::DmabufCreated`]). The import is the binary's, since it is a
//! system call, and its answer comes back through
//! [`Client::dmabuf_imported`]: a `create` is answered `created` or `failed`
//! only then, and a `create_immed` whose buffer cannot be imported ends the
//! connection with `invalid_wl_buffer`, as the protocol allows.
//!
//! # Descriptors
//!
//! Every `add` takes a descriptor off the connection, and the binary has to
//! claim it whatever becomes of the request -- an unclaimed one would be
//! read again as the next message's. So each `add` is reported at once
//! ([`Event::DmabufPlane`]) and the binary holds the descriptor from then
//! on: it imports it when the buffer is made, and closes it when the
//! parameters go without one.
//!
//! # What is taken
//!
//! One plane, `ARGB8888` or `XRGB8888`, linear: the buffers a virgl
//! resource is, and the ones `src/user/system/linux/compositor/gbm` makes.
//! The modifier may also be `DRM_FORMAT_MOD_INVALID`, the implicit layout,
//! which is linear here. No flags: a buffer drawn upside down or interlaced
//! is refused rather than shown wrong. Version 3 only: version 4's feedback
//! -- the format table and the main device -- comes with Mesa (§3a), its
//! first reader.
//!
//! # What is offered
//!
//! The modifiers announced are the ones the compositor above can take in,
//! which depends on how it takes a buffer in ([`Taken`],
//! [`modifiers_offered`]) and is said with the globals
//! ([`crate::Globals::offer_dmabuf_modifiers`]). A modifier that is neither
//! offered nor one of the two above is the protocol's `invalid_format`.

use compositor_protocol::core;
use compositor_protocol::linux_dmabuf::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use compositor_wire::{Arg, ArgType, ObjectId};

use crate::client::{Client, Event, Fatal};
use crate::role::Role;
use crate::shm::{Buffer, Format, PoolKey};

/// The DRM format code `ARGB8888`: `fourcc_code('A', 'R', '2', '4')`.
pub const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;

/// The DRM format code `XRGB8888`: `fourcc_code('X', 'R', '2', '4')`.
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// `DRM_FORMAT_MOD_LINEAR`: rows one after another.
pub const MOD_LINEAR: u64 = 0;

/// `DRM_FORMAT_MOD_INVALID`: no modifier said, the driver's implicit
/// layout -- linear, for a virgl resource seen from the guest.
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// The most modifiers offered with a format: more than any driver's EGL
/// reports for one, and a bound on what a bind sends.
pub const MODIFIERS_OFFERED_MOST: usize = 32;

/// A modifier a GPU renderer's EGL says it imports a format with
/// (`EGL_EXT_image_dma_buf_import_modifiers`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reported {
    /// The DRM format modifier.
    pub modifier: u64,
    /// Whether it is imported for external textures only, which a
    /// renderer that samples 2D textures cannot use.
    pub external_only: bool,
}

/// How the compositor above takes a client's buffer in, which is what
/// decides the modifiers it may offer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Taken<'a> {
    /// Into its own open of the render node its frames are drawn through
    /// (virgl): linear, and the implicit layout, which is linear there.
    Node,
    /// Mapped and read by the processor: linear only. The implicit layout
    /// goes unannounced, because NVIDIA's GBM backend answers "no modifier"
    /// with block-linear video memory, which cannot be mapped (N3b,
    /// `docs/NVIDIA.md` §4.6).
    Mapped,
    /// Sampled by a GPU renderer whose EGL reported these modifiers for
    /// every format offered: each one it imports as a 2D texture, and
    /// linear, which is mapped where it can be. Never the
    /// implicit layout: what a driver makes of "no modifier" is a layout
    /// nobody named, and here one EGL may not have reported.
    Sampled(&'a [Reported]),
}

/// The modifiers to offer with every format, in the order they are
/// announced, for a compositor that takes buffers in as `taken` says.
#[must_use]
pub fn modifiers_offered(taken: Taken<'_>) -> Vec<u64> {
    match taken {
        Taken::Node => vec![MOD_LINEAR, MOD_INVALID],
        Taken::Mapped => vec![MOD_LINEAR],
        Taken::Sampled(reported) => {
            let mut offered: Vec<u64> = Vec::new();
            for found in reported {
                let usable = !found.external_only
                    && found.modifier != MOD_INVALID
                    && found.modifier != MOD_LINEAR
                    && !offered.contains(&found.modifier);
                if usable && offered.len() < MODIFIERS_OFFERED_MOST - 1 {
                    offered.push(found.modifier);
                }
            }
            // Last: a driver choosing among them takes its own layout
            // first where it reads this as an order of preference.
            offered.push(MOD_LINEAR);
            offered
        }
    }
}

/// The formats offered, each with the modifiers it is offered with.
const OFFERED: [(u32, Format); 2] = [
    (DRM_FORMAT_ARGB8888, Format::Argb8888),
    (DRM_FORMAT_XRGB8888, Format::Xrgb8888),
];

/// The format a DRM code names, if it is one offered.
#[must_use]
pub fn format_of(fourcc: u32) -> Option<Format> {
    OFFERED
        .iter()
        .find(|(code, _)| *code == fourcc)
        .map(|(_, format)| *format)
}

/// What a buffer's one plane is, as `add` said it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plane {
    /// Where the first row starts in the buffer object.
    pub offset: u32,
    /// Bytes from one row's start to the next.
    pub stride: u32,
    /// Its layout.
    pub modifier: u64,
}

/// A dmabuf the compositor above is to import, as `create` described it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dmabuf {
    /// The parameters object it was made from, by which the descriptor
    /// [`Event::DmabufPlane`] handed over is known.
    pub params: ObjectId,
    /// The key its `wl_buffer` will name, as a pool's: the import is the
    /// buffer's memory, and goes when the buffer does.
    pub pool: PoolKey,
    /// In pixels.
    pub width: i32,
    /// In pixels.
    pub height: i32,
    /// What the pixels are.
    pub format: Format,
    /// Its one plane.
    pub plane: Plane,
}

/// One `zwp_linux_buffer_params_v1`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) struct Params {
    /// The plane, once `add` has said it.
    plane: Option<Plane>,
    /// Whether `create` or `create_immed` has been asked: a params object
    /// makes one buffer.
    used: bool,
    /// The buffer waiting for its import to be answered: the `wl_buffer`
    /// `create_immed` named, or `None` for a `create` whose `wl_buffer` is
    /// made only once the import worked.
    waiting: Option<(Option<ObjectId>, Buffer)>,
}

impl Client {
    /// `zwp_linux_dmabuf_v1`: `create_params`; `destroy` is its destructor,
    /// and the feedback requests are version 4's, which is not offered.
    pub(super) fn linux_dmabuf(&mut self, version: u32, opcode: u16, args: &[Arg<'_>]) {
        if opcode != zwp_linux_dmabuf_v1::request::CREATE_PARAMS {
            return;
        }
        let Some(id) = args.first().and_then(Arg::as_object) else {
            return;
        };
        if self.make(
            id,
            &compositor_protocol::linux_dmabuf::ZWP_LINUX_BUFFER_PARAMS_V1,
            version,
            Role::BufferParams,
        ) {
            let _ = self.params.insert(id, Params::default());
        }
    }

    /// What a bound `zwp_linux_dmabuf_v1` is told at once: every format and
    /// the modifiers it is offered with. Version 3 sends both events, as
    /// the protocol has it below version 4.
    pub(super) fn announce_dmabuf(&mut self, id: ObjectId, version: u32) {
        for (code, _) in OFFERED {
            let _ = self.out.write(
                id,
                zwp_linux_dmabuf_v1::event::FORMAT,
                &[ArgType::Uint],
                &[Arg::Uint(code)],
            );
            if version < 3 {
                continue;
            }
            for &modifier in self.globals.dmabuf_modifiers() {
                let (high, low) = halves(modifier);
                let _ = self.out.write(
                    id,
                    zwp_linux_dmabuf_v1::event::MODIFIER,
                    &[ArgType::Uint, ArgType::Uint, ArgType::Uint],
                    &[Arg::Uint(code), Arg::Uint(high), Arg::Uint(low)],
                );
            }
        }
    }

    /// `zwp_linux_buffer_params_v1`: `add`, `create` and `create_immed`.
    pub(super) fn buffer_params(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            zwp_linux_buffer_params_v1::request::ADD => self.params_add(sender, args),
            zwp_linux_buffer_params_v1::request::CREATE => {
                if let Some(shape) = shape(args, 0) {
                    self.params_create(sender, None, shape);
                }
            }
            zwp_linux_buffer_params_v1::request::CREATE_IMMED => {
                let (Some(id), Some(shape)) =
                    (args.first().and_then(Arg::as_object), shape(args, 1))
                else {
                    return;
                };
                self.params_create(sender, Some(id), shape);
            }
            _ => {}
        }
    }

    /// `add`: the one plane.
    fn params_add(&mut self, sender: ObjectId, args: &[Arg<'_>]) {
        let Some(fd) = args.first().and_then(Arg::as_fd) else {
            return;
        };
        // The descriptor is off the connection whatever is said next, so
        // the binary is handed it before anything can refuse the request.
        self.events.push(Event::DmabufPlane { params: sender, fd });
        let words: Vec<u32> = args.iter().skip(1).filter_map(Arg::as_uint).collect();
        let [plane_idx, offset, stride, high, low] = words.as_slice() else {
            return;
        };
        let Some(params) = self.params.get(&sender).copied() else {
            return;
        };
        let refuse = |code, text: &str| Fatal::Interface {
            object: sender,
            code,
            text: text.to_owned(),
        };
        let reason = if params.used {
            Some(refuse(
                zwp_linux_buffer_params_v1::error::ALREADY_USED,
                "these parameters have made their buffer already",
            ))
        } else if *plane_idx != 0 {
            Some(refuse(
                zwp_linux_buffer_params_v1::error::PLANE_IDX,
                "one plane is taken: plane 0",
            ))
        } else if params.plane.is_some() {
            Some(refuse(
                zwp_linux_buffer_params_v1::error::PLANE_SET,
                "plane 0 is set already",
            ))
        } else {
            None
        };
        if let Some(reason) = reason {
            self.fail(reason);
            return;
        }
        if let Some(held) = self.params.get_mut(&sender) {
            held.plane = Some(Plane {
                offset: *offset,
                stride: *stride,
                modifier: (u64::from(*high) << 32) | u64::from(*low),
            });
        }
    }

    /// `create` (`immed` is `None`) and `create_immed`: check the buffer
    /// against the protocol's errors and ask the binary to import it.
    fn params_create(&mut self, sender: ObjectId, immed: Option<ObjectId>, shape: Shape) {
        let Shape {
            width,
            height,
            fourcc,
            flags,
        } = shape;
        let Some(params) = self.params.get(&sender).copied() else {
            return;
        };
        let refuse = |code, text: String| Fatal::Interface {
            object: sender,
            code,
            text,
        };
        if params.used {
            self.fail(refuse(
                zwp_linux_buffer_params_v1::error::ALREADY_USED,
                "these parameters have made their buffer already".to_owned(),
            ));
            return;
        }
        if let Some(held) = self.params.get_mut(&sender) {
            held.used = true;
        }
        let Some(plane) = params.plane else {
            self.fail(refuse(
                zwp_linux_buffer_params_v1::error::INCOMPLETE,
                "no plane was added".to_owned(),
            ));
            return;
        };
        let Some(format) = format_of(fourcc) else {
            self.fail(refuse(
                zwp_linux_buffer_params_v1::error::INVALID_FORMAT,
                format!("format 0x{fourcc:08x} is not one this compositor offers"),
            ));
            return;
        };
        // Linear and the implicit layout are taken whether or not they
        // were announced, as before there was anything else to announce:
        // the import says whether such a buffer is one that can be shown.
        if plane.modifier != MOD_LINEAR
            && plane.modifier != MOD_INVALID
            && !self.globals.dmabuf_modifiers().contains(&plane.modifier)
        {
            self.fail(refuse(
                zwp_linux_buffer_params_v1::error::INVALID_FORMAT,
                format!(
                    "modifier 0x{:016x} is not one this compositor offers",
                    plane.modifier
                ),
            ));
            return;
        }
        if width <= 0 || height <= 0 {
            self.fail(refuse(
                zwp_linux_buffer_params_v1::error::INVALID_DIMENSIONS,
                format!("a buffer of {width}x{height}"),
            ));
            return;
        }
        // The rows fit their stride and the whole fits the arithmetic; that
        // they fit the buffer object is the binary's to say, since only the
        // import knows how large it is.
        let row = width.checked_mul(Format::BYTES);
        let (offset, stride) = (i32::try_from(plane.offset), i32::try_from(plane.stride));
        let fits = match (row, offset, stride) {
            (Some(row), Ok(offset), Ok(stride)) => {
                stride >= row
                    && stride
                        .checked_mul(height)
                        .and_then(|bytes| bytes.checked_add(offset))
                        .is_some()
            }
            _ => false,
        };
        let (Ok(offset), Ok(stride), true) = (offset, stride, fits) else {
            self.fail(refuse(
                zwp_linux_buffer_params_v1::error::OUT_OF_BOUNDS,
                "the plane's offset and stride do not fit its rows".to_owned(),
            ));
            return;
        };
        // A buffer drawn upside down, interlaced or bottom field first is
        // not one this compositor shows right, so it is not one it takes.
        if flags != 0 {
            match immed {
                Some(_) => self.fail(refuse(
                    zwp_linux_buffer_params_v1::error::INVALID_WL_BUFFER,
                    format!("flags 0x{flags:x} are not taken"),
                )),
                None => self.params_failed(sender),
            }
            return;
        }
        self.pools_made += 1;
        let pool = PoolKey(self.pools_made);
        let buffer = Buffer {
            pool,
            offset,
            width,
            height,
            stride,
            format,
            solid: None,
            dmabuf: true,
        };
        if let Some(id) = immed {
            if !self.make(id, &core::WL_BUFFER, 1, Role::Buffer) {
                return;
            }
            let _ = self.buffers.insert(id, buffer);
        }
        if let Some(held) = self.params.get_mut(&sender) {
            held.waiting = Some((immed, buffer));
        }
        self.events.push(Event::DmabufCreated {
            dmabuf: Dmabuf {
                params: sender,
                pool,
                width,
                height,
                format,
                plane,
            },
        });
    }

    /// Tell a `create` it failed.
    fn params_failed(&mut self, params: ObjectId) {
        let _ = self
            .out
            .write(params, zwp_linux_buffer_params_v1::event::FAILED, &[], &[]);
    }

    /// The binary's answer to [`Event::DmabufCreated`]: whether the buffer
    /// could be imported.
    ///
    /// A `create` is answered now: `created` with a `wl_buffer` the server
    /// makes, or `failed`. A `create_immed`'s buffer exists already, and one
    /// that cannot be imported ends the connection with
    /// `invalid_wl_buffer`, which the protocol allows "at the time of
    /// buffer use"; this is the earliest that is.
    pub fn dmabuf_imported(&mut self, params: ObjectId, imported: bool) {
        let Some((immed, buffer)) = self
            .params
            .get_mut(&params)
            .and_then(|held| held.waiting.take())
        else {
            return;
        };
        match (immed, imported) {
            (Some(_), true) => {}
            (Some(id), false) => self.fail(Fatal::Interface {
                object: id,
                code: zwp_linux_buffer_params_v1::error::INVALID_WL_BUFFER,
                text: "the buffer could not be imported".to_owned(),
            }),
            (None, true) => {
                let Ok(id) = self.objects.create(&core::WL_BUFFER, 1, Role::Buffer) else {
                    return;
                };
                let _ = self.buffers.insert(id, buffer);
                let _ = self.out.write(
                    params,
                    zwp_linux_buffer_params_v1::event::CREATED,
                    &[ArgType::NewId],
                    &[Arg::NewId(id)],
                );
            }
            (None, false) => self.params_failed(params),
        }
    }
}

/// What a `create` says after its new id, if anything: width and height,
/// which are `int` on the wire, then format and flags, which are `uint`.
#[derive(Clone, Copy, Debug)]
struct Shape {
    width: i32,
    height: i32,
    fourcc: u32,
    flags: u32,
}

/// [`Shape`] from a `create`'s arguments, `skip` of them in.
fn shape(args: &[Arg<'_>], skip: usize) -> Option<Shape> {
    Some(Shape {
        width: args.get(skip)?.as_int()?,
        height: args.get(skip + 1)?.as_int()?,
        fourcc: args.get(skip + 2)?.as_uint()?,
        flags: args.get(skip + 3)?.as_uint()?,
    })
}

/// A 64-bit modifier as the two words `zwp_linux_dmabuf_v1.modifier` sends.
fn halves(modifier: u64) -> (u32, u32) {
    let high = u32::try_from(modifier >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(modifier & 0xffff_ffff).unwrap_or(u32::MAX);
    (high, low)
}
