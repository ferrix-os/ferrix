//! The front end's tables: fixed-size slabs of files, GEM objects and their
//! handles, bindings, VMs, contexts, syncobjs, requests, kernel fences and
//! held waits.
//!
//! An id a program sees is a slot's index plus one: a GEM handle, a syncobj
//! handle, a context id and a VM id are each unique across every file, which
//! Linux's per-file `idr`s do not promise but nothing a program may rely on
//! forbids. Each slot records its file, and a lookup from another file's id
//! fails as an unknown id does (`ENOENT`).

use super::backend::{Engine, HwContext, VmId};
use super::limits;
use super::{RequestId, fence::Waits};
use ferrix_linux_abi::errno::Errno;

/// A fixed-size table of `N` optional `T`s.
pub struct Slab<T, const N: usize> {
    slots: [Option<T>; N],
    hint: usize,
    live: usize,
}

impl<T, const N: usize> Slab<T, N> {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            hint: 0,
            live: 0,
        }
    }

    /// Put `value` in a free slot; `None` when every slot is taken.
    pub fn insert(&mut self, value: T) -> Option<usize> {
        if self.live == N {
            return None;
        }
        for step in 0..N {
            let index = (self.hint + step) % N;
            if let Some(slot) = self.slots.get_mut(index)
                && slot.is_none()
            {
                *slot = Some(value);
                self.hint = (index + 1) % N;
                self.live += 1;
                return Some(index);
            }
        }
        None
    }

    /// The value in slot `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        self.slots.get(index).and_then(Option::as_ref)
    }

    /// The value in slot `index`, to change.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.slots.get_mut(index).and_then(Option::as_mut)
    }

    /// Take the value out of slot `index`.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        let taken = self.slots.get_mut(index).and_then(Option::take);
        if taken.is_some() {
            self.live -= 1;
        }
        taken
    }

    /// How many slots are taken.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.live
    }

    /// Whether no slot is taken.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The taken slots' indices and values.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|value| (index, value)))
    }

    /// The taken slots' indices and values, to change.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut T)> {
        self.slots
            .iter_mut()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_mut().map(|value| (index, value)))
    }
}

impl<T, const N: usize> core::fmt::Debug for Slab<T, N> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "Slab({} of {N})", self.live)
    }
}

impl<T, const N: usize> Default for Slab<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// A fence, as the front end tracks it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FenceRef {
    /// Signalled already, with this status (0 or a negative errno).
    Signalled(i32),
    /// The fence of one request.
    Request(RequestId),
    /// Every request with an id up to this one: what several fences merge
    /// into when a table keeps too few apart. It may wait longer than the
    /// fences it stands for, never shorter.
    UpTo(RequestId),
}

impl FenceRef {
    /// The newest request this fence may wait for.
    #[must_use]
    pub const fn horizon(self) -> RequestId {
        match self {
            FenceRef::Signalled(_) => 0,
            FenceRef::Request(id) | FenceRef::UpTo(id) => id,
        }
    }

    /// One fence that waits for both.
    #[must_use]
    pub fn merge(self, other: FenceRef) -> FenceRef {
        match (self, other) {
            (FenceRef::Signalled(0), other) | (other, FenceRef::Signalled(0)) => other,
            (FenceRef::Signalled(error), FenceRef::Signalled(_)) => FenceRef::Signalled(error),
            (a, b) => FenceRef::UpTo(a.horizon().max(b.horizon())),
        }
    }
}

/// An open file of the render node.
#[derive(Clone, Copy, Debug)]
pub struct File {
    /// The chardev core's identity for it.
    pub id: u64,
    /// The opening program's process id.
    pub pid: u32,
    /// Its default context (id 0), a [`Tables::contexts`] slot.
    pub default_context: u16,
}

/// A GEM object.
pub struct Object<M> {
    /// Its memory.
    pub memory: M,
    /// Its size in bytes, a multiple of 4 KiB.
    pub size: u64,
    /// How many handles name it, in every file.
    pub handles: u32,
    /// Whether a dmabuf of it is alive (made, and its `DmabufRelease` not
    /// yet seen): that holds the object as a handle does.
    pub dmabuf_alive: bool,
    /// Whether it was ever exported, so other programs may share it: its
    /// dmabuf's fence slot is set for every execbuffer that uses it
    /// (ledger 2360).
    pub shared: bool,
    /// `SET_TILING`'s mode and stride, kept and told back.
    pub tiling: u32,
    /// The stride.
    pub stride: u32,
    /// `SET_CACHING`'s level, kept and told back.
    pub caching: u32,
    /// `MADVISE`'s advice, kept and told back. Nothing is ever purged.
    pub madv: u32,
    /// The PAT index its bindings use.
    pub pat_index: u8,
    /// The files that asked for an mmap offset, one bit per file slot:
    /// only they may map it (`drm_vma_node_allow`).
    pub mmap_files: u32,
    /// The mode it was first mapped in, write-combining or not: K6 lets a
    /// VMO become write-combining only before anything maps it.
    pub mapped_wc: Option<bool>,
    /// Whether a write-combining mmap of it was served write-back
    /// (`gem::wc_served_cached`): it then keeps the LLC's PAT index.
    pub wc_as_cached: bool,
    /// The last write's fence.
    pub write: FenceRef,
    /// The reads since that write.
    pub reads: [FenceRef; limits::OBJECT_READS],
    /// How many bindings it has.
    pub bindings: u32,
}

impl<M> core::fmt::Debug for Object<M> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Object")
            .field("size", &self.size)
            .field("handles", &self.handles)
            .field("dmabuf_alive", &self.dmabuf_alive)
            .field("write", &self.write)
            .field("reads", &self.reads)
            .field("bindings", &self.bindings)
            .finish_non_exhaustive()
    }
}

impl<M> Object<M> {
    /// Whether every access to it has finished, by `status`.
    pub fn idle(&self, status: impl Fn(FenceRef) -> Option<i32>) -> bool {
        status(self.write).is_some() && self.reads.iter().all(|read| status(*read).is_some())
    }

    /// Whether nothing holds it any more: no handle and no live dmabuf.
    #[must_use]
    pub const fn orphaned(&self) -> bool {
        self.handles == 0 && !self.dmabuf_alive
    }
}

/// A GEM handle: one file's name for an object.
#[derive(Clone, Copy, Debug)]
pub struct Handle {
    /// The file's slot.
    pub file: u8,
    /// The object's slot.
    pub object: u16,
}

/// One object bound into one VM.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Binding {
    /// The VM's slot, in the top 16 bits, and the address: the sort key.
    pub key: u64,
    /// Its length in bytes.
    pub size: u64,
    /// The object's slot.
    pub object: u16,
    /// The newest request that used it, or 0.
    pub last_use: RequestId,
}

impl Binding {
    /// The sort key of `address` in VM slot `vm`.
    #[must_use]
    pub const fn key_of(vm: u16, address: u64) -> u64 {
        ((vm as u64) << 48) | (address & ((1 << 48) - 1))
    }

    /// Its VM's slot.
    #[must_use]
    pub const fn vm(&self) -> u16 {
        (self.key >> 48) as u16
    }

    /// Its address.
    #[must_use]
    pub const fn address(&self) -> u64 {
        self.key & ((1 << 48) - 1)
    }

    /// The key one past its end.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.key + self.size
    }
}

/// Every binding, sorted by VM and address, so an overlap is found by its
/// neighbours.
pub struct Bindings {
    entries: [Binding; limits::BINDINGS],
    len: usize,
}

impl Bindings {
    /// None.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [Binding {
                key: 0,
                size: 0,
                object: 0,
                last_use: 0,
            }; limits::BINDINGS],
            len: 0,
        }
    }

    /// The bindings, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[Binding] {
        self.entries.get(..self.len).unwrap_or(&[])
    }

    /// Where `key` is or would go.
    fn search(&self, key: u64) -> Result<usize, usize> {
        self.as_slice()
            .binary_search_by(|entry| entry.key.cmp(&key))
    }

    /// The binding that starts at `key`.
    #[must_use]
    pub fn at(&self, key: u64) -> Option<usize> {
        self.search(key).ok()
    }

    /// The binding at index `index`, to change.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Binding> {
        self.entries.get_mut(..self.len)?.get_mut(index)
    }

    /// The bindings that overlap `[key, key + size)`, as an index range.
    #[must_use]
    pub fn overlapping(&self, key: u64, size: u64) -> core::ops::Range<usize> {
        let end = key + size;
        let slice = self.as_slice();
        let mut first = match self.search(key) {
            Ok(index) | Err(index) => index,
        };
        while first > 0
            && let Some(before) = slice.get(first - 1)
            && before.end() > key
        {
            first -= 1;
        }
        let mut last = first;
        while let Some(entry) = slice.get(last)
            && entry.key < end
        {
            last += 1;
        }
        // Entries between `first` and `last` start before `end`; those that
        // end at or before `key` cannot be there by the walk above, but the
        // binary search's own index can be one that ends before `key`.
        while first < last
            && let Some(entry) = slice.get(first)
            && entry.end() <= key
        {
            first += 1;
        }
        first..last
    }

    /// Add `binding`; `false` when the table is full.
    pub fn insert(&mut self, binding: Binding) -> bool {
        if self.len == limits::BINDINGS {
            return false;
        }
        let at = match self.search(binding.key) {
            Ok(index) | Err(index) => index,
        };
        self.entries.copy_within(at..self.len, at + 1);
        if let Some(slot) = self.entries.get_mut(at) {
            *slot = binding;
        }
        self.len += 1;
        true
    }

    /// Take out the binding at `index`.
    pub fn remove(&mut self, index: usize) -> Option<Binding> {
        if index >= self.len {
            return None;
        }
        let taken = self.entries.get(index).copied();
        self.entries.copy_within(index + 1..self.len, index);
        self.len -= 1;
        taken
    }
}

impl core::fmt::Debug for Bindings {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_list().entries(self.as_slice()).finish()
    }
}

impl Default for Bindings {
    fn default() -> Self {
        Self::new()
    }
}

/// A VM: a PPGTT, made at its first binding.
#[derive(Clone, Copy, Debug)]
pub struct Vm {
    /// The file that made it.
    pub file: u8,
    /// The back end's PPGTT, once made.
    pub backend: Option<VmId>,
    /// Its ids and contexts.
    pub refs: u32,
}

/// One VM id of one file.
#[derive(Clone, Copy, Debug)]
pub struct VmName {
    /// The file.
    pub file: u8,
    /// The VM's slot.
    pub vm: u16,
}

/// A context.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    /// The file.
    pub file: u8,
    /// Its VM's slot.
    pub vm: u16,
    /// Whether it is past i915's proto-context stage (Gen12): looked up by
    /// an execbuffer or `GETPARAM`, or the default context. Only a proto
    /// context takes `VM` and `ENGINES`.
    pub finalized: bool,
    /// The user's engine map, if `ENGINES` was set; `None` is the legacy
    /// ring map.
    pub engines: Option<[Option<Engine>; limits::CONTEXT_ENGINES]>,
    /// How many of the map's entries there are.
    pub engine_count: u8,
    /// A hardware context per engine slot (the map's index, or the legacy
    /// map's engine class), made at its first request.
    pub hw: [Option<HwContext>; limits::CONTEXT_ENGINES],
    /// `BANNABLE`.
    pub bannable: bool,
    /// `RECOVERABLE`.
    pub recoverable: bool,
    /// `PERSISTENCE`.
    pub persistence: bool,
    /// `NO_ERROR_CAPTURE`.
    pub no_error_capture: bool,
    /// `PRIORITY`: bookkeeping only; execlists here are FIFO.
    pub priority: i64,
    /// Requests of it that hung (`guilty_count`).
    pub guilty: u32,
    /// Requests of it lost to a hang (`active_count`).
    pub lost: u32,
    /// Whether a hang banned it: its execbuffers answer `EIO`.
    pub banned: bool,
    /// Destroyed by the program, kept until its requests retire.
    pub closed: bool,
    /// Its requests not yet retired.
    pub in_flight: u32,
}

/// A syncobj's fence: none, one fence, or a timeline's points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncFence {
    /// No fence (`drm_syncobj_replace_fence(NULL)`).
    None,
    /// One fence, not a chain.
    Single(FenceRef),
    /// A `dma_fence_chain`: the fence it was built on, if that was not a
    /// chain, the highest point already folded away as signalled, and the
    /// points in ascending order, each signalling once it and every point
    /// before it have.
    Chain {
        /// The fence below the first point.
        prefix: FenceRef,
        /// Every point up to this value is signalled.
        base: u64,
        /// The points kept apart.
        points: [(u64, FenceRef); limits::POINTS],
        /// How many of `points` there are; at least 1.
        len: u8,
    },
}

/// A syncobj.
#[derive(Clone, Copy, Debug)]
pub struct Syncobj {
    /// The file.
    pub file: u8,
    /// Its fence.
    pub fence: SyncFence,
}

/// Where a request is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestState {
    /// Waiting for its fences or for room on the GPU.
    Queued,
    /// Handed to the back end.
    Submitted,
    /// Finished, with this status.
    Done(i32),
}

/// A request: one execbuffer's batch.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    /// Its id.
    pub id: RequestId,
    /// Where it is.
    pub state: RequestState,
    /// Its context's slot.
    pub context: u16,
    /// The engine slot in the context.
    pub slot: u8,
    /// The engine.
    pub engine: Engine,
    /// The batch's address in the context's VM.
    pub batch: u64,
    /// The batch's length.
    pub length: u32,
    /// The fences it waits on.
    pub deps: [FenceRef; limits::REQUEST_DEPS],
    /// How many of `deps` there are.
    pub dep_count: u8,
}

/// The requests in flight, a ring by id.
pub struct Requests {
    ring: [Option<Request>; limits::REQUESTS],
    /// The next id to give.
    pub next: RequestId,
    /// Every id up to this one has finished and is gone from the ring.
    pub retired: RequestId,
}

impl Requests {
    /// None yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ring: [const { None }; limits::REQUESTS],
            next: 1,
            retired: 0,
        }
    }

    fn index(id: RequestId) -> usize {
        (id % limits::REQUESTS as u64) as usize
    }

    /// Whether another request fits.
    #[must_use]
    pub const fn has_room(&self) -> bool {
        self.next - self.retired <= limits::REQUESTS as u64
    }

    /// Add `request` under the next id, which it is given.
    pub fn push(&mut self, mut request: Request) -> Option<RequestId> {
        if !self.has_room() {
            return None;
        }
        let id = self.next;
        request.id = id;
        *self.ring.get_mut(Self::index(id))? = Some(request);
        self.next += 1;
        Some(id)
    }

    /// Request `id`, if it has not retired.
    #[must_use]
    pub fn get(&self, id: RequestId) -> Option<&Request> {
        self.ring
            .get(Self::index(id))
            .and_then(Option::as_ref)
            .filter(|request| request.id == id)
    }

    /// Request `id`, to change.
    pub fn get_mut(&mut self, id: RequestId) -> Option<&mut Request> {
        self.ring
            .get_mut(Self::index(id))
            .and_then(Option::as_mut)
            .filter(|request| request.id == id)
    }

    /// Take out the oldest request if it has finished.
    pub fn retire_one(&mut self) -> Option<Request> {
        let id = self.retired + 1;
        if id >= self.next {
            return None;
        }
        let slot = self.ring.get_mut(Self::index(id))?;
        match slot {
            Some(request) if request.id == id && matches!(request.state, RequestState::Done(_)) => {
                let taken = slot.take();
                self.retired = id;
                taken
            }
            _ => None,
        }
    }

    /// The ids not yet retired, oldest first.
    pub fn live_ids(&self) -> core::ops::Range<RequestId> {
        self.retired + 1..self.next
    }

    /// A fence's status: `Some` once it has signalled.
    #[must_use]
    pub fn status(&self, fence: FenceRef) -> Option<i32> {
        match fence {
            FenceRef::Signalled(status) => Some(status),
            FenceRef::Request(id) => {
                if id <= self.retired {
                    return Some(0);
                }
                match self.get(id).map(|request| request.state) {
                    Some(RequestState::Done(status)) => Some(status),
                    // An id never given cannot be waited on; it reads as
                    // signalled rather than holding a wait for ever.
                    None if id >= self.next => Some(0),
                    _ => None,
                }
            }
            FenceRef::UpTo(id) => (id <= self.retired).then_some(0),
        }
    }
}

impl core::fmt::Debug for Requests {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "Requests(next {}, retired {})",
            self.next, self.retired
        )
    }
}

impl Default for Requests {
    fn default() -> Self {
        Self::new()
    }
}

/// A kernel fence (a sync file or a dmabuf slot's fence) the front end
/// signals once its fence has.
#[derive(Clone, Copy, Debug)]
pub struct KernelFence {
    /// The cookie the kernel knows it by.
    pub cookie: u64,
    /// What it stands for.
    pub fence: FenceRef,
}

/// Lines said once a boot, so a loop cannot flood the console.
#[derive(Clone, Copy, Default, Debug)]
pub struct Said {
    /// A bit per kind of unserved call already named.
    pub bits: u64,
}

/// Every table.
pub struct Tables<M> {
    /// Open files.
    pub files: Slab<File, { limits::FILES }>,
    /// GEM objects.
    pub objects: Slab<Object<M>, { limits::OBJECTS }>,
    /// Each object slot's generation, bumped when it is freed, so a stale
    /// mmap offset or dmabuf cookie names nothing.
    pub generations: [u8; limits::OBJECTS],
    /// GEM handles.
    pub handles: Slab<Handle, { limits::HANDLES }>,
    /// Bindings.
    pub bindings: Bindings,
    /// VMs.
    pub vms: Slab<Vm, { limits::VMS }>,
    /// VM ids.
    pub vm_names: Slab<VmName, { limits::VM_IDS }>,
    /// Contexts.
    pub contexts: Slab<Context, { limits::CONTEXTS }>,
    /// Syncobjs.
    pub syncobjs: Slab<Syncobj, { limits::SYNCOBJS }>,
    /// Requests.
    pub requests: Requests,
    /// Kernel fences not yet signalled.
    pub kernel_fences: Slab<KernelFence, { limits::KERNEL_FENCES }>,
    /// Held waits.
    pub waits: Waits,
    /// The next kernel fence cookie.
    pub next_cookie: u64,
    /// Lines said once.
    pub said: Said,
    /// Say each failed ioctl, the held waits, their answers, and a wait
    /// held unanswered too long (`ferrix.intelgpu.uapitrace`).
    pub trace: bool,
    /// Why the ioctl being handled was refused, when the refusing site
    /// said; for the `intel: uapi refused` line.
    pub why: &'static str,
    /// The `intel: uapi refused` lines' rate limit.
    pub refusals: Refusals,
}

/// The `intel: uapi refused` lines' rate limit: [`Refusals::BURST`] a
/// second, and how many went unsaid.
#[derive(Clone, Copy, Debug, Default)]
pub struct Refusals {
    /// When the current second began.
    pub window_ns: u64,
    /// Lines said in it.
    pub said: u32,
    /// Lines not said since the last one.
    pub quiet: u32,
}

impl Refusals {
    /// Lines a second.
    pub const BURST: u32 = 8;

    /// Whether a line may be said at `now`; counts it either way.
    pub fn allow(&mut self, now: u64) -> bool {
        if now.saturating_sub(self.window_ns) >= 1_000_000_000 {
            self.window_ns = now;
            self.said = 0;
        }
        if self.said < Self::BURST {
            self.said += 1;
            true
        } else {
            self.quiet = self.quiet.saturating_add(1);
            false
        }
    }
}

impl<M> Tables<M> {
    /// Every table empty.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            files: Slab::new(),
            objects: Slab::new(),
            generations: [0; limits::OBJECTS],
            handles: Slab::new(),
            bindings: Bindings::new(),
            vms: Slab::new(),
            vm_names: Slab::new(),
            contexts: Slab::new(),
            syncobjs: Slab::new(),
            requests: Requests::new(),
            kernel_fences: Slab::new(),
            waits: Waits::new(),
            next_cookie: 1,
            said: Said { bits: 0 },
            trace: false,
            why: "",
            refusals: Refusals {
                window_ns: 0,
                said: 0,
                quiet: 0,
            },
        }
    }

    /// Refuse with `errno`, saying `why` in the `intel: uapi refused` line.
    pub fn refuse(&mut self, why: &'static str, errno: Errno) -> Errno {
        self.why = why;
        errno
    }

    /// The slot of the file the chardev core calls `id`.
    #[must_use]
    pub fn file_slot(&self, id: u64) -> Option<u8> {
        self.files
            .iter()
            .find(|(_, file)| file.id == id)
            .and_then(|(slot, _)| u8::try_from(slot).ok())
    }

    /// The object a file's GEM handle names.
    #[must_use]
    pub fn object_of(&self, file: u8, handle: u32) -> Option<u16> {
        let slot = usize::try_from(handle).ok()?.checked_sub(1)?;
        self.handles
            .get(slot)
            .filter(|entry| entry.file == file)
            .map(|entry| entry.object)
    }

    /// The syncobj a file's handle names.
    #[must_use]
    pub fn syncobj_of(&self, file: u8, handle: u32) -> Option<usize> {
        let slot = usize::try_from(handle).ok()?.checked_sub(1)?;
        self.syncobjs
            .get(slot)
            .filter(|entry| entry.file == file)
            .map(|_| slot)
    }

    /// The context a file's id names: 0 is its default context.
    #[must_use]
    pub fn context_of(&self, file: u8, id: u32) -> Option<u16> {
        let slot = if id == 0 {
            usize::from(self.files.get(usize::from(file))?.default_context)
        } else {
            usize::try_from(id).ok()?.checked_sub(1)?
        };
        self.contexts
            .get(slot)
            .filter(|context| context.file == file && !context.closed)
            .and_then(|_| u16::try_from(slot).ok())
    }

    /// The id a file knows a context by.
    #[must_use]
    pub fn context_id(&self, file: u8, slot: u16) -> u32 {
        match self.files.get(usize::from(file)) {
            Some(entry) if entry.default_context == slot => 0,
            _ => u32::from(slot) + 1,
        }
    }

    /// The VM a file's VM id names.
    #[must_use]
    pub fn vm_of(&self, file: u8, id: u64) -> Option<u16> {
        let slot = usize::try_from(id).ok()?.checked_sub(1)?;
        self.vm_names
            .get(slot)
            .filter(|name| name.file == file)
            .map(|name| name.vm)
    }

    /// A fresh kernel fence cookie.
    pub fn cookie(&mut self) -> u64 {
        let cookie = self.next_cookie;
        self.next_cookie += 1;
        cookie
    }

    /// The dmabuf cookie of object slot `object`: its slot and generation,
    /// so a stale cookie names nothing.
    #[must_use]
    pub fn dmabuf_cookie(&self, object: u16) -> u64 {
        let generation = self
            .generations
            .get(usize::from(object))
            .copied()
            .unwrap_or(0);
        (1 << 32) | (u64::from(generation) << 16) | u64::from(object)
    }

    /// The object a dmabuf cookie names, if it is still that object.
    #[must_use]
    pub fn object_of_cookie(&self, cookie: u64) -> Option<u16> {
        if cookie >> 32 != 1 {
            return None;
        }
        let object = (cookie & 0xffff) as u16;
        let generation = ((cookie >> 16) & 0xff) as u8;
        (self.generations.get(usize::from(object)) == Some(&generation)
            && self.objects.get(usize::from(object)).is_some())
        .then_some(object)
    }

    /// Whether a once-a-boot line of kind `bit` was said; marks it said.
    pub fn say_once(&mut self, bit: u32) -> bool {
        let mask = 1_u64 << (bit % 64);
        let first = self.said.bits & mask == 0;
        self.said.bits |= mask;
        first
    }
}

impl<M> core::fmt::Debug for Tables<M> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Tables")
            .field("files", &self.files)
            .field("objects", &self.objects)
            .field("handles", &self.handles)
            .field("bindings", &self.bindings.as_slice().len())
            .field("vms", &self.vms)
            .field("contexts", &self.contexts)
            .field("syncobjs", &self.syncobjs)
            .field("requests", &self.requests)
            .field("kernel_fences", &self.kernel_fences)
            .field("waits", &self.waits)
            .finish_non_exhaustive()
    }
}

impl<M> Default for Tables<M> {
    fn default() -> Self {
        Self::new()
    }
}
