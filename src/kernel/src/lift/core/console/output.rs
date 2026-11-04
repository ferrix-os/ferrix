use crate::arch;
use crate::sched::WaitQueue;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
#[derive(Debug)]
pub struct Checked {
    pub line: usize,
    pub by_interrupt: u64,
    pub by_writer: u64,
}
