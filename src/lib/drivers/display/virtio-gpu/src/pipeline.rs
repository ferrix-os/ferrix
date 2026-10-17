//! From the display core's requests to device commands, and back.
//!
//! Each request the core sends becomes a short sequence of steps, run one at
//! a time:
//!
//! | Request | Steps | Reply |
//! |---|---|---|
//! | ATTACH | pin the range, `RESOURCE_CREATE_2D`, `RESOURCE_ATTACH_BACKING` | ATTACHED |
//! | SCANOUT | `SET_SCANOUT` | none |
//! | FLUSH | `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH` | FLIPPED |
//! | CURSOR | `TRANSFER_TO_HOST_2D`, then `UPDATE_CURSOR` on the cursor queue | FLIPPED |
//! | DETACH | `RESOURCE_DETACH_BACKING`, `RESOURCE_UNREF`, unpin | DETACHED |
//!
//! MOVE is not a request here: it waits for nothing on the control queue,
//! so the glue hands it to the driver's cursor queue the moment it is read,
//! and a CURSOR's place is taken the same way. What the pipeline keeps in
//! order is the image, which must be on the device before the cursor queue
//! is told to show it.
//!
//! A step that fails is undone as far as it safely can be: a resource created
//! and then refused its backing is unreferenced and its range unpinned. One
//! rule is never bent: pages the device may still hold as backing are not
//! unpinned. If the device refuses `RESOURCE_DETACH_BACKING`, the range stays
//! pinned for good and DETACHED says the device refused, which tells the core
//! never to hand the range out again.
//!
//! The pipeline does nothing itself. [`Pipeline::next`] says what the glue is
//! to do — pin, submit a command, unpin, reply, or wait — and the glue reports
//! back with [`Pipeline::pinned`] and [`Pipeline::done`].

use ferrix_displayctl::message::{
    Attach, AttachObject, BYTES_PER_PIXEL, Message, Rect as CtlRect, Status,
};
use ferrix_virtio::gpu::{Command, DeviceError as Refusal, Format, MemEntry, Rect, Response};

/// Requests waiting behind the one being run.
pub const QUEUE_DEPTH: usize = 16;

/// Buffers the pipeline remembers the geometry of.
pub const MAX_BUFFERS: usize = 32;

/// A request from the display core.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// ATTACH.
    Attach(Attach),
    /// `ATTACH_OBJ`: a buffer whose pixels the device holds already.
    AttachObject(AttachObject),
    /// SCANOUT.
    Scanout {
        /// The scanout.
        scanout: u32,
        /// The buffer, or 0 to turn it off.
        buffer: u32,
        /// What to show.
        rect: CtlRect,
    },
    /// FLUSH.
    Flush {
        /// The buffer.
        buffer: u32,
        /// The flush's sequence number.
        sequence: u64,
        /// What changed.
        rect: CtlRect,
    },
    /// DETACH.
    Detach {
        /// The buffer.
        buffer: u32,
    },
    /// CURSOR, less its place, which the glue gave the driver when the
    /// message was read.
    Cursor {
        /// The scanout.
        scanout: u32,
        /// The buffer, or 0 for none.
        buffer: u32,
        /// Its number in the flushes' line.
        sequence: u64,
        /// The hotspot's column.
        hot_x: u32,
        /// The hotspot's row.
        hot_y: u32,
    },
}

impl Request {
    /// The request a core message carries, if it carries one.
    #[must_use]
    pub const fn from_message(message: &Message) -> Option<Self> {
        Some(match *message {
            Message::Attach(attach) => Self::Attach(attach),
            Message::AttachObject(attach) => Self::AttachObject(attach),
            Message::Scanout {
                scanout,
                buffer,
                rect,
            } => Self::Scanout {
                scanout,
                buffer,
                rect,
            },
            Message::Flush {
                buffer,
                sequence,
                rect,
            } => Self::Flush {
                buffer,
                sequence,
                rect,
            },
            Message::Detach { buffer } => Self::Detach { buffer },
            Message::Cursor {
                scanout,
                buffer,
                sequence,
                hot_x,
                hot_y,
                ..
            } => Self::Cursor {
                scanout,
                buffer,
                sequence,
                hot_x,
                hot_y,
            },
            _ => return None,
        })
    }

    const fn buffer(&self) -> u32 {
        match *self {
            Self::Attach(attach) => attach.buffer,
            Self::AttachObject(attach) => attach.buffer,
            Self::Scanout { buffer, .. }
            | Self::Flush { buffer, .. }
            | Self::Detach { buffer }
            | Self::Cursor { buffer, .. } => buffer,
        }
    }
}

/// What the glue does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "a Reply carries a display protocol message, which HELLO's timings make 600 \
              bytes; a step lives on the driver's stack until it is acted on"
)]
pub enum Step<'e> {
    /// Pin `length` bytes of the buffer's VMO from `offset`, read-only, turn the
    /// pages' device addresses into backing entries, and report with
    /// [`Pipeline::pinned`].
    Pin {
        /// The buffer the pin is for, which [`Step::Unpin`] names later.
        buffer: u32,
        /// Where its range starts.
        offset: u64,
        /// Its length.
        length: u64,
    },
    /// Submit this command and report its outcome with [`Pipeline::done`].
    Submit(Command<'e>),
    /// Close the buffer's pin, then ask again.
    Unpin {
        /// The buffer.
        buffer: u32,
    },
    /// Show `resource` -- 0 for none -- as `scanout`'s cursor with its
    /// hotspot at (`hot_x`, `hot_y`), on the cursor queue, then ask again.
    /// Nothing is waited for: the image is on the device already.
    Cursor {
        /// The scanout.
        scanout: u32,
        /// The resource.
        resource: u32,
        /// The hotspot's column.
        hot_x: u32,
        /// The hotspot's row.
        hot_y: u32,
    },
    /// Send this to the display core, then ask again.
    Reply(Message),
    /// A pin or command is outstanding; report it first.
    Wait,
    /// Nothing to do until another request arrives.
    Idle,
}

/// A report the pipeline was not waiting for, or a request it has no room
/// for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PipelineError {
    /// [`QUEUE_DEPTH`] requests are already waiting.
    Full,
    /// Nothing of that kind is outstanding.
    NotWaiting,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Pin,
    Create,
    Back,
    UnrefAfterBack(Status),
    UnpinThenReply(Status),
    ReplyAttached(Status),
    SetScanout,
    Transfer,
    FlushResource,
    CursorTransfer,
    ShowCursor,
    ReplyFlipped(Status),
    DetachBacking,
    UnrefDetach,
    UnpinDetach(Status),
    ReplyDetached(Status),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Op {
    request: Request,
    stage: Stage,
    waiting: bool,
    entries: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Geometry {
    buffer: u32,
    /// What the *device* calls the buffer's pixels. The driver makes a
    /// resource of its own for an ATTACH and numbers it by the buffer id;
    /// an `ATTACH_OBJ`'s resource was made through the render conversation
    /// and is named by its object id, which is why the two are not one
    /// number.
    resource: u32,
    width: u32,
    height: u32,
    stride: u32,
    /// Whether the driver made the resource, and so has to unref it when
    /// the buffer is detached. An object's belongs to the renderer.
    own: bool,
}

/// The requests in hand and the one being run.
#[derive(Clone, Debug)]
pub struct Pipeline {
    queue: [Option<Request>; QUEUE_DEPTH],
    head: usize,
    count: usize,
    current: Option<Op>,
    buffers: [Option<Geometry>; MAX_BUFFERS],
    refused_scanouts: u64,
    transfer_offset: TransferOffset,
}

/// How the device reads a 2D transfer's `offset`, which the two machines
/// Ferrix's GPU runs under disagree about.
///
/// virtio 1.2 §5.7.6.8 makes it the offset into the resource's backing, and
/// QEMU and Linux's driver read it as where the rectangle starts there:
/// `y * stride + x * 4`. crosvm adds the rectangle's origin to it again
/// (`transfer_2d` in `rutabaga_2d.rs`), so the same transfer copies from
/// `x` and `y` further on, and every partial update lands shifted up and
/// left by its own origin -- 20 pixels on the Pixel 7's VM, the
/// compositor's gap (2026-09-26). crosvm's reading needs an offset of 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TransferOffset {
    /// Where the rectangle starts in the backing: the specification's,
    /// QEMU's and Linux's.
    #[default]
    RectangleStart,
    /// The backing's start, with the device adding the rectangle's origin
    /// itself: crosvm's.
    BackingStart,
}

impl TransferOffset {
    /// crosvm's virtio devices carry their own device identifier again as
    /// their PCI subsystem, `0x1af4:0x1050` for the GPU; QEMU's carry
    /// `0x1af4:0x1100`. So a GPU whose subsystem repeats its identifier is
    /// crosvm's, and any other reads the offset as the specification says.
    #[must_use]
    pub const fn for_device(device_id: u16, subsystem_vendor: u16, subsystem: u16) -> Self {
        if subsystem_vendor == VIRTIO_VENDOR && subsystem == device_id {
            TransferOffset::BackingStart
        } else {
            TransferOffset::RectangleStart
        }
    }
}

/// The PCI vendor every virtio device carries, and crosvm as its subsystem
/// vendor.
const VIRTIO_VENDOR: u16 = 0x1af4;

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// The DETACHED or ATTACHED status a device refusal becomes.
const fn status_of(refusal: Refusal) -> Status {
    match refusal {
        Refusal::OutOfMemory => Status::OutOfMemory,
        _ => Status::DeviceRefused,
    }
}

const fn rect(rect: CtlRect) -> Rect {
    Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

impl Pipeline {
    /// An empty pipeline.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: [None; QUEUE_DEPTH],
            head: 0,
            count: 0,
            current: None,
            buffers: [None; MAX_BUFFERS],
            refused_scanouts: 0,
            transfer_offset: TransferOffset::RectangleStart,
        }
    }

    /// An empty pipeline for a device that reads a transfer's offset as
    /// `transfer_offset` says.
    #[must_use]
    pub const fn for_device(transfer_offset: TransferOffset) -> Self {
        let mut pipeline = Self::new();
        pipeline.transfer_offset = transfer_offset;
        pipeline
    }

    /// How many `SET_SCANOUT` commands the device refused, which have no reply
    /// to carry them.
    #[must_use]
    pub const fn refused_scanouts(&self) -> u64 {
        self.refused_scanouts
    }

    /// Whether nothing is running or waiting.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.current.is_none() && self.count == 0
    }

    /// Whether [`QUEUE_DEPTH`] requests are waiting, so a push would fail.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.count >= QUEUE_DEPTH
    }

    /// Queue a request.
    pub fn push(&mut self, request: Request) -> Result<(), PipelineError> {
        if self.count >= QUEUE_DEPTH {
            return Err(PipelineError::Full);
        }
        let slot = self
            .queue
            .get_mut((self.head + self.count) % QUEUE_DEPTH)
            .ok_or(PipelineError::Full)?;
        *slot = Some(request);
        self.count += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<Request> {
        if self.count == 0 {
            return None;
        }
        let request = self.queue.get_mut(self.head).and_then(Option::take);
        self.head = (self.head + 1) % QUEUE_DEPTH;
        self.count -= 1;
        request
    }

    fn geometry(&self, buffer: u32) -> Option<Geometry> {
        self.buffers
            .iter()
            .flatten()
            .find(|geometry| geometry.buffer == buffer)
            .copied()
    }

    fn remember(&mut self, geometry: Geometry) {
        if let Some(slot) = self.buffers.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(geometry);
        }
    }

    /// What the device calls `buffer`'s pixels.
    ///
    /// A buffer not remembered yet is one being attached, which is the
    /// driver's own resource and numbered by the buffer id.
    fn resource(&self, buffer: u32) -> u32 {
        self.geometry(buffer).map_or(buffer, |held| held.resource)
    }

    /// Whether the driver made `buffer`'s resource, and so owes the device
    /// the commands that take it away.
    ///
    /// A buffer nothing was written down for is one of the driver's own:
    /// that is what every buffer was before `ATTACH_OBJ` existed, and a
    /// DETACH of a buffer whose ATTACH never finished still owes the
    /// device its half of the undoing.
    fn own(&self, buffer: u32) -> bool {
        self.geometry(buffer).is_none_or(|held| held.own)
    }

    fn forget(&mut self, buffer: u32) {
        for slot in &mut self.buffers {
            if slot.is_some_and(|geometry| geometry.buffer == buffer) {
                *slot = None;
            }
        }
    }

    /// Where a request starts.
    ///
    /// A buffer made of an object the render conversation holds costs the
    /// device nothing to make, nothing to send when it changes and nothing
    /// to take away: its pixels are the device's already, which is the
    /// whole reason for the second way of making one.
    fn first(&self, request: Request) -> Stage {
        match request {
            Request::Attach(_) => Stage::Pin,
            Request::AttachObject(_) => Stage::ReplyAttached(Status::Ok),
            Request::Scanout { .. } => Stage::SetScanout,
            Request::Flush { buffer, .. } if self.own(buffer) => Stage::Transfer,
            Request::Flush { .. } => Stage::FlushResource,
            Request::Detach { buffer } if self.own(buffer) => Stage::DetachBacking,
            Request::Detach { .. } => Stage::ReplyDetached(Status::Ok),
            Request::Cursor { buffer, .. } if buffer != 0 && self.own(buffer) => {
                Stage::CursorTransfer
            }
            Request::Cursor { .. } => Stage::ShowCursor,
        }
    }

    /// What to do next. `entries` is where the glue put the backing entries
    /// of the pin it last reported; only the count [`Pipeline::pinned`] was
    /// given is used.
    pub fn next<'e>(&mut self, entries: &'e [MemEntry]) -> Step<'e> {
        if self.current.is_none() {
            let Some(request) = self.pop() else {
                return Step::Idle;
            };
            self.current = Some(Op {
                request,
                stage: self.first(request),
                waiting: false,
                entries: 0,
            });
        }
        let Some(mut op) = self.current else {
            return Step::Idle;
        };
        if op.waiting {
            return Step::Wait;
        }
        let buffer = op.request.buffer();
        let step = match (op.request, op.stage) {
            (Request::Attach(attach), Stage::Pin) => Step::Pin {
                buffer,
                offset: attach.offset,
                length: attach.length,
            },
            (_, Stage::UnpinThenReply(status)) => {
                op.stage = Stage::ReplyAttached(status);
                self.current = Some(op);
                return Step::Unpin { buffer };
            }
            (_, Stage::UnpinDetach(status)) => {
                op.stage = Stage::ReplyDetached(status);
                self.current = Some(op);
                self.forget(buffer);
                return Step::Unpin { buffer };
            }
            (Request::Attach(attach), Stage::ReplyAttached(status)) => {
                self.current = None;
                if status == Status::Ok {
                    self.remember(Geometry {
                        buffer: attach.buffer,
                        resource: attach.buffer,
                        width: attach.width,
                        height: attach.height,
                        stride: attach.stride,
                        own: true,
                    });
                }
                return Step::Reply(Message::Attached { buffer, status });
            }
            (Request::AttachObject(attach), Stage::ReplyAttached(status)) => {
                self.current = None;
                if status == Status::Ok {
                    self.remember(Geometry {
                        buffer: attach.buffer,
                        resource: attach.object,
                        width: attach.width,
                        height: attach.height,
                        stride: attach.stride,
                        own: false,
                    });
                }
                return Step::Reply(Message::Attached { buffer, status });
            }
            (
                Request::Flush { sequence, .. } | Request::Cursor { sequence, .. },
                Stage::ReplyFlipped(status),
            ) => {
                self.current = None;
                return Step::Reply(Message::Flipped { sequence, status });
            }
            (Request::Cursor { .. }, Stage::ShowCursor) => return self.show_cursor(op),
            (_, Stage::ReplyDetached(status)) => {
                self.current = None;
                self.forget(buffer);
                return Step::Reply(Message::Detached { buffer, status });
            }
            _ => match self.command(&op, entries) {
                Some(command) => Step::Submit(command),
                // Every request starts at a stage of its own kind and only
                // moves to stages of that kind, so this is not reached.
                None => {
                    self.current = None;
                    return Step::Idle;
                }
            },
        };
        op.waiting = true;
        self.current = Some(op);
        step
    }

    /// A cursor whose image is on the device: show it, and answer next.
    fn show_cursor<'e>(&mut self, mut op: Op) -> Step<'e> {
        let Request::Cursor {
            scanout,
            buffer,
            hot_x,
            hot_y,
            ..
        } = op.request
        else {
            self.current = None;
            return Step::Idle;
        };
        op.stage = Stage::ReplyFlipped(Status::Ok);
        self.current = Some(op);
        Step::Cursor {
            scanout,
            resource: if buffer == 0 {
                0
            } else {
                self.resource(buffer)
            },
            hot_x,
            hot_y,
        }
    }

    /// The device command `op`'s stage submits, if it submits one.
    fn command<'e>(&self, op: &Op, entries: &'e [MemEntry]) -> Option<Command<'e>> {
        let resource_id = self.resource(op.request.buffer());
        Some(match (op.request, op.stage) {
            (Request::Attach(attach), Stage::Create) => Command::ResourceCreate2d {
                resource_id,
                format: Format::B8G8R8X8,
                width: attach.width,
                height: attach.height,
            },
            (Request::Attach(_), Stage::Back) => Command::ResourceAttachBacking {
                resource_id,
                entries: entries.get(..op.entries).unwrap_or(&[]),
            },
            (Request::Attach(_), Stage::UnrefAfterBack(_)) | (_, Stage::UnrefDetach) => {
                Command::ResourceUnref { resource_id }
            }
            (
                Request::Scanout {
                    scanout,
                    buffer,
                    rect: area,
                },
                Stage::SetScanout,
            ) => Command::SetScanout {
                rect: if buffer == 0 {
                    Rect::default()
                } else {
                    rect(area)
                },
                scanout_id: scanout,
                resource_id: if buffer == 0 {
                    0
                } else {
                    self.resource(buffer)
                },
            },
            (Request::Flush { rect: area, .. }, Stage::Transfer) => {
                let stride = self
                    .geometry(op.request.buffer())
                    .map_or(0, |geometry| geometry.stride);
                Command::TransferToHost2d {
                    rect: rect(area),
                    offset: match self.transfer_offset {
                        TransferOffset::RectangleStart => {
                            u64::from(area.y) * u64::from(stride)
                                + u64::from(area.x) * u64::from(BYTES_PER_PIXEL)
                        }
                        TransferOffset::BackingStart => 0,
                    },
                    resource_id,
                }
            }
            (Request::Flush { rect: area, .. }, Stage::FlushResource) => Command::ResourceFlush {
                rect: rect(area),
                resource_id,
            },
            // The whole image: a cursor is small, and a partial one is a
            // cursor with somebody else's pixels in it.
            (Request::Cursor { .. }, Stage::CursorTransfer) => {
                let geometry = self.geometry(op.request.buffer())?;
                Command::TransferToHost2d {
                    rect: Rect::sized(geometry.width, geometry.height),
                    offset: 0,
                    resource_id,
                }
            }
            (Request::Detach { .. }, Stage::DetachBacking) => {
                Command::ResourceDetachBacking { resource_id }
            }
            _ => return None,
        })
    }

    /// Report the pin [`Step::Pin`] asked for: how many backing entries it
    /// made, or that it failed.
    pub fn pinned(&mut self, result: Result<usize, ()>) -> Result<(), PipelineError> {
        let op = self
            .current
            .as_mut()
            .filter(|op| op.waiting && op.stage == Stage::Pin)
            .ok_or(PipelineError::NotWaiting)?;
        op.waiting = false;
        match result {
            Ok(count) if count > 0 => {
                op.entries = count;
                op.stage = Stage::Create;
            }
            _ => op.stage = Stage::ReplyAttached(Status::PinFailed),
        }
        Ok(())
    }

    /// Take back the [`Step::Submit`] just given, which the driver had no
    /// room for: the next [`Pipeline::next`] gives the same command again.
    pub fn unsent(&mut self) -> Result<(), PipelineError> {
        let op = self
            .current
            .as_mut()
            .filter(|op| op.waiting && op.stage != Stage::Pin)
            .ok_or(PipelineError::NotWaiting)?;
        op.waiting = false;
        Ok(())
    }

    /// Report the outcome of the command [`Step::Submit`] asked for.
    pub fn done(&mut self, result: Result<Response, Refusal>) -> Result<(), PipelineError> {
        let op = self
            .current
            .as_mut()
            .filter(|op| op.waiting && op.stage != Stage::Pin)
            .ok_or(PipelineError::NotWaiting)?;
        op.waiting = false;
        let ok = result.is_ok();
        let status = result.map_or_else(status_of, |_| Status::Ok);
        op.stage = match op.stage {
            Stage::Create if ok => Stage::Back,
            Stage::Create => Stage::UnpinThenReply(status),
            Stage::Back if ok => Stage::ReplyAttached(Status::Ok),
            Stage::Back => Stage::UnrefAfterBack(status),
            Stage::UnrefAfterBack(failure) => Stage::UnpinThenReply(failure),
            Stage::SetScanout => {
                if !ok {
                    self.refused_scanouts += 1;
                }
                self.current = None;
                return Ok(());
            }
            Stage::Transfer if ok => Stage::FlushResource,
            Stage::Transfer | Stage::FlushResource => Stage::ReplyFlipped(status),
            // A cursor whose pixels did not arrive is not shown: the reply
            // carries the refusal and the cursor queue hears nothing.
            Stage::CursorTransfer if ok => Stage::ShowCursor,
            Stage::CursorTransfer => Stage::ReplyFlipped(status),
            // Refused: the device may still hold the pages, so they stay
            // pinned and the reply says so.
            Stage::DetachBacking if ok => Stage::UnrefDetach,
            Stage::DetachBacking => Stage::ReplyDetached(status),
            Stage::UnrefDetach => Stage::UnpinDetach(status),
            other => other,
        };
        Ok(())
    }
}
