//! The messages, as bytes.
//!
//! Every message starts with its type and its length, four bytes each, and
//! has exactly the length its type fixes. Handles ride in the channel
//! message's handle array; [`Hello::validate`] and [`Ready::HANDLE_RIGHTS`]
//! say what they must be.
//!
//! ```text
//! HELLO      driver -> core, 48 bytes, handles [driver port]
//!   8 version u16   10 reserved u16   12 location u32
//!   16 name [u8; 16]   32 features u32   36 capsets u32 (a bit per id)
//!   40 object_max u64
//! READY      core -> driver, 24 bytes, handles [work VMO, core port]
//!   8 renderer u32   12 reserved   16 work_bytes u64
//! REFUSED    core -> driver, 12 bytes: 8 reason u32
//! MAKE_CTX   core -> driver, 16 bytes: 8 context u32   12 capset u32
//! CTX_MADE   driver -> core, 16 bytes: 8 context u32   12 status u32
//! DROP_CTX   core -> driver, 16 bytes: 8 context u32   12 reserved
//! CTX_GONE   driver -> core, 16 bytes: 8 context u32   12 status u32
//! MAKE_OBJ   core -> driver, 40 bytes, handles [backing VMO] when MAPPABLE
//!   8 object u32   12 context u32 (0: none)   16 bytes u64
//!   24 flags u32   28 reserved   32 describe_at u32   36 describe_len u32
//! OBJ_MADE   driver -> core, 16 bytes: 8 object u32   12 status u32
//! DROP_OBJ   core -> driver, 16 bytes: 8 object u32   12 reserved
//! OBJ_GONE   driver -> core, 16 bytes: 8 object u32   12 status u32
//! SUBMIT     core -> driver, 32 bytes
//!   8 context u32   12 ring u32 (NO_RING: none)   16 fence u64
//!   24 at u32   28 len u32
//! SUBMITTED  driver -> core, 24 bytes: 8 fence u64   16 status u32   20 reserved
//! WAIT       core -> driver, 24 bytes: 8 context u32   12 reserved   16 fence u64
//! WAITED     driver -> core, 24 bytes: 8 fence u64   16 status u32   20 reserved
//! TRANSFER   core -> driver, 64 bytes
//!   8 object u32   12 context u32 (0: none)   16 direction u32   20 level u32
//!   24 offset u64   32 x y z width height depth, u32 each
//!   56 stride u32   60 layer_stride u32
//! TRANSFERRED driver -> core, 16 bytes: 8 object u32   12 status u32
//! GET_CAPS   core -> driver, 16 bytes: 8 capset u32   12 version u32
//! CAPS       driver -> core, 24 bytes, handles [caps VMO] when status is Ok
//!   8 capset u32   12 status u32   16 len u32   20 reserved
//! MAKE_BLOB  core -> driver, 48 bytes
//!   8 object u32   12 context u32 (0: none)   16 memory u32   20 flags u32
//!   24 blob_id u64   32 bytes u64   40 window u64 (NO_WINDOW: not mapped)
//! BLOB_MADE  driver -> core, 24 bytes: 8 object u32   12 status u32
//!   16 map_info u32   20 reserved
//! ATTACH_OBJ core -> driver, 16 bytes: 8 object u32   12 context u32
//! OBJ_ATTACHED driver -> core, 16 bytes: 8 object u32   12 status u32
//! STOP, STOPPED                8 bytes
//! ```
//!
//! Reserved bytes are written as zero and a message with any of them set is
//! malformed, so they can be given a meaning later without an old reader
//! misreading them.
//!
//! # What crosses as bytes, and what does not
//!
//! Two fields name a range of the *work VMO* rather than carrying its
//! contents: a command buffer (`SUBMIT`) and an object's descriptor
//! (`MAKE_OBJ`). The core owns that VMO, hands out ranges of it, and never
//! reads what is written there; the driver pins the range and passes it to
//! the device. This is the seam `docs/GPU.md` §3.3 describes, and the
//! reason the core can stay the same for a second GPU: what those bytes
//! *mean* is the driver's business and no part of this protocol.
//!
//! An object's *backing* does not ride in the work VMO either. A mappable
//! object has a VMO of its own, which the core makes and keeps -- it is what
//! a program maps through the render node -- and hands to the driver with
//! `MAKE_OBJ`. The driver pins it for the device and keeps the pin until the
//! device has let the object go, which is `docs/DISPLAY.md` §2.2's rule once
//! more: a VMO the device may still hold is never unpinned. `TRANSFER` then
//! moves bytes between that backing and the device's own copy, and names
//! the part of the object by a box, a level and two strides, which is as
//! much as any GPU's texture has and no more than that.
//!
//! A capability set is the other way about: the *driver* read it from the
//! device, so `CAPS` brings a VMO of the driver's making, and the core
//! copies the bytes out and lets it go.
//!
//! # Blobs, and the window they are mapped through
//!
//! A *blob* is an object whose memory the device's side allocates -- Venus
//! keeps every host-visible Vulkan allocation and its command rings in one
//! (`docs/GPU.md` §6.1). It has no backing VMO: a program sees it through a
//! window of device memory, a PCI BAR the host maps blobs into. The window is
//! the kernel's, found when the device was enumerated, and the core hands out
//! places in it the way it hands out ranges of the work VMO. `MAKE_BLOB`
//! names the place and the driver asks its device to put the blob there;
//! `BLOB_MADE` says how the device wants it cached, which is how the core
//! maps it. What `memory`, `flags` and `blob_id` mean is the driver's --
//! virtio-gpu's blob kinds here -- as a description is.
//!
//! A submission that names a *ring* is fenced on it: `SUBMITTED` comes when
//! the work has finished, rather than when the device has taken it, which is
//! what a program waiting on a fence descriptor needs.
//!
//! # An object another context imports
//!
//! An object is made in one context and is nameable there. A program that
//! is handed it as a dmabuf imports it into its own open, and so into its
//! own context: `ATTACH_OBJ` asks the driver to make the object nameable in
//! that context too (`docs/GPU.md` §3.13). Nothing about the object's
//! backing changes. Attaching an object twice to one context is not an
//! error -- Linux's virtio-gpu attaches on every handle it opens -- so the
//! core need not remember which contexts an object was given to.

use ::core::fmt;

use ferrix_native_abi::rights::Rights;

/// The protocol version this crate speaks.
pub const VERSION: u16 = 4;

/// HELLO's type.
pub const HELLO: u32 = 1;
/// READY's type.
pub const READY: u32 = 2;
/// REFUSED's type.
pub const REFUSED: u32 = 3;
/// `MAKE_CTX`'s type.
pub const MAKE_CTX: u32 = 4;
/// `CTX_MADE`'s type.
pub const CTX_MADE: u32 = 5;
/// `DROP_CTX`'s type.
pub const DROP_CTX: u32 = 6;
/// `CTX_GONE`'s type.
pub const CTX_GONE: u32 = 7;
/// `MAKE_OBJ`'s type.
pub const MAKE_OBJ: u32 = 8;
/// `OBJ_MADE`'s type.
pub const OBJ_MADE: u32 = 9;
/// `DROP_OBJ`'s type.
pub const DROP_OBJ: u32 = 10;
/// `OBJ_GONE`'s type.
pub const OBJ_GONE: u32 = 11;
/// SUBMIT's type.
pub const SUBMIT: u32 = 12;
/// `SUBMITTED`'s type.
pub const SUBMITTED: u32 = 13;
/// WAIT's type.
pub const WAIT: u32 = 14;
/// WAITED's type.
pub const WAITED: u32 = 15;
/// STOP's type.
pub const STOP: u32 = 16;
/// STOPPED's type.
pub const STOPPED: u32 = 17;
/// TRANSFER's type.
pub const TRANSFER: u32 = 18;
/// `TRANSFERRED`'s type.
pub const TRANSFERRED: u32 = 19;
/// `GET_CAPS`'s type.
pub const GET_CAPS: u32 = 20;
/// CAPS's type.
pub const CAPS: u32 = 21;
/// `MAKE_BLOB`'s type.
pub const MAKE_BLOB: u32 = 22;
/// `BLOB_MADE`'s type.
pub const BLOB_MADE: u32 = 23;
/// `ATTACH_OBJ`'s type.
pub const ATTACH_OBJ: u32 = 24;
/// `OBJ_ATTACHED`'s type.
pub const OBJ_ATTACHED: u32 = 25;

/// Bytes of the type and length, and all of STOP and STOPPED.
pub const HEADER_BYTES: usize = 8;

/// Bytes of a driver's name in HELLO, padded with zeros.
///
/// The name is what `DRM_IOCTL_VERSION` reports and what userspace picks a
/// back end by: `virtio_gpu` today, `nvidia` if Path B is taken. It is the
/// one field in this protocol whose whole purpose is that there will be
/// more than one driver.
pub const NAME_BYTES: usize = 16;

/// Bytes of HELLO.
pub const HELLO_BYTES: usize = 48;
/// Bytes of READY.
pub const READY_BYTES: usize = 24;
/// Bytes of REFUSED.
pub const REFUSED_BYTES: usize = 12;
/// Bytes of `MAKE_OBJ`, the longest of the rest.
pub const MAKE_OBJ_BYTES: usize = 40;
/// Bytes of `SUBMIT`.
pub const SUBMIT_BYTES: usize = 32;
/// Bytes of `MAKE_BLOB`.
pub const MAKE_BLOB_BYTES: usize = 48;
/// Bytes of a message that is a pair of words after the header.
pub const PAIR_BYTES: usize = 16;
/// Bytes of a message carrying a fence and a status.
pub const FENCE_BYTES: usize = 24;
/// Bytes of `TRANSFER`.
pub const TRANSFER_BYTES: usize = 64;
/// Bytes of the longest message.
pub const MAX_BYTES: usize = TRANSFER_BYTES;

/// `SUBMIT`'s ring when it names none: the submission is answered when the
/// device has taken it.
pub const NO_RING: u32 = u32::MAX;

/// The most rings a context may have, which is what Linux's `CONTEXT_INIT`
/// lets a program ask for.
pub const MAX_RINGS: u32 = 64;

/// `MAKE_BLOB`'s window when the blob is not to be mapped.
pub const NO_WINDOW: u64 = u64::MAX;

/// Exactly the rights each side holds the other's port with.
pub const PORT_RIGHTS: Rights = Rights(Rights::WRITE.0 | Rights::TRANSFER.0);

/// Exactly the rights the driver holds the work VMO with: it reads what the
/// core wrote there and pins it for the device, and never writes it.
pub const WORK_VMO_RIGHTS: Rights = Rights(Rights::READ.0 | Rights::TRANSFER.0);

/// Exactly the rights the driver holds a mappable object's backing with:
/// it pins the pages for the device, which may write them on a transfer
/// from it, and never maps them.
pub const BACKING_RIGHTS: Rights = Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::TRANSFER.0);

/// The largest capability set a core takes from a driver: 64 KiB, which is
/// many times any renderer's and small enough to copy without thought.
pub const MAX_CAPS_BYTES: u32 = 64 * 1024;

/// The largest object a core will ask a driver to make: 256 MiB.
///
/// A bound rather than a promise. The driver says its own in HELLO's
/// `object_max` and the core takes the smaller, so a device with less is
/// believed and a device claiming more than this is not.
pub const MAX_OBJECT_BYTES: u64 = 256 * 1024 * 1024;

/// The largest command buffer or descriptor one message may name.
///
/// Both are ranges of the work VMO, and a range longer than the VMO is a
/// core asking for something it did not hand out.
pub const MAX_WORK_BYTES: u32 = 16 * 1024 * 1024;

/// `MAKE_OBJ`'s flags.
pub mod flags {
    /// The object may be mapped into a process through the render node.
    pub const MAPPABLE: u32 = 1 << 0;
    /// The object is written by the guest and read by the device, so its
    /// backing is pinned read-only.
    pub const TO_DEVICE: u32 = 1 << 1;
    /// The other way: the device writes it and the guest reads it.
    pub const FROM_DEVICE: u32 = 1 << 2;
    /// Every flag this version defines, for refusing the rest.
    pub const KNOWN: u32 = MAPPABLE | TO_DEVICE | FROM_DEVICE;
}

/// What a driver says it can do, in HELLO's `features`.
pub mod features {
    /// The driver takes command buffers at all. A driver without it is a
    /// scanout with a render node it cannot serve, which is refused.
    pub const SUBMIT: u32 = 1 << 0;
    /// Fences are real: `WAIT` returns when the work has finished rather
    /// than when the device has read the command.
    pub const FENCES: u32 = 1 << 1;
    /// The driver makes blobs, and maps a mappable one into the device's
    /// window where the core says.
    pub const BLOBS: u32 = 1 << 2;
    /// A submission may name a ring of its context, and is then answered
    /// when the work on it has finished.
    pub const RINGS: u32 = 1 << 3;
    /// Every feature this version defines.
    pub const KNOWN: u32 = SUBMIT | FENCES | BLOBS | RINGS;
}

/// `HELLO`: the driver introduces its device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hello {
    /// The protocol version.
    pub version: u16,
    /// The device's PCI location, as START gave it.
    pub location: u32,
    /// What the driver is called, zero-padded: `virtio_gpu`, `nvidia`.
    /// This is what `DRM_IOCTL_VERSION` reports and what userspace picks a
    /// back end by.
    pub name: [u8; NAME_BYTES],
    /// What it can do: [`features`].
    pub features: u32,
    /// Which capability sets a context may be made for: bit `n` for the set
    /// numbered `n`, as Linux's `VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS` says
    /// them. virtio-gpu's `CAPSET_VIRGL2` and `CAPSET_VENUS`, say; none for a
    /// device that has none. Bit 0 names no set and is never set.
    pub capsets: u32,
    /// The largest object it will make, in bytes.
    pub object_max: u64,
}

impl Hello {
    /// HELLO's handles, in order, with exactly the rights each must carry.
    pub const HANDLE_RIGHTS: [Rights; 1] = [PORT_RIGHTS];

    /// The name as text, up to its first zero.
    #[must_use]
    pub fn name(&self) -> &str {
        let end = self
            .name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(NAME_BYTES);
        ::core::str::from_utf8(self.name.get(..end).unwrap_or(&[])).unwrap_or("")
    }

    /// A name from text, zero-padded, or `None` if it does not fit.
    #[must_use]
    pub fn named(name: &str) -> Option<[u8; NAME_BYTES]> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > NAME_BYTES {
            return None;
        }
        let mut out = [0u8; NAME_BYTES];
        out.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(out)
    }

    /// Check a HELLO and the rights of the handles it came with.
    ///
    /// # Errors
    ///
    /// The reason to refuse the driver.
    pub fn validate(&self, handle_rights: &[Rights]) -> Result<(), Refusal> {
        if self.version != VERSION {
            return Err(Refusal::Version);
        }
        // A name is what userspace picks a back end by, so an empty one or
        // one that is not text is a driver nothing could ask for by name.
        if self.name.first() == Some(&0) {
            return Err(Refusal::Name);
        }
        let end = self
            .name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(NAME_BYTES);
        let text = self.name.get(..end).unwrap_or(&[]);
        if ::core::str::from_utf8(text).is_err()
            || !text
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            || self
                .name
                .get(end..)
                .is_some_and(|rest| rest.iter().any(|&byte| byte != 0))
        {
            return Err(Refusal::Name);
        }
        if self.features & !features::KNOWN != 0 {
            return Err(Refusal::Features);
        }
        // A render node exists to take command buffers. A driver that
        // cannot is one the core has nothing to publish for.
        if self.features & features::SUBMIT == 0 {
            return Err(Refusal::Features);
        }
        if self.object_max == 0 {
            return Err(Refusal::ObjectMax);
        }
        if self.capsets & 1 != 0 {
            return Err(Refusal::Capsets);
        }
        if handle_rights != Self::HANDLE_RIGHTS {
            return Err(Refusal::Rights);
        }
        Ok(())
    }

    /// The largest object the core will ask for: the smaller of what the
    /// driver said and what this protocol allows.
    #[must_use]
    pub const fn object_limit(&self) -> u64 {
        if self.object_max < MAX_OBJECT_BYTES {
            self.object_max
        } else {
            MAX_OBJECT_BYTES
        }
    }
}

/// `READY`: the core accepts the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ready {
    /// The renderer's number, `renderD<128 + N>`.
    pub renderer: u32,
    /// How many bytes the work VMO has.
    pub work_bytes: u64,
}

impl Ready {
    /// READY's handles, in order, with exactly the rights each must carry.
    pub const HANDLE_RIGHTS: [Rights; 2] = [WORK_VMO_RIGHTS, Rights(Rights::WRITE.0)];
}

/// Why the core refuses a driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Refusal {
    /// The version is not [`VERSION`].
    Version = 1,
    /// The name is empty, is not text, or has something after its zero.
    Name = 2,
    /// A feature bit this version does not define, or none of the ones a
    /// render node needs.
    Features = 3,
    /// `object_max` is zero.
    ObjectMax = 4,
    /// A handle is missing, extra, or has rights other than exactly the
    /// specified ones.
    Rights = 5,
    /// The message is not a well-formed HELLO.
    Malformed = 6,
    /// `location` is not the device the channel was made for.
    WrongLocation = 7,
    /// The driver answered something the core did not ask.
    Protocol = 8,
    /// `capsets` names set 0, which is none.
    Capsets = 9,
}

impl Refusal {
    /// The refusal a reason word names, if any.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::Version,
            2 => Self::Name,
            3 => Self::Features,
            4 => Self::ObjectMax,
            5 => Self::Rights,
            6 => Self::Malformed,
            7 => Self::WrongLocation,
            8 => Self::Protocol,
            9 => Self::Capsets,
            _ => return None,
        })
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Version => "render protocol version mismatch",
            Self::Name => "the driver's name is not a name",
            Self::Features => "the driver offers features this core does not know, or too few",
            Self::ObjectMax => "the driver will make no object of any size",
            Self::Rights => "a handle has the wrong rights",
            Self::Malformed => "malformed HELLO",
            Self::WrongLocation => "HELLO names another device",
            Self::Protocol => "the driver broke the protocol",
            Self::Capsets => "the driver offers capability set 0",
        })
    }
}

/// How a driver answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Status {
    /// Done.
    Ok = 0,
    /// The device refused it.
    DeviceRefused = 1,
    /// The pages could not be pinned.
    PinFailed = 2,
    /// The device has no room.
    OutOfMemory = 3,
    /// The driver will not do it: an id it does not know, a range it was
    /// not given, a descriptor it cannot read.
    Invalid = 4,
    /// The wait timed out with the work unfinished.
    TimedOut = 5,
}

impl Status {
    /// The status a word names, if any.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::Ok,
            1 => Self::DeviceRefused,
            2 => Self::PinFailed,
            3 => Self::OutOfMemory,
            4 => Self::Invalid,
            5 => Self::TimedOut,
            _ => return None,
        })
    }
}

/// A range of the work VMO: where a command buffer or a descriptor is.
///
/// The core hands these out and the driver pins them. Neither carries the
/// bytes in a message, because a command buffer is as long as a frame's
/// worth of drawing and a channel message is not.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Work {
    /// Where it starts in the work VMO.
    pub at: u32,
    /// How many bytes, which may be 0 for "none".
    pub len: u32,
}

impl Work {
    /// Whether the range lies inside a work VMO of `bytes`, and is not
    /// longer than [`MAX_WORK_BYTES`].
    #[must_use]
    pub fn fits(self, bytes: u64) -> bool {
        self.len <= MAX_WORK_BYTES
            && u64::from(self.at)
                .checked_add(u64::from(self.len))
                .is_some_and(|end| end <= bytes)
    }
}

/// `MAKE_OBJ`: the core asks for an object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MakeObject {
    /// The id to give it, which the core chose and no other live object on
    /// this renderer has.
    pub object: u32,
    /// Which context it belongs to, or 0 for none.
    pub context: u32,
    /// How big, in bytes.
    pub bytes: u64,
    /// What it is for: [`flags`].
    pub flags: u32,
    /// Where the driver's own description of it is, in the work VMO. What
    /// is in those bytes is the driver's business -- virgl's target,
    /// format and bind words here; something else for another device -- and
    /// the core neither writes nor reads them.
    pub describe: Work,
}

/// `MAKE_BLOB`: the core asks for a blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MakeBlob {
    /// The id to give it, chosen as an object's is.
    pub object: u32,
    /// The context it is made in, whose protocol named `blob_id`, or 0.
    pub context: u32,
    /// Where its memory is, in the driver's words: virtio-gpu's
    /// `BLOB_MEM_*`.
    pub memory: u32,
    /// What it may be used for, in the driver's words: virtio-gpu's
    /// `BLOB_FLAG_*`.
    pub flags: u32,
    /// The context's own name for the memory, or 0.
    pub blob_id: u64,
    /// How big, in bytes: a whole number of pages.
    pub bytes: u64,
    /// Where in the device's window to map it, from the window's start, or
    /// [`NO_WINDOW`].
    pub window: u64,
}

/// `SUBMIT`: the core hands over a command buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Submit {
    /// Which context it runs in.
    pub context: u32,
    /// Which of the context's rings it is fenced on, or [`NO_RING`].
    pub ring: u32,
    /// The fence to answer with, which the core chose.
    pub fence: u64,
    /// Where the command buffer is, in the work VMO.
    pub commands: Work,
}

/// Which way a `TRANSFER` moves bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Direction {
    /// From the object's backing to the device's copy.
    ToDevice = 1,
    /// From the device's copy to the object's backing.
    FromDevice = 2,
}

impl Direction {
    /// The direction a word names, if any.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::ToDevice,
            2 => Self::FromDevice,
            _ => return None,
        })
    }
}

/// A part of an object: a box of texels. A buffer is a box one texel high
/// and one deep.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Region {
    /// Where it starts.
    pub x: u32,
    /// Where it starts.
    pub y: u32,
    /// Where it starts.
    pub z: u32,
    /// How wide.
    pub width: u32,
    /// How high.
    pub height: u32,
    /// How deep.
    pub depth: u32,
}

/// `TRANSFER`: move bytes between a mappable object's backing and the
/// device's copy of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Transfer {
    /// Which object, which must be mappable.
    pub object: u32,
    /// Which context asks, or 0 for none.
    pub context: u32,
    /// Which way.
    pub direction: Direction,
    /// Which mip level.
    pub level: u32,
    /// Where in the backing the region's first byte is.
    pub offset: u64,
    /// Which part of the object.
    pub region: Region,
    /// Bytes a row in the backing, or 0 for the object's own.
    pub stride: u32,
    /// Bytes a layer in the backing, or 0 for the object's own.
    pub layer_stride: u32,
}

/// A message on the control channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    /// The driver introduces its device.
    Hello(Hello),
    /// The core accepts it.
    Ready(Ready),
    /// The core refuses it.
    Refused(Refusal),
    /// Make a context.
    MakeContext {
        /// The id to give it.
        context: u32,
        /// Which capability set it is for, or 0.
        capset: u32,
    },
    /// It was made, or was not.
    ContextMade {
        /// Which.
        context: u32,
        /// How it went.
        status: Status,
    },
    /// Destroy a context.
    DropContext {
        /// Which.
        context: u32,
    },
    /// It is gone, or is not.
    ContextGone {
        /// Which.
        context: u32,
        /// How it went.
        status: Status,
    },
    /// Make an object.
    MakeObject(MakeObject),
    /// It was made, or was not.
    ObjectMade {
        /// Which.
        object: u32,
        /// How it went.
        status: Status,
    },
    /// Destroy an object.
    DropObject {
        /// Which.
        object: u32,
    },
    /// It is gone, or is not.
    ///
    /// A driver that could not take the object's backing away from the
    /// device says so here, and the core never hands that memory out again
    /// -- the rule `docs/DISPLAY.md` §2.2 states for the display, which is
    /// the core's and not virtio's.
    ObjectGone {
        /// Which.
        object: u32,
        /// How it went.
        status: Status,
    },
    /// Run a command buffer.
    Submit(Submit),
    /// It was taken, or was not.
    Submitted {
        /// The fence it was given.
        fence: u64,
        /// How it went.
        status: Status,
    },
    /// Wait for a fence.
    Wait {
        /// Which context's.
        context: u32,
        /// Which fence.
        fence: u64,
    },
    /// The wait is over.
    Waited {
        /// Which fence.
        fence: u64,
        /// How it went.
        status: Status,
    },
    /// Move bytes between an object's backing and the device.
    Transfer(Transfer),
    /// They were moved, or were not.
    Transferred {
        /// Which object.
        object: u32,
        /// How it went.
        status: Status,
    },
    /// Ask for a capability set's bytes.
    GetCaps {
        /// Which set.
        capset: u32,
        /// Which version of it.
        version: u32,
    },
    /// Make a blob.
    MakeBlob(MakeBlob),
    /// It was made, and mapped if it was to be, or was not.
    BlobMade {
        /// Which.
        object: u32,
        /// How it went.
        status: Status,
        /// How the device wants the mapping cached, in its own words:
        /// virtio-gpu's `MAP_CACHE_*`. 0 for a blob not mapped.
        map_info: u32,
    },
    /// Make an object nameable in another context: one that imported it.
    AttachObject {
        /// Which object.
        object: u32,
        /// The context it is given to.
        context: u32,
    },
    /// It was, or was not.
    ObjectAttached {
        /// Which object.
        object: u32,
        /// How it went.
        status: Status,
    },
    /// Here they are, in the VMO this came with, or here they are not.
    Caps {
        /// Which set.
        capset: u32,
        /// How it went.
        status: Status,
        /// How many bytes of the VMO are the set.
        len: u32,
    },
    /// The core asks the driver to stop.
    Stop,
    /// It has.
    Stopped,
}

/// An encoded message: bytes up to [`MAX_BYTES`] and how many are used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    bytes: [u8; MAX_BYTES],
    len: usize,
}

impl Encoded {
    /// The message's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl Message {
    /// The message's type.
    #[must_use]
    pub const fn kind(&self) -> u32 {
        match self {
            Self::Hello(_) => HELLO,
            Self::Ready(_) => READY,
            Self::Refused(_) => REFUSED,
            Self::MakeContext { .. } => MAKE_CTX,
            Self::ContextMade { .. } => CTX_MADE,
            Self::DropContext { .. } => DROP_CTX,
            Self::ContextGone { .. } => CTX_GONE,
            Self::MakeObject(_) => MAKE_OBJ,
            Self::ObjectMade { .. } => OBJ_MADE,
            Self::DropObject { .. } => DROP_OBJ,
            Self::ObjectGone { .. } => OBJ_GONE,
            Self::Submit(_) => SUBMIT,
            Self::Submitted { .. } => SUBMITTED,
            Self::Wait { .. } => WAIT,
            Self::Waited { .. } => WAITED,
            Self::Transfer(_) => TRANSFER,
            Self::Transferred { .. } => TRANSFERRED,
            Self::GetCaps { .. } => GET_CAPS,
            Self::Caps { .. } => CAPS,
            Self::MakeBlob(_) => MAKE_BLOB,
            Self::BlobMade { .. } => BLOB_MADE,
            Self::AttachObject { .. } => ATTACH_OBJ,
            Self::ObjectAttached { .. } => OBJ_ATTACHED,
            Self::Stop => STOP,
            Self::Stopped => STOPPED,
        }
    }

    /// How long a message of type `kind` is, if this protocol has one.
    #[must_use]
    pub const fn length_of(kind: u32) -> Option<usize> {
        Some(match kind {
            HELLO => HELLO_BYTES,
            READY => READY_BYTES,
            REFUSED => REFUSED_BYTES,
            MAKE_CTX | CTX_MADE | DROP_CTX | CTX_GONE | OBJ_MADE | DROP_OBJ | OBJ_GONE
            | TRANSFERRED | GET_CAPS | ATTACH_OBJ | OBJ_ATTACHED => PAIR_BYTES,
            MAKE_OBJ => MAKE_OBJ_BYTES,
            SUBMIT => SUBMIT_BYTES,
            SUBMITTED | WAIT | WAITED | CAPS | BLOB_MADE => FENCE_BYTES,
            MAKE_BLOB => MAKE_BLOB_BYTES,
            TRANSFER => TRANSFER_BYTES,
            STOP | STOPPED => HEADER_BYTES,
            _ => return None,
        })
    }

    /// The message's bytes.
    #[must_use]
    pub fn encode(&self) -> Encoded {
        let kind = self.kind();
        let len = Self::length_of(kind).unwrap_or(HEADER_BYTES);
        let mut out = Encoded {
            bytes: [0; MAX_BYTES],
            len,
        };
        let bytes = &mut out.bytes;
        put32(bytes, 0, kind);
        put32(bytes, 4, u32::try_from(len).unwrap_or(0));
        match *self {
            Self::Hello(hello) => {
                put16(bytes, 8, hello.version);
                put32(bytes, 12, hello.location);
                put(bytes, 16, &hello.name);
                put32(bytes, 32, hello.features);
                put32(bytes, 36, hello.capsets);
                put64(bytes, 40, hello.object_max);
            }
            Self::Ready(ready) => {
                put32(bytes, 8, ready.renderer);
                put64(bytes, 16, ready.work_bytes);
            }
            Self::Refused(reason) => put32(bytes, 8, reason as u32),
            Self::MakeContext { context, capset } => {
                put32(bytes, 8, context);
                put32(bytes, 12, capset);
            }
            Self::ContextMade { context, status } | Self::ContextGone { context, status } => {
                put32(bytes, 8, context);
                put32(bytes, 12, status as u32);
            }
            Self::DropContext { context } => put32(bytes, 8, context),
            Self::MakeObject(make) => {
                put32(bytes, 8, make.object);
                put32(bytes, 12, make.context);
                put64(bytes, 16, make.bytes);
                put32(bytes, 24, make.flags);
                put32(bytes, 32, make.describe.at);
                put32(bytes, 36, make.describe.len);
            }
            Self::ObjectMade { object, status }
            | Self::ObjectGone { object, status }
            | Self::ObjectAttached { object, status } => {
                put32(bytes, 8, object);
                put32(bytes, 12, status as u32);
            }
            Self::DropObject { object } => put32(bytes, 8, object),
            Self::AttachObject { object, context } => {
                put32(bytes, 8, object);
                put32(bytes, 12, context);
            }
            Self::Submit(submit) => {
                put32(bytes, 8, submit.context);
                put32(bytes, 12, submit.ring);
                put64(bytes, 16, submit.fence);
                put32(bytes, 24, submit.commands.at);
                put32(bytes, 28, submit.commands.len);
            }
            Self::Submitted { fence, status } | Self::Waited { fence, status } => {
                put64(bytes, 8, fence);
                put32(bytes, 16, status as u32);
            }
            Self::Wait { context, fence } => {
                put32(bytes, 8, context);
                put64(bytes, 16, fence);
            }
            Self::Transfer(transfer) => put_transfer(bytes, &transfer),
            Self::Transferred { object, status } => {
                put32(bytes, 8, object);
                put32(bytes, 12, status as u32);
            }
            Self::GetCaps { capset, version } => {
                put32(bytes, 8, capset);
                put32(bytes, 12, version);
            }
            Self::Caps {
                capset,
                status,
                len,
            } => {
                put32(bytes, 8, capset);
                put32(bytes, 12, status as u32);
                put32(bytes, 16, len);
            }
            Self::MakeBlob(make) => put_blob(bytes, &make),
            Self::BlobMade {
                object,
                status,
                map_info,
            } => {
                put32(bytes, 8, object);
                put32(bytes, 12, status as u32);
                put32(bytes, 16, map_info);
            }
            Self::Stop | Self::Stopped => {}
        }
        out
    }

    /// The messages that are a word or two after their header, which is
    /// most of them: kept apart so that [`Message::decode`] stays one
    /// screen. `None` means "not one of these", not "malformed", and the
    /// caller goes on to the wider ones.
    fn decode_short(kind: u32, bytes: &[u8]) -> Option<Self> {
        let zero32 = |at: usize| (get32(bytes, at)? == 0).then_some(());
        let status = |at: usize| Status::from_raw(get32(bytes, at)?);
        Some(match kind {
            REFUSED => Message::Refused(Refusal::from_raw(get32(bytes, 8)?)?),
            MAKE_CTX => Message::MakeContext {
                context: get32(bytes, 8)?,
                capset: get32(bytes, 12)?,
            },
            CTX_MADE => Message::ContextMade {
                context: get32(bytes, 8)?,
                status: status(12)?,
            },
            DROP_CTX => {
                zero32(12)?;
                Message::DropContext {
                    context: get32(bytes, 8)?,
                }
            }
            CTX_GONE => Message::ContextGone {
                context: get32(bytes, 8)?,
                status: status(12)?,
            },
            OBJ_MADE => Message::ObjectMade {
                object: get32(bytes, 8)?,
                status: status(12)?,
            },
            DROP_OBJ => {
                zero32(12)?;
                Message::DropObject {
                    object: get32(bytes, 8)?,
                }
            }
            OBJ_GONE => Message::ObjectGone {
                object: get32(bytes, 8)?,
                status: status(12)?,
            },
            TRANSFERRED => Message::Transferred {
                object: get32(bytes, 8)?,
                status: status(12)?,
            },
            ATTACH_OBJ => Message::AttachObject {
                object: get32(bytes, 8)?,
                context: get32(bytes, 12)?,
            },
            OBJ_ATTACHED => Message::ObjectAttached {
                object: get32(bytes, 8)?,
                status: status(12)?,
            },
            GET_CAPS => Message::GetCaps {
                capset: get32(bytes, 8)?,
                version: get32(bytes, 12)?,
            },
            STOP => Message::Stop,
            STOPPED => Message::Stopped,
            _ => return None,
        })
    }

    /// Read a message, refusing one that is not exactly what its type says.
    ///
    /// # Errors
    ///
    /// `None` for a type this protocol has not got, a length that is not the
    /// type's, a reserved field that is not zero, or an enumeration with a
    /// value it has not got.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let kind = get32(bytes, 0)?;
        let len = Self::length_of(kind)?;
        if bytes.len() != len || get32(bytes, 4)? as usize != len {
            return None;
        }
        // Every reserved word is checked the same way, and a message with
        // one set is malformed.
        let zero32 = |at: usize| (get32(bytes, at)? == 0).then_some(());
        let status = |at: usize| Status::from_raw(get32(bytes, at)?);
        if let Some(short) = Self::decode_short(kind, bytes) {
            return Some(short);
        }
        Some(match kind {
            HELLO => {
                // The reserved half-word after the version, not the
                // version itself.
                (get16(bytes, 10)? == 0).then_some(())?;
                let mut name = [0u8; NAME_BYTES];
                name.copy_from_slice(bytes.get(16..16 + NAME_BYTES)?);
                Message::Hello(Hello {
                    version: get16(bytes, 8)?,
                    location: get32(bytes, 12)?,
                    name,
                    features: get32(bytes, 32)?,
                    capsets: get32(bytes, 36)?,
                    object_max: get64(bytes, 40)?,
                })
            }
            READY => {
                zero32(12)?;
                Message::Ready(Ready {
                    renderer: get32(bytes, 8)?,
                    work_bytes: get64(bytes, 16)?,
                })
            }
            MAKE_OBJ => {
                zero32(28)?;
                Message::MakeObject(MakeObject {
                    object: get32(bytes, 8)?,
                    context: get32(bytes, 12)?,
                    bytes: get64(bytes, 16)?,
                    flags: get32(bytes, 24)?,
                    describe: Work {
                        at: get32(bytes, 32)?,
                        len: get32(bytes, 36)?,
                    },
                })
            }
            SUBMIT => {
                let ring = get32(bytes, 12)?;
                (ring == NO_RING || ring < MAX_RINGS).then_some(())?;
                Message::Submit(Submit {
                    context: get32(bytes, 8)?,
                    ring,
                    fence: get64(bytes, 16)?,
                    commands: Work {
                        at: get32(bytes, 24)?,
                        len: get32(bytes, 28)?,
                    },
                })
            }
            SUBMITTED => {
                zero32(20)?;
                Message::Submitted {
                    fence: get64(bytes, 8)?,
                    status: status(16)?,
                }
            }
            WAIT => {
                zero32(12)?;
                Message::Wait {
                    context: get32(bytes, 8)?,
                    fence: get64(bytes, 16)?,
                }
            }
            WAITED => {
                zero32(20)?;
                Message::Waited {
                    fence: get64(bytes, 8)?,
                    status: status(16)?,
                }
            }
            TRANSFER => Message::Transfer(get_transfer(bytes)?),
            MAKE_BLOB => Message::MakeBlob(get_blob(bytes)?),
            BLOB_MADE => {
                zero32(20)?;
                Message::BlobMade {
                    object: get32(bytes, 8)?,
                    status: status(12)?,
                    map_info: get32(bytes, 16)?,
                }
            }
            CAPS => {
                zero32(20)?;
                Message::Caps {
                    capset: get32(bytes, 8)?,
                    status: status(12)?,
                    len: get32(bytes, 16)?,
                }
            }
            _ => return None,
        })
    }
}

/// `TRANSFER`'s fields, kept apart so that [`Message::encode`] stays one
/// screen.
fn put_transfer(bytes: &mut [u8], transfer: &Transfer) {
    put32(bytes, 8, transfer.object);
    put32(bytes, 12, transfer.context);
    put32(bytes, 16, transfer.direction as u32);
    put32(bytes, 20, transfer.level);
    put64(bytes, 24, transfer.offset);
    let region = transfer.region;
    for (index, word) in [
        region.x,
        region.y,
        region.z,
        region.width,
        region.height,
        region.depth,
    ]
    .into_iter()
    .enumerate()
    {
        put32(bytes, 32 + index * 4, word);
    }
    put32(bytes, 56, transfer.stride);
    put32(bytes, 60, transfer.layer_stride);
}

/// The same, read back.
fn get_transfer(bytes: &[u8]) -> Option<Transfer> {
    Some(Transfer {
        object: get32(bytes, 8)?,
        context: get32(bytes, 12)?,
        direction: Direction::from_raw(get32(bytes, 16)?)?,
        level: get32(bytes, 20)?,
        offset: get64(bytes, 24)?,
        region: Region {
            x: get32(bytes, 32)?,
            y: get32(bytes, 36)?,
            z: get32(bytes, 40)?,
            width: get32(bytes, 44)?,
            height: get32(bytes, 48)?,
            depth: get32(bytes, 52)?,
        },
        stride: get32(bytes, 56)?,
        layer_stride: get32(bytes, 60)?,
    })
}

/// `MAKE_BLOB`'s fields, for the reason [`put_transfer`] is apart.
fn put_blob(bytes: &mut [u8], make: &MakeBlob) {
    put32(bytes, 8, make.object);
    put32(bytes, 12, make.context);
    put32(bytes, 16, make.memory);
    put32(bytes, 20, make.flags);
    put64(bytes, 24, make.blob_id);
    put64(bytes, 32, make.bytes);
    put64(bytes, 40, make.window);
}

/// The same, read back.
fn get_blob(bytes: &[u8]) -> Option<MakeBlob> {
    Some(MakeBlob {
        object: get32(bytes, 8)?,
        context: get32(bytes, 12)?,
        memory: get32(bytes, 16)?,
        flags: get32(bytes, 20)?,
        blob_id: get64(bytes, 24)?,
        bytes: get64(bytes, 32)?,
        window: get64(bytes, 40)?,
    })
}

fn get16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn get32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn get64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn put(out: &mut [u8], at: usize, value: &[u8]) {
    if let Some(slot) = at
        .checked_add(value.len())
        .and_then(|end| out.get_mut(at..end))
    {
        slot.copy_from_slice(value);
    }
}

fn put16(out: &mut [u8], at: usize, value: u16) {
    put(out, at, &value.to_le_bytes());
}

fn put32(out: &mut [u8], at: usize, value: u32) {
    put(out, at, &value.to_le_bytes());
}

fn put64(out: &mut [u8], at: usize, value: u64) {
    put(out, at, &value.to_le_bytes());
}
