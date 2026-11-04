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
}
pub fn effective_mask(task: &Task) -> CpuSet {
    task.affinity()
}
pub fn set_affinity(task: &Arc<Task>, asked: &CpuSet) -> Result<CpuSet, AffinityRefusal> {
    if task.thread().is_none() {
        return Err(AffinityRefusal::KernelThread);
    }
    let domain = CpuSet::first(crate::smp::count()).map_err(|_| AffinityRefusal::Empty)?;
    let mask = asked.intersection(&domain);
    if mask.is_empty() {
        return Err(AffinityRefusal::Empty);
    }
    *task.mask.lock() = Some(mask);
    let handle = task.thread_handle.load(Ordering::Acquire);
    if handle != 0 {
        let own = super::with_current(|me| me.id == task.id).unwrap_or(false);
        tell_kernel(if own { 0 } else { handle }, &mask);
    }
    Ok(mask)
}
fn tell_kernel(thread: u32, mask: &CpuSet) {
    let mut bytes = [0_u8; ferrix_sched::WORDS * 8];
    for (out, word) in bytes.chunks_exact_mut(8).zip(mask.words()) {
        out.copy_from_slice(&word.to_le_bytes());
    }
    if let Err(why) = ferrix_native::lift_calls::thread_affinity(ferrix_rt::Kernel, thread, &bytes)
    {
        crate::println!("linuxd: a thread's affinity was refused: {why:?}");
    }
}
/// A new thread: its creator's mask, or what was asked before it existed.
pub(super) fn apply_own(task: &Task) {
    let start = task.thread().and_then(|thread| thread.start_affinity());
    let mut held = task.mask.lock();
    if held.is_none() {
        *held = start;
    }
    if let Some(mask) = *held {
        tell_kernel(0, &mask);
    }
}
