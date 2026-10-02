//! Allocation that reports failure, for the write path (finding F-23).
//!
//! This crate runs in the kernel, whose allocator returns null when memory has
//! run out and whose every ordinary `Vec` or map turns that null into a stop.
//! So the write path allocates through this module and nowhere else, and
//! `tools/common/check/check-fallible-alloc.py` holds it to that: every helper
//! here answers [`Error::OutOfMemory`] instead.
//!
//! Two mechanisms, the kernel's own (`src/kernel/src/fallible.rs`):
//!
//! * a `Vec` is reserved with `try_reserve` before it grows, through
//!   `ferrix_fallible`;
//! * a `BTreeMap` or `BTreeSet` insert cannot be made fallible from outside
//!   `alloc`, so it runs inside the section the kernel installs with
//!   `ferrix_fallible::set_section`: the kernel fills this processor's
//!   reserve first, which is where failure is reported, and serves the
//!   insert's nodes from it if the heap refuses. Each insert is its own
//!   section, so the reserve's depth bounds one insert, never a loop of them;
//!   a map is copied one insert at a time ([`copy_map`]) for that reason.
//!
//! Failure is injected through the same `ferrix_fallible` policy the kernel's
//! allocation checks use, which is how the host tests fail each allocation of
//! an operation in turn.
//!
//! # What a failure does to a transaction
//!
//! Nothing different from any other failure, which is to say:
//!
//! * inside an edit -- every tree edit runs in `WriteVolume::guarded`, and
//!   the commit is one -- running out of memory aborts the transaction, even
//!   when it comes before the edit's first change: `guarded` aborts on every
//!   error but `Exists` and `NotFound`, and cannot tell where one came from.
//!   So every failure during a commit aborts;
//! * in an operation's own code before its first edit -- building a payload,
//!   reading the items it needs -- it returns [`Error::OutOfMemory`] with the
//!   transaction as it was. Payloads are built before the edit that takes
//!   them for this reason;
//! * reads outside any operation change nothing either way.
//!
//! An aborted transaction is discarded and the volume reopened at its last
//! commit. No failure is reported for a commit that is on the disk: what the
//! commit needs after its primary superblock is written is made before it.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

pub(crate) use ferrix_fallible::AllocError;

use crate::{Error, Result};

impl From<AllocError> for Error {
    fn from(_: AllocError) -> Self {
        Error::OutOfMemory
    }
}

/// `vec![0u8; len]`.
pub(crate) fn zeroed(len: usize) -> Result<Vec<u8>> {
    Ok(ferrix_fallible::try_filled(0u8, len)?)
}

/// `vec![value; len]`.
pub(crate) fn filled<T: Clone>(value: T, len: usize) -> Result<Vec<T>> {
    Ok(ferrix_fallible::try_filled(value, len)?)
}

/// `items.to_vec()`.
pub(crate) fn copy<T: Clone>(items: &[T]) -> Result<Vec<T>> {
    Ok(ferrix_fallible::try_to_vec(items)?)
}

/// `Vec::with_capacity(capacity)`.
pub(crate) fn with_capacity<T>(capacity: usize) -> Result<Vec<T>> {
    Ok(ferrix_fallible::try_with_capacity(capacity)?)
}

/// `vec.push(value)`.
pub(crate) fn push<T>(vec: &mut Vec<T>, value: T) -> Result<()> {
    Ok(ferrix_fallible::try_push(vec, value)?)
}

/// `vec.insert(index, value)`; `index` is at most the length.
pub(crate) fn insert_at<T>(vec: &mut Vec<T>, index: usize, value: T) -> Result<()> {
    Ok(ferrix_fallible::try_insert(vec, index, value)?)
}

/// `vec.extend_from_slice(items)`.
pub(crate) fn extend_from_slice<T: Clone>(vec: &mut Vec<T>, items: &[T]) -> Result<()> {
    Ok(ferrix_fallible::try_extend_from_slice(vec, items)?)
}

/// `vec.resize(len, value)`.
pub(crate) fn resize<T: Clone>(vec: &mut Vec<T>, len: usize, value: T) -> Result<()> {
    Ok(ferrix_fallible::try_resize(vec, len, value)?)
}

/// `items.collect::<Vec<_>>()`.
pub(crate) fn collect<T>(items: impl IntoIterator<Item = T>) -> Result<Vec<T>> {
    Ok(ferrix_fallible::try_collect(items)?)
}

/// `items.collect::<Result<Vec<_>>>()`: the first error, or every item.
pub(crate) fn collect_ok<T>(items: impl IntoIterator<Item = Result<T>>) -> Result<Vec<T>> {
    let items = items.into_iter();
    let mut out = with_capacity(items.size_hint().0)?;
    for item in items {
        push(&mut out, item?)?;
    }
    Ok(out)
}

/// `vec.split_off(at)`: everything from `at` on, moved into a vector of its
/// own. On failure `vec` is unchanged.
pub(crate) fn split_off<T>(vec: &mut Vec<T>, at: usize) -> Result<Vec<T>> {
    let at = at.min(vec.len());
    let mut tail = with_capacity(vec.len() - at)?;
    // NOALLOC: into the capacity just reserved for exactly these elements.
    tail.extend(vec.drain(at..));
    Ok(tail)
}

/// `map.insert(key, value)`, inside the kernel's section.
pub(crate) fn insert<K: Ord, V>(map: &mut BTreeMap<K, V>, key: K, value: V) -> Result<Option<V>> {
    Ok(ferrix_fallible::try_map_insert(map, key, value)?)
}

/// `map.entry(key).or_insert_with(make)`, inside the kernel's section;
/// `make` must allocate nothing.
pub(crate) fn entry<K: Ord, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    make: impl FnOnce() -> V,
) -> Result<&mut V> {
    Ok(ferrix_fallible::try_map_entry(map, key, make)?)
}

/// `set.insert(value)`, inside the kernel's section.
pub(crate) fn insert_into_set<T: Ord>(set: &mut BTreeSet<T>, value: T) -> Result<bool> {
    Ok(ferrix_fallible::try_set_insert(set, value)?)
}

/// A copy of `map`, one insert, and one section, at a time.
pub(crate) fn copy_map<K: Ord + Copy, V: Copy>(map: &BTreeMap<K, V>) -> Result<BTreeMap<K, V>> {
    let mut out = BTreeMap::new();
    for (&key, &value) in map {
        let _ = insert(&mut out, key, value)?;
    }
    Ok(out)
}
