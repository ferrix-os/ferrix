//! VT-d queued invalidation: the 128-bit descriptors a remapping unit's
//! invalidation queue holds, and the queue's registers' encodings.
//!
//! Reference: Intel Virtualization Technology for Directed I/O, Architecture
//! Specification, "Queued Invalidation Interface" and its descriptor layouts;
//! checked against QEMU's `hw/i386/intel_iommu.c` and
//! `intel_iommu_internal.h`, whose reserved-bit masks a descriptor here must
//! pass (`VTD_INV_DESC_*_RSVD*`), or the unit stops its queue with `IQE`.
//!
//! The kernel uses the legacy 128-bit descriptors (`IQA.DW` = 0) in one
//! 4 KiB queue (`IQA.QS` = 0): 256 of them. Every invalidation the kernel
//! makes is one descriptor and a wait descriptor behind it, so the queue is
//! never more than a few deep.

pub use super::unit::Drain;

/// One descriptor: its low quadword, then its high one, as the unit reads
/// them from the queue.
pub type Descriptor = [u64; 2];

/// Bytes of one descriptor.
pub const DESCRIPTOR_BYTES: u64 = 16;

/// Descriptors in the kernel's queue: one 4 KiB frame of them.
pub const QUEUE_LENGTH: u32 = 256;

/// Descriptor type: context-cache invalidate.
const TYPE_CONTEXT: u64 = 0x1;
/// Descriptor type: IOTLB invalidate.
const TYPE_IOTLB: u64 = 0x2;
/// Descriptor type: interrupt entry cache invalidate.
const TYPE_INTERRUPT_ENTRY: u64 = 0x4;
/// Descriptor type: invalidation wait.
const TYPE_WAIT: u64 = 0x5;

/// Context-cache and IOTLB granularity, bits 5:4: every entry.
const GRANULARITY_GLOBAL: u64 = 1 << 4;
/// IOTLB granularity: one domain, named in bits 31:16.
const GRANULARITY_DOMAIN: u64 = 2 << 4;
/// Context-cache granularity: one source ID (bits 47:32) in one domain
/// (bits 31:16).
const GRANULARITY_DEVICE: u64 = 3 << 4;

/// IOTLB invalidate: drain writes.
const DRAIN_WRITE: u64 = 1 << 6;
/// IOTLB invalidate: drain reads.
const DRAIN_READ: u64 = 1 << 7;

/// Interrupt entry cache invalidate: one index (bits 47:32), not every
/// entry.
const INTERRUPT_ENTRY_INDEX: u64 = 1 << 4;

/// Wait: raise the invalidation completion event (`ICS.IWC`).
const WAIT_INTERRUPT: u64 = 1 << 4;
/// Wait: write the status data (bits 63:32) to the status address.
const WAIT_STATUS_WRITE: u64 = 1 << 5;
/// Wait: fence, so no later descriptor is processed before every earlier
/// one has completed.
const WAIT_FENCE: u64 = 1 << 6;

/// What a context-cache invalidation reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContextScope {
    /// Every context entry the unit cached.
    Global,
    /// The entry of one source ID, cached under one domain identifier.
    Device {
        /// The source ID: bus, device and function.
        source: u16,
        /// The domain identifier it was cached under: the domain's own for
        /// an entry that was present, 0 for one cached not present in
        /// caching mode.
        domain: u16,
    },
}

/// What an IOTLB invalidation reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IotlbScope {
    /// Every translation the unit cached.
    Global,
    /// Every translation of one domain.
    Domain(u16),
}

/// What an interrupt entry cache invalidation reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InterruptEntryScope {
    /// Every interrupt remapping table entry the unit cached.
    Global,
    /// One entry, by its index.
    Index(u16),
}

/// How a wait descriptor tells the kernel it completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Completion {
    /// The unit writes `data` to the 32-bit word at physical `address`
    /// (`SW` = 1), the only completion the kernel waits for.
    Status {
        /// The word's physical address, four-byte aligned.
        address: u64,
        /// The value written: a sequence number of the unit's, so a late
        /// completion of an earlier wait is never taken for this one.
        data: u32,
    },
    /// No status written (`SW` = 0): the unit raises its completion event
    /// (`IF` = 1), which the kernel keeps masked and never reads. A wait the
    /// kernel can never see complete, for the boot check that a failed
    /// invalidation releases nothing (`docs/NVIDIA.md` §12.3, check R7);
    /// `IF` is set so that the descriptor is still a valid one and the
    /// queue does not stop on it.
    Unwritten,
}

/// A context-cache invalidation of `scope`.
#[must_use]
pub const fn context(scope: ContextScope) -> Descriptor {
    match scope {
        ContextScope::Global => [TYPE_CONTEXT | GRANULARITY_GLOBAL, 0],
        ContextScope::Device { source, domain } => [
            TYPE_CONTEXT | GRANULARITY_DEVICE | (domain as u64) << 16 | (source as u64) << 32,
            0,
        ],
    }
}

/// An IOTLB invalidation of `scope`, draining as `drain` says: `DR` (bit 7)
/// and `DW` (bit 6) make the invalidation complete only once the reads and
/// writes that used the old translations have, which the kernel asks for
/// whenever the unit can drain (`CAP.DRD`, `CAP.DWD`), as Linux's
/// `qi_flush_iotlb` does. Without it a DMA in flight through a mapping just
/// taken away could land after the wait, in a frame already given back.
#[must_use]
pub const fn iotlb(scope: IotlbScope, drain: Drain) -> Descriptor {
    let mut low = match scope {
        IotlbScope::Global => TYPE_IOTLB | GRANULARITY_GLOBAL,
        IotlbScope::Domain(domain) => TYPE_IOTLB | GRANULARITY_DOMAIN | (domain as u64) << 16,
    };
    if drain.read {
        low |= DRAIN_READ;
    }
    if drain.write {
        low |= DRAIN_WRITE;
    }
    [low, 0]
}

/// An interrupt entry cache invalidation of `scope`, with no index mask.
#[must_use]
pub const fn interrupt_entry(scope: InterruptEntryScope) -> Descriptor {
    match scope {
        InterruptEntryScope::Global => [TYPE_INTERRUPT_ENTRY, 0],
        InterruptEntryScope::Index(index) => [
            TYPE_INTERRUPT_ENTRY | INTERRUPT_ENTRY_INDEX | (index as u64) << 32,
            0,
        ],
    }
}

/// A wait, fenced, that completes as `completion` says.
#[must_use]
pub const fn wait(completion: Completion) -> Descriptor {
    match completion {
        Completion::Status { address, data } => [
            TYPE_WAIT | WAIT_STATUS_WRITE | WAIT_FENCE | (data as u64) << 32,
            address & !0b11,
        ],
        Completion::Unwritten => [TYPE_WAIT | WAIT_INTERRUPT | WAIT_FENCE, 0],
    }
}

/// What `IQA` holds for a queue of [`QUEUE_LENGTH`] 128-bit descriptors in
/// the 4 KiB frame at `frame`: its address, `QS` = 0 and `DW` = 0.
#[must_use]
pub const fn queue_address(frame: u64) -> u64 {
    frame & !0xFFF
}

/// What `IQT` holds to say the queue's next free slot is `index`, and what
/// `IQH` holds when the unit's next descriptor to fetch is: bits 18:4.
#[must_use]
pub const fn slot_register(index: u32) -> u64 {
    ((index % QUEUE_LENGTH) as u64) << 4
}

/// The slot an `IQH` or `IQT` value names.
#[must_use]
pub const fn slot_of(register: u64) -> u32 {
    ((register >> 4) & 0x7FFF) as u32 % QUEUE_LENGTH
}

/// Free slots in the queue between the unit's head and the kernel's tail,
/// one always left empty so that a full queue is not mistaken for an empty
/// one.
#[must_use]
pub const fn free_slots(head: u32, tail: u32) -> u32 {
    let used = (tail + QUEUE_LENGTH - head % QUEUE_LENGTH) % QUEUE_LENGTH;
    QUEUE_LENGTH - 1 - used
}

/// `FSTS`: invalidation queue error, the unit stopped at a descriptor it
/// would not take. Write one to clear.
pub const FSTS_IQE: u32 = 1 << 4;
/// `FSTS`: invalidation completion error, a device-TLB invalidation that
/// did not complete. Write one to clear.
pub const FSTS_ICE: u32 = 1 << 5;
/// `FSTS`: invalidation time-out error; the unit stops its queue as for
/// [`FSTS_IQE`]. Write one to clear.
pub const FSTS_ITE: u32 = 1 << 6;
/// The three queue errors, each write-one-to-clear.
pub const FSTS_QUEUE_ERRORS: u32 = FSTS_IQE | FSTS_ICE | FSTS_ITE;

/// What a wait for a submission came to, from the fault status register
/// read after it and whether its status word was written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The status word was written and no queue error is set.
    Completed,
    /// The status word was not written within the patience.
    TimedOut,
    /// `ICE`: the invalidation failed, the queue goes on. The bit is to be
    /// cleared at once, or it would fail every later wait without the unit
    /// ever being marked failed.
    CompletionError,
    /// `IQE` or `ITE`: the unit stopped its queue, and is to be marked
    /// failed.
    QueueStopped,
}

/// What a wait came to: a stopped queue first, whatever else is set, then a
/// completion error, then the status word.
#[must_use]
pub const fn outcome(fsts: u32, completed: bool) -> Outcome {
    if fsts & (FSTS_IQE | FSTS_ITE) != 0 {
        Outcome::QueueStopped
    } else if fsts & FSTS_ICE != 0 {
        Outcome::CompletionError
    } else if completed {
        Outcome::Completed
    } else {
        Outcome::TimedOut
    }
}
