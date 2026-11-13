//! K3b: a disk file's mapped writes, seen page by page (`docs/microkernel/K3b.md`),
//! with kernel objects alone: an object over a source of the check's own, a space,
//! and the fault a program's store takes. A store from ring 3 is
//! `fs/btrfs_write_check/mapped_check.rs`'s.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::VmaFlags;

use super::space::{Access, AddressSpace, FileMapping, FilePlace};
use super::vmo::{Filler, PageState, Vmo};
use crate::mm;

/// The object's pages.
const PAGES: u64 = 8;

/// The page the source does not hold.
const HOLE: u64 = PAGES - 1;

/// A source of the check's own: every page zero, always clean.
#[derive(Debug)]
struct Source;

impl Filler for Source {
    fn fill(&self, vmo: &Vmo, index: u64) -> Result<(), ferrix_errno::Errno> {
        self.fill_one(vmo, index)
    }

    fn fill_one(&self, vmo: &Vmo, index: u64) -> Result<(), ferrix_errno::Errno> {
        // The last page is a hole: past what the source holds, it fills nothing.
        if index == HOLE {
            return Ok(());
        }
        let frame = mm::allocate_user_frame().ok_or(ferrix_errno::Errno::ENOMEM)?;
        // SAFETY: (FRAME) a frame just allocated, ours until it goes in; the
        // direct map is writable for all of RAM.
        unsafe {
            core::ptr::write_bytes(
                mm::direct_map(frame * PAGE_SIZE) as *mut u8,
                0,
                PAGE_SIZE as usize,
            );
        };
        if !vmo.insert_absent(index, frame) {
            let _ = mm::release_frame(frame);
        }
        Ok(())
    }

    fn reclaimable(&self) -> bool {
        true
    }

    fn reclaim_clean(&self, pages: &mut Vec<u64>, drop: &mut dyn FnMut(&[u64]) -> u64) -> u64 {
        drop(pages)
    }
}

// CHECKS-ONLY: iommu/domain_check.rs
/// An object over a source of this check's own, for the check that a pin refuses
/// one: a device's write would mark no page (K3b, L.user.212).
///
/// # Errors
///
/// [`crate::fallible::AllocError`].
pub fn sourced_object() -> Result<Arc<Vmo>, crate::fallible::AllocError> {
    let source: Arc<dyn Filler> = crate::fallible::try_arc(Source)?;
    Vmo::new_filled(1, Some(source))
}

/// An object over [`Source`], mapped shared and writable in a space of its own.
fn mapped() -> Result<(Arc<AddressSpace>, Arc<Vmo>, u64), &'static str> {
    let source: Arc<dyn Filler> = Arc::new(Source);
    let vmo = Vmo::new_filled(PAGES, Some(source)).map_err(|_| "dirty: no memory for an object")?;
    let space = AddressSpace::new().map_err(|_| "dirty: no memory for a space")?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            PAGES * PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            Arc::clone(&vmo),
            0,
            FileMapping {
                file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                may_write: true,
            },
        )
        .map_err(|_| "dirty: a sourced object would not map")?;
    Ok((space, vmo, at))
}

/// The fault a program's store or load takes, on page `page`.
fn touch(space: &AddressSpace, at: u64, page: u64, access: Access) -> Result<(), &'static str> {
    crate::object::oom::user_fault(space, at + page * PAGE_SIZE, access)
        .map(|_| ())
        .map_err(|_| "dirty: a fault through a shared mapping failed")
}

/// The leaf's writability, if the page has a leaf.
fn leaf_writes(space: &AddressSpace, at: u64, page: u64) -> Option<bool> {
    mm::flags_in(space.root_table(), at + page * PAGE_SIZE).map(|flags| flags.write)
}

/// L.user.206: a Clean page is read-only, and its first write marks it Dirty.
///
/// Verifies: L.user.206
fn a_write_is_seen(report: &mut Report) -> Result<(), &'static str> {
    let (space, vmo, at) = mapped()?;
    touch(&space, at, 1, Access::READ)?;
    if leaf_writes(&space, at, 1) != Some(false) {
        return Err("dirty: a Clean page of a sourced object was mapped writable");
    }
    if vmo.page_state(1) != PageState::Clean {
        return Err("dirty: a page read was not Clean");
    }
    touch(&space, at, 1, Access::WRITE)?;
    if vmo.page_state(1) != PageState::Dirty || leaf_writes(&space, at, 1) != Some(true) {
        return Err("dirty: a write through a mapping did not make its page Dirty and writable");
    }
    // A hole's page, committed by the write as zeros, and marked (a file grown and
    // written through its mapping).
    touch(&space, at, HOLE, Access::WRITE)?;
    if vmo.page_state(HOLE) != PageState::Dirty {
        return Err("dirty: a write to a hole in a sourced object did not make its page Dirty");
    }
    // A fork's child refaults, and its first write marks too (5981 (8)).
    touch(&space, at, 2, Access::READ)?;
    let child = space.fork().map_err(|_| "dirty: no memory for a fork")?;
    touch(&child, at, 2, Access::WRITE)?;
    if vmo.page_state(2) != PageState::Dirty {
        return Err("dirty: a fork child's first write did not make its page Dirty");
    }
    report.pages += 2;
    Ok(())
}

/// L.user.207, L.user.208, L.user.209: begin answers exactly what was written and
/// takes those translations down, and only those; a later write is Dirty again;
/// a failed write-back reports its pages again.
///
/// Verifies: L.user.207, L.user.208, L.user.209
fn a_write_back(report: &mut Report) -> Result<(), &'static str> {
    let (space, vmo, at) = mapped()?;
    touch(&space, at, 0, Access::READ)?;
    touch(&space, at, 3, Access::WRITE)?;
    touch(&space, at, 5, Access::WRITE)?;
    let shots = space.shootdowns_begun();
    let written = vmo
        .begin_mapped_writes()
        .map_err(|_| "dirty: no memory for a write-back's list")?;
    if written.as_slice() != [3, 5] {
        return Err("dirty: a write-back did not answer exactly the pages written");
    }
    if vmo.page_state(3) != PageState::AwaitingClean {
        return Err("dirty: a page written back is not awaiting clean");
    }
    if leaf_writes(&space, at, 3).is_some() || leaf_writes(&space, at, 5).is_some() {
        return Err("dirty: a page being written back kept its translation");
    }
    if leaf_writes(&space, at, 0).is_none() {
        return Err("dirty: a write-back took a translation of a page not written");
    }
    if space.shootdowns_begun() <= shots {
        return Err("dirty: a write-back began no shootdown");
    }
    touch(&space, at, 5, Access::WRITE)?;
    vmo.end_mapped_writes(None);
    if vmo.page_state(3) != PageState::Clean {
        return Err("dirty: a page written back is not Clean");
    }
    if vmo.page_state(5) != PageState::Dirty {
        return Err("dirty: a write during a write-back was lost from the dirty set");
    }
    // With nothing to do, no shootdown.
    let again = vmo
        .begin_mapped_writes()
        .map_err(|_| "dirty: no memory for a write-back's list")?;
    if again.as_slice() != [5] {
        return Err("dirty: the page written during a write-back was not reported next");
    }
    vmo.end_mapped_writes(Some(5));
    if vmo.page_state(5) != PageState::Dirty {
        return Err("dirty: a failed write-back's page was not Dirty again");
    }
    vmo.end_mapped_writes(None);
    let last = vmo
        .begin_mapped_writes()
        .map_err(|_| "dirty: no memory for a write-back's list")?;
    vmo.end_mapped_writes(None);
    let shots = space.shootdowns_begun();
    if last.as_slice() != [5]
        || !vmo.begin_mapped_writes().is_ok_and(|none| none.is_empty())
        || space.shootdowns_begun() != shots
    {
        return Err("dirty: a write-back with nothing to do was not silent");
    }
    report.pages += 3;
    Ok(())
}

/// L.user.210: reclaim takes Clean pages, mapped writable or not, and leaves the
/// others; a truncation's take leaves no state behind for a refill to find.
///
/// Verifies: L.user.210
fn reclaim_takes_clean_pages(report: &mut Report) -> Result<(), &'static str> {
    let (space, vmo, at) = mapped()?;
    touch(&space, at, 0, Access::READ)?;
    touch(&space, at, 1, Access::WRITE)?;
    touch(&space, at, 2, Access::WRITE)?;
    let awaiting = vmo
        .begin_mapped_writes()
        .map_err(|_| "dirty: no memory for a write-back's list")?;
    touch(&space, at, 2, Access::WRITE)?;
    if awaiting.len() != 2 {
        return Err("dirty: the write-back for the reclaim case answered the wrong pages");
    }
    let mut picked = Vec::new();
    for page in 0..3 {
        crate::fallible::try_push(&mut picked, page).map_err(|_| "dirty: no memory for a list")?;
    }
    let taken = vmo.reclaim_clean(&mut picked);
    if taken != 1 || vmo.page_state(0) != PageState::Absent {
        return Err("dirty: reclaim did not take the Clean page of a writably mapped object");
    }
    if vmo.page_state(1) != PageState::AwaitingClean || vmo.page_state(2) != PageState::Dirty {
        return Err("dirty: reclaim took a page a mapping wrote");
    }
    vmo.end_mapped_writes(None);
    let gone = vmo.decommit_from(0);
    let _ = gone;
    touch(&space, at, 2, Access::READ)?;
    if vmo.page_state(2) != PageState::Clean {
        return Err("dirty: a page refilled after a cut did not start Clean");
    }
    report.pages += 1;
    Ok(())
}

/// What the check found, for the boot line.
#[derive(Debug, Default, Clone, Copy)]
struct Report {
    /// Cases of pages seen.
    pages: u64,
}

// CHECKS-ONLY: stages_check.rs
/// Every case; how many pages' states it judged.
///
/// # Errors
///
/// The first that did not hold.
///
/// Verifies: H.MEM.28
pub fn run() -> Result<u64, &'static str> {
    let mut report = Report::default();
    a_write_is_seen(&mut report)?;
    a_write_back(&mut report)?;
    reclaim_takes_clean_pages(&mut report)?;
    Ok(report.pages)
}
