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
//!   of its TLB: [`AddressSpace::uninstall`]'s, or the `install` of the next
//!   space. On x86-64 that is the `CR3` write — without `PCID` it drops every
//!   entry not marked global, and no user mapping is ever global: every
//!   `map_in` in this file passes `global: false`. On `AArch64` and ARMv7-A a
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
//! ever held together, and no shootdown waits under any of them.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::ops::Deref;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::SpinLock;
use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END, is_user_address};
use ferrix_frame::Frame;
use ferrix_paging::MapFlags;
use ferrix_sched::CpuSet;
use ferrix_sync::{SleepLock, SleepLockGuard};
use ferrix_vma::{Backing, PageRange, Unmapping, Vma, VmaFlags};

use crate::arch;
use crate::fallible;
use crate::mm;
use crate::smp::{self, CpuMask, TlbPages};
use crate::user::memory_type::{self, MemoryType};
use crate::user::vmo::{Kept, Own, Retired, ShadowCopies, Sharing, Vmo, VmoError};

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
pub(crate) const MMAP_MIN_ADDR: u64 = 0x1_0000;

/// Why an address space operation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SpaceError {
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
        }
    }
}

/// What a fault was trying to do, which decides whether it is allowed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Access {
    /// The access wrote.
    pub(crate) write: bool,
    /// The access fetched an instruction.
    pub(crate) execute: bool,
}

impl Access {
    /// A read.
    pub(crate) const READ: Access = Access {
        write: false,
        execute: false,
    };
    /// A write.
    pub(crate) const WRITE: Access = Access {
        write: true,
        execute: false,
    };
}

/// The parts that the lock protects.
#[derive(Debug)]
struct Inner {
    /// What is mapped where.
    map: ferrix_vma::AddressSpace,
    /// The objects those regions name, by the id the region carries. Every id
    /// in here is attached to its object, and detached as it leaves.
    objects: BTreeMap<u64, Arc<Vmo>>,
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
    shadows: BTreeMap<u64, Arc<Vmo>>,
    /// The next object id to hand out. Never zero, which `Backing` reserves
    /// for private memory with no named object.
    next_id: u64,
    /// The objects `vmo_map` put here, which the Linux calls that reshape a
    /// region -- `mremap`, `mprotect` -- may not touch: a native VMO's size
    /// and the rights its protection stands for belong to its handle.
    native: BTreeSet<u64>,
}

impl Inner {
    /// The object a region names and its offset, if the Linux calls that
    /// reshape a region may: anonymous memory that `vmo_map` did not put here.
    fn reshapeable(&self, backing: Backing) -> Option<(u64, u64)> {
        match backing {
            Backing::Anonymous { id, offset } if !self.native.contains(&id) => Some((id, offset)),
            _ => None,
        }
    }
}

/// One process's address space.
#[derive(Debug)]
pub(crate) struct AddressSpace {
    /// Physical frame of the root table.
    root: Frame,
    /// This space, as the objects it maps record it.
    me: Weak<AddressSpace>,
    /// The processors whose TLB may still hold this space's translations.
    cpus: CpuMask,
    /// Its shootdowns: those not yet returned, and those ever begun.
    flushes: Flushes,
    /// Held by each system call that changes which ranges are mapped, from
    /// its first look at the map to its last change: see
    /// [`AddressSpace::layout`].
    layout: SleepLock<()>,
    inner: SpinLock<Inner>,
    /// The speculation domain its programs are in, zero for none, or
    /// [`UNCLAIMED`] before a process has claimed it: see
    /// [`AddressSpace::claim_domain`] and `docs/OPAQUE-KERNEL.md` §9.2.
    domain: AtomicU64,
}

impl AddressSpace {
    /// An empty address space, with the kernel reachable from it.
    ///
    /// # Errors
    ///
    /// [`SpaceError::OutOfMemory`] if there is no frame for the root.
    pub(crate) fn new() -> Result<Arc<AddressSpace>, SpaceError> {
        let root = mm::allocate_frames(0).ok_or(SpaceError::OutOfMemory)?;
        mm::zero_frame(root);

        // Before anything is mapped into it: on x86-64 this is what puts the
        // kernel's half in reach, and a trap taken in this address space
        // before it ran would have nowhere to go.
        arch::prepare_user_root(root * PAGE_SIZE);

        let map = ferrix_vma::AddressSpace::new(MMAP_MIN_ADDR, USER_VIRT_END).map_err(|_| {
            // The window is a compile-time constant of the layout, so this is
            // unreachable in practice; reported rather than panicked because a
            // kernel has no supervisor to restart it.
            SpaceError::BadRange
        })?;

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
                    native: BTreeSet::new(),
                },
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
    pub(crate) fn set_ceiling(&self, end: u64) -> Result<(), SpaceError> {
        if end > USER_VIRT_END {
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
    pub(crate) fn ceiling(&self) -> u64 {
        self.inner.lock().map.high()
    }

    /// The physical address of the root table, for whoever installs it.
    pub(crate) fn root_table(&self) -> u64 {
        self.root * PAGE_SIZE
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
    /// root write just made has dropped whatever of it this TLB held.
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
    pub(crate) unsafe fn install(&self, replacing: Option<&AddressSpace>) {
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
        // SAFETY: (TRANSLATE) the root was made by `new`, so `prepare_user_root` has run
        // on it and the kernel is reachable through it on the architecture
        // that needs that; the caller guarantees it outlives the installation.
        unsafe { arch::install_user_root(self.root * PAGE_SIZE) };
        crate::sched::trip::count(crate::sched::trip::Count::RootInstall);
        if let Some(previous) = replacing
            && !core::ptr::eq(previous, self)
        {
            previous.cpus.leave(cpu);
        }
    }

    /// Leave this address space on the processor that is running.
    ///
    /// After this no user address translates here, which is the state a kernel
    /// thread runs in. The processor leaves the set after the root write,
    /// which on every architecture drops the user translations it had cached.
    ///
    /// # Safety
    ///
    /// (TRANSLATE) Nothing on this processor may still need a user address, and interrupts
    /// must be masked across the call.
    pub(crate) unsafe fn uninstall(&self) {
        // As it leaves, for the next install's decision: see `install`.
        arch::left_space(self.domain());
        let cpu = this_logical_cpu();
        // SAFETY: (TRANSLATE) the caller guarantees no user address is wanted, and the kernel
        // is reachable without one on every architecture.
        unsafe { arch::uninstall_user_root() };
        crate::sched::trip::count(crate::sched::trip::Count::RootUninstall);
        self.cpus.leave(cpu);
    }
}

impl AddressSpace {
    /// What [`AddressSpace::new`] and [`AddressSpace::fork`] make alike: a
    /// space over `root` holding `inner`, on no processor, with no shootdown
    /// begun, and claimed by no speculation domain yet.
    fn assemble(root: Frame, me: &Weak<AddressSpace>, inner: Inner) -> AddressSpace {
        AddressSpace {
            root,
            me: me.clone(),
            cpus: CpuMask::new(),
            flushes: Flushes::new(),
            layout: SleepLock::new((), &crate::sync::SchedParker),
            inner: SpinLock::new(inner),
            domain: AtomicU64::new(UNCLAIMED),
        }
    }
}

/// An [`AddressSpace`]'s domain before any process has claimed it.
const UNCLAIMED: u64 = u64::MAX;

/// An [`AddressSpace`]'s domain once it is out of every domain for good: two
/// domains claimed it, or its process left its domain. Zero, by contrast, is
/// a space claimed by a process in no domain, which a birth may still place.
const OUT: u64 = u64::MAX - 1;

impl AddressSpace {
    /// The speculation domain the switch barrier decision reads: zero for
    /// none, which an unclaimed space and one out for good are too.
    pub(crate) fn domain(&self) -> u64 {
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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| match held {
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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
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

/// The frame for page `index` of `vmo`, committed on first touch.
///
/// A page of a file mapping (`file`) wholly past the end of its file is
/// refused with [`SpaceError::PastEnd`] at `address`, never committed: the
/// end is read under the object's pages lock, and a truncation lowers it
/// before it takes that lock to take pages away.
fn commit_page(vmo: &Vmo, index: u64, file: bool, address: u64) -> Result<Frame, SpaceError> {
    if file {
        vmo.commit_within(index)
            .map_err(SpaceError::Backing)?
            .ok_or(SpaceError::PastEnd(address))
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
fn inherited(
    inner: &Inner,
) -> Result<(BTreeSet<u64>, BTreeMap<u64, FileMapping>), fallible::AllocError> {
    let mut native = BTreeSet::new();
    for &id in &inner.native {
        let _ = fallible::insert_into_set(&mut native, id)?;
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

impl AddressSpace {
    /// How many regions the map holds.
    pub(crate) fn region_count(&self) -> usize {
        self.inner.lock().map.region_count()
    }

    /// The object this space knows as `id`, if it has one.
    pub(crate) fn object(&self, id: u64) -> Option<Arc<Vmo>> {
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
    pub(crate) fn map_anonymous(
        &self,
        at: u64,
        len: u64,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        if !is_user_address(at) || at.checked_add(len).is_none_or(|end| end > USER_VIRT_END) {
            return Err(SpaceError::NotUserRange(at));
        }
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;

        let mut inner = self.inner.lock();
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);

        let pages = len.div_ceil(PAGE_SIZE);
        let vmo = Vmo::new_anonymous(pages).map_err(|_| SpaceError::OutOfMemory)?;
        self.add_region(
            &mut inner,
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
        // FALLIBLE: the map's insert refuses with `VmaError::NoMemory`.
        if let Err(error) = inner.map.insert(range, flags, backing) {
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
        if let Some(vmo) = inner.objects.remove(&id) {
            vmo.detach(self, id);
        }
        let _ = inner.native.remove(&id);
        let _ = inner.map.remove_quietly(range);
    }

    /// Take this space's translations of every copy-on-write region out of
    /// its tables, for `fork`: the pages its shootdown must cover.
    fn unmap_copied_on_write(&self, map: &ferrix_vma::AddressSpace) -> TlbPages {
        let mut pages = TlbPages::new();
        for region in map.iter().filter(|region| region.cow) {
            self.unmap_range(region.range, &mut pages);
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
    /// # Why the parent's mappings are taken down
    ///
    /// Marking the map is not enough. The parent's page tables still hold
    /// *writable* translations to the pages it has just agreed to share, and a
    /// write through one of those would never reach [`AddressSpace::fault`] at
    /// all — it would land in a page the child can read. So every region now
    /// marked copy-on-write is unmapped in the parent, the processors in its
    /// set are told, and the next access re-faults and is reinstalled
    /// read-only because `cow` is set.
    ///
    /// Unmapping rather than write-protecting in place costs the parent one
    /// extra fault per page — a read that would have hit now faults once — and
    /// is what [`crate::mm`] offers today: `map_in` refuses to overwrite a live
    /// mapping, by design, and there is no `protect_in`. Adding one is the
    /// obvious improvement and changes nothing about what is correct here.
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
    pub(crate) fn fork(&self) -> Result<Arc<AddressSpace>, SpaceError> {
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

        // Only now the parent's map is marked and copied, and its writable
        // translations to the shared pages taken down.
        let Ok(map) = inner.map.clone_for_fork() else {
            // Nothing is marked yet; the forked objects go as this returns.
            mm::deallocate_frames(root, 0);
            return Err(SpaceError::OutOfMemory);
        };
        let mut pages = self.unmap_copied_on_write(&inner.map);

        // Read before the lock goes, because the child inherits them. A
        // native object is shared, so the child names the same one.
        let next_id = inner.next_id;

        // The parent's writable translations to the shared pages are out of
        // its tables, but may still be in the TLB of a processor in its set —
        // and a write through one of those would reach a page the child can
        // read, which is the whole thing fork just promised would not happen.
        let cpus = self.begin_shootdown(&inner);
        drop(inner);
        self.shoot(&cpus, &mut pages);

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
                },
            )
        })
        // Refused before the closure ran, so nothing owns the root but this:
        // it goes back here, and the rest the closure captured goes as it
        // drops. The parent's marks only make it copy on write what it could
        // have written, which is harmless.
        .map_err(|_| unrooted(root))?;
        attach_child(&child)?;
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
    pub(crate) fn fault(&self, address: u64, access: Access) -> Result<(), SpaceError> {
        fault_requested();
        self.fill_file_page(address)?;
        self.resolve(address, access)
    }

    /// [`AddressSpace::fault`] once a file's page is in its object: find the
    /// region, ask its object for the page, install it.
    fn resolve(&self, address: u64, access: Access) -> Result<(), SpaceError> {
        let inner = self.inner.lock();

        let region = *inner
            .map
            .find(address)
            .ok_or(SpaceError::NotMapped(address))?;

        if !permits(region.flags, access) {
            return Err(SpaceError::Refused(address));
        }

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
            let shared = vmo.commit(index).map_err(SpaceError::Backing)?;

            // The one real decision. A page nobody else holds any more needs
            // no copy: the other side has already copied it, or unmapped, or
            // exited, and copying would allocate a frame in order to duplicate
            // data this space is the sole owner of.
            let (frame, retired) = if mm::frame_references(shared) > 1 {
                copy_on_write(&vmo, index, shared)?
            } else {
                (shared, None)
            };

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
            return mapped.map_err(|_| SpaceError::OutOfMemory);
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
        if mm::translate_in(self.root * PAGE_SIZE, page).is_some() {
            return Ok(());
        }

        let frame = commit_page(&vmo, index, file, address)?;

        // A copy-on-write region is installed read-only however writable the
        // region is, so that the *next* write faults here again and can copy.
        let writable = region.flags.write && !region.cow;
        let flags = MapFlags {
            read: true,
            write: writable,
            execute: region.flags.execute,
            user: true,
            global: false,
            device: false,
            // Past the caches if a device shares it (`Vmo::make_coherent`).
            uncached: vmo.is_coherent(),
            write_combining: false,
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
    pub(crate) fn fill_file_page_alone(&self, address: u64) -> Result<(), SpaceError> {
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
        let present =
            mm::translate_in(self.root * PAGE_SIZE, page).map(|physical| physical / PAGE_SIZE);
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
                return self.install_page(inner, placed(frame), user_page(false, execute), false);
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
            match file.page(index) {
                Some(original) => mm::copy_frame(frame, original),
                None => mm::zero_frame(frame),
            }
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
            // A shadow page a fork left in another space too is copied again.
            // One nobody else holds any more -- the other side copied its own,
            // unmapped or exited -- is this space's alone, and is mapped
            // writable in place of the read-only entry the fork or a read
            // left: returning because a page is present would send the write
            // back into that entry forever.
            if mm::frame_references(copied) > 1 {
                return self.copy_shadow_page(inner, &shadow, placed(copied), execute);
            }
            let flags = user_page(true, execute);
            return self.install_page(inner, placed(copied), flags, present.is_some());
        }
        if present == Some(copied) {
            return Ok(());
        }
        let flags = user_page(region.flags.write && !region.cow, execute);
        self.install_page(inner, placed(copied), flags, present.is_some())
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
    pub(crate) fn with_page<R>(
        &self,
        address: u64,
        access: Access,
        touch: impl FnOnce(u64) -> R,
    ) -> Result<R, SpaceError> {
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
                if let Some(physical) = mm::translate_in(self.root * PAGE_SIZE, address)
                    && (!access.write
                        || writable_in_place(&inner, &region, address, physical / PAGE_SIZE))
                {
                    let virt = mm::direct_map_ram(physical).ok_or(SpaceError::Refused(address))?;
                    let answer = touch(virt);
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
    pub(crate) fn with_present_page<R>(
        &self,
        address: u64,
        access: Access,
        touch: impl FnOnce(u64) -> R,
    ) -> Result<Option<R>, SpaceError> {
        let inner = self.inner.lock();
        let region = *inner
            .map
            .find(address)
            .ok_or(SpaceError::NotMapped(address))?;
        if !permits(region.flags, access) || !copyable(&region) {
            return Err(SpaceError::Refused(address));
        }
        let Some(physical) = mm::translate_in(self.root * PAGE_SIZE, address) else {
            return Ok(None);
        };
        if access.write && !writable_in_place(&inner, &region, address, physical / PAGE_SIZE) {
            return Ok(None);
        }
        let virt = mm::direct_map_ram(physical).ok_or(SpaceError::Refused(address))?;
        let answer = touch(virt);
        drop(inner);
        Ok(Some(answer))
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
    pub(crate) fn layout(&self) -> SleepLockGuard<'_, ()> {
        self.layout.lock()
    }

    /// Unmap `len` bytes at `at`, giving back the pages and the tables.
    ///
    /// # Errors
    ///
    /// [`SpaceError::BadRange`] if the range is malformed.
    pub(crate) fn unmap(&self, at: u64, len: u64) -> Result<(), SpaceError> {
        let range = PageRange::from_len(at, len).map_err(|_| SpaceError::BadRange)?;
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
        let cpus = {
            let mut inner = self.inner.lock();
            let removed = inner.map.remove(range).map_err(map_error)?;
            self.take_down(&removed, &mut freeing, &mut pages);
            self.begin_shootdown(&inner)
        };

        // Phase two. Nothing can reach these pages through this address space
        // any more, on any processor.
        self.shoot(&cpus, &mut pages);

        // Phase three: give the pages back, not just the mapping.
        self.give_back(freeing);
        Ok(())
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
    fn give_back(&self, mut freeing: Vec<Freeing>) {
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
            self.drop_unnamed(&mut inner)
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
        // little longer and is otherwise harmless, and the next unmap, or the
        // space going, takes them (finding F-23).
        // Which ids the map still names, read in one pass rather than one
        // pass per id asked about: a space with thousands of regions and as
        // many objects made every unmap quadratic under this lock.
        let named = Named::of(map);
        let gone = files.keys().filter(|&&id| !named.contains(id)).count();
        let Ok(mut unkept) = fallible::try_with_capacity(gone) else {
            let kept = |id: u64| named.contains(id) || files.contains_key(&id);
            drop_objects(me, objects, shadows, native, kept);
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
        drop_objects(me, objects, shadows, native, |id| named.contains(id));
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
                let _ = mm::unmap_in(self.root * PAGE_SIZE, address, len, flush);
                found = true;
            }
        }
        found
    }

    /// Take `range`'s translations out of the tables, adding the addresses
    /// and every table the removal empties to `pages`, for its shootdown.
    fn unmap_range(&self, range: PageRange, pages: &mut TlbPages) {
        let _ = mm::unmap_in(self.root * PAGE_SIZE, range.start(), range.bytes(), pages);
    }

    /// Count a shootdown of this space pending, and read the processors it
    /// must reach.
    ///
    /// Called with the lock held — `_locked` is the proof — and after the
    /// translations it is for are out of the tables, never before.
    fn begin_shootdown(&self, _locked: &Inner) -> CpuSet {
        let _ = self.flushes.pending.fetch_add(1, Ordering::SeqCst);
        let _ = self.flushes.begun.fetch_add(1, Ordering::Relaxed);
        self.cpus.snapshot()
    }

    /// How many shootdowns of this space have begun: see the field.
    pub(crate) fn shootdowns_begun(&self) -> u64 {
        self.flushes.begun.load(Ordering::Relaxed)
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
    /// Ever begun, for the checks: what a change that should have taken
    /// nothing down is held to.
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
pub(crate) struct FileMapping {
    /// The open file it was mapped through.
    pub(crate) file: Arc<dyn Any + Send + Sync>,
    /// Whether this is a shared mapping that may write the file: of a file
    /// open for writing, made while no seal refused writes. Counted in the
    /// file object's may-write count for as long as the id is in the tables,
    /// and what `mprotect` asks before it makes a shared file mapping writable.
    pub(crate) may_write: bool,
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

/// The translation flags of a user page.
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
fn drop_objects(
    me: *const AddressSpace,
    objects: &mut BTreeMap<u64, Arc<Vmo>>,
    shadows: &mut BTreeMap<u64, Arc<Vmo>>,
    native: &mut BTreeSet<u64>,
    kept: impl Fn(u64) -> bool,
) {
    objects.retain(|&id, vmo| {
        let named = kept(id);
        if !named {
            vmo.detach(me, id);
        }
        named
    });
    native.retain(|id| objects.contains_key(id));
    shadows.retain(|&id, shadow| {
        let named = kept(id);
        if !named {
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
pub(crate) enum FilePlace {
    /// Exactly here: `MAP_FIXED`, with the range already cleared.
    Fixed(u64),
    /// Wherever it fits, near the hint if there is one.
    Anywhere(Option<u64>),
}

/// Where `mremap` may put the region it resizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Destination {
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
    pub(crate) fn remap(
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
            let fresh = if shared {
                None
            } else {
                Some(self.move_private(&mut inner, &vmo, first, old_len.min(new_len), new_len)?)
            };

            // Everything that can be refused has been. Whatever a fixed
            // destination covers goes, as `MAP_FIXED` would take it; then the
            // old range leaves the map and the tables, but not its pages.
            let cleared = self.clear_for_remap(
                &mut inner,
                matches!(destination, Destination::Fixed(_)),
                new_range,
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
        self.give_back(freeing);
        placed
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
        new_range: PageRange,
        old_range: PageRange,
        freeing: &mut Vec<Freeing>,
        pages: &mut TlbPages,
    ) -> Result<(), SpaceError> {
        if fixed {
            let removed = inner.map.remove(new_range).map_err(map_error)?;
            self.take_down(&removed, freeing, pages);
        }
        let _ = inner.map.remove(old_range).map_err(map_error)?;
        self.unmap_range(old_range, pages);
        Ok(())
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
    access.write || access.execute || flags.read
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
pub(crate) struct WindowPages {
    /// The first page mapped.
    pub(crate) physical: u64,
    /// The window's first page.
    pub(crate) whole: u64,
    /// The window's length, whole pages.
    pub(crate) whole_len: u64,
}

impl WindowPages {
    /// `len` bytes from `physical`, a window of their own.
    pub(crate) const fn all(physical: u64, len: u64) -> WindowPages {
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
    pub(crate) fn map_window(
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
    pub(crate) fn map_window_typed(
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
        if mm::translate_in(self.root * PAGE_SIZE, page).is_some() {
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

        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        let vmo =
            Vmo::new_anonymous(len.div_ceil(PAGE_SIZE)).map_err(|_| SpaceError::OutOfMemory)?;
        self.add_region(
            &mut inner,
            NewRegion {
                id,
                range,
                flags,
                backing: Backing::Anonymous { id, offset: 0 },
                sharing: sharing_of(flags),
            },
            vmo,
        )?;
        Ok(at)
    }

    /// Map `len` bytes of `vmo`, from byte `offset`, at `at` or wherever there
    /// is room: what `vmo_map` does.
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
    /// [`SpaceError::Refused`] for an executable mapping;
    /// [`SpaceError::BadRange`] for a length or offset that is not whole
    /// pages, a range past the object's end, or one that overlaps a mapping;
    /// [`SpaceError::NotUserRange`] outside the user half; and
    /// [`SpaceError::OutOfMemory`] if no free range is long enough.
    pub(crate) fn map_object(
        &self,
        at: Option<u64>,
        len: u64,
        vmo: Arc<Vmo>,
        offset: u64,
        flags: VmaFlags,
    ) -> Result<u64, SpaceError> {
        if flags.execute {
            return Err(SpaceError::Refused(at.unwrap_or(0)));
        }
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
        let flags = VmaFlags {
            execute: false,
            shared: true,
            grows_down: false,
            ..flags
        };
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        self.add_region(
            &mut inner,
            NewRegion {
                id,
                range,
                flags,
                backing: Backing::Anonymous { id, offset },
                sharing: Sharing::Shared,
            },
            vmo,
        )?;
        if fallible::insert_into_set(&mut inner.native, id).is_err() {
            self.abandon_region(&mut inner, id, range);
            return Err(SpaceError::OutOfMemory);
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
    pub(crate) fn map_code_page(&self, vmo: Arc<Vmo>) -> Result<u64, SpaceError> {
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
        if fallible::insert_into_set(&mut inner.native, id).is_err() {
            self.abandon_region(&mut inner, id, range);
            return Err(SpaceError::OutOfMemory);
        }
        Ok(at)
    }

    /// Map the two pages of `vmo` wherever there is room: the first to be
    /// read, the second to be read and run. What the Linux personality maps
    /// its vDSO with, a data page and the code that reads it. Returns where
    /// the second page went.
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
    pub(crate) fn shared_code_at(&self, vmo: &Arc<Vmo>) -> Option<u64> {
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

    pub(crate) fn map_shared_code(&self, vmo: Arc<Vmo>) -> Result<u64, SpaceError> {
        let len = 2 * PAGE_SIZE;
        if vmo.len_bytes() != len {
            return Err(SpaceError::BadRange);
        }
        let mut inner = self.inner.lock();
        let at = inner
            .map
            .find_free(len, PAGE_SIZE, None)
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
        // FALLIBLE: the map's insert refuses with `VmaError::NoMemory`.
        let inserted = inner.map.insert(
            code,
            runnable,
            Backing::Anonymous {
                id,
                offset: PAGE_SIZE,
            },
        );
        if inserted.is_err() || fallible::insert_into_set(&mut inner.native, id).is_err() {
            if inserted.is_ok() {
                let _ = inner.map.remove_quietly(code);
            }
            self.abandon_region(&mut inner, id, data);
            return Err(SpaceError::OutOfMemory);
        }
        Ok(at + PAGE_SIZE)
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
    pub(crate) fn map_file(
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
    pub(crate) fn mapped_file(&self, id: u64) -> Option<Arc<dyn Any + Send + Sync>> {
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
    pub(crate) fn protect(&self, at: u64, len: u64, flags: VmaFlags) -> Result<(), SpaceError> {
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
                    && matches!(region.backing, Backing::Anonymous { id, .. } if native.contains(&id))
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

    /// The highest address any region reaches, or `None` for an empty space.
    ///
    /// `brk` needs it to place the heap above everything the ELF loader
    /// mapped, without the loader and the heap having to agree on a number.
    pub(crate) fn highest_mapped(&self) -> Option<u64> {
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
pub(crate) enum Advice {
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
pub(crate) enum Declined {
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
    pub(crate) fn advise(&self, at: u64, len: u64, advice: Advice) -> Result<(), Declined> {
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
        let native = inner.native.contains(&id);
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
        let _ = mm::unmap_in(
            self.root * PAGE_SIZE,
            start,
            stop - start,
            &mut advised.pages,
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
    match region.backing {
        Backing::File { id, offset } if !region.flags.shared => {
            let into = (address & !(PAGE_SIZE - 1)).saturating_sub(region.range.start());
            let index = offset.saturating_add(into) / PAGE_SIZE;
            inner.shadows.get(&id).and_then(|shadow| shadow.page(index)) == Some(frame)
        }
        _ => true,
    }
}

/// Whether any region still names object `id`.
fn still_named(map: &ferrix_vma::AddressSpace, id: u64) -> bool {
    map.iter().any(|region| naming(region) == Some(id))
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
        ids.sort_unstable();
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
        // all -- the module's second rule.
        for region in inner.map.iter() {
            let _ = mm::unmap_unwalked(
                self.root * PAGE_SIZE,
                region.range.start(),
                region.range.bytes(),
            );
        }
        for (&id, vmo) in &inner.objects {
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
    }
}

/// One region of an address space, as `/proc/<pid>/maps` reports it.
///
/// No name: the region map records none. `[heap]` and `[stack]` are things
/// the process knows about its regions, not things the map knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Region {
    /// First address.
    pub(crate) start: u64,
    /// First address past the end.
    pub(crate) end: u64,
    /// Permissions and mapping kind.
    pub(crate) flags: VmaFlags,
    /// For a file mapping, the id [`AddressSpace::mapped_file`] finds its file
    /// by, and the byte offset of the region's first page in the file.
    pub(crate) file: Option<(u64, u64)>,
}

impl AddressSpace {
    /// Every region, lowest first, as it stands at the moment of the call.
    ///
    /// # Errors
    ///
    /// [`fallible::AllocError`] when there is no memory for the list; a
    /// caller that only looks uses [`AddressSpace::with_regions`], which
    /// needs none.
    pub(crate) fn regions(&self) -> Result<Vec<Region>, fallible::AllocError> {
        self.with_regions(|regions| fallible::try_collect(regions))
    }

    /// Show `visit` every region, lowest first, under the space's lock: it
    /// must not wait, and nothing can change the map while it looks.
    pub(crate) fn with_regions<R>(
        &self,
        visit: impl FnOnce(&mut dyn Iterator<Item = Region>) -> R,
    ) -> R {
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
    pub(crate) fn resident_pages(&self) -> Result<u64, fallible::AllocError> {
        let inner = self.inner.lock();
        let mut objects = fallible::try_collect(inner.objects.values())?;
        objects.sort_unstable_by_key(|vmo| Arc::as_ptr(vmo));
        objects.dedup_by_key(|vmo| Arc::as_ptr(vmo));
        Ok(objects.iter().map(|vmo| vmo.committed() as u64).sum())
    }
}
