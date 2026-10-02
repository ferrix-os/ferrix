//! Virtual memory areas: the map a process's address space is made of.
//!
//! An address space is a sorted, non-overlapping set of regions. Each region
//! ([`Vma`]) covers a page-aligned half-open range and carries permissions and
//! a backing object, and the three system calls that reshape it are
//! [`AddressSpace::map_fixed`] (`mmap` with `MAP_FIXED`),
//! [`AddressSpace::remove`] (`munmap`) and [`AddressSpace::protect`]
//! (`mprotect`).
//!
//! # Every range here came from a user program
//!
//! The arguments are whatever a process passed to `mmap`, so every operation
//! is total: it either succeeds or returns a [`VmaError`], and it never
//! panics, indexes out of bounds or overflows. Ranges are validated once, on
//! the way in, by [`PageRange`], whose fields are private precisely so that a
//! value of that type is a standing promise that the range is page-aligned,
//! non-empty and does not wrap. The map then only has to check the one thing
//! that is its own business, which is whether the range is inside the usable
//! window. The single failure not reported as a `VmaError` is allocation
//! failure, which the global allocator handles as it does everywhere in
//! `alloc`.
//!
//! # Why a `Vec` and not a `BTreeMap`
//!
//! Regions live in a `Vec` kept sorted by start address and searched by binary
//! search. A `BTreeMap` keyed by start address would give the same lookups,
//! but every operation here works on a *span* of adjacent regions -- split at
//! both edges, delete what lies between them, then look at the neighbour on
//! each side to decide whether to merge -- and a contiguous index range
//! expresses that directly, where a tree needs a cursor and a second lookup
//! for each neighbour. Real address spaces hold tens to a few hundred regions,
//! so the memmove an insertion costs is cheaper than the pointer chasing it
//! replaces, and the whole structure is easier to argue about.
//!
//! # Merging is not an optimisation
//!
//! Two adjacent regions with identical flags and contiguous backing are merged
//! into one. Without that, a program that `mprotect`s one page in the middle
//! of its heap and then puts the protection back has three regions where it
//! started with one, and repeating that leaks regions until the kernel refuses
//! the next `mmap`.
//!
//! ```
//! use ferrix_vma::{AddressSpace, Backing, PageRange, VmaFlags};
//!
//! let mut space = AddressSpace::new(0x1000, 0x0000_8000_0000_0000).unwrap();
//! let text = PageRange::new(0x40_0000, 0x40_4000).unwrap();
//! space
//!     .insert(text, VmaFlags::READ_EXECUTE, Backing::File { id: 7, offset: 0 })
//!     .unwrap();
//!
//! // `mmap` with no fixed address: top-down, so the highest gap that fits.
//! let free = space.find_free(0x2000, 0x1000, None).unwrap();
//! assert_eq!(free, 0x0000_8000_0000_0000 - 0x2000);
//!
//! assert_eq!(space.find(0x40_0000).map(|vma| vma.range), Some(text));
//! assert_eq!(space.total_mapped(), 0x4000);
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use ferrix_kmem::{Charge, buffer_footprint};

/// The page size every address and length in this crate is quantised to.
pub const PAGE_SIZE: u64 = 4096;

/// True when `address` sits on a page boundary.
const fn is_page_aligned(address: u64) -> bool {
    address & (PAGE_SIZE - 1) == 0
}

/// Rounds `address` down to a multiple of `align`, which must be a power of
/// two. Every caller has already checked that, so the mask is exact.
const fn align_down(address: u64, align: u64) -> u64 {
    address & !align.wrapping_sub(1)
}

/// Why an operation on an address space was refused.
///
/// Every variant is reachable from arguments a user program chose, which is
/// why they are errors rather than assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VmaError {
    /// The range covers no pages, or its end is below its start.
    ZeroLength,
    /// An address, a length or a backing offset was not page-aligned.
    Misaligned,
    /// `start + len` left the 64-bit address space.
    Wraps,
    /// The range is wholly or partly outside the window the map was built for.
    OutOfRange,
    /// [`AddressSpace::insert`] was given a range that is already mapped.
    Overlap,
    /// [`AddressSpace::protect`] was given a range with a hole in it. Nothing
    /// was changed: `mprotect` over unmapped pages fails as a whole.
    NotMapped,
    /// The backing object would have to extend past its own 64-bit end to
    /// cover the range, so no offset arithmetic on it could be trusted later.
    BackingOverflow,
    /// There was no memory for the map to grow, or for the list of what an
    /// unmapping removed. Nothing was changed: the room is reserved before
    /// anything is touched.
    NoMemory,
    /// The operation would change a fault window other than by taking it
    /// out whole: cover part of one, change its permissions, or make one
    /// executable or private. Nothing was changed. A window is one region
    /// for its whole life ([`Backing::Window`]).
    Window,
}

impl From<ferrix_fallible::AllocError> for VmaError {
    fn from(_: ferrix_fallible::AllocError) -> VmaError {
        VmaError::NoMemory
    }
}

impl fmt::Display for VmaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match *self {
            VmaError::ZeroLength => "the range covers no pages",
            VmaError::Misaligned => "the address, length or backing offset is not page-aligned",
            VmaError::Wraps => "the range wraps the end of the address space",
            VmaError::OutOfRange => "the range is outside the usable address window",
            VmaError::Overlap => "the range is already mapped",
            VmaError::NotMapped => "part of the range is not mapped",
            VmaError::BackingOverflow => "the backing object would extend past its own end",
            VmaError::NoMemory => "there was no memory for the map to change",
            VmaError::Window => "the range would change a fault window other than whole",
        };
        formatter.write_str(message)
    }
}

impl core::error::Error for VmaError {}

/// A half-open, page-aligned range of virtual addresses, `[start, end)`.
///
/// The fields are private and both constructors validate, so holding one of
/// these is a promise that the range is page-aligned, non-empty and does not
/// wrap. That is what lets the map's internals do arithmetic on a range
/// without re-deriving whether it is sane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageRange {
    start: u64,
    end: u64,
}

impl PageRange {
    /// Builds `[start, end)`.
    ///
    /// # Errors
    ///
    /// [`VmaError::Misaligned`] if either address is not page-aligned, and
    /// [`VmaError::ZeroLength`] if the range covers no pages.
    pub const fn new(start: u64, end: u64) -> Result<PageRange, VmaError> {
        if !is_page_aligned(start) || !is_page_aligned(end) {
            return Err(VmaError::Misaligned);
        }
        if start >= end {
            // An empty mapping has no pages to fault in, and an inverted range
            // is what a caller's own wrapped `start + len` looks like by the
            // time it arrives here.
            return Err(VmaError::ZeroLength);
        }
        Ok(PageRange { start, end })
    }

    /// Builds `[start, start + len)`, which is the shape `mmap` uses.
    ///
    /// # Errors
    ///
    /// As [`PageRange::new`], plus [`VmaError::Wraps`] when `start + len`
    /// leaves the 64-bit address space.
    pub const fn from_len(start: u64, len: u64) -> Result<PageRange, VmaError> {
        if len == 0 {
            return Err(VmaError::ZeroLength);
        }
        match start.checked_add(len) {
            None => Err(VmaError::Wraps),
            Some(end) => PageRange::new(start, end),
        }
    }

    /// First address in the range.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// One past the last address in the range.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Size of the range in bytes, always a non-zero multiple of [`PAGE_SIZE`].
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.end - self.start
    }

    /// Whether `address` falls inside the range. The end is exclusive, so the
    /// first address of the next mapping is not contained here.
    #[must_use]
    pub const fn contains(self, address: u64) -> bool {
        self.start <= address && address < self.end
    }

    /// Whether the two ranges share at least one page.
    #[must_use]
    pub const fn overlaps(self, other: PageRange) -> bool {
        self.start < other.end && other.start < self.end
    }
}

impl fmt::Display for PageRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#x}..{:#x}", self.start, self.end)
    }
}

/// Permissions and mapping kind: the part of a region that `mprotect` writes
/// and a page fault reads.
///
/// Two regions merge only if these compare equal, so anything that must keep
/// neighbouring regions apart belongs in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VmaFlags {
    /// The pages may be read.
    pub read: bool,
    /// The pages may be written.
    pub write: bool,
    /// Instructions may be fetched from the pages.
    pub execute: bool,
    /// `MAP_SHARED` rather than `MAP_PRIVATE`: writes are visible to every
    /// other mapping of the same object, so the region is never made
    /// copy-on-write at fork.
    pub shared: bool,
    /// A stack: the region may grow towards lower addresses, so a fault just
    /// below it extends the mapping rather than killing the process.
    pub grows_down: bool,
    /// `mlock`ed: the pages must stay resident and may not be reclaimed.
    pub locked: bool,
}

impl VmaFlags {
    /// No access at all: the `PROT_NONE` guard page.
    pub const NONE: VmaFlags = VmaFlags {
        read: false,
        write: false,
        execute: false,
        shared: false,
        grows_down: false,
        locked: false,
    };

    /// Read-only and private, the shape of a constant data segment.
    pub const READ: VmaFlags = VmaFlags {
        read: true,
        ..VmaFlags::NONE
    };

    /// Read/write and private, the shape of a heap or a stack.
    pub const READ_WRITE: VmaFlags = VmaFlags {
        read: true,
        write: true,
        ..VmaFlags::NONE
    };

    /// Read/execute and private, the shape of a text segment.
    pub const READ_EXECUTE: VmaFlags = VmaFlags {
        read: true,
        execute: true,
        ..VmaFlags::NONE
    };
}

/// What the pages of a region are backed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backing {
    /// Zero-filled memory with no file behind it, from `offset` bytes into the
    /// object holding it.
    ///
    /// # Why anonymous memory names an object at all
    ///
    /// It did not, until stage 6 needed it to, and the argument for the unit
    /// variant was good: zero-filled memory has no identity to disagree about,
    /// so any two anonymous regions are interchangeable. That holds right up
    /// until anonymous memory can be *shared*, and three things need it to be:
    ///
    /// * `MAP_SHARED | MAP_ANONYMOUS` between the children of a `fork`, which
    ///   musl uses, and a futex in such memory has to resolve to one wait
    ///   queue rather than to one per process;
    /// * `/proc/self/maps` and reclaim, which both want to say what a page
    ///   belongs to — `PageEntry`'s owning-object field is there for it;
    /// * `fork` itself, which is easier to reason about as "both regions name
    ///   the same object, copied per page on write" than as a page-table trick
    ///   that has to be unpicked when shared anonymous memory arrives.
    ///
    /// # The convention for private memory
    ///
    /// An `id` of zero means no named object: ordinary private anonymous
    /// memory. Its `offset` is by convention the mapping's own start address,
    /// which is what Linux's `vm_pgoff` holds for an anonymous VMA and for the
    /// same reason — it makes two adjacent private regions contiguous by
    /// construction, so they still merge, and an `mprotect` loop still does
    /// not leak regions.
    Anonymous {
        /// The object these pages belong to, or zero for private memory.
        id: u64,
        /// Byte offset into that object of the region's first page.
        offset: u64,
    },
    /// A file, from `offset` bytes in.
    File {
        /// Opaque to this crate: the kernel's inode number or VMO handle.
        id: u64,
        /// Byte offset into that object of the region's first page.
        offset: u64,
    },
    /// Device memory, never swapped and never copied: a driver's registers,
    /// or a window of memory a device shares with the guest, as a GPU's
    /// host-visible BAR is.
    Device {
        /// Physical address of the region's first page.
        physical: u64,
        /// Opaque to this crate: the kernel's name for what keeps the memory
        /// the region's, or zero for registers, which nothing has to keep.
        id: u64,
        /// Whether the pages may be mapped cacheable: memory rather than
        /// registers, as the device said it may be.
        cached: bool,
    },
    /// A fault window: a range whose pages a server puts in and takes out
    /// again, page by page, and whose faults the server answers.
    ///
    /// # One region for its whole life
    ///
    /// A window's pages are named by their offset from the window's start,
    /// and the server, the kernel's window object and every address space
    /// mapping it all agree on that. So a window region is never split,
    /// merged, moved or made copy-on-write, and the operations that would do
    /// one of those refuse with [`VmaError::Window`] instead: an unmap or a
    /// fixed mapping that covers part of it, an `mprotect` that touches it.
    /// Taken out whole, by an unmap or a fixed mapping covering all of it,
    /// it goes like any region. It is always shared, so `fork` shares it,
    /// and never executable.
    Window {
        /// Opaque to this crate: the kernel's name for the window object.
        id: u64,
    },
}

impl Backing {
    /// The backing of the part of a region that starts `bytes` in.
    ///
    /// Truncating a region's *front* has to move the offset with it; forgetting
    /// that is how unmapping the first page of a file mapping silently shifts
    /// the rest of the file by one page.
    const fn advanced_by(self, bytes: u64) -> Backing {
        match self {
            Backing::Anonymous { id, offset } => Backing::Anonymous {
                id,
                offset: offset.saturating_add(bytes),
            },
            // `saturating_add` cannot saturate here: `validate_backing`
            // refused any region whose end offset would leave the object's own
            // 64-bit space, and `bytes` never exceeds the region's length.
            Backing::File { id, offset } => Backing::File {
                id,
                offset: offset.saturating_add(bytes),
            },
            Backing::Device {
                physical,
                id,
                cached,
            } => Backing::Device {
                physical: physical.saturating_add(bytes),
                id,
                cached,
            },
            // Never reached: a window is never split, so nothing is ever cut
            // from its front. Unchanged, so the match stays total.
            Backing::Window { id } => Backing::Window { id },
        }
    }

    /// Whether this is a fault window's backing.
    #[must_use]
    pub const fn is_window(self) -> bool {
        matches!(self, Backing::Window { .. })
    }
}

/// One virtual memory area: a range of addresses with uniform permissions and
/// one backing object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vma {
    /// The addresses this region covers.
    pub range: PageRange,
    /// Permissions and mapping kind.
    pub flags: VmaFlags,
    /// What the pages are backed by, taken at the region's first page.
    pub backing: Backing,
    /// Copy-on-write: the pages may be shared read-only with another address
    /// space, and a write fault must copy a page someone else holds before
    /// letting the write through. Set by [`AddressSpace::clone_for_fork`] on
    /// both sides, and by [`AddressSpace::protect`] on a private region it
    /// makes writable, which a fork may have shared while it was read-only.
    ///
    /// This is a field of the region rather than of [`VmaFlags`] so that
    /// `mprotect`, which replaces the flags wholesale, cannot clear it and let
    /// a child write into pages its parent can still see. Set where it need
    /// not be, it costs a fault that copies nothing: a page nobody else holds
    /// is taken in place. So two regions differing only in it merge, and the
    /// merged one keeps it.
    pub cow: bool,
}

/// A range that an operation actually removed from the map.
///
/// Reported so the caller can tear down the page tables and, for a shared file
/// mapping, write the pages back. The `backing` is the backing *of the removed
/// part*, with the offset already advanced when only the tail of a region was
/// taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unmapping {
    /// The addresses that are no longer mapped.
    pub range: PageRange,
    /// The flags the removed pages had.
    pub flags: VmaFlags,
    /// What backed the removed pages, at their own offset.
    pub backing: Backing,
    /// Whether the removed pages were copy-on-write.
    pub cow: bool,
}

/// Whether two adjacent regions describe one continuous mapping and may be
/// replaced by a single region.
fn mergeable(left: &Vma, right: &Vma) -> bool {
    left.range.end == right.range.start
        && left.flags == right.flags
        && contiguous_backing(left.backing, right.backing, left.range.bytes())
}

/// Whether `right` continues `left`'s backing object exactly where `left`'s
/// `left_len` bytes end.
///
/// Every kind has to line up, or merging would make the second half of the
/// region read from the wrong part of the object. Anonymous memory included,
/// since it grew an identity -- and the convention that private memory's
/// offset is its own start address is what keeps two adjacent private regions
/// contiguous, so they merge exactly as they did when the variant carried
/// nothing.
fn contiguous_backing(left: Backing, right: Backing, left_len: u64) -> bool {
    match (left, right) {
        (
            Backing::Anonymous {
                id: left_id,
                offset: left_offset,
            },
            Backing::Anonymous {
                id: right_id,
                offset: right_offset,
            },
        ) => left_id == right_id && left_offset.checked_add(left_len) == Some(right_offset),
        (
            Backing::File {
                id: left_id,
                offset: left_offset,
            },
            Backing::File {
                id: right_id,
                offset: right_offset,
            },
        ) => left_id == right_id && left_offset.checked_add(left_len) == Some(right_offset),
        (
            Backing::Device {
                physical: left_base,
                id: left_id,
                cached: left_cached,
            },
            Backing::Device {
                physical: right_base,
                id: right_id,
                cached: right_cached,
            },
        ) => {
            left_id == right_id
                && left_cached == right_cached
                && left_base.checked_add(left_len) == Some(right_base)
        }
        _ => false,
    }
}

/// Checks a backing object against the length of the region it will cover.
fn validate_backing(backing: Backing, len: u64) -> Result<(), VmaError> {
    match backing {
        Backing::Anonymous { offset, .. } => validate_backing_base(offset, len),
        Backing::File { offset, .. } => validate_backing_base(offset, len),
        Backing::Device { physical, .. } => validate_backing_base(physical, len),
        // A window's pages are named from its start, so there is no offset
        // that could leave anything.
        Backing::Window { .. } => Ok(()),
    }
}

/// A window region is shared and never executable: what [`Backing::Window`]
/// promises, checked wherever a region is made.
fn validate_window(flags: VmaFlags, backing: Backing) -> Result<(), VmaError> {
    if backing.is_window() && (!flags.shared || flags.execute) {
        return Err(VmaError::Window);
    }
    Ok(())
}

/// A backing offset has to be page-aligned for the same reason an address
/// does, and `base + len` has to fit, because every later split advances the
/// offset and a split must not be the thing that overflows.
fn validate_backing_base(base: u64, len: u64) -> Result<(), VmaError> {
    if !is_page_aligned(base) {
        return Err(VmaError::Misaligned);
    }
    if base.checked_add(len).is_none() {
        return Err(VmaError::BackingOverflow);
    }
    Ok(())
}

/// The highest `align`-aligned address in `[gap_start, gap_end)` at which
/// `len` bytes still fit, if there is one.
fn fit_top_down(gap_start: u64, gap_end: u64, len: u64, align: u64) -> Option<u64> {
    // A gap can be empty, and the caller walks gaps built from region
    // boundaries, so both subtractions are checked rather than assumed.
    let space = gap_end.checked_sub(gap_start)?;
    if space < len {
        return None;
    }
    let top = gap_end.checked_sub(len)?;
    let candidate = align_down(top, align);
    (candidate >= gap_start).then_some(candidate)
}

/// Whether a region has to become copy-on-write when the address space forks.
///
/// Shared mappings are meant to alias, and device registers have exactly one
/// physical home, so neither can be copied; everything else private and
/// writable must be.
fn needs_cow(region: &Vma) -> bool {
    region.flags.write
        && !region.flags.shared
        && !matches!(
            region.backing,
            Backing::Device { .. } | Backing::Window { .. }
        )
}

/// Describes a region as an [`Unmapping`] of the sub-range `range`, whose
/// backing offset is advanced by however far into the region it starts.
fn unmapping_of(region: &Vma, range: PageRange) -> Unmapping {
    let into_region = range.start.saturating_sub(region.range.start);
    Unmapping {
        range,
        flags: region.flags,
        backing: region.backing.advanced_by(into_region),
        cow: region.cow,
    }
}

/// The set of regions that make up one process's address space.
///
/// # Invariant
///
/// Wherever a caller can observe it, the regions are sorted by start address,
/// non-empty, page-aligned, contained in `[low, high)`, pairwise
/// non-overlapping, and no two adjacent regions are mergeable.
/// [`AddressSpace::check_invariants`] verifies all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressSpace {
    regions: Vec<Vma>,
    low: u64,
    high: u64,
    /// The room `regions` holds, charged to the job that made the space.
    room: Room,
}

/// The heap an address space's regions hold, charged to the job of the task
/// that made the space, as it grows (certification finding F-37): a program
/// that splits one mapping into a region a page by `mprotect` or `munmap`,
/// or maps one page of a file at many addresses, pays for every region.
///
/// A copy by `Clone` is charged to nobody -- `fork` copies with
/// [`AddressSpace::clone_for_fork`], which is charged -- and two maps compare
/// equal whatever their charges.
#[derive(Debug, Default)]
struct Room(Charge);

impl Clone for Room {
    fn clone(&self) -> Room {
        Room(Charge::none())
    }
}

impl PartialEq for Room {
    fn eq(&self, _: &Room) -> bool {
        true
    }
}

impl Eq for Room {}

impl AddressSpace {
    /// Creates an empty address space usable over `[low, high)`.
    ///
    /// For a Linux-shaped x86-64 user space that is
    /// `0x1000 .. 0x0000_8000_0000_0000`: the first page is left out so that a
    /// null dereference faults, and the top is where the non-canonical hole
    /// begins.
    ///
    /// # Errors
    ///
    /// [`VmaError::Misaligned`] unless both bounds are page-aligned, and
    /// [`VmaError::ZeroLength`] if the window is empty.
    pub fn new(low: u64, high: u64) -> Result<AddressSpace, VmaError> {
        if !is_page_aligned(low) || !is_page_aligned(high) {
            return Err(VmaError::Misaligned);
        }
        if low >= high {
            return Err(VmaError::ZeroLength);
        }
        Ok(AddressSpace {
            regions: Vec::new(),
            low,
            high,
            room: Room(Charge::bytes(0).map_err(|_| VmaError::NoMemory)?),
        })
    }

    /// The heap its regions hold and are charged for, in bytes.
    #[must_use]
    pub fn charged(&self) -> u64 {
        self.room.0.charged()
    }

    /// Room for `additional` more regions, charged first: what the vector
    /// will grow to, which is twice what it has or what is needed if more.
    ///
    /// # Errors
    ///
    /// [`VmaError::NoMemory`], with nothing changed, when the job is at its
    /// limit or the heap is empty.
    fn grow(&mut self, additional: usize) -> Result<(), VmaError> {
        let (len, room) = (self.regions.len(), self.regions.capacity());
        let needed = len.checked_add(additional).ok_or(VmaError::NoMemory)?;
        if needed <= room {
            return Ok(());
        }
        let target = needed.max(room.saturating_mul(2)).max(4);
        self.room
            .0
            .resize(buffer_footprint::<Vma>(target))
            .map_err(|_| VmaError::NoMemory)?;
        if let Err(error) = ferrix_fallible::try_reserve(&mut self.regions, additional) {
            let _ = self.room.0.resize(buffer_footprint::<Vma>(room));
            return Err(error.into());
        }
        let _ = self
            .room
            .0
            .resize(buffer_footprint::<Vma>(self.regions.capacity()));
        Ok(())
    }

    /// Lowest usable address.
    #[must_use]
    pub const fn low(&self) -> u64 {
        self.low
    }

    /// One past the highest usable address.
    #[must_use]
    pub const fn high(&self) -> u64 {
        self.high
    }

    /// Move the top of the usable window to `high`, down or back up: what
    /// `execve` does when the program it loads has a smaller address space
    /// than the one before it, as a 32-bit program on a 64-bit kernel has, or
    /// a larger one.
    ///
    /// # Errors
    ///
    /// [`VmaError::Misaligned`] unless `high` is page-aligned,
    /// [`VmaError::ZeroLength`] if the window would be empty, and
    /// [`VmaError::OutOfRange`] if a region already mapped would be left
    /// outside it. Nothing is changed on an error.
    pub fn set_high(&mut self, high: u64) -> Result<(), VmaError> {
        if !is_page_aligned(high) {
            return Err(VmaError::Misaligned);
        }
        if self.low >= high {
            return Err(VmaError::ZeroLength);
        }
        if self
            .regions
            .last()
            .is_some_and(|region| region.range.end > high)
        {
            return Err(VmaError::OutOfRange);
        }
        self.high = high;
        Ok(())
    }

    /// Number of regions in the map.
    ///
    /// This is the number that a `mprotect` cycle would grow without bound if
    /// merging were wrong, so it is worth watching in tests and in a process's
    /// own accounting.
    #[must_use]
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Whether nothing at all is mapped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Total number of bytes mapped, across all regions.
    #[must_use]
    pub fn total_mapped(&self) -> u64 {
        // The sum cannot exceed `high - low`, so the saturation is unreachable
        // and is here only to keep the fold total.
        self.regions.iter().fold(0, |total, region| {
            total.saturating_add(region.range.bytes())
        })
    }

    /// The regions, in ascending address order.
    pub fn iter(&self) -> core::slice::Iter<'_, Vma> {
        self.regions.iter()
    }

    /// The region containing `address`, which is what a page fault needs.
    #[must_use]
    pub fn find(&self, address: u64) -> Option<&Vma> {
        let index = self.first_touching(address);
        self.regions
            .get(index)
            .filter(|region| region.range.contains(address))
    }

    /// Adds a region, failing if any of it is already mapped.
    ///
    /// This is `mmap` once a free range has been chosen, and the overlap check
    /// is what makes a stale [`AddressSpace::find_free`] result a refusal
    /// rather than two regions claiming the same pages.
    ///
    /// # Errors
    ///
    /// [`VmaError::OutOfRange`] outside the window, [`VmaError::Overlap`] if
    /// anything is mapped there, [`VmaError::Misaligned`] or
    /// [`VmaError::BackingOverflow`] for a backing offset that cannot cover
    /// the range, and [`VmaError::Window`] for a window region that is private
    /// or executable. On any of them the map is unchanged.
    pub fn insert(
        &mut self,
        range: PageRange,
        flags: VmaFlags,
        backing: Backing,
    ) -> Result<(), VmaError> {
        self.insert_region(Vma {
            range,
            flags,
            backing,
            cow: false,
        })
    }

    /// Maps a region exactly as described, its copy-on-write marking
    /// included, into a range that must be free. This is `mremap` putting a
    /// region back down somewhere else.
    ///
    /// [`AddressSpace::insert`] cannot do it, because a fresh mapping is never
    /// copy-on-write and it says so. A region that *moves* can be: the pages
    /// it names may still be shared with a `fork` child, and a moved region
    /// that lost the marking would let the next write land in a page the child
    /// can read.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::insert`], and [`VmaError::Window`] for a window
    /// region marked copy-on-write. On any of them the map is unchanged.
    pub fn insert_region(&mut self, region: Vma) -> Result<(), VmaError> {
        self.check_range(region.range)?;
        validate_backing(region.backing, region.range.bytes())?;
        validate_window(region.flags, region.backing)?;
        if region.backing.is_window() && region.cow {
            return Err(VmaError::Window);
        }
        if self.overlaps(region.range) {
            return Err(VmaError::Overlap);
        }
        self.grow(1)?;
        self.place(region);
        Ok(())
    }

    /// Maps `range`, replacing whatever was there. This is `MAP_FIXED`.
    ///
    /// Regions straddling either edge are split, regions wholly inside are
    /// dropped, and the returned [`Unmapping`]s name, in ascending order,
    /// exactly the ranges that stopped being mapped, so the caller can tear
    /// down the page tables for them.
    ///
    /// # Errors
    ///
    /// [`VmaError::OutOfRange`] outside the window,
    /// [`VmaError::Misaligned`] or [`VmaError::BackingOverflow`] for an
    /// unusable backing offset, and [`VmaError::Window`] if the range covers
    /// part of a window region, or the new region is a private or executable
    /// window. In any case nothing has been changed.
    pub fn map_fixed(
        &mut self,
        range: PageRange,
        flags: VmaFlags,
        backing: Backing,
    ) -> Result<Vec<Unmapping>, VmaError> {
        self.check_range(range)?;
        validate_backing(backing, range.bytes())?;
        validate_window(flags, backing)?;
        self.refuse_partial_window(range)?;
        // A carve splits at most one region, and the new one goes in after.
        let mut unmapped = self.room_to_carve(range, 2)?;
        self.carve(range, &mut |removed| {
            let _ = ferrix_fallible::push_within(&mut unmapped, removed);
        });
        self.place(Vma {
            range,
            flags,
            backing,
            cow: false,
        });
        Ok(unmapped)
    }

    /// Unmaps `range`. This is `munmap`.
    ///
    /// Unmapping a range that is only partly mapped is not an error, exactly as
    /// it is not in Linux: the holes are skipped. The returned [`Unmapping`]s
    /// name, in ascending order, the ranges that were mapped and no longer are.
    ///
    /// # Errors
    ///
    /// [`VmaError::OutOfRange`] if the range leaves the window, and
    /// [`VmaError::Window`] if it covers part of a window region; in either
    /// case nothing has been changed.
    pub fn remove(&mut self, range: PageRange) -> Result<Vec<Unmapping>, VmaError> {
        self.check_range(range)?;
        self.refuse_partial_window(range)?;
        let mut unmapped = self.room_to_carve(range, 1)?;
        self.carve(range, &mut |removed| {
            let _ = ferrix_fallible::push_within(&mut unmapped, removed);
        });
        Ok(unmapped)
    }

    /// Unmaps `range`, as [`AddressSpace::remove`] does, without reporting
    /// what went: for a caller that already knows, such as an arena giving a
    /// span back.
    ///
    /// Needs memory only to split a region, and none at all once
    /// [`AddressSpace::reserve`] has made room for one more region.
    ///
    /// # Errors
    ///
    /// [`VmaError::OutOfRange`], [`VmaError::Window`] as for
    /// [`AddressSpace::remove`], or [`VmaError::NoMemory`] when a split found
    /// no room; in any case nothing has been changed.
    pub fn remove_quietly(&mut self, range: PageRange) -> Result<(), VmaError> {
        self.check_range(range)?;
        self.refuse_partial_window(range)?;
        // Only a range strictly inside one region splits it, and only that
        // needs room. The standard library's own reserve rather than the
        // injecting one: with the room there it allocates nothing, and a test
        // that injects failures must not be told otherwise.
        let splits = self
            .regions
            .get(self.first_touching(range.start))
            .is_some_and(|region| region.range.start < range.start && region.range.end > range.end);
        if splits && self.regions.len() == self.regions.capacity() {
            let room = self.regions.capacity();
            let target = room.saturating_mul(2).max(4);
            self.room
                .0
                .resize(buffer_footprint::<Vma>(target))
                .map_err(|_| VmaError::NoMemory)?;
            if self.regions.try_reserve(1).is_err() {
                let _ = self.room.0.resize(buffer_footprint::<Vma>(room));
                return Err(VmaError::NoMemory);
            }
            let _ = self
                .room
                .0
                .resize(buffer_footprint::<Vma>(self.regions.capacity()));
        }
        self.carve(range, &mut |_| {});
        Ok(())
    }

    /// Make room for `regions` more regions than the map holds now, so that
    /// changes needing no more than that cannot fail for memory.
    ///
    /// # Errors
    ///
    /// [`VmaError::NoMemory`].
    pub fn reserve(&mut self, regions: usize) -> Result<(), VmaError> {
        self.grow(regions)
    }

    /// Room for carving `range`: a list with space for everything it could
    /// report, and `regions` more slots in the map. Reserved before anything
    /// is touched, so a failure changes nothing.
    fn room_to_carve(
        &mut self,
        range: PageRange,
        regions: usize,
    ) -> Result<Vec<Unmapping>, VmaError> {
        let touched = self
            .first_beyond(range.end)
            .saturating_sub(self.first_touching(range.start));
        let unmapped = ferrix_fallible::try_with_capacity(touched.saturating_add(1))?;
        self.grow(regions)?;
        Ok(unmapped)
    }

    /// Changes the permissions of every page in `range`. This is `mprotect`.
    ///
    /// Regions straddling either edge are split so that the new flags apply to
    /// exactly `range`, and the result is merged with its neighbours
    /// afterwards, so restoring the previous flags restores the previous region
    /// count as well.
    ///
    /// A region's `cow` marking is deliberately untouched: a child that
    /// `mprotect`s its inherited heap must still copy on write.
    ///
    /// # Errors
    ///
    /// [`VmaError::OutOfRange`] outside the window, [`VmaError::NotMapped`]
    /// if any page of the range is unmapped, and [`VmaError::Window`] if any
    /// page of it is a window's. As in Linux the call is all or nothing: on
    /// any error the map is unchanged.
    pub fn protect(&mut self, range: PageRange, flags: VmaFlags) -> Result<(), VmaError> {
        self.check_range(range)?;
        if !self.is_fully_mapped(range) {
            return Err(VmaError::NotMapped);
        }
        if self.touches_window(range) {
            return Err(VmaError::Window);
        }
        // Each edge may split one region.
        self.grow(2)?;
        self.split_at(range.start);
        self.split_at(range.end);
        let first = self.first_touching(range.start);
        let last = self.first_beyond(range.end);
        for region in self.regions.get_mut(first..last).into_iter().flatten() {
            region.flags = flags;
            // A private region made writable may hold frames a fork left in
            // another space: read-only at the fork, it was not marked then,
            // and its first write would land in the frame the other space
            // still reads. Marked now, that write copies a frame someone else
            // holds and takes one nobody else does in place, as a region
            // writable at the fork does. Chromium on AArch64 met it: glibc's
            // stack guard lives in ld.so's RELRO page there, and a child's
            // `mprotect` of that page wrote its guard into its parent's.
            if needs_cow(region) {
                region.cow = true;
            }
        }
        self.merge_span(first, last);
        Ok(())
    }

    /// Finds a free range of `len` bytes at an `align`-aligned address.
    ///
    /// Placement is top-down, which is the layout Linux has used since the
    /// mmap-topdown change: a new mapping goes just below the lowest existing
    /// one, so mappings grow down from the top of the window and stay out of
    /// the way of a heap growing up from the bottom for as long as possible.
    ///
    /// `hint` is advisory, as `mmap`'s address argument is. It is rounded down
    /// to `align` and used when the result is free and inside the window;
    /// otherwise the top-down search decides, rather than the call failing.
    ///
    /// Returns `None` when nothing fits, and also for a `len` that is zero or
    /// unaligned or an `align` that is not a power of two of at least
    /// [`PAGE_SIZE`], since neither can describe a mapping at all.
    ///
    /// A range it does return lies wholly inside the window, `low..high`: a
    /// hint is taken only past the window check, and the search only fits
    /// gaps between `low` and `high`. The kernel's mapping paths rely on this
    /// and keep their own check of the user half as a second line.
    #[must_use]
    pub fn find_free(&self, len: u64, align: u64, hint: Option<u64>) -> Option<u64> {
        if len == 0 || !is_page_aligned(len) || !align.is_power_of_two() || align < PAGE_SIZE {
            return None;
        }
        if let Some(address) = hint.and_then(|address| self.hint_fits(address, len, align)) {
            return Some(address);
        }
        let mut gap_end = self.high;
        for region in self.regions.iter().rev() {
            if let Some(address) = fit_top_down(region.range.end, gap_end, len, align) {
                return Some(address);
            }
            gap_end = region.range.start;
        }
        fit_top_down(self.low, gap_end, len, align)
    }

    /// Deep-copies the address space for `fork`, marking what must be copied on
    /// write.
    ///
    /// A private writable region is marked copy-on-write in *both* address
    /// spaces, because the parent's own writes must stop reaching pages the
    /// child can see. Shared regions are left alone -- that is what
    /// `MAP_SHARED` means -- and so are device registers, which have one
    /// physical home and cannot be copied at all.
    ///
    /// Marking can make two neighbours that differed only in `cow` identical,
    /// so the map is re-merged before it is copied and both sides come back
    /// canonical.
    ///
    /// # Errors
    ///
    /// [`VmaError::NoMemory`], with nothing marked: the copy's room is taken
    /// first.
    pub fn clone_for_fork(&mut self) -> Result<AddressSpace, VmaError> {
        let len = self.regions.len();
        let mut room =
            Charge::bytes(buffer_footprint::<Vma>(len)).map_err(|_| VmaError::NoMemory)?;
        let mut regions = ferrix_fallible::try_with_capacity(len)?;
        let _ = room.resize(buffer_footprint::<Vma>(regions.capacity()));
        for region in &mut self.regions {
            if needs_cow(region) {
                region.cow = true;
            }
        }
        self.merge_all();
        // Merging only shrank the map, so the copy fits the room taken above.
        regions.extend_from_slice(&self.regions);
        Ok(AddressSpace {
            regions,
            low: self.low,
            high: self.high,
            room: Room(room),
        })
    }

    /// Verifies every part of the structural invariant.
    ///
    /// Cheap enough to call after each operation in a test, which is where it
    /// earns its keep: a splitting bug shows up here at once instead of as a
    /// lost region three operations later.
    ///
    /// # Errors
    ///
    /// A short description of the first violation found.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        let mut previous: Option<&Vma> = None;
        for region in &self.regions {
            self.check_one(region)?;
            if let Some(before) = previous {
                if before.range.end > region.range.start {
                    return Err("regions overlap or are out of order");
                }
                if mergeable(before, region) {
                    return Err("adjacent regions were left unmerged");
                }
            }
            previous = Some(region);
        }
        Ok(())
    }

    /// The part of the invariant that concerns one region on its own.
    fn check_one(&self, region: &Vma) -> Result<(), &'static str> {
        if region.range.start >= region.range.end {
            return Err("region is empty or inverted");
        }
        if !is_page_aligned(region.range.start) || !is_page_aligned(region.range.end) {
            return Err("region is not page-aligned");
        }
        if region.range.start < self.low || region.range.end > self.high {
            return Err("region is outside the usable address window");
        }
        if validate_backing(region.backing, region.range.bytes()).is_err() {
            return Err("region backing cannot cover the region");
        }
        if validate_window(region.flags, region.backing).is_err()
            || (region.backing.is_window() && region.cow)
        {
            return Err("window region is private, executable or copy-on-write");
        }
        Ok(())
    }

    // -- Lookups ------------------------------------------------------------

    /// Index of the first region that ends after `address`: the first one that
    /// could contain it, or else the first one after it.
    fn first_touching(&self, address: u64) -> usize {
        self.regions
            .partition_point(|region| region.range.end <= address)
    }

    /// Index one past the last region that starts before `address`.
    fn first_beyond(&self, address: u64) -> usize {
        self.regions
            .partition_point(|region| region.range.start < address)
    }

    /// Whether any region shares a page with `range`.
    fn overlaps(&self, range: PageRange) -> bool {
        let index = self.first_touching(range.start);
        self.regions
            .get(index)
            .is_some_and(|region| region.range.start < range.end)
    }

    /// Whether any window region shares a page with `range`.
    fn touches_window(&self, range: PageRange) -> bool {
        let first = self.first_touching(range.start);
        let last = self.first_beyond(range.end);
        self.regions
            .get(first..last)
            .is_some_and(|span| span.iter().any(|region| region.backing.is_window()))
    }

    /// Whether taking `range` out of the map would cut a window region
    /// rather than take it out whole: what [`AddressSpace::remove`] and
    /// [`AddressSpace::map_fixed`] refuse, for a caller that must know before
    /// it changes anything else -- `mremap` with a fixed destination.
    #[must_use]
    pub fn cuts_window(&self, range: PageRange) -> bool {
        self.refuse_partial_window(range).is_err()
    }

    /// Refuses a carve of `range` that would cut a window region rather than
    /// take it out whole: one that starts inside it or ends inside it.
    fn refuse_partial_window(&self, range: PageRange) -> Result<(), VmaError> {
        let first = self.first_touching(range.start);
        let last = self.first_beyond(range.end);
        let cut = self.regions.get(first..last).is_some_and(|span| {
            span.iter().any(|region| {
                region.backing.is_window()
                    && (region.range.start < range.start || region.range.end > range.end)
            })
        });
        if cut {
            return Err(VmaError::Window);
        }
        Ok(())
    }

    /// Whether every page of `range` is mapped, with no hole anywhere in it.
    fn is_fully_mapped(&self, range: PageRange) -> bool {
        let first = self.first_touching(range.start);
        let mut covered = range.start;
        for region in self.regions.iter().skip(first) {
            if region.range.start > covered {
                return false;
            }
            covered = region.range.end;
            if covered >= range.end {
                return true;
            }
        }
        false
    }

    /// Rejects a range outside the usable window. Alignment, emptiness and
    /// wrapping were settled by [`PageRange`] before the range got here.
    fn check_range(&self, range: PageRange) -> Result<(), VmaError> {
        if range.start < self.low || range.end > self.high {
            return Err(VmaError::OutOfRange);
        }
        Ok(())
    }

    /// The hinted address, rounded down to `align`, if a mapping of `len`
    /// bytes would fit there.
    fn hint_fits(&self, hint: u64, len: u64, align: u64) -> Option<u64> {
        let start = align_down(hint, align);
        let range = PageRange::from_len(start, len).ok()?;
        self.check_range(range).ok()?;
        if self.overlaps(range) {
            return None;
        }
        Some(start)
    }

    // -- Mutation primitives ------------------------------------------------

    /// Inserts a region into a range known to be free, then merges it with
    /// whichever neighbours it now continues. The caller has reserved room
    /// for it.
    fn place(&mut self, region: Vma) {
        let index = self.first_touching(region.range.start);
        let index = index.min(self.regions.len());
        self.regions.insert(index, region);
        self.merge_span(index, index.saturating_add(1));
    }

    /// Cuts the region containing `address` in two at that address, so that a
    /// later operation can treat the halves separately.
    ///
    /// Does nothing when `address` is already a boundary or is not mapped,
    /// which is what makes it safe to call blindly on both edges of a range.
    fn split_at(&mut self, address: u64) {
        let index = self.first_touching(address);
        let Some(region) = self.regions.get_mut(index) else {
            return;
        };
        if region.range.start >= address || region.range.end <= address {
            return;
        }
        let head_len = address.saturating_sub(region.range.start);
        let tail = Vma {
            range: PageRange {
                start: address,
                end: region.range.end,
            },
            flags: region.flags,
            backing: region.backing.advanced_by(head_len),
            cow: region.cow,
        };
        region.range.end = address;
        // `index` is a valid element index, so `index + 1` is at most the
        // length and the insertion point exists.
        self.regions.insert(index.saturating_add(1), tail);
    }

    /// Merges the regions at `index` and `index + 1` if they describe one
    /// continuous mapping. Returns whether it merged them.
    fn try_merge_at(&mut self, index: usize) -> bool {
        let Some(next) = index.checked_add(1) else {
            return false;
        };
        let (Some(left), Some(right)) = (self.regions.get(index), self.regions.get(next)) else {
            return false;
        };
        if !mergeable(left, right) {
            return false;
        }
        let absorbed = self.regions.remove(next);
        if let Some(left) = self.regions.get_mut(index) {
            left.range.end = absorbed.range.end;
            left.cow |= absorbed.cow;
        }
        true
    }

    /// Restores the "no two adjacent regions are mergeable" invariant across a
    /// span that was just rewritten, including the joins with the untouched
    /// regions on either side.
    fn merge_span(&mut self, first: usize, last: usize) {
        let mut index = first.saturating_sub(1);
        let mut end = last.min(self.regions.len());
        while index < end {
            if self.try_merge_at(index) {
                // The vector shrank at `index`, so the same index now names the
                // merged region and its new neighbour, which may also join.
                end = end.saturating_sub(1);
            } else {
                index = index.saturating_add(1);
            }
        }
    }

    /// Merges every mergeable neighbour in the whole map.
    ///
    /// Only [`AddressSpace::clone_for_fork`] needs this, because marking
    /// copy-on-write can make pairs mergeable anywhere at once.
    fn merge_all(&mut self) {
        let mut index: usize = 0;
        while index.saturating_add(1) < self.regions.len() {
            if !self.try_merge_at(index) {
                index = index.saturating_add(1);
            }
        }
    }

    // -- Carving ------------------------------------------------------------

    /// Removes every page of `range` from the map, appending what was actually
    /// removed to `out` in ascending order.
    ///
    /// This is the whole of `munmap` and the first half of `MAP_FIXED`. It
    /// never leaves two mergeable neighbours behind, because everything it
    /// removes leaves a hole between what is left.
    fn carve(&mut self, range: PageRange, out: &mut dyn FnMut(Unmapping)) {
        let first = self.first_touching(range.start);
        let last = self.first_beyond(range.end);
        if first >= last {
            return;
        }
        if self.carve_interior(first, range, out) {
            return;
        }
        let first = self.carve_head(first, range, out);
        let (last, tail) = self.carve_tail(last, range);
        self.carve_whole(first, last, out);
        if let Some(tail) = tail {
            out(tail);
        }
    }

    /// Handles the one shape that turns a region into *two*: `range` strictly
    /// inside a single region, which leaves a piece on either side.
    ///
    /// Returns whether that was the shape, in which case there is nothing else
    /// to do, since a region reaching past both edges is necessarily the only
    /// one the range touches.
    fn carve_interior(
        &mut self,
        index: usize,
        range: PageRange,
        out: &mut dyn FnMut(Unmapping),
    ) -> bool {
        let Some(region) = self.regions.get_mut(index) else {
            return false;
        };
        if region.range.start >= range.start || region.range.end <= range.end {
            return false;
        }
        let start = region.range.start;
        let tail = Vma {
            range: PageRange {
                start: range.end,
                end: region.range.end,
            },
            flags: region.flags,
            backing: region.backing.advanced_by(range.end.saturating_sub(start)),
            cow: region.cow,
        };
        let removed = unmapping_of(region, range);
        region.range.end = range.start;
        self.regions.insert(index.saturating_add(1), tail);
        out(removed);
        true
    }

    /// Truncates the region at `index` if it starts before `range`, so that the
    /// remainder ends where `range` begins.
    ///
    /// Returns the index of the first region that is now wholly inside `range`.
    fn carve_head(
        &mut self,
        index: usize,
        range: PageRange,
        out: &mut dyn FnMut(Unmapping),
    ) -> usize {
        let Some(region) = self.regions.get_mut(index) else {
            return index;
        };
        if region.range.start >= range.start {
            return index;
        }
        // The region ends inside `range`, because the case where it reaches
        // past the far edge was already taken by `carve_interior`.
        let cut = PageRange {
            start: range.start,
            end: region.range.end,
        };
        let removed = unmapping_of(region, cut);
        region.range.end = range.start;
        out(removed);
        index.saturating_add(1)
    }

    /// Truncates the region below `last` if it ends after `range`, so that the
    /// remainder starts where `range` ends.
    ///
    /// Returns the index one past the last region now wholly inside `range`,
    /// and the piece that was cut away, which the caller appends after the
    /// whole regions to keep the report in ascending order.
    fn carve_tail(&mut self, last: usize, range: PageRange) -> (usize, Option<Unmapping>) {
        let Some(index) = last.checked_sub(1) else {
            return (last, None);
        };
        let Some(region) = self.regions.get_mut(index) else {
            return (last, None);
        };
        if region.range.end <= range.end {
            return (last, None);
        }
        // The region starts inside `range`: a region starting below it and
        // ending above it is the `carve_interior` shape, and a head that was
        // just truncated now ends at `range.start` and was caught above.
        let cut = PageRange {
            start: region.range.start,
            end: range.end,
        };
        let removed = unmapping_of(region, cut);
        region.backing = region.backing.advanced_by(cut.bytes());
        region.range.start = range.end;
        (index, Some(removed))
    }

    /// Deletes the regions in `first..last`, which are wholly inside the range
    /// being carved, and reports each of them.
    fn carve_whole(&mut self, first: usize, last: usize, out: &mut dyn FnMut(Unmapping)) {
        if first >= last || last > self.regions.len() {
            return;
        }
        for region in self.regions.drain(first..last) {
            out(Unmapping {
                range: region.range,
                flags: region.flags,
                backing: region.backing,
                cow: region.cow,
            });
        }
    }
}

impl<'a> IntoIterator for &'a AddressSpace {
    type Item = &'a Vma;
    type IntoIter = core::slice::Iter<'a, Vma>;

    fn into_iter(self) -> Self::IntoIter {
        self.regions.iter()
    }
}

#[cfg(test)]
mod tests;
