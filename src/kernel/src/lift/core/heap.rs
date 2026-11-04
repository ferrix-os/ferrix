//! linuxd's heap (docs/microkernel/M5-LIFT.md §3). A page or more comes from
//! the arena's frame allocator as one block, aligned to its own size, so a task's
//! aligned stack is an ordinary allocation; less is carved from chunks of it.
//! There is one pool: memory the heap gave back is a program's again, and the
//! load's reclaim and OOM answer when it runs out, not a fault in normal mode, which
//! would end the partition. What runs before the arena exists has a mapping of
//! its own, and what it gets from there stays.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_frame::MAX_ORDER;
use ferrix_native::vmo;
use ferrix_rt::Kernel;

const PAGE: usize = 4096;
/// What is mapped for the time before the arena. Untouched pages cost a table
/// entry at most; the frame table is the largest thing in it.
const EARLY_BYTES: usize = 64 << 20;
const CHUNK_ORDER: u8 = 4;
const CHUNK_BYTES: usize = PAGE << CHUNK_ORDER;

/// A block's alignment is the lowest set bit of its size.
const SIZES: [usize; 16] = [
    16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072,
];

struct Lists {
    free: [usize; SIZES.len()],
    /// The chunk being carved.
    next: usize,
    end: usize,
    /// The mapping before the arena.
    early_next: usize,
    early_end: usize,
}

pub struct Heap {
    lock: AtomicBool,
    frames_ready: AtomicBool,
    lists: core::cell::UnsafeCell<Lists>,
}

// SAFETY: `lists` is reached only under `lock`.
unsafe impl Sync for Heap {}

fn small_class(layout: &Layout) -> Option<usize> {
    SIZES.iter().position(|&size| {
        size >= layout.size() && (1_usize << size.trailing_zeros()) >= layout.align()
    })
}

fn pages_order(layout: &Layout) -> u32 {
    let bytes = layout.size().max(layout.align()).max(PAGE);
    usize::BITS - (bytes.div_ceil(PAGE) - 1).leading_zeros()
}

impl Lists {
    fn early(&mut self, size: usize, align: usize) -> usize {
        let at = (self.early_next + align - 1) & !(align - 1);
        if at + size > self.early_end {
            return 0;
        }
        self.early_next = at + size;
        at
    }
    fn chunk(&mut self, frames_ready: bool) -> Option<(usize, usize)> {
        let at = if frames_ready {
            crate::mm::direct_map(crate::mm::allocate_frames(CHUNK_ORDER)? * PAGE as u64) as usize
        } else {
            match self.early(CHUNK_BYTES, CHUNK_BYTES) {
                0 => return None,
                at => at,
            }
        };
        Some((at, at + CHUNK_BYTES))
    }
}

impl Heap {
    pub const fn new() -> Heap {
        Heap {
            lock: AtomicBool::new(false),
            frames_ready: AtomicBool::new(false),
            lists: core::cell::UnsafeCell::new(Lists {
                free: [0; SIZES.len()],
                next: 0,
                end: 0,
                early_next: 0,
                early_end: 0,
            }),
        }
    }

    /// Map the early memory: before the first allocation.
    pub fn init(&self) -> Result<(), ferrix_native::error::Error> {
        let memory = vmo::create(Kernel, EARLY_BYTES).map_err(|why| {
            crate::println!("linuxd: the heap's VMO: {why:?}");
            why
        })?;
        let at = memory
            .map(
                None,
                EARLY_BYTES,
                ferrix_native::pending::Protection::ReadWrite,
                0,
            )
            .map_err(|why| {
                crate::println!("linuxd: the heap's mapping: {why:?}");
                why
            })?;
        core::mem::forget(memory);
        self.lock();
        // SAFETY: under the lock.
        let lists = unsafe { &mut *self.lists.get() };
        lists.early_next = at;
        lists.early_end = at + EARLY_BYTES;
        self.unlock();
        Ok(())
    }

    /// The arena's allocator stands: everything from here on draws on it.
    pub fn frames_ready(&self) {
        self.frames_ready.store(true, Ordering::Release);
    }

    fn lock(&self) {
        let mut spins = 0_u32;
        while self.lock.swap(true, Ordering::Acquire) {
            spins = spins.wrapping_add(1);
            if spins % 256 == 0 {
                crate::sys::thread_yield();
            }
            core::hint::spin_loop();
        }
    }
    fn unlock(&self) {
        self.lock.store(false, Ordering::Release);
    }

    fn pages(&self, layout: &Layout) -> usize {
        let order = pages_order(layout);
        if !self.frames_ready.load(Ordering::Acquire) {
            let size = PAGE << order;
            self.lock();
            // SAFETY: under the lock.
            let at = unsafe { &mut *self.lists.get() }.early(size, size);
            self.unlock();
            return at;
        }
        let frame = if order <= u32::from(MAX_ORDER) {
            crate::mm::allocate_frames(order as u8)
        } else {
            crate::mm::allocate_frame_run(1_u64 << (order - u32::from(MAX_ORDER)))
        };
        frame.map_or(0, |frame| {
            crate::mm::direct_map(frame * PAGE as u64) as usize
        })
    }
}

// SAFETY: blocks are carved from memory nothing else names, each aligned as the
// layout asks and at least its size, and handed out once.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(class) = small_class(&layout) else {
            return self.pages(&layout) as *mut u8;
        };
        let size = SIZES[class];
        let align = 1_usize << size.trailing_zeros();
        let ready = self.frames_ready.load(Ordering::Acquire);
        self.lock();
        // SAFETY: under the lock.
        let lists = unsafe { &mut *self.lists.get() };
        let block = lists.free[class];
        let answer = if block != 0 {
            // SAFETY: a free block holds the next free block's address.
            lists.free[class] = unsafe { *(block as *const usize) };
            block
        } else {
            let at = (lists.next + align - 1) & !(align - 1);
            if lists.next != 0 && at + size <= lists.end {
                lists.next = at + size;
                at
            } else if let Some((start, end)) = lists.chunk(ready) {
                lists.next = start + size;
                lists.end = end;
                start
            } else {
                0
            }
        };
        self.unlock();
        answer as *mut u8
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        let Some(class) = small_class(&layout) else {
            let Some(frame) = crate::mm::frame_of(block as usize) else {
                return; // From the early mapping; it stays.
            };
            let order = pages_order(&layout);
            if order <= u32::from(MAX_ORDER) {
                crate::mm::deallocate_frames(frame, order as u8);
            } else {
                for run in 0..1_u64 << (order - u32::from(MAX_ORDER)) {
                    crate::mm::deallocate_frames(frame + (run << MAX_ORDER), MAX_ORDER);
                }
            }
            return;
        };
        self.lock();
        // SAFETY: under the lock; the block is the caller's to give back.
        unsafe {
            let lists = &mut *self.lists.get();
            *(block as *mut usize) = lists.free[class];
            lists.free[class] = block as usize;
        }
        self.unlock();
    }
}
