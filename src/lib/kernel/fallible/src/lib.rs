//! Fallible construction on stable Rust: `Box`, `Vec`, `VecDeque`, `String`.
//!
//! Finding F-23 of `docs/certification/FINDINGS.md`: the kernel's allocator
//! returns null when it is out of memory, and every ordinary container turns
//! that null into a call to the allocation error handler, which in a `no_std`
//! kernel is a panic. The attribute that would let the kernel install its own
//! handler, `#[alloc_error_handler]`, is unstable (rust-lang #51540), and so
//! are `Box::try_new`, `Arc::try_new` and `BTreeMap::try_insert`; the kernel
//! uses no unstable feature. What *is* stable is `Vec::try_reserve`,
//! `VecDeque::try_reserve`, `String::try_reserve`, the global allocator's
//! `alloc`, and `Box::from_raw` over memory allocated with `Layout::new`.
//! Everything here is built out of those.
//!
//! # What this covers, and what it cannot
//!
//! A `Box`, `Vec`, `VecDeque` or `String` made or grown through this crate
//! fails with [`AllocError`] instead of stopping the machine. `Arc` and the
//! ordered maps cannot be made fallible from outside the standard library at
//! all: their allocation happens inside `alloc` with a layout nobody else can
//! name. The kernel covers those with a per-processor reserve that the global
//! allocator falls back on (`src/kernel/src/fallible.rs`), and what that reserve
//! must hold is computed here, where it can be checked against the pinned
//! standard library: [`arc_layout`] is the one allocation `Arc::new` makes,
//! and [`btree_node_bound`] bounds the nodes a map insert makes. The tests in
//! `tests.rs` measure both with a recording allocator, so a toolchain whose
//! `alloc` did something else would fail here rather than in a machine that
//! had run out of memory. A library crate that cannot name the kernel -- the
//! btrfs writer -- reaches the same reserve through [`in_section`] and
//! [`try_map_insert`], which run one map insert in the section the kernel
//! installs at boot with [`set_section`].
//!
//! # Failure injection
//!
//! A test has to be able to make these fail on demand, on a real kernel, on
//! the paths a program drives. [`set_injector`] installs the kernel's policy
//! and [`arm`] turns it on; while armed, every function here that could
//! allocate asks the policy first and fails without allocating when told to.
//! Disarmed it costs one relaxed load.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_sync::Once;

/// An allocation could not be made.
///
/// Carries nothing: which allocation failed is the caller's business, and the
/// ABI error every caller turns this into (`ENOMEM`, `NO_MEMORY`) carries
/// nothing either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocError;

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("out of memory")
    }
}

impl From<alloc::collections::TryReserveError> for AllocError {
    fn from(_: alloc::collections::TryReserveError) -> AllocError {
        AllocError
    }
}

// ---------------------------------------------------------------------------
// Failure injection
// ---------------------------------------------------------------------------

/// Whether [`injected`] consults the policy at all.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The policy: asked once per fallible operation while armed, and a `true`
/// answer fails that operation.
static INJECTOR: Once<fn() -> bool> = Once::new();

/// Install the failure-injection policy. Only the first call has an effect,
/// and it says so by returning `true`.
pub fn set_injector(policy: fn() -> bool) -> bool {
    let mut installed = false;
    let _ = INJECTOR.call_once(|| {
        installed = true;
        policy
    });
    installed
}

/// Turn failure injection on or off.
pub fn arm(on: bool) {
    ARMED.store(on, Ordering::Release);
}

/// Whether the operation about to be attempted should fail as if memory had
/// run out. Public so that the kernel's reserve-backed constructors ask the
/// same policy.
#[must_use]
pub fn injected() -> bool {
    ARMED.load(Ordering::Relaxed) && INJECTOR.get().is_some_and(|policy| policy())
}

/// [`injected`] as a `Result`, for `?`.
///
/// # Errors
///
/// [`AllocError`] when the policy says this operation fails.
pub fn check() -> Result<(), AllocError> {
    if injected() { Err(AllocError) } else { Ok(()) }
}

// ---------------------------------------------------------------------------
// Box
// ---------------------------------------------------------------------------

/// `Box::new(value)`, or [`AllocError`] instead of the allocation error
/// handler.
///
/// `Box`'s documentation makes this conversion part of its contract: memory
/// from the global allocator with `Layout::new::<T>()`, holding a valid `T`,
/// may become a `Box<T>` through `Box::from_raw`. A zero-sized `T` is never
/// allocated, by `Box::new` or here.
///
/// # Errors
///
/// [`AllocError`] when the allocator returns null or the injection policy
/// says so. `value` is dropped.
pub fn try_box<T>(value: T) -> Result<Box<T>, AllocError> {
    check()?;
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        // No allocation: `Box::new` of a zero-sized type is a dangling,
        // aligned pointer and never reaches the allocator.
        return Ok(Box::new(value));
    }
    // SAFETY: `layout` has a non-zero size, checked above, which is the one
    // requirement `alloc` places on its caller.
    let raw = unsafe { alloc::alloc::alloc(layout) };
    let pointer = NonNull::new(raw.cast::<T>()).ok_or(AllocError)?;
    // SAFETY: `pointer` is non-null, was allocated for `Layout::new::<T>()` so
    // is aligned for and large enough to hold a `T`, and is written before
    // anything reads it.
    unsafe { pointer.as_ptr().write(value) };
    // SAFETY: allocated by the global allocator with `Layout::new::<T>()` and
    // holding a valid `T`, which is exactly the memory `Box::from_raw` accepts
    // (the "Memory layout" section of `alloc::boxed`). Nothing else owns it.
    Ok(unsafe { Box::from_raw(pointer.as_ptr()) })
}

/// A `Box<[T]>` holding clones of `items`, allocated exactly once.
///
/// Not `items.to_vec().into_boxed_slice()`: when a `Vec`'s capacity exceeds
/// its length, `into_boxed_slice` reallocates, and that second allocation is
/// infallible.
///
/// # Errors
///
/// [`AllocError`] when the allocator returns null, the size overflows, or the
/// injection policy says so.
pub fn try_boxed_slice<T: Clone>(items: &[T]) -> Result<Box<[T]>, AllocError> {
    let mut vec = try_with_capacity_exact(items.len())?;
    vec.extend_from_slice(items);
    exact_boxed_slice(vec)
}

/// A `Box<[T]>` of `len` clones of `value`, allocated exactly once:
/// `vec![value; len].into_boxed_slice()`, which is two allocations that
/// cannot fail, as one that can.
///
/// # Errors
///
/// As [`try_boxed_slice`].
pub fn try_boxed_filled<T: Clone>(value: T, len: usize) -> Result<Box<[T]>, AllocError> {
    let mut vec = try_with_capacity_exact(len)?;
    vec.resize(len, value);
    exact_boxed_slice(vec)
}

/// A `Box<str>` holding a copy of `text`, allocated exactly once.
///
/// # Errors
///
/// As [`try_boxed_slice`].
pub fn try_boxed_str(text: &str) -> Result<Box<str>, AllocError> {
    let mut string = String::new();
    check()?;
    string.try_reserve_exact(text.len())?;
    string.push_str(text);
    if string.capacity() != string.len() {
        // `into_boxed_str` would reallocate to shed the excess, infallibly.
        return Err(AllocError);
    }
    Ok(string.into_boxed_str())
}

/// A vector with room for exactly `capacity` elements.
fn try_with_capacity_exact<T>(capacity: usize) -> Result<Vec<T>, AllocError> {
    check()?;
    let mut vec = Vec::new();
    vec.try_reserve_exact(capacity)?;
    Ok(vec)
}

/// `vec` as a boxed slice, refusing rather than reallocating.
///
/// `try_reserve_exact` on an empty vector asks the allocator for exactly the
/// elements requested, and the global allocator gives exactly that, so the
/// refusal is not expected to be taken. It is there because the documentation
/// of `try_reserve_exact` does not promise it, and a reallocation here would
/// be the one infallible allocation this crate exists to remove.
fn exact_boxed_slice<T>(vec: Vec<T>) -> Result<Box<[T]>, AllocError> {
    if vec.capacity() != vec.len() {
        return Err(AllocError);
    }
    Ok(vec.into_boxed_slice())
}

// ---------------------------------------------------------------------------
// Vec
// ---------------------------------------------------------------------------

/// `vec.try_reserve(additional)`, asking the injection policy first.
///
/// Room already there is not an allocation, so it is not failed: a caller
/// that reserved ahead can count on the room it reserved, injection or not.
///
/// # Errors
///
/// [`AllocError`] when the allocator refuses, the capacity overflows, or the
/// injection policy says so. `vec` is unchanged.
pub fn try_reserve<T>(vec: &mut Vec<T>, additional: usize) -> Result<(), AllocError> {
    if vec.capacity() - vec.len() >= additional {
        return Ok(());
    }
    check()?;
    vec.try_reserve(additional)?;
    Ok(())
}

/// `Vec::with_capacity(capacity)`.
///
/// # Errors
///
/// As [`try_reserve`].
pub fn try_with_capacity<T>(capacity: usize) -> Result<Vec<T>, AllocError> {
    let mut vec = Vec::new();
    try_reserve(&mut vec, capacity)?;
    Ok(vec)
}

/// `vec.push(value)`.
///
/// # Errors
///
/// As [`try_reserve`]; `value` is dropped and `vec` is unchanged.
pub fn try_push<T>(vec: &mut Vec<T>, value: T) -> Result<(), AllocError> {
    try_reserve(vec, 1)?;
    vec.push(value);
    Ok(())
}

/// `vec.push(value)` only if it fits in the capacity already there, which
/// cannot allocate, so is not subject to injection.
///
/// # Errors
///
/// `value` back, when `vec` is full.
pub fn push_within<T>(vec: &mut Vec<T>, value: T) -> Result<(), T> {
    if vec.len() < vec.capacity() {
        vec.push(value);
        Ok(())
    } else {
        Err(value)
    }
}

/// `vec.insert(index, value)`, with `index` clamped to the length, where
/// `Vec::insert` would panic.
///
/// # Errors
///
/// As [`try_push`].
pub fn try_insert<T>(vec: &mut Vec<T>, index: usize, value: T) -> Result<(), AllocError> {
    try_reserve(vec, 1)?;
    vec.insert(index.min(vec.len()), value);
    Ok(())
}

/// `vec.extend_from_slice(items)`.
///
/// # Errors
///
/// As [`try_reserve`]; `vec` is unchanged.
pub fn try_extend_from_slice<T: Clone>(vec: &mut Vec<T>, items: &[T]) -> Result<(), AllocError> {
    try_reserve(vec, items.len())?;
    vec.extend_from_slice(items);
    Ok(())
}

/// `items.to_vec()`.
///
/// # Errors
///
/// As [`try_reserve`].
pub fn try_to_vec<T: Clone>(items: &[T]) -> Result<Vec<T>, AllocError> {
    let mut vec = try_with_capacity(items.len())?;
    vec.extend_from_slice(items);
    Ok(vec)
}

/// `vec![value; len]`.
///
/// # Errors
///
/// As [`try_reserve`].
pub fn try_filled<T: Clone>(value: T, len: usize) -> Result<Vec<T>, AllocError> {
    let mut vec = try_with_capacity(len)?;
    vec.resize(len, value);
    Ok(vec)
}

/// `vec.resize(len, value)`.
///
/// # Errors
///
/// As [`try_reserve`]; `vec` is unchanged.
pub fn try_resize<T: Clone>(vec: &mut Vec<T>, len: usize, value: T) -> Result<(), AllocError> {
    try_reserve(vec, len.saturating_sub(vec.len()))?;
    vec.resize(len, value);
    Ok(())
}

/// `vec.extend(items)`.
///
/// Reserves the iterator's lower size bound up front and one element at a
/// time past it, so an iterator that says how long it is costs one
/// reservation.
///
/// # Errors
///
/// As [`try_reserve`]. **Not atomic:** what was appended before the failure
/// stays appended. A caller that needs all or nothing collects into a fresh
/// vector with [`try_collect`] and appends that.
pub fn try_extend<T>(
    vec: &mut Vec<T>,
    items: impl IntoIterator<Item = T>,
) -> Result<(), AllocError> {
    let items = items.into_iter();
    try_reserve(vec, items.size_hint().0)?;
    for item in items {
        if vec.len() == vec.capacity() {
            try_reserve(vec, 1)?;
        }
        vec.push(item);
    }
    Ok(())
}

/// `items.collect::<Vec<_>>()`.
///
/// # Errors
///
/// As [`try_reserve`]; what was collected is dropped.
pub fn try_collect<T>(items: impl IntoIterator<Item = T>) -> Result<Vec<T>, AllocError> {
    let mut vec = Vec::new();
    try_extend(&mut vec, items)?;
    Ok(vec)
}

/// `vec.append(other)`.
///
/// # Errors
///
/// As [`try_reserve`]; both vectors are unchanged.
pub fn try_append<T>(vec: &mut Vec<T>, other: &mut Vec<T>) -> Result<(), AllocError> {
    try_reserve(vec, other.len())?;
    vec.append(other);
    Ok(())
}

// ---------------------------------------------------------------------------
// VecDeque
// ---------------------------------------------------------------------------

/// `queue.try_reserve(additional)`, asking the injection policy first.
///
/// # Errors
///
/// As [`try_reserve`].
pub fn try_reserve_deque<T>(queue: &mut VecDeque<T>, additional: usize) -> Result<(), AllocError> {
    if queue.capacity() - queue.len() >= additional {
        return Ok(());
    }
    check()?;
    queue.try_reserve(additional)?;
    Ok(())
}

/// `VecDeque::with_capacity(capacity)`.
///
/// # Errors
///
/// As [`try_reserve`].
pub fn try_deque_with_capacity<T>(capacity: usize) -> Result<VecDeque<T>, AllocError> {
    let mut queue = VecDeque::new();
    try_reserve_deque(&mut queue, capacity)?;
    Ok(queue)
}

/// `queue.push_back(value)`.
///
/// # Errors
///
/// As [`try_reserve`]; `value` is dropped and `queue` is unchanged.
pub fn try_push_back<T>(queue: &mut VecDeque<T>, value: T) -> Result<(), AllocError> {
    try_reserve_deque(queue, 1)?;
    queue.push_back(value);
    Ok(())
}

/// `queue.push_front(value)`.
///
/// # Errors
///
/// As [`try_push_back`].
pub fn try_push_front<T>(queue: &mut VecDeque<T>, value: T) -> Result<(), AllocError> {
    try_reserve_deque(queue, 1)?;
    queue.push_front(value);
    Ok(())
}

// ---------------------------------------------------------------------------
// String
// ---------------------------------------------------------------------------

/// `String::from(text)`.
///
/// # Errors
///
/// As [`try_reserve`].
pub fn try_string(text: &str) -> Result<String, AllocError> {
    let mut string = String::new();
    try_push_str(&mut string, text)?;
    Ok(string)
}

/// `string.push_str(text)`.
///
/// # Errors
///
/// As [`try_reserve`]; `string` is unchanged.
pub fn try_push_str(string: &mut String, text: &str) -> Result<(), AllocError> {
    check()?;
    string.try_reserve(text.len())?;
    string.push_str(text);
    Ok(())
}

/// `format!(...)`, as `try_format(format_args!(...))`.
///
/// # Errors
///
/// [`AllocError`] when a piece could not be appended. A `Display`
/// implementation that fails on its own account is reported the same way:
/// the kernel has none, and a caller of this has no better answer for one.
pub fn try_format(args: fmt::Arguments<'_>) -> Result<String, AllocError> {
    let mut out = Growing {
        string: String::new(),
    };
    fmt::write(&mut out, args).map_err(|_| AllocError)?;
    Ok(out.string)
}

/// A `fmt::Write` whose every append is reserved fallibly first.
struct Growing {
    /// What has been written so far.
    string: String,
}

impl fmt::Write for Growing {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        try_push_str(&mut self.string, piece).map_err(|_| fmt::Error)
    }
}

// ---------------------------------------------------------------------------
// The ordered maps, for a crate that cannot reach the kernel's reserve
// ---------------------------------------------------------------------------

/// How a host runs one ordered-map operation so that it cannot stop the
/// machine: `run` makes the allocations of one `BTreeMap` or `BTreeSet`
/// insert, each a node of at most `node` bytes ([`btree_node_bound`]), and
/// nothing else. The host either runs it where those allocations cannot
/// fail and answers `Ok`, or does not run it and answers [`AllocError`].
///
/// The kernel's is a reserved section (`src/kernel/src/fallible.rs`): it fills
/// this processor's reserve first, which is where failure is reported, and
/// serves the insert from the reserve if the heap refuses.
pub type Section = fn(node: usize, run: &mut dyn FnMut()) -> Result<(), AllocError>;

/// The host's [`Section`], installed once at boot.
static SECTION: Once<Section> = Once::new();

/// Install the host's [`Section`]. Only the first call has an effect, and it
/// says so by returning `true`.
///
/// Until one is installed -- on the host, in tests -- an operation passed to
/// [`in_section`] runs as it is: only the injection policy can fail it, and a
/// heap that refuses one of its nodes calls the allocation error handler as
/// any `BTreeMap` insert does.
pub fn set_section(section: Section) -> bool {
    let mut installed = false;
    let _ = SECTION.call_once(|| {
        installed = true;
        section
    });
    installed
}

/// Run `op`, one insert into an ordered map whose nodes are at most `node`
/// bytes, inside the host's [`Section`].
///
/// For a library crate: the kernel's reserve is not its to enter, and this
/// is how it gets there. `op` must allocate nothing but the map's nodes --
/// a value to be inserted is made before, fallibly -- and wait for nothing:
/// the kernel runs it with interrupts masked.
///
/// # Errors
///
/// [`AllocError`] when the injection policy says so or the section cannot
/// be entered. `op` has not run, and what it owned is dropped.
pub fn in_section<R>(node: usize, op: impl FnOnce() -> R) -> Result<R, AllocError> {
    check()?;
    let Some(section) = SECTION.get() else {
        return Ok(op());
    };
    let mut op = Some(op);
    let mut out = None;
    section(node, &mut || {
        if let Some(op) = op.take() {
            out = Some(op());
        }
    })?;
    out.ok_or(AllocError)
}

/// [`btree_node_bound`] for a `BTreeMap<K, V>`.
#[must_use]
pub const fn map_node_bound<K, V>() -> usize {
    let align = if align_of::<K>() > align_of::<V>() {
        align_of::<K>()
    } else {
        align_of::<V>()
    };
    btree_node_bound(size_of::<K>(), size_of::<V>(), align)
}

/// `map.insert(key, value)`, inside the host's [`Section`].
///
/// # Errors
///
/// As [`in_section`]; `key` and `value` are dropped and `map` is unchanged.
pub fn try_map_insert<K: Ord, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
) -> Result<Option<V>, AllocError> {
    in_section(map_node_bound::<K, V>(), || map.insert(key, value))
}

/// `map.entry(key).or_insert_with(make)`, inside the host's [`Section`].
/// `make` runs inside it too, so it must allocate nothing.
///
/// # Errors
///
/// As [`in_section`]; `map` is unchanged.
pub fn try_map_entry<K: Ord, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    make: impl FnOnce() -> V,
) -> Result<&mut V, AllocError> {
    in_section(map_node_bound::<K, V>(), || {
        map.entry(key).or_insert_with(make)
    })
}

/// `set.insert(value)`, inside the host's [`Section`].
///
/// # Errors
///
/// As [`in_section`]; `value` is dropped and `set` is unchanged.
pub fn try_set_insert<T: Ord>(set: &mut BTreeSet<T>, value: T) -> Result<bool, AllocError> {
    in_section(map_node_bound::<T, ()>(), || set.insert(value))
}

// ---------------------------------------------------------------------------
// What the kernel's reserve has to hold
// ---------------------------------------------------------------------------

/// The layout of the one allocation `Arc::<T>::new` makes, or `None` if it
/// would overflow.
///
/// `alloc::sync::ArcInner` is private, but it is `#[repr(C)]` with two
/// `usize` counts before the value, and its layout is what this computes.
/// Nothing unsafe depends on it: the kernel uses it only to decide whether
/// the reserve must hold a block too large for a size class. The test
/// `arc_new_allocates_exactly_arc_layout` measures `Arc::new` against it.
#[must_use]
pub fn arc_layout<T>() -> Option<Layout> {
    let counts = Layout::new::<[usize; 2]>();
    let (inner, _) = counts.extend(Layout::new::<T>()).ok()?;
    Some(inner.pad_to_align())
}

/// Keys a `BTreeMap` node holds at most: `2 * B - 1` for the standard
/// library's `B` of 6.
pub const BTREE_CAPACITY: usize = 11;

/// An upper bound on the bytes of one `BTreeMap<K, V>` node, internal or
/// leaf, from the sizes of `K` and `V` and the larger of their alignments.
///
/// A leaf is a parent pointer, a `u16` index and length, and
/// [`BTREE_CAPACITY`] keys and values; an internal node is a leaf followed by
/// `BTREE_CAPACITY + 1` child pointers. The field order of a leaf is the
/// compiler's, so the bound allows one alignment's padding for each of its
/// five fields. The test `btree_nodes_fit_their_bound` measures real maps
/// against it.
#[must_use]
pub const fn btree_node_bound(key: usize, value: usize, align: usize) -> usize {
    let pointer = size_of::<usize>();
    let leaf = pointer + 4 + BTREE_CAPACITY * (key + value) + 5 * align;
    leaf + (BTREE_CAPACITY + 1) * pointer
}

/// The nodes one `BTreeMap` insert can allocate in a tree of `height`
/// internal levels: a leaf for the split at the bottom, an internal node for
/// a split at each level, and a new root. An empty map's first insert
/// allocates one leaf, within this.
#[must_use]
pub const fn btree_insert_nodes(height: usize) -> usize {
    height + 2
}

#[cfg(test)]
mod tests;
