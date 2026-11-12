//! The recount the kept resident counts are held to (`L.user.236`,
//! `docs/RSS-COUNTS.md` §7): check code only, in no ring.
//!
//! This is what `AddressSpace::resident_split` counted for every reader until
//! the counts were kept, its bodies moved here unchanged: each region's
//! object's committed pages inside the region's own range, by kind, a private
//! file mapping's copies as anonymous and not as its file's. It reads the
//! space's map and the objects' pages and nothing of the kept counts -- no
//! view, cover, mask or `Rss` -- and procfs never falls back to it. It is
//! compared only while no other thread of the checked space runs.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fallible;
use crate::user::rss::Kind;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::Backing;

use crate::user::space::{AddressSpace, Resident};
use crate::user::vmo::{COUNT_CHUNK, Vmo};

/// One region's part of an object, as [`recount`] lists it under the space's
/// lock to count after.
pub(crate) struct Counted {
    /// The region's object.
    pub(crate) vmo: Arc<Vmo>,
    /// A private file mapping's shadow: the pages it has copied.
    pub(crate) shadow: Option<Arc<Vmo>>,
    /// The region's first page index in the object.
    pub(crate) first: u64,
    /// One past its last.
    pub(crate) end: u64,
    /// Whether it maps a file.
    pub(crate) file: bool,
    /// Whether the mapping is shared.
    pub(crate) shared: bool,
}

impl Counted {
    /// Which count this region's own pages go to: private memory is
    /// anonymous, shared anonymous memory and a memory filesystem's file
    /// shared memory, a disk file's pages a file's.
    fn kind(&self) -> Kind {
        if !self.file {
            if self.shared { Kind::Shmem } else { Kind::Anon }
        } else if self.vmo.is_disk_file() {
            Kind::File
        } else {
            Kind::Shmem
        }
    }
}

/// Pages [`committed_in_both`] takes per hold, kept on the stack.
const OVERLAP_CHUNK: usize = 64;

/// Pages committed in `first..end` of `vmo`, page indexes: what a mapping of
/// that part of the object shows. Counted [`COUNT_CHUNK`] entries at a time,
/// the pages lock let go between chunks, so a fault on this object waits for
/// at most one chunk however many pages it holds; pages committed or taken
/// while it counts may or may not be counted, as in any report.
fn committed_in(vmo: &Vmo, first: u64, end: u64) -> u64 {
    let mut counted = 0u64;
    let mut from = first;
    while from < end {
        let (taken, last) = vmo.with_frames(|frames| {
            let mut taken = 0usize;
            let mut last = None;
            for (&index, _) in frames.range(from..end).take(COUNT_CHUNK) {
                taken += 1;
                last = Some(index);
            }
            (taken, last)
        });
        counted = counted.saturating_add(taken as u64);
        match last {
            Some(index) if taken == COUNT_CHUNK => from = index.saturating_add(1),
            _ => break,
        }
    }
    counted
}

/// Of the pages committed in `vmo` in `first..end`, those `other` also holds
/// at the same index: a private file mapping's pages its shadow has copied,
/// which the mapping no longer shows from the file. Each chunk's indexes are
/// taken under this object's lock, then looked up in `other` under its own,
/// one lock at a time.
fn committed_in_both(vmo: &Vmo, other: &Vmo, first: u64, end: u64) -> u64 {
    let mut counted = 0u64;
    let mut from = first;
    let mut chunk = [0u64; OVERLAP_CHUNK];
    while from < end {
        let taken = vmo.with_frames(|frames| {
            let mut taken = 0usize;
            for (slot, (&index, _)) in chunk
                .iter_mut()
                .zip(frames.range(from..end).take(OVERLAP_CHUNK))
            {
                *slot = index;
                taken += 1;
            }
            taken
        });
        let indexes = chunk.get(..taken).unwrap_or_default();
        let held = indexes
            .iter()
            .filter(|&&index| other.page(index).is_some())
            .count();
        counted = counted.saturating_add(held as u64);
        match indexes.last() {
            Some(&index) if taken == OVERLAP_CHUNK => from = index.saturating_add(1),
            _ => break,
        }
    }
    counted
}

/// The recount's list: one entry per region that maps an object, made under
/// the space's lock and nothing counted.
fn list_counted(space: &AddressSpace) -> Result<Vec<Counted>, fallible::AllocError> {
    let inner = space.inner.lock();
    let mut listed: Vec<Counted> = fallible::try_with_capacity(inner.map.iter().len())?;
    for vma in inner.map.iter() {
        let pages = (vma.range.end() - vma.range.start()) / PAGE_SIZE;
        let (id, offset, file) = match vma.backing {
            Backing::Anonymous { id, offset } => (id, offset, false),
            Backing::File { id, offset } => (id, offset, true),
            Backing::Device { .. } => continue,
        };
        let Some(vmo) = inner.objects.get(&id) else {
            continue;
        };
        let first = offset / PAGE_SIZE;
        let shadow = (file && !vma.flags.shared)
            .then(|| inner.shadows.get(&id).cloned())
            .flatten();
        // NOALLOC: room for every region was had above.
        listed.push(Counted {
            vmo: Arc::clone(vmo),
            shadow,
            first,
            end: first.saturating_add(pages),
            file,
            shared: vma.flags.shared,
        });
    }
    Ok(listed)
}

/// Add `pages` to the count `kind` names.
fn add(resident: &mut Resident, kind: Kind, pages: u64) {
    let slot = match kind {
        Kind::Anon => &mut resident.anon,
        Kind::File => &mut resident.file,
        Kind::Shmem => &mut resident.shmem,
    };
    *slot = slot.saturating_add(pages);
}

/// Take `pages` off the count `kind` names.
fn take(resident: &mut Resident, kind: Kind, pages: u64) {
    let slot = match kind {
        Kind::Anon => &mut resident.anon,
        Kind::File => &mut resident.file,
        Kind::Shmem => &mut resident.shmem,
    };
    *slot = slot.saturating_sub(pages);
}

/// One object's pages, from its regions, `group`, lowest first: for regions
/// that do not overlap and are of one kind, the shorter walk -- the regions
/// when they span fewer pages than the object holds (a file read with `read`
/// and mapped in part), else its pages less those in the gaps between them,
/// which normally hold none; otherwise each region over its own range. Then
/// each private file mapping's copies, by walking its shadow.
fn count_object(group: &[Counted], resident: &mut Resident) {
    let Some(head) = group.first() else {
        return;
    };
    let kind = head.kind();
    let disjoint = group.windows(2).all(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(low, high)| low.end <= high.first)
    }) && group.iter().all(|counted| counted.kind() == kind);
    let spans = group.iter().fold(0u64, |sum, counted| {
        sum.saturating_add(counted.end.saturating_sub(counted.first))
    });
    let committed = head.vmo.committed() as u64;
    if !disjoint || spans < committed {
        for counted in group {
            add(
                resident,
                counted.kind(),
                committed_in(&counted.vmo, counted.first, counted.end),
            );
        }
    } else {
        let mut outside = 0u64;
        let mut from = 0u64;
        for counted in group {
            outside = outside.saturating_add(committed_in(&head.vmo, from, counted.first));
            from = counted.end;
        }
        outside = outside.saturating_add(committed_in(&head.vmo, from, u64::MAX));
        let inside = committed.saturating_sub(outside);
        add(resident, kind, inside);
    }
    // A private file mapping's copies: anonymous, and no longer its file's.
    // Counted by walking the shadow, which holds only them.
    for counted in group {
        if let Some(shadow) = &counted.shadow {
            let (first, end) = (counted.first, counted.end);
            add(resident, Kind::Anon, committed_in(shadow, first, end));
            take(
                resident,
                counted.kind(),
                committed_in_both(shadow, &counted.vmo, first, end),
            );
        }
    }
}

/// The resident set of `space`, counted afresh from its regions and their
/// objects' pages.
///
/// # Errors
///
/// [`fallible::AllocError`] when there is no memory to list the regions in.
pub(crate) fn recount(space: &AddressSpace) -> Result<Resident, fallible::AllocError> {
    let mut listed: Vec<Counted> = list_counted(space)?;
    listed.sort_unstable_by_key(|counted| (Arc::as_ptr(&counted.vmo) as usize, counted.first));
    let mut resident = Resident::default();
    let mut start = 0;
    while let Some(head) = listed.get(start) {
        let object = Arc::as_ptr(&head.vmo);
        let count = listed
            .get(start..)
            .unwrap_or_default()
            .iter()
            .take_while(|counted| Arc::as_ptr(&counted.vmo) == object)
            .count();
        let group = listed.get(start..start + count).unwrap_or_default();
        start += count;
        count_object(group, &mut resident);
    }
    Ok(resident)
}

// CHECKS-ONLY: user/rss_kept_check.rs, fs/btrfs_write_check/reclaim_check.rs
/// Whether `space`'s kept counts equal [`recount`]'s; if not, both.
///
/// # Errors
///
/// The kept counts and the recount, when they differ or the recount had no
/// memory (the recount then reads as all zeros).
pub fn agrees(space: &AddressSpace) -> Result<(), (Resident, Resident)> {
    let kept = space.resident();
    let counted = recount(space).unwrap_or_default();
    if kept == counted {
        Ok(())
    } else {
        Err((kept, counted))
    }
}

// CHECKS-ONLY: user/rss_kept_check.rs
/// The entries the file views of `vmo` keep in their masks, and whether the
/// heap charged for them covers them all (ledger 8894 T1).
pub fn mask_held(vmo: &Vmo) -> (usize, bool) {
    vmo.with_frames(|frames| {
        let (entries, charged) = frames
            .views
            .iter()
            .filter_map(|view| view.mask.as_ref())
            .fold((0usize, 0u64), |(entries, charged), mask| {
                (entries + mask.entries.len(), charged + mask.room.charged())
            });
        let wanted = (entries as u64).saturating_mul(crate::user::rss::MASK_ENTRY_HEAP as u64);
        (entries, charged >= wanted)
    })
}

// CHECKS-ONLY: user/rss_kept_check.rs
/// Whether any space still views `vmo`: none may once every space mapping it
/// has gone.
pub fn viewed(vmo: &Vmo) -> bool {
    vmo.with_frames(|frames| frames.viewed())
}

// CHECKS-ONLY: stages_check.rs
/// What [`run`] found, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct KeptFound {
    /// Comparisons of the kept counts with [`recount`], every one equal.
    pub compared: u32,
    /// Regions of the largest space timed.
    pub regions: usize,
    /// Nanoseconds for [`READS`] reads of its counts.
    pub large_ns: u64,
    /// Nanoseconds for as many reads over a space of one region.
    pub small_ns: u64,
}

// CHECKS-ONLY: stages_check.rs
/// Reads of the counts [`run`] times over each space.
pub const READS: u32 = 200;
/// Two-page pieces of the reserve [`run`] makes regions of.
const PIECES: u64 = 4_000;

// CHECKS-ONLY: stages_check.rs
/// The kept counts with kernel objects alone, on every profile: a space's
/// changes made through the space itself -- a first touch, an `mprotect`
/// split and merge, an unmap of a middle, `madvise(DONTNEED)`, a fork with
/// copy-on-write faults on both sides, a shared mapping both see, the
/// child's drop -- each followed by the kept counts against [`recount`];
/// then a reserve of about
/// 4,000 regions, and the read timed over it and over a
/// space of one region (`docs/RSS-COUNTS.md` §7; the syscall-driven cases
/// are `user::rss_kept_check`'s).
///
/// Verifies: L.user.236
/// Verifies: L.user.237
///
/// # Errors
///
/// What was not as `L.user.236` and `L.user.237` have it.
pub fn run() -> Result<KeptFound, &'static str> {
    use crate::user::space::Advice;
    use ferrix_vma::VmaFlags;

    let mut compared = 0;
    let space = AddressSpace::new().map_err(|_| "no address space for the rss check")?;
    let len = 8 * PAGE_SIZE;
    let private = space
        .map_anywhere(None, len, VmaFlags::READ_WRITE)
        .map_err(|_| "the rss check's space refused a mapping")?;
    (0..8).try_for_each(|page| touch(&space, private + page * PAGE_SIZE))?;
    same(&space, &mut compared)?;
    space
        .protect(private + 2 * PAGE_SIZE, 2 * PAGE_SIZE, VmaFlags::READ)
        .map_err(|_| "an mprotect split was refused")?;
    same(&space, &mut compared)?;
    space
        .protect(private + 2 * PAGE_SIZE, 2 * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "an mprotect merge was refused")?;
    same(&space, &mut compared)?;
    space
        .unmap(private + 3 * PAGE_SIZE, 2 * PAGE_SIZE)
        .map_err(|_| "an unmap of a middle was refused")?;
    same(&space, &mut compared)?;
    // Pages a region still covers, taken out of its object.
    space
        .advise(
            private,
            2 * PAGE_SIZE,
            Advice::Discard { locked_too: false },
        )
        .map_err(|_| "madvise(DONTNEED) was refused")?;
    same(&space, &mut compared)?;
    let shared_flags = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let shared = space
        .map_anywhere(None, 4 * PAGE_SIZE, shared_flags)
        .map_err(|_| "the rss check's space refused a shared mapping")?;
    touch(&space, shared)?;
    same(&space, &mut compared)?;
    let child = space
        .fork()
        .map_err(|_| "the rss check's space would not fork")?;
    same(&space, &mut compared)?;
    same(&child, &mut compared)?;
    touch(&child, private)?;
    touch(&child, shared + PAGE_SIZE)?;
    same(&child, &mut compared)?;
    touch(&space, private + PAGE_SIZE)?;
    same(&space, &mut compared)?;
    drop(child);
    same(&space, &mut compared)?;

    let (regions, large_ns, small_ns) = over_many_regions(&space, &mut compared)?;
    Ok(KeptFound {
        compared,
        regions,
        large_ns,
        small_ns,
    })
}

/// The kept counts of `space` against [`recount`], counted in `compared`.
fn same(space: &AddressSpace, compared: &mut u32) -> Result<(), &'static str> {
    *compared += 1;
    agrees(space).map_err(|_| "a space's kept resident counts differ from a recount")
}

/// A write fault at `at` in `space`.
fn touch(space: &AddressSpace, at: u64) -> Result<(), &'static str> {
    space
        .fault(at, crate::user::space::Access::WRITE)
        .map_err(|_| "a write fault of the rss check was refused")
}

/// About 4,000 regions over one reserve of `space`, every second piece
/// writable and touched, as Wine allocates, compared; then the read timed
/// over it and over a space of one region: the regions and the two times.
fn over_many_regions(
    space: &AddressSpace,
    compared: &mut u32,
) -> Result<(usize, u64, u64), &'static str> {
    use ferrix_vma::VmaFlags;
    let reserve = space
        .map_anywhere(None, PIECES * 2 * PAGE_SIZE, VmaFlags::NONE)
        .map_err(|_| "the rss check's space refused the reserve")?;
    for piece in (1..PIECES).step_by(2) {
        let at = reserve + piece * 2 * PAGE_SIZE;
        space
            .protect(at, 2 * PAGE_SIZE, VmaFlags::READ_WRITE)
            .map_err(|_| "an mprotect of the reserve was refused")?;
        touch(space, at)?;
    }
    same(space, compared)?;
    let regions = space.region_count();
    let large_ns = time_reads(space);
    let small = AddressSpace::new().map_err(|_| "no second space for the rss check")?;
    let one = small
        .map_anywhere(None, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "the second space refused a mapping")?;
    touch(&small, one)?;
    let small_ns = time_reads(&small);
    // Work independent of the regions: the large space's reads within four
    // times the small's, and a millisecond for a slow processor or an
    // interrupt between them. A recount takes milliseconds a read.
    if large_ns > small_ns.saturating_mul(4).saturating_add(1_000_000) {
        return Err("a read of the resident counts grew with the regions");
    }
    Ok((regions, large_ns, small_ns))
}

/// Nanoseconds for [`READS`] reads of `space`'s counts, as procfs reads them.
fn time_reads(space: &AddressSpace) -> u64 {
    let start = crate::timer::now_nanos();
    let mut seen = 0u64;
    for _ in 0..READS {
        seen = seen.wrapping_add(core::hint::black_box(space.resident()).total());
    }
    let _ = core::hint::black_box(seen);
    crate::timer::now_nanos().saturating_sub(start)
}
