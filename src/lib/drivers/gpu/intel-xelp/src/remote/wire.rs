//! The keeper's and the worker's messages (`docs/INTEL-GPU.md` §12.4): one
//! fixed-size record per call, little-endian, a kind byte first. Several
//! records travel in one channel message ([`Batch`]), so a burst of binds
//! is one write and the channel's queue of 256 never fills in normal work.
//!
//! Handles travel beside the bytes, never in them: a [`Call::Bind`] carries
//! one, the address VMO of the memory it binds (read-only), and nothing
//! else does.

use ferrix_linux_abi::errno::Errno;

use crate::frontend::{Completion, Engine, Facts};
use crate::sseu::SseuInfo;

/// One record's size in bytes.
pub const RECORD_BYTES: usize = 96;

/// The most records one channel message carries: 64 KiB of them would be
/// the channel's limit; 128 keeps a message small and a batch's handles
/// (one per bind) under the channel's 64 with room to spare, since a batch
/// is sent once it holds [`BATCH_HANDLES`].
pub const BATCH_RECORDS: usize = 128;

/// The most handles one batch carries: the channel's limit.
pub const BATCH_HANDLES: usize = 64;

/// The engines a worker may report, in uAPI order. A worker's
/// [`Facts::engines`] must be a prefix of this list (the GT back end's and
/// the fake's both are), so the keeper can name them with a `'static`
/// slice as the front end wants.
pub static UAPI_ENGINES: [Engine; 5] = [
    Engine {
        class: 0,
        instance: 0,
    },
    Engine {
        class: 1,
        instance: 0,
    },
    Engine {
        class: 2,
        instance: 0,
    },
    Engine {
        class: 2,
        instance: 1,
    },
    Engine {
        class: 3,
        instance: 0,
    },
];

/// What the keeper asks of a worker. Every id is the keeper's, the same for
/// every worker it starts; the worker maps it to its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Call {
    /// The first record a worker reads: which of its keeper's workers it
    /// is, from 1.
    Hello {
        /// The generation.
        generation: u32,
        /// The PCI function's location, for the worker's lines.
        location: u32,
        /// The PCI device id, which only the keeper can read: the worker
        /// holds no device handle.
        device_id: u16,
        /// The PCI revision, likewise.
        revision: u8,
        /// System memory in bytes, for the uAPI's facts.
        system_memory_bytes: u64,
    },
    /// `Backend::vm_create`, as keeper VM `vm`.
    VmCreate {
        /// The keeper's VM id.
        vm: u32,
    },
    /// `Backend::vm_destroy`.
    VmDestroy {
        /// The keeper's VM id.
        vm: u32,
    },
    /// `Backend::bind`: the memory's page addresses travel as the record's
    /// handle, a read-only address VMO.
    Bind {
        /// The keeper's VM id.
        vm: u32,
        /// The GPU address.
        address: u64,
        /// Bytes bound.
        bytes: u64,
        /// The memory's pages, so the worker can check the address VMO.
        pages: u64,
        /// The PAT index.
        pat_index: u8,
    },
    /// `Backend::unbind`.
    Unbind {
        /// The keeper's VM id.
        vm: u32,
        /// The GPU address.
        address: u64,
        /// Bytes.
        bytes: u64,
    },
    /// `Backend::context_create`, as keeper context `context`.
    ContextCreate {
        /// The keeper's context id.
        context: u32,
        /// The keeper's VM id.
        vm: u32,
        /// The engine.
        engine: Engine,
        /// The ring's size.
        ring_bytes: u32,
    },
    /// `Backend::context_destroy`.
    ContextDestroy {
        /// The keeper's context id.
        context: u32,
    },
    /// `Backend::submit`.
    Submit {
        /// The keeper's context id.
        context: u32,
        /// The batch's GPU address.
        batch: u64,
        /// Its length.
        length: u32,
        /// The front end's request id.
        request: u64,
    },
    /// The keeper's MSI fired: service the GT as after an interrupt.
    Interrupt,
    /// Answer with [`Event::Pong`] carrying `seq`: the keeper's TDR.
    Ping {
        /// The ping's number.
        seq: u32,
    },
    /// `Backend::set_held`.
    Held(bool),
    /// `Backend::set_watched`.
    Watched(bool),
    /// `Backend::boost`.
    Boost {
        /// The request.
        request: u64,
    },
}

/// What a worker tells its keeper.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The GT is up: its facts, whether every engine a predecessor left
    /// pointed at a status page was reset (`done`), and a timestamp
    /// sample. The keeper replays its journal only after this, and frees a
    /// dead worker's objects only after a `reset_done` one.
    Ready {
        /// The facts.
        facts: Facts,
        /// Every inherited engine reset, or none was inherited.
        reset_done: bool,
        /// `RING_TIMESTAMP` at `now_ns`.
        timestamp: u64,
        /// When, monotonic.
        now_ns: u64,
    },
    /// A request finished.
    Completed {
        /// The front end's request id.
        request: u64,
        /// How.
        how: Completion,
    },
    /// The answer to [`Call::Ping`], with a timestamp sample.
    Pong {
        /// The ping's number.
        seq: u32,
        /// `RING_TIMESTAMP` at `now_ns`.
        timestamp: u64,
        /// When, monotonic.
        now_ns: u64,
    },
    /// A call the keeper accepted that this worker could not do (out of
    /// table space or page-table memory): the keeper takes the worker as
    /// failed, and a new one gets the journal.
    Refused {
        /// The refused call's kind byte.
        kind: u8,
        /// Its id (VM, context or request), or the address for a bind.
        id: u64,
        /// The worker's errno.
        errno: u16,
    },
}

/// Why a record could not be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Too short, or a kind byte nobody sends.
    Kind,
    /// A field out of its range: an engine list that is not a prefix of
    /// [`UAPI_ENGINES`], a completion or bool byte past its values.
    Field,
}

/// Kind bytes. Calls from 1, events from 0x81.
pub mod kind {
    /// [`super::Call::Hello`].
    pub const HELLO: u8 = 1;
    /// [`super::Call::VmCreate`].
    pub const VM_CREATE: u8 = 2;
    /// [`super::Call::VmDestroy`].
    pub const VM_DESTROY: u8 = 3;
    /// [`super::Call::Bind`].
    pub const BIND: u8 = 4;
    /// [`super::Call::Unbind`].
    pub const UNBIND: u8 = 5;
    /// [`super::Call::ContextCreate`].
    pub const CONTEXT_CREATE: u8 = 6;
    /// [`super::Call::ContextDestroy`].
    pub const CONTEXT_DESTROY: u8 = 7;
    /// [`super::Call::Submit`].
    pub const SUBMIT: u8 = 8;
    /// [`super::Call::Interrupt`].
    pub const INTERRUPT: u8 = 9;
    /// [`super::Call::Ping`].
    pub const PING: u8 = 10;
    /// [`super::Call::Held`].
    pub const HELD: u8 = 11;
    /// [`super::Call::Watched`].
    pub const WATCHED: u8 = 12;
    /// [`super::Call::Boost`].
    pub const BOOST: u8 = 13;
    /// [`super::Event::Ready`].
    pub const READY: u8 = 0x81;
    /// [`super::Event::Completed`].
    pub const COMPLETED: u8 = 0x82;
    /// [`super::Event::Pong`].
    pub const PONG: u8 = 0x83;
    /// [`super::Event::Refused`].
    pub const REFUSED: u8 = 0x84;
}

/// A record being written: fields in order after the kind byte.
struct Writer {
    bytes: [u8; RECORD_BYTES],
    at: usize,
}

impl Writer {
    fn new(kind: u8) -> Self {
        let mut bytes = [0; RECORD_BYTES];
        if let Some(first) = bytes.first_mut() {
            *first = kind;
        }
        Writer { bytes, at: 1 }
    }

    fn put(&mut self, field: &[u8]) -> &mut Self {
        if let Some(slot) = self.bytes.get_mut(self.at..self.at + field.len()) {
            slot.copy_from_slice(field);
        }
        self.at += field.len();
        self
    }

    fn u8(&mut self, value: u8) -> &mut Self {
        self.put(&[value])
    }

    fn u16(&mut self, value: u16) -> &mut Self {
        self.put(&value.to_le_bytes())
    }

    fn u32(&mut self, value: u32) -> &mut Self {
        self.put(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> &mut Self {
        self.put(&value.to_le_bytes())
    }

    fn done(&self) -> [u8; RECORD_BYTES] {
        self.bytes
    }
}

/// A record being read.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let field = self
            .bytes
            .get(self.at..self.at + N)
            .ok_or(Malformed::Kind)?;
        self.at += N;
        let mut out = [0; N];
        out.copy_from_slice(field);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        self.take::<1>().map(|[byte]| byte)
    }

    fn bool(&mut self) -> Result<bool, Malformed> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Malformed::Field),
        }
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        self.take().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        self.take().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        self.take().map(u64::from_le_bytes)
    }
}

impl Call {
    /// The record.
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_BYTES] {
        match *self {
            Call::Hello {
                generation,
                location,
                device_id,
                revision,
                system_memory_bytes,
            } => Writer::new(kind::HELLO)
                .u32(generation)
                .u32(location)
                .u16(device_id)
                .u8(revision)
                .u64(system_memory_bytes)
                .done(),
            Call::VmCreate { vm } => Writer::new(kind::VM_CREATE).u32(vm).done(),
            Call::VmDestroy { vm } => Writer::new(kind::VM_DESTROY).u32(vm).done(),
            Call::Bind {
                vm,
                address,
                bytes,
                pages,
                pat_index,
            } => Writer::new(kind::BIND)
                .u32(vm)
                .u64(address)
                .u64(bytes)
                .u64(pages)
                .u8(pat_index)
                .done(),
            Call::Unbind { vm, address, bytes } => Writer::new(kind::UNBIND)
                .u32(vm)
                .u64(address)
                .u64(bytes)
                .done(),
            Call::ContextCreate {
                context,
                vm,
                engine,
                ring_bytes,
            } => Writer::new(kind::CONTEXT_CREATE)
                .u32(context)
                .u32(vm)
                .u16(engine.class)
                .u16(engine.instance)
                .u32(ring_bytes)
                .done(),
            Call::ContextDestroy { context } => {
                Writer::new(kind::CONTEXT_DESTROY).u32(context).done()
            }
            Call::Submit {
                context,
                batch,
                length,
                request,
            } => Writer::new(kind::SUBMIT)
                .u32(context)
                .u64(batch)
                .u32(length)
                .u64(request)
                .done(),
            Call::Interrupt => Writer::new(kind::INTERRUPT).done(),
            Call::Ping { seq } => Writer::new(kind::PING).u32(seq).done(),
            Call::Held(on) => Writer::new(kind::HELD).u8(u8::from(on)).done(),
            Call::Watched(on) => Writer::new(kind::WATCHED).u8(u8::from(on)).done(),
            Call::Boost { request } => Writer::new(kind::BOOST).u64(request).done(),
        }
    }

    /// The call a record holds.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        let mut r = Reader { bytes, at: 1 };
        Ok(match bytes.first().copied() {
            Some(kind::HELLO) => Call::Hello {
                generation: r.u32()?,
                location: r.u32()?,
                device_id: r.u16()?,
                revision: r.u8()?,
                system_memory_bytes: r.u64()?,
            },
            Some(kind::VM_CREATE) => Call::VmCreate { vm: r.u32()? },
            Some(kind::VM_DESTROY) => Call::VmDestroy { vm: r.u32()? },
            Some(kind::BIND) => Call::Bind {
                vm: r.u32()?,
                address: r.u64()?,
                bytes: r.u64()?,
                pages: r.u64()?,
                pat_index: r.u8()?,
            },
            Some(kind::UNBIND) => Call::Unbind {
                vm: r.u32()?,
                address: r.u64()?,
                bytes: r.u64()?,
            },
            Some(kind::CONTEXT_CREATE) => Call::ContextCreate {
                context: r.u32()?,
                vm: r.u32()?,
                engine: Engine {
                    class: r.u16()?,
                    instance: r.u16()?,
                },
                ring_bytes: r.u32()?,
            },
            Some(kind::CONTEXT_DESTROY) => Call::ContextDestroy { context: r.u32()? },
            Some(kind::SUBMIT) => Call::Submit {
                context: r.u32()?,
                batch: r.u64()?,
                length: r.u32()?,
                request: r.u64()?,
            },
            Some(kind::INTERRUPT) => Call::Interrupt,
            Some(kind::PING) => Call::Ping { seq: r.u32()? },
            Some(kind::HELD) => Call::Held(r.bool()?),
            Some(kind::WATCHED) => Call::Watched(r.bool()?),
            Some(kind::BOOST) => Call::Boost { request: r.u64()? },
            _ => return Err(Malformed::Kind),
        })
    }

    /// Whether the record carries a handle: a bind's address VMO.
    #[must_use]
    pub const fn carries_handle(&self) -> bool {
        matches!(self, Call::Bind { .. })
    }
}

/// A completion's byte.
const fn completion_byte(how: Completion) -> u8 {
    match how {
        Completion::Ok => 0,
        Completion::Hung => 1,
        Completion::Cancelled => 2,
    }
}

fn put_facts(w: &mut Writer, facts: &Facts) {
    let _ = w
        .u16(facts.device_id)
        .u8(facts.revision)
        .u8(facts.sseu.slice_mask)
        .u8(facts.sseu.subslice_mask)
        .u16(facts.sseu.eu_mask)
        .u32(facts.sseu.eu_per_subslice)
        .u32(facts.sseu.eu_total)
        .u8(facts.sseu.slice_enable)
        .u32(facts.cs_timestamp_hz)
        .u8(u8::try_from(facts.engines.len()).unwrap_or(u8::MAX))
        .u64(facts.ggtt_bytes)
        .u8(facts.ppgtt_bits)
        .u64(facts.system_memory_bytes)
        .u8(facts.max_pat_index);
    for engine in facts.engines {
        let _ = w.u16(engine.class).u16(engine.instance);
    }
}

fn take_facts(r: &mut Reader<'_>) -> Result<Facts, Malformed> {
    let device_id = r.u16()?;
    let revision = r.u8()?;
    let sseu = SseuInfo {
        slice_mask: r.u8()?,
        subslice_mask: r.u8()?,
        eu_mask: r.u16()?,
        eu_per_subslice: r.u32()?,
        eu_total: r.u32()?,
        slice_enable: r.u8()?,
    };
    let cs_timestamp_hz = r.u32()?;
    let count = usize::from(r.u8()?);
    let ggtt_bytes = r.u64()?;
    let ppgtt_bits = r.u8()?;
    let system_memory_bytes = r.u64()?;
    let max_pat_index = r.u8()?;
    let engines = UAPI_ENGINES.get(..count).ok_or(Malformed::Field)?;
    for wanted in engines {
        let said = Engine {
            class: r.u16()?,
            instance: r.u16()?,
        };
        if said != *wanted {
            return Err(Malformed::Field);
        }
    }
    Ok(Facts {
        device_id,
        revision,
        sseu,
        cs_timestamp_hz,
        engines,
        ggtt_bytes,
        ppgtt_bits,
        system_memory_bytes,
        max_pat_index,
    })
}

impl Event {
    /// The record.
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_BYTES] {
        match *self {
            Event::Ready {
                ref facts,
                reset_done,
                timestamp,
                now_ns,
            } => {
                let mut w = Writer::new(kind::READY);
                let _ = w.u8(u8::from(reset_done)).u64(timestamp).u64(now_ns);
                put_facts(&mut w, facts);
                w.done()
            }
            Event::Completed { request, how } => Writer::new(kind::COMPLETED)
                .u64(request)
                .u8(completion_byte(how))
                .done(),
            Event::Pong {
                seq,
                timestamp,
                now_ns,
            } => Writer::new(kind::PONG)
                .u32(seq)
                .u64(timestamp)
                .u64(now_ns)
                .done(),
            Event::Refused {
                kind: refused,
                id,
                errno,
            } => Writer::new(kind::REFUSED)
                .u8(refused)
                .u64(id)
                .u16(errno)
                .done(),
        }
    }

    /// The event a record holds.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        let mut r = Reader { bytes, at: 1 };
        Ok(match bytes.first().copied() {
            Some(kind::READY) => {
                let reset_done = r.bool()?;
                let timestamp = r.u64()?;
                let now_ns = r.u64()?;
                Event::Ready {
                    facts: take_facts(&mut r)?,
                    reset_done,
                    timestamp,
                    now_ns,
                }
            }
            Some(kind::COMPLETED) => Event::Completed {
                request: r.u64()?,
                how: match r.u8()? {
                    0 => Completion::Ok,
                    1 => Completion::Hung,
                    2 => Completion::Cancelled,
                    _ => return Err(Malformed::Field),
                },
            },
            Some(kind::PONG) => Event::Pong {
                seq: r.u32()?,
                timestamp: r.u64()?,
                now_ns: r.u64()?,
            },
            Some(kind::REFUSED) => Event::Refused {
                kind: r.u8()?,
                id: r.u64()?,
                errno: r.u16()?,
            },
            _ => return Err(Malformed::Kind),
        })
    }
}

/// An errno as an event's field.
#[must_use]
pub fn errno_field(errno: Errno) -> u16 {
    errno.0
}

/// Records gathered for one channel message: bytes, and how many of them
/// carry a handle.
#[derive(Debug)]
pub struct Batch {
    bytes: [u8; RECORD_BYTES * BATCH_RECORDS],
    records: usize,
    handles: usize,
}

impl Default for Batch {
    fn default() -> Self {
        Batch {
            bytes: [0; RECORD_BYTES * BATCH_RECORDS],
            records: 0,
            handles: 0,
        }
    }
}

impl Batch {
    /// Whether one more record, carrying a handle or not, fits.
    #[must_use]
    pub const fn fits(&self, handle: bool) -> bool {
        self.records < BATCH_RECORDS && (!handle || self.handles < BATCH_HANDLES)
    }

    /// Add `record`; `false`, and nothing added, when it does not fit.
    pub fn push(&mut self, record: &[u8; RECORD_BYTES], handle: bool) -> bool {
        if !self.fits(handle) {
            return false;
        }
        let at = self.records * RECORD_BYTES;
        if let Some(slot) = self.bytes.get_mut(at..at + RECORD_BYTES) {
            slot.copy_from_slice(record);
        }
        self.records += 1;
        if handle {
            self.handles += 1;
        }
        true
    }

    /// The bytes to send.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes
            .get(..self.records * RECORD_BYTES)
            .unwrap_or_default()
    }

    /// How many records.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.records
    }

    /// Whether it holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.records == 0
    }

    /// Empty it, after a send.
    pub fn clear(&mut self) {
        self.records = 0;
        self.handles = 0;
    }
}

/// The records of a received message, in order.
pub fn records(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.chunks_exact(RECORD_BYTES)
}
