//! A buddy allocator for physical frames.
//!
//! # Why this is pure array manipulation
//!
//! A textbook buddy allocator threads its free lists through the free pages
//! themselves, which means every list operation is a write to physical memory
//! and the whole allocator can only be tested on a machine. This one keeps the
//! links in a side array — one [`PageEntry`] per frame — so allocating and
//! freeing is index arithmetic and nothing else.
//!
//! That is not a trick to make it testable, though it does: the side array has
//! to exist anyway. Copy-on-write needs a refcount per frame, reclaim needs to
//! know which object owns a frame, and neither can live in a page that is
//! currently in use. Linux reaches the same conclusion and calls it `struct
//! page`. Putting the free-list links there too costs nothing and buys a
//! `#![forbid(unsafe_code)]` allocator that `cargo test`, Miri and a fuzzer can
//! all drive.
//!
//! # What it does
//!
//! Blocks are powers of two frames, from one frame up to `2^MAX_ORDER`. A block
//! is split when a smaller one is needed and merged with its buddy when freed,
//! so the free lists stay as coarse as the allocation pattern allows.
//!
//! ```
//! # use ferrix_frame::{Frames, PageEntry, MAX_ORDER};
//! // One entry per frame, for frames 0x100..0x200.
//! let mut entries = [PageEntry::RESERVED; 0x100];
//! let mut frames = Frames::new(&mut entries, 0x100);
//! frames.insert_free(0x100, 0x100);
//!
//! let block = frames.allocate(3).unwrap(); // eight frames
//! assert_eq!(frames.free_frames(), 0x100 - 8);
//! frames.deallocate(block, 3);
//! assert_eq!(frames.free_frames(), 0x100);
//! # let _ = MAX_ORDER;
//! ```

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// Largest block the allocator will hand out, as a power of two frames.
///
/// Ten means 1024 frames, which is 4 MiB at a 4 KiB page size. Above that the
/// coalescing work stops paying for itself, and a caller that wants more should
/// be asking for a range rather than a block.
pub const MAX_ORDER: u8 = 10;

/// Number of free lists: orders `0..=MAX_ORDER`.
pub const ORDERS: usize = MAX_ORDER as usize + 1;

/// Sentinel for "no frame", since 0 is a perfectly good frame number.
const NONE: u32 = u32::MAX;

/// The owner of a frame nobody is charged for: what [`Frames::owner`] answers
/// for every frame [`Frames::set_owner`] was not called on.
pub const NO_OWNER: u32 = NONE;

/// A physical frame number: a physical address divided by the page size.
///
/// Frame numbers rather than addresses because the allocator never needs the
/// low twelve bits, and a `u64` of frames covers more address space than any
/// machine has.
pub type Frame = u64;

/// What one frame is being used for.
///
/// `repr(u8)` with `Reserved = 0` stated explicitly, and it is load-bearing.
///
/// The kernel carves the side array straight out of physical RAM, which means
/// it forms a `&mut [PageEntry]` over memory it has only zeroed. That is sound
/// only if the all-zero bit pattern is a *valid* `PageEntry` — an enum holding
/// a discriminant it does not define is undefined behaviour, not a surprising
/// value. Zero is valid precisely because `Reserved` is zero.
///
/// Note what is **not** claimed: a zeroed entry is not equal to
/// [`PageEntry::RESERVED`], whose links are `u32::MAX` rather than zero.
/// Nothing needs it to be — [`Frames::new`] fills the whole array before a
/// single entry is read. `zero_is_a_valid_state` in the tests pins the part
/// that does matter.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Not memory, or memory the kernel must never hand out.
    Reserved = 0,
    /// On a free list. Only the first frame of a free block is marked this way.
    Free = 1,
    /// Inside a free block but not its head.
    FreeTail = 2,
    /// Handed out. Only the first frame of an allocated block is marked this
    /// way, and it carries the block's order and its reference count.
    Allocated = 3,
    /// Inside an allocated block but not its head.
    ///
    /// A separate state rather than [`State::Allocated`] on every frame,
    /// because a tail carries neither an order nor a count: those live on the
    /// head. A tail that looked like a head would accept a `share` whose count
    /// nothing reads, or a `deallocate` that frees part of somebody's block.
    AllocatedTail = 4,
}

/// The per-frame record.
///
/// Sixteen bytes, which is one mebibyte of side array for every four gibibytes
/// of RAM. The links are `u32` frame indices rather than pointers: that caps
/// the allocator at 2^32 frames, which is 16 TiB, and halves the size of the
/// array that has to be allocated before anything else can be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageEntry {
    /// Next frame on the same free list, or [`NONE`].
    next: u32,
    /// Previous frame on the same free list, or [`NONE`]; for an allocated
    /// single frame, whoever it is charged to ([`Frames::set_owner`]).
    previous: u32,
    /// How many references exist to this frame, once it is allocated.
    ///
    /// Unused by the allocator itself, and the reason the side array exists at
    /// all: copy-on-write is a refcount on a frame.
    refcount: u32,
    /// Order of the block this frame heads, meaningful only when [`State::Free`]
    /// or [`State::Allocated`].
    order: u8,
    /// What the frame is doing.
    state: State,
    /// Objects still free in this frame, when the kernel heap is using it as a
    /// slab page.
    ///
    /// Meaningless to this allocator, which never reads it — the same
    /// arrangement as `refcount`, and for the same reason. It lives here
    /// because the alternative is a header stolen from the first object of
    /// every slab page, which costs a whole object of the smallest class and
    /// puts allocator metadata inside memory the allocator hands out. Two
    /// bytes that were padding anyway is a better trade: `PageEntry` is
    /// sixteen bytes with or without it.
    slab_free: u16,
}

impl PageEntry {
    /// A frame that is not available: the initial state of every entry, so an
    /// array that was never told about a region hands nothing out.
    pub const RESERVED: PageEntry = PageEntry {
        next: NONE,
        previous: NONE,
        refcount: 0,
        order: 0,
        state: State::Reserved,
        slab_free: 0,
    };

    /// What this frame is doing.
    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// How many references exist to this frame.
    #[must_use]
    pub const fn refcount(&self) -> u32 {
        self.refcount
    }

    /// Objects still free in this frame, if it is a slab page.
    #[must_use]
    pub const fn slab_free(&self) -> u16 {
        self.slab_free
    }
}

/// Why an operation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameError {
    /// The frame is outside the range this allocator covers.
    OutOfRange(Frame),
    /// The order is above [`MAX_ORDER`].
    OrderTooLarge(u8),
    /// The block being freed is not aligned to its own order.
    Misaligned(Frame),
    /// The block being freed was not allocated.
    NotAllocated(Frame),
    /// The frame has references beyond the caller's, so freeing it would take
    /// it out from under whoever else still holds it.
    StillShared(Frame),
    /// The reference count is saturated and cannot record another sharer.
    TooManyReferences(Frame),
    /// The frame is inside an allocated block rather than at its head. A block
    /// is freed, shared and released through its head, which is the only
    /// frame that knows the block's order and count.
    InsideBlock(Frame),
    /// The block was allocated at another order than the one given, or the
    /// call takes single frames and the block is larger.
    WrongOrder(Frame),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::OutOfRange(frame) => write!(f, "frame {frame:#x} is out of range"),
            FrameError::OrderTooLarge(order) => write!(f, "order {order} exceeds {MAX_ORDER}"),
            FrameError::Misaligned(frame) => {
                write!(f, "frame {frame:#x} is not aligned to its order")
            }
            FrameError::NotAllocated(frame) => write!(f, "frame {frame:#x} was not allocated"),
            FrameError::StillShared(frame) => {
                write!(f, "frame {frame:#x} is still shared")
            }
            FrameError::TooManyReferences(frame) => {
                write!(f, "frame {frame:#x} has too many references")
            }
            FrameError::InsideBlock(frame) => {
                write!(f, "frame {frame:#x} is inside a block, not its head")
            }
            FrameError::WrongOrder(frame) => {
                write!(f, "frame {frame:#x} heads a block of another order")
            }
        }
    }
}

/// What [`Frames::release`] did with the reference it dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Released {
    /// References remain, so the frame is still allocated and still mapped by
    /// somebody. Carries the count that is left.
    Shared(u32),
    /// The last reference went, and the frame is back in the allocator.
    Freed,
}

/// The allocator.
///
/// Borrows its side array rather than owning one, because the array has to be
/// carved out of memory before there is anything to carve it with. The kernel
/// bump-allocates it from the largest usable region the direct map reaches and
/// hands it here; a test passes a slice of a `Vec`.
#[derive(Debug)]
pub struct Frames<'a> {
    entries: &'a mut [PageEntry],
    /// Frame number of `entries[0]`.
    base: Frame,
    /// Head of each order's free list, or [`NONE`].
    heads: [u32; ORDERS],
    free: u64,
    managed: u64,
}

impl<'a> Frames<'a> {
    /// How many entries are needed to cover `lowest..highest` frames.
    #[must_use]
    pub const fn entries_needed(lowest: Frame, highest: Frame) -> usize {
        if highest <= lowest {
            0
        } else {
            (highest - lowest) as usize
        }
    }

    /// Wrap a side array covering frames `base..base + entries.len()`.
    ///
    /// Every frame starts [`State::Reserved`]: an allocator that was never told
    /// about a region hands nothing out, which is the safe direction for the
    /// mistake of forgetting to add one.
    pub fn new(entries: &'a mut [PageEntry], base: Frame) -> Frames<'a> {
        entries.fill(PageEntry::RESERVED);
        Frames {
            entries,
            base,
            heads: [NONE; ORDERS],
            free: 0,
            managed: 0,
        }
    }

    /// Frames currently available.
    #[must_use]
    pub const fn free_frames(&self) -> u64 {
        self.free
    }

    /// Frames this allocator has ever been given, free or not.
    #[must_use]
    pub const fn managed_frames(&self) -> u64 {
        self.managed
    }

    /// The lowest frame covered by the side array.
    #[must_use]
    pub const fn base(&self) -> Frame {
        self.base
    }

    /// One past the highest frame covered by the side array.
    #[must_use]
    pub const fn end(&self) -> Frame {
        self.base + self.entries.len() as u64
    }

    /// What `frame` is doing, or `None` if it is outside the covered range.
    #[must_use]
    pub fn state(&self, frame: Frame) -> Option<State> {
        Some(self.entry(frame)?.state)
    }

    /// Record how many objects are still free in a slab page.
    ///
    /// The kernel heap's bookkeeping, kept here rather than in the heap for
    /// the reason [`PageEntry::slab_free`] gives. Ignored for a frame outside
    /// this allocator's range, which is how a heap built on a different page
    /// supply — a test's, for instance — costs nothing.
    pub fn set_slab_free(&mut self, frame: Frame, objects: u16) {
        if let Some(entry) = self.entry_mut(frame) {
            entry.slab_free = objects;
        }
    }

    /// Objects still free in a slab page, or zero if the frame is unknown.
    #[must_use]
    pub fn slab_free(&self, frame: Frame) -> u16 {
        self.entry(frame).map_or(0, PageEntry::slab_free)
    }

    /// The record for `frame`, if it is covered.
    #[must_use]
    pub fn entry(&self, frame: Frame) -> Option<&PageEntry> {
        let index = self.index(frame)?;
        self.entries.get(index)
    }

    /// Give the allocator a range of usable frames.
    ///
    /// The range is broken into the largest naturally aligned blocks that fit,
    /// so a region added in one call is immediately available as big blocks
    /// rather than as a list of single frames waiting to be merged.
    ///
    /// Frames outside the covered range, and frames already known, are skipped.
    pub fn insert_free(&mut self, start: Frame, count: u64) {
        let mut frame = start.max(self.base);
        let end = start.saturating_add(count).min(self.end());

        while frame < end {
            let Some(index) = self.index(frame) else {
                break;
            };
            if self.entries.get(index).map(|entry| entry.state) != Some(State::Reserved) {
                frame += 1;
                continue;
            }

            let order = self.largest_block_at(frame, end);
            let frames = 1u64 << order;
            self.set_state_range(frame, frames, State::FreeTail);
            self.push(frame, order);
            self.free += frames;
            self.managed += frames;
            frame += frames;
        }
    }

    /// The largest order whose block starts at `frame` and ends by `end`.
    fn largest_block_at(&self, frame: Frame, end: Frame) -> u8 {
        let mut order = 0u8;
        while order < MAX_ORDER {
            let next = order + 1;
            let size = 1u64 << next;
            // A block of order `next` must be aligned to its own size and must
            // fit entirely inside the range being added.
            if !frame.is_multiple_of(size) || frame + size > end {
                break;
            }
            if !self.range_is_reserved(frame, size) {
                break;
            }
            order = next;
        }
        order
    }

    /// True if every frame in the range is still untouched.
    fn range_is_reserved(&self, frame: Frame, count: u64) -> bool {
        (0..count).all(|offset| {
            self.index(frame + offset)
                .and_then(|index| self.entries.get(index))
                .is_some_and(|entry| entry.state == State::Reserved)
        })
    }

    /// Take a block of `2^order` frames.
    ///
    /// Splits the smallest available block that is large enough, putting each
    /// unused half back on its own list.
    pub fn allocate(&mut self, order: u8) -> Option<Frame> {
        if order > MAX_ORDER {
            return None;
        }

        // The smallest order that has anything.
        let available = (order..=MAX_ORDER).find(|&candidate| self.head(candidate) != NONE)?;
        let frame = self.pop(available)?;
        Some(self.take(frame, available, order))
    }

    /// Take a block of `2^order` frames lying wholly below frame `limit`.
    ///
    /// For the rare caller that needs memory in a particular place: x86-64
    /// starts its application processors in real mode, which reaches only the
    /// first mebibyte. Slower than [`Frames::allocate`] — it walks the free
    /// lists looking for a low enough block instead of taking the head of one
    /// — which is the right trade for something done a few times a boot.
    ///
    /// Smaller orders are searched first, so a low single frame is found
    /// without splitting a large block when one is free.
    pub fn allocate_below(&mut self, order: u8, limit: Frame) -> Option<Frame> {
        if order > MAX_ORDER {
            return None;
        }
        let size = 1u64 << order;

        for available in order..=MAX_ORDER {
            let mut cursor = self.head(available);
            while cursor != NONE {
                let frame = self.base + u64::from(cursor);
                // What is handed out is the lowest `2^order` frames of this
                // free block — where splitting leaves it — so that is the
                // part that has to fit, not the whole block.
                if frame.saturating_add(size) <= limit {
                    self.unlink(frame, available);
                    return Some(self.take(frame, available, order));
                }
                cursor = self.entries.get(cursor as usize)?.next;
            }
        }
        None
    }

    /// Take `blocks` blocks of `2^MAX_ORDER` frames that lie back to back:
    /// one run of physical memory longer than the largest block, returned
    /// as its first frame, each block allocated at [`MAX_ORDER`] on its own.
    ///
    /// For the one caller that needs more contiguous memory than a block: a
    /// display controller with no scatter-gather reads a whole frame from
    /// one run of addresses, and 1920x1080 at four bytes a pixel is 8.3 MB
    /// where a block is 4 MiB. The module doc's "a caller that wants more
    /// should be asking for a range" is this call. Maximal blocks are what
    /// it walks, and only free ones: no block is split and nothing is moved,
    /// so a run exists only where the whole of it is free and aligned to
    /// maximal blocks, which on a machine not long booted is most of memory.
    /// The caller gives each block back on its own, or splits it and frees
    /// its frames one by one, exactly as it would a block from
    /// [`Frames::allocate`].
    ///
    /// `None` for zero blocks or when no such run is free.
    pub fn allocate_run(&mut self, blocks: u64) -> Option<Frame> {
        let size = 1u64 << MAX_ORDER;
        let free_maximal = |frames: &Self, frame: Frame| {
            frames
                .entry(frame)
                .is_some_and(|entry| entry.state == State::Free && entry.order == MAX_ORDER)
        };
        if blocks == 0 {
            return None;
        }
        let mut cursor = self.head(MAX_ORDER);
        let mut start = None;
        while cursor != NONE {
            let frame = self.base + u64::from(cursor);
            // Every block of a run is on the list; a run is found from its
            // lowest block, whichever order the list holds them in.
            let whole = (1..blocks).all(|k| {
                frame
                    .checked_add(k * size)
                    .is_some_and(|next| free_maximal(self, next))
            });
            if whole {
                start = Some(frame);
                break;
            }
            cursor = self.entries.get(cursor as usize)?.next;
        }
        let start = start?;
        for k in 0..blocks {
            let block = start + k * size;
            self.unlink(block, MAX_ORDER);
            let _ = self.take(block, MAX_ORDER, MAX_ORDER);
        }
        Some(start)
    }

    /// Finish taking a block of order `available`, already off its free list:
    /// split it down to `order`, freeing each unused upper half, and mark what
    /// is left allocated.
    ///
    /// Keeps the low half at every split, so the block returned is always the
    /// lowest address of what was taken.
    fn take(&mut self, frame: Frame, available: u8, order: u8) -> Frame {
        let mut current = available;
        while current > order {
            current -= 1;
            let buddy = frame + (1u64 << current);
            self.set_state_range(buddy, 1u64 << current, State::FreeTail);
            self.push(buddy, current);
        }

        let frames = 1u64 << order;
        self.set_state_range(frame, frames, State::AllocatedTail);
        if let Some(entry) = self.entry_mut(frame) {
            entry.state = State::Allocated;
            entry.order = order;
            entry.refcount = 1;
            entry.previous = NONE;
        }
        self.free -= frames;
        frame
    }

    /// The head of an allocated block of exactly `order`, or why `frame` is
    /// not one.
    ///
    /// The one check `deallocate`, `share` and `release` all need. A count is
    /// kept per *block*, on its head, so a call naming a tail or the wrong
    /// order is acting on a count that does not describe what it is about to
    /// change: a `release` of an order-3 head would free one frame of eight,
    /// and a `share` of a tail would raise a count nothing ever lowers.
    fn allocated_head(&self, frame: Frame, order: u8) -> Result<(), FrameError> {
        let entry = self.entry(frame).ok_or(FrameError::OutOfRange(frame))?;
        match entry.state {
            State::Allocated if entry.order == order => Ok(()),
            State::Allocated => Err(FrameError::WrongOrder(frame)),
            State::AllocatedTail => Err(FrameError::InsideBlock(frame)),
            State::Reserved | State::Free | State::FreeTail => Err(FrameError::NotAllocated(frame)),
        }
    }

    /// Turn an allocated block of `2^order` frames into as many single
    /// frames, each allocated on its own with one reference.
    ///
    /// For memory that has to be physically contiguous when it is taken and
    /// is owned page by page afterwards: a display controller that scans out
    /// one run of addresses reads a buffer whose pages a VMO holds, maps and
    /// gives back one at a time. Each frame is then exactly what
    /// [`Frames::allocate_frame`] returns, so [`Frames::release`] frees it and
    /// the block re-forms as its frames come back. Nothing is freed or taken
    /// here; the free count does not change.
    ///
    /// # Errors
    ///
    /// The refusals of [`Frames::deallocate`] for a frame that is not the head
    /// of an allocated block of exactly `order`, and
    /// [`FrameError::StillShared`] for a block somebody else holds too.
    pub fn split(&mut self, frame: Frame, order: u8) -> Result<(), FrameError> {
        if order > MAX_ORDER {
            return Err(FrameError::OrderTooLarge(order));
        }
        self.allocated_head(frame, order)?;
        if self.entry(frame).is_some_and(|entry| entry.refcount > 1) {
            return Err(FrameError::StillShared(frame));
        }
        for single in frame..frame + (1u64 << order) {
            if let Some(entry) = self.entry_mut(single) {
                entry.state = State::Allocated;
                entry.order = 0;
                entry.refcount = 1;
                // A tail's link is whatever its last free list left there,
                // and an allocated frame's link is its owner.
                entry.previous = NONE;
            }
        }
        Ok(())
    }

    /// Take a single frame.
    pub fn allocate_frame(&mut self) -> Option<Frame> {
        self.allocate(0)
    }

    /// The frames this allocator's side array covers: the first, and how
    /// many.
    #[must_use]
    pub fn span(&self) -> (Frame, u64) {
        (self.base, self.entries.len() as u64)
    }

    /// The free block holding `frame`: its first frame and its order, the
    /// block [`Frames::claim`] would split; `None` when `frame` is not free.
    #[must_use]
    pub fn free_block_of(&self, frame: Frame) -> Option<(Frame, u8)> {
        (0..=MAX_ORDER).find_map(|order| {
            let head = frame & !((1u64 << order) - 1);
            self.entry(head)
                .filter(|entry| entry.state == State::Free && entry.order == order)
                .map(|_| (head, order))
        })
    }

    /// Whether a free block of `order` or larger exists other than the one
    /// starting at `head`: so that taking that one leaves a request of any
    /// order up to `order` still able to be met (`docs/RAM-SCRUBBER.md`
    /// §3.1). One look at each list's first two entries.
    #[must_use]
    pub fn another_free_block(&self, head: Frame, order: u8) -> bool {
        let own = self.index(head).and_then(|index| u32::try_from(index).ok());
        (order..=MAX_ORDER).any(|at| {
            let first = self.head(at);
            if first == NONE {
                return false;
            }
            if at != order || Some(first) != own {
                return true;
            }
            self.entries
                .get(first as usize)
                .is_some_and(|entry| entry.next != NONE)
        })
    }

    /// Take exactly `frame`, if it is free, as a single allocated frame.
    ///
    /// For a check that has to get back the very frame it just gave up — to
    /// fill it with a pattern and prove nothing still writes to it — which
    /// [`Frames::allocate`] cannot promise: the freed frame may have merged
    /// with its buddy, and the head of the smallest list may be some other
    /// block. The free block holding `frame` is found by its head, taken off
    /// its list, and split down with every half not holding `frame` put back.
    ///
    /// `None` if `frame` is not free or not this allocator's.
    pub fn claim(&mut self, frame: Frame) -> Option<Frame> {
        let (head, order) = (0..=MAX_ORDER).find_map(|order| {
            let head = frame & !((1u64 << order) - 1);
            self.entry(head)
                .filter(|entry| entry.state == State::Free && entry.order == order)
                .map(|_| (head, order))
        })?;
        self.unlink(head, order);

        let (mut block, mut current) = (head, order);
        while current > 0 {
            current -= 1;
            let half = 1u64 << current;
            let upper = block + half;
            let (keep, give) = if frame >= upper {
                (upper, block)
            } else {
                (block, upper)
            };
            self.set_state_range(give, half, State::FreeTail);
            self.push(give, current);
            block = keep;
        }

        if let Some(entry) = self.entry_mut(frame) {
            entry.state = State::Allocated;
            entry.order = 0;
            entry.refcount = 1;
            entry.previous = NONE;
        }
        self.free -= 1;
        Some(frame)
    }

    /// Give back a block taken with [`Frames::allocate`].
    ///
    /// `order` must be the order it was allocated with. Merges with the buddy
    /// as far up as the buddy is free, so a workload that frees everything ends
    /// with the free lists it started with.
    pub fn deallocate(&mut self, frame: Frame, order: u8) -> Result<(), FrameError> {
        if order > MAX_ORDER {
            return Err(FrameError::OrderTooLarge(order));
        }
        if self.index(frame).is_none() {
            return Err(FrameError::OutOfRange(frame));
        }
        if !frame.is_multiple_of(1u64 << order) {
            return Err(FrameError::Misaligned(frame));
        }
        // The head, at the order it was taken with. Freeing a block at a
        // smaller order leaks the rest of it; at a larger one, or from a tail,
        // it hands out frames somebody else still holds.
        self.allocated_head(frame, order)?;
        // A frame two address spaces share is not one caller's to free. Without
        // this the other sharer keeps a mapping to a frame the allocator has
        // since handed to somebody else, which is the quietest memory
        // corruption there is: nothing faults, the page simply starts changing
        // under one of its owners. [`Frames::release`] is the way to drop a
        // reference, and it reaches here only once the count is zero.
        if self.entry(frame).is_some_and(|entry| entry.refcount > 1) {
            return Err(FrameError::StillShared(frame));
        }

        let frames = 1u64 << order;
        self.free += frames;
        self.set_state_range(frame, frames, State::FreeTail);

        let (frame, order) = self.coalesce(frame, order);
        self.push(frame, order);
        Ok(())
    }

    /// Record another reference to an allocated frame, and report the new
    /// count.
    ///
    /// This is the whole of what copy-on-write asks of an allocator. `fork`
    /// marks both address spaces' regions read-only and calls this once per
    /// frame the two now share; the write fault that follows copies the page
    /// and calls [`Frames::release`] for the reference it stopped needing. The
    /// allocator itself never reads the count — it is the kernel's
    /// bookkeeping, kept here because this is the array with one slot per
    /// frame, and because the alternative is a second array indexed the same
    /// way.
    ///
    /// Shared frames are single frames, and that is enforced rather than
    /// assumed: the unit a fault copies is a page, and [`Frames::release`]
    /// frees at order 0, so a count on a larger block would free one frame of
    /// it when the count ran out. Sharing a huge page wants its own call.
    ///
    /// # Errors
    ///
    /// [`FrameError::OutOfRange`] if the frame is not this allocator's;
    /// [`FrameError::NotAllocated`] if it is free, since a free frame has no
    /// contents to share; [`FrameError::InsideBlock`] for a frame inside a
    /// larger allocation and [`FrameError::WrongOrder`] for the head of one;
    /// and [`FrameError::TooManyReferences`] if the count would wrap. Wrapping
    /// is refused rather than allowed because a wrapped count frees memory
    /// that somebody is still reading out of, and the refusal is a mapping
    /// that fails where the alternative is corruption that does not.
    pub fn share(&mut self, frame: Frame) -> Result<u32, FrameError> {
        self.allocated_head(frame, 0)?;

        let Some(entry) = self.entry_mut(frame) else {
            return Err(FrameError::OutOfRange(frame));
        };
        let Some(raised) = entry.refcount.checked_add(1) else {
            return Err(FrameError::TooManyReferences(frame));
        };
        entry.refcount = raised;
        Ok(raised)
    }

    /// Drop one reference to an allocated frame, freeing it if it was the last.
    ///
    /// The counterpart of [`Frames::share`], and the only correct way to undo
    /// one: a caller that tracked sharing itself and then called
    /// [`Frames::deallocate`] would be refused, which is the point of the
    /// check there.
    ///
    /// Freeing goes through `deallocate` at order 0, so the frame coalesces
    /// with its buddy exactly as any other single frame does — a process that
    /// exits gives its memory back in whatever blocks it can re-form, not as a
    /// heap of orphaned single frames.
    ///
    /// # Errors
    ///
    /// [`FrameError::OutOfRange`] if the frame is not this allocator's,
    /// [`FrameError::NotAllocated`] if it is already free, and
    /// [`FrameError::InsideBlock`] or [`FrameError::WrongOrder`] for any frame
    /// of an allocation larger than one frame — the same refusals as
    /// [`Frames::share`], and for the same reason.
    pub fn release(&mut self, frame: Frame) -> Result<Released, FrameError> {
        self.allocated_head(frame, 0)?;

        let Some(entry) = self.entry_mut(frame) else {
            return Err(FrameError::OutOfRange(frame));
        };
        // Saturating rather than checked: an allocated frame whose count is
        // already zero is an inconsistency somewhere above, and the useful
        // response is to free it once rather than to leak it forever.
        let remaining = entry.refcount.saturating_sub(1);
        entry.refcount = remaining;

        if remaining > 0 {
            return Ok(Released::Shared(remaining));
        }

        self.deallocate(frame, 0)?;
        Ok(Released::Freed)
    }

    /// Charge an allocated single frame to `owner`: a number the caller
    /// chooses, kept in the link an allocated frame does not use, and handed
    /// back by [`Frames::owner`] until the frame is freed.
    ///
    /// The kernel's job quotas are why: a frame is freed under whatever lock
    /// its last holder had, and has to find what to uncharge from the frame
    /// alone. Only single frames, for the reason [`Frames::share`] gives.
    ///
    /// # Errors
    ///
    /// The refusals of [`Frames::share`] for a frame that is not an allocated
    /// single frame.
    pub fn set_owner(&mut self, frame: Frame, owner: u32) -> Result<(), FrameError> {
        self.allocated_head(frame, 0)?;
        let entry = self.entry_mut(frame).ok_or(FrameError::OutOfRange(frame))?;
        entry.previous = owner;
        Ok(())
    }

    /// Who an allocated single frame is charged to, or [`NO_OWNER`]: for a
    /// frame never charged, a free one, or a block.
    #[must_use]
    pub fn owner(&self, frame: Frame) -> u32 {
        self.entry(frame)
            .filter(|entry| entry.state == State::Allocated && entry.order == 0)
            .map_or(NONE, |entry| entry.previous)
    }

    /// Merge upwards while the buddy is a free block of the same order.
    fn coalesce(&mut self, mut frame: Frame, mut order: u8) -> (Frame, u8) {
        while order < MAX_ORDER {
            // Exactly one bit apart: the buddy of an aligned block of order `o`
            // is the block that would pair with it to form order `o + 1`.
            let buddy = frame ^ (1u64 << order);

            let mergeable = self
                .entry(buddy)
                .is_some_and(|entry| entry.state == State::Free && entry.order == order);
            if !mergeable {
                break;
            }

            self.unlink(buddy, order);
            if let Some(entry) = self.entry_mut(buddy) {
                entry.state = State::FreeTail;
            }
            // The merged block starts at whichever half is lower.
            frame = frame.min(buddy);
            order += 1;
        }
        (frame, order)
    }

    // -- free list plumbing ------------------------------------------------

    /// Put a block at the head of its order's list.
    fn push(&mut self, frame: Frame, order: u8) {
        let Some(index) = self.index(frame) else {
            return;
        };
        let head = self.head(order);

        if let Some(entry) = self.entries.get_mut(index) {
            entry.next = head;
            entry.previous = NONE;
            entry.order = order;
            entry.state = State::Free;
            entry.refcount = 0;
        }
        if head != NONE
            && let Some(entry) = self.entries.get_mut(head as usize)
        {
            entry.previous = index as u32;
        }
        self.set_head(order, index as u32);
    }

    /// Take the first block off an order's list.
    fn pop(&mut self, order: u8) -> Option<Frame> {
        let index = self.head(order);
        if index == NONE {
            return None;
        }
        let next = self.entries.get(index as usize)?.next;
        self.set_head(order, next);
        if next != NONE
            && let Some(entry) = self.entries.get_mut(next as usize)
        {
            entry.previous = NONE;
        }
        if let Some(entry) = self.entries.get_mut(index as usize) {
            entry.next = NONE;
            entry.previous = NONE;
        }
        Some(self.base + u64::from(index))
    }

    /// Remove a block from the middle of its order's list.
    fn unlink(&mut self, frame: Frame, order: u8) {
        let Some(index) = self.index(frame) else {
            return;
        };
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let (next, previous) = (entry.next, entry.previous);

        if previous == NONE {
            self.set_head(order, next);
        } else if let Some(entry) = self.entries.get_mut(previous as usize) {
            entry.next = next;
        }
        if next != NONE
            && let Some(entry) = self.entries.get_mut(next as usize)
        {
            entry.previous = previous;
        }
        if let Some(entry) = self.entries.get_mut(index) {
            entry.next = NONE;
            entry.previous = NONE;
        }
    }

    // -- small helpers -----------------------------------------------------

    /// Head of `order`'s free list.
    ///
    /// `heads` is a fixed-size array and every caller has already bounded
    /// `order` by `MAX_ORDER`, but `indexing_slicing` is denied across the
    /// workspace and rightly does not take that on trust. Saying the bound
    /// once here is cheaper than arguing it at six call sites.
    fn head(&self, order: u8) -> u32 {
        self.heads.get(order as usize).copied().unwrap_or(NONE)
    }

    /// Set the head of `order`'s free list.
    fn set_head(&mut self, order: u8, value: u32) {
        if let Some(slot) = self.heads.get_mut(order as usize) {
            *slot = value;
        }
    }

    /// Index into the side array, or `None` outside the covered range.
    fn index(&self, frame: Frame) -> Option<usize> {
        let offset = frame.checked_sub(self.base)?;
        let index = usize::try_from(offset).ok()?;
        (index < self.entries.len()).then_some(index)
    }

    fn entry_mut(&mut self, frame: Frame) -> Option<&mut PageEntry> {
        let index = self.index(frame)?;
        self.entries.get_mut(index)
    }

    /// Set the state of every frame in a block.
    fn set_state_range(&mut self, frame: Frame, count: u64, state: State) {
        for offset in 0..count {
            if let Some(entry) = self.entry_mut(frame + offset) {
                entry.state = state;
            }
        }
    }

    /// Blocks currently on each order's free list, for tests and diagnostics.
    #[must_use]
    pub fn free_blocks(&self) -> [usize; ORDERS] {
        let mut counts = [0usize; ORDERS];
        for (order, &head) in self.heads.iter().enumerate() {
            let mut cursor = head;
            while cursor != NONE {
                if let Some(slot) = counts.get_mut(order) {
                    *slot += 1;
                }
                let Some(entry) = self.entries.get(cursor as usize) else {
                    break;
                };
                cursor = entry.next;
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests;
