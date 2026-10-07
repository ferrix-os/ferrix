//! What a task is.
//!
//! A kernel thread: a stack, a saved stack pointer, and the bookkeeping that
//! says where it is. Everything about *choosing* between tasks is
//! `ferrix_sched`'s; everything here is what the choice is made about.
//!
//! # Where the saved stack pointer lives
//!
//! In an [`UnsafeCell`], written by the CPU switching away from the task and
//! read by the CPU switching to it. Neither is a data race, because both hold
//! the same run queue's lock: the lock is taken before the decision and
//! released by the *incoming* context after the switch, so a task's saved
//! stack pointer is only ever touched by the one CPU that owns its queue at
//! that moment. That hand-over is what `SpinLock::lock_manually` exists for.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{
    AtomicBool, AtomicI64, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering,
};

use ferrix_sched::{CpuSet, EntityState, Node, Slot};
use ferrix_sync::IrqControl;

use crate::arch;
use crate::fallible::{self, AllocError};
use crate::object::process::Host;
use crate::object::quota;
use crate::user::space::AddressSpace;
use crate::vmap::Stack;

/// A line of execution through a program, as the scheduler sees it: what a
/// task that runs user code holds.
///
/// The thread itself is the personality's -- its id, the signals sent to it
/// alone and the address to clear when it ends are POSIX's, and none of them
/// is the scheduler's business. What the scheduler needs is the process it
/// runs in, to count it starting and gone, and the reference that keeps the
/// thread, and through it the process, alive while the task is.
///
/// `Any`, so that the personality can have its own thread back from a task.
pub(crate) trait UserThread: Any + Send + Sync + fmt::Debug {
    /// The process whose code it runs.
    fn process(&self) -> &dyn Host;
}

/// A task's name, unique for the life of the machine.
pub(crate) type TaskId = u64;

/// Runnable: on a run queue, or running.
pub(crate) const RUNNABLE: u8 = 0;
/// Blocked: waiting for something, and on no run queue.
pub(crate) const BLOCKED: u8 = 1;
/// Dead: it has returned or exited, and its stack is waiting to be freed.
pub(crate) const DEAD: u8 = 2;

/// One kernel thread.
#[derive(Debug)]
pub(crate) struct Task {
    /// Its name.
    pub(crate) id: TaskId,
    /// What to call it in the boot log.
    pub(crate) name: &'static str,
    /// What it runs. `None` for a context that was already running when the
    /// scheduler adopted it: the boot task, and each CPU's idle task.
    entry: Option<(fn(usize), usize)>,
    /// Its stack, unless it is running on one boot gave it.
    stack: Option<Stack>,
    /// Where its stack pointer is kept while it is not running.
    stack_pointer: UnsafeCell<u64>,
    /// The address space its user half is translated through, or `None` for a
    /// kernel thread.
    ///
    /// Holding an [`Arc`] here is what keeps the tables alive while a
    /// processor is walking them: the root register is installed by
    /// `sched::choose_next` on the way in, and the only thing standing between
    /// those tables and the frame allocator is this reference. A task that
    /// dies keeps it until it is reaped, which is after the last switch away
    /// from it.
    address_space: Option<Arc<AddressSpace>>,
    /// One reference to the core process its thread runs in
    /// (`thread.process().core_arc()`), taken as the task is made, for the
    /// native round trip's fast path, which reaches the handle table through
    /// it without the two `dyn` calls (`docs/OPAQUE-KERNEL.md` §9.11). `Some`
    /// exactly when `thread` is. Declared before `thread`, so that it is let
    /// go first and the core is still freed with its personality's process.
    core: Option<Arc<crate::object::process::Process>>,
    /// The thread of a process whose code this task runs in user mode, or
    /// `None` for a kernel thread. Holding it is what keeps the thread, and
    /// through it the process, alive while the task is: a process does not own
    /// its tasks, its tasks own it.
    thread: Option<Arc<dyn UserThread>>,
    /// The user registers no trap saves -- thread pointer, floating point --
    /// kept here while the task is not running. `Some` exactly when `thread`
    /// is. Boxed because it is half a kilobyte and most tasks have none.
    user: Option<Box<UnsafeCell<arch::UserState>>>,
    /// [`RUNNABLE`], [`BLOCKED`] or [`DEAD`].
    state: AtomicU8,
    /// The logical CPU whose queue owns it.
    cpu: AtomicU64,
    /// Whether a run queue holds it, as a queued entity or as the running one.
    queued: AtomicBool,
    /// Whether it is inside a system call, as the processor running it last
    /// had it (`crate::sched::IN_CALL`): kept here while it does not run.
    in_call: AtomicBool,
    /// Whether it may be moved to another CPU.
    /// The processors it may run on.
    affinity: CpuSet,
    /// Its share of a CPU, as the run queue has it: [`Task::base_weight`]
    /// scaled by its job's processor weight (`object::quota::effective`).
    weight: AtomicU32,
    /// Its own weight, from its nice value, before its job's share is applied.
    base_weight: AtomicU32,
    /// The job whose processor share it runs in, and the weight it adds to
    /// that job's load while runnable: the quota slot in the high half, the
    /// weight in the low, zero while not runnable. One word, so that a task
    /// joining, leaving and changing job at once always takes out exactly
    /// what it put in, from where it put it.
    group: AtomicU64,
    /// How many moves between jobs it had seen when it last looked at its
    /// process's job (`crate::sched::regroup_current`).
    seen_moves: AtomicU64,
    /// The lag it left its last queue with.
    vlag: AtomicI64,
    /// Real nanoseconds it has run for, mirrored out of the queue so that a
    /// reader needs no lock.
    sum_exec: AtomicU64,
    /// What `sum_exec` was when a measurement window opened.
    baseline: AtomicU64,
    /// Its runtime when the window closed, for a check that reads the shares
    /// afterwards: what it ran between `close_window` and its exit was never
    /// measured and must not be judged.
    window_end: AtomicU64,
    /// Whether a measurement window is counting this task. A task that joined
    /// a queue after the window opened is not: it was never owed the service
    /// handed out before it arrived, and counting it would make the fairness
    /// check report a violation that is really an arrival.
    measured: AtomicBool,
    /// When it should wake, or zero if it is not sleeping. Read and cleared
    /// under the owning queue's lock, at the moment the task leaves it.
    sleep_until: AtomicU64,
    /// How many times it has been switched to.
    switches: AtomicU64,
    /// How many times it was switched away from while still runnable, by an
    /// interrupt that arrived while it ran in user mode: taken off the
    /// processor in the middle of its own code, not at a call it made.
    preemptions: AtomicU64,
    /// Which CPUs it has run on, one bit each.
    cpus_run_on: AtomicU64,
    /// Its pending work, one bit for each thing the way back to user mode
    /// would have to act on (`sched::work`): written only through that
    /// module, set by a poster after the state it stands for and cleared by
    /// the task itself before it reads that state.
    work: AtomicU32,
    /// The reply cell of step 4's fast path (`docs/OPAQUE-KERNEL.md` §9.7,
    /// part 2): a message handed to it while it was parked in
    /// `channel_write_read`. The length with [`REPLY_FULL`] in the first
    /// word, the three words after. Filled only by the commit that hands the
    /// processor to it, under its home's run-queue lock while it is asleep;
    /// emptied only by itself, once a switch under that lock has resumed it.
    reply: [AtomicU64; 4],
    /// The node a run queue holds it in, while no run queue does.
    ///
    /// Lent to a queue as the task is queued and handed back as it leaves,
    /// so that queueing, which a wake-up does from an interrupt handler,
    /// allocates nothing (finding F-23). Allocated with the task.
    run_slot: SlotCell,
    /// The node a processor's sleeper set, or the reaper's list, holds it
    /// in, while neither does. As `run_slot`, for sleeping and for dying.
    sleep_slot: SlotCell,
}

/// One of a task's slots, while the task holds it: a node pointer, null
/// while a queue, a sleeper set or the reaper's list holds the slot
/// (`L.sched.63`).
///
/// **Each operation is one atomic, so each is linearisable.** Taking it is a
/// swap with null; asking whether it is held is one load; giving it back is
/// a compare-exchange from null. The swap means two takers never both get
/// the node, and the compare-exchange that no give-back overwrites a node:
/// one into a cell that is not empty stops the machine (FX-0534). Only the
/// holder that took a slot gives it back, and it took the only copy, so that
/// never happens; and a node is freed only by the task's drop, from the
/// cell, once. The `loom` model is `src/tests/loom/tests/slots.rs`; a load
/// and then a store for the give-back, the form first proposed, could not
/// be held to it there. Until 2026-10-07 the cell was a
/// `SpinLock<Option<TaskSlot>>`, about 5 ns a take or give-back against
/// about 4 for one locked operation and 0.3 for a load, and the fast path
/// made three a direction (`docs/OPAQUE-KERNEL.md` §9.7, "as built", 14).
struct SlotCell(AtomicPtr<Node<Arc<Task>>>);

impl fmt::Debug for SlotCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.holds() {
            "SlotCell(held)"
        } else {
            "SlotCell(lent)"
        })
    }
}

impl SlotCell {
    /// A cell holding `slot`.
    fn new(slot: TaskSlot) -> SlotCell {
        SlotCell(AtomicPtr::new(Box::into_raw(slot.into_box())))
    }

    /// Take the slot, if the cell holds it.
    fn take(&self) -> Option<TaskSlot> {
        let node = self.0.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if node.is_null() {
            return None;
        }
        // SAFETY: (KMEM) a non-null pointer in the cell came from
        // `Box::into_raw` in `new` or `put`, and the swap took the cell's only
        // copy of it, so this is the one box made from it.
        Some(Slot::from_box(unsafe { Box::from_raw(node) }))
    }

    /// Give the slot back, into a cell that must be empty (FX-0534).
    fn put(&self, slot: TaskSlot) {
        let node = Box::into_raw(slot.into_box());
        if self
            .0
            .compare_exchange(
                core::ptr::null_mut(),
                node,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            crate::panic::fatal!(
                crate::panic::catalog::TASK_SLOT_RETURNED_TWICE,
                "a task's slot was given back while its cell held one"
            );
        }
    }

    /// Whether the cell holds its slot.
    fn holds(&self) -> bool {
        !self.0.load(Ordering::Acquire).is_null()
    }
}

impl Drop for SlotCell {
    /// Free the node, if the cell holds it. A task is dropped only when
    /// nothing holds it, so then nothing holds its slots either.
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// A full reply cell's mark, in its first word beside the length.
const REPLY_FULL: u64 = 1 << 63;

/// The node a queue, a sleeper set or the reaper holds a task in.
pub(crate) type TaskSlot = Slot<Arc<Task>>;

/// The two slots a task needs, allocated together.
fn slots() -> Result<(TaskSlot, TaskSlot), AllocError> {
    Ok((Slot::new()?, Slot::new()?))
}

// SAFETY: (SHARED) every field but `stack_pointer` and `user` is an atomic or
// immutable. Both cells are written by the CPU that switches away from this
// task and read by the one that switches to it, and both hold the lock of the
// run queue that owns the task at that moment — so the accesses are ordered by
// that lock and never overlap.
unsafe impl Sync for Task {}

impl Drop for Task {
    /// Let go of the job it ran in. Its weight left the job's load when it
    /// died, so only the hold is left.
    fn drop(&mut self) {
        let word = *self.group.get_mut();
        if counted_of(word) != 0 {
            quota::adjust(group_of(word), -i64::from(counted_of(word)));
        }
        quota::release_group(group_of(word));
    }
}

/// A task's group word with no job and nothing counted.
const fn ungrouped() -> u64 {
    pack(quota::NONE, 0)
}

/// A group word: `index` in the high half, `counted` in the low.
const fn pack(index: u32, counted: u32) -> u64 {
    ((index as u64) << 32) | counted as u64
}

/// The job a group word names.
const fn group_of(word: u64) -> u32 {
    (word >> 32) as u32
}

/// The weight a group word counts.
const fn counted_of(word: u64) -> u32 {
    (word & 0xFFFF_FFFF) as u32
}

/// Everything a new task needs, which is more than a function should take as
/// loose arguments: nine of them, four of which are integers, is a call whose
/// meaning depends on getting the order right.
///
/// **No longer `Copy`**, and the reason is the point rather than an
/// inconvenience. It used to be, on the grounds that nothing here owned a
/// resource whose release the type system tracks — a [`Stack`] is freed by
/// `vmap::free_stack` against the address in it rather than by dropping
/// anything. An [`Arc<AddressSpace>`] is exactly such a resource: copying the
/// descriptor would duplicate a reference without raising the count, and the
/// page tables would be freed while a processor still had their root in its
/// register. `Clone` stays, because cloning does raise it.
#[derive(Clone, Debug)]
pub(crate) struct NewTask {
    /// Its identifier, never reused.
    pub(crate) id: TaskId,
    /// What it is called, for the boot log and for diagnostics.
    pub(crate) name: &'static str,
    /// Where it starts, and the one argument it is handed.
    pub(crate) entry: fn(usize),
    /// That argument.
    pub(crate) argument: usize,
    /// The stack it owns, and the pointer into it a switch resumes.
    pub(crate) stack: Stack,
    /// Where in that stack `prepare_stack` left its first frame.
    pub(crate) stack_pointer: u64,
    /// Its scheduling weight.
    pub(crate) weight: u32,
    /// The processor it starts on.
    pub(crate) cpu: usize,
    /// The processors it may run on. A task pinned to one is an affinity of
    /// one, which is the same thing said once rather than twice.
    pub(crate) affinity: CpuSet,
    /// The address space it runs in, or `None` for a kernel thread.
    pub(crate) address_space: Option<Arc<AddressSpace>>,
    /// The thread it runs user code for, or `None` for a kernel thread.
    pub(crate) thread: Option<Arc<dyn UserThread>>,
    /// The user registers it starts with, for a task with a thread: `None`
    /// is a program's starting state, and a fork child passes a copy of its
    /// parent's.
    pub(crate) user_state: Option<arch::UserState>,
}

impl Task {
    /// Its pending-work word, for `sched::work` alone, which owns every
    /// read and write of it.
    pub(super) const fn work(&self) -> &AtomicU32 {
        &self.work
    }

    /// A task that will start at `entry` on a stack of its own.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for its slots or its user
    /// registers.
    pub(crate) fn new(new: NewTask) -> Result<Task, AllocError> {
        let NewTask {
            id,
            name,
            entry,
            argument,
            stack,
            stack_pointer,
            weight,
            cpu,
            affinity,
            address_space,
            thread,
            user_state,
        } = new;
        let user = match thread.as_ref() {
            Some(_) => Some(fallible::try_box(UnsafeCell::new(
                user_state.unwrap_or_else(arch::UserState::new),
            ))?),
            None => None,
        };
        let (run_slot, sleep_slot) = slots()?;
        let core = thread
            .as_ref()
            .map(|thread| Arc::clone(thread.process().core_arc()));
        Ok(Task {
            id,
            name,
            entry: Some((entry, argument)),
            stack: Some(stack),
            stack_pointer: UnsafeCell::new(stack_pointer),
            state: AtomicU8::new(RUNNABLE),
            cpu: AtomicU64::new(cpu as u64),
            queued: AtomicBool::new(false),
            in_call: AtomicBool::new(false),
            affinity,
            address_space,
            thread,
            core,
            user,
            weight: AtomicU32::new(weight),
            base_weight: AtomicU32::new(weight),
            group: AtomicU64::new(ungrouped()),
            seen_moves: AtomicU64::new(0),
            vlag: AtomicI64::new(0),
            sum_exec: AtomicU64::new(0),
            baseline: AtomicU64::new(0),
            window_end: AtomicU64::new(0),
            measured: AtomicBool::new(false),
            sleep_until: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            preemptions: AtomicU64::new(0),
            cpus_run_on: AtomicU64::new(0),
            work: AtomicU32::new(0),
            reply: [const { AtomicU64::new(0) }; 4],
            run_slot: SlotCell::new(run_slot),
            sleep_slot: SlotCell::new(sleep_slot),
        })
    }

    /// A task for a context that is already running: the boot task, and each
    /// CPU's idle task. Its stack is whoever started it, and its saved stack
    /// pointer is filled in the first time it is switched away from.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for its slots.
    pub(crate) fn adopt(
        id: TaskId,
        name: &'static str,
        weight: u32,
        cpu: usize,
    ) -> Result<Task, AllocError> {
        let (run_slot, sleep_slot) = slots()?;
        Ok(Task {
            id,
            name,
            entry: None,
            stack: None,
            stack_pointer: UnsafeCell::new(0),
            state: AtomicU8::new(RUNNABLE),
            cpu: AtomicU64::new(cpu as u64),
            queued: AtomicBool::new(false),
            in_call: AtomicBool::new(false),
            // An adopted context — the boot task, or a processor's idle task —
            // is the one thing that genuinely cannot move: it *is* that
            // processor's context. An affinity of exactly its own processor
            // says so in the same terms as everything else.
            affinity: CpuSet::of(cpu),
            // The boot task and the idle tasks are the kernel's own and have
            // no user half to translate.
            address_space: None,
            thread: None,
            core: None,
            user: None,
            weight: AtomicU32::new(weight),
            base_weight: AtomicU32::new(weight),
            group: AtomicU64::new(ungrouped()),
            seen_moves: AtomicU64::new(0),
            vlag: AtomicI64::new(0),
            sum_exec: AtomicU64::new(0),
            baseline: AtomicU64::new(0),
            window_end: AtomicU64::new(0),
            measured: AtomicBool::new(false),
            sleep_until: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            preemptions: AtomicU64::new(0),
            cpus_run_on: AtomicU64::new(0),
            work: AtomicU32::new(0),
            reply: [const { AtomicU64::new(0) }; 4],
            run_slot: SlotCell::new(run_slot),
            sleep_slot: SlotCell::new(sleep_slot),
        })
    }

    /// Take the slot a run queue holds this task in, to queue it with.
    /// `None` if a queue already holds it.
    pub(crate) fn take_run_slot(&self) -> Option<TaskSlot> {
        self.run_slot.take()
    }

    /// Have back the slot a run queue held this task in.
    pub(crate) fn return_run_slot(&self, slot: TaskSlot) {
        self.run_slot.put(slot);
    }

    /// Take the slot a sleeper set or the reaper holds this task in. `None`
    /// if one already holds it.
    pub(crate) fn take_sleep_slot(&self) -> Option<TaskSlot> {
        self.sleep_slot.take()
    }

    /// Have back the slot a sleeper set or the reaper held this task in.
    pub(crate) fn return_sleep_slot(&self, slot: TaskSlot) {
        self.sleep_slot.put(slot);
    }

    /// Hand it a reply of `len` bytes in `words`, the bytes past `len`
    /// already zero: the fast path's commit, under its home's run-queue lock
    /// while it is asleep there. The lock's hand-over at the switch to it is
    /// what orders these stores before its own reads.
    pub(crate) fn fill_reply(&self, len: usize, words: [u64; 3]) {
        for (cell, word) in self.reply.iter().skip(1).zip(words) {
            cell.store(word, Ordering::Relaxed);
        }
        if let Some(first) = self.reply.first() {
            first.store(REPLY_FULL | len as u64, Ordering::Relaxed);
        }
    }

    /// The reply it was handed, emptying the cell: by the task itself, as it
    /// resumes in `channel_write_read`. `None` when it was woken some other
    /// way.
    pub(crate) fn take_reply(&self) -> Option<(usize, [u64; 3])> {
        let first = self.reply.first()?;
        let head = first.load(Ordering::Relaxed);
        if head & REPLY_FULL == 0 {
            return None;
        }
        first.store(0, Ordering::Relaxed);
        let mut words = [0_u64; 3];
        for (word, cell) in words.iter_mut().zip(self.reply.iter().skip(1)) {
            *word = cell.load(Ordering::Relaxed);
        }
        Some(((head & !REPLY_FULL) as usize, words))
    }

    /// What it runs, if it has not started yet.
    pub(crate) const fn entry(&self) -> Option<(fn(usize), usize)> {
        self.entry
    }

    /// Its stack, for the reaper to free.
    pub(crate) const fn stack(&self) -> Option<Stack> {
        self.stack
    }

    /// Where to save its stack pointer.
    ///
    /// # Safety
    ///
    /// (SHARED) The caller must hold the lock of the run queue that owns this task,
    /// and must only pass the pointer to the context switch.
    pub(crate) const unsafe fn stack_pointer_slot(&self) -> *mut u64 {
        self.stack_pointer.get()
    }

    /// The stack pointer it was last saved at.
    ///
    /// # Safety
    ///
    /// (SHARED) The caller must hold the lock of the run queue that owns this task.
    pub(crate) unsafe fn saved_stack_pointer(&self) -> u64 {
        // SAFETY: (SHARED) the caller holds the owning queue's lock, which is what
        // orders this read against the write made by whoever switched away.
        unsafe { *self.stack_pointer.get() }
    }

    /// What it is doing.
    pub(crate) fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// Say what it is doing: and, as it becomes runnable or stops being, add
    /// its weight to its job's processor load or take it out.
    ///
    /// **One step on this processor**, under masked interrupts. A switch
    /// acts on the state alone: one that finds the running task not
    /// runnable takes it off the queue, and a task marked dead is never run
    /// again. With interrupts on, a switch between the state and the load
    /// found an exiting task dead and still counted, and the line that would
    /// have taken its weight out never ran: its job's load stayed up for good
    /// (FX-0905, the quota check's "tasks gone and still counted"). A task
    /// switched out there as it blocked kept its weight while asleep, and
    /// took it out once woken, running uncounted until it next blocked.
    pub(crate) fn set_state(&self, state: u8) {
        let saved = <arch::Irq as IrqControl>::disable();
        let before = self.state.swap(state, Ordering::AcqRel);
        if before != RUNNABLE && state == RUNNABLE {
            self.join_group();
        } else if before == RUNNABLE && state != RUNNABLE {
            self.leave_group();
        }
        <arch::Irq as IrqControl>::restore(saved);
    }

    /// [`Task::set_state`] for a change whose old state the caller knows and
    /// no one else can change meanwhile, with interrupts masked: a store and
    /// the job's load, no read-modify-write of the state and no second mask.
    /// The direct switch's, under the run-queue lock every waker of either
    /// task takes, for the peer asleep (`BLOCKED`) and the caller running
    /// (`RUNNABLE`).
    /// TIMING ONLY.
    pub(crate) fn set_state_raw(&self, state: u8) {
        self.state.store(state, Ordering::Release);
    }

    pub(crate) fn set_state_from(&self, before: u8, state: u8) {
        self.state.store(state, Ordering::Release);
        if before != RUNNABLE && state == RUNNABLE {
            self.join_group();
        } else if before == RUNNABLE && state != RUNNABLE {
            self.leave_group();
        }
    }

    /// The job whose processor share it runs in, or `quota::NONE`.
    pub(crate) fn group(&self) -> u32 {
        group_of(self.group.load(Ordering::Acquire))
    }

    /// Run in `index`'s share from now on, taking its weight out of the job
    /// it was in and into the new one if it is runnable.
    ///
    /// For a task not running, or the running task on its own processor
    /// (`crate::sched::set_current_group`, which says so where charges look):
    /// a processor running a task keeps the group it switched to it with as
    /// the one its charges go to.
    pub(crate) fn set_group(&self, index: u32) {
        quota::hold_group(index);
        let swapped = self
            .group
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                Some(pack(index, counted_of(word)))
            });
        let Ok(was) = swapped else {
            quota::release_group(index);
            return;
        };
        let (old, counted) = (group_of(was), counted_of(was));
        if counted != 0 {
            quota::adjust(old, -i64::from(counted));
            quota::adjust(index, i64::from(counted));
        }
        quota::release_group(old);
        // A runnable task not counted anywhere -- one with no job until now
        // -- is counted in its new one at once, not at its next wake.
        self.join_group();
    }

    /// Count its weight in its job's load, once, if it is runnable.
    pub(crate) fn join_group(&self) {
        if self.state() != RUNNABLE {
            return;
        }
        let base = self.base_weight.load(Ordering::Relaxed);
        let joined = self
            .group
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                (counted_of(word) == 0 && group_of(word) != quota::NONE)
                    .then(|| pack(group_of(word), base))
            });
        if let Ok(word) = joined {
            quota::adjust(group_of(word), i64::from(base));
        }
    }

    /// Take its weight out of its job's load, if it is counted there.
    /// TIMING ONLY: join_group's word update without the job's load.
    pub(crate) fn join_word(&self) -> Option<(u32, u32)> {
        let base = self.base_weight.load(Ordering::Relaxed);
        self.group
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                (counted_of(word) == 0 && group_of(word) != quota::NONE)
                    .then(|| pack(group_of(word), base))
            })
            .ok()
            .map(|word| (group_of(word), base))
    }

    /// TIMING ONLY: leave_group's word update without the job's load.
    pub(crate) fn leave_word(&self) -> Option<(u32, u32)> {
        self.group
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                (counted_of(word) != 0).then(|| pack(group_of(word), 0))
            })
            .ok()
            .map(|word| (group_of(word), counted_of(word)))
    }

    fn leave_group(&self) {
        let left = self
            .group
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                (counted_of(word) != 0).then(|| pack(group_of(word), 0))
            });
        if let Ok(word) = left {
            quota::adjust(group_of(word), -i64::from(counted_of(word)));
        }
    }

    /// Whether it has already looked at its job since the `moves`th move,
    /// recording that it now has.
    pub(crate) fn seen_moves(&self, moves: u64) -> bool {
        self.seen_moves.swap(moves, Ordering::AcqRel) == moves
    }

    /// How many moves it had seen when it last looked.
    pub(crate) fn moves_seen(&self) -> u64 {
        self.seen_moves.load(Ordering::Acquire)
    }

    /// Its own weight, before its job's share.
    pub(crate) fn base_weight(&self) -> u32 {
        self.base_weight.load(Ordering::Relaxed)
    }

    /// Give it a new weight of its own: a nice value's.
    pub(crate) fn set_base_weight(&self, weight: u32) {
        self.base_weight.store(weight, Ordering::Relaxed);
    }

    /// The weight it should run at now: its own, scaled by its job's share.
    pub(crate) fn effective_weight(&self) -> u32 {
        let base = self.base_weight();
        match self.group() {
            quota::NONE => base,
            group => quota::effective(group, base),
        }
    }

    /// Whether it may be moved to another CPU.
    /// Whether it is confined to a single processor, which is what makes it
    /// ineligible for stealing or balancing.
    pub(crate) fn is_pinned(&self) -> bool {
        self.affinity.len() <= 1
    }

    /// Whether `cpu` is one of the processors it may run on.
    pub(crate) fn may_run_on(&self, cpu: usize) -> bool {
        self.affinity.contains(cpu)
    }

    /// The queue that owns it.
    pub(crate) fn cpu(&self) -> usize {
        self.cpu.load(Ordering::Acquire) as usize
    }

    /// Say which queue owns it. Only under that queue's lock, and under both
    /// when it moves.
    pub(crate) fn set_cpu(&self, cpu: usize) {
        self.cpu.store(cpu as u64, Ordering::Release);
    }

    /// Whether a run queue holds it.
    pub(crate) fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Acquire)
    }

    /// Say whether a run queue holds it. Only under that queue's lock.
    pub(crate) fn set_queued(&self, queued: bool) {
        self.queued.store(queued, Ordering::Release);
    }

    /// Whether it holds both its slots: no run queue holds it, and no sleeper
    /// set or reaper's list does. What a task [`crate::sched::wake_with`] may
    /// move must be, since a set elsewhere still holding it would run it
    /// there too.
    ///
    /// Two loads, one after the other, as it was two locks: not one atomic
    /// reading of the pair. Its caller asks under the home queue's lock,
    /// which every taker of the run slot holds, and a sleeper set elsewhere
    /// that holds the sleep slot gives it back only under its own lock and
    /// only once (`asleep_at_home`).
    pub(crate) fn holds_slots(&self) -> bool {
        self.run_slot.holds() && self.sleep_slot.holds()
    }

    /// Whether it holds its sleep slot: no sleeper set or reaper's list
    /// holds it. The direct switch's half of [`Task::holds_slots`]; its run
    /// slot is held exactly while it is not queued, which it asserts.
    pub(crate) fn holds_sleep_slot(&self) -> bool {
        self.sleep_slot.holds()
    }

    /// Swap in whether it is inside a system call, for the switch that takes
    /// it off its processor, and answer what it was: see `sched::IN_CALL`.
    ///
    /// A load and a store: only `carry_in_call` reads or writes it, under the
    /// lock of the queue that owns the task.
    pub(crate) fn swap_in_call(&self, in_call: bool) -> bool {
        let was = self.in_call.load(Ordering::Relaxed);
        self.in_call.store(in_call, Ordering::Relaxed);
        was
    }

    /// What it carries between queues.
    pub(crate) fn entity_state(&self) -> EntityState {
        EntityState {
            weight: self.weight.load(Ordering::Relaxed),
            vlag: self.vlag.load(Ordering::Relaxed),
            sum_exec: self.sum_exec.load(Ordering::Relaxed),
        }
    }

    /// Give it a new scheduling weight.
    ///
    /// The record a task carries between queues, and what it is enqueued with
    /// next. A task that is on a queue now has that queue's copy changed as
    /// well, which is [`crate::sched::set_weight`]'s job and not this one's:
    /// this one is only reached under the queue's lock.
    pub(crate) fn set_weight(&self, weight: u32) {
        self.weight.store(weight, Ordering::Relaxed);
    }

    /// Remember what it left a queue with.
    pub(crate) fn store_entity_state(&self, state: EntityState) {
        self.weight.store(state.weight, Ordering::Relaxed);
        self.vlag.store(state.vlag, Ordering::Relaxed);
        self.sum_exec.store(state.sum_exec, Ordering::Relaxed);
    }

    /// Charge it for time on a CPU.
    ///
    /// A load and a store: only the run queue that owns it charges it, under
    /// that queue's lock, and readers take one load.
    pub(crate) fn add_runtime(&self, nanos: u64) {
        let total = self.sum_exec.load(Ordering::Relaxed).wrapping_add(nanos);
        self.sum_exec.store(total, Ordering::Relaxed);
    }

    /// Real nanoseconds it has run for, up to when it was last charged: the
    /// CPU-time clocks' reading.
    pub(crate) fn runtime(&self) -> u64 {
        self.sum_exec.load(Ordering::Relaxed)
    }

    /// Start a measurement window here, and count this task in it.
    pub(crate) fn open_window(&self) {
        self.baseline
            .store(self.sum_exec.load(Ordering::Relaxed), Ordering::Relaxed);
        self.measured.store(true, Ordering::Release);
    }

    /// Stop counting this task, remembering where its runtime stood.
    pub(crate) fn close_window(&self) {
        self.window_end
            .store(self.sum_exec.load(Ordering::Relaxed), Ordering::Relaxed);
        self.measured.store(false, Ordering::Release);
    }

    /// What it ran while the last window was open.
    pub(crate) fn measured_runtime(&self) -> u64 {
        self.since_baseline(self.window_end.load(Ordering::Relaxed))
    }

    /// Whether a measurement window is counting it.
    pub(crate) fn is_measured(&self) -> bool {
        self.measured.load(Ordering::Acquire)
    }

    /// What it has run since the window opened, given its total.
    pub(crate) fn since_baseline(&self, total: u64) -> u64 {
        total.saturating_sub(self.baseline.load(Ordering::Relaxed))
    }

    /// Say when it should wake.
    pub(crate) fn set_sleep_deadline(&self, deadline: u64) {
        self.sleep_until.store(deadline, Ordering::Relaxed);
    }

    /// Take its wake-up time, leaving it not sleeping.
    ///
    /// A load first, and the swap only when one is there: most takes find
    /// none, and a deadline stored after the load is one the swap would have
    /// missed too, had it come first.
    pub(crate) fn take_sleep_deadline(&self) -> Option<u64> {
        if self.sleep_until.load(Ordering::Relaxed) == 0 {
            return None;
        }
        match self.sleep_until.swap(0, Ordering::Relaxed) {
            0 => None,
            deadline => Some(deadline),
        }
    }

    /// Note that it is about to run on `cpu`.
    ///
    /// Loads and stores: only the processor switching to it writes either,
    /// under the lock of the queue that owns it, and readers take one load.
    pub(crate) fn note_switch(&self, cpu: usize) {
        let switches = self.switches.load(Ordering::Relaxed).wrapping_add(1);
        self.switches.store(switches, Ordering::Relaxed);
        if cpu < 64 {
            let bit = 1 << cpu;
            let ran = self.cpus_run_on.load(Ordering::Relaxed);
            if ran & bit == 0 {
                self.cpus_run_on.store(ran | bit, Ordering::Relaxed);
            }
        }
    }

    /// How many times it has been switched to.
    pub(crate) fn switches(&self) -> u64 {
        self.switches.load(Ordering::Relaxed)
    }

    /// Note that an interrupt in user mode is switching it out still runnable.
    pub(crate) fn note_preemption(&self) {
        let _ = self.preemptions.fetch_add(1, Ordering::Relaxed);
    }

    /// How many times an interrupt in user mode switched it out still runnable.
    ///
    /// The number a check about preemption has to read. Being switched *to*
    /// says nothing about it: a task is switched to once when it starts and
    /// again after any call that blocked, so two switches are what a program
    /// that was never preempted shows. Nor does every switch away while
    /// runnable: a pending reschedule is also taken when a lock that disables
    /// preemption is released, and a system call that takes several such
    /// locks while the timer ticks is switched out several times without the
    /// program's own code ever having been cut.
    pub(crate) fn preemptions(&self) -> u64 {
        self.preemptions.load(Ordering::Relaxed)
    }

    /// Which CPUs it has run on, one bit each.
    pub(crate) fn cpus_run_on(&self) -> u64 {
        self.cpus_run_on.load(Ordering::Relaxed)
    }

    /// The address space it runs in, or `None` if it is a kernel thread.
    ///
    /// Borrowed rather than cloned, because the caller that matters is the
    /// switch path: it compares this against the outgoing task's by pointer
    /// and installs a root, all under the run queue lock, and raising a
    /// reference count on every switch to say what a borrow already says would
    /// be a contended atomic on the hottest path in the kernel.
    pub(crate) fn address_space(&self) -> Option<&Arc<AddressSpace>> {
        self.address_space.as_ref()
    }

    /// The thread this task runs user code for, or `None` for a kernel thread.
    pub(crate) fn thread(&self) -> Option<&Arc<dyn UserThread>> {
        self.thread.as_ref()
    }

    /// The core process its thread runs in: `thread().process().core()`,
    /// without the two `dyn` calls; `None` for a kernel thread.
    pub(crate) fn core_process(&self) -> Option<&crate::object::process::Process> {
        self.core.as_deref()
    }

    /// Where this task's user registers are kept while it is not running, or
    /// `None` for a kernel thread.
    ///
    /// # Safety
    ///
    /// (SHARED) As [`Task::stack_pointer_slot`]: the caller must hold the lock of the
    /// run queue that owns this task, which is what orders every access.
    pub(crate) unsafe fn user_state(&self) -> Option<*mut arch::UserState> {
        self.user.as_ref().map(|cell| cell.get())
    }

    /// The top of this task's own kernel stack, if it has one.
    pub(crate) fn stack_top(&self) -> Option<u64> {
        self.stack.map(|stack| stack.top)
    }

    /// Whether it has exited.
    pub(crate) fn is_dead(&self) -> bool {
        self.state() == DEAD
    }

    /// Whether it is waiting for something, on no run queue.
    pub(crate) fn is_blocked(&self) -> bool {
        self.state() == BLOCKED
    }
}
