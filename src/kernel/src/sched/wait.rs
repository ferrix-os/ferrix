//! Waiting for something, and being woken for it.
//!
//! # The lost wake-up
//!
//! The race every wait queue is built around: a task checks a condition,
//! finds it false, and is about to block — and between the two, another CPU
//! makes the condition true and wakes everyone waiting. If the sleeper is not
//! on the queue yet, that wake-up finds nobody, and the sleeper waits for an
//! event that has already happened.
//!
//! The order here is the one that closes it. A waiter joins the queue and
//! marks itself blocked *before* it looks at the condition the last time. A
//! waker takes the same lock, so it either sees the waiter — and wakes it —
//! or it made the condition true before the waiter's final look, which then
//! sees it. Marking itself runnable again is enough to cancel the block,
//! because the scheduler decides whether a task leaves the run queue by
//! reading that state, under the run queue's lock, at the moment it switches.
//!
//! # Marks
//!
//! A queue can also tell something that is not a task: a [`Mark`], which
//! every wake tells once after the tasks it drained, and the queue's drop
//! tells it is gone. A mark puts a number -- an epoll registration's serial
//! -- on its [`ReadyList`], once until the list's taker clears it, and wakes
//! the list's own queue. An epoll set hooks a mark of each registration on
//! the queues its file names, so its wait looks at only what was woken and
//! sleeps on its list's queue alone (`fs::epoll`), as Linux's
//! `ep_poll_callback` arranges. Marks and lists are this module's own types:
//! what runs under a queue's hook lock, interrupts masked, in every wake, an
//! interrupt handler's included, is core code, never the load's.
//!
//! The order that closes the lost wake-up for a mark: the hooker lists the
//! mark under the hook lock, then stores the count under the waiter lock,
//! then looks at the condition; a waker changes the condition, reads the
//! count under the waiter lock it takes anyway, then tells the marks under
//! the hook lock. A waker whose waiter-lock section begins after the
//! hooker's store sees the count and, through the two locks, the mark; one
//! whose section came before made its change before the hooker's look,
//! which sees it. So a queue nothing hooks costs a wake one relaxed load
//! inside a lock it already holds, and no fence.
//!
//! **Lock order.** A queue's hook lock is held while its marks are told. A
//! mark takes its list's lock, a leaf, lets it go, and wakes the list's
//! queue: that queue's waiter lock, the run queue's lock in
//! [`super::wake_with`], then that queue's own hook lock follow. The chain
//! is bounded and acyclic only because a list's queue is hooked by nothing
//! but the marks of the epoll sets that contain the set it belongs to, and
//! `fs::epoll` refuses a loop of sets and a depth past `MAX_NESTS`: at most
//! `MAX_NESTS + 1` hook locks deep. Nothing takes a hook lock while holding
//! a waiter lock or a run queue's lock: a wake lets its waiter lock go
//! before it wakes a task or tells a mark. Hooking and unhooking take the
//! hook lock, then the waiter lock, one after the other, never nested.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ferrix_sync::IrqSpinLock;

use crate::arch;
use crate::fallible;

use super::task::{BLOCKED, RUNNABLE, Task};

/// How long a bounded wait sleeps before looking again of its own accord.
///
/// Belt and braces against a missing notify: see `wait_until_deadline`.
const RECHECK_NANOS: u64 = 5_000_000;

/// How many waiters a wake moves out without taking the list's buffer: see
/// [`WaitQueue::wake_all_with`].
const WAKE_BATCH: usize = 4;

/// Whether [`LastLook`] holds preemption off. Always, in every build that
/// lands: only the negative controls of L.sched.71 and L.sched.72 turn it
/// off, to show their checks fire (F-69). False, it compiles to nothing.
const LAST_LOOK_HOLDS: bool = true;

/// No task: what [`LISTED_MEET`] holds when no check has armed it.
const NO_MEET: u64 = u64::MAX;

/// The task a boot check holds just after a wait has listed it, or
/// [`NO_MEET`]: see [`arm_listed_meet`]. A wait reads it once a pass, a
/// relaxed load, and a task never armed never waits there.
static LISTED_MEET: AtomicU64 = AtomicU64::new(NO_MEET);

/// Where the armed task is in the meet: [`MEET_ARRIVED`] once it is there,
/// [`MEET_RELEASED`] once the check lets it go on.
static LISTED_MEET_STAGE: AtomicU32 = AtomicU32::new(0);

/// See [`LISTED_MEET_STAGE`].
const MEET_ARRIVED: u32 = 1;

/// See [`LISTED_MEET_STAGE`].
const MEET_RELEASED: u32 = 2;

/// How long a task waits in the meet for the check to let it go.
const LISTED_MEET_PATIENCE_NANOS: u64 = 2_000_000_000;

/// Hold `task`'s next wait at the point just after it is listed, until
/// [`release_listed_meet`], for the boot check that a wake draining it
/// there does not lose it (`sched::check`, L.sched.73): the window between
/// the listing and the `BLOCKED` mark that `wait_until_deadline` closes by
/// marking it under the list's lock. Before the task waits; one wait only.
/// Only `sched::check` calls it: nothing else may hold a wait there.
pub(crate) fn arm_listed_meet(task: super::TaskId) {
    LISTED_MEET_STAGE.store(0, Ordering::Release);
    LISTED_MEET.store(task, Ordering::Release);
}

/// Whether the armed task has reached the meet: listed, and held there.
pub(crate) fn listed_meet_arrived() -> bool {
    LISTED_MEET_STAGE.load(Ordering::Acquire) == MEET_ARRIVED
}

/// Let the task held in the meet go on, and disarm it.
pub(crate) fn release_listed_meet() {
    LISTED_MEET.store(NO_MEET, Ordering::Release);
    LISTED_MEET_STAGE.store(MEET_RELEASED, Ordering::Release);
}

/// The meet itself, for `task` just listed: nothing unless a check armed it
/// for this task, and then a spin until the check lets it go, bounded by
/// [`LISTED_MEET_PATIENCE_NANOS`]. It spins, holding nothing but what the
/// wait holds there, so the check must run on another processor.
fn listed_meet(task: super::TaskId) {
    if LISTED_MEET.load(Ordering::Relaxed) != task {
        return;
    }
    LISTED_MEET.store(NO_MEET, Ordering::Relaxed);
    LISTED_MEET_STAGE.store(MEET_ARRIVED, Ordering::Release);
    let until = crate::timer::now_nanos().saturating_add(LISTED_MEET_PATIENCE_NANOS);
    while LISTED_MEET_STAGE.load(Ordering::Acquire) != MEET_RELEASED
        && crate::timer::now_nanos() < until
    {
        core::hint::spin_loop();
    }
}

/// The task a boot check holds in its next wake just before the wake's
/// waiter-lock section, or [`NO_MEET`]: see [`arm_hook_meet`]. A wake reads
/// it once, a relaxed load, and a task never armed never waits there.
static HOOK_MEET: AtomicU64 = AtomicU64::new(NO_MEET);

/// The queue whose wake [`HOOK_MEET`]'s task is held in.
static HOOK_MEET_QUEUE: AtomicUsize = AtomicUsize::new(0);

/// Where the armed waker is in the meet, as [`LISTED_MEET_STAGE`] is.
static HOOK_MEET_STAGE: AtomicU32 = AtomicU32::new(0);

/// Hold `task`'s next wake of `queue` just before its waiter-lock section,
/// until [`release_hook_meet`], for the boot check that a mark hooked
/// meanwhile is told by that wake (`sched::mark_check`, L.sched.75). One
/// wake only. Only `sched::mark_check` calls it.
pub(crate) fn arm_hook_meet(task: super::TaskId, queue: &WaitQueue) {
    HOOK_MEET_STAGE.store(0, Ordering::Release);
    HOOK_MEET_QUEUE.store(core::ptr::from_ref(queue).addr(), Ordering::Release);
    HOOK_MEET.store(task, Ordering::Release);
}

/// Whether the armed waker has reached the meet.
pub(crate) fn hook_meet_arrived() -> bool {
    HOOK_MEET_STAGE.load(Ordering::Acquire) == MEET_ARRIVED
}

/// Let the waker held in the meet go on, and disarm it.
pub(crate) fn release_hook_meet() {
    HOOK_MEET.store(NO_MEET, Ordering::Release);
    HOOK_MEET_STAGE.store(MEET_RELEASED, Ordering::Release);
}

/// The meet itself, in a wake of `queue`: nothing unless a check armed it
/// for the running task and this queue, and then a spin until the check
/// lets it go, bounded by [`LISTED_MEET_PATIENCE_NANOS`]. The check runs on
/// another processor.
fn hook_meet(queue: &WaitQueue) {
    let armed = HOOK_MEET.load(Ordering::Relaxed);
    if armed == NO_MEET
        || HOOK_MEET_QUEUE.load(Ordering::Relaxed) != core::ptr::from_ref(queue).addr()
        || super::current().is_none_or(|task| task.id != armed)
    {
        return;
    }
    HOOK_MEET.store(NO_MEET, Ordering::Relaxed);
    HOOK_MEET_STAGE.store(MEET_ARRIVED, Ordering::Release);
    let until = crate::timer::now_nanos().saturating_add(LISTED_MEET_PATIENCE_NANOS);
    while HOOK_MEET_STAGE.load(Ordering::Acquire) != MEET_RELEASED
        && crate::timer::now_nanos() < until
    {
        core::hint::spin_loop();
    }
}

/// No switch from a wait's `set_state(BLOCKED)` to the end of its last look
/// (F-69, `docs/roadmap/stage-05-tasks-scheduler.md`).
///
/// `choose_next` takes a current task that is not runnable off its queue,
/// so a switch inside that window counts as the task's block. If a wake had
/// drained its entry while it was still runnable, a drain that did nothing,
/// then nobody is left to wake it. The task stays blocked on no queue and on
/// no list, with what it waited for already there. ipc-bench stalled so on
/// ARMv7-A.
///
/// The preemption count is raised, not the interrupt mask. Interrupts are
/// taken, and a decision one of them asks for waits for the drop:
/// - if the look found the condition, the task is runnable by then and the
///   switch is a preemption;
/// - if it did not, the task is listed or filed, and the switch is the block
///   that comes next anyway.
///
/// So `ready` runs unpreemptible and must not block. One that blocks stops
/// the machine with FX-0503, where it would otherwise corrupt the outer wait.
#[must_use = "dropping it at once leaves the last look preemptible"]
pub(crate) struct LastLook(());

impl LastLook {
    /// Hold preemption off from here, before the task is marked blocked.
    /// The wait's own line is the site FX-0503 names if `ready` blocks.
    #[track_caller]
    pub(crate) fn hold() -> LastLook {
        if LAST_LOOK_HOLDS {
            super::preempt::preempt_disable();
        }
        LastLook(())
    }

    /// The look did not find the condition: block. The hold is let go inside
    /// the switch's own interrupt mask, with no decision at the lowering,
    /// since the switch is the decision (`super::block_ending_hold`).
    pub(crate) fn block(self) {
        // Its drop would lower the count and decide; this lowers it below.
        let _ = core::mem::ManuallyDrop::new(self);
        if LAST_LOOK_HOLDS {
            super::block_ending_hold();
        } else {
            super::block();
        }
    }
}

impl Drop for LastLook {
    /// The last look is over: the task is runnable again, or listed and
    /// about to block. Makes any decision an interrupt asked for meanwhile.
    fn drop(&mut self) {
        if LAST_LOOK_HOLDS {
            super::preempt::preempt_enable();
        }
    }
}

/// Tasks waiting for one thing.
///
/// **A wait's `ready` must not block.** Its last call runs under
/// [`LastLook`], with the preemption count raised, so it may not:
/// - take a `SleepLock`;
/// - wait for memory;
/// - touch user memory;
/// - wait on anything itself.
///
/// Spin locks and atomics are what it may use. Every `ready`, and every
/// `poll` that `poll`, `select` and `epoll_wait` reach through one, was
/// audited against this when F-69 was fixed.
#[derive(Debug)]
pub(crate) struct WaitQueue {
    /// Who is waiting.
    ///
    /// **Masking interrupts, so that a handler may wake the queue and a
    /// holder cannot be switched out.** A bound interrupt's handler queues
    /// its packet on a port and calls `wake_all` for the task waiting there,
    /// and a plain lock taken by a handler that interrupted its own holder
    /// would spin forever. The wait's correctness still rests on the order
    /// in `wait_until_deadline`, not on the lock. The other thing the masking
    /// buys is the reason it was added: a holder cannot be switched out with
    /// it held. The lock is a ticket lock, and a ticket
    /// lock hands itself to whoever is next in line whether or not that
    /// context is running. A holder preempted while holding it — a worker in
    /// `wake_all`, cut by the timer inside the few instructions the lock is
    /// held — stalls every waiter until it is scheduled again, and on a
    /// queue of five hundred tasks at the shortest slice that is nearly two
    /// hundred milliseconds. Worse, the waiters spin their slices away, are
    /// preempted holding *tickets*, and the lock then passes to each of them
    /// in turn, each hand-off costing another round of the queue.
    ///
    /// That was the thousand-task check taking twenty to fifty seconds one
    /// boot in three on two processors: the checker spinning here with
    /// interrupts on, on a processor that was tickless and never interrupted,
    /// while the workers finishing on the other processor queued for the same
    /// lock — two hundred thousand switches to run a thousand tasks that
    /// needed three thousand, and at the end a quarter of them finished but
    /// unable to leave. A holder with interrupts masked cannot be preempted,
    /// so the lock is held for exactly the instructions it covers and no
    /// convoy can form. Any plain `SpinLock` taken from a task with
    /// interrupts on is exposed to the same thing once it is contended; this
    /// is the one the scheduler's own check contends.
    waiters: IrqSpinLock<Vec<Arc<Task>>, arch::Irq>,
    /// Waits on this queue that a wake ended: `wake_all` took the task off the
    /// list, and the wait found what it was waiting for when it ran again --
    /// or at its last look before sleeping, when the wake came between the
    /// task listing itself and that look.
    ///
    /// For the checks, which need to tell a wake from the recheck without
    /// timing either. A task the recheck timer wakes is still on the list when
    /// it runs; one a waker woke is not, however long the processor took to
    /// run it -- so the count is a fact about the waker, not about the host.
    /// That holds only if the last look counts too. A check sends its event
    /// once it sees the waiter listed, and a waiter listed is one that has not
    /// yet looked a last time; on a loaded host its processor can stall there
    /// for milliseconds, the event lands, and the wait ends at the last look
    /// with nobody asleep. Uncounted, that read as the recheck having ended
    /// it, and failed the signalfd check (FX-0884) once in a loaded control of
    /// twenty `test-shell` runs on 2026-09-26.
    woken: AtomicU32,
    /// How many tasks are listed, stored under the list's lock whenever the
    /// list changes: what the fast path's T10 reads without taking the lock
    /// (`docs/OPAQUE-KERNEL.md` §9.7, "as built"). A reader without the lock
    /// may see a listing a moment late, which it takes as a waiter that
    /// listed itself after its look.
    count: AtomicUsize,
    /// How many times [`WaitQueue::wake_all`] has run, whether or not anyone
    /// was waiting: a number that moves whenever what the queue waits for may
    /// have changed, which is what `epoll`'s edge-triggered mode reads as an
    /// event.
    wakes: AtomicU64,
    /// What else every wake tells: see [`Mark`].
    ///
    /// Masking interrupts for the reason the waiter list does: a handler's
    /// wake calls them.
    ///
    /// Boxed, and only once a mark is first hooked: a queue is inside many
    /// objects, a channel's half and a disk's 64 completion slots among
    /// them, and nearly none is ever hooked.
    #[expect(
        clippy::box_collection,
        reason = "AUDIT: a thin pointer while no mark is hooked, which is nearly always"
    )]
    hooks: IrqSpinLock<Option<alloc::boxed::Box<Vec<Arc<Mark>>>>, arch::Irq>,
    /// How many marks are listed, stored under the *waiter* lock after the
    /// hook list changed and read by a wake under the same lock: the order
    /// the module doc's "Marks" gives, which is what lets a wake skip the
    /// hook lock when it is zero without missing a mark (L.sched.75).
    hooked: AtomicU32,
}

/// A [`Mark`]'s state: not on its list.
const IDLE: u8 = 0;
/// On its list, put there by a queue going, which woke nobody.
const QUIET: u8 = 1;
/// On its list, and the list's queue woken for it.
const WOKEN: u8 = 2;

/// Where marks put their numbers: an epoll set's ready list.
///
/// The list's room is reserved by its owner, one place for each mark it
/// made, before any can be told, so a mark never allocates; a mark that
/// finds no room flags the list overflowed instead, and the taker then looks
/// at everything (L.sched.76).
#[derive(Debug)]
pub(crate) struct ReadyList {
    /// The numbers of the marks told since the taker last took them, and
    /// whether the list's queue was woken for one of them since.
    serials: IrqSpinLock<Listed, arch::Irq>,
    /// A mark found no room.
    overflowed: AtomicBool,
    /// Woken when a mark is first told it was woken: what the list's owner
    /// sleeps on.
    queue: Arc<WaitQueue>,
    /// Every time a mark of this list was told anything.
    tells: AtomicU64,
}

impl ReadyList {
    /// An empty list with its own queue.
    ///
    /// # Errors
    ///
    /// [`fallible::AllocError`] for no memory.
    pub(crate) fn new() -> Result<Arc<ReadyList>, fallible::AllocError> {
        let queue = fallible::try_arc(WaitQueue::new())?;
        fallible::try_arc(ReadyList {
            serials: IrqSpinLock::new(Listed {
                serials: Vec::new(),
                woken: false,
            }),
            overflowed: AtomicBool::new(false),
            queue,
            tells: AtomicU64::new(0),
        })
    }

    /// The queue a wake of any of its marks wakes.
    pub(crate) fn queue(&self) -> &Arc<WaitQueue> {
        &self.queue
    }

    /// Room for at least `room` numbers, answering the room it has.
    ///
    /// # Errors
    ///
    /// [`fallible::AllocError`] for no memory: the room is as it was.
    pub(crate) fn reserve(&self, room: usize) -> Result<usize, fallible::AllocError> {
        let mut listed = self.serials.lock();
        let more = room.saturating_sub(listed.serials.len());
        fallible::try_reserve(&mut listed.serials, more)?;
        Ok(listed.serials.capacity())
    }

    /// Move the numbers told since the last take into `into`, as far as its
    /// room goes, and answer whether the list overflowed since then (or
    /// `into` had too little room): the caller then looks at every number it
    /// knows, and clears *every* mark it holds before it looks, since a mark
    /// that found the list full is marked and not on it. The marks stay on
    /// the list until the caller clears each ([`Mark::clear`]), which it does
    /// before it looks at what the number names.
    pub(crate) fn take(&self, into: &mut Vec<u64>) -> bool {
        let mut overflowed = false;
        {
            let mut listed = self.serials.lock();
            for &serial in listed.serials.iter() {
                overflowed |= fallible::push_within(into, serial).is_err();
            }
            listed.serials.clear();
            // The next mark told of a wake wakes the queue again.
            listed.woken = false;
        }
        self.overflowed.swap(false, Ordering::AcqRel) || overflowed
    }

    /// How many times its marks were told anything: a count that moves with
    /// every wake of a queue they are hooked on.
    pub(crate) fn tells(&self) -> u64 {
        self.tells.load(Ordering::Relaxed)
    }

    /// Fill the list's room with a number no mark has, for the check that a
    /// mark which then finds no room is not lost (`fs::epoll_check`): an
    /// overflow nothing else can reach, since a list's room is reserved for
    /// every mark that could be on it. Allocates nothing.
    pub(crate) fn crowd(&self) {
        let mut listed = self.serials.lock();
        while fallible::push_within(&mut listed.serials, u64::MAX).is_ok() {}
    }

    /// How many numbers are on it, for the checks.
    pub(crate) fn listed(&self) -> usize {
        self.serials.lock().serials.len()
    }
}

/// A ready list's numbers, and whether its queue was woken since they were
/// last taken.
#[derive(Debug)]
struct Listed {
    serials: Vec<u64>,
    woken: bool,
}

/// What a queue tells, besides waking its tasks: one number on one
/// [`ReadyList`].
#[derive(Debug)]
pub(crate) struct Mark {
    /// The number it puts on the list.
    serial: u64,
    /// [`IDLE`], [`QUIET`] or [`WOKEN`].
    state: AtomicU8,
    /// The list.
    list: Arc<ReadyList>,
}

impl Mark {
    /// A mark putting `serial` on `list`, on no queue yet.
    ///
    /// # Errors
    ///
    /// [`fallible::AllocError`] for no memory.
    pub(crate) fn new(
        list: &Arc<ReadyList>,
        serial: u64,
    ) -> Result<Arc<Mark>, fallible::AllocError> {
        fallible::try_arc(Mark {
            serial,
            state: AtomicU8::new(IDLE),
            list: Arc::clone(list),
        })
    }

    /// Off the list as far as its next telling knows: the taker's half, done
    /// after [`ReadyList::take`] and before it looks at what the number
    /// names. A swap, so that a telling which found the mark still listed --
    /// and so neither listed it nor woke anyone -- is ordered before the
    /// taker's look, which then sees its change.
    pub(crate) fn clear(&self) {
        let _ = self.state.swap(IDLE, Ordering::AcqRel);
    }

    /// Whether it is on its list, for the checks.
    pub(crate) fn is_listed(&self) -> bool {
        self.state.load(Ordering::Acquire) != IDLE
    }

    /// Told of a wake (`woken`) or of its queue going (L.sched.76): on the
    /// list once until cleared, with the list's leaf lock held only for the
    /// push; the list's queue woken, after that lock is let go, by the first
    /// wake told to any of its marks since the list was last taken -- never
    /// by a going. Allocates nothing and drops nothing.
    ///
    /// One wake a take is enough: a task waiting on the list's queue lists
    /// itself there before its last look takes the list, so a mark listed
    /// after an earlier wake drained it is taken by that look, and one listed
    /// after the look finds the flag the take cleared and wakes it.
    fn tell(&self, woken: bool) {
        let _ = self.list.tells.fetch_add(1, Ordering::Relaxed);
        let was = if woken {
            self.state.swap(WOKEN, Ordering::AcqRel)
        } else {
            match self
                .state
                .compare_exchange(IDLE, QUIET, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(was) | Err(was) => was,
            }
        };
        // Listed and its wake made since the list was taken: nothing to do.
        if was == WOKEN || (!woken && was != IDLE) {
            return;
        }
        let wake = {
            let mut listed = self.list.serials.lock();
            if was == IDLE && fallible::push_within(&mut listed.serials, self.serial).is_err() {
                self.list.overflowed.store(true, Ordering::Release);
            }
            let wake = woken && !listed.woken;
            if wake {
                listed.woken = true;
            }
            wake
        };
        if wake {
            self.list.queue.wake_all();
        }
    }
}

/// The fewest hooks a hook list keeps room for before it gives memory
/// back: see [`WaitQueue::unhook`].
const HOOKS_KEPT: usize = 8;

/// Count a wait ended on each of `queues` whose wake took the task off it.
fn count_wakes(queues: &[&WaitQueue], drained: &[bool]) {
    for (queue, was) in queues.iter().zip(drained) {
        if *was {
            let _ = queue.woken.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl WaitQueue {
    /// A queue with nobody on it.
    pub(crate) const fn new() -> WaitQueue {
        WaitQueue {
            waiters: IrqSpinLock::new(Vec::new()),
            woken: AtomicU32::new(0),
            count: AtomicUsize::new(0),
            wakes: AtomicU64::new(0),
            hooks: IrqSpinLock::new(None),
            hooked: AtomicU32::new(0),
        }
    }

    /// Have every wake of this queue tell `mark`, from now on: listed under
    /// the hook lock, then counted under the waiter lock, in the order the
    /// module doc's "Marks" argues. Every wake whose waiter-lock section
    /// begins after this returns tells it (L.sched.75), so a caller that
    /// looks at its condition after this sees every change no telling
    /// reports.
    ///
    /// # Errors
    ///
    /// [`fallible::AllocError`] when there is no memory to list it: the mark
    /// is not listed, and no wake tells it.
    pub(crate) fn hook(&self, mark: &Arc<Mark>) -> Result<(), fallible::AllocError> {
        let listed = {
            let mut hooks = self.hooks.lock();
            if hooks.is_none() {
                *hooks = Some(fallible::try_box(Vec::new())?);
            }
            let Some(list) = hooks.as_mut() else {
                return Err(fallible::AllocError);
            };
            fallible::try_push(list, Arc::clone(mark))?;
            list.len()
        };
        self.count_hooks(listed);
        Ok(())
    }

    /// Stop telling `mark`, which [`WaitQueue::hook`] listed: no wake whose
    /// hook lock is taken after this returns tells it (L.sched.74). A mark
    /// listed twice is taken off once.
    ///
    /// The entry is dropped under the hook lock, which is safe only because
    /// it is never the last reference: the caller holds `mark`. A list down
    /// to a quarter of its room gives half back, fallibly, so a queue that
    /// lives as long as the kernel (the net core's progress queue) does not
    /// keep the room of the most registrations it ever had; so the room is
    /// at most four places for each mark listed, past [`HOOKS_KEPT`], which
    /// is what an owner charges for a mark it hooks.
    pub(crate) fn unhook(&self, mark: &Arc<Mark>) {
        let (listed, given_back) = {
            let mut hooks = self.hooks.lock();
            let Some(list) = hooks.as_mut() else {
                return;
            };
            if let Some(at) = list.iter().position(|held| Arc::ptr_eq(held, mark)) {
                let _ = list.swap_remove(at);
            }
            let given_back = if list.is_empty() {
                // The last one gone: the box goes too.
                hooks.take().map(|list| (Some(list), None))
            } else if list.capacity() > HOOKS_KEPT && list.len() * 4 <= list.capacity() {
                match fallible::try_with_capacity(list.capacity() / 2) {
                    Ok(mut smaller) => {
                        // NOALLOC: half the room for at most a quarter of it.
                        smaller.append(list);
                        Some((None, Some(core::mem::replace(&mut **list, smaller))))
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            (hooks.as_ref().map_or(0, |list| list.len()), given_back)
        };
        drop(given_back);
        self.count_hooks(listed);
    }

    /// Store how many marks are listed, under the waiter lock: the second
    /// half of [`WaitQueue::hook`] and [`WaitQueue::unhook`].
    fn count_hooks(&self, listed: usize) {
        let listed = u32::try_from(listed).unwrap_or(u32::MAX);
        let _waiters = self.waiters.lock();
        self.hooked.store(listed, Ordering::Relaxed);
    }

    /// How many marks are listed, read without a lock: what a debug build's
    /// assertion on the fast path reads (`object::channel::sendable`).
    pub(crate) fn hooked_now(&self) -> u32 {
        self.hooked.load(Ordering::Relaxed)
    }

    /// How many marks are listed, for the checks.
    pub(crate) fn hooks(&self) -> usize {
        self.hooks.lock().as_ref().map_or(0, |list| list.len())
    }

    /// Tell every mark listed, under the hook lock.
    fn tell_marks(&self, woken: bool) {
        let hooks = self.hooks.lock();
        for mark in hooks.iter().flat_map(|list| list.iter()) {
            mark.tell(woken);
        }
    }

    /// Block until `ready` is true or `deadline` passes, whichever comes
    /// first. Answers whether `ready` was the reason.
    ///
    /// # Why a wait ever needs a deadline
    ///
    /// A wait woken only by a waker is the right shape for a kernel that
    /// works and the wrong one for a check trying to find out whether it
    /// does. A scheduler that has lost a task wakes nobody, and a wait with
    /// no deadline turns that into a boot test which says nothing for two
    /// minutes and then times out. With one, the same failure is a sentence
    /// naming what never happened — so there is only this form, and callers
    /// that genuinely never want to give up pass one far enough out to say
    /// so.
    ///
    /// The deadline is the sleep the scheduler already knows how to serve:
    /// the task goes on the run queue's sleeper set as well as on this queue,
    /// so the timer wakes it even when nothing else does. Being woken by the
    /// waker first leaves a stale entry in one of the two, and both tolerate
    /// it — waking a task that is already runnable is a no-op, and a sleeper
    /// whose deadline was cleared is dropped when it is next looked at.
    pub(crate) fn wait_until_deadline(&self, ready: impl FnMut() -> bool, deadline: u64) -> bool {
        self.wait_sliced(ready, deadline, Some(RECHECK_NANOS))
    }

    /// Block until `ready` is true, trusting this queue's wakes: no deadline,
    /// and no recheck while the task is listed.
    ///
    /// For a wait every change it waits for wakes, and that a round trip
    /// takes twice: `channel_write_read`'s, whose message and whose peer's
    /// close both wake the queue, as a signal and a kill wake the task
    /// itself. A recheck is a sleep deadline filed in the run queue's sleeper
    /// set as the task blocks and taken out again as it is woken, a fifth of
    /// a microsecond a wait for a timer that does not fire; `poll` and
    /// `epoll_wait` trust their queues the same way, with
    /// [`WaitQueue::wait_on_any`]'s long `recheck`. A task there was no
    /// memory to list is on no list a waker reads, and rechecks as every
    /// wait does (finding F-23).
    ///
    /// # Why no wake is missed
    ///
    /// The task is listed on this queue and marked blocked under the queue's
    /// lock, then `ready` is looked at, so a wake that drains its entry finds
    /// it blocked and makes it runnable, whatever `ready` then says. For each
    /// waker of `channel_write_read`'s wait:
    /// - **A message and the peer's close**, the end's state word. The writer
    ///   and the closer set it under the end's inbox lock and then take this
    ///   queue's lock to wake it; `ready` reads it with one load
    ///   (`readable_or_closed`). A drain before the listing passes the store
    ///   on through this queue's lock. A drain after it finds the task
    ///   marked blocked, since both are done under that lock, and wakes it;
    ///   the waker's `SeqCst` fence before its wake, paired with this one,
    ///   is no longer needed for this, and is kept: it dates from when the
    ///   mark came after the lock, and taking it out is a change of its own,
    ///   to be measured (`docs/OPAQUE-KERNEL.md` §9.8, 2e, condition 5).
    /// - **A kill and another thread's `execve`**, a fence pair. They take no
    ///   lock this wait takes: they post `END` to the process's tasks, which
    ///   `ready` reads, and wake them (`sched::work::wake_posted`), which
    ///   reads each task's state. That is a store then a load on each side,
    ///   so each side has a `SeqCst` fence between them: here after the task
    ///   is marked blocked, and in `wake_posted` after the bit and before
    ///   the state is read. Of two such fences one comes first, and the side
    ///   whose fence comes second sees the other's store: the waker finds the
    ///   task blocked and wakes it, or `ready` finds the process ending.
    pub(crate) fn wait_trusting(&self, ready: impl FnMut() -> bool) -> bool {
        self.wait_sliced(ready, u64::MAX, None)
    }

    /// [`WaitQueue::wait_trusting`] until `deadline`: no recheck while the
    /// task is listed, only the deadline's own sleep. Answers whether `ready`
    /// was the reason.
    ///
    /// For a wait whose every change wakes this queue, under the same rule
    /// [`WaitQueue::wait_trusting`] gives, but which a caller may also bound:
    /// `port_wait`'s. A driver idles there with no deadline, and a 5 ms
    /// recheck woke every one of them 200 times a second for nothing
    /// (backlog p2-138). A task there was no memory to list rechecks every
    /// [`RECHECK_NANOS`] as every wait does (finding F-23), whatever the
    /// deadline.
    pub(crate) fn wait_trusting_until(&self, ready: impl FnMut() -> bool, deadline: u64) -> bool {
        self.wait_sliced(ready, deadline, None)
    }

    /// [`WaitQueue::wait_until_deadline`], looking again every `recheck`
    /// nanoseconds, or only when woken for `None`.
    fn wait_sliced(
        &self,
        mut ready: impl FnMut() -> bool,
        deadline: u64,
        recheck: Option<u64>,
    ) -> bool {
        // Whether the last sleep ended because a waker took this task off the
        // list, for the count `waits_ended_by_a_wake` reports.
        let mut drained = false;
        loop {
            if ready() {
                if drained {
                    let _ = self.woken.fetch_add(1, Ordering::Relaxed);
                }
                return true;
            }
            if crate::timer::now_nanos() >= deadline {
                return false;
            }

            let Some(task) = super::current() else {
                // No scheduler yet, so there is nothing to switch to and
                // nothing to be woken by: spin, and let the deadline above
                // end it.
                core::hint::spin_loop();
                continue;
            };

            // **Sleep in slices, not in one span to the deadline.** A wait
            // queue only ends a wait early if somebody notifies it, and a
            // caller that forgets is not detectable from here — the wait
            // simply costs its whole budget and then succeeds. Waking
            // periodically turns that mistake from a twenty-second silent tax
            // into a few milliseconds, which is the difference between a bug
            // that hides for a day and one that never matters.
            //
            // Polling a condition is the wrong shape for a kernel and the
            // right one for a checking harness, which is all this serves.
            let slice = recheck.map_or(u64::MAX, |recheck| {
                crate::timer::now_nanos().saturating_add(recheck)
            });
            let wake_at = if slice < deadline { slice } else { deadline };

            // **Findable at every instant, which fixes the order.** A switch
            // that finds the task `BLOCKED` takes it off the run queue and
            // files it as a sleeper only if it has a deadline. So `BLOCKED` is
            // set last, once the deadline and the waiter entry both exist;
            // set first, a switch between the lines lost the task: blocked,
            // no deadline to wake it, on no list a waker reads. Under the
            // hold below no interrupt exit switches it before its block, but
            // the order is kept so that nothing depends on the hold alone.
            //
            // With no memory to list it, it is not listed, and sleeps to the
            // slice's end instead of until a wake: the condition is looked at
            // then as it would be anyway (finding F-23). A wait that trusts
            // its wakes files no deadline while it is listed, and a recheck's
            // when it could not be.
            //
            // **And no switch from `BLOCKED` to the end of the last look**,
            // so the hold comes first (`LastLook`, F-69).
            let last_look = LastLook::hold();
            // **Blocked before the list's lock is let go.** A waker drains
            // the list under that lock and then wakes what it took, so it
            // finds every entry it takes already marked blocked, and its wake
            // makes the task runnable. Marked after the lock, a drain in
            // between found the task runnable and did nothing: a wake whose
            // change did not yet satisfy `ready` -- one 1 KiB read of a full
            // pipe, for a writer of 4 KiB -- then left it blocked on no list,
            // and every later wake missed it. On a trusting wait that was for
            // good: `wget | head` stopped with both asleep (pipe-epipe).
            {
                let mut waiters = self.waiters.lock();
                let listed = fallible::try_push(&mut waiters, Arc::clone(&task)).is_ok();
                self.count.store(waiters.len(), Ordering::Release);
                // Unlisted, it rechecks whatever its deadline: a trusting
                // wait with a far deadline would otherwise sleep to it.
                let wake_at = if listed {
                    wake_at
                } else {
                    wake_at.min(crate::timer::now_nanos().saturating_add(RECHECK_NANOS))
                };
                if wake_at != u64::MAX {
                    task.set_sleep_deadline(wake_at);
                }
                task.set_state(BLOCKED);
            }
            listed_meet(task.id);
            // A wait that trusts its wakes has no recheck to find a wake it
            // missed, so the store above is ordered before `ready`'s loads
            // for the wakers that take no lock this wait takes: a kill and
            // an `execve`, whose own fence is in `sched::work::wake_posted`.
            if recheck.is_none() {
                core::sync::atomic::fence(Ordering::SeqCst);
            }

            // The last look, now that both a waker and the timer could find
            // us. Cancelling the sleep as well as the block, so a deadline
            // this task never used cannot wake it out of some later wait, and
            // in the reverse order for the same reason: runnable first, so a
            // switch in between leaves the task where it is. A waker that took
            // the task off the list meanwhile ended this wait, and is counted
            // as one that found it asleep would be.
            if ready() {
                task.set_state(RUNNABLE);
                drop(last_look);
                let _ = task.take_sleep_deadline();
                if !self.unqueue(task.id) {
                    let _ = self.woken.fetch_add(1, Ordering::Relaxed);
                }
                return true;
            }
            last_look.block();
            // Running again, so not filed anywhere: whatever deadline is left
            // belongs to no sleep and must not reach the next switch.
            let _ = task.take_sleep_deadline();

            // **Off the queue on the way out, however we left.** A waiter that
            // returns while still listed is woken by the *next* `wake_all`,
            // out of whatever it happens to be doing then — which showed up as
            // the sleep check failing with "a sleep came back before its
            // deadline", a task cut short of a sleep it had nothing to do with
            // this queue for. `wake_all` drains the list, so this only matters
            // for the paths that leave without being drained: the recheck
            // timer, and the condition coming true.
            drained = !self.unqueue(task.id);
            super::trip::slept(self, drained);
        }
    }

    /// Block until `ready` is true or `deadline` passes, woken by a wake of
    /// any of `queues`, and otherwise looking again every `recheck`
    /// nanoseconds. Answers whether `ready` was the reason.
    ///
    /// [`WaitQueue::wait_until_deadline`] for several queues at once, in the
    /// same order and for the same reasons: on every queue and the deadline
    /// set before the task is marked blocked, the last look after, and off
    /// every queue on the way out. A wake of one queue leaves the task listed
    /// on the others until then, and a second wake of a task already runnable
    /// changes nothing. A wait that one of the queues' wakes ended counts on
    /// that queue, as a single queue's does.
    ///
    /// `recheck` is how long the wait trusts its queues. `poll` and
    /// `epoll_wait` pass a long one when every file they watch wakes a queue
    /// on every change, so a program waiting on quiet files is not woken
    /// every few milliseconds to find them still quiet.
    pub(crate) fn wait_on_any(
        queues: &[&WaitQueue],
        mut ready: impl FnMut() -> bool,
        deadline: u64,
        recheck: u64,
    ) -> bool {
        // Which queues' wakes ended the last sleep, for the counts: without
        // memory for it, none are counted, and nothing else changes.
        let mut drained: Vec<bool> = fallible::try_filled(false, queues.len()).unwrap_or_default();
        loop {
            if ready() {
                count_wakes(queues, &drained);
                return true;
            }
            if crate::timer::now_nanos() >= deadline {
                return false;
            }
            let Some(task) = super::current() else {
                core::hint::spin_loop();
                continue;
            };
            let slice = crate::timer::now_nanos().saturating_add(recheck);
            let wake_at = if slice < deadline { slice } else { deadline };
            task.set_sleep_deadline(wake_at);
            // As the single queue's: no switch until the last look is over,
            // and blocked before any queue lists it, so that a waker finds
            // every entry it drains marked blocked. Marked after the
            // listings, a drain of one queue in between did nothing, and a
            // wake that did not yet satisfy `ready` cost a recheck.
            let last_look = LastLook::hold();
            task.set_state(BLOCKED);
            // Not listed on a queue there is no memory to list it on, as in
            // `wait_until_deadline`: that queue's wake is missed and the
            // recheck finds what it would have said.
            for queue in queues {
                let mut waiters = queue.waiters.lock();
                let _ = fallible::try_push(&mut waiters, Arc::clone(&task));
                queue.count.store(waiters.len(), Ordering::Release);
            }
            if ready() {
                task.set_state(RUNNABLE);
                drop(last_look);
                let _ = task.take_sleep_deadline();
                // Counted where a waker got there first, as the single
                // queue's last look counts.
                for (queue, was) in queues.iter().zip(drained.iter_mut()) {
                    *was = !queue.unqueue(task.id);
                }
                count_wakes(queues, &drained);
                return true;
            }
            last_look.block();
            let _ = task.take_sleep_deadline();
            for (index, queue) in queues.iter().enumerate() {
                let was = !queue.unqueue(task.id);
                if let Some(slot) = drained.get_mut(index) {
                    *slot = was;
                }
            }
        }
    }

    /// Take `id` off the waiter list, and say whether it was on it.
    fn unqueue(&self, id: super::TaskId) -> bool {
        let mut waiters = self.waiters.lock();
        let listed = waiters.len();
        waiters.retain(|waiter| waiter.id != id);
        self.count.store(waiters.len(), Ordering::Release);
        waiters.len() != listed
    }

    /// How many waits on this queue a wake has ended: see the field.
    pub(crate) fn waits_ended_by_a_wake(&self) -> u32 {
        self.woken.load(Ordering::Relaxed)
    }

    /// How many tasks are listed on the queue now, for the checks: one that
    /// means to show a waiter was ended by a wake has to know the waiter got
    /// onto the queue before the wake, not guess it from how long it gave it.
    pub(crate) fn listed(&self) -> usize {
        self.waiters.lock().len()
    }

    /// Count a wait on this queue's object ended by a wake that did not go
    /// through the list: a reader parked by the fast path, which a general
    /// write or close took off its record and woke
    /// (`docs/OPAQUE-KERNEL.md` §9.7).
    pub(crate) fn note_ended_by_a_wake(&self) {
        let _ = self.woken.fetch_add(1, Ordering::Relaxed);
    }

    /// How many tasks are listed, read without the list's lock: the fast
    /// path's T10 (`docs/OPAQUE-KERNEL.md` §9.7). See the field.
    pub(crate) fn listed_now(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// How many times the queue has been woken: see the field.
    pub(crate) fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::Relaxed)
    }

    /// Wake everything waiting.
    pub(crate) fn wake_all(&self) {
        self.wake_all_with(super::Wake::Home);
    }

    /// Wake everything waiting, each as `how` allows: see
    /// [`super::wake_with`]. At most the first can find room on the waker's
    /// processor; the rest are woken where they are.
    ///
    /// # Kept, not given away
    ///
    /// The list keeps its buffer. Taking the whole `Vec` left the queue an
    /// empty one, so the next waiter's push allocated a buffer and this wake
    /// freed it: an allocation and a free for every wait, on the path a
    /// round trip between two programs takes twice. Up to [`WAKE_BATCH`]
    /// waiters, the common case by far, are moved out by value and the
    /// buffer stays; a longer list is taken whole, as before.
    pub(crate) fn wake_all_with(&self, how: super::Wake) {
        let _ = self.wakes.fetch_add(1, Ordering::Relaxed);
        let mut few: [Option<Arc<Task>>; WAKE_BATCH] = [const { None }; WAKE_BATCH];
        let hooked_now = || self.hooked.load(Ordering::Relaxed);
        hook_meet(self);
        let (many, hooked) = {
            let mut waiters = self.waiters.lock();
            let taken = if waiters.len() <= WAKE_BATCH {
                for (slot, task) in few.iter_mut().zip(waiters.drain(..)) {
                    *slot = Some(task);
                }
                Vec::new()
            } else {
                core::mem::take(&mut *waiters)
            };
            self.count.store(0, Ordering::Release);
            // Read under the waiter lock, where `hook` stores it: the
            // module doc's "Marks" (L.sched.75).
            (taken, hooked_now())
        };
        for task in few.iter().flatten().chain(&many) {
            super::wake_with(task, how);
        }
        if hooked != 0 {
            self.tell_marks(true);
        }
    }
}

/// A queue going tells its marks so, and wakes nothing (L.sched.74): an
/// epoll registration whose file held the queue is then dropped at its set's
/// next wait, rather than at its once-a-second rescan.
impl Drop for WaitQueue {
    fn drop(&mut self) {
        if *self.hooked.get_mut() != 0 {
            self.tell_marks(false);
        }
    }
}

/// A wait queue is what the kernel lends a `ferrix_sync::SleepLock` to sleep
/// on, through `sync::SchedParker`.
///
/// `park_until` is `wait_until_deadline` with no deadline worth the name: the
/// lock's release wakes the queue, and the 5 ms recheck only costs a waiter
/// one more look at the flag. The lock loops on every return, so the early
/// returns the recheck makes are the "spurious wakes" the `Parking` contract
/// allows. The order that closes the lost wake-up -- on the queue and marked
/// blocked before the last look at `ready` -- is the one above, and it is
/// what the contract asks of a parking.
impl ferrix_sync::Parking for WaitQueue {
    fn park_until(&self, ready: &mut dyn FnMut() -> bool) {
        let _ = self.wait_until_deadline(ready, u64::MAX);
    }

    fn unpark_all(&self) {
        self.wake_all();
    }

    /// The rule for a sleeping lock — never with a spin lock held or
    /// preemption otherwise off, never with interrupts masked — checked on
    /// every acquisition. Blocking under the count is FX-0503 in `block`,
    /// but only a contended lock ever blocks; this catches the holder that
    /// took one under a spin lock and got away with it on a quiet boot.
    fn may_park(&self) {
        debug_assert!(
            !super::started() || super::may_block(),
            "a sleeping lock was taken where a task may not block: preemption off, interrupts masked, or no task"
        );
    }
}
