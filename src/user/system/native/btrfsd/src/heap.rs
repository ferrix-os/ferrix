//! btrfsd's allocator: size classes over mapped VMOs, for a program with one
//! thread (`docs/microkernel/BTRFSD.md` §4, BD.8). A request up to [`LARGEST`] is
//! rounded up to a power of two; a class's freed objects stay on its list and are
//! used again, so the memory a mount settles at is what it keeps. The whole is
//! bounded by [`MOST_BYTES`]: past it an allocation fails, and the file system
//! answers `ENOMEM` on the paths that ask fallibly; a `Vec` or `Box` that does not abort this process, which takes its partition's files until reboot (`restarts = 0`): BACKLOG, ledger 6933 BF.3.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

use ferrix_rt::Kernel;
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::vmo;

/// The smallest class.
const SMALLEST: usize = 16;
/// The largest class: above it nothing the btrfs crates ask for.
const LARGEST: usize = 16 * 1024 * 1024;
/// The classes: 16, 32, ... [`LARGEST`].
const CLASSES: usize = 21;
/// Bytes mapped at a time for the small classes.
const CHUNK: usize = 1024 * 1024;
/// The most bytes this program maps: its reservation.
const MOST_BYTES: usize = 512 * 1024 * 1024;

struct State {
    free: [usize; CLASSES],
    next: usize,
    end: usize,
    mapped: usize,
}

struct Heap(UnsafeCell<State>);

// SAFETY: btrfsd runs one thread; nothing reaches the state from two at once.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap(UnsafeCell::new(State {
    free: [0; CLASSES],
    next: 0,
    end: 0,
    mapped: 0,
}));

fn class_of(size: usize) -> Option<(usize, usize)> {
    let rounded = size.max(SMALLEST).checked_next_power_of_two()?;
    (rounded <= LARGEST).then(|| (rounded.trailing_zeros() as usize - 4, rounded))
}

impl State {
    fn map(&mut self, bytes: usize) -> usize {
        if self.mapped.saturating_add(bytes) > MOST_BYTES {
            return 0;
        }
        let Ok(made) = vmo::create(Kernel, bytes) else {
            return 0;
        };
        // Mapped for the process's life: the handle goes, the mapping stays.
        match made.map(None, bytes, Protection::ReadWrite, 0) {
            Ok(at) => {
                self.mapped += bytes;
                at
            }
            Err(_) => 0,
        }
    }

    fn take(&mut self, class: usize, size: usize) -> *mut u8 {
        let Some(head) = self.free.get_mut(class) else {
            return ptr::null_mut();
        };
        if *head != 0 {
            let object = *head as *mut usize;
            // SAFETY: a free object holds the next one's address in its first
            // word, written by `release`; it is ours and aligned.
            *head = unsafe { object.read() };
            return object.cast();
        }
        if size >= CHUNK / 8 {
            return self.map(size.next_multiple_of(4096)) as *mut u8;
        }
        let start = self.next.next_multiple_of(size);
        if self.next == 0 || start + size > self.end {
            let chunk = self.map(CHUNK);
            if chunk == 0 {
                return ptr::null_mut();
            }
            self.next = chunk + size;
            self.end = chunk + CHUNK;
            return chunk as *mut u8;
        }
        self.next = start + size;
        start as *mut u8
    }

    #[expect(
        clippy::cast_ptr_alignment,
        reason = "an object of a class is aligned to the class's size, at least a word's"
    )]
    fn release(&mut self, class: usize, object: *mut u8) {
        let Some(head) = self.free.get_mut(class) else {
            return;
        };
        // SAFETY: handed out for this class, no longer used, room for a word.
        unsafe { object.cast::<usize>().write(*head) };
        *head = object as usize;
    }
}

// SAFETY: a block handed out is live, unshared and of at least the layout's size
// and alignment until it comes back through `dealloc`: chunks and mappings are
// page-aligned and classes are powers of two.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(layout.align());
        // SAFETY: one thread; no other reference to the state lives now.
        let state = unsafe { &mut *self.0.get() };
        match class_of(size) {
            Some((class, class_size)) => state.take(class, class_size),
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, object: *mut u8, layout: Layout) {
        let size = layout.size().max(layout.align());
        // SAFETY: one thread; no other reference to the state lives now.
        let state = unsafe { &mut *self.0.get() };
        if let Some((class, _)) = class_of(size) {
            state.release(class, object);
        }
    }
}
