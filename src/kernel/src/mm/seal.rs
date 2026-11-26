//! K16's seal (`docs/microkernel/K16.md`, `max`'s deviation): after the first processes are
//! started the shared heap serves nobody, a job draws on its own promise, its nearest
//! ancestor's being its own (`Quota::draws_from`), a task of the kernel's with no job on a
//! named pool, and an interrupt on nothing. Only the global allocator: page tables, stacks,
//! IOMMU tables and `vmap` are the buddy's, counted here and not sealed.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::Reservation;
use crate::object::job::Job;
use crate::object::quota::{self, NO_TARGET, NONE, Resource};
use crate::sync::SpinLock;

/// The jobs a report says the frames of.
const PAYERS: usize = 8;
static SEALED: AtomicBool = AtomicBool::new(false);
/// Reports still to walk the frame records, asked by a row's check-only boot option
/// (`ferrix.check_walk_outside`, ledger 10892 P1): none in any other boot.
static WALKS_LEFT: AtomicU32 = AtomicU32::new(0);
/// Whether the walk at the seal ran and agreed with the allocator's subtraction.
static AGREED_AT_THE_SEAL: AtomicBool = AtomicBool::new(false);
/// The kernel pool's entry (K16.4).
static POOL: AtomicU8 = AtomicU8::new(super::NO_RESERVATION);
static COUNT: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static NAMED: SpinLock<[Option<Arc<Job>>; PAYERS]> = SpinLock::new([const { None }; PAYERS]);

/// Who pays: the task's ticket or its own job, never whoever was running (K16.4).
pub(super) fn charged() -> u32 {
    crate::sched::with_current(|task| match task.charge_target() {
        NO_TARGET => task.group(),
        target => target,
    })
    .unwrap_or(NONE)
}

/// The entry a request draws on: its job's, or, for a task with no job at all, the
/// pool's. A job with no promise has none, and is refused: the pool is the kernel's.
pub(super) fn route(owner: u32) -> Reservation {
    let entry = super::reservation_for(owner);
    if owner == NONE && SEALED.load(Ordering::Acquire) && !crate::irq::in_interrupt() {
        return POOL.load(Ordering::Acquire);
    }
    entry
}

/// Whether a request of the shared heap is to be refused, and counted.
pub(super) fn note_shared(bytes: usize) -> bool {
    if !SEALED.load(Ordering::Acquire) {
        return false;
    }
    let _ = COUNT.fetch_add(1, Ordering::Relaxed);
    let _ = BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    true
}

/// Say `job`'s frames in each report.
pub fn note_payer(job: &Arc<Job>) {
    if let Some(slot) = NAMED.lock().iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(Arc::clone(job));
    }
}

/// From here on a request of the shared heap is refused, and a task with no job
/// draws on `pool`. The first processes are made by now. The manager has said READY and devmgr has started, each drawing on its own promise or, before this, on the shared heap; `linuxd` has not run.
pub fn seal_after_boot(pool: Reservation) {
    POOL.store(pool, Ordering::Release);
    SEALED.store(true, Ordering::Release);
}

/// The pool's entry, for the check.
pub(super) fn pool() -> Reservation {
    POOL.load(Ordering::Acquire)
}

/// Say what the seal has refused, what the pool drew and what is outside every promise:
/// page tables, stacks and IOMMU tables, which this seal does not cover (F.8).
pub fn report() {
    let promised = drawn_everywhere();
    let used = super::managed_frames().saturating_sub(super::free_frames());
    let (size, drawn) = super::reservation_usage(POOL.load(Ordering::Acquire));
    crate::console::println!(
        "  retyped  {} requests, {} bytes refused; pool drew {drawn} of {size} frames; {} frames outside every promise",
        COUNT.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
        used.saturating_sub(promised),
    );
    for job in NAMED.lock().iter().flatten() {
        if let Some(tables) = job.usage(Resource::Tables) {
            crate::console::println!(
                "  retyped  job {}: {} table bytes",
                job.quota_index(),
                tables.used
            );
        }
    }
    // By payer after the seal only where a row asked (ledger 10892 P1): the walk holds the
    // allocator's lock, interrupts masked, for the whole array.
    let asked = WALKS_LEFT
        .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
            left.checked_sub(1)
        })
        .is_ok();
    if asked {
        let outside = walk_outside();
        say(&outside, "now");
        judge(&outside);
    }
}

/// How long the report sleeps, in nanoseconds: 10 s, or 1 s while a row's walks are asked
/// for, so that a short row reads them (ledger 10953 G3).
pub fn report_period() -> u64 {
    if WALKS_LEFT.load(Ordering::Acquire) == 0 {
        10_000_000_000
    } else {
        1_000_000_000
    }
}

// CHECKS-ONLY: checks.rs
/// Walk the frame records at the next `reports` reports too: a row's check-only boot
/// option, read by `checks::init` (`ferrix.check_walk_outside`, ledger 10892 P1).
pub fn walk_at_reports(reports: u32) {
    WALKS_LEFT.store(reports, Ordering::Release);
}

/// Whether the walk at the seal ran and agreed with the allocator's own count (L.object.349,
/// for the seal's check).
pub(crate) fn walked_and_agreed_at_the_seal() -> bool {
    AGREED_AT_THE_SEAL.load(Ordering::Acquire)
}

/// What a walk of the frame records found outside every promise (F.8, ledger 10639 F1-F3):
/// frames held and drawn from no reservation, by who they are charged to.
#[derive(Clone, Copy, Debug, Default)]
struct Outside {
    /// Every frame held and drawn from no reservation.
    frames: u64,
    /// Of them, charged to no job: the kernel's own, a block's tail, a frame of the root's.
    kernel: u64,
    /// Of them, the large area's frames kept on its chain (`large::KEPT_OWNER`).
    kept: u64,
    /// Of them, charged to a job: each payer's slot, its pages and its tables, the first
    /// [`PAYERS`] the walk meets; the rest in [`Outside::others`].
    payers: [(u32, u64, u64); PAYERS],
    /// Frames of payers past the [`PAYERS`] named.
    others: u64,
    /// Frames whose charge is of no class on the list (F3): a defect.
    unlisted: u64,
    /// What the allocator's counts say there is: frames in use less those drawn from every
    /// entry, read in the same hold as the walk (F1's witness).
    used_less_drawn: u64,
    /// How long the walk held the allocator's lock, interrupts masked, in nanoseconds.
    held_nanos: u64,
    /// Frames walked.
    walked: u64,
}

/// A frame of the walk, charged to `owner`, in its class.
fn count_owned(outside: &mut Outside, owner: u32) {
    let (slot, table) = if owner == super::large::KEPT_OWNER {
        outside.kept += 1;
        return;
    } else if owner & super::TABLE_OWNER != 0 {
        (owner & !super::TABLE_OWNER, true)
    } else {
        (owner, false)
    };
    // The root job has no quota slot, so its frames read as charged to nobody: the kernel's
    // (ledger 10639 F3), counted before this.
    if u64::from(slot) >= quota::SLOT_LIMIT {
        outside.unlisted += 1;
        return;
    }
    let at = outside
        .payers
        .iter()
        .position(|&(payer, pages, tables)| payer == slot || pages + tables == 0);
    let Some((payer, pages, tables)) = at.and_then(|at| outside.payers.get_mut(at)) else {
        outside.others += 1;
        return;
    };
    *payer = slot;
    if table {
        *tables += 1;
    } else {
        *pages += 1;
    }
}

/// Walk every frame record under the allocator's lock and count what is held and drawn
/// from no reservation, by class and payer, beside the allocator's own subtraction
/// (F1, F2): the seal's act, not a report's, since it holds interrupts masked for the
/// whole array.
fn walk_outside() -> Outside {
    let started = crate::timer::now_nanos();
    let mut outside = super::with_frames(|frames| {
        let mut outside = Outside::default();
        let (base, entries) = frames.span();
        // A tail takes its head's charge (10892 P3). A block's head reads as charged to
        // nobody (`Frames::owner` answers a single frame's alone), so a block, tails and
        // all, is the kernel's.
        let mut head = NONE;
        for frame in base..base.saturating_add(entries) {
            let held = match frames.state(frame) {
                Some(ferrix_frame::State::Allocated) => {
                    head = frames.owner(frame);
                    Some(head)
                }
                Some(ferrix_frame::State::AllocatedTail) => Some(head),
                _ => None,
            };
            let Some(owner) = held else {
                continue;
            };
            outside.walked += 1;
            if super::reservation::drawn_from(frame) != super::reservation::NONE {
                continue;
            }
            outside.frames += 1;
            if owner == NONE {
                outside.kernel += 1;
            } else {
                count_owned(&mut outside, owner);
            }
        }
        let used = frames.managed_frames().saturating_sub(frames.free_frames());
        outside.used_less_drawn = used.saturating_sub(drawn_everywhere());
        outside
    })
    .unwrap_or_default();
    outside.held_nanos = crate::timer::now_nanos().saturating_sub(started);
    outside
}

/// Frames drawn from every entry: the ledger's own count.
fn drawn_everywhere() -> u64 {
    (1..=super::reservation::MOST)
        .map(|entry| super::reservation_usage(entry as Reservation).1)
        .sum()
}

/// Say what a walk found outside every promise, by class and payer.
fn say(outside: &Outside, when: &str) {
    crate::console::println!(
        "  retyped  {when} {} frames outside every promise ({} of the kernel's, {} kept by the \
         large area, {} of payers past the named), {} walked in {} us, interrupts masked",
        outside.frames,
        outside.kernel,
        outside.kept,
        outside.others,
        outside.walked,
        outside.held_nanos / 1000,
    );
    for &(payer, pages, tables) in outside
        .payers
        .iter()
        .filter(|(_, pages, tables)| pages + tables != 0)
    {
        crate::console::println!(
            "  retyped  job {payer}: {pages} pages and {tables} tables outside its promise"
        );
    }
}

/// The walk at the seal (F1): the line by payer, judged.
pub fn count_at_the_seal() {
    let outside = walk_outside();
    say(&outside, "at the seal");
    judge(&outside);
    AGREED_AT_THE_SEAL.store(true, Ordering::Release);
}

/// Stop the boot by name when a walk and the allocator's subtraction disagree (FX-0981) or
/// a frame is charged to no class on the list (FX-0983).
fn judge(outside: &Outside) {
    if outside.frames != outside.used_less_drawn {
        crate::panic::fatal!(
            crate::panic::catalog::RETYPED_UNCOUNTED,
            "the frames outside every promise disagree: the walk found {} and the allocator's \
             used less drawn is {}",
            outside.frames,
            outside.used_less_drawn
        );
    }
    if outside.unlisted != 0 {
        crate::panic::fatal!(
            crate::panic::catalog::RETYPED_UNLISTED,
            "{} frames outside every promise are charged to no class on the list",
            outside.unlisted
        );
    }
}

/// The frames linuxd's arena needs at its start under `kernel_memory=retyped`
/// (`docs/microkernel/K16.md`, R.5): the root it unpacks from the initramfs of
/// `archive_bytes`, the frames a program's page never takes, and the programs'
/// own, [`ARENA_WORKING_FRAMES`].
pub const fn arena_needed(archive_bytes: u64) -> u64 {
    archive_bytes.div_ceil(ferrix_bootinfo::PAGE_SIZE)
        + ferrix_native_abi::lift::ARENA_KEPT_FRAMES
        + ARENA_WORKING_FRAMES
}

/// The arena the programs of a boot run in beyond the root and the floor, measured
/// (`docs/microkernel/K16.md` §4.1, R.5): the heaviest row, test-init under `fs=server` on
/// x86_64, peaks at 14824 frames, so 1560 frames (about 6 MiB) of room.
pub(crate) const ARENA_WORKING_FRAMES: u64 = 64 << 20 >> 12;

/// Whether an arena of `arena` frames is refused for a root of `archive_bytes`
/// (L.object.345).
pub const fn arena_refused(arena: u64, archive_bytes: u64) -> bool {
    arena < arena_needed(archive_bytes)
}
