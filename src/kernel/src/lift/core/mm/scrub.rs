use crate::sync::SpinLock;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_frame::Frame;
pub fn render(out: &mut impl core::fmt::Write) -> core::fmt::Result {
    Ok(())
}
