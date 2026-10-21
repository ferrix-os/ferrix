//! Memory reservations: frames the kernel promises to a job
//! (`docs/microkernel/K12.md`, `docs/MICROKERNEL.md` §3 K12).
//!
//! A job's limit (`object::quota`, F-35, F-37) says how much it may hold. A
//! reservation says how much it will find: while a job holds fewer frames
//! *drawn* from its reservation than the reservation's size, the next frame
//! it asks for is there, whatever every other job and the kernel itself have
//! taken meanwhile. So a QM partition cannot take the memory a certified one
//! was promised.
//!
//! # The ledger
//!
//! A fixed table of [`MOST`] entries, each a `size` and a `drawn`, and one
//! machine-wide number, the **outstanding promise** `O`: the sum over the
//! entries of `size − drawn` where that is positive. `O` is how many of the
//! free frames belong to somebody. Every write to an entry and to `O` is made
//! while the frame allocator's lock (`mm::FRAMES`) is held, in the same hold
//! that takes or frees the frames it counts, so at every release of that lock
//! `O` is exact and `free >= O`. The fields are atomics only so that a
//! report may read them without the lock.
//!
//! A frame drawn from entry `r` is **tagged** `r + 1` in a byte array beside
//! the frame records (`TAGS`): written in the hold that takes it, cleared in
//! the one that frees it. The frame record's owner, the job its limit
//! charges, is a separate mark: a frame counts against a limit and is drawn
//! from a reservation independently.
//!
//! # The floors
//!
//! [`Floor`] is the one decision every route of the frame allocator makes
//! with the free count, under the lock: a frame drawn from a reservation with
//! room may take any free frame; a program's frame otherwise is refused below
//! the watermark plus `O`; the kernel's own below `O`. With no reservation
//! `O` is 0 and every route is as it was before K12.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use ferrix_frame::Frame;
use ferrix_sync::Once;

/// Entries the table holds: reservations bound or still draining.
pub(crate) const MOST: usize = 64;

/// No reservation: what a tag reads for a frame drawn from none, and what
/// [`Entry`] numbers are offset from.
pub(crate) const NONE: u8 = 0;

/// One reservation.
struct Slot {
    /// Frames promised.
    size: AtomicU64,
    /// Frames held now that were drawn from it.
    drawn: AtomicU64,
    /// Whether a job's quota slot owns it. An entry that is not bound and
    /// still has frames drawn is draining: nothing is promised by it, and it
    /// is reused only once the last of them is freed.
    bound: AtomicBool,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            size: AtomicU64::new(0),
            drawn: AtomicU64::new(0),
            bound: AtomicBool::new(false),
        }
    }

    /// What it adds to the outstanding promise.
    fn outstanding(&self) -> u64 {
        self.size
            .load(Ordering::Relaxed)
            .saturating_sub(self.drawn.load(Ordering::Relaxed))
    }
}

/// The table.
static SLOTS: [Slot; MOST] = [const { Slot::new() }; MOST];

/// The outstanding promise, `O`, in frames.
static OUTSTANDING: AtomicU64 = AtomicU64::new(0);

/// One tag a frame, from the lowest frame the allocator manages.
static TAGS: Once<(&'static [AtomicU8], Frame)> = Once::new();

/// A bound or draining entry: its index plus one, so that [`NONE`] is none.
pub(crate) type Entry = u8;

/// The entry's slot.
fn slot(entry: Entry) -> Option<&'static Slot> {
    usize::from(entry)
        .checked_sub(1)
        .and_then(|at| SLOTS.get(at))
}

/// Hand the tag array to the ledger: one byte for each of the frames the
/// allocator manages from `base`, zeroed. Once, at `mm::init`.
pub(super) fn install(tags: &'static [AtomicU8], base: Frame) {
    let _ = TAGS.call_once(|| (tags, base));
}

/// The tag of `frame`, if the array covers it.
fn tag_of(frame: Frame) -> Option<&'static AtomicU8> {
    let (tags, base) = TAGS.get()?;
    let at = usize::try_from(frame.checked_sub(*base)?).ok()?;
    tags.get(at)
}

/// The reservation `frame` was drawn from, or [`NONE`]. Read with no lock:
/// a frame's tag changes only while it is taken or freed, never under a
/// holder of it.
pub(crate) fn drawn_from(frame: Frame) -> Entry {
    tag_of(frame).map_or(NONE, |tag| tag.load(Ordering::Acquire))
}

/// The outstanding promise, in frames: free frames that belong to a
/// reservation.
pub(crate) fn outstanding() -> u64 {
    OUTSTANDING.load(Ordering::Relaxed)
}

/// An entry's size and what is drawn from it, in frames.
pub(crate) fn usage(entry: Entry) -> (u64, u64) {
    slot(entry).map_or((0, 0), |slot| {
        (
            slot.size.load(Ordering::Relaxed),
            slot.drawn.load(Ordering::Relaxed),
        )
    })
}

/// Who a run of frames is taken for, and so how low it may take the free
/// count: the one decision of `L.mm.77` to `L.mm.79`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Floor {
    /// A program's memory or page tables, for a job with this reservation
    /// ([`NONE`] for none, or in interrupt context): drawn from it while it
    /// has room, held above the watermark and the promise otherwise.
    Program(Entry),
    /// The kernel's own: held above the promise.
    Kernel,
}

/// What [`take`] decided, for the caller to finish in the same hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Taking {
    /// Taken against this entry: tag the frames with [`drawn`].
    Drawn(Entry),
    /// Taken against nothing.
    Plain,
}

/// Whether `n` frames may be taken from `free` for `floor`, given the
/// watermark: `Some` with how they are to be counted, `None` when refused.
/// Under the frame allocator's lock.
///
/// * `L.mm.79`: a program's frames drawn from an entry with `drawn + n <=
///   size` are given while `n` are free;
/// * `L.mm.77`: a program's frames otherwise only if `free − n >= W + O`;
/// * `L.mm.78`: the kernel's own only if `free − n >= O`.
pub(super) fn take(floor: Floor, free: u64, n: u64, watermark: u64) -> Option<Taking> {
    let outstanding = outstanding();
    let (taking, least) = match floor {
        Floor::Program(entry) if room(entry, n) => (Taking::Drawn(entry), 0),
        Floor::Program(_) => (Taking::Plain, watermark.saturating_add(outstanding)),
        Floor::Kernel => (Taking::Plain, outstanding),
    };
    (free.checked_sub(n)? >= least).then_some(taking)
}

/// Whether `entry` is bound and `n` more frames drawn from it stay within
/// its size.
fn room(entry: Entry, n: u64) -> bool {
    slot(entry).is_some_and(|slot| {
        slot.bound.load(Ordering::Relaxed)
            && slot
                .drawn
                .load(Ordering::Relaxed)
                .checked_add(n)
                .is_some_and(|then| then <= slot.size.load(Ordering::Relaxed))
    })
}

/// Count `n` frames from `first`, just taken as [`Taking::Drawn`] of
/// `entry`: tagged, drawn, and out of the promise. Under the frame
/// allocator's lock, in the hold that took them.
pub(super) fn drawn(entry: Entry, first: Frame, n: u64) {
    let Some(slot) = slot(entry) else {
        return;
    };
    for frame in first..first.saturating_add(n) {
        if let Some(tag) = tag_of(frame) {
            tag.store(entry, Ordering::Release);
        }
    }
    adjust(slot, |slot| {
        let _ = slot.drawn.fetch_add(n, Ordering::Relaxed);
    });
}

/// Count `n` frames from `first` as freed: each tagged one leaves its
/// entry's drawn count, and the promise takes back what that makes
/// outstanding again (`L.mm.80`). Under the frame allocator's lock, in the
/// hold that frees them.
pub(super) fn freed(first: Frame, n: u64) {
    for frame in first..first.saturating_add(n) {
        let Some(tag) = tag_of(frame) else {
            continue;
        };
        let entry = tag.swap(NONE, Ordering::AcqRel);
        if let Some(slot) = slot(entry) {
            adjust(slot, |slot| {
                let _ = slot
                    .drawn
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |drawn| {
                        Some(drawn.saturating_sub(1))
                    });
            });
        }
    }
}

/// Change `slot` by `change`, and the promise by what that changes of the
/// slot's part of it. Under the frame allocator's lock.
fn adjust(slot: &Slot, change: impl FnOnce(&Slot)) {
    let before = slot.outstanding();
    change(slot);
    let after = slot.outstanding();
    let _ = OUTSTANDING.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |outstanding| {
        Some(outstanding.saturating_add(after).saturating_sub(before))
    });
}

/// Bind a free entry, of size 0: one with nothing drawn and no owner.
/// Under the frame allocator's lock. `None` when every entry is bound or
/// still draining.
pub(super) fn bind() -> Option<Entry> {
    let at = SLOTS.iter().position(|slot| {
        !slot.bound.load(Ordering::Relaxed) && slot.drawn.load(Ordering::Relaxed) == 0
    })?;
    let slot = SLOTS.get(at)?;
    slot.size.store(0, Ordering::Relaxed);
    slot.bound.store(true, Ordering::Relaxed);
    Entry::try_from(at + 1).ok()
}

/// Unbind `entry`: nothing promised by it from now on, and it drains
/// (`L.object.225`). Under the frame allocator's lock.
pub(super) fn unbind(entry: Entry) {
    if let Some(slot) = slot(entry) {
        adjust(slot, |slot| slot.size.store(0, Ordering::Relaxed));
        slot.bound.store(false, Ordering::Relaxed);
    }
}

/// Why a resize was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Refused {
    /// The free frames could not cover it: how many more it needed.
    Short(u64),
    /// The entry is not bound.
    Unbound,
}

/// Make `entry`'s size `frames`, if the free count allows (`L.mm.76`):
/// growing what it adds to the promise by `d` needs `free − d >= W + O`;
/// shrinking always succeeds. Under the frame allocator's lock.
pub(super) fn resize(entry: Entry, frames: u64, free: u64, watermark: u64) -> Result<(), Refused> {
    let Some(slot) = slot(entry).filter(|slot| slot.bound.load(Ordering::Relaxed)) else {
        return Err(Refused::Unbound);
    };
    let then = frames.saturating_sub(slot.drawn.load(Ordering::Relaxed));
    let grows = then.saturating_sub(slot.outstanding());
    let least = watermark
        .saturating_add(outstanding())
        .saturating_add(grows);
    if free < least {
        return Err(Refused::Short(least - free));
    }
    adjust(slot, |slot| slot.size.store(frames, Ordering::Relaxed));
    Ok(())
}
