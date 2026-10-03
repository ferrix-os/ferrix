//! Work queues: a thread each, running items in the order queued
//! (`docs/NVIDIA.md` §4.1: "the bottom half `rm_isr_bh` and the work items
//! `os_queue_work_item` queues").
//!
//! Linux's glue runs RM's work items on `nv_kthread_q`, a kernel thread, or
//! on a GPU's own queue. Here a queue is a thread of nvrm's. The global one
//! starts when first used; a GPU's queue is made by the kept C with
//! [`nvos_work_queue_create`] and handed to RM as its `struct
//! os_work_queue`, which RM never looks inside.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::libc;
use crate::log::say;
use crate::status::{self, NvBool, NvStatus};
use crate::sync::{Event, Mutex};
use crate::thread;

/// RM's alternate stack for an entry point, `nvidia_stack_t`: 3 pages on
/// x86-64. RM's build has altstacks off, so it is never switched to, but
/// each entry point is still handed one, as Linux's glue hands it.
pub(crate) const STACK_BYTES: usize = 3 * 4096;

unsafe extern "C" {
    /// RM's entry point for a work item `os_queue_work_item` queued.
    fn rm_execute_work_item(stack: *mut c_void, data: *mut c_void);
}

/// One queued item.
struct Item {
    /// The next item, or null.
    next: *mut Item,
    /// What to run.
    run: extern "C" fn(*mut c_void),
    /// Its argument.
    argument: *mut c_void,
}

/// A work queue.
#[derive(Debug)]
pub struct WorkQueue {
    /// Guards the list.
    lock: Mutex,
    /// The first and last items.
    list: UnsafeCell<(*mut Item, *mut Item)>,
    /// Items ever queued.
    queued: AtomicU64,
    /// Items ever run to their end.
    done: AtomicU64,
    /// Signalled when an item is queued, or the queue is to stop.
    work: Event,
    /// Signalled when an item finishes.
    finished: Event,
    /// 0 not started, 1 starting, 2 running, 3 stopped.
    state: AtomicU32,
    /// Asked to stop.
    stop: AtomicBool,
    /// Whether a flush for unload is under way (`os_is_queue_flush_ongoing`).
    unload_flush: AtomicBool,
}

// SAFETY: the list is only touched under `lock`; the rest are atomics.
unsafe impl Sync for WorkQueue {}

/// Not started.
const IDLE: u32 = 0;
/// Its thread is being started.
const STARTING: u32 = 1;
/// Its thread runs.
const RUNNING: u32 = 2;

impl WorkQueue {
    /// A queue whose thread has not started.
    pub const fn new() -> WorkQueue {
        WorkQueue {
            lock: Mutex::new(),
            list: UnsafeCell::new((ptr::null_mut(), ptr::null_mut())),
            queued: AtomicU64::new(0),
            done: AtomicU64::new(0),
            work: Event::new(),
            finished: Event::new(),
            state: AtomicU32::new(IDLE),
            stop: AtomicBool::new(false),
            unload_flush: AtomicBool::new(false),
        }
    }

    /// Start the thread if nobody has; whether it runs.
    fn start(&'static self) -> bool {
        match self
            .state
            .compare_exchange(IDLE, STARTING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                let started = thread::spawn(worker, ptr::from_ref(self).cast_mut().cast());
                self.state
                    .store(if started { RUNNING } else { IDLE }, Ordering::Release);
                started
            }
            Err(STARTING) => {
                while self.state.load(Ordering::Acquire) == STARTING {
                    core::hint::spin_loop();
                }
                self.state.load(Ordering::Acquire) == RUNNING
            }
            Err(state) => state == RUNNING,
        }
    }

    /// Queue `run(argument)`; whether it was queued.
    pub fn schedule(&'static self, run: extern "C" fn(*mut c_void), argument: *mut c_void) -> bool {
        if !self.start() {
            return false;
        }
        // SAFETY: a fresh block for one `Item`; malloc's alignment suffices.
        let item = unsafe { libc::malloc(size_of::<Item>()) }.cast::<Item>();
        if item.is_null() {
            return false;
        }
        // SAFETY: `item` is a fresh, aligned block for one `Item`.
        unsafe {
            item.write(Item {
                next: ptr::null_mut(),
                run,
                argument,
            });
        }
        self.lock.lock();
        // SAFETY: under the lock.
        let list = unsafe { &mut *self.list.get() };
        if list.1.is_null() {
            list.0 = item;
        } else {
            // SAFETY: the tail is a live item of this list, under the lock.
            unsafe { (*list.1).next = item };
        }
        list.1 = item;
        let _ = self.queued.fetch_add(1, Ordering::AcqRel);
        self.lock.unlock();
        self.work.signal();
        true
    }

    /// Take the first item, if any.
    fn pop(&self) -> *mut Item {
        self.lock.lock();
        // SAFETY: under the lock.
        let list = unsafe { &mut *self.list.get() };
        let item = list.0;
        if !item.is_null() {
            // SAFETY: a live item of this list, under the lock.
            list.0 = unsafe { (*item).next };
            if list.0.is_null() {
                list.1 = ptr::null_mut();
            }
        }
        self.lock.unlock();
        item
    }

    /// Wait until every item queued before this call has run.
    pub fn flush(&self) {
        let target = self.queued.load(Ordering::Acquire);
        loop {
            let seen = self.finished.now();
            if self.done.load(Ordering::Acquire) >= target {
                return;
            }
            self.finished.wait(seen, None);
        }
    }

    /// The thread's loop.
    fn serve(&self) {
        loop {
            let seen = self.work.now();
            let item = self.pop();
            if item.is_null() {
                if self.stop.load(Ordering::Acquire) {
                    return;
                }
                self.work.wait(seen, None);
                continue;
            }
            // SAFETY: popped, so this thread alone has it; allocated by
            // `schedule`.
            let Item { run, argument, .. } = unsafe { item.read() };
            // SAFETY: as above, and read just now.
            unsafe { libc::free(item.cast()) };
            run(argument);
            let _ = self.done.fetch_add(1, Ordering::AcqRel);
            self.finished.signal();
        }
    }
}

/// A queue's thread.
extern "C" fn worker(queue: *mut c_void) {
    // SAFETY: `start` passes a `&'static WorkQueue`.
    let queue = unsafe { &*queue.cast_const().cast::<WorkQueue>() };
    queue.serve();
}

/// The global queue: Linux's `nv_kthread_q`.
static GLOBAL: WorkQueue = WorkQueue::new();

/// The queue RM names: the global one for null.
///
/// # Safety
///
/// `queue` is null or one [`nvos_work_queue_create`] made, never destroyed.
unsafe fn queue_of(queue: *mut c_void) -> &'static WorkQueue {
    if queue.is_null() {
        return &GLOBAL;
    }
    // SAFETY: the caller vouches for it; queues are never freed while used.
    unsafe { &*queue.cast_const().cast::<WorkQueue>() }
}

/// Run one of RM's work items, as Linux's `os_execute_work_item` does.
extern "C" fn execute_rm_work_item(data: *mut c_void) {
    // SAFETY: a zeroed block for RM's stack argument.
    let stack = unsafe { libc::calloc(1, STACK_BYTES) };
    if stack.is_null() {
        say!("no memory for a work item's stack; the item is dropped");
        return;
    }
    // SAFETY: RM's own entry point, with the data it queued.
    unsafe { rm_execute_work_item(stack, data) };
    // SAFETY: allocated above, and RM keeps no pointer to it.
    unsafe { libc::free(stack) };
}

/// `os_queue_work_item`: run `rm_execute_work_item(data)` on `queue`, or
/// on the global queue for null.
///
/// # Safety
///
/// `queue` is null or a queue this layer made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_queue_work_item(queue: *mut c_void, data: *mut c_void) -> NvStatus {
    // SAFETY: the caller vouches for the queue.
    let queue = unsafe { queue_of(queue) };
    if queue.schedule(execute_rm_work_item, data) {
        status::OK
    } else {
        say!("os_queue_work_item: the queue's thread did not start");
        status::NOT_READY
    }
}

/// `os_flush_work_queue`.
///
/// # Safety
///
/// `queue` is null or a queue this layer made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_flush_work_queue(queue: *mut c_void, is_unload: NvBool) -> NvStatus {
    // SAFETY: the caller vouches for the queue.
    let queue = unsafe { queue_of(queue) };
    queue.unload_flush.store(is_unload != 0, Ordering::Release);
    queue.flush();
    queue.unload_flush.store(false, Ordering::Release);
    status::OK
}

/// `os_is_queue_flush_ongoing`.
///
/// # Safety
///
/// `queue` is null or a queue this layer made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_is_queue_flush_ongoing(queue: *mut c_void) -> NvBool {
    // SAFETY: the caller vouches for the queue.
    let queue = unsafe { queue_of(queue) };
    status::bool(queue.unload_flush.load(Ordering::Acquire))
}

/// Make a work queue, for a GPU's `nv_state_t.queue`; null without memory.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_work_queue_create() -> *mut c_void {
    // SAFETY: a fresh block for one queue; malloc's alignment suffices.
    let queue = unsafe { libc::malloc(size_of::<WorkQueue>()) }.cast::<WorkQueue>();
    if !queue.is_null() {
        // SAFETY: a fresh, aligned block for one queue.
        unsafe { queue.write(WorkQueue::new()) };
    }
    queue.cast()
}

/// Queue a C function on `queue`, or on the global queue for null.
///
/// # Safety
///
/// `queue` is null or a queue this layer made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_work_queue_schedule(
    queue: *mut c_void,
    run: extern "C" fn(*mut c_void),
    argument: *mut c_void,
) -> bool {
    // SAFETY: the caller vouches for the queue.
    let queue = unsafe { queue_of(queue) };
    queue.schedule(run, argument)
}

/// Wait for everything queued on `queue` so far to run.
///
/// # Safety
///
/// `queue` is null or a queue this layer made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_work_queue_flush(queue: *mut c_void) {
    // SAFETY: the caller vouches for the queue.
    let queue = unsafe { queue_of(queue) };
    queue.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static RAN: AtomicUsize = AtomicUsize::new(0);
    static ORDER: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

    extern "C" fn count(argument: *mut c_void) {
        let at = RAN.fetch_add(1, Ordering::SeqCst);
        if let Some(slot) = ORDER.get(at) {
            slot.store(argument as usize, Ordering::SeqCst);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    #[test]
    fn items_run_in_order_and_a_flush_waits_for_them() {
        static QUEUE: WorkQueue = WorkQueue::new();
        for n in 1..=4_usize {
            assert!(QUEUE.schedule(count, n as *mut c_void));
        }
        QUEUE.flush();
        assert_eq!(RAN.load(Ordering::SeqCst), 4);
        let order: Vec<usize> = ORDER.iter().map(|slot| slot.load(Ordering::SeqCst)).collect();
        assert_eq!(order, [1, 2, 3, 4]);
    }
}
