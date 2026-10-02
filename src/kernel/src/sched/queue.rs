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

use ferrix_sched::{Config, CpuLoad, EntityState, Load, RunQueue, Timeline, slice_for};

use super::task::{BLOCKED, RUNNABLE, Task, TaskId};

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

/// Whether `a` is before `b` in wrapping virtual time.
const fn before(a: u64, b: u64) -> bool {
    (a.wrapping_sub(b) as i64) < 0
}

/// One CPU's queue.
#[derive(Debug)]
pub(crate) struct CpuQueue {
    /// The fair class. The idle task is not in it: it is what runs when this
    /// is empty, which is what makes it the bottom of the class stack rather
    /// than a task with a very small weight.
    pub(crate) fair: RunQueue<Arc<Task>>,
    /// What is running, which is the idle task when the fair class is empty.
    pub(crate) current: Option<Arc<Task>>,
    /// This CPU's idle task.
    pub(crate) idle: Option<Arc<Task>>,
    /// What was running before the last switch, for the incoming context to
    /// finish with.
    pub(crate) previous: Option<Arc<Task>>,
    /// Tasks asleep on this CPU, by the instant they wake, each in its own
    /// sleep slot: filing and waking allocate nothing.
    sleepers: Timeline<Arc<Task>>,
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
}

impl CpuQueue {
    /// An empty queue.
    pub(crate) fn new() -> Result<CpuQueue, &'static str> {
        let fair = RunQueue::new(Config {
            slice_ns: TARGET_LATENCY_NS,
        })
        .map_err(|_| "the scheduler's slice is not a slice")?;
        Ok(CpuQueue {
            fair,
            current: None,
            idle: None,
            previous: None,
            sleepers: Timeline::new(),
            load: Load::new(),
            load_updated: 0,
            exec_start: 0,
            stats: Stats::default(),
            trace: [Pick::default(); TRACE_PICKS],
            trace_next: 0,
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
        let Some(picked) = self.fair.current_id() else {
            return;
        };
        let mut pick = Pick {
            at: now,
            picked,
            scanned: picked,
            avg: self.fair.avg_vruntime(),
            seen: [Seen::default(); TRACE_ENTITIES],
            count: 0,
        };
        let mut best_eligible: Option<Seen> = None;
        let mut best_any: Option<Seen> = None;
        self.fair.for_each(|view| {
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
        self.account_load(now);
        let delta = now.saturating_sub(self.exec_start);
        self.exec_start = now;
        if delta == 0 {
            return;
        }
        if self.fair.current().is_some() {
            self.stats.busy_ns += delta;
        } else {
            self.stats.idle_ns += delta;
        }
        let Some(remaining) = self.fair.remaining_ns() else {
            return;
        };
        if let Some(task) = self.fair.current() {
            task.add_runtime(delta);
        }
        if self.fair.update_curr(delta) {
            let overrun = delta.saturating_sub(remaining);
            self.stats.worst_overrun = self.stats.worst_overrun.max(overrun);
            if self.stats.measuring {
                self.stats.overrun_total = self.stats.overrun_total.saturating_add(overrun);
            }
        }
        self.follow_group_share();
        if self.stats.measuring {
            self.measure();
        }
    }

    /// Give the running task the weight its job's share says it should have
    /// now, if that has moved by more than an eighth: the other tasks of its
    /// job came and went since it was queued (`object::quota::effective`).
    /// A task in no job other than the root never changes here.
    fn follow_group_share(&mut self) {
        let Some(task) = self.fair.current() else {
            return;
        };
        if task.group() == crate::object::quota::NONE {
            return;
        }
        let (id, now) = (task.id, task.entity_state().weight);
        let due = task.effective_weight();
        if now.abs_diff(due) <= now / 8 {
            return;
        }
        task.set_weight(due);
        // The only refusal is a weight of zero, which `effective` never
        // answers.
        let _ = self.fair.set_weight(id, due);
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
        if self.fair.current().is_some() {
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
        self.fair.for_each(|view| {
            if view.payload.is_measured() {
                total += u128::from(view.payload.since_baseline(view.sum_exec));
                weights += u128::from(view.weight);
            }
        });
        if weights == 0 {
            return;
        }

        let mut worst = 0u64;
        self.fair.for_each(|view| {
            if !view.payload.is_measured() {
                return;
            }
            let share = total * u128::from(view.weight) / weights;
            let had = u128::from(view.payload.since_baseline(view.sum_exec));
            worst = worst.max(share.abs_diff(had) as u64);
        });
        self.stats.worst_lag = self.stats.worst_lag.max(worst);
    }

    /// Wake every task whose sleep has ended.
    pub(crate) fn wake_sleepers(&mut self, now: u64) {
        while let Some((_, task, slot)) = self.sleepers.pop_due(now) {
            task.return_sleep_slot(slot);
            // Only a task still blocked is woken. One already woken some other
            // way is running or queued, and one that has exited must never run
            // again; for either, the entry is stale and is dropped.
            if task.state() != BLOCKED {
                continue;
            }
            task.set_state(RUNNABLE);
            // NOALLOC: `CpuQueue::insert` queues the task in its own run slot.
            self.insert(&task);
        }
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
        // NOALLOC: a `Timeline` files the task in the slot it is given.
        self.sleepers.insert(at, task.id, Arc::clone(task), slot);
        true
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
        let Some(slot) = task.take_run_slot() else {
            super::note_missing_slot();
            return;
        };
        if self.current.is_some() {
            self.account(crate::timer::now_nanos());
        }
        // A task new to any queue is counted in its job's load here, once;
        // one woken was counted as it became runnable. Its weight is its
        // job's share of it as things stand now.
        task.join_group();
        task.set_weight(task.effective_weight());
        if let Err(refused) =
            self.fair
                .enqueue(task.id, Arc::clone(task), task.entity_state(), slot)
        {
            // The only refusals are a duplicate identifier and a zero weight,
            // neither of which this kernel can produce; dropping the clone is
            // what keeps the task alive in the table regardless.
            task.return_run_slot(refused.slot);
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
        let _ = self.fair.set_weight(task.id, weight);
    }

    /// Take the running task out of the fair class, keeping what it needs to
    /// come back with.
    pub(crate) fn detach_current(&mut self) {
        if let Some((_, task, state, slot)) = self.fair.remove_curr() {
            task.return_run_slot(slot);
            task.store_entity_state(state);
            task.set_queued(false);
        }
    }

    /// What should run now: the fair class's choice, or the idle task.
    pub(crate) fn pick_next(&mut self) -> Option<Arc<Task>> {
        if let Some(task) = self.fair.pick_next() {
            return Some(Arc::clone(task));
        }
        self.idle.clone()
    }

    /// Whether this CPU has anything to run besides its idle task.
    pub(crate) fn has_work(&self) -> bool {
        !self.fair.is_empty()
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
        self.load.accumulate(elapsed, self.fair.load_weight());
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
        let sleeper = self.sleepers.first_due();
        // A slice only ends in a decision if something is waiting for it. One
        // task alone on a CPU is left to run: interrupting it would change
        // nothing, and this is where tickless comes from. With something
        // waiting, a decision at least once a slice, however long a request
        // the running task holds: see `RunQueue::decision_in_ns`.
        let slice = self
            .fair
            .decision_in_ns()
            .map(|left| now.saturating_add(left));

        let profile_tick = (!self.is_running_idle()).then(|| now.saturating_add(1_000_000));
        match [sleeper, slice, profile_tick].into_iter().flatten().min() {
            Some(at) => crate::timer::after(at.saturating_sub(now).max(MIN_ARM_NS)),
            None => crate::timer::stop(),
        }
    }

    /// The task another CPU should take from this one, if any may be moved.
    pub(crate) fn steal_candidate(&self, to: usize) -> Option<TaskId> {
        // `may_run_on` is the one that matters now that affinity is a set:
        // a task allowed on processors 0 and 1 must not be moved to 2 however
        // idle 2 is. `is_pinned` stays because it says something different —
        // a task with exactly one home should not be moved even *to* that
        // home, since it is already there.
        self.fair.latest_where(|task| {
            !task.is_pinned() && task.may_run_on(to) && task.state() == RUNNABLE
        })
    }

    /// Take a queued task off this queue, for another one to run.
    pub(crate) fn release(&mut self, id: TaskId) -> Option<(Arc<Task>, EntityState)> {
        // Charged first for the reason `insert` is: the lag the leaving task
        // takes with it is measured against a virtual time that must include
        // everything the running task has had.
        if self.current.is_some() {
            self.account(crate::timer::now_nanos());
        }
        let (task, state, slot) = self.fair.remove(id)?;
        task.return_run_slot(slot);
        task.set_queued(false);
        self.rescale_slice();
        Some((task, state))
    }

    /// Share the target latency out among however many are runnable now.
    ///
    /// A fixed slice is also a fixed latency bound *per task*, so the last of
    /// `n` runnable tasks waits `n` slices — which at stage 5's thousand
    /// threads and three milliseconds each is three seconds before the last
    /// one is looked at. Scaling the slice makes the wait the target latency
    /// instead, until the floor stops it.
    fn rescale_slice(&mut self) {
        let slice = slice_for(TARGET_LATENCY_NS, MIN_SLICE_NS, self.fair.len());
        // The only refusal is a slice of zero, and `slice_for` floors at
        // `MIN_SLICE_NS`, which is not zero.
        let _ = self.fair.set_slice_ns(slice);
    }

    /// The slice this queue is currently handing out.
    pub(crate) fn slice_ns(&self) -> u64 {
        self.fair.config().slice_ns
    }

    /// Whether the running task should give way to something queued.
    pub(crate) fn should_preempt(&self) -> bool {
        self.fair.should_preempt()
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
    /// Scanned rather than keyed, because the map is keyed by wake-up time and
    /// the task's own copy of that was consumed when it was filed. Sleeper sets
    /// are short.
    pub(crate) fn remove_sleeper(&mut self, id: TaskId) -> bool {
        let Some((task, slot)) = self.sleepers.remove(id) else {
            return false;
        };
        task.return_sleep_slot(slot);
        true
    }

    /// Tasks waiting to run: everything on the queue but the running one.
    ///
    /// The number `arm_timer` decides on, so it is also the number that says
    /// whether this processor's timer needs to exist at all.
    pub(crate) fn waiting(&self) -> usize {
        self.fair.queued()
    }

    /// Tasks on this queue, the running one included.
    pub(crate) fn len(&self) -> usize {
        self.fair.len()
    }

    /// Check the fair class's own bookkeeping.
    pub(crate) fn check_invariants(&self) -> Result<(), &'static str> {
        self.fair.check_invariants()?;
        let running_is_current = match (self.fair.current(), self.current.as_ref()) {
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
