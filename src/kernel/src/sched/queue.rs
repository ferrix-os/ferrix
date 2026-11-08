//! One CPU's run queue: what is running there, what is waiting, and when the
//! timer should next interrupt it.
//!
//! # The lock
//!
//! One plain [`SpinLock`] per CPU, always taken with interrupts already
//! masked — never an `IrqSpinLock`, because a context switch hands the lock
//! from the outgoing context to the incoming one and a guard cannot cross
//! that. The rule it replaces the guard with is written at each call site: the
//! lock is taken by `lock_manually` and released by exactly one
//! `force_unlock`, either by the same context when no switch happened or by
//! the context switched to.
//!
//! Order against the rest of the kernel's locks: a run queue's lock is taken
//! *outside* the heap's and the frame allocator's — placing an entity in the
//! tree allocates — and nothing is ever taken outside it. Nothing here maps
//! memory, so `mm`'s table lock never appears under it.
//!
//! # Tickless
//!
//! The timer is armed for the moment this CPU next has a decision to make:
//! the end of the running task's slice, or the first sleeper's wake-up,
//! whichever comes first. A CPU running one task with nothing queued behind it
//! arms nothing at all, because there is no decision to make until something
//! else happens — which is what "tickless" means and why the timer facade's
//! one-shot is the primitive stage 3 left behind.

use alloc::sync::Arc;

use ferrix_sched_class::{CpuLoad, Load, RtParams, SchedClass};
use ferrix_sched_fair::{Config, FairTier, StackTiers, Timeline, slice_for};
#[cfg(ferrix_fast_ipc)]
use ferrix_sched_fair::{JobPlan, settle_job_fold};

use super::stack::{Stack, StackQueue};

use super::task::{BLOCKED, RUNNABLE, SleepSlot, Task, TaskId};
use crate::object::timer::Armed;

// CHECKS-ONLY: object/timer_check.rs; home: in crate
/// What a processor's sleeper set holds: one set for every deadline the
/// processor keeps, so that one hardware one-shot, armed by
/// [`CpuQueue::arm_timer`] for the set's first key, serves them all
/// (`docs/microkernel/K10.md` §2.1).
#[derive(Debug)]
#[doc(hidden)]
pub enum Due {
    /// A sleeping task, made runnable at its instant.
    Wake(Arc<Task>),
    /// A timer object's arm, keyed by the latest instant of its window.
    Fire(Armed),
}

// CHECKS-ONLY: object/timer_check.rs; home: in crate
/// The most timer arms one processor holds between being taken off its
/// sleeper set and being fired by its timer interrupt, and the most a pass
/// re-files when that is full (`L.object.218`).
#[doc(hidden)]
pub const FIRE_BATCH: usize = 16;

/// The most a task under a `cpu.max` runs before the quota is looked at
/// again, however much is left: with `n` tasks of one job running at once on
/// `n` processors the quota goes `n` times faster than one task's look at it
/// would say. Linux hands each processor a slice of the quota at a time for
/// the same reason. One millisecond bounds the overshoot to about a
/// millisecond a processor a period.
pub(crate) const BANDWIDTH_SLICE_NS: u64 = 1_000_000;

/// How much CPU a task asks for at a time.
///
/// Three milliseconds: long enough that a switch costs a fraction of a per
/// cent of it under an emulator, short enough that a task waiting behind
/// three others still runs within a human's idea of immediately. It is also
/// the unit of the fairness bound — no task strays further than one of these
/// from its share — so it is the number stage 5's exit criterion is stated
/// in.
pub(crate) const TARGET_LATENCY_NS: u64 = 3_000_000;

/// The shortest slice the scheduler will hand out, however many tasks are
/// runnable.
///
/// Below this a processor spends more time switching than running. Linux calls
/// the same number `sched_min_granularity`, and the ratio between it and the
/// target latency is what decides how many runnable tasks it takes before
/// latency has to give: eight, here.
pub(crate) const MIN_SLICE_NS: u64 = TARGET_LATENCY_NS / 8;

/// The largest slice, which is what one runnable task gets and what the
/// fairness bound is stated in.
pub(crate) const SLICE_NS: u64 = TARGET_LATENCY_NS;

/// The shortest interval worth arming the timer for: below this the interrupt
/// costs more than the time it measures.
pub(crate) const MIN_ARM_NS: u64 = 20_000;

/// What one CPU's scheduling has done, for the boot report.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stats {
    /// Context switches.
    pub(crate) switches: u64,
    /// Tasks taken from another CPU's queue.
    pub(crate) stolen_in: u64,
    /// Tasks another CPU took from this one.
    pub(crate) stolen_out: u64,
    /// The most any task ran past its deadline before being switched out.
    pub(crate) worst_overrun: u64,
    /// Every overrun served while the window was open, added up. What the
    /// fairness bound has to allow for: each is time one task was charged
    /// without the scheduler having chosen to give it, and in the worst case
    /// they all landed on the same task.
    pub(crate) overrun_total: u64,
    /// The most any task's service strayed from its share while a measurement
    /// window was open.
    pub(crate) worst_lag: u64,
    /// Whether that window is open.
    pub(crate) measuring: bool,
    /// Picks made while it was open.
    pub(crate) picks: u64,
    /// Of those, picks that a scan of the queue would have made differently:
    /// an eligible entity with an earlier deadline was passed over. Zero on a
    /// queue whose tree is right; see [`CpuQueue::note_pick`].
    pub(crate) wrong_picks: u64,
    /// Nanoseconds charged while a task other than the idle task was running.
    pub(crate) busy_ns: u64,
    /// Nanoseconds charged while the idle task was.
    pub(crate) idle_ns: u64,
}

/// How many recent picks a queue remembers while a window is open.
pub(crate) const TRACE_PICKS: usize = 12;

/// How many entities one remembered pick records.
pub(crate) const TRACE_ENTITIES: usize = 5;

/// One entity as a pick saw it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Seen {
    /// Its identifier.
    pub(crate) id: TaskId,
    /// Its virtual runtime.
    pub(crate) vruntime: u64,
    /// Its deadline.
    pub(crate) deadline: u64,
    /// Its lag: non-negative is eligible.
    pub(crate) lag: i64,
}

/// One scheduling decision, remembered for a fairness failure to explain.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pick {
    /// When.
    pub(crate) at: u64,
    /// What the fair class chose.
    pub(crate) picked: TaskId,
    /// What a scan would have chosen.
    pub(crate) scanned: TaskId,
    /// The queue's virtual time.
    pub(crate) avg: u64,
    /// Every entity, running one first.
    pub(crate) seen: [Seen; TRACE_ENTITIES],
    /// How many of `seen` are filled.
    pub(crate) count: usize,
}

// CHECKS-ONLY: object/timer_check.rs; home: in crate
/// Timer arms a pass took off a sleeper set and the timer interrupt has not
/// fired yet: at most [`FIRE_BATCH`], in a fixed array, so that a pass
/// allocates nothing.
#[derive(Debug)]
#[doc(hidden)]
pub struct Fired {
    /// The arms and their slots; the first `len` are full.
    pub slots: [Option<(Armed, SleepSlot)>; FIRE_BATCH],
    /// How many are full.
    pub len: usize,
}

impl Default for Fired {
    fn default() -> Fired {
        Fired::new()
    }
}

impl Fired {
    /// None.
    pub const fn new() -> Fired {
        Fired {
            slots: [const { None }; FIRE_BATCH],
            len: 0,
        }
    }

    /// Every arm held, emptying it.
    pub fn take(&mut self) -> [Option<(Armed, SleepSlot)>; FIRE_BATCH] {
        self.len = 0;
        core::mem::replace(&mut self.slots, [const { None }; FIRE_BATCH])
    }
}

// CHECKS-ONLY: object/timer_check.rs; home: in crate
/// One pass's next step over `sleepers` at `now`: entries leave in key
/// order while the first is due. A timer's arm goes into `fired`, which only
/// the timer interrupt empties: a decision holds the run queue's lock across
/// its switch, and a fire wakes waiters, which takes run queue locks. An arm
/// whose window has opened (its earliest instant passed) leaves too while it
/// is first and `fired` has room: that is how one interrupt serves every
/// timer at the head of the set whose window it falls in (`L.object.217`).
/// When `fired` is full a due arm is filed again [`MIN_ARM_NS`] from now, so
/// that the tasks behind it are still woken on time: timers never delay a
/// task's wake. At most [`FIRE_BATCH`] arms a pass are filed again
/// (`refiled` counts them across the pass's calls); past that the pass
/// stops, and the tasks behind wait for the next interrupt, armed
/// [`MIN_ARM_NS`] away (`docs/certification/MEMORY-AND-TIMING.md` §2.2ab).
///
/// Answers the next due sleeping task, taken out with its slot, for the
/// caller to wake; with `tasks` false it stops at one instead and takes it
/// out not. `None` when nothing more is due. A free function over the set
/// and the buffer so that stage 9's coalescing case runs this same code over
/// a set it built (`object::timer_check`).
#[doc(hidden)]
pub fn take_next(
    sleepers: &mut Timeline<Due>,
    fired: &mut Fired,
    now: u64,
    refiled: &mut usize,
    tasks: bool,
) -> Option<(Arc<Task>, SleepSlot)> {
    loop {
        let room = fired.len < FIRE_BATCH;
        let (key, due, wake) = sleepers.first().map(|(at, _, payload)| {
            let open = matches!(payload, Due::Fire(armed) if room && armed.earliest <= now);
            (at, at <= now || open, matches!(payload, Due::Wake(_)))
        })?;
        if !due || (wake && !tasks) {
            return None;
        }
        let (id, payload, slot) = sleepers.pop_first()?;
        match payload {
            Due::Wake(task) => return Some((task, slot)),
            Due::Fire(armed) => {
                let stop = place(sleepers, fired, (key, id), armed, slot, now, refiled);
                if stop {
                    return None;
                }
            }
        }
    }
}

/// Where [`take_next`] puts a due timer arm taken off the set under
/// `(key, id)`: into `fired` while it has room, else filed again
/// [`MIN_ARM_NS`] on, at most [`FIRE_BATCH`] a pass, and past that back
/// where it was. Answers whether the pass stops.
fn place(
    sleepers: &mut Timeline<Due>,
    fired: &mut Fired,
    (key, id): (u64, u64),
    armed: Armed,
    slot: SleepSlot,
    now: u64,
    refiled: &mut usize,
) -> bool {
    if fired.len < FIRE_BATCH {
        armed.timer.unfiled();
        if let Some(place) = fired.slots.get_mut(fired.len) {
            *place = Some((armed, slot));
            fired.len += 1;
        }
        return false;
    }
    let (at, stop) = if *refiled < FIRE_BATCH {
        *refiled += 1;
        (now.saturating_add(MIN_ARM_NS), false)
    } else {
        // Back where it was, first: nothing behind it is looked at this
        // pass.
        (key, true)
    };
    armed.timer.refiled(at);
    // NOALLOC: filed in the slot it was taken out with.
    sleepers.insert(at, id, Due::Fire(armed), slot);
    stop
}

/// Whether `a` is before `b` in wrapping virtual time.
const fn before(a: u64, b: u64) -> bool {
    (a.wrapping_sub(b) as i64) < 0
}

/// One CPU's queue.
#[derive(Debug)]
pub(crate) struct CpuQueue {
    /// The class stack's queue (`sched::stack`; today the fair class
    /// alone). The idle task is not in it: it is what runs when this is
    /// empty, which is what makes it the bottom of the class stack rather
    /// than a task with a very small weight.
    pub(crate) stack: StackQueue,
    /// What is running, which is the idle task when the fair class is empty.
    pub(crate) current: Option<Arc<Task>>,
    /// This CPU's idle task.
    pub(crate) idle: Option<Arc<Task>>,
    /// What was running before the last switch, for the incoming context to
    /// finish with.
    pub(crate) previous: Option<Arc<Task>>,
    /// Tasks asleep on this CPU, by the instant they wake, each in its own
    /// sleep slot, and timer objects' arms, by the latest instant of their
    /// windows, each in the slot its timer made: filing and waking allocate
    /// nothing.
    sleepers: Timeline<Due>,
    /// Timer arms taken off `sleepers` and not yet fired: filled by a pass
    /// ([`take_next`]), emptied by the timer interrupt
    /// ([`CpuQueue::take_fired`]).
    fired: Fired,
    /// How busy this processor has been, decaying.
    ///
    /// "Busy" means running something that is not the idle task. It is
    /// accumulated wherever time is already being accounted, so it costs a
    /// pair of adds rather than a pass of its own.
    load: Load,
    /// When [`Self::account_load`] last folded time into `load`.
    load_updated: u64,
    /// When the running task was last charged.
    pub(crate) exec_start: u64,
    /// What this CPU's scheduling has done.
    pub(crate) stats: Stats,
    /// The last few picks, while a window is open.
    pub(crate) trace: [Pick; TRACE_PICKS],
    /// Where the next one goes.
    pub(crate) trace_next: usize,
    /// What its charges have kept back of `cpu.stat`'s time, for the slots
    /// (`object::quota::Pending`): touched only under this queue's lock.
    pub(crate) pending: crate::object::quota::Pending,
}

impl CpuQueue {
    /// An empty queue.
    pub(crate) fn new() -> Result<CpuQueue, &'static str> {
        let stack = Stack::new_queue(<Stack as StackTiers<Arc<Task>>>::stack_config(Config {
            slice_ns: TARGET_LATENCY_NS,
        }))
        .map_err(|_| "the scheduler's slice is not a slice")?;
        Ok(CpuQueue {
            stack,
            current: None,
            idle: None,
            previous: None,
            sleepers: Timeline::new(),
            fired: Fired::new(),
            load: Load::new(),
            load_updated: 0,
            exec_start: 0,
            stats: Stats::default(),
            trace: [Pick::default(); TRACE_PICKS],
            trace_next: 0,
            pending: crate::object::quota::Pending::new(),
        })
    }

    /// Check the pick just made against a scan of the queue, and remember it.
    ///
    /// Only while a window is open, and only because the fairness check has
    /// failed with one of three pinned spinners skipped for tens of
    /// milliseconds while its peers alternated — the shape of an augmented
    /// tree whose minima are stale, which is a bug Linux's `pick_eevdf` has
    /// had more than once. The tree's own invariants are checked at the end
    /// of stage 5 and hold; this asks the question at the moment it matters,
    /// on every decision, against the definition: the eligible entity with
    /// the earliest deadline, or the earliest deadline outright when none is
    /// eligible. A disagreement is counted, and the last decisions are kept
    /// so that a failure prints what every entity looked like when it was
    /// passed over.
    pub(crate) fn note_pick(&mut self, now: u64) {
        let Some(picked) = Stack::fair(&self.stack).current_id() else {
            return;
        };
        let mut pick = Pick {
            at: now,
            picked,
            scanned: picked,
            avg: Stack::fair(&self.stack).avg_vruntime(),
            seen: [Seen::default(); TRACE_ENTITIES],
            count: 0,
        };
        let mut best_eligible: Option<Seen> = None;
        let mut best_any: Option<Seen> = None;
        Stack::fair(&self.stack).for_each(|view| {
            let seen = Seen {
                id: view.id,
                vruntime: view.vruntime,
                deadline: view.deadline,
                lag: view.lag,
            };
            if let Some(slot) = pick.seen.get_mut(pick.count) {
                *slot = seen;
                pick.count += 1;
            }
            if best_any.is_none_or(|best| before(seen.deadline, best.deadline)) {
                best_any = Some(seen);
            }
            if seen.lag >= 0
                && best_eligible.is_none_or(|best| before(seen.deadline, best.deadline))
            {
                best_eligible = Some(seen);
            }
        });
        if let Some(best) = best_eligible.or(best_any) {
            pick.scanned = best.id;
            if best.id != picked {
                self.stats.wrong_picks += 1;
            }
        }
        self.stats.picks += 1;
        if let Some(slot) = self.trace.get_mut(self.trace_next) {
            *slot = pick;
        }
        self.trace_next = (self.trace_next + 1) % TRACE_PICKS;
    }

    /// The remembered picks, oldest first.
    pub(crate) fn picks(&self) -> impl Iterator<Item = &Pick> {
        let (newer, older) = self.trace.split_at(self.trace_next);
        older.iter().chain(newer.iter()).filter(|pick| pick.at != 0)
    }

    /// Charge the running task for the time since it was last charged.
    ///
    /// Also where the overrun is measured: how far past its deadline a task
    /// ran before the timer cut it. That is the difference between the slice
    /// the scheduler asked for and the request it actually served, and it is
    /// what widens the fairness bound on a real machine.
    pub(crate) fn account(&mut self, now: u64) {
        self.account_in(now, false);
    }

    /// [`CpuQueue::account`], with the time since the last charge counted
    /// as used in user mode when `user`: the tick that cut it found the
    /// processor there. Tick accounting, as `cpu.stat` has had it since
    /// Linux was young.
    pub(crate) fn account_in(&mut self, now: u64, user: bool) {
        let _ = self.account_with(now, user, None);
    }

    /// [`CpuQueue::account_in`], its group share computed with `extra` added
    /// to the load of the job `extra` names (the direct switch's fold,
    /// `docs/OPAQUE-KERNEL.md` §9.11, J2). Answers the base weight and the
    /// weight its job's share gave the running task, where the share was
    /// looked at.
    pub(crate) fn account_with(
        &mut self,
        now: u64,
        user: bool,
        extra: Option<(u32, i64)>,
    ) -> Option<(u32, u32)> {
        // The deadline tier's time, before the charge (ledger 7763 DG.4).
        <Stack as StackTiers<Arc<Task>>>::note_time(&mut self.stack, now);
        self.account_load(now);
        let delta = now.saturating_sub(self.exec_start);
        self.exec_start = now;
        if delta == 0 {
            return None;
        }
        if let Some(task) = Stack::current(&self.stack) {
            self.stats.busy_ns += delta;
            // Its job's `cpu.stat`, and its `cpu.max` (`object::quota`).
            crate::object::quota::charge_cpu(&mut self.pending, task.group(), now, delta, user);
        } else {
            self.stats.idle_ns += delta;
        }
        let Some(remaining) = Stack::remaining_ns(&self.stack) else {
            // Nothing ran. The time passed all the same for a tier whose
            // budget is counted per period of real time (ledger 5684's C1).
            Stack::elapse(&mut self.stack, delta);
            return None;
        };
        if let Some(task) = Stack::current(&self.stack) {
            task.add_runtime(delta);
        }
        if Stack::charge(&mut self.stack, delta) {
            let overrun = delta.saturating_sub(remaining);
            self.stats.worst_overrun = self.stats.worst_overrun.max(overrun);
            if self.stats.measuring {
                self.stats.overrun_total = self.stats.overrun_total.saturating_add(overrun);
            }
        }
        let share = self.follow_group_share(extra);
        if self.stats.measuring {
            self.measure();
        }
        share
    }

    /// What is left of the running entity's slice or budget, as a charge
    /// reads it: for the real-time tier's floor check.
    pub(super) fn remaining_for_check(&self) -> Option<u64> {
        Stack::remaining_ns(&self.stack)
    }

    /// Give the running task the weight its job's share says it should have
    /// now, if that has moved by more than an eighth: the other tasks of its
    /// job came and went since it was queued (`object::quota::effective`).
    /// A task in no job other than the root never changes here. `extra` is
    /// [`CpuQueue::account_with`]'s. Answers the base weight and the weight
    /// the share gave, kept or not.
    fn follow_group_share(&mut self, extra: Option<(u32, i64)>) -> Option<(u32, u32)> {
        let task = Stack::current(&self.stack)?;
        if task.group() == crate::object::quota::NONE {
            return None;
        }
        let (id, now) = (task.id, task.entity_state().weight);
        let (base, due) = task.effective_weight_with(extra);
        if now.abs_diff(due) <= now / 8 {
            return Some((base, due));
        }
        task.set_weight(due);
        // The only refusal is a weight of zero, which `effective` never
        // answers.
        let _ = Stack::set_weight(&mut self.stack, id, due);
        Some((base, due))
    }

    /// Busy and idle nanoseconds up to `now`, charging nothing.
    ///
    /// The time since the last charge is added to whichever of the two
    /// [`Self::account`] would charge it to, so neither goes backwards between
    /// two reads however the charges fall between them. A queue whose
    /// processor has not joined the scheduler has nothing running, and
    /// counts nothing.
    pub(crate) fn time_spent(&self, now: u64) -> (u64, u64) {
        if self.current.is_none() {
            return (self.stats.busy_ns, self.stats.idle_ns);
        }
        let since = now.saturating_sub(self.exec_start);
        if Stack::current(&self.stack).is_some() {
            (self.stats.busy_ns.saturating_add(since), self.stats.idle_ns)
        } else {
            (self.stats.busy_ns, self.stats.idle_ns.saturating_add(since))
        }
    }

    /// Compare every task's service with its weighted share, and remember the
    /// worst difference.
    ///
    /// This is EEVDF's promise, measured in real nanoseconds on a running
    /// machine rather than in the scheduler's own virtual time: a task's lag
    /// is what it was owed less what it had, and the theorem says that stays
    /// inside one request. Measured from each task's baseline so that a task
    /// which joined the queue later is not asked to account for time before
    /// it arrived.
    fn measure(&mut self) {
        let mut total = 0u128;
        let mut weights = 0u128;
        Stack::fair(&self.stack).for_each(|view| {
            if view.payload.is_measured() {
                total += u128::from(view.payload.since_baseline(view.sum_exec));
                weights += u128::from(view.weight);
            }
        });
        if weights == 0 {
            return;
        }

        let mut worst = 0u64;
        Stack::fair(&self.stack).for_each(|view| {
            if !view.payload.is_measured() {
                return;
            }
            let share = total * u128::from(view.weight) / weights;
            let had = u128::from(view.payload.since_baseline(view.sum_exec));
            worst = worst.max(share.abs_diff(had) as u64);
        });
        self.stats.worst_lag = self.stats.worst_lag.max(worst);
    }

    /// Wake every task whose sleep has ended, each queued where `elsewhere`
    /// queues it -- an idle processor's queue (`sched::place_on_idle`) --
    /// or, when it answers `false`, here; and take every timer arm that is
    /// due off the sleeper set into the fired buffer ([`take_next`]): a
    /// decision's pass, under this lock, after the running task was charged
    /// (`choose_next`'s `account_in`).
    pub(crate) fn wake_sleepers(
        &mut self,
        now: u64,
        mut elsewhere: impl FnMut(&Arc<Task>) -> bool,
    ) {
        let mut refiled = 0_usize;
        while let Some((task, slot)) =
            take_next(&mut self.sleepers, &mut self.fired, now, &mut refiled, true)
        {
            task.set_filed_at(0);
            task.return_sleep_slot(slot);
            // Only a task still blocked is woken. One already woken some
            // other way is running or queued, and one that has exited must
            // never run again; for either, the entry is stale and is dropped.
            if task.state() != BLOCKED {
                continue;
            }
            task.set_state(RUNNABLE);
            if elsewhere(&task) {
                continue;
            }
            // NOALLOC: `CpuQueue::insert` queues the task in its own run slot.
            self.insert(&task);
        }
    }

    /// The timer interrupt's pass: take the timer arms due at the head of
    /// the sleeper set into the fired buffer, and stop at a sleeping task
    /// that is due. Tasks are woken by the decision this interrupt asks for,
    /// after `choose_next` has charged the task the interrupt cut, never
    /// here, where nothing has: waking one charges the running task
    /// (`CpuQueue::insert`), and from the interrupt that charge would be
    /// made without knowing whether the cut was in user mode. The arms
    /// behind such a task are taken by that decision's pass and fired by
    /// the interrupt it arms [`MIN_ARM_NS`] away (`L.object.218`).
    pub(crate) fn take_due_timers(&mut self, now: u64) {
        let mut refiled = 0_usize;
        let _ = take_next(
            &mut self.sleepers,
            &mut self.fired,
            now,
            &mut refiled,
            false,
        );
    }

    /// The timer arms a pass took off the sleeper set, with their slots,
    /// emptying the fired buffer: for the timer interrupt, which fires them
    /// once this lock is let go.
    pub(crate) fn take_fired(&mut self) -> [Option<(Armed, SleepSlot)>; FIRE_BATCH] {
        self.fired.take()
    }

    /// Timer arms waiting in the fired buffer.
    pub(crate) fn fired_len(&self) -> usize {
        self.fired.len
    }

    /// File `task`, which is blocking, to wake at `at`. Answers whether it
    /// could be: a task whose sleep slot a sleeper set elsewhere still holds
    /// -- one filed there, woken early, and not taken out because it had
    /// moved -- cannot be filed twice, and the caller keeps it runnable, so
    /// that its sleep returns early and asks again.
    pub(crate) fn file_sleeper(&mut self, at: u64, task: &Arc<Task>) -> bool {
        let Some(slot) = task.take_sleep_slot() else {
            return false;
        };
        // Zero is the task's "not filed" (`Task::filed_at`), so an instant of
        // zero -- the counter's first nanosecond -- is filed at one, under
        // the key kept (K10 code review, ledger 2773, (2)).
        let at = at.max(1);
        task.set_filed_at(at);
        let wake = Due::Wake(Arc::clone(task));
        // NOALLOC: a `Timeline` files the task in the slot it is given.
        self.sleepers.insert(at, task.id, wake, slot);
        true
    }

    /// File a timer's arm, keyed by the latest instant of its window, under
    /// timeline id `id`, in the slot the timer made at its creation. Under
    /// the timer's lock, which is taken before this one (`object::timer`).
    pub(crate) fn file_timer(&mut self, latest: u64, id: u64, armed: Armed, slot: SleepSlot) {
        armed.timer.refiled(latest);
        // NOALLOC: a `Timeline` files the arm in the slot it is given.
        self.sleepers.insert(latest, id, Due::Fire(armed), slot);
    }

    /// Take a timer's arm filed under `latest` and `id` out of this
    /// processor's sleeper set, with its slot: a descent by its key, never a
    /// walk. `None` when it is not filed here under that key: taken into the
    /// fired buffer, or being fired.
    pub(crate) fn unfile_timer(&mut self, latest: u64, id: u64) -> Option<(Armed, SleepSlot)> {
        match self.sleepers.remove_at(latest, id)? {
            (Due::Fire(armed), slot) => {
                armed.timer.unfiled();
                Some((armed, slot))
            }
            (wake @ Due::Wake(_), slot) => {
                // A timer's id has its top bit set and a task's never does
                // (`object::timer::timeline_id`), so this is not reached; if
                // it were, the task goes back where it was.
                // NOALLOC: back into the slot it was taken out with.
                self.sleepers.insert(latest, id, wake, slot);
                None
            }
        }
    }

    /// How many entries, tasks and timer arms, this processor's sleeper set
    /// holds: for the checks.
    pub(crate) fn sleepers_len(&self) -> usize {
        self.sleepers.len()
    }

    /// Put a runnable task into the fair class, carrying its lag.
    ///
    /// **The running task is charged first**, as Linux's `enqueue_entity`
    /// calls `update_curr` before placing anything. A task alone on a tickless
    /// processor is charged only at its next decision, so its `exec_start` can
    /// be a hundred milliseconds old when something arrives. Charged after the
    /// arrival is placed, all of that moves the virtual time with the newcomer
    /// already counted, and the newcomer comes out owed half of it: the stage 7
    /// check's first spinner arrived at lag +59 ms behind a checker that had
    /// run 118 ms uncharged, ran its whole loop before the checker was eligible
    /// to start the second, and the two "ran one after the other". The clamp in
    /// `placement_lag` cannot see this, because the lag is made after placement.
    ///
    /// In the task's own run slot, which it lends the queue until it leaves:
    /// nothing here allocates, and this is reached from interrupt handlers
    /// (finding F-23). A task without its slot is already queued somewhere,
    /// which the callers' `is_queued` checks rule out; it is left alone.
    pub(crate) fn insert(&mut self, task: &Arc<Task>) {
        self.insert_at(task, None);
    }

    /// [`CpuQueue::insert`], charging the running task up to `now` when it
    /// is given, so that a caller that has read the clock once reads it no
    /// more: the direct switch's `hand_over`.
    pub(crate) fn insert_at(&mut self, task: &Arc<Task>, now: Option<u64>) {
        let Some(slot) = task.take_run_slot() else {
            super::note_missing_slot();
            return;
        };
        if self.current.is_some() {
            self.account(now.unwrap_or_else(crate::timer::now_nanos));
        } else if super::stack::DEADLINE {
            // The deadline tier's time, before each enqueue onto it (DG.4); `account` tells it
            // above.
            <Stack as StackTiers<Arc<Task>>>::note_time(
                &mut self.stack,
                now.unwrap_or_else(crate::timer::now_nanos),
            );
        }
        // A task new to any queue is counted in its job's load here, once;
        // one woken was counted as it became runnable. Its weight is its
        // job's share of it as things stand now.
        task.join_group();
        task.set_weight(task.effective_weight());
        if let Err(refused) = Stack::enqueue(
            &mut self.stack,
            task.id,
            Arc::clone(task),
            task.entity_with(slot),
        ) {
            // An admitted entity on a processor that holds no admission for it:
            // `entity_with` reads the same word under this lock, so this is that
            // rule broken (docs/SCHED-CLASSES.md §9g, L.sched.184).
            if refused.reason == ferrix_sched_class::Refusal::NoSuchTier {
                crate::panic::fatal!(
                    crate::panic::catalog::ADMITTED_NOT_HERE,
                    "a task was queued as admitted where it holds no admission: task {}",
                    task.id
                );
            }
            // The only other refusals are a duplicate identifier and a zero weight,
            // neither of which this kernel can produce; dropping the clone is
            // what keeps the task alive in the table regardless.
            task.keep_refused(refused.entity);
            drop(refused.payload);
            return;
        }
        task.set_queued(true);
        self.rescale_slice();
    }

    /// Give a task a new weight, whether it is running here, queued here or
    /// neither.
    ///
    /// The task's own record is set either way, because that is what it is
    /// enqueued with next and it may be on no queue at all. The fair class is
    /// told as well when it holds the task, and answers whether it did.
    pub(crate) fn set_weight(&mut self, task: &Arc<Task>, weight: u32) {
        // Charged first for the reason `insert` and `release` are: a weight is
        // the rate the running task accrues virtual time at, and time it has
        // already run is owed at the rate it ran under.
        if self.current.is_some() {
            self.account(crate::timer::now_nanos());
        }
        task.set_weight(weight);
        // The only refusal is a weight of zero, which `weight_of_nice` never
        // answers and no caller here passes.
        let _ = Stack::set_weight(&mut self.stack, task.id, weight);
    }

    /// Give `task` a new policy (`sched::set_policy`), whether it is running
    /// here, queued here or on no queue: one queued here and not running is
    /// taken off its tier and enqueued again at the tail of its new place
    /// at once (L.sched.134); the running one is moved at this processor's
    /// next decision ([`CpuQueue::requeue_if_moved`]), so that the stack's
    /// running entity is the processor's running task at every moment
    /// outside a decision. Answers whether a decision is due now: the
    /// running task is to move, or something now outranks it.
    pub(crate) fn set_policy(
        &mut self,
        task: &Arc<Task>,
        params: Option<RtParams>,
        reset_on_fork: bool,
    ) -> bool {
        task.store_policy(params, reset_on_fork);
        if !task.is_queued() || task.entity_is_current() {
            // On no queue, it is enqueued with its new policy next time;
            // unchanged, nothing moves, as Linux's `__sched_setscheduler`
            // returns before its requeue.
            return false;
        }
        let running = self
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, task));
        if running {
            return true;
        }
        if let Some(task) = self.release(task.id, task.queued_upper()) {
            // NOALLOC: `CpuQueue::insert` queues the task in its own run slot,
            // or in its node of the tier above fair, given before its policy.
            self.insert(&task);
        }
        self.should_preempt()
    }

    /// At a decision (`choose_next`, before its pick): move `task`, the
    /// running one, still runnable, to the tier and place its policy names,
    /// if a change of policy left it in its old one (L.sched.134).
    pub(crate) fn requeue_if_moved(&mut self, task: &Arc<Task>) {
        if !task.is_queued() || task.entity_is_current() {
            return;
        }
        if let Some(task) = self.release(task.id, task.queued_upper()) {
            // NOALLOC: as `set_policy`'s.
            self.insert(&task);
        }
    }

    /// Task `id`'s request as the fair class holds it, for stage 5's check of
    /// slice requests (L.sched.107-108): its deadline less its virtual
    /// runtime, its weight, what it asked for, whether it is running, and
    /// the queue's slice now. `None` if it is not on this queue.
    pub(crate) fn request_sample(&self, id: TaskId) -> Option<(i64, u32, u64, bool, u64)> {
        let view = Stack::fair(&self.stack).get(id)?;
        Some((
            view.deadline.wrapping_sub(view.vruntime) as i64,
            view.weight,
            view.request_ns,
            view.running,
            Stack::fair(&self.stack).slice_ns(),
        ))
    }

    /// Give a task a new slice request, whether it is running here, queued
    /// here or neither (L.sched.107). Its own record is set either way; the
    /// fair class keeps the deadline it has and measures the next with it.
    pub(crate) fn set_request(&mut self, task: &Arc<Task>, request_ns: u64) {
        task.set_request(request_ns);
        let _ = Stack::set_request(&mut self.stack, task.id, request_ns);
    }

    /// Take the running task out of the fair class, keeping what it needs to
    /// come back with.
    pub(crate) fn detach_current(&mut self) {
        if let Some((_, task, entity)) = Stack::remove_current(&mut self.stack) {
            task.keep_entity(entity);
            task.set_queued(false);
        }
    }

    /// What runs on a processor asked to park (`docs/CORE-QUARANTINE.md`
    /// §3.2): its idle task, the running task, if it is another and still
    /// runnable, put back on the queue first, to be moved off by the park.
    pub(crate) fn park_pick(&mut self) -> Option<Arc<Task>> {
        if let Some(current) = self.current.clone()
            && !self.is_running_idle()
            && current.state() == RUNNABLE
        {
            self.detach_current();
            // NOALLOC: `CpuQueue::insert` queues the task in its own run slot,
            // which `detach_current` just gave back to it.
            self.insert(&current);
        }
        self.idle.clone()
    }

    /// Take out one sleeper that may run on `to` and is not pinned: its
    /// instant and the task, its sleep slot given back so that `to`'s
    /// timeline can file it again (`docs/CORE-QUARANTINE.md` §3.2).
    pub(crate) fn take_movable_sleeper(&mut self, to: usize) -> Option<(u64, Arc<Task>)> {
        let (at, due, slot) = self.sleepers.take_last_where(|due| match due {
            Due::Wake(task) => !super::pinned(task) && super::allowed(task, to),
            Due::Fire(_) => false,
        })?;
        match due {
            Due::Wake(task) => {
                task.return_sleep_slot(slot);
                Some((at, task))
            }
            // The test above refused these; if it had not, the arm goes back
            // where it was.
            Due::Fire(armed) => {
                let id = armed.timer.timeline_id();
                // NOALLOC: back into the slot it was taken out with.
                self.sleepers.insert(at, id, Due::Fire(armed), slot);
                None
            }
        }
    }

    /// Take out one timer arm, with the key it is filed under and its slot,
    /// for a processor leaving service to file on one in service: an arm is
    /// nobody's to pin (`docs/CORE-QUARANTINE.md` §3.2, L.sched.104). Under
    /// this queue's lock and the destination's.
    pub(crate) fn take_timer_arm(&mut self) -> Option<(u64, Armed, SleepSlot)> {
        let (at, due, slot) = self
            .sleepers
            .take_last_where(|due| matches!(due, Due::Fire(_)))?;
        match due {
            Due::Fire(armed) => Some((at, armed, slot)),
            // The test above refused these.
            wake @ Due::Wake(_) => {
                if let Due::Wake(task) = &wake {
                    let id = task.id;
                    // NOALLOC: back into the slot it was taken out with.
                    self.sleepers.insert(at, id, wake, slot);
                }
                None
            }
        }
    }

    /// What should run now: the fair class's choice, or the idle task.
    pub(crate) fn pick_next(&mut self) -> Option<Arc<Task>> {
        // The deadline tier's time, before the pick (DG.4).
        if super::stack::DEADLINE {
            <Stack as StackTiers<Arc<Task>>>::note_time(&mut self.stack, crate::timer::now_nanos());
        }
        if let Some(task) = Stack::pick(&mut self.stack) {
            // Only this processor's own running task may be picked while a
            // processor runs it (container review 30 (not on record; ledger 5684), B1): any
            // other would be run twice.
            let here = self
                .current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, task));
            if task.is_on_cpu() && !here {
                crate::panic::fatal!(
                    crate::panic::catalog::RUNNING_TASK_QUEUED,
                    "a task another processor runs was picked to run here: task {}",
                    task.id
                );
            }
            return Some(Arc::clone(task));
        }
        self.idle.clone()
    }

    /// Whether this CPU has anything to run besides its idle task.
    pub(crate) fn has_work(&self) -> bool {
        Stack::len(&self.stack) != 0
    }

    /// Fold the time since the last call into the load average.
    ///
    /// Busy is "the fair queue had something runnable", not "a task was
    /// current": the idle task is current when there is nothing to do, and
    /// counting it would make every processor read as permanently full.
    pub(crate) fn account_load(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.load_updated);
        if elapsed == 0 {
            return;
        }
        self.load_updated = now;
        // The level is the queue's total weight, so a processor with four
        // runnable tasks reads four times one with a single task rather than
        // the same "busy". The idle task is not in the fair queue, so an idle
        // processor contributes nothing without a special case.
        self.load
            .accumulate(elapsed, Stack::load_weight(&self.stack));
    }

    /// This processor as the balancer sees it.
    pub(crate) fn snapshot(&self) -> CpuLoad {
        CpuLoad {
            queued: self.len(),
            average: self.load.average(),
            // **Nothing to run, not "running the idle task".** A processor
            // that has just been given a task still has the idle task current
            // until it next schedules, so the second of a burst of placements
            // would see it as idle and pile on behind the first. What the
            // placer is asking is whether this processor has work, and an
            // empty fair queue is that question answered.
            idle: !self.has_work(),
        }
    }

    /// The load average, for the boot log.
    pub(crate) fn load_average(&self) -> u64 {
        self.load.average()
    }

    /// Whether this processor is running its idle task, or nothing at all.
    ///
    /// The idle task is deliberately not in the fair queue — it is what runs
    /// when that queue is empty, not the lowest-weight thing in it — so
    /// `should_preempt` has nothing to compare a new arrival against and
    /// answers false. That makes "should the target switch?" the wrong
    /// question to ask on its own when placing a task on another processor:
    /// an idle processor is asleep, and a sleeping processor that is never
    /// told has no way to find out.
    pub(crate) fn is_running_idle(&self) -> bool {
        match (self.current.as_ref(), self.idle.as_ref()) {
            (Some(current), Some(idle)) => Arc::ptr_eq(current, idle),
            (None, _) => true,
            (Some(_), None) => false,
        }
    }

    /// Arm the timer for the next decision this CPU has to make.
    pub(crate) fn arm_timer(&self, now: u64) {
        // A timer arm waiting in the fired buffer is fired by the next timer
        // interrupt, so one is asked for at once.
        let sleeper = if self.fired.len > 0 {
            Some(now)
        } else {
            self.sleepers.first_due()
        };
        // A slice only ends in a decision if something is waiting for it. One
        // task alone on a CPU is left to run: interrupting it would change
        // nothing, and this is where tickless comes from. With something
        // waiting, a decision at least once a slice, however long a request
        // the running task holds: see `RunQueue::decision_in_ns`.
        let slice = Stack::decision_in_ns(&self.stack).map(|left| now.saturating_add(left));

        // A task under a `cpu.max` is cut when its quota is used up, however
        // alone it is: nothing else would interrupt it to throttle it. The
        // task the fair class runs, which is the one charged (`account_in`):
        // a switch arms the timer before it stores its pick in `current`
        // (`switch_chosen`), so `current` there is still the outgoing task.
        let bandwidth = Stack::current(&self.stack)
            .filter(|_| crate::object::quota::bandwidth_in_use())
            .and_then(|task| crate::object::quota::runtime_left(task.group(), now))
            .map(|left| now.saturating_add(left.min(BANDWIDTH_SLICE_NS)));

        match [sleeper, slice, bandwidth].into_iter().flatten().min() {
            Some(at) => crate::timer::after_from(at.saturating_sub(now).max(MIN_ARM_NS), now),
            None => crate::timer::stop(),
        }
    }

    /// The task another CPU should take from this one, if any may be moved.
    pub(crate) fn steal_candidate(&self, to: usize) -> Option<TaskId> {
        // `allowed` is the one that matters now that affinity is a set: a
        // task allowed on processors 0 and 1 must not be moved to 2 however
        // idle 2 is (L.sched.97; read exactly, under this queue's lock, the
        // mask's writer's). `pinned` stays because it says something
        // different -- a task with exactly one home should not be moved even
        // *to* that home, since it is already there; one queued elsewhere is
        // the pick push's (L.sched.98).
        // And its domain, which is `to`'s (D-2): asked under both queues'
        // locks, which `steal_from` holds.
        Stack::fair(&self.stack).latest_where(|task| {
            !super::pinned(task)
                && super::allowed(task, to)
                && task.state() == RUNNABLE
                && super::domains::admits(task, to)
        })
    }

    /// A waiting real-time task processor `to` may take when this one goes
    /// out of service (`docs/CORE-QUARANTINE.md`), by the rules
    /// [`CpuQueue::steal_candidate`] applies to a fair one. A walk of the
    /// tier above fair, never a steal: real-time tasks are not stolen
    /// (`docs/SCHED-CLASSES.md` §9e, R5).
    pub(crate) fn upper_evacuee(&self, to: usize) -> Option<TaskId> {
        Stack::upper_waiting_where(&self.stack, |task| {
            !super::pinned(task)
                && super::allowed(task, to)
                && task.state() == RUNNABLE
                && super::domains::admits(task, to)
        })
    }

    /// A task waiting here, not running, that may not run on `cpu`, this
    /// queue's processor, for its domain (`sched::domains::admits`), with
    /// whether it waits in the tier above fair: what a processor leaving its
    /// domain moves off at its decision (docs/SCHED-CLASSES.md §9f, the
    /// switch's step 3). One walk of each tier.
    pub(crate) fn foreign_waiting(&self, cpu: usize) -> Option<(TaskId, bool)> {
        let foreign = |task: &Arc<Task>| !super::domains::admits(task, cpu);
        if let Some(id) = Stack::fair(&self.stack).latest_where(foreign) {
            return Some((id, false));
        }
        <Stack as StackTiers<Arc<Task>>>::upper_waiting_where(&self.stack, foreign)
            .map(|id| (id, true))
    }

    /// Whether everything here may run on `cpu`, this queue's processor:
    /// the running task and every waiting one (the switch's wait, V5).
    pub(crate) fn holds_only_admitted(&self, cpu: usize) -> bool {
        self.current
            .as_ref()
            .is_none_or(|task| super::domains::admits(task, cpu))
            && self.foreign_waiting(cpu).is_none()
    }

    /// Whether `task` is the one this queue runs.
    pub(crate) fn is_current(&self, task: &Arc<Task>) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, task))
    }

    /// Take a queued task off this queue, for another one to run: from the
    /// tier above fair with `upper`, from fair's otherwise, walking no other
    /// tier's queue (container review 30, B3).
    pub(crate) fn release(&mut self, id: TaskId, upper: bool) -> Option<Arc<Task>> {
        // Charged first for the reason `insert` is: the lag the leaving task
        // takes with it is measured against a virtual time that must include
        // everything the running task has had.
        if self.current.is_some() {
            self.account(crate::timer::now_nanos());
        }
        let (task, entity) =
            <Stack as StackTiers<Arc<Task>>>::remove_in(&mut self.stack, id, upper)?;
        task.keep_entity(entity);
        task.set_queued(false);
        self.rescale_slice();
        Some(task)
    }

    /// Share the target latency out among however many are runnable now.
    ///
    /// A fixed slice is also a fixed latency bound *per task*, so the last of
    /// `n` runnable tasks waits `n` slices — which at stage 5's thousand
    /// threads and three milliseconds each is three seconds before the last
    /// one is looked at. Scaling the slice makes the wait the target latency
    /// instead, until the floor stops it.
    fn rescale_slice(&mut self) {
        let slice = slice_for(TARGET_LATENCY_NS, MIN_SLICE_NS, Stack::len(&self.stack));
        // The only refusal is a slice of zero, and `slice_for` floors at
        // `MIN_SLICE_NS`, which is not zero.
        let _ = Stack::set_slice_ns(&mut self.stack, slice);
    }

    /// The slice this queue is currently handing out.
    pub(crate) fn slice_ns(&self) -> u64 {
        Stack::fair(&self.stack).config().slice_ns
    }

    /// Whether the running task should give way to something queued.
    pub(crate) fn should_preempt(&self) -> bool {
        Stack::should_preempt(&self.stack)
    }

    /// Whether a sleeper's time has come by `now`: whether
    /// [`CpuQueue::wake_sleepers`] at `now` would find an entry to take. The
    /// direct switch declines when one has, rather than wake it, so that a
    /// declined attempt leaves the queue as it found it (T12,
    /// `docs/OPAQUE-KERNEL.md` §9.7).
    #[cfg(ferrix_fast_ipc)]
    pub(crate) fn sleeper_due(&self, now: u64) -> bool {
        let room = self.fired.len < FIRE_BATCH;
        self.sleepers.first().is_some_and(|(at, _, payload)| {
            at <= now || matches!(payload, Due::Fire(armed) if room && armed.earliest <= now)
        })
    }

    /// The direct switch's queue operation (`docs/OPAQUE-KERNEL.md` §9.7,
    /// part 1): `peer`, made runnable by its caller, joins this queue while
    /// the running task still runs, `between` runs (the running task is set
    /// blocked there), the running task leaves the fair class, and the pick
    /// is made. The general path's sequence -- the wake's
    /// [`CpuQueue::insert`], then `choose_next`'s `account`,
    /// `detach_current` and `pick_next` -- with every clock read at `now`,
    /// on a queue with nothing waiting and no sleeper due, so the pick can
    /// only be `peer`. Answers the pick.
    ///
    /// `insert`'s charge and the weight it gives `peer` are its own code;
    /// the fair class's part is `RunQueue::hand_over`, which the host test
    /// holds to `enqueue`, the rescale, `remove_curr` and `pick_next` bit for
    /// bit. `choose_next`'s second `account`, at the same `now`, changes
    /// nothing and is not made.
    ///
    /// **The job loads** (`docs/OPAQUE-KERNEL.md` §9.11, J1 and J2; ledger
    /// line 461): `plan` is what the caller of this made of the two tasks'
    /// words before any charge. [`JobPlan::Separate`]: the peer's join was
    /// made in full, and the caller's leave is `between`'s, as the general
    /// path makes them. [`JobPlan::Fold`]: the peer is counted in its word
    /// and not yet in the job's load, which the caller keeps busy; the
    /// charge's share and the peer's weight are computed with the peer's
    /// weight added to that load (one walk for both when their bases are
    /// equal), and once the caller's state is stored and its word left, the
    /// load changes once by the net. A way out that does not reach the
    /// caller's leave makes the peer's join alone ([`PendingJoin`]'s drop).
    #[cfg(ferrix_fast_ipc)]
    pub(crate) fn hand_over(
        &mut self,
        peer: Arc<Task>,
        now: u64,
        plan: JobPlan,
        between: impl FnOnce(Arc<Task>),
    ) -> Option<Arc<Task>> {
        let mut pending = PendingJoin::of(plan);
        let extra = pending.extra();
        let Some(slot) = peer.take_run_slot() else {
            super::note_missing_slot();
            return None;
        };
        let share = if self.current.is_some() {
            self.account_with(now, false, extra)
        } else {
            None
        };
        // As `insert`: counted in its job (it already is, made runnable),
        // and weighed by its job's share as things stand, the caller still
        // counted. Under a fold, the caller's walk is the peer's when both
        // have one base: one job, one read of its levels.
        peer.join_group();
        let weight = match share {
            Some((base, due)) if extra.is_some() && base == peer.base_weight() => due,
            _ => peer.effective_weight_with(extra).1,
        };
        peer.set_weight(weight);
        // `rescale_slice` as `insert` makes it, with the peer counted.
        let slice_after = slice_for(TARGET_LATENCY_NS, MIN_SLICE_NS, Stack::len(&self.stack) + 1);
        let (id, entity) = (peer.id, peer.entity_with(slot));
        // The peer's own reference goes into the queue, and the caller's
        // comes out of it into `between`: moved, not counted up and down.
        match Stack::hand_over(&mut self.stack, id, peer, entity, slice_after) {
            Ok(left) => {
                // The pick, read once: marked queued here and answered below.
                let next = Stack::current(&self.stack).cloned();
                if let Some(next) = next.as_ref() {
                    next.set_queued(true);
                }
                if let Some((_, task, entity)) = left {
                    task.keep_entity(entity);
                    task.set_queued(false);
                    // Under a fold, the caller's state and word here, the
                    // store first as `set_state_from` makes them, and the
                    // load's one change; `between`'s own `set_state_from`
                    // then finds the word left and changes nothing.
                    if pending.extra().is_some() {
                        task.store_state(BLOCKED);
                        pending.settle(task.leave_word());
                    }
                    // The caller set blocked and parked, its job's load let
                    // go after the peer's weight was taken with it counted.
                    between(task);
                }
                next
            }
            Err(refused) => {
                // A duplicate or a weight of zero, neither of which this
                // kernel makes: as `insert` refuses.
                refused.payload.keep_refused(refused.entity);
                None
            }
        }
    }

    /// Take `id` out of this processor's sleeper set, if it is in it.
    ///
    /// **A task woken early is still filed under the deadline it no longer
    /// intends to keep.** Leave the entry behind and `wake_sleepers` finds it
    /// later, sees a time already past, and makes the task runnable — out of
    /// whatever it is doing by then. What that looked like was the sleep check
    /// failing with "a sleep came back before its deadline": a twenty
    /// millisecond sleep cut short by a five millisecond deadline the task had
    /// abandoned two checks earlier.
    ///
    /// Keyed, not scanned: the task keeps the instant it was filed under
    /// ([`Task::filed_at`]), so this is a descent of the set, never a walk,
    /// however many tasks and timer arms the set holds (`L.sched.84`). Every
    /// move of a blocked task passes `asleep_at_home` under its home's lock,
    /// which unfiles it, so a task's entry is on its home's set; were one
    /// elsewhere it would not be found here and would stay filed under its key.
    pub(crate) fn remove_sleeper(&mut self, task: &Task) -> bool {
        let at = task.filed_at();
        if at == 0 {
            return false;
        }
        match self.sleepers.remove_at(at, task.id) {
            Some((Due::Wake(task), slot)) => {
                task.set_filed_at(0);
                task.return_sleep_slot(slot);
                true
            }
            Some((other, slot)) => {
                // A timer's id never equals a task's (`object::timer`), so
                // this is not reached; if it were, the arm goes back.
                // NOALLOC: back into the slot it was taken out with.
                self.sleepers.insert(at, task.id, other, slot);
                false
            }
            None => false,
        }
    }

    /// Tasks waiting to run: everything on the queue but the running one.
    ///
    /// The number `arm_timer` decides on, so it is also the number that says
    /// whether this processor's timer needs to exist at all.
    #[inline]
    pub(crate) fn waiting(&self) -> usize {
        // Derived over the whole stack (docs/SCHED-CLASSES.md §9d, F3):
        // everything queued less the running one, of any tier.
        Stack::len(&self.stack).saturating_sub(usize::from(Stack::current(&self.stack).is_some()))
    }

    /// Tasks on this queue, the running one included.
    pub(crate) fn len(&self) -> usize {
        Stack::len(&self.stack)
    }

    /// Check the fair class's own bookkeeping.
    pub(crate) fn check_invariants(&self) -> Result<(), &'static str> {
        Stack::check(&self.stack)?;
        let running_is_current = match (Stack::current(&self.stack), self.current.as_ref()) {
            (Some(fair), Some(current)) => Arc::ptr_eq(fair, current),
            // Nothing in the fair class: the idle task is what runs.
            (None, Some(current)) => self
                .idle
                .as_ref()
                .is_some_and(|idle| Arc::ptr_eq(idle, current)),
            _ => false,
        };
        if !running_is_current {
            return Err("the running task is not the fair class's running entity");
        }
        // Whether a dead task is still queued is not asked here. It was, of
        // `previous`, which `finish_switch` empties before it releases the
        // lock this runs under, so it could never fail. `finish_switch` asks
        // it instead, at the moment it has an answer.
        Ok(())
    }
}

/// A [`JobPlan::Fold`]'s peer join, counted in the peer's word and not yet
/// in its job's load, until [`PendingJoin::settle`] makes the load's change
/// with the caller's leave (`docs/OPAQUE-KERNEL.md` §9.11, J1). Dropped
/// unsettled -- a way out of `CpuQueue::hand_over` that did not reach the
/// caller's leave -- it makes the peer's join alone, as the general path
/// leaves things there: the peer joined, the caller not left. One place for
/// every such way out (ledger line 461, J-C1).
#[cfg(ferrix_fast_ipc)]
struct PendingJoin {
    /// The job and the weight the peer's word was counted with, until
    /// settled.
    fold: Option<(u32, u32)>,
}

#[cfg(ferrix_fast_ipc)]
impl PendingJoin {
    /// The pending join `plan` leaves, if it folds.
    const fn of(plan: JobPlan) -> PendingJoin {
        PendingJoin {
            fold: match plan {
                JobPlan::Fold { job, joined } => Some((job, joined)),
                JobPlan::Separate => None,
            },
        }
    }

    /// What the weights are computed with: the peer's weight, added to its
    /// job's load, while it is pending.
    fn extra(&self) -> Option<(u32, i64)> {
        self.fold.map(|(job, joined)| (job, i64::from(joined)))
    }

    /// The caller's leave answered `left`: the load's change, once, by
    /// `ferrix_sched_fair::settle_job_fold`. A leave that did not answer as
    /// planned -- which `Task::set_group`'s callers rule out -- is counted,
    /// and the stage-9 check requires none.
    fn settle(&mut self, left: Option<(u32, u32)>) {
        let Some((job, joined)) = self.fold.take() else {
            return;
        };
        let (changes, planned) = settle_job_fold(job, joined, left);
        for (at, delta) in changes.into_iter().flatten() {
            crate::object::quota::adjust(at, delta);
        }
        if !planned {
            super::direct::count(super::direct::Count::FoldUnplanned);
        }
    }
}

#[cfg(ferrix_fast_ipc)]
impl Drop for PendingJoin {
    fn drop(&mut self) {
        if let Some((job, joined)) = self.fold.take() {
            crate::object::quota::adjust(job, i64::from(joined));
        }
    }
}
