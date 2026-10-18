//! Per-CPU data.
//!
//! One array, indexed by the CPU number [`crate::smp::this_cpu`] returns, with
//! each slot padded out to its own cache line. Not a segment register and a
//! linker section, which is what Linux does and what x86-64 makes cheap through
//! `GS`: that buys one instruction per access and costs a second way for a
//! pointer to be wrong, and nothing in this kernel is yet doing per-CPU accesses
//! often enough for the instruction to matter. When something is —
//! the scheduler's runqueue, stage 5 — this module changes and its callers do
//! not, which is the reason it exists as a module at all.
//!
//! **The padding is not a micro-optimisation.** Two CPUs incrementing two
//! counters in the same cache line serialise on the cache coherence protocol as
//! completely as if they shared one counter; the only difference is that the
//! answer comes out right, so nothing ever tells you. Stage 4's own exit
//! criterion — a contended counter across every core — would measure the
//! padding rather than the lock if the slots shared a line.

use core::sync::atomic::{AtomicUsize, Ordering};

/// CPUs this kernel can bring up.
///
/// A fixed bound rather than an allocation, because per-CPU state has to exist
/// before the CPU that owns it does, and because the roadmap's rule against
/// fixed-size tables is about the *process* table — a machine does not grow
/// cores while it runs. Raising it costs `MAX_CPUS * 64` bytes of `.bss` per
/// per-CPU variable.
pub(crate) const MAX_CPUS: usize = 8;

/// A cache line on both architectures this kernel builds for.
///
/// x86-64 is 64 bytes everywhere. `AArch64` varies — Apple's cores use 128 —
/// but `CTR_EL0.CWG` is the architectural answer and 64 is the value QEMU's
/// `cortex-a72` reports. Over-aligning would waste memory; under-aligning would
/// silently reintroduce the sharing this exists to prevent, so this is the one
/// constant here worth revisiting on real hardware.
pub(crate) const CACHE_LINE: usize = 64;

/// One CPU's slot, alone on its cache line.
#[repr(align(64))]
#[derive(Debug)]
pub(crate) struct Padded<T>(pub(crate) T);

const _: () = assert!(
    align_of::<Padded<u8>>() == CACHE_LINE,
    "the padding has to be a cache line, or two CPUs share one"
);

/// A value per CPU.
#[derive(Debug)]
pub(crate) struct PerCpu<T> {
    slots: [Padded<T>; MAX_CPUS],
}

impl<T> PerCpu<T> {
    /// Wrap an array of already-padded slots.
    ///
    /// Built from an array rather than by cloning a prototype because the
    /// things that go in here are atomics, which are not `Copy` — the caller
    /// writes `[const { Padded(AtomicU64::new(0)) }; MAX_CPUS]`, which is a
    /// `const` expression and so lives in `.bss` rather than being written out
    /// at boot.
    pub(crate) const fn new(slots: [Padded<T>; MAX_CPUS]) -> PerCpu<T> {
        PerCpu { slots }
    }

    /// This CPU's slot.
    #[expect(
        clippy::indexing_slicing,
        reason = "AUDIT: the index is clamped to the array's last element on \
                  the line above, so it is in range by construction. The \
                  alternative is an `Option` on every per-CPU read, including \
                  the ones on the interrupt path, to describe a case that \
                  cannot arise."
    )]
    pub(crate) fn mine(&self) -> &T {
        let cpu = crate::smp::this_cpu().min(MAX_CPUS - 1);
        &self.slots[cpu].0
    }

    /// A particular CPU's slot.
    pub(crate) fn get(&self, cpu: usize) -> Option<&T> {
        self.slots.get(cpu).map(|slot| &slot.0)
    }

    /// Every slot, in CPU order, whether or not that CPU is online.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().map(|slot| &slot.0)
    }
}

// SAFETY: the slots are only ever handed out by shared reference, so `T`'s own
// rules govern what may be done with one. Every `T` this kernel puts here is an
// atomic, and the type system requires `T: Sync` for the static to exist at all.
unsafe impl<T: Sync> Sync for PerCpu<T> {}

/// Which CPU is which.
///
/// The hardware's identifier for a core — the local APIC ID on x86-64, `MPIDR`
/// on `AArch64` — is not a small dense integer and is not required to be
/// either, so it cannot index an array. This is the translation: the boot CPU
/// claims slot 0 and each processor firmware describes claims the next, in the
/// order the MADT lists them, which is the same order on every boot.
static IDENTIFIERS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(NONE) }; MAX_CPUS];

/// No CPU has claimed this slot.
///
/// `usize::MAX` rather than zero: zero is a perfectly ordinary APIC ID, and is
/// in fact the boot CPU's on every machine anyone has.
const NONE: usize = usize::MAX;

/// How many slots have been claimed.
static ASSIGNED: AtomicUsize = AtomicUsize::new(0);

/// Give `identifier` the next free CPU number.
///
/// Called once per processor the firmware tables describe, on the boot CPU,
/// before any of them is started — so the mapping is complete before the first
/// AP can look itself up in it.
pub(crate) fn assign(identifier: usize) -> Option<usize> {
    let index = ASSIGNED.load(Ordering::Relaxed);
    let slot = IDENTIFIERS.get(index)?;
    slot.store(identifier, Ordering::Release);
    ASSIGNED.store(index + 1, Ordering::Release);
    Some(index)
}

/// The CPU number for a hardware identifier, or `None` if it was never
/// assigned one.
pub(crate) fn index_of(identifier: usize) -> Option<usize> {
    IDENTIFIERS
        .iter()
        .position(|slot| slot.load(Ordering::Acquire) == identifier)
}

/// The hardware identifier of a CPU number.
pub(crate) fn identifier_of(cpu: usize) -> Option<usize> {
    match IDENTIFIERS.get(cpu)?.load(Ordering::Acquire) {
        NONE => None,
        identifier => Some(identifier),
    }
}

/// How many CPUs have a number.
///
/// Every processor the firmware tables describe, which is not the same as the
/// number that came up — see [`crate::smp::online`].
pub(crate) fn assigned() -> usize {
    ASSIGNED.load(Ordering::Acquire)
}
