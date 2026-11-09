//! The address bookkeeping of the kernel's large-request area
//! (`docs/ROBUSTNESS.md` §2.6, `L.mm.141`-`L.mm.145`).
//!
//! A kernel heap request above a page that the buddy cannot serve as one run
//! of frames is served from single frames mapped into a span of kernel
//! address space set aside at boot: Linux's `vmalloc`, behind `kvmalloc`.
//! This is the part of that which is pure bookkeeping, so that `cargo test`
//! drives it: which addresses are free, which wait for a shootdown, and which
//! frames are kept until it.
//!
//! The addresses are a buddy over units of [`UNIT_PAGES`] pages, which is
//! [`Frames`] itself over unit numbers instead of frame numbers: a request
//! of `n` pages takes a block of the smallest power of two units holding
//! `n + 1` pages, aligned to its size, and its top page is never mapped. So
//! every block's top is a guard, and every block's bottom sits on the guard
//! of the block below it.
//!
//! A freed block is not free. Its addresses may still be in some processor's
//! TLB, so it goes on the **lazy list** and is handed out again only after a
//! purge has detached the list ([`Area::detach`]), the kernel has made one
//! shootdown covering all of it, and the purge has given each block back
//! ([`Area::give_back`]). The frames behind a freed block may have to wait
//! for that shootdown as well, where the architecture's own invalidation is
//! not broadcast: the kernel chains them through their own memory, and this
//! keeps the chain's head and length, so that one detach takes the blocks
//! and their frames together.

use crate::{Frame, FrameError, Frames, MAX_ORDER, PageEntry};

/// Pages in one unit of the area's buddy: two, so that [`MAX_ORDER`] units
/// hold the largest request, `2^MAX_ORDER` pages, with its guard.
pub const UNIT_PAGES: u64 = 2;

/// A unit's lazy-list word when it heads no block on the list.
const NOT_LAZY: u32 = 0;

/// Set in a unit's lazy-list word while it heads a block on the list.
const ON_LIST: u32 = 1 << 31;

/// Bits of a lazy-list word that hold the block's order.
const ORDER_BITS: u32 = 4;

/// The next block's unit, in a lazy-list word, at the end of the list.
const END: u32 = (ON_LIST >> ORDER_BITS) - 1;

/// The most units an area may have: a unit number must fit a lazy-list word
/// beside its flag and its order, below the end-of-list mark.
pub const MOST_UNITS: u64 = END as u64;

/// A block of the area: `2^order` units from unit `unit`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Block {
    /// The first unit, counted from the area's base.
    pub unit: u64,
    /// The block is `2^order` units.
    pub order: u8,
}

impl Block {
    /// The block's first page, counted from the area's base.
    #[must_use]
    pub const fn first_page(&self) -> u64 {
        self.unit * UNIT_PAGES
    }

    /// Pages the block spans, its guard included.
    #[must_use]
    pub const fn pages(&self) -> u64 {
        UNIT_PAGES << self.order
    }
}

/// The order of the block a request of `pages` pages takes, its guard page
/// included: `None` for no pages, and for more than [`MAX_ORDER`] units.
#[must_use]
pub const fn order_for(pages: u64) -> Option<u8> {
    if pages == 0 {
        return None;
    }
    let Some(with_guard) = pages.checked_add(1) else {
        return None;
    };
    let units = with_guard.div_ceil(UNIT_PAGES).next_power_of_two();
    let order = units.trailing_zeros();
    if order > MAX_ORDER as u32 {
        return None;
    }
    Some(order as u8)
}

/// The blocks and kept frames a purge detached: what waits for its one
/// shootdown, and nothing else may touch.
#[derive(Debug)]
#[must_use = "a detached block is given back only through `Area::give_back`"]
pub struct Detached {
    /// The first detached block's unit word, or [`END`].
    next: u32,
    /// The head of the kept frames' chain, if any.
    pub kept: Option<Frame>,
    /// How many frames the chain holds.
    pub kept_count: u64,
}

impl Detached {
    /// Whether nothing was detached: no block and no frame.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.next == END && self.kept.is_none()
    }
}

/// The area: its address buddy, its lazy list and its kept frames.
#[derive(Debug)]
pub struct Area<'a> {
    /// Free units.
    units: Frames<'a>,
    /// One word for each unit: [`NOT_LAZY`], or for the head of a block on the
    /// lazy list [`ON_LIST`], the next block's unit and the block's order.
    lazy: &'a mut [u32],
    /// The newest block on the lazy list, or [`END`].
    lazy_head: u32,
    /// Units on the lazy list.
    lazy_units: u64,
    /// The head of the kept frames' chain: frames of freed blocks that wait
    /// for the purge's shootdown before they go back.
    kept: Option<Frame>,
    /// Frames on that chain.
    kept_count: u64,
}

impl<'a> Area<'a> {
    /// An area of `units` units, every one free. `entries` and `lazy` hold one
    /// element for each unit, and may be longer; `None` if either is shorter,
    /// or `units` is above [`MOST_UNITS`].
    #[must_use]
    pub fn new(entries: &'a mut [PageEntry], lazy: &'a mut [u32], units: u64) -> Option<Self> {
        let count = usize::try_from(units).ok()?;
        if units > MOST_UNITS || entries.len() < count || lazy.len() < count {
            return None;
        }
        let (entries, _) = entries.split_at_mut(count);
        let (lazy, _) = lazy.split_at_mut(count);
        lazy.fill(NOT_LAZY);
        let mut free = Frames::new(entries, 0);
        free.insert_free(0, units);
        Some(Area {
            units: free,
            lazy,
            lazy_head: END,
            lazy_units: 0,
            kept: None,
            kept_count: 0,
        })
    }

    /// Units the area has.
    #[must_use]
    pub const fn units(&self) -> u64 {
        self.units.managed_frames()
    }

    /// Units free to hand out now.
    #[must_use]
    pub const fn free_units(&self) -> u64 {
        self.units.free_frames()
    }

    /// Units on the lazy list.
    #[must_use]
    pub const fn lazy_units(&self) -> u64 {
        self.lazy_units
    }

    /// Frames kept until the next purge's shootdown.
    #[must_use]
    pub const fn kept_count(&self) -> u64 {
        self.kept_count
    }

    /// A block for a request of `pages` pages, or `None` when no block of its
    /// order is free (or `pages` is none, or too many).
    pub fn take(&mut self, pages: u64) -> Option<Block> {
        let order = order_for(pages)?;
        let unit = self.units.allocate(order)?;
        Some(Block { unit, order })
    }

    /// Whether `block` is live: the head of an allocated block of its order,
    /// not on the lazy list. What a free asks before it touches anything of
    /// the block's.
    #[must_use]
    pub fn is_live(&self, block: Block) -> bool {
        self.units.allocated_head(block.unit, block.order).is_ok()
            && usize::try_from(block.unit)
                .ok()
                .and_then(|index| self.lazy.get(index))
                .is_some_and(|word| *word == NOT_LAZY)
    }

    /// Put `block`, taken with [`Area::take`] and freed, on the lazy list:
    /// its addresses are not handed out again until a purge has detached it
    /// and given it back.
    ///
    /// # Errors
    ///
    /// [`Frames::deallocate`]'s refusals for a block that is not the head of
    /// an allocated block of its order, and [`FrameError::NotAllocated`] for
    /// one already on the list; either way nothing changes.
    pub fn retire(&mut self, block: Block) -> Result<(), FrameError> {
        self.units.allocated_head(block.unit, block.order)?;
        let index = usize::try_from(block.unit).map_err(|_| FrameError::OutOfRange(block.unit))?;
        let word = self
            .lazy
            .get_mut(index)
            .ok_or(FrameError::OutOfRange(block.unit))?;
        if *word != NOT_LAZY {
            return Err(FrameError::NotAllocated(block.unit));
        }
        *word = ON_LIST | (self.lazy_head << ORDER_BITS) | u32::from(block.order);
        // Below `MOST_UNITS`, which `new` holds the area to.
        self.lazy_head = block.unit as u32;
        self.lazy_units += 1 << block.order;
        Ok(())
    }

    /// The head of the kept frames' chain, which the kernel links a freed
    /// block's own chain onto before [`Area::keep`].
    #[must_use]
    pub const fn kept(&self) -> Option<Frame> {
        self.kept
    }

    /// Make `head`, a chain of `count` frames whose last links to
    /// [`Area::kept`], the kept chain.
    pub fn keep(&mut self, head: Frame, count: u64) {
        self.kept = Some(head);
        self.kept_count += count;
    }

    /// Take the whole lazy list and the kept chain, for a purge: none of it
    /// is handed out or given back until [`Area::give_back`].
    pub fn detach(&mut self) -> Detached {
        let detached = Detached {
            next: self.lazy_head,
            kept: self.kept.take(),
            kept_count: self.kept_count,
        };
        self.lazy_head = END;
        self.lazy_units = 0;
        self.kept_count = 0;
        detached
    }

    /// Give the next block of `detached` back to the buddy, once the
    /// shootdown that covers it has returned, and answer it; `None` once all
    /// are back. A block the buddy refuses is kept from it, lost address
    /// space rather than a second owner, and answered all the same.
    pub fn give_back(&mut self, detached: &mut Detached) -> Option<Block> {
        let unit = detached.next;
        if unit == END {
            return None;
        }
        let word = self.lazy.get_mut(unit as usize)?;
        let order = (*word & ((1 << ORDER_BITS) - 1)) as u8;
        detached.next = (*word & !ON_LIST) >> ORDER_BITS;
        *word = NOT_LAZY;
        let block = Block {
            unit: u64::from(unit),
            order,
        };
        let _ = self.units.deallocate(block.unit, block.order);
        Some(block)
    }
}
