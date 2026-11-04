use crate::arch;
use crate::sched::WaitQueue;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU16;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_sync::IrqSpinLock;
#[derive(Debug)]
pub struct Checked {
    pub held: usize,
    pub dropped: u64,
}
pub fn waiters() -> &'static WaitQueue {
    static WAITERS: WaitQueue = WaitQueue::new();
    &WAITERS
}
pub fn interrupt_driven() -> bool {
    false
}
