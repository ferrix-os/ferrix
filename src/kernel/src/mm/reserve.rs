//! Each processor's allocation reserve: how an `Arc` or a map insert is made
//! unable to fail once it has started (finding F-23).
//!
//! # The problem
//!
//! `Arc::new`, `Arc::new_cyclic` and `BTreeMap::insert` allocate inside the
//! standard library, with layouts it does not publish, and have no fallible
//! form on stable Rust. When the heap is empty they call the allocation error
//! handler, which here stops the machine. Nothing outside `alloc` can make
//! those allocations report failure.
//!
//! # The answer
//!
//! Make sure, before one starts, that it cannot fail -- and fail *then*, while
//! failing is still an answer. A [`Reserved`] section is entered with
//! [`reserve`], which tops this processor's [`Reserve`] up to
//! [`RESERVE_DEPTH`] objects of every size class (and holds a large block if
//! the caller names one) and fails with [`AllocError`] if the heap cannot
//! supply them. Inside the section, an allocation the heap refuses is served
//! from the reserve instead of returning null; the section ends when the
//! guard drops. So the operation inside either never starts, and the caller
//! returns `ENOMEM`, or it runs to the end on memory that was set aside for it.
//!
//! The reserve is drawn on only when the heap has refused, so a section on a
//! healthy machine costs masking interrupts and one flag test, and a reserve
//! that was drawn on is refilled by the next section to start on that
//! processor.
//!
//! # Why the reserve is enough
//!
//! **One section, one processor, nothing else.** A section masks interrupts
//! for its length, so nothing else runs on its processor to draw on the same
//! reserve, and no other processor ever reaches it. Sections are short by
//! construction: one `Arc::new` or one map insert, never anything that waits.
//!
//! **Depth.** `ferrix_fallible` bounds what one operation can ask for, and its
//! tests measure the pinned standard library against the bounds: `Arc::new`
//! makes one allocation, of `arc_layout::<T>()`; a `BTreeMap` insert makes at
//! most `height + 2` allocations, each of a node no larger than
//! `btree_node_bound`, which `src/kernel/src/fallible.rs` holds at or below the
//! largest size class at compile time. A B-tree of height `h` has at least
//! `10 * 6^(h - 1)` entries, so [`RESERVE_DEPTH`] of 16 objects per class
//! covers a tree of height 14 -- more than 10^11 entries, more memory than any
//! machine this kernel runs on has -- even when its leaves and internal nodes
//! share a class. An `Arc` too large for a class names its layout, and the
//! section holds one block of that order for it.
//!
//! **Soundness does not rest on either bound.** Every block in a reserve is an
//! ordinary heap object of a whole class or order, and [`Reserve::take`] hands
//! one out only for a request of that same class or order, so it is freed onto
//! the right list whoever frees it. If a bound were wrong, the reserve would
//! run out and the allocation would return null as before: the section would
//! have failed to prevent a stop, not caused a fault.

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_fallible::AllocError;
use ferrix_heap::{Request, Reserve};
use ferrix_sched::MAX_CPUS;
use ferrix_sync::IrqControl;

use super::{KernelPages, with_heap};
use crate::arch;

/// Objects of every size class each processor keeps for its sections. The
/// module documentation argues the number.
pub(crate) const RESERVE_DEPTH: u16 = 16;

/// One processor's reserve and what it is doing with it.
#[derive(Debug)]
struct CpuReserve {
    /// The blocks.
    reserve: Reserve,
    /// Sections open on this processor: nested sections are allowed, and the
    /// reserve is drawn on while any is open.
    sections: u32,
    /// Whether every class holds [`RESERVE_DEPTH`] objects, so that entering
    /// a section need not look.
    full: bool,
}

impl CpuReserve {
    /// An empty reserve with no section open.
    const fn new() -> CpuReserve {
        CpuReserve {
            reserve: Reserve::new(),
            sections: 0,
            full: false,
        }
    }
}

/// Every processor's reserve, indexed by its logical number.
struct Reserves([UnsafeCell<CpuReserve>; MAX_CPUS]);

// SAFETY: (SHARED) each slot is reached only by the processor whose logical number
// indexes it, and only with interrupts masked on that processor (`reserve`,
// `leave` and `draw` mask them before `slot` is called). Nothing else runs on
// a processor whose interrupts are masked, so a slot is never reached from two
// contexts at once.
unsafe impl Sync for Reserves {}

/// The reserves.
static RESERVES: Reserves = Reserves([const { UnsafeCell::new(CpuReserve::new()) }; MAX_CPUS]);

/// Allocations served from a reserve since boot, across processors.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Sections that could not be entered because the heap could not fill the
/// reserve: each one is an `ENOMEM` a caller returned.
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// When set, an allocation inside a section is served from the reserve
/// without asking the heap first -- the heap running dry, as far as the
/// section can tell. For the boot check that proves the reserve serves.
static BYPASS_HEAP: AtomicBool = AtomicBool::new(false);

/// The processor (its logical number plus one) whose sections are all
/// refused, as if the heap could not fill its reserve; zero for none. For the
/// boot check that proves a section that cannot be entered refuses before its
/// operation runs. One processor, so the rest of the machine is untouched.
static REFUSE_FILL: AtomicU64 = AtomicU64::new(0);

/// This processor's slot. Interrupts must be masked.
fn slot() -> Option<&'static UnsafeCell<CpuReserve>> {
    let cpu = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    RESERVES.0.get(cpu)
}

/// An open reserved section. Interrupts stay masked on this processor until
/// it drops, so it must not be held across anything that waits.
///
/// Drop sections in the reverse of the order they were entered, which a
/// `let` binding does by itself: each one restores the interrupt state from
/// before it.
#[derive(Debug)]
#[must_use = "the section ends when this is dropped"]
pub(crate) struct Reserved {
    /// The interrupt state to restore.
    saved: usize,
    /// A section belongs to its processor: not `Send`.
    _here: PhantomData<*const ()>,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        leave();
        <arch::Irq as IrqControl>::restore(self.saved);
    }
}

/// Enter a reserved section: fill this processor's reserve, and hold a block
/// for `large` if it is too big for a size class.
///
/// # Errors
///
/// [`AllocError`] when the heap cannot fill the reserve. Nothing has been
/// started, and the caller reports it.
pub(crate) fn reserve(large: Option<Layout>) -> Result<Reserved, AllocError> {
    let saved = <arch::Irq as IrqControl>::disable();
    match enter(large) {
        Ok(()) => Ok(Reserved {
            saved,
            _here: PhantomData,
        }),
        Err(error) => {
            let _ = REFUSED.fetch_add(1, Ordering::Relaxed);
            <arch::Irq as IrqControl>::restore(saved);
            Err(error)
        }
    }
}

/// Fill this processor's reserve and count a section open.
fn enter(large: Option<Layout>) -> Result<(), AllocError> {
    let refused = REFUSE_FILL.load(Ordering::Acquire);
    if refused != 0 && Some(refused) == this_cpu_tag() {
        return Err(AllocError);
    }
    let cell = slot().ok_or(AllocError)?;
    // SAFETY: (SHARED) this processor's own slot, with interrupts masked by `reserve`:
    // see `Reserves`. The reference ends with this function, and nothing in
    // it allocates through the global allocator, which is the only other
    // place that reaches the slot.
    let here = unsafe { &mut *cell.get() };
    if !here.full {
        with_heap(|heap| here.reserve.fill(heap, &mut KernelPages, RESERVE_DEPTH))
            .map_err(|_| AllocError)?;
        here.full = true;
    }
    if let Some(layout) = large {
        let request = Request::new(layout.size(), layout.align());
        with_heap(|heap| here.reserve.hold_large(heap, &mut KernelPages, request))
            .map_err(|_| AllocError)?;
    }
    here.sections = here.sections.saturating_add(1);
    Ok(())
}

/// Fill this processor's reserve now, rather than at its first section: for
/// bring-up, so that what the reserve holds is there before anything
/// measures the heap.
///
/// # Errors
///
/// [`AllocError`] when the heap cannot fill it.
pub(crate) fn fill_reserve() -> Result<(), AllocError> {
    let section = reserve(None)?;
    drop(section);
    Ok(())
}

/// Count a section closed, and give back a large block the outermost one did
/// not use: whole pages are not kept out of the frame allocator between
/// sections.
fn leave() {
    let Some(cell) = slot() else { return };
    // SAFETY: (SHARED) as in `enter`; the guard being dropped kept interrupts masked.
    let here = unsafe { &mut *cell.get() };
    here.sections = here.sections.saturating_sub(1);
    if here.sections == 0 {
        with_heap(|heap| here.reserve.release_large(heap, &mut KernelPages));
    }
}

/// A block for `request` from this processor's reserve, if a section is open
/// here and the reserve holds one. Called by the global allocator when the
/// heap has refused.
pub(super) fn draw(request: Request) -> Option<u64> {
    let saved = <arch::Irq as IrqControl>::disable();
    let block = slot().and_then(|cell| {
        // SAFETY: (SHARED) as in `enter`, with interrupts masked just above. The
        // allocator calling this holds no reference to the slot.
        let here = unsafe { &mut *cell.get() };
        if here.sections == 0 {
            return None;
        }
        let block = here.reserve.take(&KernelPages, request)?;
        here.full = false;
        Some(block)
    });
    <arch::Irq as IrqControl>::restore(saved);
    if block.is_some() {
        let _ = DRAWN.fetch_add(1, Ordering::Relaxed);
    }
    block
}

/// Whether the allocator should go to the reserve without asking the heap.
pub(super) fn bypassing_heap() -> bool {
    BYPASS_HEAP.load(Ordering::Relaxed)
}

/// Make every section behave as if the heap had run dry, or stop. For the
/// boot check in `object/alloc_check.rs`, which proves an operation inside a
/// section completes on the reserve alone.
pub(crate) fn bypass_heap_in_sections(on: bool) {
    BYPASS_HEAP.store(on, Ordering::Release);
}

/// This processor's logical number plus one, as [`REFUSE_FILL`] holds it.
fn this_cpu_tag() -> Option<u64> {
    let cpu = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    u64::try_from(cpu).ok()?.checked_add(1)
}

/// Make every section on this processor refused, as if the heap could not
/// fill its reserve, or stop. For the boot check in `object/alloc_check.rs`,
/// which runs with preemption off across the window.
pub(crate) fn refuse_reserve_fills(on: bool) {
    let tag = if on { this_cpu_tag().unwrap_or(0) } else { 0 };
    REFUSE_FILL.store(tag, Ordering::Release);
}

/// Allocations served from a reserve, and sections refused, since boot.
pub(crate) fn reserve_counts() -> (u64, u64) {
    (
        DRAWN.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}
