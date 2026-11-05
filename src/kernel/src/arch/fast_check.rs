//! What `syscall::region_check` reads and sets for `restricted-check`'s cases of K1's fast
//! path (ledger 7216, 7249 N.3): checks' items, in no path a program's call reaches.

use core::sync::atomic::Ordering;

use crate::sched;

// CHECKS-ONLY: syscall/region_check.rs
/// `task`'s three counts (`RestrictedState::fast`): it asks about the
/// thread that asked, which waits for the answer. Zero for a thread that never went in.
pub fn restricted_fast_counts(task: &sched::Task) -> [u64; 3] {
    // SAFETY: (SHARED) the thread that holds it waits for this check's answer, so it is not
    // on a way that writes them; a read of three whole words.
    sched::fast::state_of(task).map_or([0; 3], |state| state.fast)
}

// CHECKS-ONLY: syscall/region_check.rs
/// Give `task`'s own record the two bases, for the cases with normal mode's non-zero (ledger
/// 6464 Z4). The one writer of normal mode's bases a user kernel holds (K1 §7.1 (4)), and a
/// check's: no kernel call reaches it, and the record goes to the processor at the thread's
/// next switch, so the case blocks before it reads them. The thread waits for the answer,
/// not in the gap: `clean`'s and the mark's rule is not moved under it.
pub fn restricted_set_normal_bases_for_check(task: &sched::Task, fs: Option<u64>, gs: Option<u64>) {
    let Some(record) = sched::fast::own_record_for_check(task) else {
        return;
    };
    // SAFETY: (SHARED) as `restricted_fast_counts`: its thread waits, and only this check
    // writes it.
    let record = unsafe { &mut *record };
    if let Some(fs) = fs {
        record.set_thread_pointer(fs);
    }
    if let Some(gs) = gs {
        record.set_gs_base(gs);
    }
}

// CHECKS-ONLY: syscall/region_check.rs
/// Refills at restricted exits, and whether this processor's root write empties the return
/// predictor instead (ledger 6747 (1d)).
pub fn exit_refills() -> (u64, bool) {
    (
        super::x86_64::speculation::EXIT_REFILLS.load(Ordering::Relaxed),
        super::x86_64::speculation::ERAPS_EMPTIES.load(Ordering::Relaxed),
    )
}
