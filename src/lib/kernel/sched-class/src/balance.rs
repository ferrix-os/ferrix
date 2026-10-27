//! Where a task should run, and how loaded a processor is.
//!
//! Stage 5 left the scheduler fair on each processor and naive across them: a
//! task started wherever it was created, and the only thing that ever moved
//! one was a processor going idle and taking it. That is enough to pass a test
//! in which every processor eventually runs out of work, and not enough for a
//! machine where one does not.
//!
//! Four things live here, and all four are arithmetic over numbers the kernel
//! hands in — no processor is named, nothing is read from hardware, and every
//! one of them is decided by a function `cargo test` can call.
//!
//! * [`Load`], a decaying average of how busy something has been, which is
//!   what every decision below is made against. A queue's length is the
//!   obvious alternative and the wrong one: four tasks that sleep most of the
//!   time are not the load that one spinner is.
//! * [`place`], which picks the processor a waking or new task should go to.
//! * [`imbalance`] and [`busiest`], which say whether it is worth moving a
//!   task that is already somewhere, and from where.

use crate::CpuSet;

// ---------------------------------------------------------------------------
// Load tracking
// ---------------------------------------------------------------------------

/// One nice-0 entity's worth of demand.
///
/// [`Load::average`] is measured in these: a processor with one nice-0 entity
/// permanently runnable reads `LOAD_SCALE`, one with four reads about
/// `4 * LOAD_SCALE`, and one with a single entity that is runnable a quarter
/// of the time reads about `LOAD_SCALE / 4`.
///
/// **Demand, not occupancy, and the difference is the whole point.** A
/// processor is either running something or it is not, so "busy" saturates
/// the moment it has one task and says nothing thereafter — which makes every
/// busy processor look identical and gives a balancer nothing to compare.
/// Weighted demand keeps counting.
///
/// A power of two so the arithmetic below is shifts, and equal to
/// `NICE_0_WEIGHT` (the fair class's), which is Linux's choice for the
/// same quantity.
pub const LOAD_SCALE: u64 = 1024;

/// How long a load period is, in nanoseconds.
///
/// Just over a millisecond, and a power of two so that dividing by it is a
/// shift. Linux uses 1024 microseconds for exactly that reason.
pub const LOAD_PERIOD_NS: u64 = 1 << 20;

/// The numerator of the per-period decay, over [`DECAY_DEN`].
///
/// `4008/4096` is `0.978515625`, and raised to the 32nd power it is `0.4991` —
/// so the half-life is 32 periods, near enough 33 milliseconds. That is the
/// same shape as Linux's `y^32 = 0.5`, in arithmetic that is exact in
/// integers rather than a table of magic constants.
const DECAY_NUM: u64 = 4008;
/// The denominator of the decay. A power of two, so the division is a shift.
const DECAY_DEN: u64 = 4096;

/// Extra bits the average is carried with internally.
///
/// **Not a detail: without them the average never reaches its own fixed
/// point.** Each step is `average * 4008/4096 + utilisation * 88/4096`, and
/// each of those divisions truncates. At full utilisation the loss from
/// truncating the first term equals the gain from the second at about 978 of
/// 1024, so a permanently busy processor would report 95% busy forever and
/// the balancer would compare every processor against a ceiling it could not
/// reach. Ten extra bits put the truncation error a thousand times below the
/// increment, and the fixed point comes out exact.
const PRECISION: u32 = 10;

/// How many periods of history are worth walking one at a time.
///
/// After this many the average has been multiplied by about `2^-16`, so what
/// it held before them is gone and what is left is the level held throughout.
/// [`Load::accumulate`] writes that down directly rather than walking there.
///
/// Bounding the walk is not tidiness. The kernel folds time in under a run
/// queue's lock with interrupts masked, and a processor idle for an hour hands
/// over an hour: three and a half million periods, each with a 64-bit division
/// that ARMv7-A does in a library call.
const MAX_DECAY_PERIODS: u32 = 512;

/// A decaying average of how busy something has been.
///
/// Reads as a fraction of [`LOAD_SCALE`]: `LOAD_SCALE` for something that has
/// been busy throughout, zero for something that has been idle for long
/// enough, and the steady-state duty cycle for anything in between.
///
/// # Why an average and not a count
///
/// Every decision in this module is "which processor has the most work", and
/// the cheap answer — how many entities are on its queue — is wrong in the
/// case that matters. Four tasks that each run a hundredth of the time are a
/// quarter the work of one that never sleeps, and a balancer that believed
/// otherwise would move the spinner onto the processor already running one
/// and call the machine balanced.
///
/// # Why it decays
///
/// A total would remember a burst of work forever. What the balancer wants to
/// know is what this processor is doing *now*, with enough memory not to chase
/// every millisecond of noise — which is exactly a geometric decay, and the
/// half-life is the knob.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Load {
    /// The average, in units of [`LOAD_SCALE`] shifted up by [`PRECISION`].
    scaled: u64,
    /// Nanoseconds accumulated towards the period in progress.
    elapsed_ns: u64,
    /// Demand summed over those nanoseconds: the level times the time it was
    /// held for.
    weighted_ns: u64,
}

impl Load {
    /// Idle, and never yet busy.
    #[must_use]
    pub const fn new() -> Load {
        Load {
            scaled: 0,
            elapsed_ns: 0,
            weighted_ns: 0,
        }
    }

    /// What the average reads, in units of [`LOAD_SCALE`].
    #[must_use]
    pub const fn average(&self) -> u64 {
        self.scaled >> PRECISION
    }

    /// Account `elapsed_ns` of wall time during which demand stood at `level`.
    ///
    /// `level` is in units of [`LOAD_SCALE`]: what a run queue's total weight
    /// comes to, so nothing runnable is zero, one nice-0 entity is
    /// `LOAD_SCALE`, and four of them are four times that. It is a *level*,
    /// held for the whole of `elapsed_ns`, not an amount consumed during it —
    /// so a caller that samples on every tick and one that samples on every
    /// context switch describe the same history.
    ///
    /// Called with whatever the caller has: a timer tick, a context switch, a
    /// processor going idle. The period is closed only when a whole one has
    /// accumulated, so calling this often and calling it rarely give the same
    /// answer.
    pub const fn accumulate(&mut self, elapsed_ns: u64, level: u64) {
        let total_ns = self.elapsed_ns.saturating_add(elapsed_ns);
        // **More history than the average remembers: write down the answer.**
        // Every whole period but the partial one at the end was spent at
        // `level`, and more than `MAX_DECAY_PERIODS` of them leave nothing of
        // what came before. The per-period walk would settle at `level` too,
        // or a unit short of it from below, where its truncating steps stall.
        // Checked before anything is multiplied, because `level` times a long
        // enough stretch also saturates the demand accumulator.
        if total_ns / LOAD_PERIOD_NS > MAX_DECAY_PERIODS as u64 {
            let partial_ns = total_ns % LOAD_PERIOD_NS;
            self.scaled = level.saturating_mul(1 << PRECISION);
            self.elapsed_ns = partial_ns;
            self.weighted_ns = level.saturating_mul(partial_ns);
            return;
        }

        self.elapsed_ns = total_ns;
        self.weighted_ns = self
            .weighted_ns
            .saturating_add(level.saturating_mul(elapsed_ns));

        // Whole periods only. A partial one stays in the accumulators and is
        // folded in when it completes, so the result does not depend on how
        // finely the caller chopped up the time.
        while self.elapsed_ns >= LOAD_PERIOD_NS {
            // This period's share of the accumulated demand, which for a
            // level held throughout is that level.
            let share = self.weighted_ns / self.elapsed_ns;
            let contribution = share << PRECISION;

            // Decay the old and add the new share. Written that way rather
            // than as a weighted mean because the second form needs a
            // division that rounds a light period's contribution to nothing.
            self.scaled = (self.scaled * DECAY_NUM / DECAY_DEN)
                + (contribution * (DECAY_DEN - DECAY_NUM) / DECAY_DEN);

            self.weighted_ns = self.weighted_ns.saturating_sub(share * LOAD_PERIOD_NS);
            self.elapsed_ns -= LOAD_PERIOD_NS;
        }
    }

    /// Age the average by `elapsed_ns` of doing nothing.
    ///
    /// The same as [`accumulate`](Self::accumulate) with no busy time, and as
    /// bounded: a processor that has been idle for a second decays to the same
    /// place as one idle for a minute, and neither walks every period to say
    /// so.
    pub const fn decay(&mut self, elapsed_ns: u64) {
        self.accumulate(elapsed_ns, 0);
    }
}

// ---------------------------------------------------------------------------
// What the kernel tells this module about a processor
// ---------------------------------------------------------------------------

/// One processor, as far as a placement decision is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CpuLoad {
    /// Entities on its run queue, the running one included.
    pub queued: usize,
    /// Its decaying load average, from [`Load::average`].
    pub average: u64,
    /// Whether it is running its idle task.
    pub idle: bool,
}

impl CpuLoad {
    /// A processor with nothing on it.
    #[must_use]
    pub const fn idle() -> CpuLoad {
        CpuLoad {
            queued: 0,
            average: 0,
            idle: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Choose the processor a task should run on.
///
/// `prefer` is where it ran last, or where it is being created — the tie is
/// broken towards it, because a task that goes back to the processor it just
/// left finds its own data in that processor's caches, and the cost of not
/// doing so is invisible to every counter in this module.
///
/// `allowed` is the task's affinity: the answer is always inside it, and
/// `None` means there is no processor it may run on at all.
///
/// # The order, and why
///
/// 1. **`prefer`, if it is idle.** Nothing beats staying put on a processor
///    with nothing else to do: no migration, no cold cache, no lock on another
///    processor's queue.
/// 2. **Any idle processor.** An idle processor is capacity going to waste,
///    and waking one is what stops a burst of new tasks queueing behind each
///    other on the processor that created them. This is the case stage 5 got
///    wrong by not looking at all.
/// 3. **The least loaded.** With nothing idle, the question is only where the
///    task will wait least, which is where the load average is lowest; ties go
///    to `prefer`, then to the lowest processor number so the answer does not
///    depend on iteration order.
#[must_use]
pub fn place(loads: &[CpuLoad], allowed: &CpuSet, prefer: usize) -> Option<usize> {
    let mut choice = Placement::new(prefer);
    for (cpu, load) in loads.iter().enumerate() {
        if allowed.contains(cpu) {
            choice.consider(cpu, *load);
        }
    }
    choice.choice()
}

/// [`place`], one processor at a time.
///
/// # Why the caller may not want the slice
///
/// A kernel asking this question holds a different lock for each processor it
/// reads, so the obvious shape is to snapshot them all into an array and call
/// [`place`]. The array is the problem: sized for [`MAX_CPUS`](crate::MAX_CPUS)
/// it is six kilobytes, and the caller is a kernel with a sixteen-kilobyte
/// stack that may be several frames deep inside an interrupt when it asks.
/// That is how this module first broke the machine — not through any decision
/// it made, but by needing 38% of a stack to make one.
///
/// So the fold is exposed: read one processor, offer it here, drop the lock,
/// move on. The accumulator is three words and the answer is the same, which
/// is what [`place`] being written in terms of it is there to demonstrate.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    /// Where the task would rather be.
    prefer: usize,
    /// The best non-idle candidate so far, as the pair it is ranked on: how
    /// many are queued there, then how loaded it has been.
    best: Option<(usize, (usize, u64))>,
    /// An idle processor, once one has been offered.
    idle: Option<usize>,
}

impl Placement {
    /// A fold that has seen nothing yet.
    #[must_use]
    pub const fn new(prefer: usize) -> Placement {
        Placement {
            prefer,
            best: None,
            idle: None,
        }
    }

    /// Offer one processor. The caller has already checked it is permitted.
    pub const fn consider(&mut self, cpu: usize, load: CpuLoad) {
        if load.idle {
            // `prefer` idle beats any other idle, and the first idle beats a
            // later one, so an existing answer is only replaced by `prefer`.
            match self.idle {
                Some(_) if cpu != self.prefer => {}
                _ => self.idle = Some(cpu),
            }
            return;
        }
        // **Fewest queued first, and only then the load average.** The order
        // matters and having it the other way round was a bug.
        //
        // `queued` moves the instant a task is placed, so it is the only
        // thing here that shows a placer the effect of its own last decision.
        // `average` is a decaying history with a 33-millisecond half-life,
        // which cannot move at all inside a burst of spawns. Ranking on the
        // average first therefore sends *every* task in a burst to whichever
        // processor has been idle longest: its average is near zero and stays
        // there however much work it has just been handed.
        //
        // Found on an STM32MP157D-DK1, where a two-task burst on a quiet
        // machine put both tasks on one core. It is also why this kernel's
        // own boot log said "new tasks spread over 2 processors" on a
        // four-processor machine, which nobody chased.
        let score = (load.queued, load.average);
        let better = match self.best {
            None => true,
            // Written out rather than as a tuple comparison because this is a
            // `const fn` and `PartialOrd` is not const yet — and because
            // spelling the order out is what the comment above is about.
            Some((_, (best_queued, best_average))) => {
                if load.queued != best_queued {
                    load.queued < best_queued
                } else if load.average != best_average {
                    load.average < best_average
                } else {
                    // Equal on both counts: only the processor the task would
                    // rather have displaces the incumbent, so a tie is broken
                    // by the lowest number and the choice is deterministic.
                    cpu == self.prefer
                }
            }
        };
        if better {
            self.best = Some((cpu, score));
        }
    }

    /// The processor chosen, or `None` if nothing permitted was offered.
    #[must_use]
    pub const fn choice(&self) -> Option<usize> {
        if let Some(idle) = self.idle {
            return Some(idle);
        }
        match self.best {
            Some((cpu, _)) => Some(cpu),
            None => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Wake-affine placement
// ---------------------------------------------------------------------------

/// How often a task's flip count is halved, in nanoseconds: once a second,
/// as Linux's `record_wakee` halves at most once per `HZ` jiffies.
pub const FLIP_DECAY_NS: u64 = 1_000_000_000;

/// A waker's flip count and the time it was last halved, after it wakes a
/// task at `now`; `changed` when that task is not the one it woke last
/// (L.sched.91).
///
/// Linux's `record_wakee`: the count is halved first when more than
/// [`FLIP_DECAY_NS`] has passed since the last halving (once, however long
/// it was), and a change of wakee adds one. A task that wakes the same
/// partner every time keeps a count near zero; one that wakes many keeps a
/// high one.
#[must_use]
pub const fn flip(flips: u32, halved_at: u64, now: u64, changed: bool) -> (u32, u64) {
    let (mut flips, halved_at) = if now.saturating_sub(halved_at) > FLIP_DECAY_NS {
        (flips >> 1, now)
    } else {
        (flips, halved_at)
    };
    if changed {
        flips = flips.saturating_add(1);
    }
    (flips, halved_at)
}

/// Whether a wake between tasks with these flip counts is wide, one of
/// many partners on either side, so that pulling the wakee onto its
/// waker's processor would pile a 1:N relationship onto one processor
/// (L.sched.91).
///
/// Linux's `wake_wide`: the smaller count must reach `factor` and the
/// larger must be `factor` times the smaller. `factor` is the number of
/// processors sharing a cache, which for Ferrix is the domain's.
#[must_use]
pub const fn wake_wide(waker_flips: u32, wakee_flips: u32, factor: u32) -> bool {
    let (master, slave) = if waker_flips < wakee_flips {
        (wakee_flips, waker_flips)
    } else {
        (waker_flips, wakee_flips)
    };
    slave >= factor && master as u64 >= slave as u64 * factor as u64
}

/// Whether a wakee whose own load is `task` may be pulled onto its waker's
/// processor, of load average `here`, from its home of load average `home`
/// (L.sched.90). All three in units of [`LOAD_SCALE`].
///
/// Linux's `wake_affine_weight` with `WA_BIAS` and an `imbalance_pct` of
/// 117: the wakee's load is added to the waker's side and taken from the
/// home's, and the home's side is weighed at 108%, half the imbalance, so
/// that the waker's processor wins only with some margin. With a spinner on
/// each side, a wakee busy more than about 8% of the time stays home.
#[must_use]
pub const fn pull_fits(here: u64, home: u64, task: u64) -> bool {
    let this = here as u128 + task as u128;
    let prev = home.saturating_sub(task) as u128;
    this * 100 < prev * 108
}

/// How long a task was busy over a window, in units of [`LOAD_SCALE`]: its
/// runtime over the window's wall time, at most one whole processor
/// (L.sched.90). `None` for an empty window.
#[must_use]
pub const fn utilisation(ran_ns: u64, window_ns: u64) -> Option<u64> {
    if window_ns == 0 {
        return None;
    }
    let share = (ran_ns as u128 * LOAD_SCALE as u128) / window_ns as u128;
    Some(if share > LOAD_SCALE as u128 {
        LOAD_SCALE
    } else {
        share as u64
    })
}

// ---------------------------------------------------------------------------
// Balancing what is already placed
// ---------------------------------------------------------------------------

/// How much busier a processor must be before taking work from it is worth
/// the migration, in units of [`LOAD_SCALE`].
///
/// Half a nice-0 entity's worth of demand. Below this the move costs more in
/// cold cache than it recovers, and — worse — two processors a hair apart
/// would hand the same task back and forth forever, each correctly observing
/// that the other is less loaded.
///
/// Half rather than a whole, because the imbalance moved is half the
/// difference: a threshold of a whole entity would refuse the last move that
/// levels two processors one entity apart.
pub const BALANCE_THRESHOLD: u64 = LOAD_SCALE / 2;

/// How much work would move if `to` took from `from`, or `None` if it is not
/// worth moving any.
///
/// Half the difference, which is the amount that makes the two equal — moving
/// the whole difference would simply exchange which of them is overloaded.
#[must_use]
pub const fn imbalance(from: &CpuLoad, to: &CpuLoad) -> Option<u64> {
    // A processor with one entity has only the running one, and taking it is
    // not balancing, it is stealing the work it is in the middle of.
    if from.queued < 2 {
        return None;
    }

    // **Both tests, and the second is what stops it thrashing.** The load
    // average is the better measure of demand — it knows about weights and
    // duty cycles, which a count does not — but it is deliberately slow, with
    // a half-life of thirty-odd milliseconds. Moving a task does not change
    // it for a long time afterwards, so a balancer that consulted only the
    // average would move one task, see the same imbalance, move another, and
    // keep going until the averages caught up: eight movable tasks were
    // observed moving over a thousand times.
    //
    // The queue count updates the instant a task moves. It is a worse measure
    // and a perfect brake, so it is used as one: a difference of at least two
    // means there is a move that leaves the two closer together rather than
    // merely swapping which is ahead.
    if from.queued < to.queued + 2 {
        return None;
    }

    if from.average <= to.average {
        return None;
    }
    let difference = from.average - to.average;
    if difference < BALANCE_THRESHOLD {
        return None;
    }
    Some(difference / 2)
}

/// The processor `me` should take work from, if any is worth taking.
///
/// Answers the busiest one that clears [`imbalance`], rather than the first,
/// so that a machine with one overloaded processor and several merely busy
/// ones converges on the overloaded one instead of shuffling between the
/// others.
#[must_use]
pub fn busiest(loads: &[CpuLoad], allowed: &CpuSet, me: usize) -> Option<usize> {
    fold(loads, allowed, me).and_then(|fold| fold.pull_from())
}

/// Run [`Balance`] over a slice, for the host tests and for callers that
/// already have one.
fn fold(loads: &[CpuLoad], allowed: &CpuSet, me: usize) -> Option<Balance> {
    let mut balance = Balance::new(*loads.get(me)?);
    for (cpu, load) in loads.iter().enumerate() {
        if cpu != me && allowed.contains(cpu) {
            balance.consider(cpu, *load);
        }
    }
    Some(balance)
}

/// [`busiest`] and [`quietest`] in one pass, one processor at a time.
///
/// Exists for the same reason [`Placement`] does: the caller reads each
/// processor under a different lock, and materialising them into an array
/// sized for [`MAX_CPUS`](crate::MAX_CPUS) costs six kilobytes of a
/// sixteen-kilobyte kernel stack — which this caller is spending from inside
/// an interrupt, on top of whatever it interrupted. The accumulator is five
/// words.
#[derive(Clone, Copy, Debug)]
pub struct Balance {
    /// This processor, which every candidate is compared against.
    mine: CpuLoad,
    /// The busiest processor worth pulling from, and how much would move.
    pull: Option<(usize, u64)>,
    /// The quietest processor worth pushing to, and its load.
    push: Option<(usize, u64)>,
}

impl Balance {
    /// A fold that has seen nothing but the processor asking.
    #[must_use]
    pub const fn new(mine: CpuLoad) -> Balance {
        Balance {
            mine,
            pull: None,
            push: None,
        }
    }

    /// Offer one other processor. The caller has already excluded itself and
    /// anything outside the domain.
    pub const fn consider(&mut self, cpu: usize, load: CpuLoad) {
        if let Some(moving) = imbalance(&load, &self.mine) {
            match self.pull {
                Some((_, most)) if moving <= most => {}
                _ => self.pull = Some((cpu, moving)),
            }
        }
        if imbalance(&self.mine, &load).is_some() {
            match self.push {
                Some((_, quietest)) if load.average >= quietest => {}
                _ => self.push = Some((cpu, load.average)),
            }
        }
    }

    /// The processor to take work from, if any is worth taking.
    #[must_use]
    pub const fn pull_from(&self) -> Option<usize> {
        match self.pull {
            Some((cpu, _)) => Some(cpu),
            None => None,
        }
    }

    /// The processor to give work to, if this one has too much.
    #[must_use]
    pub const fn push_to(&self) -> Option<usize> {
        match self.push {
            Some((cpu, _)) => Some(cpu),
            None => None,
        }
    }
}

/// The processor `me` should give work to, if it has too much.
///
/// The mirror of [`busiest`], and on a tickless kernel it is the one that does
/// the work. A processor with a single runnable task is not interrupted at
/// all — there is nothing to switch to, so arming a timer would buy nothing —
/// which means an *under*-loaded processor never reaches the balancer to pull
/// anything towards itself. The overloaded one is interrupted constantly,
/// because it has tasks to switch between, so it is the one that is awake to
/// notice and the one that must push.
///
/// Answers the quietest processor that clears [`imbalance`] in this
/// direction, so the work goes where it helps most.
#[must_use]
pub fn quietest(loads: &[CpuLoad], allowed: &CpuSet, me: usize) -> Option<usize> {
    fold(loads, allowed, me).and_then(|fold| fold.push_to())
}

// ---------------------------------------------------------------------------
// Slices
// ---------------------------------------------------------------------------
