//! The per-thread work word (docs/microkernel/M5-LIFT.md §4.3): a post sets the
//! bit, wakes a parked task and kicks one in restricted mode.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use super::Task;

pub const END: u32 = 1 << 0;
pub const STOP: u32 = 1 << 1;
pub const SIGNAL: u32 = 1 << 2;

pub fn notify(task: &Arc<Task>, bits: u32) {
    post(task, bits);
    super::wake(task);
}
pub fn post(task: &Task, bits: u32) {
    let _ = task.work.fetch_or(bits, Ordering::Release);
    core::sync::atomic::fence(Ordering::SeqCst);
    // Its own post is on its way back already, and a kick would only bounce it.
    let own = super::with_current(|me| me.id == task.id).unwrap_or(false);
    if !own && task.in_restricted.load(Ordering::Acquire) {
        crate::sys::kick(task.thread_handle.load(Ordering::Acquire));
    }
}
pub fn post_own(bits: u32) {
    let _ = super::with_current(|task| post(task, bits));
}
pub fn own_end() -> bool {
    super::with_current(has_end).unwrap_or(false)
}
pub fn has_end(task: &Task) -> bool {
    task.work.load(Ordering::Acquire) & END != 0
}
pub fn place_before_entry() {}
#[must_use = "a posting counts until it is dropped, after the post"]
#[derive(Debug)]
pub struct Posting(bool);
pub fn posting() -> Posting {
    Posting(false)
}
pub fn arm(check: usize, target: &Task) {}
pub fn disarm() {}
pub fn own_word() -> u32 {
    super::with_current(|task| task.work.load(Ordering::Acquire)).unwrap_or(0)
}
pub fn wake_posted(task: &Arc<Task>) {
    super::wake(task);
}
