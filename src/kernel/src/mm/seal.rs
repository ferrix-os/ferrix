//! K16's seal (`docs/microkernel/K16.md`, `max`'s deviation): after the first processes are
//! started the shared heap serves nobody, a job draws on its own promise, its nearest
//! ancestor's being its own (`Quota::draws_from`), a task of the kernel's with no job on a
//! named pool, and an interrupt on nothing. Only the global allocator: page tables, stacks,
//! IOMMU tables and `vmap` are the buddy's, counted here and not sealed.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use super::Reservation;
use crate::object::job::Job;
use crate::object::quota::{self, NO_TARGET, NONE, Resource};
use crate::sync::SpinLock;

/// The jobs a report says the frames of.
const PAYERS: usize = 8;
static SEALED: AtomicBool = AtomicBool::new(false);
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
    let mut promised = 0;
    for entry in 1..=super::reservation::MOST {
        promised += super::reservation_usage(entry as Reservation).1;
    }
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
}
