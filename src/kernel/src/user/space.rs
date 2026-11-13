//! A process's address space: page tables, and the map that says what is in
//! them.
//!
//! `docs/ARCHITECTURE.md` §3 names this as one of the nine kernel objects and
//! §4 says what it is made of — "a red-black interval tree of `Vma`s, each
//! naming a VMO, an offset, a protection and a share mode". The tree is
//! `src/lib/kernel/vma`, host-tested for four stages as the vmap arena's allocator and
//! used here for the first time as what it was written for.
//!
//! # Why this has a lock and the kernel's tables do not need one of their own
//!
//! [`crate::mm::map_in`] documents itself as taking no lock, because the tree
//! it was written for — a processor's identity map during bring-up — is one
//! nobody has installed and therefore nobody can walk. A user address space is
//! the opposite: it is installed on whichever processors are running its
//! threads, and their `MMU`s walk it in hardware while another processor maps
//! into it. So each address space carries a lock of its own, held across the
//! pair of operations that must not be split — reshaping the map and changing
//! the tables to match — because a fault that arrives between them would find
//! a region the tables do not have or a mapping the map does not know about.
//!
//! The lock is per address space rather than global: two processes faulting at
//! once contend for nothing, which is the property a compiler workload needs.
//!
//! # Which processors may still hold this space's translations
//!
//! Every space keeps a set of processors, and the set means *may still have
//! this space's translations in its TLB* — not *has the root loaded now*. The
//! difference is the whole of what makes a shootdown to the set enough:
//!
//! * A processor **joins before** [`AddressSpace::install`] loads the root, so
//!   no processor walks these tables and caches an entry while outside the
//!   set.
//! * It **leaves only after** the root write that took its translations out
//!   of its TLB, or out of use: [`AddressSpace::uninstall`]'s, or the
//!   `install` of the next space. On x86-64 that is the `CR3` write — without
//!   `PCID` it drops every entry not marked global, and no user mapping is
//!   ever global: every `map_in` in this file passes `global: false`. With
//!   `PCID` (`docs/PCID.md`) the write may keep them, tagged with a PCID that
//!   is no longer current and so used for nothing; before that PCID is
//!   current for this space again, `install` flushes it if a shootdown of
//!   the space has begun since it was last flushed. That rests on two
//!   sequentially consistent orders: a shootdown counts itself (`begun`)
//!   before it reads the set, and `install` joins the set before it reads
//!   the count. Either the set read sees the join, and the processor is
//!   interrupted, or the count read sees the shootdown, and the slot is
//!   flushed. On `AArch64` and ARMv7-A a
//!   write to `TTBR0` drops nothing and `EPD0` stops walks rather than TLB
//!   hits, so both `install_user_root` and `uninstall_user_root` invalidate
//!   `ASID` zero, which every user translation carries and no kernel one does.
//! * There is no lazy TLB. The scheduler uninstalls a space when it switches
//!   to a kernel thread rather than leaving the root loaded, so a processor
//!   running a kernel thread is in no space's set.
//!
//! # Taking a translation down
//!
//! Whoever takes a translation out of these tables — `munmap`, `mprotect`,
//! `fork`'s write-protection, `mremap`, `madvise`, a copy-on-write fault, and a
//! VMO taking a page away through [`AddressSpace::forget_pages`] — does it in
//! one order:
//!
//! 1. under the lock, the entries come out of the tables;
//! 2. still under the lock, and only after that, the set is read and the
//!    shootdown counted pending;
//! 3. with the lock let go, [`crate::smp::flush_tlb_pages`] reaches every
//!    processor in the set and waits for each to answer;
//! 4. only then is anything the translations reached given back.
//!
//! "Anything" includes the page tables step 1 emptied. A processor caches the
//! walk as well as the leaf, and until step 3 reaches it may walk through a
//! table whose descriptor is already out of memory; freed in step 1, the
//! table could be reused and filled by anybody before that walk. So
//! [`crate::mm::unmap_in`] unlinks them onto the shootdown's own
//! [`TlbPages`], and [`crate::smp::flush_tlb_pages`] gives them back after
//! its last answer (finding F-36). A table used to go back in step 1.
//!
//! A processor that installs the space after step 2 walks tables the entries
//! are already out of. And [`AddressSpace::with_page`] relies on the same
//! order from the other side: it translates under this lock and copies before
//! letting it go, which is safe because nothing a translation found under the
//! lock reaches is released until a shootdown that began after the
//! translation came down has returned.
//!
//! The pending count is for the one gap that order leaves. Between steps 2
//! and 3 an entry is out of the tables and may still be in a TLB, and a VMO
//! asking [`AddressSpace::forget_pages`] for pages in that state would find
//! nothing to take down and release its frame at once. So while any shootdown
//! of this space is pending, `forget_pages` asks for a whole flush of the set
//! rather than trusting the tables — Linux's `mm_tlb_flush_pending`, for the
//! same race.
//!
//! # Lock order
//!
//! This space's lock, then a VMO's mapper list, then the VMO's pages. No VMO
//! lock is ever held while a space's lock is taken, no two spaces' locks are
//! ever held together, and no shootdown waits under any of them. Two VMOs'
//! pages locks are held together only as a shadow's pages lock, then its
//! file's (`Vmo::with_frames_nested`, the kept resident counts,
//! `docs/RSS-COUNTS.md` §4): never a file's then a shadow's, never two
//! otherwise; check builds stop on any other.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::sync::SpinLock;
use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END, is_user_address};
use ferrix_frame::Frame;
use ferrix_native_abi::restricted::SHARED_BASE;
use ferrix_paging::MapFlags;
use ferrix_sched::CpuSet;
use ferrix_sync::{Once, SleepLock, SleepLockGuard};
use ferrix_vma::{Backing, PageRange, Unmapping, Vma, VmaFlags};

use crate::arch;
use crate::fallible;
use crate::mm;
use crate::object::pager::{Command, FaultMode, Refusal};
use crate::smp::{self, CpuMask, TlbPages};
use crate::user::memory_type::{self, MemoryType};
use crate::user::rss::{Kind, Rss, ViewKey};
use crate::user::vmo::{
    Commit, Kept, Own, PageState, Retired, ShadowCopies, Sharing, Vmo, VmoError,
};

/// The lowest address a program may map anything at: 64 KiB, the
/// `vm.mmap_min_addr` Linux distributions ship.
///
/// A null pointer the kernel follows must fault, not read memory a program
/// chose to put there, and with no SMAP or PAN a kernel dereference of a low
/// user address reads whatever is mapped at it. So the bottom of the user half
/// is kept out of every map, not merely left free: `MAP_FIXED` cannot place a
/// page there and a hint cannot round down into it. Nothing Linux would load is
/// refused for it — a static ARM binary is linked at exactly this address, and
/// the other two architectures link theirs higher.
pub const MMAP_MIN_ADDR: u64 = 0x1_0000;

/// Why an address space operation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpaceError {
    /// No frame was available for a page table or a root.
    OutOfMemory,
    /// The range is not in the half of the address space user code gets.
    NotUserRange(u64),
    /// The range overlaps something already mapped, or is malformed.
    BadRange,
    /// The faulting address is in no region: the process gets a `SIGSEGV`.
    NotMapped(u64),
    /// The access is not one the region's permissions allow.
    Refused(u64),
    /// The backing object could not produce the page.
    Backing(VmoError),
    /// The address is in a file mapping, on a page wholly past the end of the
    /// file: the process gets a `SIGBUS`, as on Linux.
    PastEnd(u64),
    /// The address is in a mapping of a file on a disk, and the page could
    /// not be read from it: a `SIGBUS` too, as Linux gives for a mapped page
    /// whose read fails.
    Unreadable(u64),
    /// A page of the device memory asked for is mapped, by this or another
    /// address space, with another memory type (`user::memory_type`).
    OtherMemoryType,
    /// An executable mapping of something with no measurement in a measured
    /// space (`docs/microkernel/K14.md` §3).
    Unmeasured,
    /// The space's record of images is full ([`MAX_IMAGES`]).
    ImagesFull,
    /// The page of a file on a disk that a fault was filling was reclaimed
    /// before the fault could use it (`user::cache`): fill it again. Only
    /// [`AddressSpace::fault`] sees this, and it never gives it to a caller.
    Evicted,
}

impl fmt::Display for SpaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpaceError::OutOfMemory => f.write_str("no frame available"),
            SpaceError::NotUserRange(at) => write!(f, "{at:#x} is not a user address"),
            SpaceError::BadRange => f.write_str("the range is malformed or already mapped"),
            SpaceError::NotMapped(at) => write!(f, "nothing is mapped at {at:#x}"),
            SpaceError::Refused(at) => write!(f, "the access at {at:#x} is not permitted"),
            SpaceError::Backing(why) => write!(f, "the backing object refused: {why}"),
            SpaceError::PastEnd(at) => write!(f, "{at:#x} is past the end of the mapped file"),
            SpaceError::Unreadable(at) => {
                write!(f, "the mapped file's page at {at:#x} is unreadable")
            }
            SpaceError::OtherMemoryType => {
                f.write_str("the device memory is mapped with another memory type")
            }
            SpaceError::Evicted => f.write_str("a page was reclaimed as it was filled"),
            SpaceError::Unmeasured => {
                f.write_str("the executable mapping has no measurement in a measured space")
            }
            SpaceError::ImagesFull => f.write_str("the space's record of images is full"),
        }
    }
}

/// What a fault was trying to do, which decides whether it is allowed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Access {
    /// The access wrote.
    pub write: bool,
    /// The access fetched an instruction.
    pub execute: bool,
}

impl Access {
    /// A read.
    pub const READ: Access = Access {
        write: false,
        execute: false,
    };
    /// A write.
    pub const WRITE: Access = Access {
        write: true,
        execute: false,
    };
}

/// The parts that the lock protects. The map, the objects and the shadows
/// are read by the kept counts' recount too (`user::rss_check`).
#[derive(Debug)]
pub(super) struct Inner {
    /// What is mapped where.
    pub(super) map: ferrix_vma::AddressSpace,
    /// The objects those regions name, by the id the region carries. Every id
    /// in here is attached to its object, and detached as it leaves.
    pub(super) objects: BTreeMap<u64, Arc<Vmo>>,
    /// For an id a file mapping names, the open file it was mapped through:
    /// what keeps the file alive for as long as the mapping is, as Linux's
    /// `vm_file` does, and what `/proc/<pid>/maps` names the region by. It
    /// leaves with its id's object, and `give_back` drops it only after the
    /// lock. A space's last reference, though, can go in the reaper's
    /// preemption window or in a retirement's drop of the spaces it forgot, and
    /// the space drops its files there. So an open file's `Drop`, and its
    /// inode's, must take no lock that can sleep and ask for no shootdown.
    files: BTreeMap<u64, FileMapping>,
    /// For an id a private file mapping names, the object its writes go to:
    /// anonymous, attached privately, and indexed by the file's page index.
    /// A page is read from the file until the mapping first writes it, and
    /// that write copies it in here. The file's own object stays in `objects`
    /// under the same id, attached shared, so a truncation of the file or a
    /// replacement in either object takes down whatever the region shows at
    /// that address, whichever object holds the page.
    pub(super) shadows: BTreeMap<u64, Arc<Vmo>>,
    /// The next object id to hand out. Never zero, which `Backing` reserves
    /// for private memory with no named object.
    next_id: u64,
    /// The objects `vmo_map`, `space_map` and the kernel's own code pages put
    /// here, each with the most its regions may be protected to, which the
    /// Linux calls that reshape a region -- `mremap`, `mprotect` -- may not
    /// touch: a native VMO's size and the rights its protection stands for
    /// belong to its handle (`docs/microkernel/K2.md` §2.3).
    native: BTreeMap<u64, Ceiling>,
    /// The digests of the images mapped executable here, room had before this
    /// lock (`docs/microkernel/K14.md` §4). What ran stays recorded: an unmap
    /// leaves it, a clone copies it.
    images: Option<Box<Images>>,
    /// The ids of exec's stack and its two guards, whose regions a mapping
    /// beside them never joins (`L.user.242`, `docs/ANON-MERGE.md` §5.3):
    /// a mapping at the stack's top joins neither the stack, as Linux's
    /// `VM_GROWSDOWN` keeps it apart, nor the guard above it.
    apart: [Option<u64>; APART],
}

/// How many ids [`Inner::apart`] holds: exec's stack and its two guards, and
/// one more for a stack a private `mremap` moved into an object of its own.
const APART: usize = 4;

/// [`AddressSpace::layout`]'s guard: the layout lock of the space it names,
/// held. A mapping is made in a neighbour's object only under its own
/// space's (`L.user.242`, `docs/ANON-MERGE.md` §5.4).
pub struct Layout<'a> {
    /// Whose layout.
    space: &'a AddressSpace,
    /// The lock, held until the guard goes.
    _held: SleepLockGuard<'a, ()>,
}

impl fmt::Debug for Layout<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Layout").finish_non_exhaustive()
    }
}

impl Layout<'_> {
    /// [`AddressSpace::map_anonymous`] for a Linux process's `mmap` or
    /// `brk`, under this layout: made in a neighbour's object where one
    /// qualifies (`L.user.242`, `docs/ANON-MERGE.md` §5.3), else in a new
    /// one. Answers the id of the object the mapping names.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::map_anonymous`].
    pub fn map_anonymous_beside(
        &self,
        at: u64,
        len: u64,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        let space = self.space;
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        let mut inner = space.inner.lock();
        match space.place_beside(&mut inner, range, flags) {
            Some((vmo, placed, retired)) => {
                drop(inner);
                space.retire_window(&vmo, retired);
                placed
            }
            None => space.map_fresh(&mut inner, range, flags),
        }
    }

    /// [`AddressSpace::map_anywhere`] for a Linux process's `mmap`, under
    /// this layout: made in a neighbour's object where one qualifies
    /// (`L.user.242`), else in a new one. Answers where.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::map_anywhere`].
    pub fn map_anywhere_beside(
        &self,
        hint: Option<u64>,
        len: u64,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        let space = self.space;
        if len == 0 {
            return Err(SpaceError::BadRange);
        }
        let mut inner = space.inner.lock();
        let at = inner
            .map
            .find_free(len, PAGE_SIZE, hint)
            .ok_or(SpaceError::OutOfMemory)?;
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        match space.place_beside(&mut inner, range, flags) {
            Some((vmo, placed, retired)) => {
                drop(inner);
                space.retire_window(&vmo, retired);
                placed.map(|_| at)
            }
            None => space.map_fresh(&mut inner, range, flags).map(|_| at),
        }
    }
}

// CHECKS-ONLY: fs/procfs/check.rs, syscall/unmap_check.rs, user/alloc_check.rs, user/rss_kept_check.rs
/// [`AddressSpace::map_anywhere`] for a check above the core: a new object
/// wherever there is room, never a neighbour's (a process's `mmap` goes
/// through [`Layout::map_anywhere_beside`]). Answers where.
///
/// # Errors
///
/// As [`AddressSpace::map_anywhere`].
pub fn map_anywhere_for_check(
    space: &AddressSpace,
    hint: Option<u64>,
    len: u64,
    flags: VmaFlags,
) -> Result<u64, SpaceError> {
    space.map_anywhere(hint, len, flags)
}

// CHECKS-ONLY: user/anon_merge_check.rs; home: user::space
/// Every own private anonymous object of `space` on one line, and named only
/// by private anonymous regions (`L.user.244`, `docs/ANON-MERGE.md` §5.1):
/// for each id no handle, file or shadow names, every region naming it is
/// `Backing::Anonymous`, has one value of `shared`, and names offset `v - b`
/// at each address `v` for one `b`. Answers how many private ones there are.
///
/// # Errors
///
/// Which of those failed, for the first id it failed for.
pub(crate) fn own_lines_for_check(space: &AddressSpace) -> Result<usize, &'static str> {
    let inner = space.inner.lock();
    // (shared, base) per own id met.
    let mut seen: BTreeMap<u64, (bool, u64)> = BTreeMap::new();
    for region in inner.map.iter() {
        let Some((id, offset, anonymous)) = unhandled(&inner, region) else {
            continue;
        };
        if !anonymous {
            return Err("an id no handle, file or shadow names was named by a file's region");
        }
        let here = (
            region.flags.shared,
            region.range.start().wrapping_sub(offset),
        );
        match seen.get(&id) {
            Some(&(shared, _)) if shared != here.0 => {
                return Err(
                    "an own anonymous object was named by a private region and a shared one",
                );
            }
            Some(&(_, line)) if line != here.1 => {
                return Err("an own private anonymous object's regions were not on one line");
            }
            Some(_) => {}
            None => {
                let _ = fallible::insert(&mut seen, id, here)
                    .map_err(|_| "no room to note an own anonymous object")?;
            }
        }
    }
    Ok(seen.values().filter(|&&(shared, _)| !shared).count())
}

/// `region`'s id, offset and whether it is anonymous, when no handle, file
/// or shadow of the space whose tables are `inner` names the id.
fn unhandled(inner: &Inner, region: &Vma) -> Option<(u64, u64, bool)> {
    let (id, offset, anonymous) = match region.backing {
        Backing::Anonymous { id, offset } => (id, offset, true),
        Backing::File { id, offset } => (id, offset, false),
        Backing::Device { .. } => return None,
    };
    let named = inner.native.contains_key(&id)
        || inner.files.contains_key(&id)
        || inner.shadows.contains_key(&id);
    (!named).then_some((id, offset, anonymous))
}

// CHECKS-ONLY: user/anon_merge_check.rs; home: user::space
/// The frame the page at `at` in `space` reads from, if its region's object
/// holds one: what the reuse check reads a window through (`L.user.243`).
pub(crate) fn frame_at_for_check(space: &AddressSpace, at: u64) -> Option<Frame> {
    let inner = space.inner.lock();
    let region = inner.map.find(at)?;
    let (id, offset) = match region.backing {
        Backing::Anonymous { id, offset } | Backing::File { id, offset } => (id, offset),
        Backing::Device { .. } => return None,
    };
    let index = (offset + (at - region.range.start())) / PAGE_SIZE;
    inner.objects.get(&id)?.page(index)
}

/// Whether no region of the space whose tables are `inner` reaches into
/// `range`: one binary search, O(log regions).
fn range_free(inner: &Inner, range: PageRange) -> bool {
    let regions = inner.map.iter().as_slice();
    let first = regions.partition_point(|region| region.range.end() <= range.start());
    regions
        .get(first)
        .is_none_or(|region| region.range.start() >= range.end())
}

/// The neighbour of `range` a mapping there is made in, in the space whose
/// tables are `inner`, as its id and the offset its line gives `range`
/// (`L.user.242`): the left neighbour if it qualifies, else the right.
fn beside_of(inner: &Inner, range: PageRange) -> Option<(u64, u64)> {
    let left = range
        .start()
        .checked_sub(1)
        .and_then(|last| inner.map.find(last))
        .filter(|region| region.range.end() == range.start())
        .and_then(|region| {
            let (id, offset) = own_line(inner, region)?;
            Some((id, offset.checked_add(region.range.bytes())?))
        });
    left.or_else(|| {
        let region = inner
            .map
            .find(range.end())
            .filter(|region| region.range.start() == range.end())?;
        let (id, offset) = own_line(inner, region)?;
        Some((id, offset.checked_sub(range.bytes())?))
    })
}

/// The object an `mremap` placed a region of `id` in, when it moved it into
/// one of its own (`backing` naming another id).
fn moved_object(backing: &Result<Backing, SpaceError>, id: u64) -> Option<u64> {
    match backing {
        Ok(Backing::Anonymous { id: to, .. }) if *to != id => Some(*to),
        _ => None,
    }
}

/// A kept-apart region of `from` moved by an `mremap` into object `to`:
/// `to` kept apart too (`L.user.242`, `docs/ANON-MERGE.md` §5.3).
fn carry_apart(inner: &mut Inner, from: u64, to: Option<u64>) {
    if let Some(to) = to
        && inner.apart.contains(&Some(from))
        && let Some(slot) = inner.apart.iter_mut().find(|slot| slot.is_none())
    {
        *slot = Some(to);
    }
}

/// The id and offset of `region`, if it may be joined by a mapping beside
/// it (`L.user.242`, `L.user.244`): a private anonymous region of an own
/// private anonymous object of the space whose tables are `inner` -- one no
/// handle, file or shadow names, so named only by private anonymous regions
/// on one line (`docs/ANON-MERGE.md` §5.1) -- that is not exec's stack or a
/// guard of it, and whose object is plain and named by this space alone.
fn own_line(inner: &Inner, region: &Vma) -> Option<(u64, u64)> {
    let Backing::Anonymous { id, offset } = region.backing else {
        return None;
    };
    if region.flags.shared
        || inner.native.contains_key(&id)
        || inner.files.contains_key(&id)
        || inner.shadows.contains_key(&id)
        || inner.apart.contains(&Some(id))
    {
        return None;
    }
    let vmo = inner.objects.get(&id)?;
    let plain = vmo.mapper_count() == 1
        && !vmo.is_sealed()
        && !vmo.is_pager_backed()
        && !vmo.is_coherent()
        && !vmo.is_combining()
        && !vmo.has_kernel_view();
    plain.then_some((id, offset))
}

/// [`AddressSpace::reach`]'s bits.
const REACH_TAINTED: u8 = 1;
const REACH_UNLISTED: u8 = 2;
const REACH_GRANTED: u8 = 4;
const REACH_STARTED: u8 = 8;

/// The certified list (`docs/microkernel/K14.md` §5): boot-sized, read once.
pub mod certified {
    use alloc::boxed::Box;

    use ferrix_sync::Once;

    static LIST: Once<Box<[[u8; 32]]>> = Once::new();

    /// The list, once, from the first process's archive: an empty one when the
    /// image carries none.
    pub fn set(list: Box<[[u8; 32]]>) {
        let _ = LIST.call_once(|| list);
    }

    /// Whether `digest` is on it.
    pub fn listed(digest: &[u8; 32]) -> bool {
        LIST.get().is_some_and(|list| list.contains(digest))
    }
}

/// The most image digests a space records (K14.11: a fixed block, not grown
/// under the lock).
pub const MAX_IMAGES: usize = 32;

/// A space's record of what it was given to run.
#[derive(Debug, Clone)]
pub(crate) struct Images {
    digests: [[u8; 32]; MAX_IMAGES],
    count: usize,
}

impl Images {
    /// An empty record, boxed; `None` for want of memory.
    fn boxed() -> Option<Box<Images>> {
        fallible::try_box(Images {
            digests: [[0; 32]; MAX_IMAGES],
            count: 0,
        })
        .ok()
    }

    /// Note `digest`, once. `false` when the record is full of others.
    fn note(&mut self, digest: [u8; 32]) -> bool {
        let Some(known) = self.digests.get(..self.count) else {
            return false;
        };
        if known.contains(&digest) {
            return true;
        }
        let Some(slot) = self.digests.get_mut(self.count) else {
            return false;
        };
        *slot = digest;
        self.count += 1;
        true
    }

    /// The digests recorded.
    pub(crate) fn digests(&self) -> &[[u8; 32]] {
        self.digests.get(..self.count).unwrap_or(&[])
    }
}

/// The most a region a handle or the kernel mapped may be protected to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ceiling {
    /// The kernel's own pages, mapped into every process (the vDSO, the
    /// ARMv7-A signal page): no call may change their protection.
    Kernel,
    /// Mapped through a VMO handle: writable only if the handle carried
    /// `WRITE`, executable only if it carried `EXECUTE`. Readable always.
    Handle {
        /// The handle carried `WRITE`.
        write: bool,
        /// The handle carried `EXECUTE`.
        execute: bool,
    },
}

impl Ceiling {
    /// Whether `flags` are within this ceiling, and never both writable and
    /// executable (ASR-2).
    pub(crate) fn admits(self, flags: VmaFlags) -> bool {
        match self {
            Ceiling::Kernel => false,
            Ceiling::Handle { write, execute } => {
                !(flags.write && flags.execute)
                    && (!flags.write || write)
                    && (!flags.execute || execute)
            }
        }
    }
}

/// Whether every region `range` reaches was mapped by handle and admits
/// `flags`: [`AddressSpace::protect_by_handle`]'s test, made before anything
/// changes. A region no handle mapped is refused before one past its ceiling.
fn within_ceilings(inner: &Inner, range: PageRange, flags: VmaFlags) -> Result<(), ProtectRefused> {
    let mut verdict = Ok(());
    let reached = inner
        .map
        .iter()
        .filter(|region| region.range.start() < range.end() && range.start() < region.range.end());
    for region in reached {
        let ceiling = match region.backing {
            Backing::Anonymous { id, .. } => inner.native.get(&id).copied(),
            _ => None,
        };
        match ceiling {
            None | Some(Ceiling::Kernel) => return Err(ProtectRefused::NotByHandle),
            Some(ceiling) if !ceiling.admits(flags) => verdict = Err(ProtectRefused::Beyond),
            Some(_) => {}
        }
    }
    verdict
}

/// Why [`AddressSpace::protect_by_handle`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProtectRefused {
    /// As the space refuses any change: a malformed range, or one not wholly
    /// mapped.
    Space(SpaceError),
    /// The range reaches a region no handle mapped: the program's own, or
    /// the kernel's.
    NotByHandle,
    /// The protection is past what a region's handle allowed, or is writable
    /// and executable.
    Beyond,
}

impl Inner {
    /// Note `digest` on the record, whose first room was had before this lock.
    fn note_image(&mut self, digest: [u8; 32], room: Option<Box<Images>>) -> bool {
        match (&mut self.images, room) {
            (Some(images), _) => images.note(digest),
            (empty, Some(mut room)) => {
                let noted = room.note(digest);
                *empty = Some(room);
                noted
            }
            (None, None) => false,
        }
    }

    /// The object a region names and its offset, if the Linux calls that
    /// reshape a region may: anonymous memory that `vmo_map` did not put here.
    fn reshapeable(&self, backing: Backing) -> Option<(u64, u64)> {
        match backing {
            Backing::Anonymous { id, offset } if !self.native.contains_key(&id) => {
                Some((id, offset))
            }
            _ => None,
        }
    }
}

/// The most leaves [`AddressSpace::find_frame`] visits: a gibibyte of
/// four-kilobyte pages. A program that maps more, and consumed poison past
/// them, is told address 0.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(
        dead_code,
        reason = "only x86-64's machine check finds a poisoned frame's address"
    )
)]
pub(crate) const FIND_FRAME_LEAVES: usize = 1 << 18;

/// One process's address space.
#[derive(Debug)]
pub struct AddressSpace {
    /// Physical frame of the root table.
    root: Frame,
    /// This space's number, never given to another: what a processor's PCID
    /// slot records, so that a space made over a freed root's frame never
    /// finds a dead space's translations (`docs/PCID.md`).
    id: u64,
    /// This space, as the objects it maps record it.
    me: Weak<AddressSpace>,
    /// Its resident pages, kept by the objects it maps as their pages and
    /// its regions come and go (`user::rss`, `L.user.236`): made with the
    /// space, by [`AddressSpace::new`] and [`AddressSpace::fork`] alike.
    rss: Arc<Rss>,
    /// The processors whose TLB may still hold this space's translations.
    cpus: CpuMask,
    /// Whether a processor holds this space's window (ledger 6998): written by its opener
    /// and closer alone, under no lock.
    #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
    window_held: AtomicBool,
    /// Its shootdowns: those not yet returned, and those ever begun.
    flushes: Flushes,
    /// Held by each system call that changes which ranges are mapped, from
    /// its first look at the map to its last change: see
    /// [`AddressSpace::layout`].
    layout: SleepLock<()>,
    pub(super) inner: SpinLock<Inner>,
    /// Whether its executable memory comes only from sealed, measured VMOs
    /// (`docs/microkernel/K14.md` §3). Set once, never cleared; read by the
    /// map calls with no job's lock taken (K14.7), and copied by a clone.
    measured: AtomicBool,
    /// What K14's grant reads (`docs/microkernel/K14.md` §5): a word of sticky
    /// bits, so that a pass is one load and no two spaces' locks nest.
    reach: AtomicU8,
    /// The speculation domain its programs are in, zero for none, or
    /// [`UNCLAIMED`] before a process has claimed it: see
    /// [`AddressSpace::claim_domain`] and `docs/OPAQUE-KERNEL.md` §9.2.
    domain: AtomicU64,
    /// Once it is a process's restricted space: its second root, which adds
    /// the region to it (`docs/microkernel/K11.md` §3).
    normal: Once<NormalRoot>,
    /// A window: no regions, pages placed by `SPACE_PLACE` (ledger 6411). The objects
    /// they come from, held until its tables are gone (W3).
    #[cfg(ferrix_personality = "user")]
    window: AtomicBool,
    #[cfg(ferrix_personality = "user")]
    placed: SpinLock<Vec<Arc<Vmo>>>,
    /// Whether its top tables stay as they emptied, so that [`NormalRoot`]'s
    /// copies of its root entries can never name a freed table: set before
    /// the normal root is made.
    keep_top: AtomicBool,
    /// Whether this is a region's space: refused [`AddressSpace::fork`] and
    /// any process of its own.
    region: AtomicBool,
    /// Space handles naming this space: while any lives, whoever holds one
    /// may write into what runs here, so no file gives it ids
    /// ([`AddressSpace::raises_refused`], `L.object.309`). `process_space` is
    /// refused after the start, so for a started process this only falls.
    handles: AtomicU64,
    /// Whether the OOM kill gave up waiting for this space's emptying
    /// (`object::oom`, `L.object.375`): set once, never cleared.
    oom_given_up: AtomicBool,
    /// Whether the OOM kill's emptier has begun unmapping this space without
    /// its layout lock (`object::oom`): a marked space never makes a mapping
    /// in a neighbour's object (`L.user.242`, `docs/ANON-MERGE.md` §5.4).
    /// Set once, never cleared.
    emptying: AtomicBool,
}

/// A restricted space's normal root: the kernel's half, the same restricted
/// slots, and the region's (`docs/microkernel/K11.md` §3).
#[derive(Debug)]
struct NormalRoot {
    /// Its frame, given back as the space goes.
    root: Frame,
    /// Its own id, for a PCID slot of its own.
    id: u64,
    /// The region's space, whose top tables it names.
    region: Arc<AddressSpace>,
}

/// The first root slot of a region: [`SHARED_BASE`]'s.
const FIRST_SHARED_SLOT: usize = (SHARED_BASE >> ROOT_SLOT_SHIFT) as usize;
/// The top of a program's space: the user half, or under `linuxd`, where the
/// region sits above it, `SHARED_BASE` (`docs/microkernel/K11.md` §2.1).
#[cfg(not(ferrix_load_built = "lift"))]
pub const PROGRAM_TOP: u64 = USER_VIRT_END;
/// The same.
#[cfg(ferrix_load_built = "lift")]
pub const PROGRAM_TOP: u64 = SHARED_BASE;

/// One past a region's last root slot: [`USER_VIRT_END`]'s, less the window's where the
/// build has one (`restricted_window`, K11 §2.1).
#[cfg(not(ferrix_restricted_window = "on"))]
const END_SHARED_SLOT: usize = (USER_VIRT_END >> ROOT_SLOT_SHIFT) as usize;
/// The same.
#[cfg(ferrix_restricted_window = "on")]
const END_SHARED_SLOT: usize = 255;
/// A read of `$space`'s master root at `$at`: as it always was with no window in the build, the
/// same tokens, so that such a kernel is unchanged; with one, by [`AddressSpace::translate_master`].
#[cfg(not(ferrix_restricted_window = "on"))]
macro_rules! master_translate {
    ($space:expr, $root:expr, $at:expr) => {
        mm::translate_in($root, $at)
    };
}
/// See the macro of this name with no window in the build.
#[cfg(ferrix_restricted_window = "on")]
macro_rules! master_translate {
    ($space:expr, $root:expr, $at:expr) => {{
        let _ = $root;
        $space.translate_master($at)
    }};
}
/// [`master_translate`]'s for a leaf's flags.
#[cfg(not(ferrix_restricted_window = "on"))]
macro_rules! master_flags {
    ($space:expr, $root:expr, $at:expr) => {
        mm::flags_in($root, $at)
    };
}
/// See the macro of this name with no window in the build.
#[cfg(ferrix_restricted_window = "on")]
macro_rules! master_flags {
    ($space:expr, $root:expr, $at:expr) => {{
        let _ = $root;
        $space.flags_master($at)
    }};
}

/// One past a region's last byte: as far as any user address goes without a window.
#[cfg(not(ferrix_restricted_window = "on"))]
const REGION_END: u64 = USER_VIRT_END;
/// One past a region's last byte: below the window's slot.
#[cfg(ferrix_restricted_window = "on")]
const REGION_END: u64 = (END_SHARED_SLOT as u64) << ROOT_SLOT_SHIFT;
/// The root slots of a region. Empty on ARMv7-A, which has none, whose user half ends
/// below [`SHARED_BASE`].
#[allow(
    clippy::reversed_empty_ranges,
    reason = "ARMv7-A's user half ends below the base; no region is ever made there"
)]
const fn shared_root_slots() -> core::ops::Range<usize> {
    FIRST_SHARED_SLOT..END_SHARED_SLOT
}

/// The low bit of a root slot in an address, on both architectures that have
/// restricted mode: four levels of nine bits over a 4 KiB page.
const ROOT_SLOT_SHIFT: u32 = 39;

impl AddressSpace {
    /// An empty address space, with the kernel reachable from it.
    ///
    /// # Errors
    ///
    /// [`SpaceError::OutOfMemory`] if there is no frame for the root.
    pub fn new() -> Result<Arc<AddressSpace>, SpaceError> {
        let root = mm::allocate_frames(0).ok_or(SpaceError::OutOfMemory)?;
        mm::zero_frame(root);

        // Before anything is mapped into it: on x86-64 this is what puts the
        // kernel's half in reach, and a trap taken in this address space
        // before it ran would have nowhere to go.
        arch::prepare_user_root(root * PAGE_SIZE);

        let map = ferrix_vma::AddressSpace::new(MMAP_MIN_ADDR, PROGRAM_TOP).map_err(|_| {
            // The window is a compile-time constant of the layout, so this is
            // unreachable in practice; reported rather than panicked because a
            // kernel has no supervisor to restart it.
            SpaceError::BadRange
        })?;

        let Ok(rss) = fallible::try_arc(Rss::new()) else {
            mm::deallocate_frames(root, 0);
            return Err(SpaceError::OutOfMemory);
        };
        fallible::try_arc_cyclic(|me| {
            AddressSpace::assemble(
                root,
                me,
                Inner {
                    map,
                    objects: BTreeMap::new(),
                    files: BTreeMap::new(),
                    shadows: BTreeMap::new(),
                    next_id: 1,
                    native: BTreeMap::new(),
                    images: None,
                    apart: [None; APART],
                },
                rss,
            )
        })
        .map_err(|_| {
            mm::deallocate_frames(root, 0);
            SpaceError::OutOfMemory
        })
    }

    /// Move the top of the window mappings may be placed in to `end`: what
    /// `execve` does for a program whose address space is smaller than the
    /// user half, a 32-bit program's, and back again for one whose is not
    /// (`docs/I386.md` §3.4). `mmap`'s search and every fixed mapping stop
    /// there.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] for an `end` that is not a page boundary
    /// inside the user half, or below a region still mapped; nothing changes.
    pub fn set_ceiling(&self, end: u64) -> Result<(), SpaceError> {
        // A restricted space's ceiling never rises past the region's floor
        // (`docs/microkernel/K11.md` §2.1, `L.user.181`).
        if end > USER_VIRT_END || (self.is_bound() && end > SHARED_BASE) {
            return Err(SpaceError::BadRange);
        }
        self.inner
            .lock()
            .map
            .set_high(end)
            .map_err(|_| SpaceError::BadRange)
    }

    /// One past the highest address a mapping may reach: the user half's
    /// end, or a 32-bit program's lower ceiling.
    pub fn ceiling(&self) -> u64 {
        self.inner.lock().map.high()
    }

    /// The lowest address in this space's program half at which `frame` is
    /// mapped, or `None` (`docs/MACHINE-CHECK.md` §3.5): where a program
    /// consumed a poisoned frame, for its `SIGBUS`. The tables have no map
    /// from a frame back to its pages, so the walk visits the program's
    /// leaves in address order under the space's own lock, stopping at the
    /// first match, at the program half's end, or after [`FIND_FRAME_LEAVES`]
    /// leaves -- the bound its hold of the lock has (MEMORY-AND-TIMING).
    #[cfg_attr(
        not(target_arch = "x86_64"),
        expect(
            dead_code,
            reason = "only x86-64's machine check finds a poisoned frame's address"
        )
    )]
    pub(crate) fn find_frame(&self, frame: Frame) -> Option<u64> {
        let wanted = frame * PAGE_SIZE;
        let inner = self.inner.lock();
        let end = inner.map.high();
        let mut found = None;
        let mut visited = 0_usize;
        mm::for_each_leaf_in(self.root * PAGE_SIZE, |leaf| {
            visited += 1;
            if leaf.virt.0 >= end || visited > FIND_FRAME_LEAVES {
                return false;
            }
            let start = leaf.phys.0;
            if (start..start + leaf.bytes()).contains(&wanted) {
                found = Some(leaf.virt.0 + (wanted - start));
                return false;
            }
            true
        });
        drop(inner);
        found
    }

    /// The physical address of the root table, for whoever installs it.
    pub fn root_table(&self) -> u64 {
        self.root * PAGE_SIZE
    }

    /// Run `walk` on the root table's physical address with this space's
    /// lock held, so that no fault, unmap or protect changes the tables
    /// while it reads them: for the W^X sweep of a user root
    /// (`mm::check::sweep_user_root`). `walk` takes no other space's lock.
    pub(crate) fn with_root_held<R>(&self, walk: impl FnOnce(u64) -> R) -> R {
        let _inner = self.inner.lock();
        walk(self.root * PAGE_SIZE)
    }

    /// Install this address space on the processor that is running, in place
    /// of `replacing` if that was installed there.
    ///
    /// Every page faulted in so far has been reachable only through the direct
    /// map, because the kernel was walking these tables in software. After
    /// this the *hardware* walks them, and a user virtual address means
    /// something on this processor.
    ///
    /// What this does not do is change privilege level. That is the other half
    /// of stage 6 and a separate thing entirely: a processor can translate
    /// through a user address space while still running the kernel's own code,
    /// and it has to be able to — a fault taken in user mode is handled by
    /// kernel code running in the address space that faulted.
    ///
    /// The processor joins this space's set before the root is loaded, and
    /// leaves `replacing`'s only after, for the reasons the module gives. A
    /// `replacing` that was not in fact installed here costs nothing: the
    /// root write just made has dropped whatever of it this TLB held, or,
    /// with PCIDs, left it under a PCID that is not current, which the
    /// module's last rule covers.
    ///
    /// # Safety
    ///
    /// (TRANSLATE) Something must hold a reference to this address space for as long as it
    /// stays installed. The tables are freed when the last [`Arc`] goes, and a
    /// processor whose root register still names freed frames is walking
    /// memory the allocator has handed to somebody else.
    ///
    /// Interrupts must be masked across the call, so that the processor that
    /// joins is the one whose root is loaded, and the caller must not be
    /// preempted into a context expecting a different address space.
    pub unsafe fn install(&self, replacing: Option<&AddressSpace>) {
        // SAFETY: (TRANSLATE) the caller's guarantee, for the mode the running
        // task's next instruction in ring 3 runs in; a space with no region
        // has one root, whichever.
        unsafe { self.install_as(replacing, false) }
    }

    /// [`AddressSpace::install`], with the root a task in `restricted` mode
    /// runs on (`docs/microkernel/K11.md` §3).
    ///
    /// # Safety
    ///
    /// (TRANSLATE) As [`AddressSpace::install`].
    pub(crate) unsafe fn install_as(&self, replacing: Option<&AddressSpace>, restricted: bool) {
        // The speculation domains the barrier decision compares: the
        // outgoing space's read as it leaves, so that a program that left
        // its domain while it ran is out of it now (`docs/OPAQUE-KERNEL.md`
        // §9.2), and this one's.
        if let Some(previous) = replacing {
            arch::left_space(previous.domain());
        }
        arch::entering_space(self.domain());
        let cpu = this_logical_cpu();
        // NOALLOC: a `CpuMask` is a fixed bitmap; joining sets a bit.
        self.cpus.join(cpu);
        // No root write here while another processor has the window open (ledger 6998).
        #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
        self.wait_for_the_window();
        // This processor's bit is up, so no opener can pass from here on: an empty slot now, and
        // until it leaves. The flag alone would not say so, as a decliner raises it for an instant.
        // A stale read through a missing wait is too rare to catch by racing.
        #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
        if self.normal.get().is_some() && self.window_entry().load(Ordering::SeqCst) != 0 {
            crate::panic::fatal!(
                crate::panic::catalog::WINDOW_HELD,
                "a processor joined a space whose window was open"
            );
        }
        // After the join, for the order the module gives: a shootdown whose
        // set read missed the join is one whose count this read sees.
        let begun = self.flushes.begun.load(Ordering::SeqCst);
        let normal = self.normal.get();
        if let Some(normal) = normal {
            // NOALLOC: as above.
            normal.region.cpus.join(cpu);
        }
        let (root, id, begun) = match normal {
            Some(normal) => self.root_for(normal, restricted),
            None => (self.root * PAGE_SIZE, self.id, begun),
        };
        // SAFETY: (TRANSLATE) the root was made by `new` or `bind_region`, so
        // `prepare_user_root` has run on it and the kernel is reachable through
        // it on the architecture that needs that; the caller guarantees it
        // outlives the installation, and masks interrupts on `cpu`, the
        // processor running this. The speculation decision is keyed on the
        // restricted root, whichever root is written: two roots of one process
        // are one key (K1 §6).
        unsafe { arch::install_user_root(root, id, begun, cpu, self.root * PAGE_SIZE) };
        crate::sched::trip::count(crate::sched::trip::Count::RootInstall);
        if let Some(previous) = replacing
            && !core::ptr::eq(previous, self)
        {
            previous.cpus.leave(cpu);
            if let Some(normal) = previous.normal.get()
                && !normal
                    .region
                    .is_same_region(self.normal.get().map(|next| &next.region))
            {
                normal.region.cpus.leave(cpu);
            }
        }
    }

    /// Whether `other` is this same space.
    fn is_same_region(&self, other: Option<&Arc<AddressSpace>>) -> bool {
        other.is_some_and(|other| core::ptr::eq(&raw const **other, self))
    }

    /// Leave this address space on the processor that is running.
    ///
    /// After this no user address translates here, which is the state a kernel
    /// thread runs in. The processor leaves the set after the root write,
    /// which on every architecture drops the user translations it had cached
    /// or, on x86-64 with PCIDs, takes them out of use: the kernel's root is
    /// loaded with PCID 0, whose flush does not reach the space's own PCID,
    /// and the space's translations stay under that PCID, used for nothing,
    /// until `install` flushes it or keeps it by the rule the module gives
    /// for a switch to another space (`docs/PCID.md` §3).
    ///
    /// # Safety
    ///
    /// (TRANSLATE) Nothing on this processor may still need a user address, and interrupts
    /// must be masked across the call.
    pub unsafe fn uninstall(&self) {
        // As it leaves, for the next install's decision: see `install`.
        arch::left_space(self.domain());
        let cpu = this_logical_cpu();
        // SAFETY: (TRANSLATE) the caller guarantees no user address is wanted, and the kernel
        // is reachable without one on every architecture.
        unsafe { arch::uninstall_user_root() };
        crate::sched::trip::count(crate::sched::trip::Count::RootUninstall);
        self.cpus.leave(cpu);
        if let Some(normal) = self.normal.get() {
            normal.region.cpus.leave(cpu);
        }
    }
}

impl AddressSpace {
    /// What [`AddressSpace::new`] and [`AddressSpace::fork`] make alike: a
    /// space over `root` holding `inner`, on no processor, with no shootdown
    /// begun, and claimed by no speculation domain yet.
    fn assemble(root: Frame, me: &Weak<AddressSpace>, inner: Inner, rss: Arc<Rss>) -> AddressSpace {
        AddressSpace {
            root,
            id: next_id(),
            me: me.clone(),
            rss,
            cpus: CpuMask::new(),
            #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
            window_held: AtomicBool::new(false),
            flushes: Flushes::new(),
            layout: SleepLock::new((), &crate::sync::SchedParker),
            handles: AtomicU64::new(0),
            oom_given_up: AtomicBool::new(false),
            emptying: AtomicBool::new(false),
            inner: SpinLock::new(inner),
            measured: AtomicBool::new(false),
            reach: AtomicU8::new(0),
            domain: AtomicU64::new(UNCLAIMED),
            normal: Once::new(),
            #[cfg(ferrix_personality = "user")]
            window: AtomicBool::new(false),
            #[cfg(ferrix_personality = "user")]
            placed: SpinLock::new(Vec::new()),
            keep_top: AtomicBool::new(false),
            region: AtomicBool::new(false),
        }
    }

    /// A new region's space: empty, its window `[SHARED_BASE, USER_VIRT_END)`,
    /// and with the tables under every root slot of that window made now, so
    /// that a normal root copies its entries once and never again
    /// (`docs/microkernel/K11.md` §3).
    ///
    /// # Errors
    ///
    /// [`SpaceError::OutOfMemory`].
    pub(crate) fn new_region() -> Result<Arc<AddressSpace>, SpaceError> {
        let space = AddressSpace::new()?;
        space.region.store(true, Ordering::Release);
        space.keep_top.store(true, Ordering::Release);
        {
            let mut inner = space.inner.lock();
            inner.map = ferrix_vma::AddressSpace::new(SHARED_BASE, REGION_END)
                .map_err(|_| SpaceError::BadRange)?;
        }
        for slot in shared_root_slots() {
            mm::ensure_top_in(space.root_table(), (slot as u64) << ROOT_SLOT_SHIFT)
                .map_err(|_| SpaceError::OutOfMemory)?;
        }
        Ok(space)
    }

    /// Whether this is a region's space.
    pub fn is_region(&self) -> bool {
        self.region.load(Ordering::Acquire)
    }

    /// Whether its pages are placed, with no regions (ledger 6411).
    #[cfg(ferrix_personality = "user")]
    pub fn is_window(&self) -> bool {
        self.window.load(Ordering::Acquire)
    }

    /// Make this a window: a restricted space, not measured, with nothing in it (W6).
    #[cfg(ferrix_personality = "user")]
    pub fn open_window(&self) -> Result<(), SpaceError> {
        let inner = self.inner.lock();
        if !self.is_bound() || self.is_measured() || inner.map.iter().next().is_some() {
            return Err(SpaceError::Refused(0));
        }
        self.window.store(true, Ordering::Release);
        Ok(())
    }

    /// A copy's touch of a placed page: as the page table says, under the lock a place
    /// takes, so the frame cannot be changed under it.
    #[cfg(ferrix_personality = "user")]
    fn with_placed<R>(
        &self,
        address: u64,
        access: Access,
        touch: impl FnOnce(u64) -> R,
    ) -> Result<Option<R>, SpaceError> {
        let _inner = self.inner.lock();
        let root = self.root * PAGE_SIZE;
        let Some(flags) = master_flags!(self, root, address) else {
            return Ok(None);
        };
        if access.write && !flags.write {
            return Ok(None);
        }
        let Some(physical) = master_translate!(self, root, address) else {
            return Ok(None);
        };
        let virt = mm::direct_map_ram(physical).ok_or(SpaceError::Refused(address))?;
        Ok(Some(touch(virt)))
    }

    /// Place `entries`, in order, stopping at the first refusal: how many went in, and
    /// the refusal. Each is a virtual page and a page of `vmo`, checked as a mapping by
    /// handle is, and what replaced or lost a right is shot down before this returns
    /// (W4, W5). A window has no region and no rmap: the object is held instead (W3).
    #[cfg(ferrix_personality = "user")]
    pub fn place(
        &self,
        vmo: &Arc<Vmo>,
        ceiling_write: bool,
        ceiling_execute: bool,
        entries: &[(u64, u64, bool, bool)],
    ) -> (usize, Option<SpaceError>) {
        if !self.is_window() {
            return (0, Some(SpaceError::Refused(0)));
        }
        if !vmo.mark_placed() {
            return (0, Some(SpaceError::Refused(0)));
        }
        {
            let mut held = self.placed.lock();
            if !held.iter().any(|known| Arc::ptr_eq(known, vmo)) {
                if held.len() >= PLACED_OBJECTS {
                    return (0, Some(SpaceError::OutOfMemory));
                }
                if fallible::try_push(&mut held, Arc::clone(vmo)).is_err() {
                    return (0, Some(SpaceError::OutOfMemory));
                }
            }
        }
        let mut done = 0;
        let mut refusal = None;
        let mut pages = TlbPages::new();
        let root = self.root * PAGE_SIZE;
        let cpus = {
            let inner = self.inner.lock();
            for &(virt, offset, write, execute) in entries {
                let in_range = virt.is_multiple_of(PAGE_SIZE)
                    && offset.is_multiple_of(PAGE_SIZE)
                    && is_user_address(virt)
                    && virt
                        .checked_add(PAGE_SIZE)
                        .is_some_and(|end| end <= SHARED_BASE)
                    && offset
                        .checked_add(PAGE_SIZE)
                        .is_some_and(|end| end <= vmo.len_bytes());
                if !in_range {
                    refusal = Some(SpaceError::BadRange);
                    break;
                }
                if (write && (execute || !ceiling_write)) || (execute && !ceiling_execute) {
                    refusal = Some(SpaceError::Refused(virt));
                    break;
                }
                let Ok(frame) = vmo.commit(offset / PAGE_SIZE) else {
                    refusal = Some(SpaceError::OutOfMemory);
                    break;
                };
                let present = master_translate!(self, root, virt).is_some();
                #[allow(clippy::excessive_nesting, reason = "one walk's page, in place")]
                if present {
                    let Ok(range) = PageRange::from_len(virt, PAGE_SIZE) else {
                        refusal = Some(SpaceError::BadRange);
                        break;
                    };
                    self.unmap_range(range, &mut pages);
                }
                let flags = typed(user_page(write, execute), vmo);
                if mm::map_in(root, virt, frame * PAGE_SIZE, PAGE_SIZE, flags).is_err() {
                    refusal = Some(SpaceError::OutOfMemory);
                    break;
                }
                done += 1;
            }
            self.begin_shootdown(&inner)
        };
        self.shoot(&cpus, &mut pages);
        (done, refusal)
    }

    /// A window's half of an unmap: its leaves in `range` down, what to flush, and who to
    /// tell if anyone. At the shootdown's return, no translation of the range is on any
    /// processor (W4): only what came down is named, and a shootdown still out from another
    /// call, which may be of a leaf this one found gone, asks for everything; a range with
    /// nothing in it and none out begins none, and `begun` stays (ledger 6421).
    #[cfg(ferrix_personality = "user")]
    fn window_take_down(&self, range: PageRange) -> (TlbPages, Option<CpuSet>) {
        let inner = self.inner.lock();
        let mut pages = mm::unmap_placed(self.root * PAGE_SIZE, range.start(), range.bytes());
        if self.flushes.pending.load(Ordering::SeqCst) > 0 {
            pages.everything();
        }
        let cpus = (!pages.is_empty()).then(|| self.begin_shootdown(&inner));
        (pages, cpus)
    }

    /// Whether this is bound to a region as a process's restricted space.
    pub(crate) fn is_bound(&self) -> bool {
        self.normal.get().is_some()
    }

    /// Bind this space to `region` as a process's restricted space: its
    /// ceiling comes down to [`SHARED_BASE`], its top tables stop being freed,
    /// and its normal root is made. Once.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] for a space with a mapping at or above
    /// [`SHARED_BASE`], and one already bound, or a region's;
    /// [`SpaceError::OutOfMemory`]. A refusal leaves it as it was.
    pub fn bind_region(&self, region: &Arc<AddressSpace>, owner: u32) -> Result<(), SpaceError> {
        if !region.is_region() {
            return Err(SpaceError::BadRange);
        }
        let frame = mm::allocate_root_for(owner).ok_or(SpaceError::OutOfMemory)?;
        arch::prepare_user_root(frame * PAGE_SIZE);
        for slot in shared_root_slots() {
            let _ = mm::copy_root_slot(region.root_table(), frame * PAGE_SIZE, slot);
        }
        // One decision, under this space's lock: of two binds of one space by
        // duplicate handles one wins and the other is refused; and a space a
        // process already runs on, or a region's, is never bound.
        let refused = {
            let mut inner = self.inner.lock();
            if self.is_bound()
                || self.is_region()
                || self.domain.load(Ordering::Acquire) != UNCLAIMED
                || inner.map.set_high(SHARED_BASE).is_err()
            {
                true
            } else {
                self.keep_top.store(true, Ordering::Release);
                for slot in 0..FIRST_SHARED_SLOT {
                    let _ = mm::copy_root_slot(self.root_table(), frame * PAGE_SIZE, slot);
                }
                let _ = self.normal.call_once(|| NormalRoot {
                    root: frame,
                    id: next_id(),
                    region: Arc::clone(region),
                });
                false
            }
        };
        if refused {
            mm::free_root(frame);
            return Err(SpaceError::BadRange);
        }
        Ok(())
    }

    /// Copy the root entry `address` falls in from the restricted root to the
    /// normal one, if the first has one and the second does not: what a fault
    /// from normal mode does first, so that a slot's cost is one fault per
    /// process and the restricted root's path is untouched
    /// (`docs/microkernel/K11.md` §3). `true` if it copied one, when the
    /// faulting access may well be right now.
    pub(crate) fn sync_top(&self, address: u64) -> bool {
        let Some(normal) = self.normal.get() else {
            return false;
        };
        let slot = (address >> ROOT_SLOT_SHIFT) as usize;
        if slot >= FIRST_SHARED_SLOT {
            return false;
        }
        let _inner = self.inner.lock();
        let at = normal.root * PAGE_SIZE;
        if mm::root_slot_entry(at, slot) != 0 {
            return false;
        }
        mm::copy_root_slot(self.root_table(), at, slot)
    }

    /// The space a program pointer or a fault at `address` means, for the
    /// running task, whose own space this may be: below [`SHARED_BASE`] this,
    /// and at or above it the region's, only in normal mode and only for the
    /// running task's own space (`docs/microkernel/K11.md` §2.3, `L.user.182`).
    /// A by-handle call names its own space and goes straight there.
    pub fn route(&self, address: u64) -> &AddressSpace {
        let Some(normal) = self.normal.get() else {
            return self;
        };
        if address < SHARED_BASE || crate::sched::running_restricted() {
            return self;
        }
        let own = crate::sched::with_current(|task| {
            task.address_space()
                .is_some_and(|mine| core::ptr::eq(&raw const **mine, self))
        })
        .unwrap_or(false);
        if own { &normal.region } else { self }
    }

    /// [`AddressSpace::route`], as the reference a caller that keeps one needs.
    pub fn routed(this: &Arc<AddressSpace>, address: u64) -> &Arc<AddressSpace> {
        match this.normal.get() {
            Some(normal) if !core::ptr::eq(this.route(address), &raw const **this) => {
                &normal.region
            }
            _ => this,
        }
    }

    #[cfg_attr(
        not(ferrix_restricted_fastpath = "on"),
        allow(dead_code, reason = "K1's fast crossings alone ask it")
    )]
    /// Put this space's root for `restricted` on the running processor: a
    /// flip, or a switch between two of its threads in different modes. The
    /// processor is in its sets already (`docs/microkernel/K11.md` §3), so
    /// this is a root write and nothing else; the speculation decision is
    /// the caller's, and the flip does not call `entered_space`.
    ///
    /// # Safety
    ///
    /// (TRANSLATE) As [`AddressSpace::install`]: this space is the one the
    /// running processor has joined, and interrupts are masked.
    #[inline(always)]
    pub(crate) unsafe fn flip_root_on(&self, restricted: bool, cpu: usize) {
        // Only a space with a normal root is flipped. Under K1's fast path, whose conditions
        // are what keep that true, an image stops on another (ledger 6747 (3c)), at the cost of
        // one branch: a fatal no case reaches, by what a fast crossing's conditions imply.
        #[cfg(not(ferrix_fast_restricted))]
        // SAFETY: only a space with a normal root is flipped.
        let normal = unsafe { self.normal.get().unwrap_unchecked() };
        #[cfg(ferrix_fast_restricted)]
        let Some(normal) = self.normal.get() else {
            crate::panic::fatal!(
                crate::panic::catalog::FLIP_WITH_NO_NORMAL_ROOT,
                "a fast crossing flipped a space with no normal root"
            )
        };
        let (root, id, begun) = self.root_for(normal, restricted);
        // SAFETY: (TRANSLATE) the caller's guarantee; both roots carry the
        // kernel's half, and live as long as this space.
        unsafe { arch::flip_user_root(root, id, begun, cpu) };
    }

    /// [`AddressSpace::flip_root_on`] on this processor.
    ///
    /// # Safety
    ///
    /// As it.
    #[inline(always)]
    pub(crate) unsafe fn flip_root(&self, restricted: bool) {
        let Some(normal) = self.normal.get() else {
            return;
        };
        let cpu = this_logical_cpu();
        let (root, id, begun) = self.root_for(normal, restricted);
        // SAFETY: (TRANSLATE) the caller's guarantee; both roots carry the
        // kernel's half, and live as long as this space.
        unsafe { arch::flip_user_root(root, id, begun, cpu) };
    }

    /// Take the window, if this processor is the only one in the space: the flag first,
    /// then the set, as [`AddressSpace::wait_for_the_window`] joins and then reads. `false`
    /// leaves the flag down and nothing written.
    #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
    #[inline(always)]
    pub(crate) fn window_take(&self, cpu: usize) -> bool {
        self.window_held.store(true, Ordering::SeqCst);
        if self.cpus.only(cpu) {
            return true;
        }
        self.window_held.store(false, Ordering::SeqCst);
        false
    }

    /// Give the window back, after its slot is clear and its pages invalidated.
    #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
    #[inline(always)]
    pub(crate) fn window_give(&self) {
        self.window_held.store(false, Ordering::Release);
    }

    /// Wait out a window another processor holds on this space: one masked run of a
    /// stub, which waits on nobody. Past the deadline the holder is dead, and that is named.
    #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
    fn wait_for_the_window(&self) {
        /// About two seconds at the guest's clock: a descheduled vCPU, and no more.
        const PATIENCE: u64 = 10_000_000_000;
        // SeqCst though Acquire is the ruling's, at no cost on x86-64: the set's `fetch_or`
        // before it and the opener's two make the Dekker pair in the language's own model too.
        if !self.window_held.load(Ordering::SeqCst) {
            return;
        }
        let began = arch::cheap_counter();
        while self.window_held.load(Ordering::Acquire) {
            if arch::cheap_counter().wrapping_sub(began) > PATIENCE {
                crate::panic::fatal!(
                    crate::panic::catalog::WINDOW_HELD,
                    "a processor joined a space whose window was held past any stub's run"
                );
            }
            core::hint::spin_loop();
        }
    }

    /// Its restricted root's top table's entry for the window.
    #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
    pub(crate) fn window_entry(&self) -> &AtomicU64 {
        let entry = mm::root_slot_pointer(self.root * PAGE_SIZE, mm::WINDOW_ROOT_SLOT);
        // SAFETY: (FRAME) a word of this space's own root, which lives as long as it, aligned,
        // that nothing but the opener and the closer writes: `END_SHARED_SLOT` keeps every map
        // and every region out of it, and teardown runs with no thread left to open it.
        unsafe { AtomicU64::from_ptr(entry) }
    }

    /// Whether a software read of the window's two pages is refused, as the check asks it many
    /// times from a processor outside the set while another runs stubs (ledger 6998, X1).
    #[cfg(all(ferrix_restricted_window = "on", not(ferrix_load_built = "lift")))]
    #[doc(hidden)]
    pub fn window_readers_refuse(&self, stub: u64, data: u64) -> bool {
        self.translate_master(stub).is_none()
            && self.flags_master(stub).is_none()
            && self.translate_master(data).is_none()
            && self.flags_master(data).is_none()
    }

    /// A read of this space's own master root by software, on any processor, whose set
    /// bit nothing asks for: a restricted root holds nothing at or above [`SHARED_BASE`] but
    /// a window, while it is open, so that much is refused before the walk (ledger 6998, X1).
    #[cfg(ferrix_restricted_window = "on")]
    #[inline(always)]
    fn translate_master(&self, at: u64) -> Option<u64> {
        if self.master_refuses(at) {
            return None;
        }
        mm::translate_in(self.root * PAGE_SIZE, at)
    }

    /// See [`AddressSpace::translate_master`].
    #[cfg(ferrix_restricted_window = "on")]
    #[inline(always)]
    fn flags_master(&self, at: u64) -> Option<MapFlags> {
        if self.master_refuses(at) {
            return None;
        }
        mm::flags_in(self.root * PAGE_SIZE, at)
    }

    /// See [`AddressSpace::translate_master`].
    #[cfg(ferrix_restricted_window = "on")]
    #[inline(always)]
    fn master_refuses(&self, at: u64) -> bool {
        at >= SHARED_BASE && self.normal.get().is_some()
    }

    /// What a root write for `restricted` names: its table, its PCID key and
    /// the shootdown count it answers to, both spaces' for the normal root.
    #[inline(always)]
    fn root_for(&self, normal: &NormalRoot, restricted: bool) -> (u64, u64, u64) {
        let own = self.flushes.begun.load(Ordering::SeqCst);
        if restricted {
            (self.root * PAGE_SIZE, self.id, own)
        } else {
            (
                normal.root * PAGE_SIZE,
                normal.id,
                own.wrapping_add(normal.region.flushes.begun.load(Ordering::SeqCst)),
            )
        }
    }

    /// The root slots a region has.
    pub(crate) const fn shared_slots(&self) -> core::ops::Range<usize> {
        shared_root_slots()
    }

    /// Hold the normal root to its rule (`L.user.180`): each restricted slot
    /// the restricted root's entry or absent, each shared slot the region's.
    ///
    /// # Errors
    ///
    /// What it found.
    #[doc(hidden)]
    pub fn check_normal_slots(&self) -> Result<(), &'static str> {
        let normal = self.normal.get().ok_or("a space with no normal root")?;
        let _inner = self.inner.lock();
        let at = normal.root * PAGE_SIZE;
        for slot in 0..FIRST_SHARED_SLOT {
            let entry = mm::root_slot_entry(at, slot);
            if entry != 0 && entry != mm::root_slot_entry(self.root_table(), slot) {
                return Err("a normal root's restricted slot is not the restricted root's");
            }
        }
        for slot in shared_root_slots() {
            if mm::root_slot_entry(at, slot)
                != mm::root_slot_entry(normal.region.root_table(), slot)
            {
                return Err("a normal root's shared slot is not the region's");
            }
        }
        Ok(())
    }

    /// Whether its tables keep their top level: see the field.
    fn keeps_top(&self) -> bool {
        self.keep_top.load(Ordering::Relaxed)
    }
}

/// The next [`AddressSpace`]'s id. From one: zero is a PCID slot holding no
/// space.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A new space's id, never given before: the counter only moves up, and the
/// kernel stops rather than wrap it to an id already given or to zero.
fn next_id() -> u64 {
    match NEXT_ID.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1)) {
        Ok(id) if id != 0 => id,
        _ => crate::panic::fatal!(
            crate::panic::catalog::SPACE_IDS_EXHAUSTED,
            "no address space id is left that was never given"
        ),
    }
}

/// An [`AddressSpace`]'s domain before any process has claimed it.
const UNCLAIMED: u64 = u64::MAX;

/// An [`AddressSpace`]'s domain once it is out of every domain for good: two
/// domains claimed it, or its process left its domain. Zero, by contrast, is
/// a space claimed by a process in no domain, which a birth may still place.
const OUT: u64 = u64::MAX - 1;

impl AddressSpace {
    /// A space handle to this space was made.
    pub(crate) fn handle_made(&self) {
        let _ = self.handles.fetch_add(1, Ordering::SeqCst);
    }

    /// A space handle to this space is gone.
    pub(crate) fn handle_gone(&self) {
        let _ = self.handles.fetch_sub(1, Ordering::SeqCst);
    }

    /// Whether a program here may be given ids by a file: not while a handle
    /// names this space (`L.object.309`, ledger 5727 (1), 6256).
    pub fn raises_refused(&self) -> bool {
        self.handles.load(Ordering::SeqCst) != 0
    }

    /// Mark the OOM kill's wait for this space's emptying given up
    /// (`object::oom`, `L.object.375`): whether it was already.
    pub(crate) fn give_up_oom_emptying(&self) -> bool {
        self.oom_given_up.swap(true, Ordering::AcqRel)
    }

    /// Whether the OOM kill gave up waiting for this space's emptying.
    pub(crate) fn oom_emptying_given_up(&self) -> bool {
        self.oom_given_up.load(Ordering::Acquire)
    }

    /// Mark the space as being emptied without its layout lock: what the
    /// OOM kill's emptier stores before its first unmap, so that no mapping
    /// is made in a neighbour's object while its unmaps run (`L.user.242`,
    /// `docs/ANON-MERGE.md` §5.4). Stored before that unmap takes the
    /// space's lock, and read under it.
    pub(crate) fn mark_emptying(&self) {
        self.emptying.store(true, Ordering::Release);
    }

    /// Record `id`, exec's stack's or one of its guards', as never joined
    /// by a mapping beside it (`L.user.242`, `docs/ANON-MERGE.md` §5.3).
    pub fn keep_apart(&self, id: u64) {
        let mut inner = self.inner.lock();
        if let Some(slot) = inner.apart.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(id);
        }
    }
}

impl AddressSpace {
    /// The speculation domain the switch barrier decision reads: zero for
    /// none, which an unclaimed space and one out for good are too.
    pub fn domain(&self) -> u64 {
        match self.domain.load(Ordering::Acquire) {
            UNCLAIMED | OUT => 0,
            domain => domain,
        }
    }

    /// A process in speculation domain `domain` (zero for none) uses this
    /// space: as it is made for the process, or as a second process comes to
    /// share it (`CLONE_VM` without `CLONE_THREAD`).
    ///
    /// The first claim sets it. A claim that disagrees puts it out of every
    /// domain for good: a space two domains share is in neither, since a
    /// switch between its programs and either domain's must keep the barrier.
    pub(crate) fn claim_domain(&self, domain: u64) {
        let _ = self
            .domain
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| match held {
                UNCLAIMED => Some(domain),
                held if held == domain || held == OUT => None,
                _ => Some(OUT),
            });
    }

    /// Put the space in `domain` as its one process is born there: native
    /// `process_create` makes a process in the root job and then moves it,
    /// unstarted, into the job it was made for. Only for a space no other
    /// process shares, which a process not yet started is the sole user of.
    /// A space out for good stays out.
    pub(crate) fn set_birth_domain(&self, domain: u64) {
        let _ = self
            .domain
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held != OUT).then_some(domain)
            });
    }

    /// Its process has left its domain: out of every domain, for good.
    pub(crate) fn leave_domain(&self) {
        self.domain.store(OUT, Ordering::Release);
    }
}

/// The logical number of the processor running this, for its bit in a set.
///
/// Stops the machine rather than guessing when there is no per-CPU record. A
/// user space is installed only by the scheduler and by stage 6's checks, both
/// long after every processor has its record, so a missing one is a kernel
/// bug; and a default -- zero, say -- would put the wrong processor in the set
/// and leave the right one out, which no later check would see.
#[inline(always)]
fn this_logical_cpu() -> usize {
    match smp::this_cpu() {
        Some(cpu) => cpu.logical,
        None => {
            crate::panic::fatal!(
                crate::panic::catalog::SPACE_SET_WITHOUT_RECORD,
                "an address space was installed or uninstalled on a processor with no per-CPU record"
            );
        }
    }
}

/// How many times a fault fills a page that reclaim took again before it gives
/// the program a `SIGBUS`: reclaim would have to take it every time.
const REFILLS: usize = 64;

/// Fill `frame` with page `index` of `file`: a copy of it, or zeros for a hole.
/// A page of a file on a disk that was filled before the caller's lock and
/// reclaimed since is not a hole: the frame is given back and the fault is to
/// fill it again (`user::cache`).
fn copy_or_zero(file: &Vmo, index: u64, frame: Frame) -> Result<(), SpaceError> {
    match file.page(index) {
        // A write-combining object's page is read through the write-back
        // direct map: its lines out of the caches first (`L.user.133`).
        Some(original) if file.is_combining() => {
            arch::flush_cache_lines(mm::direct_map(original * PAGE_SIZE), PAGE_SIZE);
            mm::copy_frame(frame, original);
        }
        Some(original) => mm::copy_frame(frame, original),
        None if file.sourced(index) => {
            let _ = mm::release_frame(frame);
            return Err(SpaceError::Evicted);
        }
        None => mm::zero_frame(frame),
    }
    Ok(())
}

/// The frame for page `index` of `vmo`, committed on first touch.
///
/// A page of a file mapping (`file`) wholly past the end of its file is
/// refused with [`SpaceError::PastEnd`] at `address`, never committed: the
/// end is read under the object's pages lock, and a truncation lowers it
/// before it takes that lock to take pages away.
fn commit_page(vmo: &Vmo, index: u64, file: bool, address: u64) -> Result<Frame, SpaceError> {
    // A pager-backed object's absent page is its pager's, never zeros: it is
    // committed as a file's is, and its absence sends the fault round again.
    if file || vmo.is_pager_backed() {
        match vmo.commit_within(index).map_err(SpaceError::Backing)? {
            Commit::Frame(frame) => Ok(frame),
            Commit::PastEnd => Err(SpaceError::PastEnd(address)),
            Commit::Unfilled => Err(SpaceError::Evicted),
        }
    } else {
        vmo.commit(index).map_err(SpaceError::Backing)
    }
}

/// How a region with `flags` maps its object.
/// A region [`AddressSpace::add_region`] maps.
#[derive(Debug, Clone, Copy)]
struct NewRegion {
    /// The object id the region names.
    id: u64,
    /// Where.
    range: PageRange,
    /// How.
    flags: VmaFlags,
    /// What backs it.
    backing: Backing,
    /// How the object is shared through it.
    sharing: Sharing,
}

/// The object an `mremap` of the region at `old`, naming `id`, resizes or
/// moves; with room had in the map for what follows -- a split at each end of
/// the two removals, and the region put back -- because once the old range
/// is out, a refusal to put it back down would lose the mapping.
fn remap_object(inner: &mut Inner, id: u64, old: u64) -> Result<Arc<Vmo>, SpaceError> {
    let vmo = Arc::clone(inner.objects.get(&id).ok_or(SpaceError::NotMapped(old))?);
    // FALLIBLE: the map's reserve refuses with `VmaError::NoMemory`.
    inner.map.reserve(3).map_err(map_error)?;
    Ok(vmo)
}

/// Copy page `index` of `vmo`, whose frame `shared` another space still
/// holds, into a frame of its own: the copy, and the original taken out of
/// the object, to be retired.
///
/// Replacing takes the shared page out of this object; the reference is
/// given back once no processor can reach it through this space or any other
/// that maps the object.
fn copy_on_write(
    vmo: &Vmo,
    index: u64,
    shared: Frame,
) -> Result<(Frame, Option<Retired>), SpaceError> {
    let copy = mm::allocate_user_frame().ok_or(SpaceError::OutOfMemory)?;
    mm::copy_frame(copy, shared);
    match vmo.take_page(index, copy) {
        Ok(retired) => Ok((copy, Some(retired))),
        // Held since the count was read. A held page is this object's alone,
        // so the write goes to it, uncopied.
        Err(Kept::Held) => {
            let _ = mm::release_frame(copy);
            Ok((vmo.page(index).unwrap_or(shared), None))
        }
        Err(Kept::NoMemory) => {
            let _ = mm::release_frame(copy);
            Err(SpaceError::OutOfMemory)
        }
    }
}

/// Where a file mapping of `len` bytes goes: at a fixed address, or wherever
/// the map has room nearest the hint.
fn file_placement(
    map: &ferrix_vma::AddressSpace,
    place: FilePlace,
    len: u64,
) -> Result<PageRange, SpaceError> {
    let at = match place {
        FilePlace::Fixed(at) => at,
        FilePlace::Anywhere(hint) => map
            .find_free(len, PAGE_SIZE, hint)
            .ok_or(SpaceError::OutOfMemory)?,
    };
    if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
        return Err(SpaceError::NotUserRange(at));
    }
    PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)
}

/// Copies of the native-object set and the file table a forked child
/// inherits, made fallibly (finding F-23).
/// What [`inherited`] copies: the ceilings of the native objects and the
/// file table.
type Inherited = (BTreeMap<u64, Ceiling>, BTreeMap<u64, FileMapping>);

fn inherited(inner: &Inner) -> Result<Inherited, fallible::AllocError> {
    let mut native = BTreeMap::new();
    for (&id, &ceiling) in &inner.native {
        let _ = fallible::insert(&mut native, id, ceiling)?;
    }
    let mut files = BTreeMap::new();
    for (&id, mapping) in &inner.files {
        let _ = fallible::insert(&mut files, id, mapping.clone())?;
    }
    Ok((native, files))
}

/// A fork refused for memory once its child's root is taken: the root goes
/// back, since nothing else owns it yet.
fn unrooted(root: Frame) -> SpaceError {
    mm::deallocate_frames(root, 0);
    SpaceError::OutOfMemory
}

/// Attach a forked child to every object it names.
///
/// The child's copy of a mapping that may write its file counts as one more
/// first, under the child's lock, before anything can look -- and first, so
/// that the child's `Drop` lowers exactly what was raised if an attach runs
/// out of memory and the child is let go.
fn attach_child(child: &Arc<AddressSpace>) -> Result<(), SpaceError> {
    let inner = child.inner.lock();
    // A clone's copy of a region by handle counts as the original's does.
    // Never refused: an object with a writable region is one no seal took.
    for (&id, vmo) in &inner.objects {
        if writable_ceiling(&inner.native, id) {
            let _ = vmo.raise_writable_region();
        }
    }
    for (&id, mapping) in &inner.files {
        if mapping.may_write
            && let Some(vmo) = inner.objects.get(&id)
        {
            vmo.raise_shared_may_write();
        }
    }
    for (&id, vmo) in &inner.objects {
        let sharing = if shared_object(&inner.map, id) || inner.files.contains_key(&id) {
            Sharing::Shared
        } else {
            Sharing::Private
        };
        vmo.attach(Arc::downgrade(child), id, sharing)
            .map_err(|_| SpaceError::OutOfMemory)?;
    }
    for (&id, shadow) in &inner.shadows {
        shadow
            .attach(Arc::downgrade(child), id, Sharing::Private)
            .map_err(|_| SpaceError::OutOfMemory)?;
    }
    // Its resident counts, region by region: a refusal leaves views the
    // child's drop forgets (`user::rss`).
    for region in inner.map.iter() {
        child.count_in(&inner, region.range, region.backing, region.flags)?;
    }
    Ok(())
}

/// The error a refused map change is reported as: running out of memory is
/// said, and the rest is a range the map would not take.
fn map_error(error: ferrix_vma::VmaError) -> SpaceError {
    match error {
        ferrix_vma::VmaError::NoMemory => SpaceError::OutOfMemory,
        _ => SpaceError::BadRange,
    }
}

fn sharing_of(flags: VmaFlags) -> Sharing {
    if flags.shared {
        Sharing::Shared
    } else {
        Sharing::Private
    }
}

/// What one region counts in (`user::rss`): its object's view, and for a
/// private file mapping its shadow's, over the page indexes it covers.
struct Counting {
    /// The region's object.
    object: Arc<Vmo>,
    /// A private file mapping's shadow.
    shadow: Option<Arc<Vmo>>,
    /// The object's view.
    key: ViewKey,
    /// The first page index the region covers.
    first: u64,
    /// One past its last.
    end: u64,
}

/// What a region over `range` with `backing` and `flags` counts in, for a
/// space whose counts are `rss`: `None` for a device's region, or an id with
/// no object. Which count its object's pages go to: private memory is
/// anonymous, shared anonymous memory and a memory filesystem's file shared
/// memory, a disk file's pages a file's (`L.user.171`).
fn counting_of(
    rss: &Arc<Rss>,
    object: &Arc<Vmo>,
    shadow: Option<&Arc<Vmo>>,
    (range, backing, flags): (PageRange, Backing, VmaFlags),
) -> Option<Counting> {
    let (id, offset, file) = match backing {
        Backing::Anonymous { id, offset } => (id, offset, false),
        Backing::File { id, offset } => (id, offset, true),
        Backing::Device { .. } => return None,
    };
    let kind = if !file {
        if flags.shared {
            Kind::Shmem
        } else {
            Kind::Anon
        }
    } else if object.is_disk_file() {
        Kind::File
    } else {
        Kind::Shmem
    };
    let first = offset / PAGE_SIZE;
    Some(Counting {
        object: Arc::clone(object),
        shadow: shadow.filter(|_| file && !flags.shared).map(Arc::clone),
        key: ViewKey {
            rss: Arc::clone(rss),
            id,
            kind,
        },
        first,
        end: first.saturating_add(range.bytes() / PAGE_SIZE),
    })
}

/// [`counting_of`] for a region whose object, and shadow, are in `inner`'s
/// tables.
fn counting(
    inner: &Inner,
    rss: &Arc<Rss>,
    range: PageRange,
    backing: Backing,
    flags: VmaFlags,
) -> Option<Counting> {
    let id = match backing {
        Backing::Anonymous { id, .. } | Backing::File { id, .. } => id,
        Backing::Device { .. } => return None,
    };
    let object = inner.objects.get(&id)?;
    counting_of(rss, object, inner.shadows.get(&id), (range, backing, flags))
}

impl Counting {
    /// The shadow's view: the same space and id, anonymous.
    fn shadow_key(&self) -> ViewKey {
        ViewKey {
            kind: Kind::Anon,
            ..self.key.clone()
        }
    }

    /// Count the region in: the file's view linked to its shadow first, for
    /// a private file mapping, then each view's cover raised.
    fn raise(&self) -> Result<(), SpaceError> {
        if let Some(shadow) = &self.shadow {
            self.object
                .link_shadow(shadow, &self.key)
                .map_err(|_| SpaceError::OutOfMemory)?;
        }
        self.object
            .cover(&self.key, self.first, self.end, true)
            .map_err(|_| SpaceError::OutOfMemory)?;
        if let Some(shadow) = &self.shadow
            && shadow
                .cover(&self.shadow_key(), self.first, self.end, true)
                .is_err()
        {
            // The exact undo, in the room the raise had for it.
            let _ = self.object.cover(&self.key, self.first, self.end, false);
            return Err(SpaceError::OutOfMemory);
        }
        Ok(())
    }

    /// Count the region out: its room was had by [`Counting::reserve_lower`],
    /// or by the raise it undoes.
    fn lower(&self) {
        let _lowered = self
            .object
            .cover(&self.key, self.first, self.end, false)
            .and_then(|()| {
                self.shadow.as_ref().map_or(Ok(()), |shadow| {
                    shadow.cover(&self.shadow_key(), self.first, self.end, false)
                })
            });
        #[cfg(ferrix_checks = "on")]
        if let Err(error) = _lowered {
            crate::panic::fatal!(
                crate::panic::catalog::RSS_COUNT_BROKEN,
                "kept resident counts: a region could not be counted out: {error:?}"
            );
        }
    }

    /// Which view this counts in, as a key to group by.
    fn view_order(&self) -> (usize, u64, u8) {
        let object = Arc::as_ptr(&self.object) as usize;
        (object, self.key.id, self.key.kind as u8)
    }

    /// Room for `changes` lowers of parts of the region.
    fn reserve_lower(&self, changes: usize) -> Result<(), SpaceError> {
        self.object
            .reserve_lowers(&self.key, changes)
            .map_err(|_| SpaceError::OutOfMemory)?;
        if let Some(shadow) = &self.shadow {
            shadow
                .reserve_lowers(&self.shadow_key(), changes)
                .map_err(|_| SpaceError::OutOfMemory)?;
        }
        Ok(())
    }
}

/// Whether a region with `backing` and `flags` is a private file mapping,
/// which [`AddressSpace::add_region`] does not count: its shadow is not in
/// the tables yet, and [`AddressSpace::map_file`] counts it once it is.
fn counted_later(backing: Backing, flags: VmaFlags) -> bool {
    matches!(backing, Backing::File { .. }) && !flags.shared
}

impl AddressSpace {
    /// How many regions the map holds.
    pub fn region_count(&self) -> usize {
        self.inner.lock().map.region_count()
    }

    /// The object this space knows as `id`, if it has one.
    pub fn object(&self, id: u64) -> Option<Arc<Vmo>> {
        self.inner.lock().objects.get(&id).map(Arc::clone)
    }

    /// The object a shared region maps at `address`, and the byte offset of
    /// `address` in it: what names the same byte in every space that maps the
    /// object, as a shared futex's key must. `None` for an address no region
    /// covers, a private region -- whose pages are this space's alone, even
    /// while a `fork` shares them copy-on-write -- and a device region, which
    /// has no object.
    pub(crate) fn shared_object_at(&self, address: u64) -> Option<(Arc<Vmo>, u64)> {
        let inner = self.inner.lock();
        let region = inner.map.find(address)?;
        if !region.flags.shared {
            return None;
        }
        let (id, offset) = match region.backing {
            Backing::Anonymous { id, offset } | Backing::File { id, offset } => (id, offset),
            Backing::Device { .. } => return None,
        };
        let into_region = address.checked_sub(region.range.start())?;
        let object = Arc::clone(inner.objects.get(&id)?);
        Some((object, offset.saturating_add(into_region)))
    }

    /// Map `len` bytes of fresh anonymous memory at `at`, and return the id of
    /// the object created for it.
    ///
    /// Nothing is committed: the pages arrive on first touch, through
    /// [`AddressSpace::fault`]. That is what makes a large `mmap` cheap, and
    /// it is why the region is inserted before any frame is allocated.
    ///
    /// # Errors
    ///
    /// [`SpaceError::NotUserRange`] outside the user half,
    /// [`SpaceError::BadRange`] if it overlaps or is malformed, and
    /// [`SpaceError::OutOfMemory`] if the object cannot be made.
    pub fn map_anonymous(&self, at: u64, len: u64, flags: VmaFlags) -> Result<u64, SpaceError> {
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        let mut inner = self.inner.lock();
        self.map_fresh(&mut inner, range, flags)
    }

    /// Give back the pages [`AddressSpace::place_beside`] took out of `vmo`'s window,
    /// with this space's lock let go: no translation of them is left here
    /// (the unmap that left them finished its shootdown under the layout
    /// lock the join held, and no region has named the window since), and
    /// no other space names the object, so the retire skips this space
    /// (`L.user.243`, `docs/ANON-MERGE.md` §5.4).
    fn retire_window(&self, vmo: &Vmo, retired: Retired) {
        vmo.retire(
            retired,
            Some(Own {
                space: self,
                shootdown: None,
            }),
        );
    }

    /// Map `range` with `flags` to a new anonymous object at offset 0, under
    /// this space's lock (`inner`), and return the object's id.
    fn map_fresh(
        &self,
        inner: &mut Inner,
        range: PageRange,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        let pages = range.bytes().div_ceil(PAGE_SIZE);
        let vmo = Vmo::new_anonymous(pages).map_err(|_| SpaceError::OutOfMemory)?;
        self.add_region(
            inner,
            NewRegion {
                id,
                range,
                flags,
                backing: Backing::Anonymous { id, offset: 0 },
                sharing: sharing_of(flags),
            },
            vmo,
        )?;
        Ok(id)
    }

    /// Make a private anonymous mapping of `range` with `flags` in a
    /// neighbour's object (`L.user.242`, `docs/ANON-MERGE.md` §5.3), under
    /// this space's lock (`inner`) and its layout lock, which only
    /// [`Layout`]'s methods, holding it, call this with.
    ///
    /// The left neighbour if it qualifies, else the right; `None`, nothing
    /// changed, when neither does or the chosen one's window is declined
    /// ([`Vmo::join_window`]), with no try of the other side. Otherwise the
    /// object, the mapping's outcome as its id, and the pages taken out of
    /// the window, which the caller retires once this lock is let go: a
    /// refused count-in or insert leaves them taken all the same.
    fn place_beside(
        &self,
        inner: &mut Inner,
        range: PageRange,
        flags: VmaFlags,
    ) -> Option<(Arc<Vmo>, Result<u64, SpaceError>, Retired)> {
        if flags.shared || self.emptying.load(Ordering::Acquire) || self.is_region() {
            return None;
        }
        #[cfg(ferrix_personality = "user")]
        if self.is_window() {
            return None;
        }
        // Only a free range: a page of the window is taken before the map's
        // insert would refuse an overlap, and `brk` and a `MAP_FIXED` whose
        // unmap failed ask over what is mapped (ledger 9307 B1).
        if !range_free(inner, range) {
            return None;
        }
        let len = range.bytes();
        let (id, offset) = beside_of(inner, range)?;
        let vmo = Arc::clone(inner.objects.get(&id)?);
        let retired = vmo.join_window(offset / PAGE_SIZE, len / PAGE_SIZE)?;
        let backing = Backing::Anonymous { id, offset };
        let placed = self.count_in(inner, range, backing, flags).and_then(|()| {
            // FALLIBLE: the map's insert refuses with `VmaError::NoMemory`.
            let inserted = inner.map.insert(range, flags, backing).map_err(map_error);
            if inserted.is_err() {
                self.count_out(inner, range, backing, flags);
            }
            inserted.map(|()| id)
        });
        Some((vmo, placed, retired))
    }

    /// The resident set, split as Linux's `RssAnon`, `RssFile` and
    /// `RssShmem` split it (`L.user.171`), as the objects this space maps
    /// keep it (`user::rss`, `L.user.236`): three loads, no lock taken and
    /// nothing walked, whatever the regions and pages (`L.user.237`).
    pub fn resident(&self) -> Resident {
        self.rss.read()
    }

    /// Count the region over `range` in, under this space's lock (`inner`).
    fn count_in(
        &self,
        inner: &Inner,
        range: PageRange,
        backing: Backing,
        flags: VmaFlags,
    ) -> Result<(), SpaceError> {
        counting(inner, &self.rss, range, backing, flags)
            .map_or(Ok(()), |counting| counting.raise())
    }

    /// Count the region over `range` out, under this space's lock: the room
    /// for it had by [`AddressSpace::room_to_count_out`] or by its own raise.
    fn count_out(&self, inner: &Inner, range: PageRange, backing: Backing, flags: VmaFlags) {
        if let Some(counting) = counting(inner, &self.rss, range, backing, flags) {
            counting.lower();
        }
    }

    /// Count out every region `removed` names. Kept out of `unmap`'s frame,
    /// which is on a task's deepest path (`check-stack-depth`).
    #[inline(never)]
    fn count_out_removed(&self, inner: &Inner, removed: &[Unmapping]) {
        for unmapping in removed {
            self.count_out(inner, unmapping.range, unmapping.backing, unmapping.flags);
        }
    }

    /// Room for every region `ranges` reach to be counted out, in part or
    /// whole, and for `placement`'s exact undo, before a removal from the
    /// map that cannot be refused after it. A binary search for each range's
    /// first region, then each region reached; the room is had once per view
    /// for all of that view's lowers together (`Frames::reserve_lowers`).
    #[inline(never)]
    fn room_to_count_out(
        &self,
        inner: &Inner,
        ranges: &[PageRange],
        placement: Option<(PageRange, Backing, VmaFlags)>,
    ) -> Result<(), SpaceError> {
        let oom = |_| SpaceError::OutOfMemory;
        let mut reached: Vec<Counting> = Vec::new();
        let regions = inner.map.iter().as_slice();
        for &range in ranges {
            let from = regions.partition_point(|region| region.range.end() <= range.start());
            for region in regions
                .get(from..)
                .unwrap_or_default()
                .iter()
                .take_while(|region| region.range.start() < range.end())
            {
                if let Some(counting) =
                    counting(inner, &self.rss, region.range, region.backing, region.flags)
                {
                    fallible::try_push(&mut reached, counting).map_err(oom)?;
                }
            }
        }
        if let Some(counting) = placement
            .and_then(|(range, backing, flags)| counting(inner, &self.rss, range, backing, flags))
        {
            fallible::try_push(&mut reached, counting).map_err(oom)?;
        }
        // Not core's `sort_unstable`, which recurses: one entry per region
        // reached, and on an exit's unmap that is every region the process
        // has (F-112, `L.user.245`).
        ferrix_fallible::sort_flat_by_key(&mut reached, Counting::view_order);
        for group in reached.chunk_by(|one, other| one.view_order() == other.view_order()) {
            if let Some(counting) = group.first() {
                counting.reserve_lower(group.len())?;
            }
        }
        Ok(())
    }

    /// Map `region`, naming `vmo` by its id: the region in the map, this
    /// space among the object's mappers, and the object in the table -- all
    /// three or, when memory runs out part-way, none (finding F-23).
    fn add_region(
        &self,
        inner: &mut Inner,
        region: NewRegion,
        vmo: Arc<Vmo>,
    ) -> Result<(), SpaceError> {
        let NewRegion {
            id,
            range,
            flags,
            backing,
            sharing,
        } = region;
        // Room in the object table first; the rest undoes itself below.
        let held = fallible::reserve().map_err(|_| SpaceError::OutOfMemory)?;
        vmo.attach(self.me.clone(), id, sharing)
            .map_err(|_| SpaceError::OutOfMemory)?;
        // Counted in before the map takes it, so that a refusal of either is
        // undone exactly; a private file mapping is counted by `map_file`.
        let counted = (!counted_later(backing, flags))
            .then(|| counting_of(&self.rss, &vmo, None, (range, backing, flags)))
            .flatten();
        if let Some(counting) = &counted
            && let Err(why) = counting.raise()
        {
            vmo.detach(self, id);
            return Err(why);
        }
        // FALLIBLE: the map's insert refuses with `VmaError::NoMemory`.
        if let Err(error) = inner.map.insert(range, flags, backing) {
            if let Some(counting) = &counted {
                counting.lower();
            }
            vmo.detach(self, id);
            return Err(map_error(error));
        }
        let _ = fallible::insert_held(&held, &mut inner.objects, id, vmo);
        Ok(())
    }

    /// Undo [`AddressSpace::add_region`] for `id` over `range`, for a mapping
    /// whose later bookkeeping could not be done: out of the table, off the
    /// object's mappers, out of the map. The region was just inserted whole,
    /// so taking it out splits nothing and needs no memory.
    fn abandon_region(&self, inner: &mut Inner, id: u64, range: PageRange) {
        // Counted out as `add_region` counted it in, while the object is
        // still in the table: its exact undo, in the room the raise had.
        if let Some(region) = inner.map.find(range.start()).copied()
            && !counted_later(region.backing, region.flags)
        {
            let into = range.start() - region.range.start();
            let backing = match region.backing {
                Backing::Anonymous { id, offset } => Backing::Anonymous {
                    id,
                    offset: offset.saturating_add(into),
                },
                Backing::File { id, offset } => Backing::File {
                    id,
                    offset: offset.saturating_add(into),
                },
                device @ Backing::Device { .. } => device,
            };
            self.count_out(inner, range, backing, region.flags);
        }
        if let Some(vmo) = inner.objects.remove(&id) {
            vmo.forget_views(&self.rss, Some(id));
            vmo.detach(self, id);
        }
        let _ = inner.native.remove(&id);
        let _ = inner.map.remove_quietly(range);
    }

    /// Take the write permission away from this space's translations of
    /// every copy-on-write region, leaving them mapped, for `fork`: the pages
    /// its shootdown must cover, which is the whole space once any page was
    /// writable. Linux's `copy_present_ptes` write-protects the parent's
    /// entry the same way, with one `flush_tlb_mm` at the end of `dup_mmap`
    /// (`mm/memory.c`, `mm/mmap.c`).
    ///
    /// A range that would cut a block is taken down instead, as before: a
    /// user region is mapped in pages, so that is a table this space does not
    /// expect, and taking it down is what was always correct.
    fn protect_copied_on_write(&self, map: &ferrix_vma::AddressSpace) -> TlbPages {
        let mut pages = TlbPages::new();
        for region in map.iter().filter(|region| region.cow) {
            match mm::write_protect_in(
                self.root * PAGE_SIZE,
                region.range.start(),
                region.range.bytes(),
            ) {
                Ok(0) => {}
                Ok(_) => pages.everything(),
                Err(_) => self.unmap_range(region.range, &mut pages),
            }
        }
        pages
    }

    /// A second address space holding everything this one does, shared
    /// copy-on-write: the memory half of `fork`.
    ///
    /// Nothing is copied. Every private writable region is marked
    /// copy-on-write in *both* spaces — `src/lib/kernel/vma`'s `clone_for_fork` does the
    /// marking, and it must be both, because the parent's own writes have to
    /// stop reaching pages the child can now see. Each private object is
    /// cloned page-list and all, with a reference taken on every committed
    /// frame, so a write on either side copies the page into that side's own
    /// object and leaves the other's alone. A `MAP_SHARED` object is not
    /// cloned but shared, since writes through it are meant to be visible.
    ///
    /// # Why the parent's mappings are write-protected
    ///
    /// Marking the map is not enough. The parent's page tables still hold
    /// *writable* translations to the pages it has just agreed to share, and a
    /// write through one of those would never reach [`AddressSpace::fault`] at
    /// all — it would land in a page the child can read. So every translation
    /// in a region now marked copy-on-write loses its write permission in
    /// place ([`mm::write_protect_in`]), the processors in its set are told,
    /// and the next write faults and copies because `cow` is set. A read
    /// still hits.
    ///
    /// Until 2026-11 the translations were taken down instead, which cost a
    /// fault per page read afterwards, and a walk from the root and a prune
    /// per page under this lock: a 5 GB game's fork held it for seconds, with
    /// its own faulting threads spinning on it on every other processor
    /// (`docs/ROBUSTNESS.md` R5). Write-protecting is what Linux's
    /// `copy_present_ptes` does to the source entry (`mm/memory.c`). Taking a
    /// permission away needs no break-before-make on Arm, and the one
    /// whole-space shootdown below covers every page, as Linux's
    /// `flush_tlb_mm` at the end of `dup_mmap` (`mm/mmap.c`) does. Every
    /// fault path already handles a write to a present read-only page of a
    /// copy-on-write region: it is the state a read after the old unmapping
    /// left.
    ///
    /// # How the child is attached
    ///
    /// Every object the child names — the shared ones and the forked copies —
    /// is attached to it right after the child's `Arc` is built, before the
    /// child is returned. Not inside `Arc::new_cyclic`: a weak reference to a
    /// space still being built does not upgrade, and an object pruning dead
    /// entries in that window would drop the child's. Nothing is lost by
    /// waiting: until it is returned the child's tables are empty and nothing
    /// can fault into them, so an object that takes a page away meanwhile has
    /// nothing in the child to forget.
    ///
    /// Device regions are shared, not copied: they own no frames, and
    /// `ferrix_vma`'s `needs_cow` never marks them copy-on-write, so a child
    /// that touches one faults in the same registers its parent reaches.
    ///
    /// # Errors
    ///
    /// [`SpaceError::OutOfMemory`] if there is no frame for the child's root,
    /// and [`SpaceError::Backing`] if an object cannot take a reference on one
    /// of its pages. Both leave the parent usable: the child's root is freed,
    /// and a map marked copy-on-write with nobody to share with merely costs
    /// the parent a fault per page that then declines to copy anything,
    /// because the refcount says it is the only holder.
    pub fn fork(&self) -> Result<Arc<AddressSpace>, SpaceError> {
        let mut inner = self.inner.lock();

        // The child's root first, because it is the step most likely to fail
        // and the only one that fails without leaving a trace.
        let root = mm::allocate_frames(0).ok_or(SpaceError::OutOfMemory)?;
        mm::zero_frame(root);
        arch::prepare_user_root(root * PAGE_SIZE);

        // Then the objects, while the map is still untouched, so that a
        // failure here has changed nothing the parent can observe.
        let mut objects = BTreeMap::new();
        for (&id, vmo) in &inner.objects {
            // A file's own object is shared whatever its regions say: a
            // private file mapping writes into its shadow, never here.
            let forked = if shared_object(&inner.map, id) || inner.files.contains_key(&id) {
                Arc::clone(vmo)
            } else {
                match vmo.fork() {
                    Ok(forked) => forked,
                    Err(why) => {
                        mm::deallocate_frames(root, 0);
                        return Err(SpaceError::Backing(why));
                    }
                }
            };
            if fallible::insert(&mut objects, id, forked).is_err() {
                mm::deallocate_frames(root, 0);
                return Err(SpaceError::OutOfMemory);
            }
        }
        // Each private file mapping's shadow copies on write, as private
        // anonymous memory does: the child gets an object of its own.
        let mut shadows = BTreeMap::new();
        for (&id, shadow) in &inner.shadows {
            let recorded = match shadow.fork() {
                Ok(forked) => {
                    fallible::insert(&mut shadows, id, forked).map_err(|_| SpaceError::OutOfMemory)
                }
                Err(why) => Err(SpaceError::Backing(why)),
            };
            if let Err(why) = recorded {
                mm::deallocate_frames(root, 0);
                return Err(why);
            }
        }
        // And the tables the child inherits as they are, copied fallibly.
        let Ok((native, files)) = inherited(&inner) else {
            mm::deallocate_frames(root, 0);
            return Err(SpaceError::OutOfMemory);
        };

        // The record of what ran, as the source has it (K14.6, K14.7).
        let images = match inner.images.as_deref() {
            Some(images) => match fallible::try_box(images.clone()) {
                Ok(copy) => Some(copy),
                Err(_) => {
                    mm::deallocate_frames(root, 0);
                    return Err(SpaceError::OutOfMemory);
                }
            },
            None => None,
        };

        // Only now the parent's map is marked and copied, and its writable
        // translations to the shared pages taken down.
        let Ok(map) = inner.map.clone_for_fork() else {
            // Nothing is marked yet; the forked objects go as this returns.
            mm::deallocate_frames(root, 0);
            return Err(SpaceError::OutOfMemory);
        };
        let mut pages = self.protect_copied_on_write(&inner.map);

        // Read before the lock goes, because the child inherits them. A
        // native object is shared, so the child names the same one.
        let (next_id, apart) = (inner.next_id, inner.apart);

        // The parent's writable translations to the shared pages are out of
        // its tables, but may still be in the TLB of a processor in its set —
        // and a write through one of those would reach a page the child can
        // read, which is the whole thing fork just promised would not happen.
        let cpus = self.begin_shootdown(&inner);
        drop(inner);
        self.shoot(&cpus, &mut pages);

        let Ok(rss) = fallible::try_arc(Rss::new()) else {
            return Err(unrooted(root));
        };
        let child = fallible::try_arc_cyclic(|me| {
            AddressSpace::assemble(
                root,
                me,
                Inner {
                    map,
                    objects,
                    files,
                    shadows,
                    // Continued rather than restarted, so that an id means the
                    // same object in a parent and a child for as long as they
                    // share one. Two spaces may hand out the same id afterwards,
                    // which is fine: an id is only ever looked up in its own
                    // space's table.
                    next_id,
                    native,
                    images,
                    apart,
                },
                rss,
            )
        })
        // Refused before the closure ran, so nothing owns the root but this:
        // it goes back here, and the rest the closure captured goes as it
        // drops. The parent's marks only make it copy on write what it could
        // have written, which is harmless.
        .map_err(|_| unrooted(root))?;
        attach_child(&child)?;
        if self.is_measured() {
            child.measure();
        }
        // A clone carries what was done to its source, and starts unstarted.
        let carried = self.reach.load(Ordering::Acquire) & (REACH_TAINTED | REACH_UNLISTED);
        let _ = child.reach.fetch_or(carried, Ordering::AcqRel);
        Ok(child)
    }

    /// Resolve a fault at `address`, and let the faulting instruction retry.
    ///
    /// This is demand paging, and stage 3 already proved the mechanism on the
    /// kernel's own tables: a fault arrives, the handler maps the faulting
    /// address, the instruction runs again. What is new is that the decision
    /// of *what* to map comes from the region map rather than from a fixed
    /// window — find the region, ask its object for the page, install it with
    /// the region's permissions.
    ///
    /// # Errors
    ///
    /// [`SpaceError::NotMapped`] if no region covers the address, which is the
    /// segmentation fault, and [`SpaceError::Refused`] if the region does not
    /// permit the access, which is the other one.
    pub fn fault(&self, address: u64, access: Access) -> Result<(), SpaceError> {
        self.fault_with(address, access, FaultMode::Copy)
    }

    /// [`AddressSpace::fault`], saying who faulted: a program's own fault,
    /// from the trap path, or the kernel's touch of a page, a copy's
    /// (`docs/microkernel/K3.md` §2.4). The difference is a pager-backed
    /// object's: a copy into one that says `PAGER_VMO_NO_COPY_WAIT` never
    /// waits for its pager.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::fault`]; and [`SpaceError::Unreadable`] for a page
    /// a pager failed, did not answer within its bound, or will never answer
    /// (`SIGBUS`), or that a copy may not wait for.
    pub(crate) fn fault_with(
        &self,
        address: u64,
        access: Access,
        mode: FaultMode,
    ) -> Result<(), SpaceError> {
        fault_requested();
        // A page of a file on a disk that reclaim takes between its fill and
        // its use is filled again; it is never used as zeros (`user::cache`).
        // So is a pager's page a write-back took the write right from.
        for _ in 0..REFILLS {
            self.fill_file_page(address)?;
            if !self.ask_pager(address, access, mode)? {
                // The process is ending: back to user mode with nothing
                // installed, where it leaves.
                return Ok(());
            }
            match self.resolve(address, access) {
                Err(SpaceError::Evicted) => {}
                done => return done,
            }
        }
        Err(SpaceError::Unreadable(address))
    }

    /// Before a fault on a pager-backed object: have its pager supply the
    /// page if it is absent, and for a write to a Clean or awaiting-clean page
    /// have it Dirty, asking the pager first if the object traps dirty
    /// (`docs/microkernel/K3.md` §2.2 to §2.5). With no lock held, between
    /// this space's lock, under which the region is looked up, and the one
    /// [`AddressSpace::resolve`] takes. `Ok(false)` when the process is
    /// ending and a program's fault should go back to user mode as it is.
    ///
    /// # Errors
    ///
    /// [`SpaceError::Unreadable`] for a page the pager failed, timed out on
    /// or will never answer, or that a copy may not wait for;
    /// [`SpaceError::OutOfMemory`] for the request's own record.
    fn ask_pager(&self, address: u64, access: Access, mode: FaultMode) -> Result<bool, SpaceError> {
        let Some((vmo, index, writes)) = self.paged_object_at(address, access) else {
            return Ok(true);
        };
        match ask(&vmo, index, writes, mode) {
            Ok(()) => Ok(true),
            Err(Refusal::Interrupted) if mode == FaultMode::Program => Ok(false),
            Err(Refusal::NoMemory) => Err(SpaceError::OutOfMemory),
            Err(_) => Err(SpaceError::Unreadable(address)),
        }
    }

    /// The pager-backed object a shared or anonymous region maps at
    /// `address`, the page's index in it, and whether `access` writes a
    /// region that may be written; `None` for any other region or none.
    fn paged_object_at(&self, address: u64, access: Access) -> Option<(Arc<Vmo>, u64, bool)> {
        let inner = self.inner.lock();
        let region = inner.map.find(address)?;
        let (id, offset) = match region.backing {
            Backing::Anonymous { id, offset } => (id, offset),
            Backing::File { id, offset } if region.flags.shared => (id, offset),
            _ => return None,
        };
        let vmo = inner.objects.get(&id).filter(|vmo| vmo.tracks_dirty())?;
        let into = (address & !(PAGE_SIZE - 1)).saturating_sub(region.range.start());
        Some((
            Arc::clone(vmo),
            offset.saturating_add(into) / PAGE_SIZE,
            access.write && region.flags.write,
        ))
    }

    /// [`AddressSpace::fault`] once a file's page is in its object: find the
    /// region, ask its object for the page, install it.
    fn resolve(&self, address: u64, access: Access) -> Result<(), SpaceError> {
        let inner = self.inner.lock();

        let region = permitted(&inner, address, access)?;

        let page = address & !(PAGE_SIZE - 1);
        let into_region = page.saturating_sub(region.range.start());

        let (id, offset, file) = match region.backing {
            Backing::Anonymous { id, offset } => (id, offset.saturating_add(into_region), false),
            // A shared file mapping maps the file's own pages: the object is
            // the file's VMO, the one `read` copies out of.
            Backing::File { id, offset } if region.flags.shared => {
                (id, offset.saturating_add(into_region), true)
            }
            // A device region has no object: its page is the device's own.
            device @ Backing::Device { .. } => {
                return self.fault_device(page, into_region, region.flags, device);
            }
            // A private file mapping reads the file's pages until it writes
            // one, and writes into a shadow object of its own.
            Backing::File { .. } => {
                drop(inner);
                return self.fault_private_file(address, access);
            }
        };

        let vmo = Arc::clone(
            inner
                .objects
                .get(&id)
                .ok_or(SpaceError::NotMapped(address))?,
        );
        let index = offset / PAGE_SIZE;

        // Copy-on-write, and **this branch is above the present-page check on
        // purpose**. A write to a page that is present but deliberately
        // read-only is exactly the fault that has to copy; returning early
        // because the page translates would send the instruction back to fault
        // forever. The region's own `flags.write` was already checked above,
        // so reaching here means the process is entitled to write and the
        // read-only entry is the kernel's device rather than the region's
        // permission.
        if access.write && region.cow {
            return self.write_copy_on_write(inner, &region, &vmo, (id, index), page);
        }

        // A pager-backed object's Clean page is never mapped writable
        // (`docs/microkernel/K3.md` §2.5). A write reaches here once the ask
        // of the pager made its page Dirty; a write-back that began since
        // sends it round again, and a read-only translation the page had
        // while Clean is replaced, with its shootdown, as a copy-on-write
        // page's is.
        if let Some(frame) = self.read_only_paged(&vmo, index, (page, address), access)? {
            let flags = writable_flags(&region, &vmo);
            return self.install_page(inner, placed(id, index, page, frame), flags, true);
        }

        // Already present, and the fault was spurious: another processor
        // resolved this same page between the fault and this lock, or the
        // faulting processor walked a stale TLB entry. Both happen, and
        // neither is an error -- the instruction retries and succeeds.
        //
        // Safe to return early only because the copy-on-write case above has
        // already been taken: every page that reaches here carries its own
        // region's permissions, so a fault on a present one asked for nothing
        // the mapping does not already grant.
        if master_translate!(self, self.root * PAGE_SIZE, page).is_some() {
            return Ok(());
        }

        // A pager-backed object's absent page is its pager's, never zeros.
        let frame = commit_page(&vmo, index, file, address)?;

        // A copy-on-write region is installed read-only however writable the
        // region is, so that the *next* write faults here again and can copy;
        // and so is a pager-backed object's page that is not Dirty, so that
        // its first write faults and is seen.
        let writable = writable_in(&region, &vmo, index);
        let flags = MapFlags {
            read: true,
            write: writable,
            execute: region.flags.execute,
            user: true,
            global: false,
            device: false,
            // Past the caches if a device shares it (`Vmo::make_coherent`).
            uncached: vmo.is_coherent(),
            // Write-combining in every user mapping if a chardev driver made
            // it so (`Vmo::make_combining`, `L.user.133`).
            write_combining: vmo.is_combining(),
        };

        mm::map_in(
            self.root * PAGE_SIZE,
            page,
            frame * PAGE_SIZE,
            PAGE_SIZE,
            flags,
        )
        .map_err(|_| SpaceError::OutOfMemory)?;

        drop(inner);
        Ok(())
    }

    /// [`AddressSpace::resolve`]'s write to a copy-on-write region's page
    /// `index` of `vmo`, named `id`, at `page`, with this space's lock held as
    /// `inner`: the copy, or the sole holder's reuse in place (L.user.229).
    fn write_copy_on_write<G: Deref<Target = Inner>>(
        &self,
        inner: G,
        region: &Vma,
        vmo: &Arc<Vmo>,
        (id, index): (u64, u64),
        page: u64,
    ) -> Result<(), SpaceError> {
        let shared = vmo.commit(index).map_err(SpaceError::Backing)?;

        // The one real decision. A page nobody else holds any more needs
        // no copy: the other side has already copied it, or unmapped, or
        // exited, and copying would allocate a frame in order to duplicate
        // data this space is the sole owner of.
        let (frame, retired) = if mm::frame_references(shared) > 1 {
            copy_on_write(vmo, index, shared)?
        } else {
            (shared, None)
        };

        // Held alone and already mapped, read-only, to that frame -- what
        // a fork's write-protect leaves once the child has gone: made
        // writable where it is, as Linux's `wp_page_reuse` does
        // (`mm/memory.c`), with no shootdown (`docs/ROBUSTNESS.md` R5,
        // §4.6; ledger 8299). A storm of these after a big fork was a
        // shootdown each, all queued on the one shootdown turn.
        //
        // Why no other processor need be told. The frame does not change
        // and nothing is given back: this space is its only holder, so it
        // can go back to the allocator only through this space's own
        // unmap, forget or teardown, each of which takes the translation
        // down and shoots it down first (`give_back`, `forget_in`, and
        // `Drop`'s rule that no processor is left in the set). A processor
        // that cached the read-only entry can therefore reach only this
        // frame, read-only, and a write through it faults. That fault
        // comes back here, because a region's `cow` is only ever set,
        // never cleared (`ferrix_vma`); `allow_write_in` answers yes for a
        // page already writable; and the faulting processor's own entry
        // is invalidated below, under the lock, so on that processor. On
        // x86-64 `invlpg` reaches the current PCID only, but a page fault
        // also drops the entry that faulted (Intel SDM vol. 3A, 4.10.4),
        // and a space that is not current here keeps a stale read-only
        // entry under its PCID that faults once more when it runs, which
        // ends the same way. On AArch64 and ARMv7-A `flush_tlb_page` is a
        // broadcast invalidation of the page for every ASID (`TLBI
        // VAAE1IS`, `TLBIMVAAIS`), so every core is told anyway; adding a
        // permission needs no break-before-make. Linux relies on the same
        // spurious fault (`fix_spurious_fault`). Only a leaf that decodes
        // to what the path below would install, less the write, is
        // reused (`reusable_in_place`).
        if retired.is_none()
            && master_translate!(self, self.root * PAGE_SIZE, page) == Some(frame * PAGE_SIZE)
            && reusable_in_place(
                master_flags!(self, self.root * PAGE_SIZE, page),
                region.flags.execute,
            )
            && mm::allow_write_in(self.root * PAGE_SIZE, page)
        {
            // Under the lock, so on the processor that faulted.
            arch::flush_tlb_page(page);
            drop(inner);
            return Ok(());
        }

        // Held alone and not mapped at `page` at all: a page this fault
        // has just committed, or one whose translation came down earlier
        // (an `mprotect` takes a range's translations down and leaves the
        // pages to fault back in). Mapped as any first touch is, and as
        // `write_shadow_copy_on_write` already maps a shadow page held
        // alone with no translation (`install_page` with no `replace`):
        // nothing is replaced, so there is nothing to take down and
        // nobody to tell (L.user.235; ledger 8634).
        //
        // No processor holds a translation of `page` that this could
        // disagree with. An absent entry is never cached. Whatever took
        // the page's last translation down counted a shootdown begun and
        // pending under the lock hold that took it down, and that
        // shootdown reaches the page whether or not it is mapped again
        // meanwhile; until it returns the frame the old entry reaches is
        // kept. A space that is not current on a processor flushes what
        // it kept under its PCID when it next runs there, by the count
        // begun. On AArch64 and ARMv7-A a permission fault does not drop
        // the entry that faulted, so a processor that faulted through a
        // stale read-only entry while that shootdown is still pending
        // retries into it after this has mapped the page: its second
        // fault finds the leaf present and writable and ends in the
        // branch above, whose invalidation is broadcast, unless the
        // pending shootdown got there before it. Once, no loop.
        //
        // Every other translation of this object page in this space
        // still reaches this same frame, since the object did not change
        // the frame it names (a copy does, and takes the path below), and
        // so stays where it is.
        //
        // Without this, every such write walked every region of the space
        // for other translations (`forget_in`) and shot down a
        // translation that was never there, on a space of thousands of
        // regions as a Wine process is: Wine commits all its memory by
        // `mprotect` of a reserve. The figures are in
        // `docs/ROBUSTNESS.md` R5, step 1, item 5.
        if retired.is_none() && master_translate!(self, self.root * PAGE_SIZE, page).is_none() {
            let mapped = mm::map_in(
                self.root * PAGE_SIZE,
                page,
                frame * PAGE_SIZE,
                PAGE_SIZE,
                MapFlags {
                    execute: region.flags.execute,
                    ..MapFlags::USER_DATA
                },
            );
            drop(inner);
            return mapped.map_err(|_| SpaceError::OutOfMemory);
        }

        // `map_in` refuses to overwrite a live mapping, deliberately, so
        // the read-only entry comes down before the writable one goes in
        // -- and so does every other translation of this object page in
        // this space, since the object no longer names the frame they
        // reach.
        let mut pages = TlbPages::new();
        let _ = self.forget_in(&inner, id, &[(index, 1)], &mut pages);
        pages.add(page);
        let mapped = mm::map_in(
            self.root * PAGE_SIZE,
            page,
            frame * PAGE_SIZE,
            PAGE_SIZE,
            MapFlags {
                execute: region.flags.execute,
                ..MapFlags::USER_DATA
            },
        );

        // A translation that existed a moment ago has just been replaced
        // by a more permissive one, so every cached copy of the old one
        // has to go. **This is not optional and it is not a performance
        // matter**, twice over. The entry that was there says read-only,
        // and the instruction that faulted is about to retry its write: if
        // it finds the stale entry it faults again, arrives here again,
        // finds one holder and no copy to make, installs the same writable
        // entry again, and retries into the same stale entry — forever. And
        // the entry reaches the shared frame, whose reference is about to
        // be given back: a processor that kept it would be reading a page
        // the allocator may hand to anyone.
        //
        // Set read after the takedown, under the lock; shootdown after the
        // lock; the frame's reference only after the shootdown.
        let cpus = self.begin_shootdown(&inner);
        drop(inner);
        match retired {
            Some(retired) => vmo.retire(
                retired,
                Some(Own {
                    space: self,
                    shootdown: Some((cpus, pages)),
                }),
            ),
            None => self.shoot(&cpus, &mut pages),
        }
        mapped.map_err(|_| SpaceError::OutOfMemory)
    }

    /// For [`AddressSpace::resolve`]'s write to page `index` of `vmo`, a
    /// pager-backed object, at `page`: the frame to map writable in place of
    /// the read-only translation the page had while Clean, or `None` when it
    /// has none and the write goes on as any other. Called under this space's
    /// lock, which reads the translation.
    ///
    /// # Errors
    ///
    /// [`SpaceError::Evicted`] for a page that is not Dirty: a write-back
    /// began between the ask of the pager and here, and the fault asks again.
    fn read_only_paged(
        &self,
        vmo: &Vmo,
        index: u64,
        (page, address): (u64, u64),
        access: Access,
    ) -> Result<Option<Frame>, SpaceError> {
        if !access.write || !vmo.tracks_dirty() {
            return Ok(None);
        }
        if !vmo.writable_page(index) {
            return Err(SpaceError::Evicted);
        }
        if master_flags!(self, self.root * PAGE_SIZE, page).is_some_and(|flags| !flags.write) {
            return commit_page(vmo, index, true, address).map(Some);
        }
        Ok(None)
    }

    /// Before a fault on a file mapping: have the file's object hold the
    /// page, if the file is on a disk and nothing has read the page yet.
    ///
    /// Everything below commits an absent page of a file's object as zeros,
    /// which is right for tmpfs, whose object is the file, and wrong for a
    /// page cache, whose absent page is on the disk -- a shared mapping would
    /// show zeros, and a private one's first write would copy them over the
    /// file's data. The fill waits for the disk, so it runs here, between
    /// this space's lock and the one the fault takes: the region is looked up
    /// under the lock and the page filled after it is let go. A region
    /// unmapped in between costs a fill nobody maps, which the page cache
    /// keeps; a truncation in between is the fault's to refuse, as ever.
    ///
    /// # Errors
    ///
    /// [`SpaceError::Unreadable`] for a page the filesystem could not read,
    /// and [`SpaceError::OutOfMemory`] for frames to read it into.
    fn fill_file_page(&self, address: u64) -> Result<(), SpaceError> {
        self.fill_file_pages(address, false)
    }

    /// [`AddressSpace::fill_file_page`] for the page at `address` alone, with
    /// no read-ahead after it: for the loader, which writes zeros into a
    /// writable segment's last file page itself, and must read that page of
    /// the program and no other (`syscall/load.rs`, `zero_tails`). The write's
    /// own fault then finds the page in the file's object and reads nothing.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::fill_file_page`].
    pub fn fill_file_page_alone(&self, address: u64) -> Result<(), SpaceError> {
        self.fill_file_pages(address, true)
    }

    /// The body of [`AddressSpace::fill_file_page`] and
    /// [`AddressSpace::fill_file_page_alone`]: `alone` asks the file for the
    /// one page, without the read-ahead a fault's fill does.
    fn fill_file_pages(&self, address: u64, alone: bool) -> Result<(), SpaceError> {
        let (vmo, index) = {
            let inner = self.inner.lock();
            let Some(region) = inner.map.find(address) else {
                return Ok(());
            };
            let Backing::File { id, offset } = region.backing else {
                return Ok(());
            };
            let into_region = (address & !(PAGE_SIZE - 1)).saturating_sub(region.range.start());
            let Some(vmo) = inner.objects.get(&id).map(Arc::clone) else {
                return Ok(());
            };
            (vmo, offset.saturating_add(into_region) / PAGE_SIZE)
        };
        let filled = if alone {
            vmo.fill_one_page(index)
        } else {
            vmo.fill_for_fault(index)
        };
        match filled {
            Ok(()) => Ok(()),
            Err(ferrix_vfs::Errno::ENOMEM) => Err(SpaceError::OutOfMemory),
            Err(_) => Err(SpaceError::Unreadable(address)),
        }
    }

    /// [`AddressSpace::fault`] for a private file mapping.
    ///
    /// The region names two objects by one id: the file's own, attached
    /// shared, and its shadow, attached privately, which holds the pages this
    /// mapping has written. In order:
    ///
    /// * A page wholly past the file's end is refused before either object is
    ///   looked at, so a page the mapping copied before a truncation is
    ///   `SIGBUS` too, as on Linux.
    /// * A write lands only in the shadow. A page the shadow lacks is copied
    ///   into a fresh frame first: the file's page, or zeros for a hole, which
    ///   leaves the file's object untouched. A shadow page a `fork` left in two
    ///   spaces is copied again, as private anonymous memory is. **This is
    ///   decided before the check for a page already present, on purpose**:
    ///   the page present may be the file's, shown read-only, and returning
    ///   early would send the write back to fault on it forever.
    /// * A read shows the shadow's page if it has one, and otherwise the
    ///   file's, read-only however writable the region is, so that the first
    ///   write faults here.
    ///
    /// A file on a disk has had the page filled into its object already, by
    /// [`AddressSpace::fill_file_page`] before this was called, so the copy
    /// below copies the file's data rather than zeros.
    fn fault_private_file(&self, address: u64, access: Access) -> Result<(), SpaceError> {
        let inner = self.inner.lock();
        let region = *inner
            .map
            .find(address)
            .ok_or(SpaceError::NotMapped(address))?;
        if !permits(region.flags, access) {
            return Err(SpaceError::Refused(address));
        }
        let Backing::File { id, offset } = region.backing else {
            return Err(SpaceError::NotMapped(address));
        };
        let page = address & !(PAGE_SIZE - 1);
        let index = offset.saturating_add(page.saturating_sub(region.range.start())) / PAGE_SIZE;
        let (Some(file), Some(shadow)) = (
            inner.objects.get(&id).map(Arc::clone),
            inner.shadows.get(&id).map(Arc::clone),
        ) else {
            return Err(SpaceError::NotMapped(address));
        };
        if file.past_file_end(index) {
            return Err(SpaceError::PastEnd(address));
        }
        let present = master_translate!(self, self.root * PAGE_SIZE, page)
            .map(|physical| physical / PAGE_SIZE);
        let execute = region.flags.execute;
        let placed = |frame| Placement {
            id,
            index,
            page,
            frame,
        };

        let Some(copied) = shadow.page(index) else {
            if !access.write {
                if present.is_some() {
                    return Ok(());
                }
                let frame = commit_page(&file, index, true, address)?;
                let flags = typed(user_page(false, execute), &file);
                return self.install_page(inner, placed(frame), flags, false);
            }
            let frame = mm::allocate_user_frame().ok_or(SpaceError::OutOfMemory)?;
            // A snapshot. The file's page is read through the direct map
            // without its object's lock, so a write to the file landing
            // meanwhile may be partly in the copy -- which is Linux's
            // behaviour too: its private copy races a write to the file the
            // same way. A fresh frame, never a second reference to the file's:
            // a shared reference would make the file's next write copy on
            // write, and the file would stop being what its shared mappings
            // show.
            copy_or_zero(&file, index, frame)?;
            // The shadow lacked the page under this space's lock, and every
            // change to a shadow is made under it, so no page can have arrived
            // meanwhile: a frame that did not go in is one there was no memory
            // to record (finding F-23), and the fault ran out of memory as
            // surely as if the copy's own frame had been refused.
            if !shadow.insert_absent(index, frame) {
                let _ = mm::release_frame(frame);
                return Err(SpaceError::OutOfMemory);
            }
            let flags = user_page(region.flags.write, execute);
            return self.install_page(inner, placed(frame), flags, present.is_some());
        };

        if access.write && region.cow {
            return self.write_shadow_copy_on_write(
                inner,
                &shadow,
                placed(copied),
                present,
                execute,
            );
        }
        if present == Some(copied) {
            return Ok(());
        }
        let flags = user_page(region.flags.write && !region.cow, execute);
        self.install_page(inner, placed(copied), flags, present.is_some())
    }

    /// [`AddressSpace::fault_private_file`]'s write to a copy-on-write
    /// region's page whose copy `at.frame` the shadow holds, with this space's
    /// lock held as `inner` and `present` the frame the page translates to.
    ///
    /// A shadow page a fork left in another space too is copied again. One
    /// nobody else holds any more -- the other side copied its own, unmapped or
    /// exited -- is this space's alone, and is mapped writable in place of the
    /// read-only entry the fork or a read left: returning because a page is
    /// present would send the write back into that entry forever.
    fn write_shadow_copy_on_write<G: Deref<Target = Inner>>(
        &self,
        inner: G,
        shadow: &Arc<Vmo>,
        at: Placement,
        present: Option<Frame>,
        execute: bool,
    ) -> Result<(), SpaceError> {
        if mm::frame_references(at.frame) > 1 {
            return self.copy_shadow_page(inner, shadow, at, execute);
        }
        // Mapped to this very page: made writable where it is, with no
        // shootdown, for the reason `write_copy_on_write` gives (L.user.229).
        if present == Some(at.frame)
            && reusable_in_place(master_flags!(self, self.root * PAGE_SIZE, at.page), execute)
            && mm::allow_write_in(self.root * PAGE_SIZE, at.page)
        {
            // Under the lock, so on the processor that faulted.
            arch::flush_tlb_page(at.page);
            drop(inner);
            return Ok(());
        }
        let flags = user_page(true, execute);
        self.install_page(inner, at, flags, present.is_some())
    }

    /// Map `at.frame` at `at.page` with `flags`, and let `inner`, this space's
    /// lock, go. With `replace`, the translation already there -- the file's
    /// page that a copy now stands in for -- comes down first, with every
    /// other translation of that page of the id in this space, and every
    /// processor that may cache one is told before this returns.
    fn install_page<G: Deref<Target = Inner>>(
        &self,
        inner: G,
        at: Placement,
        flags: MapFlags,
        replace: bool,
    ) -> Result<(), SpaceError> {
        let mut pages = TlbPages::new();
        if replace {
            let _ = self.forget_in(&inner, at.id, &[(at.index, 1)], &mut pages);
            pages.add(at.page);
        }
        let mapped = mm::map_in(
            self.root * PAGE_SIZE,
            at.page,
            at.frame * PAGE_SIZE,
            PAGE_SIZE,
            flags,
        );
        if replace {
            let cpus = self.begin_shootdown(&inner);
            drop(inner);
            self.shoot(&cpus, &mut pages);
        } else {
            drop(inner);
        }
        mapped.map_err(|_| SpaceError::OutOfMemory)
    }

    /// Copy the shadow page `at.frame`, which a `fork` left in another space
    /// too, into a frame of this space's own, and map that writable: the
    /// copy-on-write of [`AddressSpace::fault`], on a private file mapping's
    /// shadow. The page the shadow gave up goes back once no processor can
    /// reach it.
    fn copy_shadow_page<G: Deref<Target = Inner>>(
        &self,
        inner: G,
        shadow: &Arc<Vmo>,
        at: Placement,
        execute: bool,
    ) -> Result<(), SpaceError> {
        let copy = mm::allocate_user_frame().ok_or(SpaceError::OutOfMemory)?;
        mm::copy_frame(copy, at.frame);
        let (frame, retired) = match shadow.take_page(at.index, copy) {
            Ok(retired) => (copy, Some(retired)),
            // Held since the count was read: the write goes to the held page.
            Err(Kept::Held) => {
                let _ = mm::release_frame(copy);
                (shadow.page(at.index).unwrap_or(at.frame), None)
            }
            Err(Kept::NoMemory) => {
                let _ = mm::release_frame(copy);
                return Err(SpaceError::OutOfMemory);
            }
        };
        let mut pages = TlbPages::new();
        let _ = self.forget_in(&inner, at.id, &[(at.index, 1)], &mut pages);
        pages.add(at.page);
        let mapped = mm::map_in(
            self.root * PAGE_SIZE,
            at.page,
            frame * PAGE_SIZE,
            PAGE_SIZE,
            user_page(true, execute),
        );
        let cpus = self.begin_shootdown(&inner);
        drop(inner);
        match retired {
            Some(retired) => shadow.retire(
                retired,
                Some(Own {
                    space: self,
                    shootdown: Some((cpus, pages)),
                }),
            ),
            None => self.shoot(&cpus, &mut pages),
        }
        mapped.map_err(|_| SpaceError::OutOfMemory)
    }

    /// Run `touch` on the direct-map address of the byte at `address`, with the
    /// page held where it is for as long as `touch` runs.
    ///
    /// What a copy to or from user memory goes through. Faulting the page in
    /// and then translating it is not enough on its own, because the lock goes
    /// between the two and again before the copy: a thread sharing this space
    /// can `munmap` the page in either gap, and the copy then reads or writes
    /// a frame that has been given back and handed to somebody else. So the
    /// translation is taken again under the lock, and `touch` runs before the
    /// lock goes. Every way a frame this space names is given back -- `unmap`,
    /// a copy-on-write replacement, `fork` re-sharing a page, and a VMO
    /// decommitting, replacing or moving a page through
    /// [`AddressSpace::forget_pages`] -- first takes its translation down under
    /// this same lock and releases the frame only after the shootdown that
    /// follows, so a translation found under it names a live frame until the
    /// lock goes.
    ///
    /// The page is faulted in again if what the lock finds does not do: gone
    /// since the fault, or, for a write, a copy-on-write page some other space
    /// still holds, which a `fork` in between would have left. The fault
    /// always makes progress on its own, so the retry ends unless another
    /// thread keeps undoing it.
    ///
    /// `touch` runs under a spin lock: it must not sleep, fault, take this
    /// space's lock, or take a VMO's lock, which would invert the order the
    /// module gives. A copy between the direct map and a kernel buffer does
    /// none of those.
    ///
    /// Only RAM is reached this way. A device region is refused before its
    /// page is faulted in (`copyable`), and the address `touch` is given
    /// comes from [`mm::direct_map_ram`], which answers for RAM alone (F-55).
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::fault`]; and [`SpaceError::Refused`] for a page of
    /// a device region, or one whose frame is not RAM.
    pub fn with_page<R>(
        &self,
        address: u64,
        access: Access,
        touch: impl FnOnce(u64) -> R,
    ) -> Result<R, SpaceError> {
        #[cfg(ferrix_personality = "user")]
        if self.is_window() {
            return self
                .with_placed(address, access, touch)?
                .ok_or(SpaceError::NotMapped(address));
        }
        loop {
            {
                let inner = self.inner.lock();
                let region = *inner
                    .map
                    .find(address)
                    .ok_or(SpaceError::NotMapped(address))?;
                if !permits(region.flags, access) || !copyable(&region) {
                    return Err(SpaceError::Refused(address));
                }
                if let Some(physical) = master_translate!(self, self.root * PAGE_SIZE, address)
                    && (!access.write
                        || writable_in_place(&inner, &region, address, physical / PAGE_SIZE))
                {
                    let virt = mm::direct_map_ram(physical).ok_or(SpaceError::Refused(address))?;
                    let answer = self.coherent_touch(address, virt, touch);
                    drop(inner);
                    return Ok(answer);
                }
            }
            // As the program's own fault would be, a page past its cgroup's
            // `memory.max` included (`object::oom`).
            crate::object::oom::fault(self, address, access)?;
        }
    }

    /// [`AddressSpace::with_page`] without the fault: `touch` runs only if the
    /// page is already there to be used as `access` asks, and `Ok(None)` says
    /// it would first have had to be faulted in -- not present yet, or, for a
    /// write, a copy-on-write page some other space still holds.
    ///
    /// For a caller holding a lock. Resolving a fault can copy a page and wait
    /// for every processor to drop the translation it replaced, and no lock may
    /// be held across that wait. This takes only the space's own lock, for as
    /// long as `touch` runs, and never waits for anything; the caller lets go
    /// of its lock, faults the page in, and asks again.
    ///
    /// # Errors
    ///
    /// [`SpaceError::NotMapped`] if no region covers the address, and
    /// [`SpaceError::Refused`] if the region does not permit the access, is a
    /// device's, or maps a frame that is not RAM, as for
    /// [`AddressSpace::with_page`].
    pub fn with_present_page<R>(
        &self,
        address: u64,
        access: Access,
        touch: impl FnOnce(u64) -> R,
    ) -> Result<Option<R>, SpaceError> {
        #[cfg(ferrix_personality = "user")]
        if self.is_window() {
            return self.with_placed(address, access, touch);
        }
        let inner = self.inner.lock();
        let region = *inner
            .map
            .find(address)
            .ok_or(SpaceError::NotMapped(address))?;
        if !permits(region.flags, access) || !copyable(&region) {
            return Err(SpaceError::Refused(address));
        }
        let Some(physical) = master_translate!(self, self.root * PAGE_SIZE, address) else {
            return Ok(None);
        };
        if access.write && !writable_in_place(&inner, &region, address, physical / PAGE_SIZE) {
            return Ok(None);
        }
        let virt = mm::direct_map_ram(physical).ok_or(SpaceError::Refused(address))?;
        let answer = self.coherent_touch(address, virt, touch);
        drop(inner);
        Ok(Some(answer))
    }

    /// Run `touch` on `virt`, the direct-map address of the user byte at
    /// `address`, coherently with a write-combining user mapping of its page
    /// (`L.user.133`): where this space maps the page write-combining, its
    /// lines are written back and invalidated (CLFLUSH, then MFENCE) before
    /// `touch`, so that it reads no stale line, and again after, so that no
    /// line it wrote is left to be evicted over a later write-combining
    /// store. Every other page is touched as it was. Under the space's lock,
    /// as `touch` is; the flush takes no lock and waits for nothing.
    fn coherent_touch<R>(&self, address: u64, virt: u64, touch: impl FnOnce(u64) -> R) -> R {
        let combining = master_flags!(self, self.root * PAGE_SIZE, address)
            .is_some_and(|flags| flags.write_combining && !flags.device);
        if !combining {
            return touch(virt);
        }
        let page = virt & !(PAGE_SIZE - 1);
        arch::flush_cache_lines(page, PAGE_SIZE);
        let answer = touch(virt);
        arch::flush_cache_lines(page, PAGE_SIZE);
        let _ = COHERENT_TOUCHES.fetch_add(1, Ordering::Relaxed);
        answer
    }

    /// Hold the layout: what `mmap`, `munmap`, `mprotect`, `mremap` and `brk`
    /// take first, as Linux's take `mmap_lock` for writing.
    ///
    /// Each of this type's methods is whole under the space's own lock, but a
    /// system call can be two of them: plain `MAP_FIXED` is an [`unmap`] and
    /// then a map, and the unmap has to let the lock go for its shootdown.
    /// Between the two another thread's `mmap` could be given the range just
    /// emptied, and the fixed mapping then failed or took the other's place;
    /// when that thread unmapped, memory the first had been handed went with
    /// it. jemalloc re-maps its reserved memory `MAP_FIXED` whenever it
    /// commits or decommits, so a many-threaded `rustc` met this within
    /// minutes (`cargo xtask test-selfhost`). A page fault does not take it:
    /// faults are whole under the space's lock, and a program touching a
    /// range that another of its threads is replacing gets what Linux would
    /// give it after either step.
    ///
    /// A lock that sleeps, since [`unmap`] asks for a shootdown under it;
    /// taken before any spin lock, and after `Process`'s heap lock.
    ///
    /// [`unmap`]: AddressSpace::unmap
    pub fn layout(&self) -> Layout<'_> {
        Layout {
            space: self,
            _held: self.layout.lock(),
        }
    }

    /// Unmap `len` bytes at `at`, giving back the pages and the tables.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] if the range is malformed.
    pub fn unmap(&self, at: u64, len: u64) -> Result<(), SpaceError> {
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        #[cfg(ferrix_personality = "user")]
        if self.is_window() {
            let (mut pages, cpus) = self.window_take_down(range);
            if let Some(cpus) = cpus {
                self.shoot(&cpus, &mut pages);
            }
            return Ok(());
        }
        // Three phases, and the order between them is the whole point:
        // translations down, then every processor that may have them told, and
        // only then the frames given back. It used to be one phase that freed
        // the pages *before* it took their translations down, under a comment
        // claiming it did the opposite — which is the bug `mm::unmap_kernel`
        // documents at length: a frame handed to the allocator while another
        // processor can still reach it through a cached translation is two
        // owners of one page, and the symptom turns up in whichever of them
        // writes second.
        //
        // Phase one, under the lock: take the range out of the map and take
        // its translations out of the tables, then read who may still have
        // them. What was removed is remembered rather than acted on, because
        // the acting has to happen after the shootdown and the shootdown may
        // not hold a lock.
        let mut freeing: Vec<Freeing> = Vec::new();
        let mut pages = TlbPages::new();
        let (cpus, touched) = {
            let mut inner = self.inner.lock();
            // Room to count out whatever the removal cuts, before it.
            self.room_to_count_out(&inner, &[range], None)?;
            let removed = inner.map.remove(range).map_err(map_error)?;
            self.count_out_removed(&inner, &removed);
            self.take_down(&removed, &mut freeing, &mut pages);
            (self.begin_shootdown(&inner), touched_ids(&removed))
        };

        // Phase two. Nothing can reach these pages through this address space
        // any more, on any processor.
        self.shoot(&cpus, &mut pages);

        // Phase three: give the pages back, not just the mapping.
        self.give_back(freeing, touched);
        Ok(())
    }

    /// Empty the space at its process's end: every region, translation and
    /// page table of its user half gone, and every object only it maps
    /// released, with nothing allocated, so that no refusal can stop or
    /// postpone it (L.user.247, `docs/EXIT-TEARDOWN.md` 4.2). The general
    /// [`unmap`] lists every region it removes and counts each out, and a
    /// space whose program mapped until it was refused could not have the
    /// room for that at its end either (F-114).
    ///
    /// `Drop`'s order on a live space: under the layout and the space's
    /// lock, the map taken whole and its translations taken down in the same
    /// hold, so no reader of the map finds an object mapped where the tables
    /// say otherwise (ET.4 (a)); every object's views forgotten, so the
    /// space's counts need no count-out and are stored 0; the object tables
    /// taken whole. Then the shootdown, and only after it the tables and the
    /// objects let go (ET.4 (b)): an object only this space held releases its
    /// frames as it drops, one another space maps keeps them.
    ///
    /// What it empties (ledger 9611 X5): every region of the map, which lies
    /// in `[MMAP_MIN_ADDR, PROGRAM_TOP)` and below the ceiling ([`new`] makes
    /// the map so, and [`set_ceiling`] refuses an end below a region), and
    /// every table under the root's slots below `top`. Above `PROGRAM_TOP`
    /// there is nothing of the program's: without the lift the user half
    /// ends there; under `linuxd` it is the region, `[SHARED_BASE,
    /// USER_VIRT_END)`, whose tables are the region's space's and are named
    /// only by a bound space's normal root. So a bound space's range stops at
    /// `SHARED_BASE` in every build. Above a lowered ceiling (a 32-bit
    /// program's) nothing is mapped. With `keeps_top` (a bound space) the
    /// take-down and the prune leave every table directly under the root, so
    /// the normal root's copies of its entries never name a freed table. A
    /// window has no regions and keeps its window take-down (TD.5).
    ///
    /// For a space nothing runs in user mode in and no other process shares:
    /// its process's end. Nothing of the end reads the program's memory
    /// after this: a thread's `clear_child_tid` write and wake come before
    /// its `thread_gone` (`end_thread`), the only way from a thread's end to
    /// the release, and a robust list is recorded but never walked. Takes
    /// the layout lock, which sleeps, so the caller holds no spin lock and
    /// may block; the OOM kill never reaches it on its own stack
    /// (`oom::owe_release`, L.object.374).
    ///
    /// [`unmap`]: AddressSpace::unmap
    /// [`new`]: AddressSpace::new
    /// [`set_ceiling`]: AddressSpace::set_ceiling
    pub fn empty_for_exit(&self) {
        #[cfg(ferrix_personality = "user")]
        if self.is_window() {
            let (low, high) = {
                let inner = self.inner.lock();
                (inner.map.low(), inner.map.high())
            };
            let _ = self.unmap(low, high - low);
            return;
        }
        let me: *const AddressSpace = self;
        let _layout = self.layout();
        let mut pages = TlbPages::new();
        // A bound space's normal root names the region's tables from
        // `SHARED_BASE` on, and its ceiling never passes `SHARED_BASE`
        // (`set_ceiling`): what it empties stops there.
        let top = if self.is_bound() {
            SHARED_BASE
        } else {
            USER_VIRT_END
        };
        // Nothing in this hold allocates (ledger 9611 X2): the map's take
        // moves its vector out, the walked take-down links the tables it
        // unlinks through themselves and falls back to a whole flush when its
        // page list is full, `forget_views` and `clear` store counts, and the
        // four takes leave empty maps. The pending shootdown it begins under
        // the same hold is what keeps a frame another space also maps from
        // going before every processor has dropped this space's translation
        // to it (X3): another mapper's take-down of such an object, or a
        // reclaim of it, still finds this space on the object's mapper list
        // until the detach below, and `forget_runs` makes its own shootdown a
        // whole flush of this space while one is pending, before it releases
        // anything (TD.2).
        let (regions, objects, shadows, files, native, cpus) = {
            let mut inner = self.inner.lock();
            let regions = inner.map.take();
            for region in &regions {
                self.unmap_range(region.range, &mut pages);
            }
            // And the tables no region's walk emptied, which a fault refused
            // memory above the page it could not map made: unlinked here and
            // given back after the shootdown, which is made whole for them
            // (ET.4 (b)).
            mm::prune_in_unlinking(self.root * PAGE_SIZE, 0, top, &mut pages, self.keeps_top());
            for vmo in inner.objects.values().chain(inner.shadows.values()) {
                vmo.forget_views(&self.rss, None);
            }
            self.rss.clear();
            let objects = core::mem::take(&mut inner.objects);
            let shadows = core::mem::take(&mut inner.shadows);
            let files = core::mem::take(&mut inner.files);
            let native = core::mem::take(&mut inner.native);
            let cpus = self.begin_shootdown(&inner);
            (regions, objects, shadows, files, native, cpus)
        };
        self.shoot(&cpus, &mut pages);
        for (&id, vmo) in &objects {
            if writable_ceiling(&native, id) {
                vmo.lower_writable_region();
            }
            vmo.detach(me, id);
        }
        for (&id, shadow) in &shadows {
            shadow.detach(me, id);
        }
        for (&id, mapping) in &files {
            if mapping.may_write
                && let Some(vmo) = objects.get(&id)
            {
                vmo.lower_shared_may_write();
            }
        }
        // The objects, shadows and files go here, with no lock held: an
        // object's last reference releases its frames, a file's may be its
        // inode's.
        drop((objects, shadows, files, native, regions));
    }

    /// Take the translations for `removed` out of the tables, note the pages
    /// behind them in `freeing` for [`AddressSpace::give_back`], and the
    /// addresses in `pages` for the shootdown.
    ///
    /// Phase one of an unmap, called with the lock held. Nothing is freed
    /// here, because nothing may be until every processor has been told.
    fn take_down(&self, removed: &[Unmapping], freeing: &mut Vec<Freeing>, pages: &mut TlbPages) {
        for unmapping in removed {
            self.unmap_range(unmapping.range, pages);
            let owned = match unmapping.backing {
                Backing::Anonymous { id, offset } => Some((id, offset, false)),
                // A private file mapping's own pages are its shadow's; the
                // file's are never an unmapper's to take.
                Backing::File { id, offset } if !unmapping.flags.shared => Some((id, offset, true)),
                Backing::File { .. } | Backing::Device { .. } => None,
            };
            if let Some((id, offset, shadow)) = owned {
                note(
                    freeing,
                    Freeing {
                        id,
                        first: offset / PAGE_SIZE,
                        pages: unmapping.range.bytes() / PAGE_SIZE,
                        shadow,
                    },
                );
            }
        }
    }

    /// Give back the pages `freeing` names, and drop every object no region
    /// names any more.
    ///
    /// Phase three of an unmap: after the shootdown, taking the lock itself. A
    /// process that unmaps half its heap expects the memory returned now, not
    /// when the other half goes.
    ///
    /// Only if nothing else holds the object: a page of memory shared with
    /// another address space is not this unmapper's to take away, and a
    /// strong count of one means this map is the only holder. The pages come
    /// out of the object under the lock, which is what the decision needs;
    /// they are given back after it, through [`Vmo::retire`], which skips this
    /// space — its translations are down and its shootdown has returned — and
    /// so, with no other holder, has nobody to ask and no shootdown to send.
    ///
    /// `touched` is every id the regions this unmap removed named, `None` when
    /// that is not known. An object leaves only when a region naming it went,
    /// so when all of them are still named there is nothing to drop and the
    /// tables are not walked: a `MAP_FIXED` into a reservation, Wine's way of
    /// allocating, removes a piece of one region thousands of times.
    fn give_back(&self, mut freeing: Vec<Freeing>, mut touched: Option<Vec<(u64, bool)>>) {
        // The ids in order, each once, for `all_still_named`'s binary search:
        // sorted here, before the lock, since they are this call's own list.
        // Not with core's `sort_unstable`, which recurses: one id per region
        // removed, and on an exit's unmap that is every region the process
        // had (F-112, `L.user.245`).
        if let Some(ids) = touched.as_mut() {
            ferrix_fallible::sort_flat_by_key(ids, |&id| id);
            ids.dedup();
        }
        // What each sole range's object gave up, in the order of `freeing`:
        // taken out under the lock, or -- with no memory to list the pages --
        // decommitted after it.
        let mut retiring: Vec<(Arc<Vmo>, Option<Retired>, Freeing)> = Vec::new();
        // Files no region maps any more, dropped once the lock is gone: the
        // last reference to an open file may be the last to its inode.
        let unkept = {
            let mut inner = self.inner.lock();
            // Decided for every range before a reference is taken for any of
            // them, because taking one moves the count the decision reads.
            freeing.retain(|range| {
                owner(&inner, range).is_some_and(|vmo| Arc::strong_count(vmo) == 1)
            });
            if fallible::try_reserve(&mut retiring, freeing.len()).is_err() {
                // Not even room to say which: the pages stay with their
                // objects until those go, as `note` leaves them.
                let _ = RANGES_KEPT.fetch_add(freeing.len() as u64, Ordering::Relaxed);
                freeing.clear();
            }
            for range in freeing {
                if let Some(vmo) = owner(&inner, &range) {
                    let taken = vmo.take_range(range.first, range.pages).ok();
                    // NOALLOC: room for every range was had above.
                    retiring.push((Arc::clone(vmo), taken, range));
                }
            }
            let none_gone = touched.is_some_and(|ids| all_still_named(&inner.map, ids));
            if none_gone {
                Vec::new()
            } else {
                self.drop_unnamed(&mut inner)
            }
        };
        drop(unkept);
        for (vmo, taken, range) in retiring {
            match taken {
                Some(retired) => vmo.retire(
                    retired,
                    Some(Own {
                        space: self,
                        shootdown: None,
                    }),
                ),
                None => {
                    let _ = vmo.decommit_range(range.first, range.pages);
                }
            }
        }
    }

    /// Take every object, shadow and file mapping no region names any more
    /// out of the tables, detached from this space. The objects and shadows
    /// are dropped here, which releases every page they committed; the files
    /// come back, for the caller to drop once the lock is gone.
    fn drop_unnamed(&self, inner: &mut Inner) -> Vec<Arc<dyn Any + Send + Sync>> {
        // Split borrows: the predicate reads the map while the objects are
        // being written.
        let me: *const AddressSpace = self;
        let Inner {
            map,
            objects,
            native,
            files,
            shadows,
            ..
        } = inner;
        // Room to hand the files back first. Without it a file mapping no
        // region names stays in the table, with its object and its shadow and
        // its may-write count, all as they were: that keeps a file open a
        // little longer and is otherwise harmless, and the next unmap that
        // walks the tables, or the space going, takes them (finding F-23).
        // Which ids the map still names, read in one pass rather than one
        // pass per id asked about: a space with thousands of regions and as
        // many objects made every unmap quadratic under this lock.
        let named = Named::of(map);
        let gone = files.keys().filter(|&&id| !named.contains(id)).count();
        let Ok(mut unkept) = fallible::try_with_capacity(gone) else {
            let kept = |id: u64| named.contains(id) || files.contains_key(&id);
            drop_objects((me, &self.rss), objects, shadows, native, kept);
            return Vec::new();
        };
        // A mapping that may write its file stops counting as its id leaves,
        // under this lock, while its object is still in hand.
        for (&id, mapping) in files.iter() {
            if mapping.may_write
                && !named.contains(id)
                && let Some(vmo) = objects.get(&id)
            {
                vmo.lower_shared_may_write();
            }
        }
        drop_objects((me, &self.rss), objects, shadows, native, |id| {
            named.contains(id)
        });
        files.retain(|&id, mapping| {
            let named = named.contains(id);
            if !named {
                // NOALLOC: room for every file gone was had above.
                unkept.push(Arc::clone(&mapping.file));
            }
            named
        });
        unkept
    }

    /// Take down every translation this space has of pages `first..first +
    /// pages` of the object it knows as `object`, and say which processors
    /// may still have them cached.
    ///
    /// What a VMO calls in phase two of taking a page away. The lock is taken
    /// here; the regions naming the object are found in the map as it stands,
    /// and exactly the addresses their offsets put those pages at are unmapped
    /// and added to `flush`. The set is read after that, still under the lock.
    ///
    /// `None` when there was nothing to take down and no shootdown of this
    /// space is pending. `Some` otherwise — with the empty set if no processor
    /// has this space loaded, since the tables changed all the same — and then
    /// the shootdown is counted pending, and the caller must call
    /// [`AddressSpace::flushed`] once the shootdown for `flush` on that set
    /// has returned. If one was already pending, `flush` is widened to the
    /// whole TLB, for the reason the module gives.
    ///
    /// Must be called holding no spin lock: the caller holds a VMO's pages
    /// lock never, and its mapper list not across this.
    #[expect(
        dead_code,
        reason = "the one-run entry agreed with stage 9's vmo_map; `Vmo::retire` asks for every run at once through `forget_runs`"
    )]
    pub(crate) fn forget_pages(
        &self,
        object: u64,
        first: u64,
        pages: u64,
        flush: &mut TlbPages,
    ) -> Option<CpuSet> {
        self.forget_runs(object, &[(first, pages)], flush, None)
            .map(|(cpus, _)| cpus)
    }

    /// [`AddressSpace::forget_pages`] for several runs of `(first, count)`
    /// pages, under one lock.
    ///
    /// With `cut`, the object a truncation is cutting: if that is the file this
    /// space maps privately as `object`, the shadow's pages in `runs` are taken
    /// out as well and handed back beside the set, for the caller to release
    /// once its shootdown, which reaches every address taken down here, has
    /// returned. See [`Vmo::cut_mappings`].
    pub(crate) fn forget_runs(
        &self,
        object: u64,
        runs: &[(u64, u64)],
        flush: &mut TlbPages,
        cut: Option<&Vmo>,
    ) -> Option<(CpuSet, Option<ShadowCopies>)> {
        let inner = self.inner.lock();
        let found = self.forget_in(&inner, object, runs, flush);
        // Under this lock, then the shadow's pages lock: the order the module
        // gives.
        let copies = match (inner.objects.get(&object), inner.shadows.get(&object)) {
            (Some(file), Some(shadow))
                if cut.is_some_and(|from| core::ptr::eq(Arc::as_ptr(file), from)) =>
            {
                cut_shadow(shadow, runs)
            }
            _ => None,
        };
        let pending = self.flushes.pending.load(Ordering::SeqCst) > 0;
        if !found && !pending && copies.is_none() {
            return None;
        }
        if pending {
            flush.everything();
        }
        Some((self.begin_shootdown(&inner), copies))
    }

    /// Unmap every address at which a region of `inner`'s map shows one of
    /// `runs`' pages of object `object`, adding each to `flush`. Whether any
    /// region named them.
    ///
    /// Called with the lock held; `inner` is what proves it.
    fn forget_in(
        &self,
        inner: &Inner,
        object: u64,
        runs: &[(u64, u64)],
        flush: &mut TlbPages,
    ) -> bool {
        let mut found = false;
        for region in inner.map.iter() {
            let offset = match region.backing {
                Backing::Anonymous { id, offset } | Backing::File { id, offset }
                    if id == object =>
                {
                    offset
                }
                _ => continue,
            };
            let region_first = offset / PAGE_SIZE;
            let region_end = region_first.saturating_add(region.range.bytes() / PAGE_SIZE);
            for &(first, count) in runs {
                let start = first.max(region_first);
                let end = first.saturating_add(count).min(region_end);
                if start >= end {
                    continue;
                }
                let address = region.range.start() + (start - region_first) * PAGE_SIZE;
                let len = (end - start) * PAGE_SIZE;
                let _ = mm::unmap_in_keeping(
                    self.root * PAGE_SIZE,
                    address,
                    len,
                    flush,
                    self.keeps_top(),
                );
                found = true;
            }
        }
        found
    }

    /// Take `range`'s translations out of the tables, adding the addresses
    /// and every table the removal empties to `pages`, for its shootdown.
    fn unmap_range(&self, range: PageRange, pages: &mut TlbPages) {
        let _ = mm::unmap_in_keeping(
            self.root * PAGE_SIZE,
            range.start(),
            range.bytes(),
            pages,
            self.keeps_top(),
        );
    }

    /// Count a shootdown of this space pending, and read the processors it
    /// must reach.
    ///
    /// Called with the lock held — `_locked` is the proof — and after the
    /// translations it is for are out of the tables, never before.
    fn begin_shootdown(&self, _locked: &Inner) -> CpuSet {
        let _ = self.flushes.pending.fetch_add(1, Ordering::SeqCst);
        // Sequentially consistent, and before the set is read: what lets a
        // processor leave the set with this space's translations still in
        // its TLB under a PCID (`install`, and the module's last section).
        let _ = self.flushes.begun.fetch_add(1, Ordering::SeqCst);
        self.cpus.snapshot()
    }

    /// How many shootdowns of this space have begun: see the field.
    pub fn shootdowns_begun(&self) -> u64 {
        self.flushes.begun.load(Ordering::SeqCst)
    }

    /// A shootdown [`AddressSpace::begin_shootdown`] or
    /// [`AddressSpace::forget_pages`] counted has returned.
    pub(crate) fn flushed(&self) {
        let _ = self.flushes.pending.fetch_sub(1, Ordering::SeqCst);
    }

    /// Run this space's own shootdown, begun under the lock, now that the lock
    /// is gone.
    fn shoot(&self, cpus: &CpuSet, pages: &mut TlbPages) {
        smp::flush_tlb_pages(cpus, pages);
        self.flushed();
    }
}

/// An address space's shootdowns, counted.
#[derive(Debug)]
struct Flushes {
    /// Counted under the space's lock and not yet returned.
    pending: AtomicU64,
    /// Ever begun: what a change that should have taken nothing down is
    /// held to, and what a PCID slot is compared with before it is kept
    /// (`install`).
    begun: AtomicU64,
}

impl Flushes {
    /// None of either.
    const fn new() -> Flushes {
        Flushes {
            pending: AtomicU64::new(0),
            begun: AtomicU64::new(0),
        }
    }
}

/// What a cut over `runs` takes out of a private mapping's `shadow`: every
/// page from the first run's start, since a cut runs to the end of the
/// object. `None` if the shadow has nothing there.
fn cut_shadow(shadow: &Arc<Vmo>, runs: &[(u64, u64)]) -> Option<ShadowCopies> {
    let first = runs.iter().map(|&(first, _)| first).min()?;
    let taken = match shadow.take_range(first, u64::MAX - first) {
        Ok(taken) if taken.is_empty() => return None,
        Ok(taken) => Some(taken),
        // No memory to list them: decommitted after the shootdown instead.
        Err(_) => None,
    };
    Some(ShadowCopies {
        shadow: Arc::clone(shadow),
        taken,
        first,
    })
}

/// What a file mapping's id keeps beside its object.
#[derive(Clone, Debug)]
pub struct FileMapping {
    /// The open file it was mapped through.
    pub file: Arc<dyn Any + Send + Sync>,
    /// Whether this is a shared mapping that may write the file: of a file
    /// open for writing, made while no seal refused writes. Counted in the
    /// file object's may-write count for as long as the id is in the tables,
    /// and what `mprotect` asks before it makes a shared file mapping writable.
    pub may_write: bool,
}

/// Where a private file mapping's fault puts a frame: page `index` of the
/// objects the region knows as `id`, at the address `page`.
#[derive(Clone, Copy, Debug)]
struct Placement {
    id: u64,
    index: u64,
    page: u64,
    frame: Frame,
}

/// Ask `vmo`'s pager for page `index`: supplied if absent, and for a write
/// (`writes`) to a Clean or awaiting-clean page let Dirty, by the pager first
/// in a trap-dirty object (`docs/microkernel/K3.md` §2.2 to §2.5). With no
/// lock held.
fn ask(vmo: &Vmo, index: u64, writes: bool, mode: FaultMode) -> Result<(), Refusal> {
    let Some(link) = vmo.pager() else {
        // A disk file's page: filled by the fault's own path, and marked here, by
        // the kernel, as a pager VMO that does not trap (K3b).
        return if writes && vmo.tracks_dirty() && vmo.page_state(index) != PageState::Dirty {
            // Still absent once the fill has looked: a hole, past what the file's
            // source holds, whose page a write commits as zeros. Committed here,
            // so that it is marked before a mapping can write it. Past the file's
            // end nothing goes in, and `resolve` refuses it.
            if vmo.page_state(index) == PageState::Absent {
                let _ = vmo.commit_within(index).map_err(|_| Refusal::NoMemory)?;
            }
            vmo.mark_dirty(index)
                .map(|_| ())
                .map_err(|_| Refusal::NoMemory)
        } else {
            Ok(())
        };
    };
    match vmo.page_state(index) {
        PageState::Absent => link.request(vmo, index, Command::Read, mode),
        PageState::Clean | PageState::AwaitingClean if writes && link.traps_dirty() => {
            link.request(vmo, index, Command::Dirty, mode)
        }
        PageState::Clean | PageState::AwaitingClean if writes => vmo
            .mark_dirty(index)
            .map(|_| ())
            .map_err(|_| Refusal::NoMemory),
        _ => Ok(()),
    }
}

/// The region `address` lies in, if `access` is one it permits: what a fault
/// resolves through.
///
/// # Errors
///
/// [`SpaceError::NotMapped`] for no region, [`SpaceError::Refused`] for an
/// access the region forbids.
fn permitted(inner: &Inner, address: u64, access: Access) -> Result<Vma, SpaceError> {
    let region = *inner
        .map
        .find(address)
        .ok_or(SpaceError::NotMapped(address))?;
    if !permits(region.flags, access) {
        return Err(SpaceError::Refused(address));
    }
    Ok(region)
}

/// Whether page `index` of `vmo` is installed writable through `region`:
/// never in a copy-on-write region, whose next write must fault to copy, and
/// never while a pager-backed object's page is not Dirty, whose next write
/// must fault to be seen (`docs/microkernel/K3.md` §2.5).
fn writable_in(region: &Vma, vmo: &Vmo, index: u64) -> bool {
    region.flags.write && !region.cow && vmo.writable_page(index)
}

/// How a page of `vmo` that may be written in place is mapped through
/// `region`: writable, with the object's memory type.
fn writable_flags(region: &Vma, vmo: &Vmo) -> MapFlags {
    MapFlags {
        execute: region.flags.execute,
        uncached: vmo.is_coherent(),
        write_combining: vmo.is_combining(),
        ..MapFlags::USER_DATA
    }
}

/// A [`Placement`] of its four parts.
const fn placed(id: u64, index: u64, page: u64, frame: Frame) -> Placement {
    Placement {
        id,
        index,
        page,
        frame,
    }
}

/// Touches of a write-combining user page made coherent by
/// [`AddressSpace::coherent_touch`], since boot: what stage 10's chardev
/// check reads.
static COHERENT_TOUCHES: AtomicU64 = AtomicU64::new(0);

/// [`COHERENT_TOUCHES`] now.
pub fn coherent_touches() -> u64 {
    COHERENT_TOUCHES.load(Ordering::Relaxed)
}

/// `flags` with the memory type of `vmo`'s user mappings: a
/// write-combining object's page is never mapped write-back beside its
/// other mappings (`L.user.133`).
fn typed(flags: MapFlags, vmo: &Vmo) -> MapFlags {
    MapFlags {
        write_combining: vmo.is_combining(),
        ..flags
    }
}

/// linuxd's own: what a fault brings with it (`BUILD-FLAGS.md`, `fault_ahead`). Shape-specific,
/// and in no kernel's image.
#[cfg(ferrix_load_built = "lift")]
impl AddressSpace {
    /// Map for reading, in this space's tables alone, the pages of a file mapping in the
    /// aligned group around `address` that the file's object already holds, and answer
    /// them: linuxd's fault-around, which places them with the fault's own page. A
    /// fault costs a program several times what it costs in the hybrid, and a warm
    /// `rustc` takes 20,000 of them, most reading files (`docs/microkernel/PERFORMANCE.md`).
    /// No page is read from a disk here, nothing is written, and a neighbour's trouble is
    /// no one's: it just stays out. Under the lock each look, and the walk with it: the
    /// shim frees an unlinked table at once (ledger 6537, A1).
    pub(crate) fn fault_around(&self, address: u64) -> ([u64; AROUND_PAGES as usize], usize) {
        let mut found = [0_u64; AROUND_PAGES as usize];
        let mut count = 0;
        let root = self.root * PAGE_SIZE;
        let page = address & !(PAGE_SIZE - 1);
        let group = page & !(AROUND_PAGES * PAGE_SIZE - 1);
        let reading = Access {
            write: false,
            execute: false,
        };
        for at in (0..AROUND_PAGES).map(|index| group + index * PAGE_SIZE) {
            if at == page {
                continue;
            }
            let resident = {
                let inner = self.inner.lock();
                master_translate!(self, root, at).is_none()
                    && inner.map.find(at).is_some_and(|region| {
                        let Backing::File { id, offset } = region.backing else {
                            return false;
                        };
                        let index = offset.saturating_add(at.saturating_sub(region.range.start()))
                            / PAGE_SIZE;
                        // Whether it is held is all this asks: `resolve` holds a neighbour
                        // to the region's rights, the file's end and a private copy as it
                        // holds the fault (the controls that dropped each here did not
                        // fire, ledger 6537).
                        inner
                            .objects
                            .get(&id)
                            .is_some_and(|file| file.page(index).is_some())
                    })
            };
            // As `write_ahead`: tables may be needed, and a refusal would count.
            let room = crate::object::quota::at_limit(
                crate::object::quota::charge_target(),
                crate::object::quota::Resource::Memory,
                4 * PAGE_SIZE,
            )
            .is_none();
            if resident && room && self.resolve(at, reading).is_ok() {
                found[count] = at;
                count += 1;
            }
        }
        (found, count)
    }

    /// A write fault on plain private anonymous memory whose page before is in already: a
    /// program walking up through fresh memory, whose next pages will fault one by one.
    /// Commit those up to [`AHEAD_PAGES`] with this one, in the same region, and answer them
    /// for linuxd's batch. A neighbour's trouble, a limit above all, is no one's: it stays
    /// out, and its own fault says.
    pub(crate) fn write_ahead(&self, address: u64) -> ([u64; AROUND_PAGES as usize], usize) {
        let mut found = [0_u64; AROUND_PAGES as usize];
        let mut count = 0;
        let root = self.root * PAGE_SIZE;
        let page = address & !(PAGE_SIZE - 1);
        let writing = Access {
            write: true,
            execute: false,
        };
        // How far this region goes, if it is plain and the walk has a page behind it.
        let end = {
            let inner = self.inner.lock();
            let plain = |region: &ferrix_vma::Vma| {
                let Backing::Anonymous { id, .. } = region.backing else {
                    return false;
                };
                permits(region.flags, writing)
                    && !region.flags.shared
                    && !region.flags.execute
                    && inner
                        .objects
                        .get(&id)
                        .is_some_and(|vmo| !vmo.is_pager_backed())
            };
            let behind = page
                .checked_sub(PAGE_SIZE)
                .is_some_and(|before| master_translate!(self, root, before).is_some());
            match inner.map.find(page) {
                Some(region) if behind && plain(region) => region.range.end(),
                _ => return (found, 0),
            }
        };
        for at in (1..=AHEAD_PAGES).map(|index| page + index * PAGE_SIZE) {
            let free = {
                let _inner = self.inner.lock();
                at < end && master_translate!(self, root, at).is_none()
            };
            // Looked at first: a charge refused here would be counted and audited against
            // a program that asked for nothing (ledger 6537, A6). A page and its tables.
            let room = crate::object::quota::at_limit(
                crate::object::quota::charge_target(),
                crate::object::quota::Resource::Memory,
                4 * PAGE_SIZE,
            )
            .is_none();
            if !free || !room || self.resolve(at, writing).is_err() {
                break;
            }
            found[count] = at;
            count += 1;
        }
        (found, count)
    }
}

/// The window's boot check: a window's unmap's decisions (ledger 6421, U4).
#[cfg(ferrix_personality = "user")]
#[cfg(not(ferrix_load_built = "lift"))]
pub mod window_check;

/// How many pages a fault may bring in: the aligned group around it (`fault_around`).
#[cfg(ferrix_load_built = "lift")]
pub(crate) const AROUND_PAGES: u64 = 16;

/// How many pages past a walking write fault are committed with it (`write_ahead`).
#[cfg(ferrix_load_built = "lift")]
pub(crate) const AHEAD_PAGES: u64 = 7;

/// The most objects one window's pages may come from.
#[cfg(ferrix_personality = "user")]
const PLACED_OBJECTS: usize = 4096;

/// The translation flags of a user page.
/// Whether a present leaf with `flags` may be made writable in place for a
/// copy-on-write write by its only holder: it decodes to what the old path
/// would install, a writable user page with the region's execute permission,
/// on normal memory and not global, less the write permission. Anything else
/// takes the page down and maps it again, as before (ledger 8299, A3).
fn reusable_in_place(flags: Option<MapFlags>, execute: bool) -> bool {
    flags.is_some_and(|flags| {
        MapFlags {
            write: true,
            ..flags
        } == user_page(true, execute)
    })
}

fn user_page(write: bool, execute: bool) -> MapFlags {
    MapFlags {
        read: true,
        write,
        execute,
        user: true,
        global: false,
        device: false,
        uncached: false,
        write_combining: false,
    }
}

/// Pages of one object whose translations an unmap has taken down, and which
/// go back once every processor has been told.
#[derive(Clone, Copy, Debug)]
struct Freeing {
    /// The object.
    id: u64,
    /// Its first page.
    first: u64,
    /// How many.
    pages: u64,
    /// Whether they come out of the id's shadow, for a private file mapping,
    /// rather than out of its object.
    shadow: bool,
}

/// Take every object and shadow that `kept` does not keep out of the tables,
/// detached from the space at `me`, and drop them, which releases every page
/// they committed.
/// Whether `id` is a region by handle whose ceiling holds `WRITE`: what
/// [`Vmo::seal`] refuses, by count.
fn writable_ceiling(native: &BTreeMap<u64, Ceiling>, id: u64) -> bool {
    matches!(native.get(&id), Some(Ceiling::Handle { write: true, .. }))
}

fn drop_objects(
    (me, rss): (*const AddressSpace, &Arc<Rss>),
    objects: &mut BTreeMap<u64, Arc<Vmo>>,
    shadows: &mut BTreeMap<u64, Arc<Vmo>>,
    native: &mut BTreeMap<u64, Ceiling>,
    kept: impl Fn(u64) -> bool,
) {
    objects.retain(|&id, vmo| {
        let named = kept(id);
        if !named {
            // Phase three of an unmap: this region's translations are gone
            // on every processor by now (K14.2).
            if writable_ceiling(native, id) {
                vmo.lower_writable_region();
            }
            vmo.forget_views(rss, Some(id));
            vmo.detach(me, id);
        }
        named
    });
    native.retain(|id, _| objects.contains_key(id));
    shadows.retain(|&id, shadow| {
        let named = kept(id);
        if !named {
            shadow.forget_views(rss, Some(id));
            shadow.detach(me, id);
        }
        named
    });
}

/// Ranges whose pages an unmap could not note for giving back, for want of
/// memory (finding F-23). Their pages stay committed in an object no region
/// shows them through -- `mremap` gives a private region a new object rather
/// than regrow the old one over them -- until the object goes.
static RANGES_KEPT: AtomicU64 = AtomicU64::new(0);

/// Note `range` for [`AddressSpace::give_back`], or count it kept.
fn note(freeing: &mut Vec<Freeing>, range: Freeing) {
    if fallible::try_push(freeing, range).is_err() {
        let _ = RANGES_KEPT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Where [`AddressSpace::map_file`] puts a mapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilePlace {
    /// Exactly here: `MAP_FIXED`, with the range already cleared.
    Fixed(u64),
    /// Wherever it fits, near the hint if there is one.
    Anywhere(Option<u64>),
}

/// Where `mremap` may put the region it resizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Destination {
    /// Where it is, or nowhere: no `MREMAP_MAYMOVE`.
    InPlace,
    /// Where it is if it fits there, wherever it fits otherwise:
    /// `MREMAP_MAYMOVE`.
    Anywhere,
    /// Exactly here, replacing whatever is mapped there: `MREMAP_FIXED`.
    Fixed(u64),
}

impl AddressSpace {
    /// Resize the mapping of `old_len` bytes at `old` to `new_len` bytes,
    /// moving it if `destination` allows, and say where it now is.
    ///
    /// `mremap`. Both lengths are whole pages, and `old..old + old_len` must
    /// lie inside one region: one object, one set of flags, one marking.
    ///
    /// # Nothing is copied
    ///
    /// A private region is given an object of its own, of the new length, and
    /// the pages it had are *moved* into it -- frame numbers out of one list
    /// and into the other, references and all. That is what makes glibc's
    /// `realloc` of a large block cheap, and it is also what keeps a moved
    /// region honest: an object nothing else names cannot be aliased by
    /// another region of the old one, which a region that kept naming the old
    /// object at its old offsets could be once that object's other pieces had
    /// grown or moved too. The region keeps its copy-on-write marking, so a
    /// page it still shares with a `fork` child is copied on the next write
    /// exactly as it would have been.
    ///
    /// A shared region cannot do that -- its pages are the same pages for
    /// whoever else maps the object -- so it keeps its object and its offset,
    /// and the object is grown to cover it, as Linux grows the shmem file
    /// behind shared anonymous memory. Growing it over offsets another region
    /// of this space already names is refused with
    /// [`SpaceError::OutOfMemory`]: those pages would be mapped twice, and an
    /// unmap of either would give back pages the other still shows.
    ///
    /// Either way the old translations come down and the new ones arrive on
    /// fault, with the protection the region always had.
    ///
    /// # Errors
    ///
    /// [`SpaceError::NotMapped`] if the old range is not wholly inside one
    /// region; [`SpaceError::Refused`] if that region is not anonymous memory,
    /// or is a VMO `vmo_map` put there;
    /// [`SpaceError::NotUserRange`] for a fixed destination outside the user
    /// half or off a page boundary; [`SpaceError::BadRange`] for one that
    /// overlaps the old range, or a malformed length; and
    /// [`SpaceError::OutOfMemory`] when the region cannot grow where it is and
    /// may not move, or there is no gap it fits, or a private region that
    /// would move holds a page a device holds -- moving it would give the
    /// program a different frame from the device's at the same address.
    pub fn remap(
        &self,
        old: u64,
        old_len: u64,
        new_len: u64,
        destination: Destination,
    ) -> Result<u64, SpaceError> {
        let old_range = user_range(old, old_len).ok_or(SpaceError::NotMapped(old))?;
        if new_len == 0 || !new_len.is_multiple_of(PAGE_SIZE) {
            return Err(SpaceError::BadRange);
        }

        let mut freeing: Vec<Freeing> = Vec::new();
        let mut pages = TlbPages::new();
        let (placed, cpus, moved) = {
            let mut inner = self.inner.lock();

            let region = *inner.map.find(old).ok_or(SpaceError::NotMapped(old))?;
            if region.range.end() < old_range.end() {
                return Err(SpaceError::NotMapped(old));
            }
            let Some((id, offset)) = inner.reshapeable(region.backing) else {
                return Err(SpaceError::Refused(old));
            };
            // Where in the object the old range starts, in bytes.
            let first = offset
                .checked_add(old - region.range.start())
                .ok_or(SpaceError::BadRange)?;
            let shared = region.flags.shared || shared_object(&inner.map, id);

            let Some(at) = remap_target(&inner.map, destination, old_range, new_len)? else {
                return Ok(old);
            };
            let new_range = PageRange::from_len(at, new_len).map_err(|_| SpaceError::BadRange)?;

            if shared && new_len > old_len {
                let end = first.checked_add(new_len).ok_or(SpaceError::BadRange)?;
                if named_elsewhere(&inner.map, id, first + old_len, end) {
                    return Err(SpaceError::OutOfMemory);
                }
            }
            let vmo = remap_object(&mut inner, id, old)?;

            // A private region's pages move to an object of their own, and the
            // move goes first, before the map changes, because it is the last
            // thing that can still be refused: a held page does not move.
            let mut fresh = if shared {
                None
            } else {
                Some(self.move_private(&mut inner, &vmo, first, old_len.min(new_len), new_len)?)
            };

            // The new placement counted in while a refusal can still be
            // answered, with the room to count out what the clear removes:
            // undone exactly, and the move with it, if either is refused
            // (`user::rss`).
            let fixed = matches!(destination, Destination::Fixed(_));
            let placement = (
                new_range,
                placed_backing(fresh.as_ref(), id, first),
                region.flags,
            );
            self.count_remap_in(&mut inner, placement, (fixed, old_range), &mut fresh)?;

            // Everything that can be refused has been. Whatever a fixed
            // destination covers goes, as `MAP_FIXED` would take it; then the
            // old range leaves the map and the tables, but not its pages.
            let cleared = self.clear_for_remap(
                &mut inner,
                fixed,
                placement,
                old_range,
                &mut freeing,
                &mut pages,
            );

            let resize = Resize {
                id,
                first,
                old_len,
                new_len,
            };
            let (backing, moved) =
                self.remap_backing(&mut inner, &vmo, resize, cleared, fresh, &mut freeing);

            let moved_to = moved_object(&backing, id);
            let placed = backing.and_then(|backing| {
                inner
                    .map
                    .insert_region(Vma {
                        range: new_range,
                        backing,
                        ..region
                    })
                    .map(|()| at)
                    .map_err(|_| SpaceError::BadRange)
            });
            self.count_out_unplaced(&inner, placement, cleared, placed);
            carry_apart(&mut inner, id, moved_to.filter(|_| placed.is_ok()));
            let cpus = self.begin_shootdown(&inner);
            (placed, cpus, moved.map(|retired| (vmo, retired)))
        };

        // The old translations are out of the tables but may still be in the
        // TLB of a processor in the set, and so may whatever a fixed
        // destination replaced.
        self.shoot(&cpus, &mut pages);
        // Any other space mapping the object the pages moved out of forgets
        // them too, before the new object can give one back. And the reference
        // to that object goes before `give_back`, which reads its count.
        if let Some((from, retired)) = moved {
            from.retire(
                retired,
                Some(Own {
                    space: self,
                    shootdown: None,
                }),
            );
        }
        self.give_back(freeing, None);
        placed
    }

    /// Undo [`AddressSpace::move_private`]'s move, `fresh`, for an `mremap`
    /// refused after it: the new object forgets the pages, which the old one
    /// never stopped naming, and leaves the tables.
    fn undo_move(&self, inner: &mut Inner, fresh: Option<(u64, Arc<Vmo>, Retired)>) {
        if let Some((fresh_id, fresh, _moved)) = fresh {
            fresh.disown_pages();
            self.drop_fresh(inner, fresh_id);
        }
    }

    /// For an `mremap`: count the region's new placement (its range,
    /// backing and flags) in, and have the room to count out what the clear
    /// removes -- a `fixed` destination's regions and the old range --
    /// before the map changes; with none, the placement counted out again,
    /// the move (`fresh`) undone, and the refusal answered.
    fn count_remap_in(
        &self,
        inner: &mut Inner,
        (range, backing, flags): (PageRange, Backing, VmaFlags),
        (fixed, old_range): (bool, PageRange),
        fresh: &mut Option<(u64, Arc<Vmo>, Retired)>,
    ) -> Result<(), SpaceError> {
        let removed: &[PageRange] = if fixed {
            &[range, old_range]
        } else {
            &[old_range]
        };
        let counted = self.count_in(inner, range, backing, flags).and_then(|()| {
            self.room_to_count_out(inner, removed, Some((range, backing, flags)))
                .inspect_err(|_| self.count_out(inner, range, backing, flags))
        });
        if counted.is_err() {
            self.undo_move(inner, fresh.take());
        }
        counted
    }

    /// For an `mremap` whose old range was cleared and whose new placement
    /// was then refused: the placement counted out again.
    fn count_out_unplaced(
        &self,
        inner: &Inner,
        (range, backing, flags): (PageRange, Backing, VmaFlags),
        cleared: Result<(), SpaceError>,
        placed: Result<u64, SpaceError>,
    ) {
        if cleared.is_ok() && placed.is_err() {
            self.count_out(inner, range, backing, flags);
        }
    }

    /// Move a private region's `moved` bytes of `vmo`, from byte `first`,
    /// into a new object `new_len` bytes long, for an `mremap`: the new
    /// object's id, the object, and what the move took out of `vmo`.
    ///
    /// The move goes before the map changes, because it is the last thing
    /// that can still be refused: a held page does not move, and the new
    /// object needs memory.
    fn move_private(
        &self,
        inner: &mut Inner,
        vmo: &Arc<Vmo>,
        first: u64,
        moved: u64,
        new_len: u64,
    ) -> Result<(u64, Arc<Vmo>, Retired), SpaceError> {
        // The pages leave an object only this space maps, so nobody else can
        // hold a translation to them: always checked, before anything is
        // moved or anyone told.
        if vmo.mapper_count() != 1 {
            crate::panic::fatal!(
                crate::panic::catalog::PRIVATE_OBJECT_SHARED,
                "mremap found a VMO backing a private region with more than one mapper"
            );
        }
        let fresh_id = self.add_fresh(inner, new_len / PAGE_SIZE)?;
        let fresh = Arc::clone(
            inner
                .objects
                .get(&fresh_id)
                .ok_or(SpaceError::OutOfMemory)?,
        );
        match fresh.adopt_pages(vmo, first / PAGE_SIZE, moved / PAGE_SIZE) {
            Ok(retired) => Ok((fresh_id, fresh, retired)),
            Err(_) => {
                self.drop_fresh(inner, fresh_id);
                Err(SpaceError::OutOfMemory)
            }
        }
    }

    /// A new private anonymous object of `pages` pages for an `mremap` to
    /// move pages into, attached and in the table under a new id, which is
    /// returned: made before anything is changed, so that running out of
    /// memory is still an answer (finding F-23). Until a region names it, it
    /// is an object nothing maps.
    fn add_fresh(&self, inner: &mut Inner, pages: u64) -> Result<u64, SpaceError> {
        let fresh = Vmo::new_anonymous(pages).map_err(|_| SpaceError::OutOfMemory)?;
        let id = inner.next_id;
        fresh
            .attach(self.me.clone(), id, Sharing::Private)
            .map_err(|_| SpaceError::OutOfMemory)?;
        let Ok(held) = fallible::reserve() else {
            fresh.detach(self, id);
            return Err(SpaceError::OutOfMemory);
        };
        let _ = fallible::insert_held(&held, &mut inner.objects, id, fresh);
        drop(held);
        inner.next_id = inner.next_id.saturating_add(1);
        Ok(id)
    }

    /// Undo [`AddressSpace::add_fresh`] for an `mremap` that did not move.
    fn drop_fresh(&self, inner: &mut Inner, id: u64) {
        if let Some(fresh) = inner.objects.remove(&id) {
            fresh.forget_views(&self.rss, Some(id));
            fresh.detach(self, id);
        }
    }

    /// The backing an `mremap`'s region gets, once `cleared` says whether its
    /// old placement came out of the map and the tables, and what its pages
    /// left behind in the old object if they moved.
    ///
    /// A shared region keeps its object, grown to cover it, and gives back the
    /// tail a shrink cut off. A private region takes `fresh`, attached under a
    /// new id, and the moved frames leave `vmo`. On a failure `fresh` forgets
    /// them instead: `vmo` never stopped naming them, so the old translations
    /// to them were never wrong.
    fn remap_backing(
        &self,
        inner: &mut Inner,
        vmo: &Arc<Vmo>,
        resize: Resize,
        cleared: Result<(), SpaceError>,
        fresh: Option<(u64, Arc<Vmo>, Retired)>,
        freeing: &mut Vec<Freeing>,
    ) -> (Result<Backing, SpaceError>, Option<Retired>) {
        let Resize {
            id,
            first,
            old_len,
            new_len,
        } = resize;
        match (cleared, fresh) {
            (Err(why), Some((fresh_id, fresh, _moved))) => {
                fresh.disown_pages();
                self.drop_fresh(inner, fresh_id);
                (Err(why), None)
            }
            (Err(why), None) => (Err(why), None),
            (Ok(()), None) => {
                vmo.grow_to((first + new_len) / PAGE_SIZE);
                if new_len < old_len {
                    note(
                        freeing,
                        Freeing {
                            id,
                            first: (first + new_len) / PAGE_SIZE,
                            pages: (old_len - new_len) / PAGE_SIZE,
                            shadow: false,
                        },
                    );
                }
                (Ok(Backing::Anonymous { id, offset: first }), None)
            }
            (Ok(()), Some((fresh_id, _fresh, retired))) => {
                vmo.release_moved(&retired);
                // Whatever did not move -- the tail a shrink cut off -- is
                // still the old object's, and goes back with the rest.
                note(
                    freeing,
                    Freeing {
                        id,
                        first: first / PAGE_SIZE,
                        pages: old_len / PAGE_SIZE,
                        shadow: false,
                    },
                );
                let backing = Backing::Anonymous {
                    id: fresh_id,
                    offset: 0,
                };
                (Ok(backing), Some(retired))
            }
        }
    }

    /// Take out of the map and the tables what an `mremap` replaces: the
    /// destination if it is `fixed`, then the old range.
    ///
    /// Called with the lock held. On an error the old range is still mapped,
    /// in the map and the tables both.
    fn clear_for_remap(
        &self,
        inner: &mut Inner,
        fixed: bool,
        placement: (PageRange, Backing, VmaFlags),
        old_range: PageRange,
        freeing: &mut Vec<Freeing>,
        pages: &mut TlbPages,
    ) -> Result<(), SpaceError> {
        let (new_range, backing, flags) = placement;
        let cleared = self.clear_ranges(inner, (fixed, new_range), old_range, freeing, pages);
        // The new placement, counted in before, counted out again when it
        // will not be placed.
        if cleared.is_err() {
            self.count_out(inner, new_range, backing, flags);
        }
        cleared
    }

    /// [`AddressSpace::clear_for_remap`]'s removals: the destination if it
    /// is `fixed`, then the old range, each counted out as it goes.
    fn clear_ranges(
        &self,
        inner: &mut Inner,
        (fixed, new_range): (bool, PageRange),
        old_range: PageRange,
        freeing: &mut Vec<Freeing>,
        pages: &mut TlbPages,
    ) -> Result<(), SpaceError> {
        if fixed {
            let removed = inner.map.remove(new_range).map_err(map_error)?;
            self.count_out_removed(inner, &removed);
            self.take_down(&removed, freeing, pages);
        }
        let removed = inner.map.remove(old_range).map_err(map_error)?;
        self.count_out_removed(inner, &removed);
        self.unmap_range(old_range, pages);
        Ok(())
    }
}

/// The backing an `mremap`'s new placement names: the new object a private
/// region's pages moved to, `fresh`, from its start, or the region's own
/// object `id` from byte `first`.
fn placed_backing(fresh: Option<&(u64, Arc<Vmo>, Retired)>, id: u64, first: u64) -> Backing {
    match fresh {
        Some(&(fresh_id, _, _)) => Backing::Anonymous {
            id: fresh_id,
            offset: 0,
        },
        None => Backing::Anonymous { id, offset: first },
    }
}

/// Where an object an `mremap` moves pages within starts, and the two
/// lengths.
#[derive(Clone, Copy, Debug)]
struct Resize {
    /// The object the old range names.
    id: u64,
    /// Where in it the old range starts, in bytes.
    first: u64,
    /// The old length.
    old_len: u64,
    /// The new length.
    new_len: u64,
}

/// Where an `mremap` of `old_range` to `new_len` bytes goes, or `None` if it
/// stays exactly as it is.
fn remap_target(
    map: &ferrix_vma::AddressSpace,
    destination: Destination,
    old_range: PageRange,
    new_len: u64,
) -> Result<Option<u64>, SpaceError> {
    let (old, old_len) = (old_range.start(), old_range.bytes());
    Ok(Some(match destination {
        Destination::Fixed(target) => {
            let target_range = user_range(target, new_len)
                .filter(|_| target.is_multiple_of(PAGE_SIZE))
                .ok_or(SpaceError::NotUserRange(target))?;
            if target_range.start() < old_range.end() && old_range.start() < target_range.end() {
                return Err(SpaceError::BadRange);
            }
            target
        }
        _ if new_len == old_len => return Ok(None),
        _ if new_len < old_len => old,
        _ if grows_in_place(map, old, old_len, new_len) => old,
        Destination::Anywhere => map
            .find_free(new_len, PAGE_SIZE, None)
            .filter(|&at| user_range(at, new_len).is_some())
            .ok_or(SpaceError::OutOfMemory)?,
        Destination::InPlace => return Err(SpaceError::OutOfMemory),
    }))
}

/// `at..at + len` as a range, if it is a well-formed one inside the user half.
fn user_range(at: u64, len: u64) -> Option<PageRange> {
    if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
        return None;
    }
    PageRange::from_len(at, len).ok()
}

/// Whether the mapping of `old_len` bytes at `old` can grow to `new_len`
/// bytes where it is: the pages after it are in the user half and unmapped.
///
/// Asked of `find_free` with the extension as its hint, which answers with
/// the hint exactly when that much fits there, and somewhere else otherwise.
fn grows_in_place(map: &ferrix_vma::AddressSpace, old: u64, old_len: u64, new_len: u64) -> bool {
    let Some(extension) = old.checked_add(old_len) else {
        return false;
    };
    let more = new_len.saturating_sub(old_len);
    user_range(extension, more).is_some()
        && map.find_free(more, PAGE_SIZE, Some(extension)) == Some(extension)
}

/// Whether any region names object `id` at a byte offset in `start..end`.
fn named_elsewhere(map: &ferrix_vma::AddressSpace, id: u64, start: u64, end: u64) -> bool {
    map.iter().any(|region| match region.backing {
        Backing::Anonymous { id: named, offset } if named == id => {
            offset < end && start < offset.saturating_add(region.range.bytes())
        }
        _ => false,
    })
}

/// The rule [`AddressSpace::fault`] is resolved under, checked where it is
/// resolved: no lock that disables preemption is held.
///
/// Resolving a fault takes this space's lock, allocates, and for a
/// copy-on-write page asks for a shootdown and waits for it; once memory is
/// reclaimed it may wait for that too. A caller holding a lock reads and writes
/// through [`AddressSpace::with_present_page`] instead and faults with the
/// lock let go, as `futex` and `uaccess::copy_to_user_present`'s callers do.
/// Checked on every fault rather than only the ones that happen to wait, so
/// that a read under a lock is named by the first boot that makes one, not by
/// the first boot that reclaims a page in the middle of one.
fn fault_requested() {
    // Which processor and its counts in one read: a fault is resolved with
    // preemption on, so a check that read them apart could be moved between
    // them and judge this fault by another processor's count.
    let Some((cpu, held, site)) = crate::sched::locks_here() else {
        return;
    };
    debug_assert!(
        held == 0,
        "a user page fault was resolved on processor {cpu} holding {held} lock(s) that disable \
         preemption, the outermost taken at {}:{}",
        site.map_or("?", |site| site.file()),
        site.map_or(0, core::panic::Location::line),
    );
}

/// Whether a region with `flags` permits `access`.
fn permits(flags: VmaFlags, access: Access) -> bool {
    if access.write && !flags.write {
        return false;
    }
    if access.execute && !flags.execute {
        return false;
    }
    // A plain read needs a readable region. Without this, a read of a
    // `PROT_NONE` region -- every guard page, and every `mprotect` to nothing
    // -- commits a zero page and maps it, handing the program memory where it
    // should get `SIGSEGV`. Silent in the way a mapping more permissive than
    // the map always is: nothing that worked stops working, so nothing
    // notices.
    //
    // A writable region reads, as Linux's `protection_map` makes a
    // `PROT_WRITE` mapping readable on every architecture this kernel runs
    // on, none of whose tables can say write-only (`L.user.129`). wineserver
    // maps its user shared data `PROT_WRITE` alone and reads it back.
    access.write || access.execute || flags.read || flags.write
}

/// Whether object `id` is named by a region that shares it.
///
/// One region is enough. `MAP_SHARED` is a property of the mapping rather than
/// of the object, so in principle an object could be mapped shared in one
/// place and private in another; when that happens the object is the shared
/// one and the private mapping of it has to see the shared writes, because
/// that is what the other mapping was promised.
fn shared_object(map: &ferrix_vma::AddressSpace, id: u64) -> bool {
    map.iter().any(|region| {
        region.flags.shared
            && match region.backing {
                Backing::Anonymous { id: named, .. } | Backing::File { id: named, .. } => {
                    named == id
                }
                Backing::Device { .. } => false,
            }
    })
}

// ---------------------------------------------------------------------------
// What `mmap`, `munmap` and `mprotect` need on top of the above.
//
// Kept in an impl block of its own because it was added by stage 7 against a
// stage 6 interface that was already working: nothing here changes the
// behaviour of anything above it.
// ---------------------------------------------------------------------------

/// What [`AddressSpace::map_device`] and [`AddressSpace::map_window`] refuse
/// before looking at the map: a physical range that wraps the address space
/// as a bad range, first, so that nothing after reasons about a wrapped end
/// -- the image test clamps it, and would call a range clear of an image it
/// had wrapped over; execute, or the kernel's image, as `Refused(refused_at)`;
/// and a length or address that is not whole pages as a bad range.
fn check_device_range(
    physical: u64,
    len: u64,
    flags: VmaFlags,
    refused_at: u64,
) -> Result<(), SpaceError> {
    if physical.checked_add(len).is_none() {
        return Err(SpaceError::BadRange);
    }
    if flags.execute || mm::overlaps_image(physical, len) {
        return Err(SpaceError::Refused(refused_at));
    }
    if len == 0 || !len.is_multiple_of(PAGE_SIZE) || !physical.is_multiple_of(PAGE_SIZE) {
        return Err(SpaceError::BadRange);
    }
    Ok(())
}

/// Hold `physical..physical + len` as `kind` for a device region
/// ([`memory_type::hold`]).
fn hold_type(
    physical: u64,
    len: u64,
    kind: MemoryType,
) -> Result<Arc<memory_type::Hold>, SpaceError> {
    memory_type::hold(physical, len, kind).map_err(|refused| match refused {
        memory_type::Refused::OtherType => SpaceError::OtherMemoryType,
        memory_type::Refused::NoMemory => SpaceError::OutOfMemory,
    })
}

/// The pages of a window [`AddressSpace::map_window`] maps: from
/// `physical`, inside the window `whole..whole + whole_len` -- a render
/// node's whole blob -- over which its memory type is held.
#[derive(Clone, Copy, Debug)]
pub struct WindowPages {
    /// The first page mapped.
    pub physical: u64,
    /// The window's first page.
    pub whole: u64,
    /// The window's length, whole pages.
    pub whole_len: u64,
}

impl WindowPages {
    /// `len` bytes from `physical`, a window of their own.
    pub const fn all(physical: u64, len: u64) -> WindowPages {
        WindowPages {
            physical,
            whole: physical,
            whole_len: len,
        }
    }

    /// `len` bytes from `physical`, and the whole window, each as
    /// [`check_device_range`] asks, and the first inside the second.
    fn check(self, len: u64, flags: VmaFlags) -> Result<(), SpaceError> {
        check_device_range(self.physical, len, flags, 0)?;
        check_device_range(self.whole, self.whole_len, flags, 0)?;
        let end = self.physical.checked_add(len);
        let whole_end = self.whole.checked_add(self.whole_len);
        match (end, whole_end) {
            (Some(end), Some(whole_end)) if self.physical >= self.whole && end <= whole_end => {
                Ok(())
            }
            _ => Err(SpaceError::BadRange),
        }
    }
}

/// What a window's region keeps under its id: its keeper, and its memory
/// type's hold.
#[derive(Debug)]
struct WindowKept {
    /// What the caller asked to have kept: the blob, for a render node.
    #[expect(dead_code, reason = "kept for its drop, never read")]
    keeper: Arc<dyn Any + Send + Sync>,
    /// The memory type of its pages.
    #[expect(dead_code, reason = "kept for its drop, never read")]
    held: Arc<memory_type::Hold>,
}

impl AddressSpace {
    /// Place `len` bytes of device memory, starting at physical address
    /// `physical`, in this space: at `at` if one is given, otherwise wherever
    /// it fits. Returns where it went.
    ///
    /// What `io_mapping_map` does with an aperture. The region is backed by
    /// the device itself, [`Backing::Device`], so nothing is committed or
    /// copied: its pages are translated on first touch, by
    /// [`AddressSpace::fault`], to the device's own frames, uncached. It is
    /// shared, so a `fork` child reaches the same registers rather than a copy
    /// of them, and never executable, because nobody runs code out of a
    /// register window.
    ///
    /// Whether `physical..physical + len` is memory this space may have at
    /// all is decided by the caller, which holds an `Aperture` that says so.
    /// The one range refused here whatever the caller holds is the kernel's
    /// own image ([`mm::overlaps_image`]), whose text has no writable mapping
    /// anywhere.
    ///
    /// The region holds its range's memory type, device or write-combining
    /// if `combining`, until no region of any space maps it any more
    /// ([`memory_type`]): one memory type per device page.
    ///
    /// # Errors
    ///
    /// [`SpaceError::Refused`] if `flags` asks for execute, or the range
    /// touches the kernel's image;
    /// [`SpaceError::OtherMemoryType`] if a page of the range is mapped with
    /// another memory type;
    /// [`SpaceError::BadRange`] for a physical range that wraps the address
    /// space, a length or physical address that is not whole pages, or a
    /// range overlapping a mapping;
    /// [`SpaceError::NotUserRange`] outside the user half; and
    /// [`SpaceError::OutOfMemory`] if no gap that large is free.
    pub(crate) fn map_device(
        &self,
        at: Option<u64>,
        len: u64,
        physical: u64,
        flags: VmaFlags,
        combining: bool,
    ) -> Result<u64, SpaceError> {
        check_device_range(physical, len, flags, at.unwrap_or(0))?;
        let held: Arc<dyn Any + Send + Sync> =
            hold_type(physical, len, MemoryType::of(false, combining))?;
        let mut inner = self.inner.lock();
        let at = match at {
            Some(at) => at,
            None => inner
                .map
                .find_free(len, PAGE_SIZE, None)
                .ok_or(SpaceError::OutOfMemory)?,
        };
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        // An id, as a window's region has, so the hold is kept beside it.
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        let flags = VmaFlags {
            execute: false,
            shared: true,
            grows_down: false,
            ..flags
        };
        let reserved = fallible::reserve().map_err(|_| SpaceError::OutOfMemory)?;
        inner
            .map
            // FALLIBLE: the map's insert refuses with `VmaError::NoMemory`.
            .insert(
                range,
                flags,
                Backing::Device {
                    physical,
                    id,
                    cached: false,
                    combining,
                },
            )
            .map_err(map_error)?;
        let _ = fallible::insert_held(
            &reserved,
            &mut inner.files,
            id,
            FileMapping {
                file: held,
                may_write: false,
            },
        );
        Ok(at)
    }

    /// Map `len` bytes of a window of device memory at `physical`, which
    /// `keeper` keeps the program's for as long as a region maps it, at
    /// `place`. Returns where it went.
    ///
    /// What `mmap` of a GPU's blob does (`docs/GPU.md` §6.1). A window is
    /// memory the device shares -- a BAR the host maps blob resources into --
    /// rather than registers, so the device may say it is cached; and unlike
    /// registers, which a driver holds for its whole life, a blob can be given
    /// back while a program still maps it. So the region is given an id, as a
    /// file mapping is, and `keeper` is kept under that id beside the file
    /// mappings' own, until no region names it: until then nothing can hand
    /// the window's pages to anyone else. A `fork` child keeps it too. Kept
    /// with it is the range's memory type, cached if `cached` and device
    /// memory otherwise, as [`AddressSpace::map_device`] holds its own. The
    /// type is held over the whole window `pages` names, not only the part
    /// mapped, so mapping parts of one window adds no claim
    /// ([`memory_type`]'s bound).
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::map_device`], and [`SpaceError::BadRange`] for
    /// pages outside their window.
    pub fn map_window(
        &self,
        place: FilePlace,
        len: u64,
        pages: WindowPages,
        flags: VmaFlags,
        cached: bool,
        keeper: Arc<dyn Any + Send + Sync>,
    ) -> Result<u64, SpaceError> {
        self.map_window_typed(
            place,
            len,
            pages,
            flags,
            MemoryType::of(cached, false),
            keeper,
        )
    }

    /// [`AddressSpace::map_window`] with the memory type `memory`, which may
    /// be write-combining: a prefetchable aperture a chardev node's driver
    /// answered an mmap with (`docs/NVIDIA.md` §4.4). The type is held over
    /// the window as for any window, so a page some other mapping holds with
    /// another type is refused, never changed, and the mapping records the
    /// same type it holds.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::map_window`].
    pub fn map_window_typed(
        &self,
        place: FilePlace,
        len: u64,
        pages: WindowPages,
        flags: VmaFlags,
        memory: MemoryType,
        keeper: Arc<dyn Any + Send + Sync>,
    ) -> Result<u64, SpaceError> {
        pages.check(len, flags)?;
        let physical = pages.physical;
        let held = hold_type(pages.whole, pages.whole_len, memory)?;
        let keeper: Arc<dyn Any + Send + Sync> =
            fallible::try_arc(WindowKept { keeper, held }).map_err(|_| SpaceError::OutOfMemory)?;
        let mut inner = self.inner.lock();
        let at = match place {
            FilePlace::Fixed(at) => at,
            FilePlace::Anywhere(hint) => inner
                .map
                .find_free(len, PAGE_SIZE, hint)
                .ok_or(SpaceError::OutOfMemory)?,
        };
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        let flags = VmaFlags {
            execute: false,
            shared: true,
            grows_down: false,
            ..flags
        };
        let held = fallible::reserve().map_err(|_| SpaceError::OutOfMemory)?;
        inner
            .map
            // FALLIBLE: the map's insert refuses with `VmaError::NoMemory`.
            .insert(
                range,
                flags,
                Backing::Device {
                    physical,
                    id,
                    cached: matches!(memory, MemoryType::Cached),
                    combining: matches!(memory, MemoryType::Combining),
                },
            )
            .map_err(map_error)?;
        let _ = fallible::insert_held(
            &held,
            &mut inner.files,
            id,
            FileMapping {
                file: keeper,
                may_write: false,
            },
        );
        Ok(at)
    }

    /// Translate one page of a device region to the device's own page.
    ///
    /// Called with the address space's lock held, as the anonymous path maps,
    /// so two processors faulting the same page cannot both install it. A page
    /// already present was resolved by another processor first, and the
    /// faulting instruction retries and succeeds, for the reason the anonymous
    /// path gives.
    fn fault_device(
        &self,
        page: u64,
        into_region: u64,
        flags: VmaFlags,
        backing: Backing,
    ) -> Result<(), SpaceError> {
        let Backing::Device {
            physical,
            cached,
            combining,
            ..
        } = backing
        else {
            return Err(SpaceError::BadRange);
        };
        let physical = physical.saturating_add(into_region);
        if master_translate!(self, self.root * PAGE_SIZE, page).is_some() {
            return Ok(());
        }
        mm::map_in(
            self.root * PAGE_SIZE,
            page,
            physical,
            PAGE_SIZE,
            MapFlags {
                read: flags.read,
                write: flags.write,
                execute: false,
                user: true,
                global: false,
                // Registers are device memory. A window the device said may
                // be cached is ordinary memory to the processor, and one a
                // driver asked to combine is write-combining.
                device: !cached && !combining,
                uncached: false,
                write_combining: combining && !cached,
            },
        )
        .map_err(|_| SpaceError::OutOfMemory)
    }

    /// Reserve `len` bytes wherever they fit, and say where that was.
    ///
    /// `mmap` with a null address. The search and the insertion happen under
    /// one lock, which is the whole reason this is a method rather than
    /// `find_free` followed by [`AddressSpace::map_anonymous`]: two threads
    /// calling `mmap` at once would otherwise be told about the same hole and
    /// the second insertion would fail, or worse, succeed.
    ///
    /// `hint` is advisory. A program that passes a non-null address without
    /// `MAP_FIXED` is asking, not telling, and Linux is free to answer
    /// somewhere else — so a hint that does not fit is not an error.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] if the length is malformed, and
    /// [`SpaceError::OutOfMemory`] if there is no hole big enough.
    pub(crate) fn map_anywhere(
        &self,
        hint: Option<u64>,
        len: u64,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        if len == 0 {
            return Err(SpaceError::BadRange);
        }
        let mut inner = self.inner.lock();

        let at = inner
            .map
            .find_free(len, PAGE_SIZE, hint)
            .ok_or(SpaceError::OutOfMemory)?;
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        let _ = self.map_fresh(&mut inner, range, flags)?;
        Ok(at)
    }

    /// [`AddressSpace::map_by_handle`] with the ceiling `flags` themselves
    /// set and never executable: what `vmo_map`'s callers inside the kernel
    /// map with, a chardev driver's shared VMO among them.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::map_by_handle`]: an executable mapping is refused.
    pub fn map_object(
        &self,
        at: Option<u64>,
        len: u64,
        vmo: Arc<Vmo>,
        offset: u64,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        let ceiling = Ceiling::Handle {
            write: flags.write,
            execute: false,
        };
        self.map_by_handle(at, len, vmo, offset, flags, ceiling)
    }

    /// Someone reached in who may not, so it never passes K14's grant.
    pub fn taint(&self) {
        let _ = self.reach.fetch_or(REACH_TAINTED, Ordering::AcqRel);
    }

    /// Whether anyone reached in who may not.
    pub fn tainted(&self) -> bool {
        self.reach.load(Ordering::Acquire) & REACH_TAINTED != 0
    }

    /// Its process is about to run: a change from outside taints from now on.
    pub fn mark_started(&self) {
        let _ = self.reach.fetch_or(REACH_STARTED, Ordering::AcqRel);
    }

    /// Whether its process has been started.
    pub fn started(&self) -> bool {
        self.reach.load(Ordering::Acquire) & REACH_STARTED != 0
    }

    /// It passed K14's grant, if nothing has tainted it, in one step with the taint
    /// (ledger 6316 P2.2): a change from outside, or an unlisted image, is refused
    /// from now on, rather than tainting (§5). Answers whether it was granted.
    pub fn try_grant(&self) -> bool {
        self.reach
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & (REACH_TAINTED | REACH_UNLISTED) == 0).then_some(state | REACH_GRANTED)
            })
            .is_ok()
    }

    /// Set `mark`, unless granted, in one step with the grant. False when granted.
    pub(crate) fn mark_unless_granted(&self, mark: u8) -> bool {
        self.reach
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & REACH_GRANTED == 0).then_some(state | mark)
            })
            .is_ok()
    }

    /// As [`Self::taint`], unless granted: a holder that does not pass, whose supervision
    /// a grant forbids. False when granted.
    pub fn taint_unless_granted(&self) -> bool {
        self.mark_unless_granted(REACH_TAINTED)
    }

    /// A by-handle change arrives from outside: refused once granted, in one step
    /// with the grant; otherwise a taint if it has started or the caller does not pass.
    /// False when refused.
    pub fn reached(&self, caller_passes: bool) -> bool {
        self.reach
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if state & REACH_GRANTED != 0 {
                    None
                } else if state & REACH_STARTED != 0 || !caller_passes {
                    Some(state | REACH_TAINTED)
                } else {
                    Some(state)
                }
            })
            .is_ok()
    }

    /// Whether it was granted a device.
    pub fn granted(&self) -> bool {
        self.reach.load(Ordering::Acquire) & REACH_GRANTED != 0
    }

    /// K14's pass (`docs/microkernel/K14.md` §5): measured, nobody reached in, at
    /// least one image, and every image it was given on the certified list.
    pub fn passes(&self) -> bool {
        if !self.is_measured()
            || self.reach.load(Ordering::Acquire) & (REACH_TAINTED | REACH_UNLISTED) != 0
        {
            return false;
        }
        self.inner
            .lock()
            .images
            .as_ref()
            .is_some_and(|images| !images.digests().is_empty())
    }

    /// Whether its executable memory comes only from sealed VMOs.
    pub fn is_measured(&self) -> bool {
        self.measured.load(Ordering::Acquire)
    }

    /// Hold it to that from now on. Before anything is mapped in it through
    /// a handle, by whoever makes it; there is no way back.
    pub fn measure(&self) {
        self.measured.store(true, Ordering::Release);
    }

    /// Record `digest` as something this space was given to run, by the path
    /// that loads an image into it without a map by handle (`process_create`
    /// until K15).
    ///
    /// # Errors
    ///
    /// [`SpaceError::OutOfMemory`] or [`SpaceError::ImagesFull`].
    pub fn note_image(&self, digest: [u8; 32]) -> Result<(), SpaceError> {
        if !certified::listed(&digest) {
            let _ = self.reach.fetch_or(REACH_UNLISTED, Ordering::AcqRel);
        }
        let room = Images::boxed().ok_or(SpaceError::OutOfMemory)?;
        if self.inner.lock().note_image(digest, Some(room)) {
            Ok(())
        } else {
            Err(SpaceError::ImagesFull)
        }
    }

    /// Copy its record of images out, for `process_images`: as many as `out`
    /// holds. Answers how many there are.
    pub fn images_into(&self, out: &mut [[u8; 32]]) -> usize {
        let inner = self.inner.lock();
        let Some(images) = inner.images.as_ref() else {
            return 0;
        };
        for (slot, digest) in out.iter_mut().zip(images.digests()) {
            *slot = *digest;
        }
        images.digests().len()
    }

    /// Map `len` bytes of `vmo`, from byte `offset`, at `at` or wherever there
    /// is room: what `vmo_map` and `space_map` do. `ceiling` is what the VMO's
    /// handle allowed (`docs/microkernel/K2.md` §2.3).
    ///
    /// Shared, always. The region names the object itself, not a copy of it,
    /// so a write through it is a write every handle and every other mapping
    /// of the object sees, and `fork` shares the object rather than marking
    /// the region copy-on-write. The region keeps the object alive, so the
    /// mapping outlives the handle it was made with; and a handle keeps it as
    /// well, so an object a handle still names keeps its pages when its last
    /// mapping goes, and `vmo_read` still finds them. Nothing is committed
    /// here: pages arrive on first touch, as the object's own. `mremap` and
    /// `mprotect` refuse the region: the object's size is its handle's to set,
    /// and the protection stands for rights the handle carried.
    ///
    /// Attached to the object before the object is recorded here, as every
    /// region is, so that an object taking a page away finds this space among
    /// its mappers and forgets the page here before its frame goes back.
    ///
    /// # Errors
    ///
    /// [`SpaceError::Refused`] for flags past `ceiling`, and always for a
    /// mapping both writable and executable (ASR-2: refused here, under no
    /// lock yet, whatever the caller decoded);
    /// [`SpaceError::BadRange`] for a length or offset that is not whole
    /// pages, a range past the object's end, or one that overlaps a mapping;
    /// [`SpaceError::NotUserRange`] outside the user half; and
    /// [`SpaceError::OutOfMemory`] if no free range is long enough.
    pub fn map_by_handle(
        &self,
        at: Option<u64>,
        len: u64,
        vmo: Arc<Vmo>,
        offset: u64,
        flags: VmaFlags,
        ceiling: Ceiling,
    ) -> Result<u64, SpaceError> {
        let flags = VmaFlags {
            read: true,
            shared: true,
            grows_down: false,
            ..flags
        };
        if !ceiling.admits(flags) {
            return Err(SpaceError::Refused(at.unwrap_or(0)));
        }
        // `may_execute` (`docs/microkernel/K14.md` §3): by the ceiling, not
        // only the flags, so no later protect finds an unmeasured region it
        // may raise. The room for the record is had before the lock.
        let runs = flags.execute || matches!(ceiling, Ceiling::Handle { execute: true, .. });
        let digest = if runs { vmo.digest() } else { None };
        if runs && digest.is_none() && self.is_measured() {
            return Err(SpaceError::Unmeasured);
        }
        // After a grant, only what the list names (§5). Before one, a sticky
        // mark: a pass is then one load, and no walk of the record.
        if let Some(digest) = digest.filter(|digest| !certified::listed(digest)) {
            let _ = digest;
            if !self.mark_unless_granted(REACH_UNLISTED) {
                return Err(SpaceError::Unmeasured);
            }
        }
        let room = digest
            .is_some()
            .then(|| Images::boxed().ok_or(SpaceError::OutOfMemory))
            .transpose()?;
        if len == 0
            || !len.is_multiple_of(PAGE_SIZE)
            || !offset.is_multiple_of(PAGE_SIZE)
            || offset
                .checked_add(len)
                .is_none_or(|end| end > vmo.len_bytes())
        {
            return Err(SpaceError::BadRange);
        }
        let mut inner = self.inner.lock();
        let at = match at {
            Some(at) => at,
            None => inner
                .map
                .find_free(len, PAGE_SIZE, None)
                .ok_or(SpaceError::OutOfMemory)?,
        };
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        // A second handle's map racing a seal: one of them is refused here,
        // under the object's lock.
        let writable = matches!(ceiling, Ceiling::Handle { write: true, .. });
        if writable {
            vmo.raise_writable_region().map_err(SpaceError::Backing)?;
        }
        let counted = Arc::clone(&vmo);
        let uncount = |counted: &Vmo| {
            if writable {
                counted.lower_writable_region();
            }
        };
        if let Err(why) = self.add_region(
            &mut inner,
            NewRegion {
                id,
                range,
                flags,
                backing: Backing::Anonymous { id, offset },
                sharing: Sharing::Shared,
            },
            vmo,
        ) {
            uncount(&counted);
            return Err(why);
        }
        if fallible::insert(&mut inner.native, id, ceiling).is_err() {
            uncount(&counted);
            self.abandon_region(&mut inner, id, range);
            return Err(SpaceError::OutOfMemory);
        }
        // What ran stays recorded; noted last, so a map refused above left
        // no digest of something that never ran.
        if let Some(digest) = digest
            && !inner.note_image(digest, room)
        {
            uncount(&counted);
            let _ = inner.native.remove(&id);
            self.abandon_region(&mut inner, id, range);
            return Err(SpaceError::ImagesFull);
        }
        Ok(at)
    }

    /// Map the one page of `vmo` wherever there is room, to be read and run
    /// and never written: code the kernel gives every process with no data
    /// beside it. What ARMv7-A's signal return page is mapped with, as
    /// Linux's `sigpage`: the vDSO's placement, and none of its data page.
    /// Returns where it went.
    ///
    /// Like [`AddressSpace::map_shared_code`], the object is the kernel's,
    /// shared by every process, so the region is shared, `fork` keeps it where
    /// it was, `mprotect` and `mremap` refuse it, and a program may unmap it
    /// and loses only its own view.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] for an object that is not one page, and
    /// [`SpaceError::OutOfMemory`] if there is no room.
    pub fn map_code_page(&self, vmo: Arc<Vmo>) -> Result<u64, SpaceError> {
        if vmo.len_bytes() != PAGE_SIZE {
            return Err(SpaceError::BadRange);
        }
        let mut inner = self.inner.lock();
        let at = inner
            .map
            .find_free(PAGE_SIZE, PAGE_SIZE, None)
            .ok_or(SpaceError::OutOfMemory)?;
        if !is_user_address(at)
            || at
                .checked_add(PAGE_SIZE)
                .is_none_or(|end| end > USER_VIRT_END)
        {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, PAGE_SIZE).map_err(|_| SpaceError::BadRange)?;
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        self.add_region(
            &mut inner,
            NewRegion {
                id,
                range,
                flags: VmaFlags {
                    shared: true,
                    execute: true,
                    ..VmaFlags::READ
                },
                backing: Backing::Anonymous { id, offset: 0 },
                sharing: Sharing::Shared,
            },
            vmo,
        )?;
        if fallible::insert(&mut inner.native, id, Ceiling::Kernel).is_err() {
            self.abandon_region(&mut inner, id, range);
            return Err(SpaceError::OutOfMemory);
        }
        Ok(at)
    }

    /// Map the two pages of `vmo`, ending at `end` if that is free and
    /// wherever there is room if it is not: the first to be read, the second
    /// to be read and run. What the Linux personality maps its vDSO with, a
    /// data page and the code that reads it. Returns where the second page
    /// went.
    ///
    /// The object is the kernel's, one for every process, so the region may
    /// never be written through: it is shared, so `fork` shares it rather
    /// than copying, and counted with the objects `vmo_map` puts here, which
    /// `mprotect` and `mremap` refuse and `madvise` takes no page out of. A
    /// program may unmap it, and loses only its own view.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] for an object that is not two pages, and
    /// [`SpaceError::OutOfMemory`] if there is no room.
    /// Where [`AddressSpace::map_shared_code`] or
    /// [`AddressSpace::map_code_page`] put `vmo` in this space: its first
    /// page's address, or `None` if it is not mapped here. How a signal's
    /// delivery finds the vDSO's return trampoline, or ARMv7-A's return page,
    /// which the space maps once at exec and a fork keeps where it was.
    pub fn shared_code_at(&self, vmo: &Arc<Vmo>) -> Option<u64> {
        let inner = self.inner.lock();
        let id = inner
            .objects
            .iter()
            .find_map(|(&id, object)| Arc::ptr_eq(object, vmo).then_some(id))?;
        inner.map.iter().find_map(|region| {
            matches!(region.backing, Backing::Anonymous { id: named, offset: 0 } if named == id)
                .then(|| region.range.start())
        })
    }

    /// Map `vmo`'s two pages shared below `end`, the first read-only and the
    /// second read and execute (a data page and the code beside it), and
    /// answer where.
    ///
    /// # Errors
    ///
    /// A VMO that is not two pages, no room below `end`, or no memory.
    pub fn map_shared_code(&self, vmo: Arc<Vmo>, end: u64) -> Result<u64, SpaceError> {
        self.map_shared_code_near(vmo, end.checked_sub(2 * PAGE_SIZE))
    }

    /// [`AddressSpace::map_shared_code`] at `hint` if the room is free there.
    fn map_shared_code_near(&self, vmo: Arc<Vmo>, hint: Option<u64>) -> Result<u64, SpaceError> {
        let len = 2 * PAGE_SIZE;
        if vmo.len_bytes() != len {
            return Err(SpaceError::BadRange);
        }
        let mut inner = self.inner.lock();
        let at = inner
            .map
            .find_free(len, PAGE_SIZE, hint)
            .ok_or(SpaceError::OutOfMemory)?;
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let data = PageRange::from_len(at, PAGE_SIZE).map_err(|_| SpaceError::BadRange)?;
        let code =
            PageRange::from_len(at + PAGE_SIZE, PAGE_SIZE).map_err(|_| SpaceError::BadRange)?;
        let shared = VmaFlags {
            shared: true,
            ..VmaFlags::READ
        };
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        self.add_region(
            &mut inner,
            NewRegion {
                id,
                range: data,
                flags: shared,
                backing: Backing::Anonymous { id, offset: 0 },
                sharing: Sharing::Shared,
            },
            vmo,
        )?;
        let runnable = VmaFlags {
            execute: true,
            ..shared
        };
        let code_backing = Backing::Anonymous {
            id,
            offset: PAGE_SIZE,
        };
        // Counted in before the map takes it, as `add_region` counts.
        let counted = self.count_in(&inner, code, code_backing, runnable);
        let inserted = counted.and_then(|()| {
            inner
                .map
                .insert(code, runnable, code_backing) // FALLIBLE: refuses with `NoMemory`.
                .map_err(map_error)
        });
        if inserted.is_err() || fallible::insert(&mut inner.native, id, Ceiling::Kernel).is_err() {
            if counted.is_ok() {
                self.count_out(&inner, code, code_backing, runnable);
            }
            if inserted.is_ok() {
                let _ = inner.map.remove_quietly(code);
            }
            self.abandon_region(&mut inner, id, data);
            return Err(SpaceError::OutOfMemory);
        }
        Ok(at + PAGE_SIZE)
    }

    /// Give the space another view of the kernel's two-page code object that
    /// it maps at `at` (as [`AddressSpace::shared_code_at`] says): unmapped,
    /// and `vmo` mapped again at the same place. How a fork whose child is in
    /// another time namespace than its parent swaps the vDSO's data page for
    /// the one that makes every clock a system call, at the address its
    /// auxiliary vector already named (`docs/NAMESPACES.md` §12.1).
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::map_shared_code`], and [`SpaceError::BadRange`] if
    /// the object did not land where the old one was, which is unmapped again
    /// then. The space is without the view on any error, and the caller must
    /// not run it.
    ///
    /// For a space no thread runs yet: a fork's child or a native process
    /// being loaded, both of which are abandoned on an error.
    pub fn replace_shared_code(&self, at: u64, vmo: Arc<Vmo>) -> Result<(), SpaceError> {
        let _layout = self.layout();
        self.unmap(at, 2 * PAGE_SIZE)?;
        let code = self.map_shared_code_near(vmo, Some(at))?;
        if code != at + PAGE_SIZE {
            let _ = self.unmap(code - PAGE_SIZE, 2 * PAGE_SIZE);
            return Err(SpaceError::BadRange);
        }
        Ok(())
    }

    /// Map `len` bytes of `vmo`, a file's object, from byte `offset` of it.
    /// Shared, the region shows the file's own pages, and a write through it
    /// is a write to the file. Private, it shows the same pages until it
    /// writes one, and that write copies the page into a shadow object of the
    /// mapping's own, which the file never sees. `file` is kept for as long
    /// as a region names the mapping, and names it. Returns where it went.
    ///
    /// `place` is [`FilePlace::Fixed`] for `MAP_FIXED`, whose range the caller
    /// has already cleared, or [`FilePlace::Anywhere`] with the program's hint.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::map_anonymous`] and [`AddressSpace::map_anywhere`],
    /// and [`SpaceError::BadRange`] for an offset that is not whole pages or a
    /// range that would leave the object.
    pub fn map_file(
        &self,
        place: FilePlace,
        len: u64,
        flags: VmaFlags,
        vmo: Arc<Vmo>,
        offset: u64,
        mapping: FileMapping,
    ) -> Result<u64, SpaceError> {
        if len == 0
            || !offset.is_multiple_of(PAGE_SIZE)
            || offset
                .checked_add(len)
                .is_none_or(|end| end.div_ceil(PAGE_SIZE) > vmo.len_pages())
        {
            return Err(SpaceError::BadRange);
        }
        let mut inner = self.inner.lock();
        let range = file_placement(&inner.map, place, len)?;
        let at = range.start();

        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        // The shadow a private mapping writes into, made before anything is
        // changed.
        let shadow = if flags.shared {
            None
        } else {
            let pages = offset.saturating_add(len).div_ceil(PAGE_SIZE);
            Some(Vmo::new_anonymous(pages).map_err(|_| SpaceError::OutOfMemory)?)
        };
        let may_write = mapping.may_write;
        let file = Arc::clone(&vmo);
        self.add_region(
            &mut inner,
            NewRegion {
                id,
                range,
                flags,
                backing: Backing::File { id, offset },
                sharing: Sharing::Shared,
            },
            vmo,
        )?;
        if let Err(mapping) = self.add_file(&mut inner, id, shadow, mapping) {
            self.abandon_region(&mut inner, id, range);
            // The mapping's file goes after the lock: it may hold the last
            // reference to an inode.
            drop(inner);
            drop(mapping);
            return Err(SpaceError::OutOfMemory);
        }
        // A private mapping is counted once its shadow is in the tables:
        // the file's view masks what the shadow holds (`user::rss`).
        if !flags.shared
            && let Err(why) = self.count_in(&inner, range, Backing::File { id, offset }, flags)
        {
            let mapping = inner.files.remove(&id);
            let shadow = inner.shadows.remove(&id);
            if let Some(shadow) = &shadow {
                shadow.forget_views(&self.rss, Some(id));
                shadow.detach(self, id);
            }
            self.abandon_region(&mut inner, id, range);
            drop(inner);
            drop((mapping, shadow));
            return Err(why);
        }
        if may_write {
            file.raise_shared_may_write();
        }
        Ok(at)
    }

    /// Record a file mapping's shadow, if it is private, and the mapping
    /// itself under `id`. On refusal nothing of either is recorded, the
    /// shadow is detached again, and the mapping comes back, for the caller
    /// to drop once its lock is gone.
    fn add_file(
        &self,
        inner: &mut Inner,
        id: u64,
        shadow: Option<Arc<Vmo>>,
        mapping: FileMapping,
    ) -> Result<(), FileMapping> {
        if let Some(shadow) = shadow {
            if shadow
                .attach(self.me.clone(), id, Sharing::Private)
                .is_err()
            {
                return Err(mapping);
            }
            let Ok(held) = fallible::reserve() else {
                shadow.detach(self, id);
                return Err(mapping);
            };
            let _ = fallible::insert_held(&held, &mut inner.shadows, id, shadow);
        }
        let Ok(held) = fallible::reserve() else {
            if let Some(shadow) = inner.shadows.remove(&id) {
                shadow.detach(self, id);
            }
            return Err(mapping);
        };
        let _ = fallible::insert_held(&held, &mut inner.files, id, mapping);
        Ok(())
    }

    /// The open file a file mapping's region names by `id`, for
    /// `/proc/<pid>/maps`.
    pub fn mapped_file(&self, id: u64) -> Option<Arc<dyn Any + Send + Sync>> {
        self.inner
            .lock()
            .files
            .get(&id)
            .map(|mapping| Arc::clone(&mapping.file))
    }

    /// Change the permissions of an already-mapped range.
    ///
    /// # Why the translations are taken down rather than rewritten
    ///
    /// The same reason `clone_for_fork` takes the parent's down. The page
    /// tables hold translations carrying the *old* permissions, and a region
    /// that has just become read-only is still writable through every one of
    /// them until something invalidates it. Rewriting each leaf in place would
    /// be faster and is what a later stage should do; unmapping costs one
    /// fault per touched page and is correct with the primitives `crate::mm`
    /// offers today, which is the right trade while there is no benchmark to
    /// answer to.
    ///
    /// Note this is `mprotect`'s semantics and not `mmap`'s: the range must
    /// already be mapped, and a hole in it is an error rather than a
    /// reservation.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] if the range is malformed, or is not wholly
    /// mapped; [`SpaceError::Refused`] if it reaches a region `vmo_map` made,
    /// whose protection is its handle's to grant.
    pub fn protect(&self, at: u64, len: u64, flags: VmaFlags) -> Result<(), SpaceError> {
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        let mut pages = TlbPages::new();
        let cpus = {
            let mut inner = self.inner.lock();

            let Inner {
                map, native, files, ..
            } = &*inner;
            let reaches_native = map.iter().any(|region| {
                region.range.start() < range.end()
                    && range.start() < region.range.end()
                    && matches!(region.backing, Backing::Anonymous { id, .. } if native.contains_key(&id))
            });
            // A shared file mapping that may not write its file -- of a file
            // not open for writing, or made after a seal refused writes -- may
            // not be made writable either: Linux's answer for a mapping without
            // `VM_MAYWRITE`. Refused whole, before anything changes.
            let reaches_unwritable = flags.write
                && map.iter().any(|region| {
                    region.range.start() < range.end()
                        && range.start() < region.range.end()
                        && region.flags.shared
                        && matches!(region.backing, Backing::File { id, .. }
                            if files.get(&id).is_some_and(|mapping| !mapping.may_write))
                });
            if reaches_native || reaches_unwritable {
                return Err(SpaceError::Refused(at));
            }

            inner.map.protect(range, flags).map_err(map_error)?;

            // Every page in the range re-faults and is reinstalled with the
            // permissions the map now carries.
            self.unmap_range(range, &mut pages);
            self.begin_shootdown(&inner)
        };

        // A processor still holding one of the old entries could write through
        // a page just made read-only. And a VMO taking one of these pages away
        // meanwhile finds no translation to forget: the pending count this
        // shootdown holds is what makes it flush the set anyway, rather than
        // release a frame the old entry still reaches.
        self.shoot(&cpus, &mut pages);
        Ok(())
    }

    /// Map `len` bytes of fresh private anonymous memory, zero until written,
    /// at `at` or wherever there is room: what `space_allocate` does
    /// (`docs/microkernel/K2.md` §2.3). The memory is nobody else's, so its
    /// ceiling is read, write and execute, one at a time with the third
    /// (ASR-2, refused here under no lock yet whatever the caller decoded);
    /// a clone copies it on write, and the Linux calls that reshape a region
    /// refuse it, as they refuse every region a handle made.
    ///
    /// # Errors
    ///
    /// [`SpaceError::Refused`] for writable and executable together;
    /// [`SpaceError::BadRange`] for a length that is not whole pages or a
    /// range over a mapping; [`SpaceError::NotUserRange`] outside the user
    /// half; [`SpaceError::OutOfMemory`] for no room or no memory.
    pub fn allocate_by_handle(
        &self,
        at: Option<u64>,
        len: u64,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        let flags = VmaFlags {
            shared: false,
            grows_down: false,
            ..flags
        };
        // Anonymous memory has no measurement, so a measured space's has no
        // executable ceiling either (`docs/microkernel/K14.md` §3).
        if flags.execute && self.is_measured() {
            return Err(SpaceError::Unmeasured);
        }
        let ceiling = Ceiling::Handle {
            write: true,
            execute: !self.is_measured(),
        };
        if !ceiling.admits(flags) {
            return Err(SpaceError::Refused(at.unwrap_or(0)));
        }
        if len == 0 || !len.is_multiple_of(PAGE_SIZE) {
            return Err(SpaceError::BadRange);
        }
        let mut inner = self.inner.lock();
        let at = match at {
            Some(at) => at,
            None => inner
                .map
                .find_free(len, PAGE_SIZE, None)
                .ok_or(SpaceError::OutOfMemory)?,
        };
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        let vmo = Vmo::new_anonymous(len / PAGE_SIZE).map_err(|_| SpaceError::OutOfMemory)?;
        // Its ceiling holds `WRITE`, and its going lowers the count as any
        // such region's. A fresh object: nothing has sealed it.
        vmo.raise_writable_region().map_err(SpaceError::Backing)?;
        self.add_region(
            &mut inner,
            NewRegion {
                id,
                range,
                flags,
                backing: Backing::Anonymous { id, offset: 0 },
                sharing: Sharing::Private,
            },
            vmo,
        )?;
        if fallible::insert(&mut inner.native, id, ceiling).is_err() {
            self.abandon_region(&mut inner, id, range);
            return Err(SpaceError::OutOfMemory);
        }
        Ok(at)
    }

    /// Change the protection of `len` bytes at `at`, every page of which a
    /// handle mapped, to `flags`' read, write and execute: what `space_protect`
    /// does (`docs/microkernel/K2.md` §2.3).
    ///
    /// Each region in the range is held to its [`Ceiling`]: never writable
    /// and executable at once (ASR-2, refused here under the lock whatever
    /// the caller decoded), writable only if its handle carried `WRITE`,
    /// executable only if it carried `EXECUTE`. All or nothing: every region
    /// is checked before the map changes. The translations come down and are
    /// shot down as [`AddressSpace::protect`]'s do, and a private region made
    /// writable is marked copy-on-write by the map (finding F-41).
    ///
    /// # Errors
    ///
    /// [`ProtectRefused::Space`] for a malformed range or one not wholly
    /// mapped; [`ProtectRefused::NotByHandle`] for one that reaches a region no
    /// handle mapped; [`ProtectRefused::Beyond`] for a protection past a
    /// region's ceiling.
    pub fn protect_by_handle(
        &self,
        at: u64,
        len: u64,
        flags: VmaFlags,
    ) -> Result<(), ProtectRefused> {
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(ProtectRefused::Space(SpaceError::NotUserRange(at)));
        }
        let range = PageRange::from_len(at, len)
            .map_err(|_| ProtectRefused::Space(SpaceError::BadRange))?;
        let flags = VmaFlags {
            read: true,
            ..flags
        };
        let mut pages = TlbPages::new();
        let cpus = {
            let mut inner = self.inner.lock();
            within_ceilings(&inner, range, flags)?;
            inner
                .map
                .protect(range, flags)
                .map_err(|why| ProtectRefused::Space(map_error(why)))?;
            self.unmap_range(range, &mut pages);
            self.begin_shootdown(&inner)
        };
        self.shoot(&cpus, &mut pages);
        Ok(())
    }

    /// The highest address any region reaches, or `None` for an empty space.
    ///
    /// `brk` needs it to place the heap above everything the ELF loader
    /// mapped, without the loader and the heap having to agree on a number.
    pub fn highest_mapped(&self) -> Option<u64> {
        self.inner
            .lock()
            .map
            .iter()
            .map(|vma| vma.range.end())
            .max()
    }
}

// ---------------------------------------------------------------------------
// `madvise`: giving pages back without giving the range up.
// ---------------------------------------------------------------------------

/// What `madvise` asks of a range, as far as the address space is concerned.
/// `crate::syscall::memory::sys_madvise` turns Linux's `MADV_*` into these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Advice {
    /// `MADV_DONTNEED`, and with `locked_too` `MADV_DONTNEED_LOCKED`: the
    /// range stays mapped, and its pages go. Private anonymous memory reads
    /// as zeros on its next touch and a private file mapping as the file,
    /// since what it copied goes; a shared mapping only loses its
    /// translations, and reads what the object holds, as on Linux, where the
    /// pages are the page cache's or shmem's and not the mapping's.
    Discard {
        /// Whether an `mlock`ed region is dropped too, rather than refused.
        locked_too: bool,
    },
    /// `MADV_FREE`: [`Advice::Discard`], on private anonymous memory only.
    Free,
    /// `MADV_REMOVE`: punch a hole in what a shared mapping maps, so every
    /// mapping of it reads zeros there.
    Remove,
    /// Advice that changes nothing here, refused only where Linux refuses it:
    /// on a device's registers, which Linux maps `VM_IO | VM_PFNMAP`, when
    /// `not_on_device`, and on an `mlock`ed region when `not_on_locked`.
    Hint {
        /// Refused on a device region.
        not_on_device: bool,
        /// Refused on an `mlock`ed region.
        not_on_locked: bool,
    },
}

/// Why [`AddressSpace::advise`] refused, as `madvise` reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Declined {
    /// Part of the range is mapped by nothing: `ENOMEM`, once every mapped
    /// part has been advised.
    Unmapped,
    /// A region the advice cannot apply to: `EINVAL`.
    Invalid,
    /// A mapping that may not write what it maps: `EACCES`.
    Denied,
    /// What the mapping maps cannot have a hole punched in it here:
    /// `EOPNOTSUPP`, which is what `fallocate` answers for the same hole.
    Unsupported,
    /// No memory to note what the advice takes, before it took anything:
    /// `EAGAIN`, once every part before it has been advised (finding F-23).
    NoMemory,
}

/// What [`AddressSpace::advise_region`] gathers under the lock, to finish
/// once the lock is gone.
struct Advised {
    /// Addresses whose translations came down.
    pages: TlbPages,
    /// Pages taken out of an object, released after the shootdown.
    retiring: Vec<(Arc<Vmo>, Retired)>,
    /// Holes to punch: an object, its first page and how many.
    punching: Vec<(Arc<Vmo>, u64, u64)>,
}

impl Advised {
    /// Room for one more of each, had before anything is taken down.
    fn make_room(&mut self) -> Result<(), Declined> {
        fallible::try_reserve(&mut self.retiring, 1)
            .and_then(|()| fallible::try_reserve(&mut self.punching, 1))
            .map_err(|_| Declined::NoMemory)
    }

    /// Take `count` pages from `first` out of `object`, once there is room to
    /// note them: what comes out, if anything does.
    fn take(
        &mut self,
        object: &Arc<Vmo>,
        first: u64,
        count: u64,
    ) -> Result<Option<(Arc<Vmo>, Retired)>, Declined> {
        self.make_room()?;
        let retired = object
            .take_range(first, count)
            .map_err(|_| Declined::NoMemory)?;
        Ok((!retired.is_empty()).then(|| (Arc::clone(object), retired)))
    }
}

/// The object `MADV_REMOVE` may punch a hole in, for a region naming `id`,
/// or why not.
///
/// Linux's order: no file behind the mapping is `EINVAL` -- private anonymous
/// memory, and here a VMO `vmo_map` put here, whose pages are its handle's --
/// and a mapping that may not write what it maps is `EACCES`.
fn removable(
    inner: &Inner,
    id: u64,
    file: bool,
    shared: bool,
    native: bool,
) -> Result<&Arc<Vmo>, Declined> {
    if !file && (!shared || native) {
        return Err(Declined::Invalid);
    }
    if !shared {
        return Err(Declined::Denied);
    }
    if file {
        let may_write = inner
            .files
            .get(&id)
            .is_some_and(|mapping| mapping.may_write);
        return Err(if may_write {
            Declined::Unsupported
        } else {
            Declined::Denied
        });
    }
    inner.objects.get(&id).ok_or(Declined::Invalid)
}

impl AddressSpace {
    /// Apply `advice` to the `len` bytes at `at`, both whole pages.
    ///
    /// Linux's walk (`madvise_walk_vmas`): every mapped part of the range is
    /// advised, in address order, until a region refuses, whose refusal is
    /// the answer; a part mapped by nothing is [`Declined::Unmapped`] once
    /// the rest has been advised.
    ///
    /// Dropping pages is an unmap without the unmap, in its three phases:
    /// under the lock the translations come down and the pages come out of
    /// the object that owns them -- the region's own for private anonymous
    /// memory, its shadow for a private file mapping, none for a shared
    /// mapping -- and the shootdown is counted pending; the shootdown runs
    /// with the lock let go; and only then are the frames released, through
    /// [`Vmo::retire`] as `give_back` releases them. The region stays, so the
    /// next touch faults and commits a zero page, or shows the file again. A
    /// page that a `fork` left shared keeps its other holder: the reference
    /// this object had is what goes. A page held for a device stays where it
    /// is, contents and all, as `munmap` leaves one.
    ///
    /// A hole is punched in shared anonymous memory by decommitting the
    /// object's pages, which takes them out of every space that maps it
    /// before they go back; that too runs with no lock held. A file cannot
    /// have one punched yet: tmpfs writes into a file's frames under its own
    /// inode lock, which this path does not take, and `fallocate` answers the
    /// same hole with `EOPNOTSUPP` for that reason.
    ///
    /// # Errors
    ///
    /// As [`Declined`].
    pub fn advise(&self, at: u64, len: u64, advice: Advice) -> Result<(), Declined> {
        let end = at.saturating_add(len);
        let mut advised = Advised {
            pages: TlbPages::new(),
            retiring: Vec::new(),
            punching: Vec::new(),
        };
        let (cpus, outcome) = {
            let inner = self.inner.lock();
            let mut covered = at;
            let mut hole = false;
            let mut refused = None;
            for region in inner.map.iter() {
                if region.range.end() <= covered {
                    continue;
                }
                if region.range.start() >= end {
                    break;
                }
                if region.range.start() > covered {
                    hole = true;
                }
                let start = region.range.start().max(covered);
                let stop = region.range.end().min(end);
                if let Err(why) =
                    self.advise_region(&inner, region, (start, stop), advice, &mut advised)
                {
                    refused = Some(why);
                    break;
                }
                covered = stop;
            }
            if covered < end {
                hole = true;
            }
            let outcome = match refused {
                Some(why) => Err(why),
                None if hole => Err(Declined::Unmapped),
                None => Ok(()),
            };
            // Read after the translations came down, under the lock, as every
            // takedown here reads it.
            let cpus = (!advised.pages.is_empty()).then(|| self.begin_shootdown(&inner));
            (cpus, outcome)
        };

        let Advised {
            mut pages,
            retiring,
            punching,
        } = advised;
        if let Some(cpus) = cpus {
            self.shoot(&cpus, &mut pages);
        }
        // This space's translations are down and its shootdown has returned;
        // nothing else maps a private object, so the retirement has nobody to
        // ask and only releases.
        for (vmo, retired) in retiring {
            vmo.retire(
                retired,
                Some(Own {
                    space: self,
                    shootdown: None,
                }),
            );
        }
        for (vmo, first, count) in punching {
            let _ = vmo.decommit_range(first, count);
        }
        outcome
    }

    /// [`AddressSpace::advise`] for the part `start..stop` of `region`, under
    /// the lock `inner` proves: refuse it, or take its translations down and
    /// note what goes back in `advised`.
    fn advise_region(
        &self,
        inner: &Inner,
        region: &Vma,
        (start, stop): (u64, u64),
        advice: Advice,
        advised: &mut Advised,
    ) -> Result<(), Declined> {
        let locked = region.flags.locked;
        let (id, offset, file) = match (advice, region.backing) {
            (
                Advice::Hint {
                    not_on_device,
                    not_on_locked,
                },
                backing,
            ) => {
                let device = matches!(backing, Backing::Device { .. });
                return if (not_on_device && device) || (not_on_locked && locked) {
                    Err(Declined::Invalid)
                } else {
                    Ok(())
                };
            }
            // `VM_LOCKED`, and a device's registers, which Linux maps
            // `VM_PFNMAP`: `madvise_dontneed_free_valid_vma` refuses both,
            // and `MADV_REMOVE` the first.
            (Advice::Discard { locked_too: false } | Advice::Free | Advice::Remove, _)
                if locked =>
            {
                return Err(Declined::Invalid);
            }
            (_, Backing::Device { .. }) => return Err(Declined::Invalid),
            (_, Backing::Anonymous { id, offset }) => (id, offset, false),
            (_, Backing::File { id, offset }) => (id, offset, true),
        };
        let first = offset.saturating_add(start - region.range.start()) / PAGE_SIZE;
        let count = (stop - start) / PAGE_SIZE;
        let native = inner.native.contains_key(&id);
        // A private region of an object some region of this space shares is
        // the shared object's, as `fault` treats it.
        let shared = region.flags.shared || native || (!file && shared_object(&inner.map, id));

        let owner = match advice {
            Advice::Remove => {
                let object = removable(inner, id, file, shared, native)?;
                advised.make_room()?;
                // NOALLOC: `make_room` just made room for it.
                advised.punching.push((Arc::clone(object), first, count));
                return Ok(());
            }
            // `MADV_FREE` is for private anonymous memory only: Linux's
            // `vma_is_anonymous`, which shmem and every file mapping fail.
            Advice::Free if shared || file => return Err(Declined::Invalid),
            _ if shared => None,
            _ if file => inner.shadows.get(&id),
            _ => inner.objects.get(&id),
        };

        // What goes, taken out before the translations come down, and only
        // once there is room to note it: a refusal here has changed nothing.
        let taken = match owner {
            Some(object) => advised.take(object, first, count)?,
            None => None,
        };
        let _ = mm::unmap_in_keeping(
            self.root * PAGE_SIZE,
            start,
            stop - start,
            &mut advised.pages,
            self.keeps_top(),
        );
        if let Some(taken) = taken {
            // NOALLOC: `make_room` made room for it above.
            advised.retiring.push(taken);
        }
        Ok(())
    }
}

/// The object an unmapped range's pages come out of: the id's shadow for a
/// private file mapping, and its object otherwise.
fn owner<'a>(inner: &'a Inner, range: &Freeing) -> Option<&'a Arc<Vmo>> {
    if range.shadow {
        inner.shadows.get(&range.id)
    } else {
        inner.objects.get(&range.id)
    }
}

/// Whether the kernel may copy to or from `region`'s pages through the direct
/// map: every region but a device's.
///
/// A device region's pages are a device's registers or memory -- a BAR, a
/// window of host memory a virtual GPU exposes -- and are not RAM. The direct
/// map translates what the memory map calls memory and nothing between, so
/// the alias [`mm::direct_map`] would form for one is either no translation,
/// and the copy faults in the kernel (F-55), or, for a window lying in memory
/// the direct map does translate, a cacheable alias of memory the program
/// maps uncached. A copy through one is `EFAULT` to the program until copies
/// go through a mapping with the device's own attributes. Asked before any
/// translation is taken, so no direct-map address is ever formed for such a
/// page.
fn copyable(region: &Vma) -> bool {
    !matches!(region.backing, Backing::Device { .. })
}

/// Whether a write at `address` in `region` may go straight through the frame
/// `frame` its translation reaches, or must fault first so the frame is
/// copied.
///
/// One predicate, asked by both [`AddressSpace::with_page`] and
/// [`AddressSpace::with_present_page`], so that the two can never disagree
/// about a page a write must not reach in place.
///
/// Not a copy-on-write page some other space still holds; and in a private
/// file mapping, only the page its shadow holds at that index. Any other frame
/// there is the file's page, shown read-only, and a write through it would
/// reach the file and every other mapping of it.
fn writable_in_place(inner: &Inner, region: &Vma, address: u64, frame: Frame) -> bool {
    if region.cow && mm::frame_references(frame) > 1 {
        return false;
    }
    let into = (address & !(PAGE_SIZE - 1)).saturating_sub(region.range.start());
    match region.backing {
        Backing::File { id, offset } if !region.flags.shared => {
            let index = offset.saturating_add(into) / PAGE_SIZE;
            inner.shadows.get(&id).and_then(|shadow| shadow.page(index)) == Some(frame)
        }
        // A pager-backed object's page only once it is Dirty: a kernel write
        // into a Clean one faults first, so that it is seen (K3.md §2.5).
        Backing::Anonymous { id, offset } | Backing::File { id, offset } => inner
            .objects
            .get(&id)
            .is_none_or(|vmo| vmo.writable_page(offset.saturating_add(into) / PAGE_SIZE)),
        Backing::Device { .. } => true,
    }
}

/// Whether any region still names object `id`.
fn still_named(map: &ferrix_vma::AddressSpace, id: u64) -> bool {
    map.iter().any(|region| naming(region) == Some(id))
}

/// Every id the regions in `removed` named, none yet found, for
/// [`all_still_named`]; `None` with no memory to list them, which only costs
/// [`AddressSpace::give_back`] the walk of the tables.
fn touched_ids(removed: &[Unmapping]) -> Option<Vec<(u64, bool)>> {
    let mut ids = fallible::try_with_capacity(removed.len()).ok()?;
    // NOALLOC: room for one id per removed region was had above.
    ids.extend(
        removed
            .iter()
            .filter_map(|unmapping| match unmapping.backing {
                Backing::Anonymous { id, .. } | Backing::File { id, .. } => Some((id, false)),
                Backing::Device { id, .. } => (id != 0).then_some((id, false)),
            }),
    );
    Some(ids)
}

/// Whether every id in `ids` is still named by a region of `map`: one pass of
/// the map with a binary search per region, so n log k for n regions and k ids,
/// under the space's lock, stopping when each has been found. Not a pass per id,
/// which is what [`Named`] was made to end. `ids` is sorted by id, each once:
/// [`AddressSpace::give_back`] sorts it before it takes the lock.
fn all_still_named(map: &ferrix_vma::AddressSpace, mut ids: Vec<(u64, bool)>) -> bool {
    let mut left = ids.len();
    if left == 0 {
        return true;
    }
    for id in map.iter().filter_map(naming) {
        if let Ok(at) = ids.binary_search_by_key(&id, |&(listed, _)| listed)
            && let Some((_, found)) = ids.get_mut(at)
            && !*found
        {
            *found = true;
            left -= 1;
            if left == 0 {
                return true;
            }
        }
    }
    false
}

/// The object id `region` names, if it names one.
fn naming(region: &Vma) -> Option<u64> {
    match region.backing {
        Backing::Anonymous { id, .. } | Backing::File { id, .. } => Some(id),
        // A window's keeper, and every device region's memory-type hold,
        // is kept under its id.
        Backing::Device { id, .. } => (id != 0).then_some(id),
    }
}

/// Every id a map's regions name, for asking [`still_named`] of many ids at
/// once: one pass over the map and a binary search per id, where asking each
/// id on its own is a pass over the map per id.
enum Named<'a> {
    /// The ids, sorted, each once.
    Sorted(Vec<u64>),
    /// No memory to list them in: each id is asked of the map itself, as
    /// slowly as before and with the same answer (finding F-23).
    Scan(&'a ferrix_vma::AddressSpace),
}

impl<'a> Named<'a> {
    /// The ids `map` names as it stands.
    fn of(map: &'a ferrix_vma::AddressSpace) -> Named<'a> {
        let Ok(mut ids) = fallible::try_with_capacity(map.region_count()) else {
            return Named::Scan(map);
        };
        // NOALLOC: room for one id per region was had above.
        ids.extend(map.iter().filter_map(naming));
        // Not core's `sort_unstable`, which recurses: one id per region the
        // map holds, tens of thousands in a large process (F-112).
        ferrix_fallible::sort_flat_by_key(&mut ids, |&id| id);
        ids.dedup();
        Named::Sorted(ids)
    }

    /// Whether any region of the map named `id` when this was taken: what
    /// [`still_named`] answers.
    fn contains(&self, id: u64) -> bool {
        match self {
            Named::Sorted(ids) => ids.binary_search(&id).is_ok(),
            Named::Scan(map) => still_named(map, id),
        }
    }
}

impl Drop for AddressSpace {
    /// Tear the whole space down: every mapping, every object, every table.
    ///
    /// The objects go first and the tables second. Dropping an object releases
    /// its frames, and a frame released while a translation to it still exists
    /// is only safe because nothing is running in this address space — an
    /// `AddressSpace` is dropped when its last reference goes, and a running
    /// thread is a reference — and because every processor that ran it left
    /// its set only after the root write that dropped its translations.
    ///
    /// Every object is detached before it is let go, taking only its mapper
    /// list: this runs with no lock of its own to hold.
    fn drop(&mut self) {
        let me: *const AddressSpace = &raw const *self;
        let inner = self.inner.get_mut();

        // Unwalked: no processor is in this space's set, and each left it
        // through the root write that dropped its entries, walk caches and
        // all, or, with PCIDs, left them under a PCID that is not current --
        // the module's second rule. Those are used for nothing: this space's
        // id is never given again, so no install finds its slot, and the
        // slot's next owner flushes it (`docs/PCID.md` §3).
        for region in inner.map.iter() {
            let _ = mm::unmap_unwalked(
                self.root * PAGE_SIZE,
                region.range.start(),
                region.range.bytes(),
            );
        }
        // Nothing counted for this space moves again: its views go first.
        for vmo in inner.objects.values().chain(inner.shadows.values()) {
            vmo.forget_views(&self.rss, None);
        }
        for (&id, vmo) in &inner.objects {
            if writable_ceiling(&inner.native, id) {
                vmo.lower_writable_region();
            }
            vmo.detach(me, id);
        }
        for (&id, shadow) in &inner.shadows {
            shadow.detach(me, id);
        }
        for (&id, mapping) in &inner.files {
            if mapping.may_write
                && let Some(vmo) = inner.objects.get(&id)
            {
                vmo.lower_shared_may_write();
            }
        }
        inner.objects.clear();
        inner.files.clear();
        inner.shadows.clear();

        // A window's entries are in no region: the tables go, its objects keep their
        // frames, and those objects' references go with this struct, after this.
        #[cfg(ferrix_personality = "user")]
        if self.window.load(Ordering::Acquire) {
            let _ = mm::unmap_unwalked(self.root * PAGE_SIZE, 0, SHARED_BASE);
        }

        // Tables a fault that ran out of memory made above the page it could
        // not map, which hold nothing and so were in no region's unmapping.
        // Kept while the space lived, for the next fault there; given back
        // now nothing can walk them.
        mm::prune_in(self.root * PAGE_SIZE, 0, USER_VIRT_END);

        // The root itself. On x86-64 its upper half names the kernel's own
        // tables, which are emphatically not this space's to free -- but
        // `unmap_unwalked` only ever walked the ranges above, all of which are in
        // the user half, so nothing of the kernel's was ever reached.
        mm::deallocate_frames(self.root, 0);
        // And the normal root: its slots name this tree's tables, which have
        // just gone, and the region's, which are the region's.
        if let Some(normal) = self.normal.get() {
            mm::free_root(normal.root);
        }
    }
}

/// A space's resident pages by kind: [`AddressSpace::resident`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Resident {
    /// Private memory, and private file mappings' copied pages.
    pub anon: u64,
    /// A disk file's pages.
    pub file: u64,
    /// A memory filesystem's file's pages, and shared anonymous memory.
    pub shmem: u64,
}

impl Resident {
    /// All of it: `VmRSS`.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.anon
            .saturating_add(self.file)
            .saturating_add(self.shmem)
    }
}

/// One region of an address space, as `/proc/<pid>/maps` reports it.
///
/// No name: the region map records none. `[heap]` and `[stack]` are things
/// the process knows about its regions, not things the map knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// First address.
    pub start: u64,
    /// First address past the end.
    pub end: u64,
    /// Permissions and mapping kind.
    pub flags: VmaFlags,
    /// For a file mapping, the id [`AddressSpace::mapped_file`] finds its file
    /// by, and the byte offset of the region's first page in the file.
    pub file: Option<(u64, u64)>,
}

impl AddressSpace {
    /// Every region, lowest first, as it stands at the moment of the call.
    ///
    /// # Errors
    ///
    /// [`fallible::AllocError`] when there is no memory for the list; a
    /// caller that only looks uses [`AddressSpace::with_regions`], which
    /// needs none.
    pub fn regions(&self) -> Result<Vec<Region>, fallible::AllocError> {
        self.with_regions(|regions| fallible::try_collect(regions))
    }

    /// What the regions add up to, as the map keeps it (`L.user.238`), and
    /// the region holding `address` if one does: one lookup under the lock,
    /// no walk, whatever the regions.
    pub fn region_totals(&self, address: u64) -> (ferrix_vma::Totals, Option<Region>) {
        let inner = self.inner.lock();
        let holder = inner.map.find(address).map(|vma| Region {
            start: vma.range.start(),
            end: vma.range.end(),
            flags: vma.flags,
            file: match vma.backing {
                Backing::File { id, offset } => Some((id, offset)),
                _ => None,
            },
        });
        (inner.map.totals(), holder)
    }

    /// Show `visit` every region, lowest first, under the space's lock: it
    /// must not wait, and nothing can change the map while it looks.
    pub fn with_regions<R>(&self, visit: impl FnOnce(&mut dyn Iterator<Item = Region>) -> R) -> R {
        let inner = self.inner.lock();
        let mut regions = inner.map.iter().map(|vma| Region {
            start: vma.range.start(),
            end: vma.range.end(),
            flags: vma.flags,
            file: match vma.backing {
                Backing::File { id, offset } => Some((id, offset)),
                _ => None,
            },
        });
        visit(&mut regions)
    }

    /// Pages the objects this space maps have committed: its resident set.
    ///
    /// A page shared with another space after `fork` is counted in both, as
    /// Linux's `VmRSS` counts it. An object mapped here more than once, which
    /// `vmo_map` allows, is counted once.
    ///
    /// # Errors
    ///
    /// [`fallible::AllocError`] when there is no memory to list the objects
    /// in.
    pub fn resident_pages(&self) -> Result<u64, fallible::AllocError> {
        let inner = self.inner.lock();
        let mut objects = fallible::try_collect(inner.objects.values())?;
        // Not core's `sort_unstable_by_key`, which recurses: one entry per
        // object, and the OOM kill's survey asks this of every process on
        // the stack of the task that faulted (F-112).
        ferrix_fallible::sort_flat_by_key(&mut objects, |vmo| Arc::as_ptr(vmo));
        objects.dedup_by_key(|vmo| Arc::as_ptr(vmo));
        Ok(objects.iter().map(|vmo| vmo.committed() as u64).sum())
    }
}
