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

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ferrix_sync::{IrqControl, IrqSpinLock};

/// MEASUREMENT ONLY (os07-stall): spins added inside a trusting wait's two
/// windows, from `FERRIX_STALL_WIDEN` at build time; 0 adds none.
const STALL_WIDEN: u32 = parse_u32(option_env!("FERRIX_STALL_WIDEN"));
/// MEASUREMENT ONLY (os07-stall): `FERRIX_STALL_MASK=1` masks interrupts from
/// a trusting wait's `BLOCKED` through its last look and its switch.
const STALL_MASK: bool = parse_u32(option_env!("FERRIX_STALL_MASK")) == 1;

/// A decimal number, or 0.
const fn parse_u32(text: Option<&str>) -> u32 {
    let Some(text) = text else {
        return 0;
    };
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut value: u32 = 0;
    while at < bytes.len() {
        value = value * 10 + (bytes[at] - b'0') as u32;
        at += 1;
    }
    value
}

/// MEASUREMENT ONLY (os07-stall): the spin.
fn stall_widen() {
    let mut left = STALL_WIDEN;
    while left > 0 {
        core::hint::spin_loop();
        left = core::hint::black_box(left) - 1;
    }
}

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

/// Tasks waiting for one thing.
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
}

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
    /// The task is listed on this queue, then marked blocked, then `ready` is
    /// looked at, and a wake finds it only if it is listed and blocked. For
    /// each waker of `channel_write_read`'s wait:
    /// - **A message and the peer's close**, the end's state word. The writer
    ///   and the closer set it under the end's inbox lock and then take this
    ///   queue's lock to wake it; `ready` reads it with one load
    ///   (`readable_or_closed`). A drain before the listing passes the store
    ///   on through this queue's lock. A drain after it, with the task not
    ///   yet marked blocked, is a store then a load on each side, so the
    ///   waker makes a `SeqCst` fence before its wake, paired with this
    ///   one (`docs/OPAQUE-KERNEL.md` §9.8, 2e, condition 5).
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

            // **Findable at every instant, which fixes the order.** Interrupts
            // are on here, and any interrupt exit may switch this task out. A
            // switch that finds it `BLOCKED` takes it off the run queue and
            // files it as a sleeper only if it has a deadline. So `BLOCKED` is
            // set last, once the deadline and the waiter entry both exist.
            // Set first, a switch between the lines lost the task: blocked,
            // no deadline to wake it, on no list a waker reads.
            //
            // With no memory to list it, it is not listed, and sleeps to the
            // slice's end instead of until a wake: the condition is looked at
            // then as it would be anyway (finding F-23). A wait that trusts
            // its wakes files no deadline while it is listed, and a recheck's
            // when it could not be.
            let listed = {
                let mut waiters = self.waiters.lock();
                let listed = fallible::try_push(&mut waiters, Arc::clone(&task)).is_ok();
                self.count.store(waiters.len(), Ordering::Release);
                listed
            };
            if recheck.is_none() {
                stall_widen();
            }
            let wake_at = if listed || wake_at != u64::MAX {
                wake_at
            } else {
                crate::timer::now_nanos().saturating_add(RECHECK_NANOS)
            };
            if wake_at != u64::MAX {
                task.set_sleep_deadline(wake_at);
            }
            let masked = (STALL_MASK && recheck.is_none())
                .then(<arch::Irq as IrqControl>::disable);
            task.set_state(BLOCKED);
            // A wait that trusts its wakes has no recheck to find a wake it
            // missed, so the store above is ordered before `ready`'s loads
            // for the wakers that take no lock this wait takes: a kill and
            // an `execve`, whose own fence is in `sched::work::wake_posted`.
            if recheck.is_none() {
                stall_widen();
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
                if let Some(saved) = masked {
                    <arch::Irq as IrqControl>::restore(saved);
                }
                let _ = task.take_sleep_deadline();
                if !self.unqueue(task.id) {
                    let _ = self.woken.fetch_add(1, Ordering::Relaxed);
                }
                return true;
            }
            super::block();
            if let Some(saved) = masked {
                <arch::Irq as IrqControl>::restore(saved);
            }
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
            // Not listed on a queue there is no memory to list it on, as in
            // `wait_until_deadline`: that queue's wake is missed and the
            // recheck finds what it would have said.
            for queue in queues {
                let mut waiters = queue.waiters.lock();
                let _ = fallible::try_push(&mut waiters, Arc::clone(&task));
                queue.count.store(waiters.len(), Ordering::Release);
            }
            task.set_state(BLOCKED);
            if ready() {
                task.set_state(RUNNABLE);
                let _ = task.take_sleep_deadline();
                // Counted where a waker got there first, as the single
                // queue's last look counts.
                for (queue, was) in queues.iter().zip(drained.iter_mut()) {
                    *was = !queue.unqueue(task.id);
                }
                count_wakes(queues, &drained);
                return true;
            }
            super::block();
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
        let many = {
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
            taken
        };
        for task in few.iter().flatten().chain(&many) {
            super::wake_with(task, how);
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
