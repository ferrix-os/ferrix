//! The kernel heap: segregated free lists over a page supply.
//!
//! This is what makes `alloc` work in the kernel, and therefore what every
//! later stage is built out of — a scheduler is a `Vec` of tasks, a VFS is a
//! `BTreeMap` of dentries.
//!
//! # Where the memory access went
//!
//! A free-list allocator keeps its links inside the free blocks, which means
//! every operation is a load or a store to the heap. That would make the whole
//! thing untestable off a machine, so the loads and stores are behind
//! [`Backing`] — the same shape as `ferrix_paging::PhysMem`. A test implements
//! it over a map; the kernel implements it over the direct map and the buddy
//! allocator.
//!
//! The allocator's own body contains no unsafe at all, but the crate cannot say
//! `#![forbid(unsafe_code)]` the way `ferrix-frame` and `ferrix-elf` do: that
//! also forbids *declaring* [`Backing`] as an `unsafe trait`, and the `unsafe`
//! on that declaration is the whole point. It is what obliges an implementor to
//! have read the contract, and dropping it to satisfy a lint would move the
//! danger from somewhere the compiler marks it to somewhere it does not.
//!
//! # The design, and its one known limitation
//!
//! Requests up to [`LARGEST_CLASS`] bytes are rounded to a power-of-two size
//! class and served from that class's free list, refilled a page at a time.
//! Larger requests go straight to [`Backing`] as whole pages.
//!
//! Because the classes are powers of two and a slab page is page aligned,
//! alignment up to the class size comes out for free — an eight-byte object is
//! eight-byte aligned because it sits at a multiple of eight from a 4 KiB
//! boundary.
//!
//! **A slab page goes back when its last object is freed**, provided the class
//! still has another page to serve from. The bookkeeping that makes that
//! possible is a count of free objects per page, and it lives in the frame
//! allocator's per-frame record — [`Backing::slab_free`] and
//! [`Backing::set_slab_free`] are how this crate reaches it. The alternative,
//! a header stolen from the first object of each page, costs a whole object of
//! the smallest class and puts allocator metadata inside memory the allocator
//! hands out; the per-frame record had two bytes of padding going spare.
//!
//! The "provided the class still has another page" clause is not timidity. A
//! class oscillating around a single object would otherwise return its only
//! page and immediately ask for it back on the next allocation, which turns a
//! free-list pop into a buddy allocation and a page of writes. Keeping one
//! page per class costs at most 36 KiB across all nine classes.

#![no_std]

use core::fmt;

/// Bytes in a page. Both architectures are configured for 4 KiB.
pub const PAGE_SIZE: usize = 4096;

/// Size classes, in bytes. Powers of two from a link-sized object up to half a
/// page: above that, a class wastes more than it saves and the request is
/// better served as whole pages.
pub const CLASS_SIZES: [usize; 9] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];

/// Number of size classes.
pub const CLASSES: usize = CLASS_SIZES.len();

/// Largest request served from a size class.
pub const LARGEST_CLASS: usize = 2048;

/// The smallest object, which must be large enough to hold a free-list link.
pub const SMALLEST_CLASS: usize = 8;

/// The page an object lives in.
///
/// Slab pages are page aligned and an object never straddles one, because
/// every class size divides [`PAGE_SIZE`].
const fn page_of(address: u64) -> u64 {
    address & !(PAGE_SIZE as u64 - 1)
}

/// Marks the end of a free list. Zero is not a valid heap address in any
/// address space the kernel runs in — the kernel half starts at
/// `0xFFFF_8000_0000_0000` — so it can be the sentinel without a wrapper type.
const END: u64 = 0;

/// What the caller asked for.
///
/// A local type rather than `core::alloc::Layout` so the crate has no opinion
/// about the allocator API and can be tested with plain numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    /// Bytes needed. Zero is rounded up to one, the way `Layout` allows.
    pub size: usize,
    /// Required alignment, a power of two.
    pub align: usize,
}

impl Request {
    /// A request for `size` bytes with natural alignment.
    #[must_use]
    pub const fn new(size: usize, align: usize) -> Request {
        Request { size, align }
    }

    /// The number of bytes that actually have to be reserved.
    ///
    /// An alignment larger than the size forces a bigger class, because the
    /// classes are the only alignment guarantee on offer.
    #[must_use]
    pub const fn effective_size(&self) -> usize {
        let size = if self.size == 0 { 1 } else { self.size };
        if self.align > size { self.align } else { size }
    }
}

/// Where a heap gets pages, and how it reaches the memory in them.
///
/// # Safety
///
/// An implementation must guarantee that:
///
/// * `allocate_pages` returns the address of `1 << order` contiguous,
///   page-aligned, otherwise-unused pages, which stay valid until they are
///   given back;
/// * `read_link` and `write_link` address eight readable, writable, aligned
///   bytes inside memory this heap owns, aliased by nothing else.
pub unsafe trait Backing {
    /// Take `1 << order` contiguous pages, or `None` if there are none.
    fn allocate_pages(&mut self, order: u8) -> Option<u64>;

    /// Give back pages taken with [`Backing::allocate_pages`].
    fn deallocate_pages(&mut self, address: u64, order: u8);

    /// Read the free-list link stored in a free object.
    fn read_link(&self, at: u64) -> u64;

    /// Write the free-list link stored in a free object.
    fn write_link(&mut self, at: u64, value: u64);

    /// How many objects are free in the slab page at `page`.
    ///
    /// `page` is always an address [`Backing::allocate_pages`] returned with
    /// an order of zero and that has not been given back. An implementation
    /// with nowhere to keep the number may return zero from this and ignore
    /// [`Backing::set_slab_free`]; the only consequence is that slab pages are
    /// never returned, which is what this allocator did before the count
    /// existed.
    fn slab_free(&self, page: u64) -> u16;

    /// Record how many objects are free in the slab page at `page`.
    fn set_slab_free(&mut self, page: u64, objects: u16);
}

/// Why an allocation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeapError {
    /// The page supply is exhausted.
    OutOfMemory,
    /// The alignment was not a power of two.
    BadAlignment(usize),
    /// The request is larger than the allocator will serve.
    ///
    /// The cap is what a single buddy block can cover; anything larger wants a
    /// virtual mapping rather than contiguous physical memory.
    TooLarge(usize),
}

impl fmt::Display for HeapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeapError::OutOfMemory => f.write_str("out of memory"),
            HeapError::BadAlignment(align) => write!(f, "alignment {align} is not a power of two"),
            HeapError::TooLarge(size) => write!(f, "{size} bytes is larger than one block"),
        }
    }
}

/// Largest order the heap will ask its page supply for. Matches the buddy
/// allocator's own maximum, which is 4 MiB at a 4 KiB page size.
const MAX_ORDER: u8 = 10;

/// The heap.
#[derive(Debug)]
pub struct Heap {
    /// Head of each class's free list, or [`END`].
    free: [u64; CLASSES],
    /// Bytes currently handed out, counted at class granularity so it reflects
    /// what the heap actually reserved rather than what was asked for.
    allocated: usize,
    /// Pages currently held for the size classes.
    slab_pages: usize,
    /// Of those, how many belong to each class. Consulted for one decision:
    /// whether a page that has just emptied is the class's last one, in which
    /// case it is kept rather than returned.
    pages_per_class: [usize; CLASSES],
    /// Pages currently out as large allocations.
    large_pages: usize,
    /// Slab pages handed back to the page supply since boot, for diagnostics:
    /// a heap whose classes churn should show this rising, and one that only
    /// ever grows should show it at zero.
    slab_pages_returned: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Heap::new()
    }
}

impl Heap {
    /// An empty heap. Takes no memory until something is allocated.
    #[must_use]
    pub const fn new() -> Heap {
        Heap {
            free: [END; CLASSES],
            allocated: 0,
            slab_pages: 0,
            pages_per_class: [0; CLASSES],
            large_pages: 0,
            slab_pages_returned: 0,
        }
    }

    /// Bytes currently handed out.
    #[must_use]
    pub const fn allocated_bytes(&self) -> usize {
        self.allocated
    }

    /// Pages the heap is holding for its size classes.
    #[must_use]
    pub const fn slab_pages(&self) -> usize {
        self.slab_pages
    }

    /// Slab pages this heap has given back to its page supply.
    #[must_use]
    pub const fn slab_pages_returned(&self) -> usize {
        self.slab_pages_returned
    }

    /// Pages currently out as large allocations.
    #[must_use]
    pub const fn large_pages(&self) -> usize {
        self.large_pages
    }

    /// The size class that serves `request`, or `None` if it is too large.
    ///
    /// Public because the kernel's `GlobalAlloc` needs the same answer on the
    /// free path, and computing it twice from one definition is how the two
    /// paths stay in agreement.
    #[must_use]
    pub fn class_of(request: Request) -> Option<usize> {
        let needed = request.effective_size();
        if needed > LARGEST_CLASS {
            return None;
        }
        CLASS_SIZES.iter().position(|&size| size >= needed)
    }

    /// The buddy order that serves a request too large for a class.
    fn order_of(request: Request) -> Result<u8, HeapError> {
        let bytes = request.effective_size();
        let pages = bytes.div_ceil(PAGE_SIZE);
        let order = u8::try_from(pages.next_power_of_two().trailing_zeros())
            .map_err(|_| HeapError::TooLarge(bytes))?;
        if order > MAX_ORDER {
            return Err(HeapError::TooLarge(bytes));
        }
        Ok(order)
    }

    /// Allocate.
    pub fn allocate(
        &mut self,
        backing: &mut impl Backing,
        request: Request,
    ) -> Result<u64, HeapError> {
        if !request.align.is_power_of_two() {
            return Err(HeapError::BadAlignment(request.align));
        }

        let Some(class) = Heap::class_of(request) else {
            let order = Heap::order_of(request)?;
            let address = backing
                .allocate_pages(order)
                .ok_or(HeapError::OutOfMemory)?;
            self.large_pages += 1 << order;
            self.allocated += (1usize << order) * PAGE_SIZE;
            return Ok(address);
        };

        if self.head(class) == END {
            self.refill(backing, class)?;
        }

        let address = self.pop(backing, class).ok_or(HeapError::OutOfMemory)?;
        self.take_from_page(backing, address);
        self.allocated += Heap::class_size(class);
        Ok(address)
    }

    /// Free something returned by [`Heap::allocate`].
    ///
    /// `request` must be the one it was allocated with, exactly as
    /// `GlobalAlloc::dealloc` requires — it is what says which free list the
    /// memory belongs on, and there is no header to ask instead.
    pub fn deallocate(&mut self, backing: &mut impl Backing, address: u64, request: Request) {
        let Some(class) = Heap::class_of(request) else {
            let Ok(order) = Heap::order_of(request) else {
                return;
            };
            backing.deallocate_pages(address, order);
            self.large_pages = self.large_pages.saturating_sub(1 << order);
            self.allocated = self.allocated.saturating_sub((1usize << order) * PAGE_SIZE);
            return;
        };

        self.push(backing, class, address);
        self.allocated = self.allocated.saturating_sub(Heap::class_size(class));
        self.give_back_if_empty(backing, class, address);
    }

    /// One object of the page holding `address` has been handed out.
    fn take_from_page(&mut self, backing: &mut impl Backing, address: u64) {
        let page = page_of(address);
        let free = backing.slab_free(page);
        backing.set_slab_free(page, free.saturating_sub(1));
    }

    /// Return the slab page holding `address` if that free emptied it.
    ///
    /// The list walk is what this costs, and it is paid only on the free that
    /// empties a page rather than on every free: a page with objects still out
    /// cannot be returned, so there is nothing to look for. What the walk does
    /// is unlink the page's objects from the class's free list, which a singly
    /// linked list gives no cheaper way to do.
    fn give_back_if_empty(&mut self, backing: &mut impl Backing, class: usize, address: u64) {
        let page = page_of(address);
        let objects = Heap::objects_per_page(class);
        let free = backing.slab_free(page).saturating_add(1);
        backing.set_slab_free(page, free);

        if usize::from(free) != objects {
            return;
        }
        // The last page of a class stays. See the note at the top of the file:
        // returning it means the next allocation pays for a buddy block and a
        // page of writes to get it straight back.
        if self.pages_per_class.get(class).copied().unwrap_or(0) <= 1 {
            return;
        }

        self.unlink_page(backing, class, page);
        backing.set_slab_free(page, 0);
        backing.deallocate_pages(page, 0);
        self.slab_pages = self.slab_pages.saturating_sub(1);
        self.slab_pages_returned += 1;
        if let Some(count) = self.pages_per_class.get_mut(class) {
            *count = count.saturating_sub(1);
        }
    }

    /// Take every object belonging to `page` off `class`'s free list.
    ///
    /// Every object *is* on the list — the caller has just established that
    /// the page is entirely free — so this removes exactly
    /// [`Heap::objects_per_page`] of them.
    fn unlink_page(&mut self, backing: &mut impl Backing, class: usize, page: u64) {
        let mut cursor = self.head(class);
        let mut previous = END;

        // Bounded by the objects that can exist on any list, so a corrupted
        // link that forms a cycle cannot hang the free path.
        let mut budget = self.slab_pages.saturating_add(1) * (PAGE_SIZE / SMALLEST_CLASS);
        while cursor != END && budget > 0 {
            budget -= 1;
            let next = backing.read_link(cursor);
            if page_of(cursor) == page {
                self.unlink_one(backing, class, previous, next);
            } else {
                previous = cursor;
            }
            cursor = next;
        }
    }

    /// Drop one object out of a free list, given the object before it.
    ///
    /// `previous` is [`END`] when the object is the list head, which is the
    /// only case that touches `self.free` rather than a link in the page.
    fn unlink_one(&mut self, backing: &mut impl Backing, class: usize, previous: u64, next: u64) {
        if previous == END {
            if let Some(slot) = self.free.get_mut(class) {
                *slot = next;
            }
        } else {
            backing.write_link(previous, next);
        }
    }

    /// Objects of `class` that fit in one page.
    fn objects_per_page(class: usize) -> usize {
        PAGE_SIZE / Heap::class_size(class)
    }

    /// Carve one fresh page into objects and put them all on a class's list.
    fn refill(&mut self, backing: &mut impl Backing, class: usize) -> Result<(), HeapError> {
        let size = Heap::class_size(class);
        let page = backing.allocate_pages(0).ok_or(HeapError::OutOfMemory)?;
        self.slab_pages += 1;
        if let Some(count) = self.pages_per_class.get_mut(class) {
            *count += 1;
        }

        // Backwards, so the list comes out in ascending address order. It costs
        // nothing and makes a heap dump readable, which matters the first time
        // something corrupts one.
        let objects = PAGE_SIZE / size;
        for index in (0..objects).rev() {
            let object = page + (index * size) as u64;
            self.push_raw(backing, class, object);
        }
        // Every object is free, which is what makes the count a count rather
        // than a guess: it is set here, decremented on each `allocate` and
        // incremented on each `deallocate`, and only ever reaches `objects`
        // again when all of them have come back.
        backing.set_slab_free(page, u16::try_from(objects).unwrap_or(u16::MAX));
        Ok(())
    }

    // -- free list plumbing ------------------------------------------------

    /// Bytes in one object of `class`.
    fn class_size(class: usize) -> usize {
        CLASS_SIZES.get(class).copied().unwrap_or(LARGEST_CLASS)
    }

    /// Head of a class's list.
    fn head(&self, class: usize) -> u64 {
        self.free.get(class).copied().unwrap_or(END)
    }

    /// Put an object on a class's list.
    fn push(&mut self, backing: &mut impl Backing, class: usize, address: u64) {
        self.push_raw(backing, class, address);
    }

    fn push_raw(&mut self, backing: &mut impl Backing, class: usize, address: u64) {
        let head = self.head(class);
        backing.write_link(address, head);
        if let Some(slot) = self.free.get_mut(class) {
            *slot = address;
        }
    }

    /// Take an object off a class's list.
    fn pop(&mut self, backing: &mut impl Backing, class: usize) -> Option<u64> {
        let head = self.head(class);
        if head == END {
            return None;
        }
        let next = backing.read_link(head);
        if let Some(slot) = self.free.get_mut(class) {
            *slot = next;
        }
        Some(head)
    }

    /// Objects waiting on each class's list, for tests and diagnostics.
    #[must_use]
    pub fn free_objects(&self, backing: &impl Backing) -> [usize; CLASSES] {
        let mut counts = [0usize; CLASSES];
        for (class, slot) in counts.iter_mut().enumerate() {
            let mut cursor = self.head(class);
            // Bounded: a corrupted link that forms a cycle must not hang a
            // diagnostic, which is exactly when it would be called.
            let mut budget = PAGE_SIZE / SMALLEST_CLASS * 1024;
            while cursor != END && budget > 0 {
                *slot += 1;
                cursor = backing.read_link(cursor);
                budget -= 1;
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests;
