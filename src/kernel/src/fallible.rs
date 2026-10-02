//! Allocation that reports failure, for the certified item (finding F-23).
//!
//! The certified item allocates through this module and nowhere else, and
//! `tools/common/check/check-fallible-alloc.py` holds it to that: a call to an
//! allocating standard-library API in the item's product code fails the
//! build unless it is argued at the site. Everything here returns
//! [`AllocError`] when memory has run out, and each caller turns that into
//! the answer its interface has for it -- `NO_MEMORY` from a native call,
//! `ENOMEM` from a Linux one, a refused bring-up step at boot.
//!
//! Two mechanisms, because the standard library offers two kinds of
//! allocation:
//!
//! * `Box`, `Vec`, `VecDeque` and `String` are made fallible directly, from
//!   stable parts, in `src/lib/kernel/fallible` where the host tests and Miri reach
//!   them. They are re-exported here unchanged.
//! * `Arc` and the ordered maps allocate inside `alloc` and cannot be. They
//!   run inside a reserved section (`mm/reserve.rs`): this processor's
//!   reserve is filled first, which is where the failure is reported, and the
//!   allocation itself is then served from the reserve if the heap refuses.
//!
//! # Failure injection
//!
//! [`inject`] makes every `n`th fallible allocation of one task fail, as if
//! memory had run out, without touching the heap. `object/alloc_check.rs`
//! drives the native ABI with it on every boot and requires every call to
//! either succeed or answer `NO_MEMORY`, and the kernel to be intact after.
//! [`inject_once`] fails the `n`th alone, so that `user/alloc_check.rs` can
//! fail each allocation an operation makes in turn, one run per allocation;
//! asked to, it counts the frame allocator's allocations among them
//! ([`frame_refused`]), because a frame refused is the other way memory runs
//! out and the paths that meet it are as much the item's as a heap
//! allocation's.
//! Scoped to one task, so the rest of the machine -- which includes the
//! uncertified load, whose allocations are still infallible -- is untouched.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub(crate) use ferrix_fallible::{
    AllocError, push_within, try_boxed_filled, try_boxed_slice, try_boxed_str, try_collect,
    try_deque_with_capacity, try_extend, try_extend_from_slice, try_filled, try_format, try_push,
    try_reserve, try_reserve_deque, try_string, try_to_vec, try_with_capacity,
};
use ferrix_fallible::{btree_node_bound, check};
use ferrix_heap::LARGEST_CLASS;

pub(crate) use crate::mm::Reserved;
use crate::sched::TaskId;

/// A boxed value, fallibly.
pub(crate) use ferrix_fallible::try_box;
/// `vec.insert(index, value)`, fallibly: named apart from the map [`insert`].
pub(crate) use ferrix_fallible::try_insert as try_insert_at;

/// Whether every node of a `BTreeMap<K, V>` fits a heap size class, which is
/// what the reserve holds.
const fn node_fits<K, V>() -> bool {
    let align = if align_of::<K>() > align_of::<V>() {
        align_of::<K>()
    } else {
        align_of::<V>()
    };
    btree_node_bound(size_of::<K>(), size_of::<V>(), align) <= LARGEST_CLASS
}

/// Enter a reserved section, after asking the injection policy.
///
/// `layout` is the one allocation the section will make, when it is known;
/// only one too large for a size class needs holding specially.
fn section(layout: Option<Layout>) -> Result<Reserved, AllocError> {
    check()?;
    crate::mm::reserve(layout.filter(|layout| layout.size() > LARGEST_CLASS))
}

/// A reserved section, for a caller that must not lose what it is inserting
/// if memory has run out: enter this first, then [`insert_held`].
///
/// Interrupts are masked until the guard drops. Hold it across nothing that
/// waits.
///
/// # Errors
///
/// [`AllocError`] when the reserve cannot be filled.
pub(crate) fn reserve() -> Result<Reserved, AllocError> {
    section(None)
}

/// `Arc::new(value)`.
///
/// # Errors
///
/// [`AllocError`]; `value` is dropped.
pub(crate) fn try_arc<T>(value: T) -> Result<Arc<T>, AllocError> {
    let _held = section(ferrix_fallible::arc_layout::<T>())?;
    Ok(Arc::new(value))
}

/// `Arc::new_cyclic(make)`.
///
/// `make` runs inside the section, with interrupts masked: it builds a value
/// and allocates nothing more than the reserve's depth allows, and waits for
/// nothing.
///
/// # Errors
///
/// [`AllocError`]; `make` has not run.
pub(crate) fn try_arc_cyclic<T>(make: impl FnOnce(&Weak<T>) -> T) -> Result<Arc<T>, AllocError> {
    let _held = section(ferrix_fallible::arc_layout::<T>())?;
    Ok(Arc::new_cyclic(make))
}

/// `map.insert(key, value)`.
///
/// # Errors
///
/// [`AllocError`]; `key` and `value` are dropped. A caller whose value must
/// not be dropped -- one that owns a frame, say -- enters [`reserve`] first
/// and uses [`insert_held`].
pub(crate) fn insert<K: Ord, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
) -> Result<Option<V>, AllocError> {
    let held = reserve()?;
    Ok(insert_held(&held, map, key, value))
}

/// `map.insert(key, value)` inside a section the caller has entered.
pub(crate) fn insert_held<K: Ord, V>(
    _held: &Reserved,
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
) -> Option<V> {
    const {
        assert!(
            node_fits::<K, V>(),
            "a node of this map is larger than any size class the reserve holds"
        );
    }
    map.insert(key, value)
}

/// `set.insert(value)`.
///
/// # Errors
///
/// [`AllocError`]; `value` is dropped.
pub(crate) fn insert_into_set<T: Ord>(set: &mut BTreeSet<T>, value: T) -> Result<bool, AllocError> {
    let held = reserve()?;
    Ok(insert_into_set_held(&held, set, value))
}

/// `set.insert(value)` inside a section the caller has entered.
pub(crate) fn insert_into_set_held<T: Ord>(
    _held: &Reserved,
    set: &mut BTreeSet<T>,
    value: T,
) -> bool {
    const {
        assert!(
            node_fits::<T, ()>(),
            "a node of this set is larger than any size class the reserve holds"
        );
    }
    set.insert(value)
}

/// The [`ferrix_fallible::Section`] for library crates, which cannot name
/// [`reserve`]: one map insert, in a reserved section. `node` is the largest
/// node the insert can make; one larger than a size class is refused, as
/// [`insert_held`] refuses it at compile time, because the reserve holds
/// nothing that size.
fn library_section(node: usize, run: &mut dyn FnMut()) -> Result<(), AllocError> {
    if node > LARGEST_CLASS {
        return Err(AllocError);
    }
    // Not `section`: `ferrix_fallible::in_section` has asked the injection
    // policy already, and asking twice would count one allocation as two.
    let _held = crate::mm::reserve(None)?;
    run();
    Ok(())
}

/// Let library crates' map inserts into the reserve -- the btrfs writer's,
/// through `ferrix_fallible::in_section`. Once, at boot, before any of them
/// can run.
pub(crate) fn install_library_sections() {
    let _ = ferrix_fallible::set_section(library_section);
}

// ---------------------------------------------------------------------------
// Failure injection
// ---------------------------------------------------------------------------

/// The task whose allocations fail, or zero for none.
static TARGET: AtomicU64 = AtomicU64::new(0);

/// Every how many of its fallible allocations one fails.
static PERIOD: AtomicU32 = AtomicU32::new(0);

/// Whether only the [`PERIOD`]th fails, rather than every one.
static ONCE: AtomicBool = AtomicBool::new(false);

/// Whether the frame allocator's allocations are counted and failed too.
static FRAMES: AtomicBool = AtomicBool::new(false);

/// Its fallible allocations since [`inject`], counted toward [`PERIOD`].
static SEEN: AtomicU32 = AtomicU32::new(0);

/// How many it has been made to fail.
static FAILED: AtomicU64 = AtomicU64::new(0);

/// The policy `ferrix_fallible` asks: fail every [`PERIOD`]th fallible
/// allocation the [`TARGET`] task makes, or with [`ONCE`] that one alone.
fn policy() -> bool {
    let target = TARGET.load(Ordering::Acquire);
    if target == 0 || crate::sched::current_id() != Some(target) {
        return false;
    }
    let period = PERIOD.load(Ordering::Relaxed).max(1);
    let seen = SEEN.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let fail = if ONCE.load(Ordering::Relaxed) {
        seen == period
    } else {
        seen.is_multiple_of(period)
    };
    if fail {
        let _ = FAILED.fetch_add(1, Ordering::Relaxed);
    }
    fail
}

/// Make every `period`th fallible allocation by task `task` fail, until
/// [`stop_injecting`].
pub(crate) fn inject(task: TaskId, period: u32) {
    arm(task, period, false, false);
}

/// Make the `nth` fallible allocation by task `task` fail, and no other,
/// until [`stop_injecting`]; with `frames`, counting the frame allocator's
/// allocations among them ([`frame_refused`]).
pub(crate) fn inject_once(task: TaskId, nth: u32, frames: bool) {
    arm(task, nth, true, frames);
}

/// Start failing `task`'s allocations: every `period`th, or with `once` that
/// one alone; and with `frames`, its frame allocations as well.
fn arm(task: TaskId, period: u32, once: bool, frames: bool) {
    let _ = ferrix_fallible::set_injector(policy);
    ONCE.store(once, Ordering::Relaxed);
    FRAMES.store(frames, Ordering::Relaxed);
    PERIOD.store(period.max(1), Ordering::Relaxed);
    SEEN.store(0, Ordering::Relaxed);
    FAILED.store(0, Ordering::Relaxed);
    TARGET.store(task, Ordering::Release);
    ferrix_fallible::arm(true);
}

/// Stop failing allocations, and say how many were failed.
pub(crate) fn stop_injecting() -> u64 {
    ferrix_fallible::arm(false);
    TARGET.store(0, Ordering::Release);
    FRAMES.store(false, Ordering::Relaxed);
    FAILED.load(Ordering::Relaxed)
}

/// Whether the frame allocation about to be made is to be refused, as if the
/// frame allocator were empty: under [`inject_once`] with `frames`, on the
/// task it names, when it is that allocation's turn. `mm::allocate_frames`
/// asks. Nothing but a load while no such injection is armed.
pub(crate) fn frame_refused() -> bool {
    FRAMES.load(Ordering::Relaxed) && TARGET.load(Ordering::Acquire) != 0 && policy()
}
