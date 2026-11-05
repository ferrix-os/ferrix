use super::task::Task;
use super::work;
use crate::arch;
use alloc::sync::Arc;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_sched::CpuSet;
use ferrix_sched::MAX_CPUS;
use ferrix_sync::IrqControl;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AffinityRefusal {
    KernelThread,
    Empty,
    #[cfg(ferrix_schedule = "edf")]
    Reserved,
}
pub fn effective_mask(task: &Task) -> CpuSet {
    task.affinity()
}
pub fn set_affinity(task: &Arc<Task>, asked: &CpuSet) -> Result<CpuSet, AffinityRefusal> {
    #[cfg(ferrix_schedule = "edf")]
    if task.reserved.load(core::sync::atomic::Ordering::Relaxed) {
        return Err(AffinityRefusal::Reserved);
    }
    if task.thread().is_none() {
        return Err(AffinityRefusal::KernelThread);
    }
    let domain = CpuSet::first(crate::smp::count()).map_err(|_| AffinityRefusal::Empty)?;
    let mask = asked.intersection(&domain);
    if mask.is_empty() {
        return Err(AffinityRefusal::Empty);
    }
    let handle = task.thread_handle.load(Ordering::Acquire);
    if handle != 0 {
        let own = super::with_current(|me| me.id == task.id).unwrap_or(false);
        tell_kernel(if own { 0 } else { handle }, &mask)?;
    }
    *task.mask.lock() = Some(mask);
    Ok(mask)
}
/// Tell the kernel, whose refusal is the call's: a program is never told it holds a mask the
/// kernel did not take (ledger 7132 ED.1).
fn tell_kernel(thread: u32, mask: &CpuSet) -> Result<(), AffinityRefusal> {
    let mut bytes = [0_u8; ferrix_sched::WORDS * 8];
    for (out, word) in bytes.chunks_exact_mut(8).zip(mask.words()) {
        out.copy_from_slice(&word.to_le_bytes());
    }
    ferrix_native::lift_calls::thread_affinity(ferrix_rt::Kernel, thread, &bytes).map_err(|why| {
        match why {
            #[cfg(ferrix_schedule = "edf")]
            ferrix_native::error::Error::AlreadyBound => AffinityRefusal::Reserved,
            _ => AffinityRefusal::Empty,
        }
    })
}
/// A new thread: its creator's mask, or what was asked before it existed.
pub(super) fn apply_own(task: &Task) {
    let start = task.thread().and_then(|thread| thread.start_affinity());
    let mut held = task.mask.lock();
    if held.is_none() {
        *held = start;
    }
    if let Some(mask) = *held {
        // A new thread's own start, no call's to answer: said, as it was.
        if let Err(why) = tell_kernel(0, &mask) {
            crate::println!("linuxd: a thread's affinity was refused: {why:?}");
        }
    }
}
