//! Per-CPU data.
//!
//! Stage 4 of `docs/ROADMAP.md`. One [`PerCpu`] per processor, reached through
//! a register the architecture reserves for it — `IA32_GS_BASE` on x86-64,
//! `TPIDR_EL1` on AArch64 — so that "which CPU am I on" costs one load rather
//! than a search through a table.
//!
//! # What belongs here and what does not
//!
//! The test is whether sharing the field between CPUs would be a correctness
//! bug or merely a contention one. A frame cache is here because two CPUs
//! handing each other the same frame is a correctness bug. A counter of
//! interrupts taken is here because the alternative is a cache line that every
//! CPU writes on every interrupt, which is the contention case — and stage 14
//! measures wake-up latency, so a line bouncing between cores on the interrupt
//! path is a latency bug waiting to be blamed on something else.
//!
//! # Why the frame cache is behind a lock at all
//!
//! It is per-CPU, so no other CPU contends for it. What the lock is actually
//! bought for is the *interrupt* masking: a page fault on this CPU allocates,
//! and a fault that arrived halfway through a cache update would see it torn.
//! An uncontended [`IrqSpinLock`] is two atomic operations and a `cli`, which
//! is what that safety would cost hand-rolled anyway — and this way it is not
//! `unsafe`.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_frame::Frame;
use ferrix_sync::IrqSpinLock;

/// Processors this kernel will track.
///
/// A fixed array rather than a `Vec` because the registry is read from
/// interrupt handlers — a TLB shootdown walks it — and a growable structure
/// read from an interrupt handler is exactly the read-mostly problem
/// [`crate::smp::synchronize`] exists to solve. When that is needed for
/// something real, this becomes its first user.
pub(crate) const MAX_CPUS: usize = 64;

/// How the per-CPU frame cache masks interrupts while it is held.
type Lock<T> = IrqSpinLock<T, crate::arch::Irq>;

/// Single frames held on this CPU rather than in the buddy allocator.
///
/// Only order zero. A cache of larger blocks would have to be indexed by order
/// and would mostly sit empty: every allocation on the fault path — a page, a
/// page table — is one frame, and that is the path where the global lock hurts.
#[derive(Debug)]
pub(crate) struct FrameCache {
    /// The frames themselves.
    frames: [Frame; CACHE_CAPACITY],
    /// How many of them are live, from the front.
    held: usize,
}

/// Frames a CPU may hold.
///
/// Small on purpose. Every frame in here is one the rest of the machine cannot
/// have, so the cache is sized to absorb a burst on the fault path rather than
/// to be a second allocator.
const CACHE_CAPACITY: usize = 32;

/// How many frames a refill takes from the buddy allocator, and a drain gives
/// back.
///
/// Half the cache, so a workload oscillating across the boundary does not pay
/// a global-lock round trip per allocation — which is the whole reason the
/// cache exists.
pub(crate) const BATCH: usize = CACHE_CAPACITY / 2;

impl FrameCache {
    /// An empty cache.
    const fn new() -> FrameCache {
        FrameCache {
            frames: [0; CACHE_CAPACITY],
            held: 0,
        }
    }

    /// Take a frame, if there is one.
    fn pop(&mut self) -> Option<Frame> {
        let index = self.held.checked_sub(1)?;
        self.held = index;
        self.frames.get(index).copied()
    }

    /// Put a frame back, or report that there is no room.
    fn push(&mut self, frame: Frame) -> bool {
        let Some(slot) = self.frames.get_mut(self.held) else {
            return false;
        };
        *slot = frame;
        self.held += 1;
        true
    }

    /// How many frames are held.
    const fn len(&self) -> usize {
        self.held
    }
}

/// One processor's private data.
///
/// `repr(C)` and `self_pointer` first because x86-64 reaches this through a
/// segment base: the address is not in a general register, so the way to get
/// one is to load the pointer the structure keeps to itself at offset zero.
/// AArch64 has the address in `TPIDR_EL1` directly and does not need the
/// indirection, but a layout that differs per architecture would be a layout
/// somebody has to remember.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct PerCpu {
    /// This structure's own address. See the type's documentation.
    self_pointer: usize,
    /// This CPU's number, counted from zero in the order firmware listed them.
    pub(crate) index: u32,
    /// What the interrupt controller calls this CPU: a local APIC identifier
    /// or an `MPIDR_EL1` affinity value.
    pub(crate) hardware_id: u64,
    /// The last grace period this CPU passed through. See
    /// [`crate::smp::synchronize`].
    pub(crate) quiescent: AtomicU64,
    /// Inter-processor interrupts this CPU has taken.
    pub(crate) ipis: AtomicU64,
    /// TLB invalidations other CPUs have asked this one for.
    ///
    /// A counter rather than a flag, and the pair below is not one counter with
    /// two readers: an initiator has to be able to say "the flush I asked for",
    /// not "a flush", or two shootdowns in flight let the second one's
    /// completion answer the first one's wait.
    pub(crate) flush_requested: AtomicU64,
    /// TLB invalidations this CPU has actually done.
    pub(crate) flush_completed: AtomicU64,
    /// Frames held here rather than in the buddy allocator.
    cache: Lock<FrameCache>,
}

impl PerCpu {
    /// A CPU's data, not yet installed.
    pub(crate) const fn new(index: u32, hardware_id: u64) -> PerCpu {
        PerCpu {
            self_pointer: 0,
            index,
            hardware_id,
            quiescent: AtomicU64::new(0),
            ipis: AtomicU64::new(0),
            flush_requested: AtomicU64::new(0),
            flush_completed: AtomicU64::new(0),
            cache: Lock::new(FrameCache::new()),
        }
    }

    /// Take a cached frame.
    pub(crate) fn take_frame(&self) -> Option<Frame> {
        self.cache.lock().pop()
    }

    /// Offer a frame to the cache, which returns it if it is full.
    pub(crate) fn give_frame(&self, frame: Frame) -> Option<Frame> {
        if self.cache.lock().push(frame) {
            None
        } else {
            Some(frame)
        }
    }

    /// Frames this CPU is holding.
    pub(crate) fn cached_frames(&self) -> usize {
        self.cache.lock().len()
    }

    /// Fill the cache from `supply`, which is asked for one frame at a time and
    /// may run out.
    pub(crate) fn refill(&self, mut supply: impl FnMut() -> Option<Frame>) {
        let mut cache = self.cache.lock();
        for _ in 0..BATCH {
            let Some(frame) = supply() else { break };
            if !cache.push(frame) {
                break;
            }
        }
    }

    /// Hand every cached frame to `sink`.
    ///
    /// Called before a CPU stops and by the frame accounting in
    /// [`crate::mm`], which cannot tell the difference between a frame in a
    /// cache and a frame that leaked — so there is a way to empty every cache
    /// and make the count mean what it meant before stage 4.
    pub(crate) fn drain(&self, mut sink: impl FnMut(Frame)) -> usize {
        let mut cache = self.cache.lock();
        let mut returned = 0;
        while let Some(frame) = cache.pop() {
            sink(frame);
            returned += 1;
        }
        returned
    }
}

// SAFETY: every field is either immutable after construction (`index`,
// `hardware_id`, `self_pointer`) or an atomic or lock that synchronises its own
// access. Sharing is deliberate: a TLB shootdown reads another CPU's record to
// find out whether it is online, and stage 5 will steal work from another
// CPU's runqueue through exactly this reference.
unsafe impl Sync for PerCpu {}

/// Every CPU's data, by index.
///
/// Addresses rather than references, because the array has to be a `static` and
/// the records are allocated at bring-up.
static REGISTRY: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

/// Records in the registry.
static REGISTERED: AtomicUsize = AtomicUsize::new(0);

/// Publish `cpu` in the registry and install it on the CPU it describes.
///
/// The registry entry is published *before* the pointer register is written, so
/// that a CPU which starts taking interrupts the moment it is installed is
/// already findable by the CPU that started it.
pub(crate) fn install(cpu: &'static mut PerCpu) {
    cpu.self_pointer = (&raw const *cpu) as usize;
    let index = cpu.index as usize;
    let address = cpu.self_pointer;

    if let Some(slot) = REGISTRY.get(index) {
        slot.store(address, Ordering::Release);
        let _ = REGISTERED.fetch_add(1, Ordering::AcqRel);
    }
    // SAFETY: `address` is a `&'static mut PerCpu` leaked into the registry
    // above, so it stays valid and is now reachable only through this register
    // and through `of`. Nothing else writes the pointer register on this CPU.
    unsafe { crate::arch::install_percpu(address) };
}

/// This CPU's data, or `None` before it has been installed.
pub(crate) fn current() -> Option<&'static PerCpu> {
    let address = crate::arch::current_percpu();
    if address == 0 {
        return None;
    }
    // SAFETY: the pointer register holds what `install` put there, which is the
    // address of a leaked `PerCpu` that lives for the rest of the system's
    // life. `PerCpu` is `Sync`, so a shared reference from several CPUs is
    // sound.
    Some(unsafe { &*(address as *const PerCpu) })
}

/// The record for CPU `index`, if one has been published.
pub(crate) fn of(index: usize) -> Option<&'static PerCpu> {
    let address = REGISTRY.get(index)?.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }
    // SAFETY: as `current`. The entry was published by `install` with a release
    // store and is read here with an acquire load, so the record's construction
    // happens-before this reference to it.
    Some(unsafe { &*(address as *const PerCpu) })
}

/// Run `body` on every CPU that has published a record.
pub(crate) fn for_each(mut body: impl FnMut(&'static PerCpu)) {
    for index in 0..MAX_CPUS {
        if let Some(cpu) = of(index) {
            body(cpu);
        }
    }
}

/// How many CPUs have published a record.
pub(crate) fn count() -> usize {
    REGISTERED.load(Ordering::Acquire)
}
