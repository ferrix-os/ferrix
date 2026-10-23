//! lode's allocator: size classes over mapped chunks, for a program with one
//! thread.
//!
//! A request up to [`LARGEST`] bytes is rounded up to a power of two from 16
//! and served from that class's free list, refilled by carving a mapped
//! [`CHUNK`]. A larger one is a mapping of its own, unmapped when freed.
//! Classes are powers of two and chunks page-aligned, so an object is aligned
//! to its class's size, and an alignment up to [`LARGEST`] comes out by
//! asking for at least that much. Freed small objects stay on their class's
//! list: lode's working set is a batch of records and a read buffer, and
//! settles.
//!
//! lode never starts a second thread, so the state is reached through an
//! `UnsafeCell` with no lock (`Sync` is asserted for that reason alone).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

use crate::sys;

/// The smallest class.
const SMALLEST: usize = 16;
/// The largest class; above it, a mapping each.
const LARGEST: usize = 2048;
/// The classes: 16, 32, ... 2048.
const CLASSES: usize = 8;
/// Bytes mapped at a time for the classes.
const CHUNK: usize = 256 * 1024;
/// A page.
const PAGE: usize = 4096;

/// The allocator's state.
struct State {
    /// Each class's free list: the first free object, which holds the next.
    free: [usize; CLASSES],
    /// The next byte of the current chunk not yet carved.
    next: usize,
    /// Its end.
    end: usize,
}

/// The global allocator.
pub(crate) struct Heap(UnsafeCell<State>);

// SAFETY: lode runs one thread; nothing reaches the state from two at once.
unsafe impl Sync for Heap {}

/// The one instance.
#[global_allocator]
static HEAP: Heap = Heap(UnsafeCell::new(State {
    free: [0; CLASSES],
    next: 0,
    end: 0,
}));

/// The class a size goes in, and the class's size.
fn class_of(size: usize) -> Option<(usize, usize)> {
    let rounded = size.max(SMALLEST).checked_next_power_of_two()?;
    (rounded <= LARGEST).then(|| (rounded.trailing_zeros() as usize - 4, rounded))
}

impl State {
    /// An object of class `class`, `size` bytes.
    fn small(&mut self, class: usize, size: usize) -> *mut u8 {
        let Some(head) = self.free.get_mut(class) else {
            return ptr::null_mut();
        };
        if *head != 0 {
            let object = *head as *mut usize;
            // SAFETY: a free object holds the address of the next one in its
            // first word, written by `release`; it is ours and aligned.
            *head = unsafe { object.read() };
            return object.cast();
        }
        let start = self.next.next_multiple_of(size);
        if start + size > self.end || self.next == 0 {
            let Some(chunk) = sys::map(CHUNK) else {
                return ptr::null_mut();
            };
            self.next = chunk as usize + size;
            self.end = chunk as usize + CHUNK;
            return chunk;
        }
        self.next = start + size;
        start as *mut u8
    }

    /// Put `object` back on class `class`'s list.
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "an object of a class is aligned to the class's size, at least 16"
    )]
    fn release(&mut self, class: usize, object: *mut u8) {
        let Some(head) = self.free.get_mut(class) else {
            return;
        };
        // SAFETY: `object` was handed out for this class, is no longer used,
        // and has room and alignment for a word.
        unsafe { object.cast::<usize>().write(*head) };
        *head = object as usize;
    }
}

// SAFETY: every pointer handed out is a live, unshared block of at least the
// layout's size and alignment, until it comes back through `dealloc`.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(layout.align());
        // SAFETY: one thread; no other reference to the state lives now.
        let state = unsafe { &mut *self.0.get() };
        match class_of(size) {
            Some((class, class_size)) => state.small(class, class_size),
            None if layout.align() <= PAGE => {
                sys::map(size.next_multiple_of(PAGE)).unwrap_or(ptr::null_mut())
            }
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, object: *mut u8, layout: Layout) {
        let size = layout.size().max(layout.align());
        // SAFETY: one thread; no other reference to the state lives now.
        let state = unsafe { &mut *self.0.get() };
        match class_of(size) {
            Some((class, _)) => state.release(class, object),
            None => sys::unmap(object, size.next_multiple_of(PAGE)),
        }
    }
}
