//! An unmap on one processor waits for a copy on another to let go of its page.
//!
//! Every copy to or from a program runs inside `AddressSpace::with_page`, which
//! holds the space's lock from the translation to the end of the copy, and an
//! unmap takes the same lock before it takes the translation down and gives
//! the frame back. With threads that is what stands between a `write` copying
//! from a buffer and another thread's `munmap` of it: without it the copy
//! reads, or writes, a frame already handed to somebody else.
//!
//! The check makes the race slow enough to see. A task on one processor holds
//! a page inside `with_page` for a fixed time; a task on another starts an
//! unmap of that page while it does. The unmap must not have returned when the
//! copy ends, and must have returned once both tasks are gone. A `with_page`
//! that let the lock go before the copy lets the unmap through at once, and
//! fails by name. It needs two processors, and on one it does not run.
//!
//! What it shows is the kernel's half of the race, at the call every copy goes
//! through: a user program of two threads, one in `write` and one in `munmap`,
//! reaches the same call.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_vma::VmaFlags;

use crate::sync::SpinLock;
use crate::syscall::process;
use crate::syscall::uaccess;
use crate::user::space::{Access, AddressSpace};

/// What the page holds, so the copy can tell it read the page it held.
const PATTERN: u8 = 0x5C;

/// How long the holding task keeps its page, spinning with the space's lock
/// held. Far longer than an unmap takes once it has the lock, even on a slow
/// host; short enough that two processors spinning for it cost the boot
/// nothing it would notice.
const HOLD_NANOS: u64 = 30_000_000;

/// How long the check waits for its tasks to start or to end.
const PATIENCE_NANOS: u64 = 30_000_000_000;

/// The failure a copy that does not hold its page produces, which the negative
/// control requires by name.
pub(crate) const UNMAPPED_DURING_COPY: &str =
    "an unmap on another processor returned while a copy still held its page";

/// The space and page the two tasks work on.
static SUBJECT: SpinLock<Option<(Arc<AddressSpace>, u64)>> = SpinLock::new(None);

/// Raised by the holding task once it holds the page.
static HOLDING: AtomicBool = AtomicBool::new(false);

/// Raised by the unmapping task once its unmap has returned.
static UNMAPPED: AtomicBool = AtomicBool::new(false);

/// Whether the unmap had returned when the copy ended: the failure.
static UNMAPPED_WHILE_HELD: AtomicBool = AtomicBool::new(false);

/// What the holding task read: the pattern, or zero if it read nothing.
static READ: AtomicU8 = AtomicU8::new(0);

/// Run the check. `Ok(false)` when there is one processor and it did not run.
///
/// # Errors
///
/// [`UNMAPPED_DURING_COPY`] if the unmap got past a held page, and a message
/// for a check that could not be set up or whose tasks never finished.
///
/// Verifies: L.user.63
pub(crate) fn check_an_unmap_waits_for_a_copy_holding_its_page() -> Result<bool, &'static str> {
    let online = crate::smp::topology().map_or(1, crate::smp::Topology::online);
    if online < 2 {
        return Ok(false);
    }
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the unmap check")?;
    let space = Arc::clone(process.space());
    let page = space
        .map_anywhere(None, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "the unmap check's page was refused")?;
    uaccess::copy_to_user(&space, page, &[PATTERN])
        .map_err(|_| "could not write the unmap check's page")?;

    HOLDING.store(false, Ordering::SeqCst);
    UNMAPPED.store(false, Ordering::SeqCst);
    UNMAPPED_WHILE_HELD.store(false, Ordering::SeqCst);
    READ.store(0, Ordering::SeqCst);
    *SUBJECT.lock() = Some((space, page));

    let holder = crate::sched::spawn_on("unmap-holder", holder, 0, NICE_0_WEIGHT, 0, only(0)?)?;
    let unmapper =
        crate::sched::spawn_on("unmap-unmapper", unmapper, 0, NICE_0_WEIGHT, 1, only(1)?)?;

    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !(holder.is_dead() && unmapper.is_dead()) {
        if crate::timer::now_nanos() >= deadline {
            return Err("the unmap check's tasks never finished");
        }
        crate::sched::sleep_for(1_000_000);
    }
    crate::sched::wait_until_gone(&holder, crate::sched::REAPER_PATIENCE_NANOS)?;
    crate::sched::wait_until_gone(&unmapper, crate::sched::REAPER_PATIENCE_NANOS)?;
    let _ = SUBJECT.lock().take();

    if READ.load(Ordering::SeqCst) != PATTERN {
        return Err("the unmap check's copy did not read its page");
    }
    if UNMAPPED_WHILE_HELD.load(Ordering::SeqCst) {
        return Err(UNMAPPED_DURING_COPY);
    }
    if !UNMAPPED.load(Ordering::SeqCst) {
        return Err("the unmap check's unmap never returned");
    }
    Ok(true)
}

/// A set of the one processor `cpu`.
fn only(cpu: usize) -> Result<CpuSet, &'static str> {
    let mut set = CpuSet::empty();
    set.insert(cpu)
        .map_err(|_| "the unmap check names a processor out of range")?;
    Ok(set)
}

/// The space and page, cloned out of [`SUBJECT`].
fn subject() -> Option<(Arc<AddressSpace>, u64)> {
    SUBJECT
        .lock()
        .as_ref()
        .map(|(space, page)| (Arc::clone(space), *page))
}

/// Hold the page inside `with_page` for [`HOLD_NANOS`], and note whether the
/// unmap had returned by the end.
fn holder(_argument: usize) {
    let Some((space, page)) = subject() else {
        return;
    };
    let _ = space.with_page(page, Access::READ, |source| {
        // SAFETY: (USER-COPY) `with_page` translated the page through the space's own
        // tables and runs this with the space's lock held, so `source` is the
        // direct-map address of a live frame for as long as this runs.
        let byte = unsafe { (source as *const u8).read() };
        HOLDING.store(true, Ordering::SeqCst);
        let until = crate::timer::now_nanos().saturating_add(HOLD_NANOS);
        while crate::timer::now_nanos() < until {
            core::hint::spin_loop();
        }
        UNMAPPED_WHILE_HELD.store(UNMAPPED.load(Ordering::SeqCst), Ordering::SeqCst);
        READ.store(byte, Ordering::SeqCst);
    });
}

/// Once the holder holds the page, unmap it.
fn unmapper(_argument: usize) {
    let Some((space, page)) = subject() else {
        return;
    };
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !HOLDING.load(Ordering::SeqCst) {
        if crate::timer::now_nanos() >= deadline {
            return;
        }
        core::hint::spin_loop();
    }
    if space.unmap(page, PAGE_SIZE).is_ok() {
        UNMAPPED.store(true, Ordering::SeqCst);
    }
}
