//! Memory reserved for an operation before its first change
//! (`docs/BTRFS-PRESSURE.md`, "Against Linux's btrfs", W1).
//!
//! An edit cannot be undone half-way, so an allocation that fails inside one
//! aborts the transaction (`fallible`, "What a failure does to a
//! transaction"). Linux's btrfs never meets that for a tree node: its extent
//! buffers are `__GFP_NOFAIL`. This crate gets the same from a reservation
//! the mount hands it, as Linux's `block_rsv` reserves disk space before a
//! transaction handle changes anything: the outermost operation asks it for
//! its worst case in memory before its first change, and runs inside it.
//!
//! The reservation is the kernel's (`mm::heap_scope`, `L.mm.140`): this
//! crate sees it only as the [`Reserve`] trait it was handed, as it sees the
//! device as [`crate::WriteDevice`], so nothing here names the kernel. A
//! volume with none -- a host build, a tool, a mount that found no free
//! entry -- behaves as it did before W1.

use alloc::boxed::Box;
use core::fmt;

/// A reservation of the kernel's memory for one writable volume, counted in
/// frames, which the volume asks before an operation and runs the operation
/// inside.
///
/// Only allocations of at most a page are drawn from it (`L.mm.140`): one
/// larger takes a run of frames, which a count of frames cannot promise
/// (ledger 8470, CR.1), and goes on being served and refused as before.
pub trait Reserve: Send + Sync {
    /// Make the reservation `frames` frames in all, what is drawn from it
    /// included: `false`, with nothing changed, when the machine could not
    /// promise that many even after making room.
    fn grow_to(&self, frames: u64) -> bool;

    /// Frames drawn from it now.
    fn drawn(&self) -> u64;

    /// The most it may promise beyond what is drawn (`R_MAX`).
    fn most(&self) -> u64;

    /// Run `body` with the current task's heap allocations of at most a page
    /// drawn from the reservation first.
    fn within(&self, body: &mut dyn FnMut());

    /// Promise nothing beyond what is drawn.
    fn shrink(&self);
}

/// The volume's reservation, if it was handed one.
#[derive(Default)]
pub(crate) struct Reserved(Option<Box<dyn Reserve>>);

impl fmt::Debug for Reserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Reserved").field(&self.0.is_some()).finish()
    }
}

impl Reserved {
    /// None: the volume runs as before W1.
    pub(crate) const fn none() -> Reserved {
        Reserved(None)
    }

    /// `reserve`, for the volume to keep.
    pub(crate) fn of(reserve: Box<dyn Reserve>) -> Reserved {
        Reserved(Some(reserve))
    }

    /// Promise nothing more: after a reload read-only, which takes no edit.
    pub(crate) fn shrink(&self) {
        if let Some(reserve) = &self.0 {
            reserve.shrink();
        }
    }

    /// Whether the volume holds one.
    pub(crate) const fn is_some(&self) -> bool {
        self.0.is_some()
    }

    /// The most the reservation may promise beyond what is drawn, or none
    /// without one.
    pub(crate) fn most(&self) -> Option<u64> {
        self.0.as_ref().map(|reserve| reserve.most())
    }

    /// Take it, for an operation to run inside: the operation's own nested
    /// operations then find none, and run in the outer one's admission.
    pub(crate) const fn take(&mut self) -> Option<Box<dyn Reserve>> {
        self.0.take()
    }

    /// Put back what [`Reserved::take`] took.
    pub(crate) fn put(&mut self, reserve: Box<dyn Reserve>) {
        self.0 = Some(reserve);
    }
}

/// Frames of the kernel's heap one tree node may take in allocations of at
/// most a page, on a volume with `nodesize` nodes: what an admission counts
/// for each node an operation can load or make (`L.btrfs.124`).
///
/// A node's payloads are at most its body, each rounded up to its size class
/// or to a page -- at most twice what it asks, the worst being a payload of
/// one byte past a class (a payload below the smallest class, 16 bytes, takes
/// more, which the node's item header around it covers and
/// `tests/reserve_bound.rs` measures) -- so twice the node's pages; then its list of
/// items or pointers, when that is a page or less, one page; and one page for
/// its share of the maps that hold it (the dirty set, the clean nodes, the
/// delayed references' heads and deltas). A list longer than a page (a leaf
/// of more than 85 items, an internal node of more than 102 pointers) is a
/// run of frames, outside the reservation (`L.mm.140`) and outside the claim.
/// `tests/reserve_bound.rs` builds the worst nodes and measures them against
/// this through a model of the kernel's heap.
pub const fn node_frames(nodesize: u32) -> u64 {
    let pages = (nodesize as u64).div_ceil(PAGE);
    2 * pages + 2
}

/// Frames an admitted operation may take besides its nodes': a partly used
/// page for each of the heap's eight size classes, and what the operation
/// builds beside the trees -- a split's list of pieces, a range set's
/// entries, an extent record's references.
pub const OPERATION_FRAMES: u64 = 32;

/// The kernel's page, in bytes.
const PAGE: u64 = 4096;
