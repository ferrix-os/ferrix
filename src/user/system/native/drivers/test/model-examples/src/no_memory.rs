//! The examples' allocator: they run nowhere, so every allocation answers
//! that there is no memory. Native programs have no heap; the page's
//! examples use `vec!` and `format!`, which need one to type-check.

use core::alloc::{GlobalAlloc, Layout};

/// An allocator with nothing to give.
pub(crate) struct NoMemory;

// SAFETY: an allocator that never hands out memory upholds `GlobalAlloc`'s
// contract trivially: `alloc` answers null, which callers treat as failure,
// and `dealloc` is never given a pointer `alloc` returned.
unsafe impl GlobalAlloc for NoMemory {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _pointer: *mut u8, _layout: Layout) {}
}

/// The examples' allocator.
#[global_allocator]
static ALLOCATOR: NoMemory = NoMemory;
