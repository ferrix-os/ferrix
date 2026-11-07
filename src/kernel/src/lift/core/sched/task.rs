//! A linuxd thread's record: one kernel thread, as every Linux thread is one
//! (docs/microkernel/M5-LIFT.md §4.1).

use alloc::sync::Arc;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ferrix_sched_class::CpuSet;

use crate::object::process::Host;
use crate::sync::SpinLock;

pub trait UserThread: Any + Send + Sync + fmt::Debug {
    fn process(&self) -> &dyn Host;
    fn slice_request(&self) -> u64 {
        0
    }
    fn start_affinity(&self) -> Option<CpuSet> {
        None
    }
    fn is_bare(&self) -> bool {
        false
    }
}

pub type TaskId = u64;

pub const RUNNABLE: u8 = 0;
pub const BLOCKED: u8 = 1;
pub const DEAD: u8 = 2;

/// What a waiter parks on: 0 armed, 1 woken. One word per task, so a wake that
/// finds nobody costs no system call and a wait costs no kernel object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepSlot;
impl SleepSlot {
    pub fn new() -> Result<SleepSlot, crate::fallible::AllocError> {
        Ok(SleepSlot)
    }
}

/// Wakes a thread holds back under its locks; one past these goes at once.
pub(crate) const WAKES_HELD: usize = 4;

// SAFETY: `locks_held`, `wakes_held` and `wakes_later` are touched only by the thread
// the task is, through `sched::with_current`; everything else in a task is atomic or locked.
unsafe impl Sync for Task {}

#[derive(Debug)]
pub struct Task {
    pub id: TaskId,
    pub name: &'static str,
    pub(crate) park: AtomicU32,
    pub(crate) work: AtomicU32,
    pub(crate) blocked: AtomicBool,
    pub(crate) dead: AtomicBool,
    pub(crate) in_restricted: AtomicBool,
    /// This thread's own: the locks it holds and the wakes they hold back
    /// (`sched::lock_taken`). Only the thread itself reaches these.
    pub(crate) pending_up: AtomicBool,
    /// Whether it holds a reservation.
    pub(crate) reserved: AtomicBool,
    /// In a sigreturn, whose capture wants only the processor's mask.
    pub(crate) sigreturning: AtomicBool,
    /// Whether it is inside `user::cache::reclaim`, as the kernel's task keeps
    /// it (`object::quota::KernelHeap`, FX-0973).
    pub(crate) reclaiming: AtomicBool,
    pub(crate) locks_held: core::cell::Cell<u32>,
    /// What [`Task::core_kept`] keeps.
    pub(crate) core_kept: core::sync::atomic::AtomicPtr<crate::object::process::Process>,
    pub(crate) wakes_held: core::cell::Cell<u32>,
    pub(crate) wakes_later: core::cell::UnsafeCell<[Option<Arc<Task>>; WAKES_HELD]>,
    /// The kernel thread's handle in the shared table, 0 before it starts.
    pub(crate) thread_handle: AtomicU32,
    pub(crate) group: AtomicU32,
    pub(crate) charge: AtomicU32,
    pub(crate) weight: AtomicU32,
    pub(crate) request: AtomicU64,
    pub(crate) voluntary: AtomicU64,
    pub(crate) wakes: AtomicU64,
    pub(crate) thread: Option<Arc<dyn UserThread>>,
    /// What [`Task::thread`] was found to be by its own downcast, once it has been: the
    /// personality's typed pointer to it, which that `Arc` keeps alive (ledger 6747 (4c)).
    pub(crate) thread_kept: core::sync::atomic::AtomicPtr<()>,
    pub(crate) entry: fn(usize),
    pub(crate) argument: AtomicUsize,
    pub(crate) gone: crate::sched::wait::WaitQueue,
    pub(crate) user_state: SpinLock<Option<crate::arch::UserState>>,
    pub(crate) pending: SpinLock<Option<ferrix_native::region::Context>>,
    pub(crate) time: crate::sched::account::Time,
    /// The call it is in, for `/proc`: its number plus one (0 for none), its arguments,
    /// and the program's stack and instruction pointers once it has parked in it.
    pub(crate) call: [AtomicU64; 9],
    /// The handle of the kernel process it runs in: 0 for none.
    pub(crate) bound_process: AtomicU64,
    /// The processors it may run on; `None`, every one.
    pub(crate) mask: SpinLock<Option<CpuSet>>,
}

impl Drop for Task {
    fn drop(&mut self) {
        crate::object::quota::release_group(self.group.load(Ordering::Acquire));
    }
}

/// A thread's policy as linuxd's model sees it: `SCHED_OTHER` always, since no
/// native call gives a thread of linuxd's a real-time policy
/// (docs/SCHED-CLASSES.md §9e; the kernel's `RtWord`).
#[derive(Clone, Copy, Debug)]
pub struct RtWord;

impl RtWord {
    /// No real-time policy.
    #[must_use]
    pub const fn params(self) -> Option<ferrix_sched_class::RtParams> {
        None
    }
    /// No reset-on-fork.
    #[must_use]
    pub const fn resets_on_fork(self) -> bool {
        false
    }
}

impl Task {
    /// Its policy word: `SCHED_OTHER`.
    #[must_use]
    pub const fn rt_word(&self) -> RtWord {
        RtWord
    }

    /// `task`'s wake, kept for when this thread's last lock goes. `false`: none is
    /// held, or there is no room, and the caller wakes it now.
    pub(crate) fn wake_later(&self, task: &Arc<Task>) -> bool {
        let at = self.wakes_held.get() as usize;
        if self.locks_held.get() == 0 || at >= WAKES_HELD {
            return false;
        }
        // SAFETY: only this thread reaches its own slots (the `Sync` above).
        unsafe { (*self.wakes_later.get())[at] = Some(Arc::clone(task)) };
        self.wakes_held.set(at as u32 + 1);
        true
    }
    /// The wakes held back. No lock of this thread's is held now.
    pub(crate) fn wakes_now(&self) {
        let held = self.wakes_held.replace(0) as usize;
        for at in 0..held {
            // SAFETY: as `wake_later`.
            if let Some(task) = unsafe { (*self.wakes_later.get())[at].take() } {
                crate::sys::unpark(&task.park);
            }
        }
    }
    pub fn state(&self) -> u8 {
        if self.dead.load(Ordering::Acquire) {
            DEAD
        } else if self.blocked.load(Ordering::Acquire) {
            BLOCKED
        } else {
            RUNNABLE
        }
    }
    pub fn affinity(&self) -> CpuSet {
        self.mask
            .lock()
            .unwrap_or_else(|| CpuSet::first(crate::smp::count()).unwrap_or_default())
    }
    pub fn cpu(&self) -> usize {
        crate::smp::this_cpu().map_or(0, |cpu| cpu.logical)
    }
    pub fn request_ns(&self) -> u64 {
        self.request.load(Ordering::Relaxed)
    }
    pub fn runtime(&self) -> u64 {
        self.time.runtime()
    }
    pub fn kernel_time(&self) -> u64 {
        self.time.kernel_time()
    }
    pub fn voluntary_switches(&self) -> u64 {
        self.voluntary.load(Ordering::Relaxed)
    }
    pub fn involuntary_switches(&self) -> u64 {
        0
    }
    pub fn switches(&self) -> u64 {
        self.voluntary_switches()
    }
    /// The quota group its time is charged to now.
    #[inline(always)]
    pub fn quota_group_now(&self) -> u32 {
        self.group.load(Ordering::Relaxed)
    }
    pub fn thread(&self) -> Option<&Arc<dyn UserThread>> {
        self.thread.as_ref()
    }
    /// [`Task::thread`] as the personality's own type found it: null before the first ask.
    #[inline(always)]
    pub fn thread_kept(&self) -> *const () {
        self.thread_kept.load(Ordering::Acquire)
    }
    /// Keep what the personality's downcast of [`Task::thread`] found.
    pub fn keep_thread(&self, thread: *const ()) {
        self.thread_kept.store(thread.cast_mut(), Ordering::Release);
    }
    /// [`Task::core_process`], kept after the first ask: a thread's process does not change,
    /// and this is asked on every call, through two `dyn` calls otherwise.
    #[inline(always)]
    pub fn core_kept(&self) -> Option<&crate::object::process::Process> {
        let kept = self.core_kept.load(Ordering::Relaxed);
        if kept.is_null() {
            let core = self.core_process()?;
            self.core_kept
                .store(core::ptr::from_ref(core).cast_mut(), Ordering::Relaxed);
            return Some(core);
        }
        // SAFETY: the thread this task holds owns the process, as long as the task.
        Some(unsafe { &*kept })
    }
    pub fn core_process(&self) -> Option<&crate::object::process::Process> {
        self.thread.as_ref().map(|thread| thread.process().core())
    }
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }
    pub fn is_blocked(&self) -> bool {
        self.blocked.load(Ordering::Acquire)
    }
    /// Move to slot `index`, as the hybrid's `Task::set_group` does; there is
    /// no run queue here to count it in. The running task on itself.
    pub(crate) fn set_group(&self, index: u32) {
        crate::object::quota::hold_group(index);
        let old = self.group.swap(index, Ordering::AcqRel);
        crate::object::quota::release_group(old);
    }
    #[inline(always)]
    pub(crate) fn note_call(&self, number: u64, args: &[u64; 6]) {
        self.call[0].store(number + 1, Ordering::Relaxed);
        for (slot, arg) in self.call[1..7].iter().zip(args) {
            slot.store(*arg, Ordering::Relaxed);
        }
        self.call[8].store(0, Ordering::Relaxed);
    }
    #[inline(always)]
    pub(crate) fn note_no_call(&self) {
        self.call[0].store(0, Ordering::Relaxed);
    }
    /// About to park: where the program stands, once a call.
    pub(crate) fn note_parking(&self) {
        if self.call[0].load(Ordering::Relaxed) != 0
            && self.call[8].load(Ordering::Relaxed) == 0
            && let Ok(context) = ferrix_native::region::context_read(ferrix_rt::Kernel)
        {
            #[cfg(target_arch = "x86_64")]
            let (stack, pc) = (context.rsp, context.rip);
            #[cfg(target_arch = "aarch64")]
            let (stack, pc) = (context.sp, context.pc);
            self.call[7].store(stack, Ordering::Relaxed);
            self.call[8].store(pc, Ordering::Relaxed);
        }
    }
    pub(crate) fn call_seen(&self) -> Option<(u64, [u64; 6], u64, u64)> {
        let number = self.call[0].load(Ordering::Relaxed).checked_sub(1)?;
        let mut args = [0; 6];
        for (arg, slot) in args.iter_mut().zip(&self.call[1..7]) {
            *arg = slot.load(Ordering::Relaxed);
        }
        Some((
            number,
            args,
            self.call[7].load(Ordering::Relaxed),
            self.call[8].load(Ordering::Relaxed),
        ))
    }
    /// Whether it adopted a ticket: never in `linuxd`, whose threads make no native
    /// adoption of their own (K7's calls are the kernel's, by number).
    pub fn has_adoption(&self) -> bool {
        false
    }

    /// The ticket it adopted: none, as [`Task::has_adoption`].
    pub fn adopted_ticket(&self) -> Option<Arc<crate::object::ticket::Ticket>> {
        None
    }

    /// Nothing to replace: what is handed in is dropped by the caller's hold ending.
    pub fn replace_adoption(
        &self,
        _ticket: Option<Arc<crate::object::ticket::Ticket>>,
    ) -> Option<Arc<crate::object::ticket::Ticket>> {
        None
    }

    /// Nothing to take.
    pub fn take_adoption(&self) -> Option<Arc<crate::object::ticket::Ticket>> {
        None
    }

    pub fn charge_target(&self) -> u32 {
        self.charge.load(Ordering::Relaxed)
    }
    /// Mark it inside a reclaim, or not: what the mark was before.
    pub(crate) fn swap_reclaiming(&self, now: bool) -> bool {
        self.reclaiming.swap(now, Ordering::Relaxed)
    }
    /// Whether it is inside a reclaim.
    pub(crate) fn reclaiming(&self) -> bool {
        self.reclaiming.load(Ordering::Relaxed)
    }
    pub fn set_charge_target(&self, slot: u32) {
        self.charge.store(slot, Ordering::Relaxed);
    }
    pub fn fill_reply(&self, len: usize, words: [u64; 3]) {}
    pub fn address_space(&self) -> Option<&Arc<crate::user::space::AddressSpace>> {
        self.core_process().map(|process| process.own_space())
    }
    pub fn record_copy_poison(&self, page: u64, at: Option<u64>) -> bool {
        false
    }
    /// The work word: the bits `sched::work` posts.
    pub fn work(&self) -> &AtomicU32 {
        &self.work
    }
}
