use crate::device::DeviceNode;
use crate::fallible;
use crate::user::memory_type;
use crate::user::memory_type::MemoryType;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::Framebuffer;
use ferrix_sync::Once;
pub fn may_lend() -> bool {
    false
}
