//! [`super::inspect`] proved at boot (`docs/DEBUG.md` §1.3): a blocked task's
//! chain as the task saw it itself, a freed stack read without a fault, and a
//! device window read as nothing.
//!
//! * **The chain** (`L.sched.87`). A kernel task records its own chain with
//!   `backtrace::walk` from inside [`record_and_hold`] and then blocks. The
//!   view's chain of it must end in what it recorded above its first frame,
//!   with the image's slide taken off -- the view walks more frames below
//!   that, the wait and the switch -- and every address it reports must be
//!   inside the image as it was linked. The checker waits until the view sees
//!   the task blocked, so a view taken while the task was still on its way
//!   to the switch is never one the check judges.
//! * **A freed stack** (`L.sched.88`). A stack from `vmap`, read through
//!   `mm::read_kernel_word` while mapped, then freed and read again, answers
//!   nothing; a read through its own address would fault.
//! * **A device window** (`L.sched.89`). A page of `vmap` mapped onto a
//!   physical address that is no RAM answers nothing: the read is refused
//!   before it is made, as a read through the direct map there would fault.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sched::NICE_0_WEIGHT;

use super::WaitQueue;
use super::inspect::{self, MAX_FRAMES};
use crate::{arch, backtrace, mm, vmap};

/// What the boot line says.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// Frames the view walked of the blocked task.
    pub(crate) frames: usize,
    /// Of them, the ones the task recorded itself.
    pub(crate) matched: usize,
    /// The image's slide this boot.
    pub(crate) slide: u64,
}

/// The chain the held task recorded, innermost first, as it ran.
static RECORDED: [AtomicU64; MAX_FRAMES] = [const { AtomicU64::new(0) }; MAX_FRAMES];
/// How many of [`RECORDED`] it filled.
static RECORDED_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Set once it has recorded, before it blocks.
static RECORDED_DONE: AtomicBool = AtomicBool::new(false);
/// Set to let it go.
static RELEASE: AtomicBool = AtomicBool::new(false);
/// What it blocks on.
static HOLD: WaitQueue = WaitQueue::new();

/// How long the checker waits for the held task to block: far longer than a
/// spawn and a wait take on any machine the boots run on.
const PATIENCE_NANOS: u64 = 5_000_000_000;

/// A physical address no machine the boots run on has RAM at, for the device
/// window: 512 GiB.
const NOT_RAM: u64 = 1 << 39;

/// The held task: record, then block until released.
fn held(_argument: usize) {
    record_and_hold();
}

/// Its own chain, then the wait. Not inlined, so that its frame is one the
/// view must walk through.
#[inline(never)]
fn record_and_hold() {
    let mut count = 0_usize;
    let _ = backtrace::walk(arch::frame_pointer(), |address| {
        if let Some(slot) = RECORDED.get(count) {
            slot.store(address, Ordering::Relaxed);
            count += 1;
        }
    });
    RECORDED_COUNT.store(count, Ordering::Relaxed);
    RECORDED_DONE.store(true, Ordering::Release);
    let _ = HOLD.wait_trusting(|| RELEASE.load(Ordering::Acquire));
}

/// Run the three cases.
///
/// # Errors
///
/// The case's sentence.
pub(crate) fn run() -> Result<Report, &'static str> {
    let report = chain_is_the_tasks_own()?;
    a_freed_stack_reads_as_nothing()?;
    a_device_window_reads_as_nothing()?;
    Ok(report)
}

/// The chain case.
///
/// Verifies: L.sched.87, H.SCHED.15
fn chain_is_the_tasks_own() -> Result<Report, &'static str> {
    RELEASE.store(false, Ordering::Release);
    RECORDED_DONE.store(false, Ordering::Release);
    let task = super::spawn("check-inspect", held, 0, NICE_0_WEIGHT)?;
    let mut chain = [0_u64; MAX_FRAMES];
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let found = loop {
        if RECORDED_DONE.load(Ordering::Acquire)
            && let Some(found) = inspect::chain_of(&task, &mut chain)
        {
            break Some(found);
        }
        if crate::timer::now_nanos() > deadline {
            break None;
        }
        super::yield_now();
    };
    RELEASE.store(true, Ordering::Release);
    HOLD.wake_all();
    super::wait_until_gone(&task, super::REAPER_PATIENCE_NANOS)?;
    drop(task);

    let found = found.ok_or("inspect: the held task was never seen blocked")?;
    let walked = chain.get(..found).unwrap_or_default();
    let slide = backtrace::slide();
    let linked = ferrix_bootinfo::KERNEL_VIRT_BASE;
    let end = linked.saturating_add(backtrace::image_len());
    if walked
        .iter()
        .any(|&address| !(linked..end).contains(&address))
    {
        return Err("inspect: a blocked task's chain named an address outside the linked image");
    }
    // Above the first frame the task recorded (`record_and_hold`'s own,
    // or the frame-pointer read's), the two must agree.
    let recorded_count = RECORDED_COUNT.load(Ordering::Relaxed).min(MAX_FRAMES);
    let recorded: alloc::vec::Vec<u64> = RECORDED
        .iter()
        .take(recorded_count)
        .skip(1)
        .map(|slot| slot.load(Ordering::Relaxed).wrapping_sub(slide))
        .collect();
    if recorded.is_empty() {
        return Err("inspect: the held task recorded no chain above its own frame");
    }
    if walked.len() <= recorded.len() || !walked.ends_with(&recorded) {
        return Err("inspect: a blocked task's chain did not end in the chain it recorded itself");
    }
    Ok(Report {
        frames: walked.len(),
        matched: recorded.len(),
        slide,
    })
}

/// The freed stack case.
///
/// Verifies: L.sched.88, H.SCHED.15
fn a_freed_stack_reads_as_nothing() -> Result<(), &'static str> {
    let stack = vmap::allocate_stack().map_err(|_| "inspect: no stack to free")?;
    let at = stack.top.saturating_sub(8);
    if mm::read_kernel_word(at).is_none() {
        // SAFETY: (KMEM) allocated above and never run on.
        let _ = unsafe { vmap::free_stack(stack) };
        return Err("inspect: a mapped stack's word read as nothing");
    }
    // SAFETY: (KMEM) allocated above and never run on; nothing else knows it.
    unsafe { vmap::free_stack(stack) }.map_err(|_| "inspect: the stack could not be freed")?;
    if mm::read_kernel_word(at).is_some() {
        return Err("inspect: a freed stack's word was read");
    }
    Ok(())
}

/// The device window case.
///
/// Verifies: L.sched.89, H.SCHED.15
fn a_device_window_reads_as_nothing() -> Result<(), &'static str> {
    let at = vmap::map_device(NOT_RAM, 4096).map_err(|_| "inspect: no device window to read")?;
    let read = mm::read_kernel_word(at);
    vmap::unmap_device(at).map_err(|_| "inspect: the device window could not be unmapped")?;
    if read.is_some() {
        return Err("inspect: a word of a device window was read");
    }
    Ok(())
}
