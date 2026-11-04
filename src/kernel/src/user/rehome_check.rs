//! The self-check of address spaces and memory objects with kernel objects
//! alone: `AddressSpace`, `Vmo` and a `Filler` of the check's own, no process
//! and no file system. Its caller is `syscall::rehome_check`,
//! and it builds in the core's crate, so that boundary itself says it names no
//! load (`docs/microkernel/B5.md`, class A).
//!
//! The requirements here were verified before only from the personality's
//! checks, by way of `mmap` and tmpfs, which leave ring 0 with the load. What a
//! file's store does -- bound a mapping's faults, take the pages past a cut,
//! zero the rest of the page -- [`cut_file`] does in the store's own order
//! (`fs::pages::VmoPages`'s `resize` and `discard_from`).
//!
//! Like the stage 8 checks it runs twice and counts frames across the second
//! run: the first grows the heap to where it stays.

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vfs::Errno;
use ferrix_vma::VmaFlags;

use crate::mm;
use crate::user::space::{
    Access, AddressSpace, Destination, FileMapping, FilePlace, MMAP_MIN_ADDR, SpaceError,
};
use crate::user::vmo::{Filler, Vmo, VmoError};

/// How many cases one run makes: the boot line's count.
const CASES: u64 = 24;

/// Where a run's fixed mappings go: on a page boundary on all architectures.
const FIXED: u64 = 0x7200_0000;

/// Run them twice, counted on the second, and answer how many cases a run makes.
/// `Err` names the first thing that was not true.
///
/// Called by `syscall::rehome_check`.
///
/// # Errors
///
/// The first thing that was not so, by name.
// CHECKS-ONLY: syscall/rehome_check.rs
pub fn run() -> Result<u64, &'static str> {
    once()?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    once()?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        window.report("rehomed user memory");
        return Err("the core's check of spaces and objects moved the frame count");
    }
    Ok(CASES)
}

/// One run of every case.
fn once() -> Result<(), &'static str> {
    // In this order, so that each control fires its own case: a case further
    // down holds what an earlier one proves, and a check stops at its first
    // failure.
    check_the_floor()?;
    check_adopt_and_release()?;
    check_remap()?;
    check_a_move_reaches_the_pages()?;
    check_commit_and_write()?;
    check_a_fault_fills_from_the_source()?;
    check_a_fill_reclaimed_is_filled_again()?;
    check_a_shared_mapping_is_the_file()?;
    check_a_disk_file_counts_as_a_file()?;
    check_decommit_from()?;
    check_a_file_grown_back_shows_zeros()?;
    check_a_cut_file_refuses_past_its_end()?;
    check_the_may_write_count()?;
    check_a_shared_page_crosses_a_fork()?;
    check_shared_object_at()?;
    check_protect_takes_translations_out()?;
    check_protect_keeps_shared()?;
    check_a_write_only_region_reads()?;
    check_protect_refuses_an_unwritable_mapping()?;
    check_a_private_mapping_keeps_its_copies()?;
    check_a_fork_keeps_private_copies_apart()?;
    check_the_loader_pattern()?;
    check_make_combining()?;
    check_is_anonymous()
}

/// A fresh space, or the check's failure.
fn fresh() -> Result<Arc<AddressSpace>, &'static str> {
    AddressSpace::new().map_err(|_| "no address space for the core's memory check")
}

/// A mapping's record, for an object that holds no open file.
fn mapping(may_write: bool) -> FileMapping {
    FileMapping {
        file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
        may_write,
    }
}

/// Read a byte of `space` at `at` as the kernel would for a program.
fn read(space: &AddressSpace, at: u64) -> Result<u8, SpaceError> {
    space.with_page(at, Access::READ, |byte| {
        // SAFETY: (FRAME) `with_page` passes the direct-map address of `at`,
        // translated under the space's lock, which it holds while this runs.
        unsafe { core::ptr::read(byte as *const u8) }
    })
}

/// Write a byte of `space` at `at` as the kernel would for a program.
fn write(space: &AddressSpace, at: u64, value: u8) -> Result<(), SpaceError> {
    space.with_page(at, Access::WRITE, |byte| {
        // SAFETY: (FRAME) as `read`; the write access made the page writable
        // to this mapping, a private mapping's own copy if it copies.
        unsafe { core::ptr::write(byte as *mut u8, value) }
    })
}

/// Write `bytes` at `at`, across a page boundary if they reach one.
fn write_all(space: &AddressSpace, at: u64, bytes: &[u8]) -> Result<(), SpaceError> {
    for (at, &byte) in (at..).zip(bytes) {
        write(space, at, byte)?;
    }
    Ok(())
}

/// Whether `bytes` read back at `at`.
fn holds(space: &AddressSpace, at: u64, bytes: &[u8]) -> bool {
    (at..)
        .zip(bytes)
        .all(|(at, &byte)| read(space, at) == Ok(byte))
}

/// Whether a read of the `len` bytes at `at` finds zeros only.
fn zeros(space: &AddressSpace, at: u64, len: u64) -> bool {
    (at..at + len).all(|at| read(space, at) == Ok(0))
}

/// A byte of a file's contents, by its offset.
fn pattern(at: u64) -> u8 {
    (at.wrapping_mul(29) ^ (at >> 12)) as u8
}

/// A memory file's object of `len` bytes holding [`pattern`], as a memory
/// file system's store makes one: no source, bound by the file's length.
fn memory_file(len: u64) -> Result<Arc<Vmo>, &'static str> {
    let object = Vmo::new_filled(16, None).map_err(|_| "no object for the core's memory check")?;
    object.set_file_len(len);
    let mut at = 0;
    while at < len {
        let within = (at % PAGE_SIZE) as usize;
        let take = (PAGE_SIZE - at % PAGE_SIZE).min(len - at) as usize;
        let mut page = [0_u8; PAGE_SIZE as usize];
        for (slot, offset) in page.iter_mut().zip(at..at + take as u64) {
            *slot = pattern(offset);
        }
        let bytes = page.get(..take).ok_or("a span outside a page")?;
        object
            .commit_and_write(at / PAGE_SIZE, within, bytes)
            .map_err(|_| "a page of the core's memory check's file was refused")?;
        at += take as u64;
    }
    Ok(object)
}

/// Cut `object`'s file to `len` bytes, in the order a file's store does.
fn cut_file(object: &Vmo, len: u64) {
    object.set_file_len(len);
    let first = len.div_ceil(PAGE_SIZE);
    let _ = object.decommit_from(first);
    object.cut_mappings(first);
    let within = (len % PAGE_SIZE) as usize;
    if within != 0 {
        let _ = object.zero_from(len / PAGE_SIZE, within);
    }
}

/// Whether the file holds `bytes` at `at`, in one page.
fn file_holds(object: &Vmo, at: u64, bytes: &[u8]) -> bool {
    let mut page = [0_u8; PAGE_SIZE as usize];
    let within = (at % PAGE_SIZE) as usize;
    let Some(out) = page.get_mut(..bytes.len()) else {
        return false;
    };
    object.read_page(at / PAGE_SIZE, within, out).is_ok() && out == bytes
}

/// A source of pages of the check's own: each byte is [`pattern`], and every
/// page it is asked for goes in through `insert_absent`.
#[derive(Debug)]
struct Source {
    /// Calls that asked for a page.
    calls: AtomicUsize,
}

impl Source {
    /// A source no one has called.
    fn new() -> Arc<Source> {
        Arc::new(Source {
            calls: AtomicUsize::new(0),
        })
    }
}

impl Filler for Source {
    fn fill(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        self.fill_one(vmo, index)
    }

    fn fill_one(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        let _ = self.calls.fetch_add(1, Ordering::Relaxed);
        put_a_page(vmo, index)
    }
}

/// Fill page `index` of `vmo` with [`pattern`], if it lacks it.
fn put_a_page(vmo: &Vmo, index: u64) -> Result<(), Errno> {
    if vmo.page(index).is_some() {
        return Ok(());
    }
    let frame = mm::allocate_user_frame().ok_or(Errno::ENOMEM)?;
    let base = mm::direct_map(frame * PAGE_SIZE) as *mut u8;
    for offset in 0..PAGE_SIZE {
        let byte = base.wrapping_add(offset as usize);
        // SAFETY: (FRAME) the frame was just allocated here, is in no object
        // yet and the direct map covers it whole.
        unsafe { byte.write(pattern(index * PAGE_SIZE + offset)) };
    }
    if !vmo.insert_absent(index, frame) {
        let _ = mm::release_frame(frame);
    }
    Ok(())
}

/// A source whose first fill is reclaimed before the fault that asked for it
/// can use it: a page filled, and gone by the time the fault takes its lock.
#[derive(Debug)]
struct EvictedOnce {
    /// Fills still to take back.
    left: AtomicUsize,
}

impl Filler for EvictedOnce {
    fn fill(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        self.fill_one(vmo, index)
    }

    fn fill_one(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        put_a_page(vmo, index)?;
        if self.left.load(Ordering::Relaxed) > 0 {
            let _ = self.left.fetch_sub(1, Ordering::Relaxed);
            let _ = vmo.decommit_range(index, 1);
        }
        Ok(())
    }

    fn is_sourced(&self, _index: u64) -> bool {
        true
    }
}

/// No region below the floor, asked for or hinted, and one at the floor itself.
///
/// Verifies: L.user.45
fn check_the_floor() -> Result<(), &'static str> {
    let space = fresh()?;
    if space
        .map_anonymous(0, PAGE_SIZE, VmaFlags::READ_WRITE)
        .is_ok()
    {
        return Err("a space took a fixed region at address 0, below its floor");
    }
    if space
        .map_anonymous(MMAP_MIN_ADDR - PAGE_SIZE, PAGE_SIZE, VmaFlags::READ_WRITE)
        .is_ok()
    {
        return Err("a space took a fixed region a page below its floor");
    }
    let hinted = space
        .map_anywhere(Some(0x10), PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping hinted at 0x10 was refused")?;
    if hinted < MMAP_MIN_ADDR {
        return Err("a mapping hinted at 0x10 was placed below the floor");
    }
    space
        .unmap(hinted, PAGE_SIZE)
        .map_err(|_| "the hinted mapping would not unmap")?;
    if space
        .map_anonymous(MMAP_MIN_ADDR, PAGE_SIZE, VmaFlags::READ_WRITE)
        .is_err()
    {
        return Err("a fixed region at the floor itself was refused");
    }
    if space.regions().map_or(true, |regions| {
        !regions.iter().any(|region| region.start == MMAP_MIN_ADDR)
    }) {
        return Err("a fixed region at the floor itself did not land there");
    }
    Ok(())
}

/// `adopt_pages` names each committed page in the new object at the same
/// offset from the range's start, and `release_moved` takes them out of the old.
///
/// Verifies: L.user.30
fn check_adopt_and_release() -> Result<(), &'static str> {
    let from = Vmo::new_anonymous(4).map_err(|_| "no object to move pages from")?;
    let to = Vmo::new_anonymous(4).map_err(|_| "no object to move pages to")?;
    let one = from.commit(1).map_err(|_| "no page 1 to move")?;
    let three = from.commit(3).map_err(|_| "no page 3 to move")?;
    let kept = from.commit(0).map_err(|_| "no page 0 to keep")?;
    let moved = to
        .adopt_pages(&from, 1, 3)
        .map_err(|_| "adopt_pages refused a range nothing held")?;
    if to.page(0) != Some(one) || to.page(2) != Some(three) || to.page(1).is_some() {
        return Err("adopt_pages did not name each page at its offset from the range's start");
    }
    from.release_moved(&moved);
    if from.page(1).is_some() || from.page(3).is_some() {
        return Err("release_moved left a moved page in the object it came from");
    }
    if from.page(0) != Some(kept) {
        return Err("release_moved took a page outside the range");
    }
    Ok(())
}

/// A private region an `mremap` moves reaches the pages it had.
///
/// Verifies: L.user.30
fn check_a_move_reaches_the_pages() -> Result<(), &'static str> {
    const ACROSS: &[u8] = b"across a page boundary";
    let space = fresh()?;
    let at = FIXED;
    let _ = space
        .map_anonymous(at, 2 * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no region for the move's case")?;
    // A neighbour, so that growing must move it.
    let _ = space
        .map_anonymous(at + 2 * PAGE_SIZE, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no neighbour for the move's case")?;
    let across = at + PAGE_SIZE - 5;
    write_all(&space, across, ACROSS).map_err(|_| "the move's case could not write")?;
    write(&space, at, 0x5A).map_err(|_| "the move's case could not write")?;
    let new = space
        .remap(at, 2 * PAGE_SIZE, 4 * PAGE_SIZE, Destination::Anywhere)
        .map_err(|_| "a move of a private region was refused")?;
    if new == at {
        return Err("a region with a neighbour grew without moving");
    }
    if !holds(&space, new + PAGE_SIZE - 5, ACROSS) || read(&space, new) != Ok(0x5A) {
        return Err("a moved region's pages did not move with it");
    }
    let target = FIXED + 64 * PAGE_SIZE;
    let last = space
        .remap(new, 4 * PAGE_SIZE, PAGE_SIZE, Destination::Fixed(target))
        .map_err(|_| "a move and shrink to a fixed address was refused")?;
    if last != target || read(&space, target) != Ok(0x5A) {
        return Err("a region shrunk and moved did not keep its first page");
    }
    Ok(())
}

/// Grow in the way of a neighbour only by moving, contents carried, zeros
/// after, shrink in place, and nothing readable where it no longer is.
///
/// Verifies: L.user.73
fn check_remap() -> Result<(), &'static str> {
    const STRING: &[u8] = b"a string";
    let space = fresh()?;
    let at = FIXED;
    let _ = space
        .map_anonymous(at, 2 * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no region for mremap's case")?;
    let _ = space
        .map_anonymous(at + 2 * PAGE_SIZE, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no neighbour for mremap's case")?;
    write_all(&space, at, STRING).map_err(|_| "mremap's case could not write")?;
    if space.remap(at, 2 * PAGE_SIZE, 4 * PAGE_SIZE, Destination::InPlace)
        != Err(SpaceError::OutOfMemory)
    {
        return Err("remap grew a region over its neighbour without leave to move");
    }
    let new = space
        .remap(at, 2 * PAGE_SIZE, 4 * PAGE_SIZE, Destination::Anywhere)
        .map_err(|_| "remap refused to move a region it may move")?;
    if !holds(&space, new, STRING) {
        return Err("remap did not carry a region's contents with it");
    }
    if read(&space, at).is_ok() {
        return Err("the old address still reads after a region moved away");
    }
    if !zeros(&space, new + 3 * PAGE_SIZE, PAGE_SIZE) {
        return Err("remap grew a region with something other than zeros");
    }
    let same = space
        .remap(new, 4 * PAGE_SIZE, PAGE_SIZE, Destination::InPlace)
        .map_err(|_| "remap refused to shrink a region in place")?;
    if same != new || !holds(&space, new, STRING) {
        return Err("a region shrunk in place did not keep its first page");
    }
    if read(&space, new + PAGE_SIZE).is_ok() {
        return Err("a shrunk region's tail still reads");
    }
    let back = space
        .remap(new, PAGE_SIZE, PAGE_SIZE, Destination::Fixed(at))
        .map_err(|_| "a move to a fixed address was refused")?;
    if back != at || !holds(&space, at, STRING) {
        return Err("a move to a fixed address did not land there with its contents");
    }
    Ok(())
}

/// An object over a source shows the source's bytes through a mapping, never
/// zeros, the pages going in through `insert_absent`.
///
/// Verifies: L.user.37
fn check_a_fault_fills_from_the_source() -> Result<(), &'static str> {
    let source = Source::new();
    let object = Vmo::new_filled(64, Some(Arc::clone(&source) as Arc<dyn Filler>))
        .map_err(|_| "no object over a source")?;
    let space = fresh()?;
    let len = 64 * PAGE_SIZE;
    let shared = space
        .map_file(
            FilePlace::Anywhere(None),
            len,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ
            },
            Arc::clone(&object),
            0,
            mapping(false),
        )
        .map_err(|_| "no shared mapping of an object over a source")?;
    let private = space
        .map_file(
            FilePlace::Anywhere(None),
            len,
            VmaFlags::READ_WRITE,
            Arc::clone(&object),
            0,
            mapping(false),
        )
        .map_err(|_| "no private mapping of an object over a source")?;
    let offset = 5 * PAGE_SIZE + 7;
    if read(&space, shared + offset) != Ok(pattern(offset)) {
        return Err("a shared mapping of an object over a source did not show the source's byte");
    }
    if source.calls.load(Ordering::Relaxed) != 1 || object.page(5).is_none() {
        return Err("a fault through a shared mapping did not fill the page from the source");
    }
    let written = 50 * PAGE_SIZE + 9;
    write(&space, private + written, 0xA5).map_err(|_| "a private write over a source failed")?;
    if read(&space, private + written + 1) != Ok(pattern(written + 1)) {
        return Err("a private write to a page nothing had read copied zeros, not the source");
    }
    if read(&space, private + written) != Ok(0xA5) {
        return Err("a private write over a source did not read back");
    }
    let mut own = [0_u8];
    object
        .read_page(50, 9, &mut own)
        .map_err(|_| "the object's page 50 would not read")?;
    if own != [pattern(written)] {
        return Err("a private write reached the object's own page, or it lost the source's byte");
    }
    Ok(())
}

/// A disk file's pages inside a region's range are its file pages, the others
/// nothing, and a page a private mapping wrote anonymous.
///
/// Verifies: L.user.172
fn check_a_disk_file_counts_as_a_file() -> Result<(), &'static str> {
    let source = Source::new();
    let object = Vmo::new_filled(8, Some(source as Arc<dyn Filler>))
        .map_err(|_| "no object over a source to count")?;
    for index in 0..8 {
        object
            .fill_for_fault(index)
            .map_err(|_| "a page of the file to count would not fill")?;
    }
    let space = fresh()?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            4 * PAGE_SIZE,
            VmaFlags::READ,
            Arc::clone(&object),
            2 * PAGE_SIZE,
            mapping(false),
        )
        .map_err(|_| "no read-only mapping of pages 2 to 5")?;
    let counted = space
        .resident_split()
        .map_err(|_| "no memory to count a space")?;
    if counted.file != 4 || counted.anon != 0 || counted.shmem != 0 {
        return Err("a disk file's cached pages were not counted by the region's range, as file");
    }
    space
        .unmap(at, 4 * PAGE_SIZE)
        .map_err(|_| "the counted mapping would not unmap")?;
    let private = space
        .map_file(
            FilePlace::Anywhere(None),
            PAGE_SIZE,
            VmaFlags::READ_WRITE,
            object,
            0,
            mapping(false),
        )
        .map_err(|_| "no private mapping of page 0")?;
    write(&space, private, 1).map_err(|_| "the private page to count would not write")?;
    let counted = space
        .resident_split()
        .map_err(|_| "no memory to count a space")?;
    if counted.anon != 1 || counted.file != 0 {
        return Err("a disk file's page a private mapping wrote was not counted as anonymous");
    }
    Ok(())
}

/// A file mapped shared and privately over 4 pages, the private mapping having
/// copied pages 0 and 1, and `at`'s two mappings.
fn cut_ready(space: &AddressSpace) -> Result<(Arc<Vmo>, u64, u64), &'static str> {
    let object = memory_file(2 * PAGE_SIZE + 100)?;
    let len = 4 * PAGE_SIZE;
    let shared = space
        .map_file(
            FilePlace::Anywhere(None),
            len,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no shared mapping of the file to cut")?;
    let private = space
        .map_file(
            FilePlace::Anywhere(None),
            len,
            VmaFlags::READ_WRITE,
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no private mapping of the file to cut")?;
    write(space, private, 0x11).map_err(|_| "a private copy of page 0 failed")?;
    write(space, private + PAGE_SIZE, 0x22).map_err(|_| "a private copy of page 1 failed")?;
    Ok((object, shared, private))
}

/// Once a file is cut, a fault through either mapping reaches no page wholly
/// past the end, a page the private mapping copied included.
///
/// Verifies: L.user.39, L.user.92
fn check_a_cut_file_refuses_past_its_end() -> Result<(), &'static str> {
    let space = fresh()?;
    let (object, shared, private) = cut_ready(&space)?;
    cut_file(&object, PAGE_SIZE);
    if read(&space, shared) != Ok(pattern(0)) {
        return Err("page 0 of a cut file did not read through the shared mapping");
    }
    for page in 1..4 {
        if read(&space, shared + page * PAGE_SIZE).is_ok() {
            return Err("a page past a cut file's end read through the shared mapping");
        }
        if object.page(page).is_some() {
            return Err("a fault past a cut file's end committed a page");
        }
        if read(&space, private + page * PAGE_SIZE).is_ok() {
            return Err("a page past a cut file's end read through the private mapping");
        }
    }
    Ok(())
}

/// The file grown back shows zeros through both mappings, the page the private
/// mapping had copied included.
///
/// Verifies: L.user.40, L.user.92
fn check_a_file_grown_back_shows_zeros() -> Result<(), &'static str> {
    let space = fresh()?;
    let (object, shared, private) = cut_ready(&space)?;
    cut_file(&object, PAGE_SIZE);
    object.set_file_len(2 * PAGE_SIZE);
    if !zeros(&space, shared + PAGE_SIZE, PAGE_SIZE) {
        return Err("a file grown back did not read zeros through the shared mapping");
    }
    if !zeros(&space, private + PAGE_SIZE, PAGE_SIZE) {
        return Err("a file grown back kept a private copy past the cut");
    }
    if read(&space, private) != Ok(0x11) {
        return Err("a cut took a private copy below it");
    }
    Ok(())
}

/// A truncated file grown again reads zeros, never its old bytes; a held page
/// stays.
///
/// Verifies: L.user.41
fn check_decommit_from() -> Result<(), &'static str> {
    let object = memory_file(3 * PAGE_SIZE + 123)?;
    cut_file(&object, PAGE_SIZE + 10);
    object.set_file_len(3 * PAGE_SIZE + 123);
    let mut head = [0_u8; 10];
    for (slot, at) in head.iter_mut().zip(PAGE_SIZE..) {
        *slot = pattern(at);
    }
    if !file_holds(&object, PAGE_SIZE, &head) {
        return Err("a truncation lost what is before the cut");
    }
    let zero = [0_u8; 4086];
    if !file_holds(&object, PAGE_SIZE + 10, &zero) {
        return Err("a truncated file's page reads old bytes after the cut");
    }
    if object.page(2).is_some() || object.page(3).is_some() {
        return Err("decommit_from left a committed page past the cut");
    }

    // A held page stays where a device reads it, and goes when it is let go.
    let held = Vmo::new_anonymous(4).map_err(|_| "no object to hold a page of")?;
    let _ = held.commit(1).map_err(|_| "no page to hold")?;
    let _ = held.commit(2).map_err(|_| "no page to give back")?;
    let hold = held.hold(1, 1).map_err(|_| "a page would not hold")?;
    let _ = held.decommit_from(0);
    if held.page(1).is_none() {
        return Err("decommit_from gave back a held page");
    }
    if held.page(2).is_some() {
        return Err("decommit_from kept a page no one held");
    }
    drop(hold);
    let _ = held.decommit_from(0);
    if held.page(1).is_some() {
        return Err("decommit_from kept a page no longer held");
    }
    Ok(())
}

/// The shared mappings that may write are counted, a fork child's included.
///
/// Verifies: L.user.42
fn check_the_may_write_count() -> Result<(), &'static str> {
    let object = memory_file(PAGE_SIZE)?;
    let space = fresh()?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no shared mapping that may write")?;
    if !object.writably_mapped() {
        return Err("a shared mapping that may write was not counted");
    }
    let child = space
        .fork()
        .map_err(|_| "no fork for the may-write count")?;
    space
        .unmap(at, PAGE_SIZE)
        .map_err(|_| "the mapping that may write would not unmap")?;
    if !object.writably_mapped() {
        return Err("a fork child's copy of a shared mapping that may write was not counted");
    }
    child
        .unmap(at, PAGE_SIZE)
        .map_err(|_| "the child's mapping would not unmap")?;
    if object.writably_mapped() {
        return Err("no shared mapping may write, and the count says one does");
    }
    let _ = space
        .map_file(
            FilePlace::Anywhere(None),
            PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ
            },
            Arc::clone(&object),
            0,
            mapping(false),
        )
        .map_err(|_| "no shared mapping that may not write")?;
    if object.writably_mapped() {
        return Err("a shared mapping that may not write was counted as one that may");
    }
    Ok(())
}

/// A fork child's write to a shared anonymous page reaches its parent, a page
/// the child faults in first included, and its write to a private page doesn't.
fn check_a_shared_page_crosses_a_fork() -> Result<(), &'static str> {
    let parent = fresh()?;
    let shared_flags = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let shared = parent
        .map_anywhere(None, 2 * PAGE_SIZE, shared_flags)
        .map_err(|_| "no shared region to fork")?;
    let private = parent
        .map_anywhere(None, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no private region to fork")?;
    write(&parent, shared, 1).map_err(|_| "the parent's shared write failed")?;
    write(&parent, private, 1).map_err(|_| "the parent's private write failed")?;
    let child = parent.fork().map_err(|_| "no fork of a shared region")?;
    write(&child, shared, 2).map_err(|_| "the child's shared write failed")?;
    if read(&parent, shared) != Ok(2) {
        return Err("a fork child's write to a shared page did not reach its parent");
    }
    write(&child, shared + PAGE_SIZE, 3).map_err(|_| "the child's first shared write failed")?;
    if read(&parent, shared + PAGE_SIZE) != Ok(3) {
        return Err("a shared page the fork child faulted in first did not reach its parent");
    }
    write(&child, private, 4).map_err(|_| "the child's private write failed")?;
    if read(&parent, private) != Ok(1) {
        return Err("a fork child's write to a private page reached its parent");
    }
    Ok(())
}

/// A shared region's address names the object and offset every space mapping
/// it names it by.
///
/// Verifies: L.user.78
fn check_shared_object_at() -> Result<(), &'static str> {
    let parent = fresh()?;
    let shared = parent
        .map_anywhere(
            None,
            2 * PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
        )
        .map_err(|_| "no shared region to name")?;
    let private = parent
        .map_anywhere(None, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no private region to name")?;
    let child = parent.fork().map_err(|_| "no fork to name a region in")?;
    let word = shared + PAGE_SIZE + 8;
    let (Some((one, one_at)), Some((two, two_at))) =
        (parent.shared_object_at(word), child.shared_object_at(word))
    else {
        return Err("shared_object_at named nothing for a shared region");
    };
    if !Arc::ptr_eq(&one, &two) {
        return Err("shared_object_at named two objects for one shared region across a fork");
    }
    if one_at != PAGE_SIZE + 8 || two_at != one_at {
        return Err("shared_object_at named the wrong byte offset in a shared region");
    }
    if parent.shared_object_at(private).is_some() {
        return Err("shared_object_at named an object for a private region");
    }

    // A second mapping of the object elsewhere names it by the same offset.
    let file = memory_file(2 * PAGE_SIZE)?;
    let flags = VmaFlags {
        shared: true,
        ..VmaFlags::READ
    };
    let low = parent
        .map_file(
            FilePlace::Anywhere(None),
            PAGE_SIZE,
            flags,
            Arc::clone(&file),
            PAGE_SIZE,
            mapping(false),
        )
        .map_err(|_| "no mapping of a file's second page")?;
    let high = child
        .map_file(
            FilePlace::Anywhere(None),
            2 * PAGE_SIZE,
            flags,
            Arc::clone(&file),
            0,
            mapping(false),
        )
        .map_err(|_| "no mapping of a whole file")?;
    let (Some((one, one_at)), Some((two, two_at))) = (
        parent.shared_object_at(low + 4),
        child.shared_object_at(high + PAGE_SIZE + 4),
    ) else {
        return Err("shared_object_at named nothing for a shared file mapping");
    };
    if !Arc::ptr_eq(&one, &two) || one_at != two_at || one_at != PAGE_SIZE + 4 {
        return Err("shared_object_at named a file's word differently in two spaces");
    }
    Ok(())
}

/// `protect` takes the range's translations out, and the next access is judged
/// by the new permissions, with the contents kept.
///
/// Verifies: L.user.86
fn check_protect_takes_translations_out() -> Result<(), &'static str> {
    let space = fresh()?;
    let at = space
        .map_anywhere(None, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no region to protect")?;
    write(&space, at, 0x77).map_err(|_| "the page to protect would not write")?;
    if mm::translate_in(space.root_table(), at).is_none() {
        return Err("a written page did not translate before protect");
    }
    space
        .protect(at, PAGE_SIZE, VmaFlags::READ)
        .map_err(|_| "protect to read-only was refused")?;
    if mm::translate_in(space.root_table(), at).is_some() {
        return Err("protect left the range's translation in the tables");
    }
    if write(&space, at, 0x88).is_ok() {
        return Err("a write after protect to read-only was not refused");
    }
    if read(&space, at) != Ok(0x77) {
        return Err("protect did not keep the page's contents");
    }
    Ok(())
}

/// `protect` keeps whether a region is shared.
///
/// Verifies: L.user.128
fn check_protect_keeps_shared() -> Result<(), &'static str> {
    let object = memory_file(PAGE_SIZE)?;
    let space = fresh()?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no shared mapping to protect")?;
    // As `mprotect` asks: the three permissions, nothing about sharing.
    space
        .protect(at, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "protect of a shared mapping was refused")?;
    if write(&space, at + 5, 0xEE).is_err() || !file_holds(&object, 5, &[0xEE]) {
        return Err("protect made a shared mapping private: the write is not in the file");
    }
    Ok(())
}

/// A region that permits writing reads, and one that permits nothing does not.
///
/// Verifies: L.user.129
fn check_a_write_only_region_reads() -> Result<(), &'static str> {
    let space = fresh()?;
    let write_only = VmaFlags {
        read: false,
        write: true,
        ..VmaFlags::NONE
    };
    let at = space
        .map_anywhere(None, PAGE_SIZE, write_only)
        .map_err(|_| "no write-only region")?;
    if read(&space, at) != Ok(0) {
        return Err("a read of a region that permits writing was refused");
    }
    let none = space
        .map_anywhere(None, PAGE_SIZE, VmaFlags::NONE)
        .map_err(|_| "no inaccessible region")?;
    if read(&space, none).is_ok() {
        return Err("a read of a region that permits nothing was allowed");
    }
    Ok(())
}

/// `protect` refuses to make writable a shared file mapping that may not write
/// its file, before changing anything.
///
/// Verifies: L.user.89
fn check_protect_refuses_an_unwritable_mapping() -> Result<(), &'static str> {
    let object = memory_file(PAGE_SIZE)?;
    let space = fresh()?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ
            },
            object,
            0,
            mapping(false),
        )
        .map_err(|_| "no shared mapping that may not write")?;
    if read(&space, at) != Ok(pattern(0)) {
        return Err("a shared read-only mapping did not show the file");
    }
    if space.protect(at, PAGE_SIZE, VmaFlags::READ_WRITE) != Err(SpaceError::Refused(at)) {
        return Err("protect made writable a shared mapping that may not write its file");
    }
    if mm::translate_in(space.root_table(), at).is_none() {
        return Err("a refused protect took the range's translation out");
    }
    if write(&space, at, 1).is_ok() {
        return Err("a refused protect changed the region's permissions");
    }
    Ok(())
}

/// A shared file mapping shows the file's own pages, both ways.
///
/// Verifies: L.user.93
fn check_a_shared_mapping_is_the_file() -> Result<(), &'static str> {
    const THROUGH_MAP: &[u8] = b"written through a shared mapping";
    const THROUGH_FILE: &[u8] = b"written to the file";
    let len = 2 * PAGE_SIZE + 100;
    let object = memory_file(len)?;
    let space = fresh()?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            4 * PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no shared mapping of a file")?;
    if !(0..len).all(|offset| read(&space, at + offset) == Ok(pattern(offset))) {
        return Err("a shared mapping did not show the file's bytes");
    }
    if !zeros(&space, at + len, 3 * PAGE_SIZE - len) {
        return Err("a shared mapping did not show zeros to the end of the file's last page");
    }
    write_all(&space, at + 40, THROUGH_MAP).map_err(|_| "a write through a mapping failed")?;
    if !file_holds(&object, 40, THROUGH_MAP) {
        return Err("a write through a shared mapping is not what the file reads back");
    }
    object
        .commit_and_write(1, 200, THROUGH_FILE)
        .map_err(|_| "a write to the mapped file failed")?;
    if !holds(&space, at + PAGE_SIZE + 200, THROUGH_FILE) {
        return Err("a write to the file is not what a shared mapping shows");
    }
    Ok(())
}

/// A private mapping shows the file until it writes a page and then keeps it.
///
/// Verifies: L.user.94
fn check_a_private_mapping_keeps_its_copies() -> Result<(), &'static str> {
    const BEHIND: &[u8] = b"written to the file behind a private mapping";
    let object = memory_file(3 * PAGE_SIZE)?;
    let space = fresh()?;
    let shared = space
        .map_file(
            FilePlace::Anywhere(None),
            3 * PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no shared mapping beside the private one")?;
    let private = space
        .map_file(
            FilePlace::Anywhere(None),
            3 * PAGE_SIZE,
            VmaFlags::READ_WRITE,
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no private mapping of a file")?;
    if read(&space, private + 3) != Ok(pattern(3)) {
        return Err("a private mapping did not show the file");
    }
    // A write through it, as a program's and as a read's copy into it are both
    // `with_page` for a write.
    write(&space, private + 7, 0x3C).map_err(|_| "a write through a private mapping failed")?;
    if read(&space, private + 7) != Ok(0x3C) {
        return Err("a write through a private mapping did not show in it");
    }
    if !file_holds(&object, 7, &[pattern(7)]) {
        return Err("a write through a private mapping reached the file");
    }
    if read(&space, shared + 7) != Ok(pattern(7)) {
        return Err("a write through a private mapping reached a shared mapping");
    }
    object
        .commit_and_write(2, 10, BEHIND)
        .map_err(|_| "a write behind a private mapping failed")?;
    if !holds(&space, private + 2 * PAGE_SIZE + 10, BEHIND) {
        return Err("a write to the file did not show through a page the mapping never copied");
    }
    object
        .commit_and_write(0, 600, BEHIND)
        .map_err(|_| "a write behind a private copy failed")?;
    if holds(&space, private + 600, BEHIND) {
        return Err("a write to the file showed through a page the mapping copied");
    }
    Ok(())
}

/// A fork gives the child the private mapping's copies, and its write into one
/// reaches neither the parent nor the file.
///
/// Verifies: L.user.95
fn check_a_fork_keeps_private_copies_apart() -> Result<(), &'static str> {
    let object = memory_file(2 * PAGE_SIZE)?;
    let parent = fresh()?;
    let private = parent
        .map_file(
            FilePlace::Anywhere(None),
            2 * PAGE_SIZE,
            VmaFlags::READ_WRITE,
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no private mapping to fork")?;
    write(&parent, private + PAGE_SIZE + 3, 0x41).map_err(|_| "the parent's copy failed")?;
    let child = parent.fork().map_err(|_| "no fork of a private mapping")?;
    if read(&child, private + PAGE_SIZE + 3) != Ok(0x41) {
        return Err("a fork child did not read its parent's private copy");
    }
    write(&child, private + PAGE_SIZE + 3, 0x42).map_err(|_| "the child's write failed")?;
    if read(&parent, private + PAGE_SIZE + 3) != Ok(0x41) {
        return Err("a fork child's write into a private copy reached its parent");
    }
    if !file_holds(&object, PAGE_SIZE + 3, &[pattern(PAGE_SIZE + 3)]) {
        return Err("a fork child's write into a private copy reached the file");
    }
    if read(&child, private + PAGE_SIZE + 3) != Ok(0x42) {
        return Err("a fork child's write into a private copy did not show in it");
    }
    Ok(())
}

/// A fixed private mapping lands exactly where asked: an executable one not
/// writable, and a writable one made read-only keeping what was written.
///
/// Verifies: L.user.97
fn check_the_loader_pattern() -> Result<(), &'static str> {
    let object = memory_file(2 * PAGE_SIZE)?;
    let space = fresh()?;
    let code = space
        .map_file(
            FilePlace::Fixed(FIXED),
            PAGE_SIZE,
            VmaFlags::READ_EXECUTE,
            Arc::clone(&object),
            0,
            mapping(true),
        )
        .map_err(|_| "no fixed read-execute mapping")?;
    if code != FIXED {
        return Err("a fixed private mapping did not land where asked");
    }
    if read(&space, FIXED + 11) != Ok(pattern(11)) {
        return Err("a fixed read-execute mapping did not show the file");
    }
    if write(&space, FIXED + 11, 1).is_ok() {
        return Err("a fixed read-execute mapping took a write");
    }
    let data = FIXED + PAGE_SIZE;
    if space.map_file(
        FilePlace::Fixed(data),
        PAGE_SIZE,
        VmaFlags::READ_WRITE,
        object,
        PAGE_SIZE,
        mapping(true),
    ) != Ok(data)
    {
        return Err("a fixed read-write mapping did not land next to it");
    }
    write(&space, data, 0x6B).map_err(|_| "a fixed read-write mapping refused a write")?;
    space
        .protect(data, PAGE_SIZE, VmaFlags::READ)
        .map_err(|_| "protect of the loader's data page was refused")?;
    if read(&space, data) != Ok(0x6B) {
        return Err("a private mapping made read-only did not keep what was written");
    }
    if write(&space, data, 0x6C).is_ok() {
        return Err("a private mapping made read-only took the next write");
    }
    Ok(())
}

/// True iff the object has no filler and no file bound.
///
/// Verifies: L.user.126
fn check_is_anonymous() -> Result<(), &'static str> {
    let plain = Vmo::new_anonymous(1).map_err(|_| "no plain object")?;
    if !plain.is_anonymous() {
        return Err("is_anonymous answered false for a plain object");
    }
    let filled = Vmo::new_filled(1, Some(Source::new() as Arc<dyn Filler>))
        .map_err(|_| "no object over a source")?;
    if filled.is_anonymous() {
        return Err("is_anonymous answered true for an object over a source");
    }
    let bound = Vmo::new_filled(1, None).map_err(|_| "no object to bind")?;
    bound.set_file_len(PAGE_SIZE);
    if bound.is_anonymous() {
        return Err("is_anonymous answered true for a memory file's object");
    }
    Ok(())
}

/// A page filled before a private mapping's fault took its lock and reclaimed
/// since is asked for again, never copied as zeros, and a read of one answers
/// that it is absent.
///
/// Verifies: L.object.112
fn check_a_fill_reclaimed_is_filled_again() -> Result<(), &'static str> {
    let object = Vmo::new_filled(
        4,
        Some(Arc::new(EvictedOnce {
            left: AtomicUsize::new(1),
        }) as Arc<dyn Filler>),
    )
    .map_err(|_| "no object over a source that reclaims")?;
    let space = fresh()?;
    let private = space
        .map_file(
            FilePlace::Anywhere(None),
            4 * PAGE_SIZE,
            VmaFlags::READ_WRITE,
            Arc::clone(&object),
            0,
            mapping(false),
        )
        .map_err(|_| "no private mapping over a source that reclaims")?;
    write(&space, private + PAGE_SIZE + 9, 0xA5).map_err(|_| "a private write failed")?;
    let beside = PAGE_SIZE + 10;
    if read(&space, private + beside) != Ok(pattern(beside)) {
        return Err("a private write to a page reclaimed as it was filled copied zeros");
    }
    let mut out = [0x5A_u8; 8];
    if object.read_present(3, 0, &mut out) != Ok(false) || out != [0x5A_u8; 8] {
        return Err("read_present did not answer that a page the object lacks is absent");
    }
    Ok(())
}

/// A commit if the object lacks the page, the bytes at the offset, in place, an
/// object left as it was without memory; and `zero_from`.
///
/// Verifies: L.object.190
fn check_commit_and_write() -> Result<(), &'static str> {
    const BYTES: &[u8] = b"at an offset";
    let object = Vmo::new_anonymous(4).map_err(|_| "no object to write")?;
    object
        .commit_and_write(1, 100, BYTES)
        .map_err(|_| "commit_and_write refused an absent page")?;
    if !file_holds(&object, PAGE_SIZE + 100, BYTES) {
        return Err("commit_and_write did not put the bytes at the offset");
    }
    if !file_holds(&object, PAGE_SIZE + 90, &[0; 10]) {
        return Err("commit_and_write's new page was not zeroed around the bytes");
    }

    // A frame another holder shares is written in place.
    let holder = object.fork().map_err(|_| "no second holder of the page")?;
    object
        .commit_and_write(1, 0, b"both")
        .map_err(|_| "commit_and_write refused a shared page")?;
    if !file_holds(&holder, PAGE_SIZE, b"both") {
        return Err("commit_and_write copied a frame another holder shares");
    }

    // Without a frame or a place to record it: refused, and nothing changed.
    let me = crate::sched::current_id().ok_or("the memory check is on no task")?;
    let mut refused = 0;
    for nth in 1_u32..8 {
        crate::fallible::inject_once(me, nth, true);
        let answer = object.commit_and_write(2, 0, BYTES);
        let _ = crate::fallible::stop_injecting();
        match answer {
            Ok(()) => break,
            Err(VmoError::OutOfMemory) if object.page(2).is_none() => refused += 1,
            Err(_) => {
                return Err("commit_and_write without memory answered otherwise or kept a page");
            }
        }
    }
    if refused == 0 || !file_holds(&object, 2 * PAGE_SIZE, BYTES) {
        return Err("commit_and_write was never refused for memory, or never went through");
    }

    object
        .commit_and_write(1, PAGE_SIZE as usize - 1, &[9])
        .map_err(|_| "commit_and_write refused a page's last byte")?;
    object
        .zero_from(1, 3)
        .map_err(|_| "zero_from refused a span in its page")?;
    if !file_holds(&object, PAGE_SIZE, b"bot") || !file_holds(&object, 2 * PAGE_SIZE - 1, &[0]) {
        return Err("zero_from did not zero from the offset to the page's end");
    }
    object
        .zero_from(3, 3)
        .map_err(|_| "zero_from refused an absent page")?;
    if object.page(3).is_some() {
        return Err("zero_from committed a page the object lacked");
    }
    Ok(())
}

/// Only on x86-64, and only plain anonymous memory no space maps, and refused
/// leaving the object as it was.
///
/// Verifies: L.user.132
fn check_make_combining() -> Result<(), &'static str> {
    let memory = memory_file(PAGE_SIZE)?;
    if memory.make_combining() || memory.is_combining() {
        return Err("make_combining took a memory file's object");
    }
    let mapped = Vmo::new_anonymous(1).map_err(|_| "no object to map")?;
    let space = fresh()?;
    let _ = space
        .map_object(
            None,
            PAGE_SIZE,
            Arc::clone(&mapped),
            0,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
        )
        .map_err(|_| "no mapping of the object")?;
    if mapped.make_combining() || mapped.is_combining() {
        return Err("make_combining took an object a space maps");
    }
    let plain = Vmo::new_anonymous(1).map_err(|_| "no plain object")?;
    let _ = plain.commit(0).map_err(|_| "no page of the plain object")?;
    let made = plain.make_combining();
    /// `EM_X86_64`.
    const EM_X86_64: u16 = 62;
    if crate::arch::ARCH.elf_machine() == EM_X86_64 {
        if !made || !plain.is_combining() {
            return Err("make_combining refused plain anonymous memory on x86-64");
        }
    } else if made || plain.is_combining() {
        return Err("make_combining took an object where no attribute has been argued");
    }
    Ok(())
}
