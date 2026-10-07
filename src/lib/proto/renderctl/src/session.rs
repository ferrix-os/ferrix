//! The render core's half of the conversation, as a state machine.
//!
//! A [`Session`] starts from an accepted HELLO. The core asks it for each
//! message it wants to send -- [`Session::make_context`],
//! [`Session::make_object`], [`Session::make_blob`], [`Session::submit`],
//! [`Session::wait`] and their opposites -- and the session refuses a request that would put the
//! conversation in a state the protocol does not have: an object in a
//! context that is not there, a submission into a context still being made,
//! a fence waited for twice. Every message from the driver goes through
//! [`Session::receive`], which accepts only the reply the session is waiting
//! for. Anything else is [`Refusal::Protocol`], after which the session is
//! broken and refuses everything, and the glue quiesces the driver the way
//! it quiesces a block driver that lies.
//!
//! This is the half a second driver inherits unchanged. Nothing here knows
//! what a command buffer says or what an object is for -- those are ranges
//! of the work VMO and the driver's business (`docs/GPU.md` §3.3) -- so the
//! rules it enforces are the ones any GPU's conversation has: an id is made
//! before it is used, used before it is dropped, and answered once.
//!
//! Capacity is fixed, so the kernel side allocates nothing per message:
//! [`MAX_CONTEXTS`] contexts, [`MAX_OBJECTS`] objects and
//! [`MAX_IN_FLIGHT`] submissions waiting to be answered.

use ferrix_native_abi::rights::Rights;

use crate::message::{
    Direction, Hello, MAX_CAPS_BYTES, MAX_RINGS, MakeBlob, MakeObject, Message, NO_RING, Refusal,
    Status, Submit, Transfer, Work, features, flags,
};

/// The most contexts one session tracks.
pub const MAX_CONTEXTS: usize = 16;

/// The most objects one session tracks, made or on their way.
pub const MAX_OBJECTS: usize = 256;

/// The most submissions and waits outstanding at once.
pub const MAX_IN_FLIGHT: usize = 32;

/// A request the core may not make now. The conversation is unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestError {
    /// The session is broken or stopping.
    Closed,
    /// An id that is already tracked.
    InUse,
    /// No room for another context, object or submission.
    Full,
    /// No such context, or one that is not finished being made.
    NoSuchContext,
    /// No such object, or one that is not finished being made.
    NoSuchObject,
    /// The object still has work in flight, or is still being made.
    Busy,
    /// The id 0, which names "none" and is never a thing.
    ZeroId,
    /// The object's size is zero or past what the driver said it would
    /// make.
    ObjectBytes,
    /// A flag this version does not define, or no direction at all.
    ObjectFlags,
    /// A range that is not inside the work VMO, or a command buffer of no
    /// bytes.
    Work,
    /// A transfer on an object with no backing to move bytes to or from, or
    /// of a box with no volume, or the wrong way for what the object is for.
    Transfer,
    /// Something the driver did not say in HELLO that it does: a blob, or a
    /// submission on a ring.
    Unsupported,
    /// A ring past the most a context has.
    Ring,
}

/// What a message from the driver meant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// A context is made, or was refused.
    ContextMade {
        /// Which.
        context: u32,
        /// How it went.
        status: Status,
    },
    /// A context is gone, or would not go.
    ContextGone {
        /// Which.
        context: u32,
        /// How it went.
        status: Status,
    },
    /// An object is made, or was refused.
    ObjectMade {
        /// Which.
        object: u32,
        /// How it went.
        status: Status,
    },
    /// A blob is made, and mapped if it was to be, or was refused.
    BlobMade {
        /// Which.
        object: u32,
        /// How it went.
        status: Status,
        /// How the device wants the mapping cached, in its own words.
        map_info: u32,
    },
    /// An object is gone, or would not go.
    ///
    /// A status other than [`Status::Ok`] means the device may still hold
    /// the object's backing, and the core never hands that memory out
    /// again -- the rule `docs/DISPLAY.md` §2.2 states for the display,
    /// which belongs to the core and not to virtio.
    ObjectGone {
        /// Which.
        object: u32,
        /// How it went.
        status: Status,
    },
    /// A command buffer was taken, or was not.
    Submitted {
        /// Its fence.
        fence: u64,
        /// How it went.
        status: Status,
    },
    /// A wait is over.
    Waited {
        /// Its fence.
        fence: u64,
        /// How it went.
        status: Status,
    },
    /// Bytes were moved, or were not.
    Transferred {
        /// Which object's.
        object: u32,
        /// How it went.
        status: Status,
    },
    /// An object was made nameable in another context, or was not.
    ObjectAttached {
        /// Which object.
        object: u32,
        /// The context it was given to.
        context: u32,
        /// How it went.
        status: Status,
    },
    /// A capability set came, in the VMO beside the message, or did not.
    Caps {
        /// Which set.
        capset: u32,
        /// How it went.
        status: Status,
        /// How many bytes of the VMO are the set.
        len: u32,
    },
    /// The driver has stopped.
    Stopped,
}

/// What one tracked id is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// Nothing.
    Free,
    /// Asked for, not yet answered.
    Making(u32),
    /// Made.
    Live(u32),
    /// Asked to go, not yet answered.
    Dropping(u32),
}

impl Slot {
    /// The id this slot is about, whatever it is doing.
    const fn id(self) -> Option<u32> {
        match self {
            Self::Free => None,
            Self::Making(id) | Self::Live(id) | Self::Dropping(id) => Some(id),
        }
    }
}

/// What a blob was made for, in [`Session`]'s `purposes`: none of the
/// object flags, which is also what keeps a transfer off it -- it has no
/// backing to move bytes to or from.
const BLOB_PURPOSE: u8 = 0x80;

/// A submission or a wait the driver has not answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pending {
    fence: u64,
    /// Whether this is the wait for that fence rather than the submission.
    waiting: bool,
}

/// The core's side of one driver's conversation.
#[derive(Clone, Debug)]
pub struct Session {
    name: [u8; crate::message::NAME_BYTES],
    features: u32,
    capsets: u32,
    object_limit: u64,
    work_bytes: u64,
    contexts: [Slot; MAX_CONTEXTS],
    objects: [Slot; MAX_OBJECTS],
    /// Which context each object belongs to, beside `objects`.
    owners: [u32; MAX_OBJECTS],
    /// What each object was made for, beside `objects`: its `flags`, which
    /// fit a byte, and a session is copied about on a kernel stack.
    purposes: [u8; MAX_OBJECTS],
    /// Objects with a transfer the driver has not answered, 0 for none.
    moving: [u32; MAX_IN_FLIGHT],
    /// Objects with an attach the driver has not answered, and the context
    /// each is being given to; `(0, 0)` for none. One an object at a time:
    /// the reply names the object and nothing else.
    attaching: [(u32, u32); MAX_IN_FLIGHT],
    /// The capability set asked for and not yet answered.
    caps: Option<u32>,
    pending: [Pending; MAX_IN_FLIGHT],
    pending_count: usize,
    stopping: bool,
    broken: bool,
}

impl Session {
    /// Accept a driver's HELLO, with the rights of the handles it came
    /// with, for a work VMO of `work_bytes`.
    ///
    /// # Errors
    ///
    /// The reason to refuse the driver.
    pub fn accept(
        hello: &Hello,
        handle_rights: &[Rights],
        work_bytes: u64,
    ) -> Result<Self, Refusal> {
        hello.validate(handle_rights)?;
        Ok(Self {
            name: hello.name,
            features: hello.features,
            capsets: hello.capsets,
            object_limit: hello.object_limit(),
            work_bytes,
            contexts: [Slot::Free; MAX_CONTEXTS],
            objects: [Slot::Free; MAX_OBJECTS],
            owners: [0; MAX_OBJECTS],
            purposes: [0; MAX_OBJECTS],
            moving: [0; MAX_IN_FLIGHT],
            attaching: [(0, 0); MAX_IN_FLIGHT],
            caps: None,
            pending: [Pending {
                fence: 0,
                waiting: false,
            }; MAX_IN_FLIGHT],
            pending_count: 0,
            stopping: false,
            broken: false,
        })
    }

    /// What the driver is called, which is what `DRM_IOCTL_VERSION` reports
    /// and what userspace picks a back end by.
    #[must_use]
    pub fn name(&self) -> &str {
        let end = self
            .name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(crate::message::NAME_BYTES);
        ::core::str::from_utf8(self.name.get(..end).unwrap_or(&[])).unwrap_or("")
    }

    /// Which capability sets a context may be made for, a bit per set.
    #[must_use]
    pub const fn capsets(&self) -> u32 {
        self.capsets
    }

    /// What the driver said it can do.
    #[must_use]
    pub const fn features(&self) -> u32 {
        self.features
    }

    /// The largest object this driver will be asked for, in bytes: the
    /// smaller of what it offered and what the protocol allows.
    #[must_use]
    pub const fn object_limit(&self) -> u64 {
        self.object_limit
    }

    /// How much of the work VMO the core has to hand out.
    #[must_use]
    pub const fn work_bytes(&self) -> u64 {
        self.work_bytes
    }

    /// Whether the conversation has ended badly.
    #[must_use]
    pub const fn is_broken(&self) -> bool {
        self.broken
    }

    /// Whether `object` is tracked at all -- made, on its way, or on its way
    /// out.
    ///
    /// The core chooses object ids and the session refuses one it is already
    /// holding, so the core has to be able to ask before it chooses. An id
    /// the device would not let go of stays tracked for ever, which is what
    /// stops it being handed out a second time.
    #[must_use]
    pub fn holds_object(&self, object: u32) -> bool {
        find(&self.objects, object).is_some()
    }

    /// Ask for a context.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for.
    pub fn make_context(&mut self, context: u32, capset: u32) -> Result<Message, RequestError> {
        self.open()?;
        if context == 0 {
            return Err(RequestError::ZeroId);
        }
        if find(&self.contexts, context).is_some() {
            return Err(RequestError::InUse);
        }
        let slot = free(&mut self.contexts).ok_or(RequestError::Full)?;
        *slot = Slot::Making(context);
        Ok(Message::MakeContext { context, capset })
    }

    /// Ask for a context to go.
    ///
    /// # Errors
    ///
    /// A request the conversation has no state for.
    pub fn drop_context(&mut self, context: u32) -> Result<Message, RequestError> {
        self.open()?;
        let at = live(&self.contexts, context).ok_or(RequestError::NoSuchContext)?;
        // Nor one an object is being given to: the answer would name a
        // context that is gone.
        if self.attaching.iter().any(|(_, into)| *into == context) {
            return Err(RequestError::Busy);
        }
        // A context with objects still in it is not one to take away: the
        // objects would outlive what owns them.
        if self
            .owners
            .iter()
            .zip(self.objects.iter())
            .any(|(owner, slot)| *owner == context && *slot != Slot::Free)
        {
            return Err(RequestError::Busy);
        }
        if let Some(slot) = self.contexts.get_mut(at) {
            *slot = Slot::Dropping(context);
        }
        Ok(Message::DropContext { context })
    }

    /// Ask for an object.
    ///
    /// `describe` is where the driver's own description of it is in the
    /// work VMO. The session checks that the range is one the core could
    /// have handed out and nothing else: what is written there is the
    /// driver's business.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for.
    pub fn make_object(
        &mut self,
        object: u32,
        context: u32,
        bytes: u64,
        object_flags: u32,
        describe: Work,
    ) -> Result<Message, RequestError> {
        self.open()?;
        if object == 0 {
            return Err(RequestError::ZeroId);
        }
        if find(&self.objects, object).is_some() {
            return Err(RequestError::InUse);
        }
        if context != 0 && live(&self.contexts, context).is_none() {
            return Err(RequestError::NoSuchContext);
        }
        if bytes == 0 || bytes > self.object_limit {
            return Err(RequestError::ObjectBytes);
        }
        if object_flags & !flags::KNOWN != 0
            || object_flags & (flags::TO_DEVICE | flags::FROM_DEVICE) == 0
        {
            return Err(RequestError::ObjectFlags);
        }
        if !describe.fits(self.work_bytes) {
            return Err(RequestError::Work);
        }
        let at = free_at(&self.objects).ok_or(RequestError::Full)?;
        if let Some(slot) = self.objects.get_mut(at) {
            *slot = Slot::Making(object);
        }
        if let Some(owner) = self.owners.get_mut(at) {
            *owner = context;
        }
        if let Some(purpose) = self.purposes.get_mut(at) {
            // `KNOWN` was checked above and fits.
            *purpose = (object_flags & flags::KNOWN) as u8;
        }
        Ok(Message::MakeObject(MakeObject {
            object,
            context,
            bytes,
            flags: object_flags,
            describe,
        }))
    }

    /// Ask for a blob: an object whose memory the device's side makes,
    /// mapped into the device's window at `make.window` unless that is
    /// [`crate::message::NO_WINDOW`].
    ///
    /// The window's places are the core's to hand out, as work VMO ranges
    /// are, and the session does not check them: it has not seen the
    /// window. What `memory` and `flags` mean is the driver's.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for, or a driver
    /// that makes no blobs.
    pub fn make_blob(&mut self, make: MakeBlob) -> Result<Message, RequestError> {
        self.open()?;
        if self.features & features::BLOBS == 0 {
            return Err(RequestError::Unsupported);
        }
        if make.object == 0 {
            return Err(RequestError::ZeroId);
        }
        if find(&self.objects, make.object).is_some() {
            return Err(RequestError::InUse);
        }
        if make.context != 0 && live(&self.contexts, make.context).is_none() {
            return Err(RequestError::NoSuchContext);
        }
        if make.bytes == 0 || make.bytes > self.object_limit {
            return Err(RequestError::ObjectBytes);
        }
        let at = free_at(&self.objects).ok_or(RequestError::Full)?;
        if let Some(slot) = self.objects.get_mut(at) {
            *slot = Slot::Making(make.object);
        }
        if let Some(owner) = self.owners.get_mut(at) {
            *owner = make.context;
        }
        if let Some(purpose) = self.purposes.get_mut(at) {
            *purpose = BLOB_PURPOSE;
        }
        Ok(Message::MakeBlob(make))
    }

    /// Ask for an object to go.
    ///
    /// # Errors
    ///
    /// A request the conversation has no state for.
    pub fn drop_object(&mut self, object: u32) -> Result<Message, RequestError> {
        self.open()?;
        let at = live(&self.objects, object).ok_or(RequestError::NoSuchObject)?;
        // Bytes on their way to or from the backing, or the object on its
        // way into another context: the driver is still using what a drop
        // would take away.
        if self.moving.contains(&object) || self.attaching.iter().any(|(held, _)| *held == object) {
            return Err(RequestError::Busy);
        }
        if let Some(slot) = self.objects.get_mut(at) {
            *slot = Slot::Dropping(object);
        }
        Ok(Message::DropObject { object })
    }

    /// Hand over a command buffer, which the driver answers with the fence:
    /// when it has the buffer, or on a `ring` other than [`NO_RING`], when
    /// the work has finished.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for, or a ring
    /// the driver did not say it has.
    pub fn submit(
        &mut self,
        context: u32,
        ring: u32,
        fence: u64,
        commands: Work,
    ) -> Result<Message, RequestError> {
        self.open()?;
        if live(&self.contexts, context).is_none() {
            return Err(RequestError::NoSuchContext);
        }
        if ring != NO_RING {
            if self.features & features::RINGS == 0 {
                return Err(RequestError::Unsupported);
            }
            if ring >= MAX_RINGS {
                return Err(RequestError::Ring);
            }
        }
        // A command buffer of no bytes is nothing to run, and a range the
        // core did not hand out is one the driver would pin blind.
        if commands.len == 0 || !commands.fits(self.work_bytes) {
            return Err(RequestError::Work);
        }
        if self.pending_count >= MAX_IN_FLIGHT {
            return Err(RequestError::Full);
        }
        if self.awaiting(fence, false).is_some() || self.awaiting(fence, true).is_some() {
            return Err(RequestError::InUse);
        }
        self.push(Pending {
            fence,
            waiting: false,
        });
        Ok(Message::Submit(Submit {
            context,
            ring,
            fence,
            commands,
        }))
    }

    /// Wait for a fence the driver has taken.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for.
    pub fn wait(&mut self, context: u32, fence: u64) -> Result<Message, RequestError> {
        self.open()?;
        if live(&self.contexts, context).is_none() {
            return Err(RequestError::NoSuchContext);
        }
        if self.pending_count >= MAX_IN_FLIGHT {
            return Err(RequestError::Full);
        }
        if self.awaiting(fence, true).is_some() {
            return Err(RequestError::InUse);
        }
        self.push(Pending {
            fence,
            waiting: true,
        });
        Ok(Message::Wait { context, fence })
    }

    /// Move bytes between a mappable object's backing and the device.
    ///
    /// One transfer an object at a time: the reply names the object and
    /// nothing else, so two outstanding could not be told apart.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for.
    pub fn transfer(&mut self, transfer: Transfer) -> Result<Message, RequestError> {
        self.open()?;
        let at = live(&self.objects, transfer.object).ok_or(RequestError::NoSuchObject)?;
        if transfer.context != 0 && live(&self.contexts, transfer.context).is_none() {
            return Err(RequestError::NoSuchContext);
        }
        let purpose = u32::from(self.purposes.get(at).copied().unwrap_or(0));
        let way = match transfer.direction {
            Direction::ToDevice => flags::TO_DEVICE,
            Direction::FromDevice => flags::FROM_DEVICE,
        };
        let region = transfer.region;
        if purpose & flags::MAPPABLE == 0
            || purpose & way == 0
            || region.width == 0
            || region.height == 0
            || region.depth == 0
        {
            return Err(RequestError::Transfer);
        }
        if self.moving.contains(&transfer.object) {
            return Err(RequestError::InUse);
        }
        let slot = self
            .moving
            .iter_mut()
            .find(|held| **held == 0)
            .ok_or(RequestError::Full)?;
        *slot = transfer.object;
        Ok(Message::Transfer(transfer))
    }

    /// Make a live object nameable in another live context: the one that
    /// imported it (`docs/GPU.md` §3.13).
    ///
    /// Asking again for an object already attached to that context is not
    /// refused: the device takes a second attach as it takes the first.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for, or an
    /// object already on its way into a context.
    pub fn attach_object(&mut self, object: u32, context: u32) -> Result<Message, RequestError> {
        self.open()?;
        if object == 0 || context == 0 {
            return Err(RequestError::ZeroId);
        }
        if live(&self.objects, object).is_none() {
            return Err(RequestError::NoSuchObject);
        }
        if live(&self.contexts, context).is_none() {
            return Err(RequestError::NoSuchContext);
        }
        if self.attaching.iter().any(|(held, _)| *held == object) {
            return Err(RequestError::InUse);
        }
        let slot = self
            .attaching
            .iter_mut()
            .find(|held| **held == (0, 0))
            .ok_or(RequestError::Full)?;
        *slot = (object, context);
        Ok(Message::AttachObject { object, context })
    }

    /// Ask for a capability set's bytes. One question at a time.
    ///
    /// # Errors
    ///
    /// The session is closed, or a question is already outstanding.
    pub fn get_caps(&mut self, capset: u32, version: u32) -> Result<Message, RequestError> {
        self.open()?;
        if self.caps.is_some() {
            return Err(RequestError::InUse);
        }
        self.caps = Some(capset);
        Ok(Message::GetCaps { capset, version })
    }

    /// Ask the driver to stop.
    ///
    /// # Errors
    ///
    /// The session is broken or already stopping.
    pub fn stop(&mut self) -> Result<Message, RequestError> {
        self.open()?;
        self.stopping = true;
        Ok(Message::Stop)
    }

    /// Take a message from the driver, which must be one the session is
    /// waiting for.
    ///
    /// # Errors
    ///
    /// [`Refusal::Protocol`] for anything else, after which the session is
    /// broken.
    pub fn receive(&mut self, message: &Message) -> Result<Event, Refusal> {
        if self.broken {
            return Err(Refusal::Protocol);
        }
        let event = self.take(message);
        if event.is_err() {
            self.broken = true;
        }
        event
    }

    fn take(&mut self, message: &Message) -> Result<Event, Refusal> {
        match *message {
            Message::ContextMade { context, status } => {
                let at = making(&self.contexts, context).ok_or(Refusal::Protocol)?;
                self.settle(at, status, true);
                Ok(Event::ContextMade { context, status })
            }
            Message::ContextGone { context, status } => {
                let at = dropping(&self.contexts, context).ok_or(Refusal::Protocol)?;
                // A context the driver would not drop stays live, so the
                // core does not reuse its id.
                self.settle_context(at, status);
                Ok(Event::ContextGone { context, status })
            }
            Message::ObjectMade { object, status } => {
                let at = making(&self.objects, object).ok_or(Refusal::Protocol)?;
                // And one asked for as a blob is answered as one.
                if self.purposes.get(at) == Some(&BLOB_PURPOSE) {
                    return Err(Refusal::Protocol);
                }
                self.settle_object(at, status, true);
                Ok(Event::ObjectMade { object, status })
            }
            Message::BlobMade {
                object,
                status,
                map_info,
            } => {
                let at = making(&self.objects, object).ok_or(Refusal::Protocol)?;
                // An object answered as a blob was asked for as one.
                if self.purposes.get(at) != Some(&BLOB_PURPOSE) {
                    return Err(Refusal::Protocol);
                }
                self.settle_object(at, status, true);
                Ok(Event::BlobMade {
                    object,
                    status,
                    map_info,
                })
            }
            Message::ObjectGone { object, status } => {
                let at = dropping(&self.objects, object).ok_or(Refusal::Protocol)?;
                self.settle_object(at, status, false);
                Ok(Event::ObjectGone { object, status })
            }
            Message::Submitted { fence, status } => {
                let at = self.awaiting(fence, false).ok_or(Refusal::Protocol)?;
                self.remove(at);
                Ok(Event::Submitted { fence, status })
            }
            Message::Waited { fence, status } => {
                let at = self.awaiting(fence, true).ok_or(Refusal::Protocol)?;
                self.remove(at);
                Ok(Event::Waited { fence, status })
            }
            Message::Transferred { object, status } => {
                let slot = self
                    .moving
                    .iter_mut()
                    .find(|held| **held == object && object != 0)
                    .ok_or(Refusal::Protocol)?;
                *slot = 0;
                Ok(Event::Transferred { object, status })
            }
            Message::ObjectAttached { object, status } => {
                let slot = self
                    .attaching
                    .iter_mut()
                    .find(|(held, _)| *held == object && object != 0)
                    .ok_or(Refusal::Protocol)?;
                let context = slot.1;
                *slot = (0, 0);
                Ok(Event::ObjectAttached {
                    object,
                    context,
                    status,
                })
            }
            Message::Caps {
                capset,
                status,
                len,
            } => {
                // The set that was asked for, and no longer than a core
                // takes: a length is what the core will copy by.
                if self.caps != Some(capset) || len > MAX_CAPS_BYTES {
                    return Err(Refusal::Protocol);
                }
                self.caps = None;
                Ok(Event::Caps {
                    capset,
                    status,
                    len,
                })
            }
            Message::Stopped if self.stopping => Ok(Event::Stopped),
            _ => Err(Refusal::Protocol),
        }
    }

    /// Whether the core may ask for anything.
    fn open(&self) -> Result<(), RequestError> {
        if self.broken || self.stopping {
            return Err(RequestError::Closed);
        }
        Ok(())
    }

    /// A context slot settles: made becomes live, refused becomes free.
    fn settle(&mut self, at: usize, status: Status, making: bool) {
        let Some(slot) = self.contexts.get_mut(at) else {
            return;
        };
        let Some(id) = slot.id() else {
            return;
        };
        *slot = match (status, making) {
            (Status::Ok, true) => Slot::Live(id),
            (Status::Ok, false) => Slot::Free,
            (_, true) => Slot::Free,
            (_, false) => Slot::Live(id),
        };
    }

    fn settle_context(&mut self, at: usize, status: Status) {
        self.settle(at, status, false);
    }

    /// The same for an object, whose owner is forgotten with it.
    fn settle_object(&mut self, at: usize, status: Status, making: bool) {
        let Some(slot) = self.objects.get_mut(at) else {
            return;
        };
        let Some(id) = slot.id() else {
            return;
        };
        let settled = match (status, making) {
            (Status::Ok, true) => Slot::Live(id),
            (Status::Ok, false) => Slot::Free,
            (_, true) => Slot::Free,
            // A device that would not let go still holds the backing, so
            // the object stays live and its id is never reused.
            (_, false) => Slot::Live(id),
        };
        *slot = settled;
        if settled == Slot::Free {
            if let Some(owner) = self.owners.get_mut(at) {
                *owner = 0;
            }
            if let Some(purpose) = self.purposes.get_mut(at) {
                *purpose = 0;
            }
        }
    }

    /// Where a fence of that kind is waiting to be answered.
    fn awaiting(&self, fence: u64, waiting: bool) -> Option<usize> {
        self.pending
            .get(..self.pending_count)?
            .iter()
            .position(|held| held.fence == fence && held.waiting == waiting)
    }

    fn push(&mut self, entry: Pending) {
        if let Some(slot) = self.pending.get_mut(self.pending_count) {
            *slot = entry;
            self.pending_count += 1;
        }
    }

    fn remove(&mut self, at: usize) {
        if at < self.pending_count {
            for index in at..self.pending_count.saturating_sub(1) {
                let next = self.pending.get(index + 1).copied();
                if let (Some(next), Some(slot)) = (next, self.pending.get_mut(index)) {
                    *slot = next;
                }
            }
            self.pending_count -= 1;
        }
    }
}

/// Where `id` is tracked, whatever it is doing.
fn find(slots: &[Slot], id: u32) -> Option<usize> {
    slots.iter().position(|slot| slot.id() == Some(id))
}

/// Where `id` is live.
fn live(slots: &[Slot], id: u32) -> Option<usize> {
    slots.iter().position(|slot| *slot == Slot::Live(id))
}

/// Where `id` is being made.
fn making(slots: &[Slot], id: u32) -> Option<usize> {
    slots.iter().position(|slot| *slot == Slot::Making(id))
}

/// Where `id` is being dropped.
fn dropping(slots: &[Slot], id: u32) -> Option<usize> {
    slots.iter().position(|slot| *slot == Slot::Dropping(id))
}

/// The first free slot.
fn free(slots: &mut [Slot]) -> Option<&mut Slot> {
    slots.iter_mut().find(|slot| **slot == Slot::Free)
}

/// Where the first free slot is.
fn free_at(slots: &[Slot]) -> Option<usize> {
    slots.iter().position(|slot| *slot == Slot::Free)
}
