//! The scheduler as linuxd sees it: per-thread state in the shared region and a
//! kernel thread for each task (docs/microkernel/M5-LIFT.md §3, §4). The kernel
//! schedules; nothing here decides what runs.

pub mod account;
pub mod affinity;
/// The running task, lent.
pub mod borrow {
    pub use super::with_current;
}
pub mod inspect;
pub mod preempt;
pub mod task;
pub mod trip;
pub mod vtime;
pub mod wait;
pub mod work;
/// The IPC fast path's direct switch is the kernel's; linuxd takes the ordinary path.
pub mod direct {
    use super::Task;
    use alloc::sync::Arc;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(usize)]
    pub enum Count {
        Trip,
        Park,
        T2,
        T3,
        T4,
        Halves,
        T6,
        T7,
        T8,
        T9,
        T10,
        Queue,
        Fold,
        Separate,
        FoldUnplanned,
    }
    pub fn count(what: Count) {}
    pub struct Switch;
    impl Switch {
        pub fn hand_over(
            &mut self,
            caller: &Task,
            reader: Arc<Task>,
            park: impl FnOnce(Arc<Task>),
        ) {
        }
        pub fn switch(self) {}
    }
    pub fn begin(caller: &Task, reader: &Arc<Task>) -> Result<Switch, Count> {
        Err(Count::Separate)
    }
    pub fn set_running_blocked(task: &Arc<Task>) {}
    pub fn set_blocked(task: &Task) {}
    pub fn block_parked(task: &Task, last_look: super::LastLook) -> bool {
        false
    }
}

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub use affinity::{AffinityRefusal, effective_mask, set_affinity};
use ferrix_sched::CpuSet;
pub use preempt::Preempt;
pub use preempt::counting as records_kept;
pub use task::{SleepSlot, Task, TaskId, UserThread};
pub use wait::{Mark, ReadyList, WaitQueue};

use crate::arch;
use crate::fallible;

pub const SLICE_NS: u64 = 3_000_000;
pub const BASE_SLICE_NS: u64 = SLICE_NS;

/// A linuxd stack: this big and this aligned, with its task's pointer in its
/// top word, so `current` needs no thread-local register and no agreement with
/// K1 on the FS base (M5-LIFT.md §3). At the top, not the base: an overflow runs
/// into the unmapped guard or the heap, not through the task (5714's C1).
pub const STACK_BYTES: usize = 256 * 1024;

#[inline(always)]
fn task_slot(sp: usize) -> *mut *const Task {
    ((sp & !(STACK_BYTES - 1)) + STACK_BYTES - 16) as *mut *const Task
}

#[inline(always)]
fn sp_now() -> usize {
    let at = 0_u8;
    core::ptr::from_ref(&at) as usize
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static MADE: AtomicU64 = AtomicU64::new(0);

/// Allocate a stack with `task`'s pointer in it; answers the top to start from.
fn stack_for(task: &Arc<Task>) -> Result<usize, &'static str> {
    let layout = Layout::from_size_align(STACK_BYTES, STACK_BYTES).map_err(|_| "stack layout")?;
    // SAFETY: the layout is not zero sized. The stack lives until `exit_leaving`,
    // whose thread is on it and leaks it for now.
    let base = unsafe { alloc::alloc::alloc(layout) };
    if base.is_null() {
        return Err("no memory for a stack");
    }
    let top = base as usize + STACK_BYTES - 16;
    // SAFETY: inside the allocation just made, aligned to 16.
    unsafe { *(top as *mut *const Task) = Arc::into_raw(Arc::clone(task)) };
    Ok(crate::arch::entry_stack(top))
}

fn new_task(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    thread: Option<Arc<dyn UserThread>>,
    state: Option<arch::UserState>,
) -> Result<Arc<Task>, &'static str> {
    let _ = MADE.fetch_add(1, Ordering::Relaxed);
    fallible::try_arc(Task {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        name,
        park: AtomicU32::new(0),
        work: AtomicU32::new(0),
        blocked: core::sync::atomic::AtomicBool::new(false),
        dead: core::sync::atomic::AtomicBool::new(false),
        in_restricted: core::sync::atomic::AtomicBool::new(false),
        thread_kept: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
        pending_up: core::sync::atomic::AtomicBool::new(false),
        #[cfg(ferrix_schedule = "edf")]
        reserved: core::sync::atomic::AtomicBool::new(false),
        sigreturning: core::sync::atomic::AtomicBool::new(false),
        locks_held: core::cell::Cell::new(0),
        core_kept: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
        wakes_held: core::cell::Cell::new(0),
        wakes_later: core::cell::UnsafeCell::new([const { None }; task::WAKES_HELD]),
        thread_handle: AtomicU32::new(0),
        group: AtomicU32::new(crate::object::quota::NONE),
        charge: AtomicU32::new(0),
        weight: AtomicU32::new(0),
        request: AtomicU64::new(thread.as_ref().map_or(0, |thread| thread.slice_request())),
        voluntary: AtomicU64::new(0),
        wakes: AtomicU64::new(0),
        thread,
        entry,
        argument: core::sync::atomic::AtomicUsize::new(argument),
        gone: WaitQueue::new(),
        time: account::Time::new(),
        mask: crate::sync::SpinLock::new(None),
        call: [const { AtomicU64::new(0) }; 9],
        bound_process: AtomicU64::new(0),
        user_state: crate::sync::SpinLock::new(state),
        pending: crate::sync::SpinLock::new(None),
    })
    .map_err(|_| "no memory for a task")
}

/// A new thread's first Rust frame, on its own stack.
extern "C" fn thread_main(_argument: usize) -> ! {
    if let Some(task) = current() {
        task.thread_handle
            .store(crate::sys::thread_self(), Ordering::Release);
        // Set before the kernel thread was there to take it.
        affinity::apply_own(&task);
        let entry = task.entry;
        let argument = task.argument.load(Ordering::Relaxed);
        drop(task);
        entry(argument);
    }
    exit_leaving()
}

/// What the load holds in a new process's slot (the kernel's hello to pid 1), to the
/// kernel's slot, where the program's `PROCESS_BOOTSTRAP` finds it.
fn give_bootstrap(process: &crate::object::process::Process, kernel: u32) {
    use crate::object::{Object, process::Bootstrap};
    let Some((Object::Channel(end), rights)) = process.with_bootstrap(Bootstrap::take) else {
        return;
    };
    let Some(handle) = crate::bridge::export(Object::Channel(end), rights) else {
        return;
    };
    if let Err(why) = ferrix_native::lift_calls::bootstrap_move(
        ferrix_rt::Kernel,
        ferrix_native_abi::handle::Handle(kernel),
        handle,
        true,
    ) {
        crate::println!("linuxd: a process's bootstrap channel could not go over: {why:?}");
    }
}

fn launch(task: &Arc<Task>) -> Result<(), &'static str> {
    reap_stacks();
    let top = stack_for(task)?;
    let entry = thread_main as *const () as usize;
    // A program's first thread starts a kernel process of its own (K11); every
    // other thread of it starts from one already in that process, as a Linux
    // thread's `clone` does, and a worker from linuxd's own.
    if let Some(process) = task.core_process() {
        let root = process.own_space().root_table();
        if crate::fill::space_of(root).is_none() {
            let (kernel, space) = crate::sys::process_new(b"linux").map_err(|()| "no process")?;
            crate::fill::bind(root, space);
            crate::fill::note_kernel_process(root, kernel, space);
            task.bound_process
                .store(u64::from(kernel), Ordering::Release);
            give_bootstrap(process, kernel);
            return crate::sys::process_start(kernel, entry, top).map_err(|()| "no start");
        }
        if let Some(kernel) = crate::fill::kernel_process_thread(root) {
            task.bound_process
                .store(u64::from(kernel), Ordering::Release);
        }
    }
    let handle = crate::sys::thread_create(entry, top).map_err(|_| "no thread")?;
    if task
        .thread_handle
        .compare_exchange(0, handle, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        crate::sys::handle_close(handle);
    }
    Ok(())
}

pub fn running_group() -> u32 {
    with_current(|task| task.group.load(Ordering::Relaxed)).unwrap_or(crate::object::quota::NONE)
}
pub fn set_current_group(index: u32) {
    account::settle_own();
    let _ = with_current(|task| task.set_group(index));
}
pub fn rearm_timers() {}
pub fn current_id() -> Option<TaskId> {
    with_current(|task| task.id)
}
pub fn may_block() -> bool {
    true
}
pub fn spawn(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
) -> Result<Arc<Task>, &'static str> {
    let task = new_task(name, entry, argument, None, None)?;
    launch(&task)?;
    Ok(task)
}
pub fn spawn_on(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
    cpu: usize,
    affinity: CpuSet,
) -> Result<Arc<Task>, &'static str> {
    spawn(name, entry, argument, weight)
}
pub fn spawn_in_group(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    cpu: usize,
    group: u32,
) -> Result<Arc<Task>, &'static str> {
    let task = new_task(name, entry, argument, None, None)?;
    task.set_group(group);
    launch(&task)?;
    Ok(task)
}
pub fn spawn_user(
    name: &'static str,
    entry: fn(usize),
    thread: Arc<dyn UserThread>,
    cpu: Option<usize>,
    state: Option<arch::UserState>,
) -> Result<Arc<Task>, &'static str> {
    Ok(prepare_user(name, entry, thread, cpu, state)?.launch())
}
pub fn prepare_user(
    name: &'static str,
    entry: fn(usize),
    thread: Arc<dyn UserThread>,
    cpu: Option<usize>,
    state: Option<arch::UserState>,
) -> Result<PreparedTask, &'static str> {
    let process = alloc::sync::Arc::clone(&thread);
    let task = new_task(name, entry, 0, Some(thread), state)?;
    // Counted before it can run, as the hybrid's spawn counts it: its end could
    // otherwise come before the count, and a process's last thread would not know.
    process.process().thread_starting();
    Ok(PreparedTask { task })
}
#[must_use = "dropping a prepared task frees it instead of running it"]
#[derive(Debug)]
pub struct PreparedTask {
    task: Arc<Task>,
}
impl PreparedTask {
    pub fn task(&self) -> &Arc<Task> {
        &self.task
    }
    pub fn launch(self) -> Arc<Task> {
        if let Err(why) = launch(&self.task) {
            crate::println!("linuxd: a thread did not start: {why}");
            if let Some(thread) = self.task.thread() {
                thread.process().thread_gone(false);
            }
        }
        self.task
    }
}
/// How a wake is counted; the kernel's scheduler is what acts on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    Sync,
    Interrupt,
    Plain,
    Home,
}
/// A last look before a blocked caller is parked: the kernel's fast path only.
#[derive(Clone, Copy, Debug)]
pub struct LastLook;
impl LastLook {
    pub fn hold() -> LastLook {
        LastLook
    }
}
#[derive(Debug)]
pub enum Due {
    Wake(Arc<Task>),
    Fire(crate::object::timer::Armed),
}
pub fn wake_with(task: &Arc<Task>, how: Wake) {
    wake(task);
}
pub fn file_timer_here(
    latest: u64,
    id: u64,
    armed: crate::object::timer::Armed,
    slot: SleepSlot,
    now: u64,
) -> Result<usize, (crate::object::timer::Armed, SleepSlot)> {
    panic!("shim: sched::file_timer_here: timer objects are not served to linuxd's programs")
}
pub fn unfile_timer(
    cpu: usize,
    timer: &crate::object::timer::TimerCore,
) -> Option<(crate::object::timer::Armed, SleepSlot)> {
    None
}
pub fn locks_here() -> Option<(usize, u32, Option<&'static core::panic::Location<'static>>)> {
    None
}
pub fn note_moved() {}
pub fn preempt_disable() {}
pub fn preempt_enable() {}
pub fn started() -> bool {
    true
}
pub fn spawn_on_in(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
    cpu: usize,
    affinity: CpuSet,
    group: u32,
) -> Result<Arc<Task>, &'static str> {
    spawn_in_group(name, entry, argument, cpu, group)
}
pub fn begin_leaving() {}
pub fn exit_leaving() -> ! {
    account::left_for_good();
    if let Some(task) = current() {
        task.dead.store(true, Ordering::Release);
        let kernel = task.bound_process.load(Ordering::Acquire);
        if kernel != 0 {
            crate::fill::kernel_process_thread_gone(kernel as u32);
        }
        task.gone.wake_all();
        let handle = task.thread_handle.load(Ordering::Acquire);
        drop(task);
        // The stack's own reference goes now: a thread's end is what lets its
        // process end, and its pid be given back. The stack itself waits for the
        // kernel thread to be gone (`reap_stacks`).
        let slot = task_slot(sp_now());
        // SAFETY: this thread's own slot; nothing reads it after this but `current`,
        // which reads null.
        let raw = unsafe { core::ptr::replace(slot, core::ptr::null()) };
        if !raw.is_null() {
            // SAFETY: the strong count `stack_for` left there.
            drop(unsafe { Arc::from_raw(raw) });
        }
        ENDED.lock().push((sp_now() & !(STACK_BYTES - 1), handle));
    }
    crate::sys::thread_exit()
}

/// Stacks of threads that have said they are ending, with their handles.
static ENDED: crate::sync::SpinLock<Vec<(usize, u32)>> = crate::sync::SpinLock::new(Vec::new());

/// Free the stacks whose kernel threads are gone.
fn reap_stacks() {
    let Ok(layout) = Layout::from_size_align(STACK_BYTES, STACK_BYTES) else {
        return;
    };
    let mut gone = Vec::new();
    ENDED.lock().retain(|&(base, handle)| {
        if handle != 0 && crate::sys::thread_ended(handle) {
            gone.push((base, handle));
            false
        } else {
            true
        }
    });
    for (base, handle) in gone {
        crate::sys::handle_close(handle);
        // SAFETY: `stack_for` allocated it with this layout, and its thread is gone.
        unsafe { alloc::alloc::dealloc(base as *mut u8, layout) };
    }
}
/// The running task, read from the stack the running thread is on.
pub fn current() -> Option<Arc<Task>> {
    let slot = task_slot(sp_now());
    // SAFETY: every thread of linuxd runs on a stack `stack_for` made, or on the
    // first thread's, `adopt`ed before anything asks.
    let raw = unsafe { *slot };
    if raw.is_null() {
        return None;
    }
    // SAFETY: the slot holds a strong count for as long as the thread runs.
    unsafe {
        Arc::increment_strong_count(raw);
        Some(Arc::from_raw(raw))
    }
}
/// The running task, lent.
#[inline(always)]
pub fn with_current<R>(lend: impl FnOnce(&Task) -> R) -> Option<R> {
    #[cold]
    #[inline(always)]
    const fn unusual() {}
    let slot = task_slot(sp_now());
    // SAFETY: as `current`.
    let raw = unsafe { *slot };
    if raw.is_null() {
        unusual();
        return None;
    }
    // SAFETY: as `current`; the borrow ends before the thread can.
    Some(lend(unsafe { &*raw }))
}
/// Make a task of the first thread: `top` is its stack's top, in a stack
/// allocated by `first_stack`.
pub fn first_stack(entry: fn(usize)) -> Result<usize, &'static str> {
    let task = new_task("linuxd", entry, 0, None, None)?;
    stack_for(&task)
}
pub fn current_runtime() -> u64 {
    account::settle_own();
    with_current(|task| task.time.runtime()).unwrap_or(0)
}
pub fn yield_now() {
    crate::sys::thread_yield();
}
/// The reservations, by the kernel's `THREAD_RESERVE` (`docs/microkernel/K9.md`).
#[cfg(ferrix_schedule = "edf")]
pub mod reserve {
    use core::sync::atomic::Ordering;
    /// What a thread asks for: the kernel's words.
    #[derive(Clone, Copy, Debug)]
    pub struct Reservation {
        pub runtime_ns: u64,
        pub deadline_ns: u64,
        pub period_ns: u64,
    }
    /// Why the kernel said no.
    #[derive(Clone, Copy, Debug)]
    pub enum Refusal {
        Malformed,
        Unschedulable,
        Full,
        Already,
        /// The partition's job, which linuxd holds, does not give it: never EBUSY.
        Denied,
    }
    /// Whether the running thread holds one, in the task's own record.
    #[must_use]
    pub fn holds_own() -> bool {
        super::with_current(|task| task.reserved.load(Ordering::Relaxed)) == Some(true)
    }
    fn asked(reservation: Reservation) -> Result<(), Refusal> {
        crate::sys::thread_reserve(
            reservation.runtime_ns,
            reservation.deadline_ns,
            reservation.period_ns,
        )
    }
    /// The running thread takes `reservation`.
    pub fn take(reservation: Reservation) -> Result<(), Refusal> {
        asked(reservation)?;
        let _ = super::with_current(|task| task.reserved.store(true, Ordering::Relaxed));
        Ok(())
    }
    /// The running thread gives it back.
    pub fn give_back() -> Result<(), Refusal> {
        asked(Reservation {
            runtime_ns: 0,
            deadline_ns: 0,
            period_ns: 0,
        })?;
        let _ = super::with_current(|task| task.reserved.store(false, Ordering::Relaxed));
        Ok(())
    }
}
pub fn sleep_until(deadline: u64) {
    let Some(me) = current() else {
        return;
    };
    me.blocked.store(true, Ordering::SeqCst);
    loop {
        me.park.store(0, Ordering::SeqCst);
        let now = crate::timer::now_nanos();
        if now >= deadline {
            break;
        }
        crate::sys::park(&me.park, deadline);
        if me.work.load(Ordering::Acquire) & work::END != 0 {
            break;
        }
    }
    me.blocked.store(false, Ordering::SeqCst);
}
pub fn sleep_for(nanos: u64) {
    sleep_until(crate::timer::now_nanos().saturating_add(nanos));
}
pub fn wake(task: &Arc<Task>) {
    let _ = task.wakes.fetch_add(1, Ordering::Relaxed);
    task.park.store(1, Ordering::SeqCst);
    // A thread running here is parked on nothing: its own wake needs no call. And
    // under a lock of its own, the call waits for the lock (`lock_taken`).
    let (own, later) =
        with_current(|me| (me.id == task.id, me.wake_later(task))).unwrap_or((false, false));
    if !own && !later {
        crate::sys::unpark(&task.park);
    }
}

/// A lock of this thread's is held from here: the kernel's lock would stop preemption
/// until it goes, so a wake under it does not switch. A wake's system call in ring 3
/// may switch to the woken thread at once, which then spins on the lock its waker
/// holds, on one processor until it yields (`futex-thread-pingpong`,
/// docs/microkernel/PERFORMANCE.md). So the call waits for the last lock to go.
#[inline]
pub(crate) fn lock_taken() {
    if ON_TASK_STACKS.load(Ordering::Relaxed) {
        let _ = with_current(|me| me.locks_held.set(me.locks_held.get() + 1));
    }
}

/// One of this thread's locks went: with the last, the wakes it held back.
#[inline]
pub(crate) fn lock_given_back() {
    if !ON_TASK_STACKS.load(Ordering::Relaxed) {
        return;
    }
    let _ = with_current(|me| {
        let held = me.locks_held.get().saturating_sub(1);
        me.locks_held.set(held);
        if held == 0 && me.wakes_held.get() != 0 {
            me.wakes_now();
        }
    });
}

/// This thread is about to give up the processor, perhaps under a lock (a spinning waiter
/// yields): whoever it held a wake back from goes now. Otherwise the woken thread may be
/// the one that would let it run again.
pub(crate) fn wakes_before_stopping() {
    if ON_TASK_STACKS.load(Ordering::Relaxed) {
        let _ = with_current(|me| {
            if me.wakes_held.get() != 0 {
                me.wakes_now();
            }
        });
    }
}

/// The first thread has left the stack `rt` gave it for one of `stack_for`'s: from here
/// `with_current` may be asked by every lock. Before this, the word it reads is not a
/// task's, and the heap's lock is taken long before.
static ON_TASK_STACKS: AtomicBool = AtomicBool::new(false);
pub fn on_task_stacks() {
    ON_TASK_STACKS.store(true, Ordering::Relaxed);
}
pub fn drained(cpu: usize) -> u64 {
    0
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServiceRefusal {
    BootProcessor,
    Last,
    Pinned,
    NoStealing,
    NotThere,
    TimedOut,
}
pub fn move_hold_max() -> u64 {
    0
}
pub fn rehomed(cpu: usize) -> u64 {
    0
}
pub fn set_request(task: &Arc<Task>, request_ns: u64) {
    task.request.store(request_ns, Ordering::Relaxed);
}
pub fn set_weight(task: &Arc<Task>, weight: u32) {
    task.weight.store(weight, Ordering::Relaxed);
}
pub const REAPER_PATIENCE_NANOS: u64 = 20_000_000_000;
pub fn wait_until_reaper_quiet(patience_nanos: u64) -> Result<(), &'static str> {
    Ok(())
}
pub fn wait_until_gone(task: &Task, patience_nanos: u64) -> Result<(), &'static str> {
    let until = crate::timer::now_nanos().saturating_add(patience_nanos);
    if task
        .gone
        .wait_until_deadline(|| task.dead.load(Ordering::Acquire), until)
    {
        Ok(())
    } else {
        Err("a task did not end")
    }
}
pub fn reap() -> usize {
    0
}
pub fn charge_running() {
    account::charge_running();
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub switches: u64,
}
pub fn summary() -> Summary {
    Summary::default()
}
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuTime {
    pub busy_ns: u64,
    pub idle_ns: u64,
    pub switches: u64,
    pub runnable: usize,
}
pub fn cpu_times() -> Result<Vec<CpuTime>, fallible::AllocError> {
    let mut times = Vec::new();
    fallible::try_push(&mut times, CpuTime::default())?;
    Ok(times)
}
pub fn tasks_made() -> u64 {
    MADE.load(Ordering::Relaxed)
}
pub fn bandwidth_walks() -> u64 {
    0
}

pub fn in_call_here() -> bool {
    true
}

pub fn running_restricted() -> bool {
    false
}

/// End the kernel's threads of the job at `job`: `Job::kill` under linuxd.
pub fn kill_kernel_job(job: usize) {
    crate::sys::kill_kernel_job(job);
}

/// The job at `job` has gone.
pub fn forget_kernel_job(job: usize) {
    crate::sys::forget_kernel_job(job);
}
