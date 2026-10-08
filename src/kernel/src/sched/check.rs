//! Stage 5's exit criterion.
//!
//! Four checks, and the last is the stage's own. The first shows a task can
//! be created, run, and cleaned up after. The second shows a sleep is a
//! sleep: the task is off the run queue and the processor is free, and it
//! comes back when it said it would. The third runs a thousand of them at
//! once and requires every stack back afterwards. The fourth measures
//! fairness against the bound EEVDF actually promises, rather than against
//! the eye.
//!
//! # Why the fairness check measures real service
//!
//! The scheduler's own virtual time is the thing under test, so a check
//! written in terms of it could only prove it is self-consistent. What is
//! measured instead is nanoseconds of CPU, counted by the same clock stage 3
//! calibrated, against each task's weighted share of what the group received.
//! The difference is the task's lag, and EEVDF's guarantee is that it stays
//! inside one request.
//!
//! A request is the slice, plus however late the timer cut it — under an
//! emulator whose host can deschedule a whole virtual processor, that lateness
//! is not a rounding error. So the bound is not a constant: each processor
//! adds up every overrun it served while the window was open, and the check
//! requires that processor's worst lag to be inside a slice plus that sum. The
//! sum rather than the worst one, because each overrun is time one task was
//! charged without being chosen for it, and nothing stops them all landing on
//! the same task. And the lags are levelled when the window opens, because
//! what a processor was charged *before* the window is not the window's to
//! repay: a host stall while the first spinner ran alone left its siblings
//! owed sixteen milliseconds, EEVDF paid them inside the window, and the
//! check read the payment as a violation. Both numbers go in the boot log,
//! because a bound that moves is only honest if it is printed.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched::{CpuSet, LOAD_SCALE, NICE_0_WEIGHT, weight_of_nice};
use ferrix_sync::IrqControl;

use super::task::{BLOCKED, Task};
use super::{MIN_SLICE_NS, SLICE_NS, WaitQueue};
use crate::smp::Topology;

/// Threads the many-task check runs.
const THREADS: u64 = 1000;

/// Rounds of arithmetic each one does: bounded work, and enough of it that a
/// processor which took a share of the thousand is busy for long enough to be
/// worth stealing from.
const WORK_ROUNDS: u64 = 20_000;

/// Spinners per processor in the fairness check.
const SPINNERS_PER_CPU: usize = 3;

/// How long the fairness window stays open.
const WINDOW_NANOS: u64 = 150_000_000;

/// How long the sleep check sleeps.
const SLEEP_NANOS: u64 = 20_000_000;

/// How many times [`a_running_processor_is_not_idle`] sleeps and looks.
const IDLE_LOOKS: u32 = 20;

/// How long each of those sleeps is: long enough that the processor halts.
const IDLE_LOOK_SLEEP_NANOS: u64 = 1_000_000;

/// How long to watch confined tasks for a processor they should never reach.
const AFFINITY_WATCH_NANOS: u64 = 60_000_000;

/// How long to let a load average settle before reading it.
///
/// Three of `ferrix_sched`'s 33-millisecond half-lives, which takes a
/// permanently busy processor to about seven eighths of full — comfortably
/// past the half the check asks for, and not the near-perfect convergence it
/// used to wait for.
///
/// **Guest milliseconds are expensive.** Every one of them is emulated, and
/// this file's sleeps are what took the armv7a boot test from 23 seconds to
/// 64 against a 120-second timeout. A check that is three times more precise
/// than its own threshold needs is not three times better, it is three times
/// closer to a boot test that fails on a busy machine.
const LOAD_SETTLE_NANOS: u64 = 100_000_000;

/// How long to give periodic balancing to notice an imbalance.
///
/// Balancing runs at most once every 16 milliseconds per processor and moves
/// one task each time, so this is still several chances per processor rather
/// than one — enough to level a queue that is a few tasks too long, which is
/// what the check builds.
const BALANCE_SETTLE_NANOS: u64 = 120_000_000;

/// How long a wait loop gives the machine before calling it wedged.
///
/// Wall-clock rather than a spin count, because the wait blocks rather than
/// spins: what is being waited for is a thousand tasks getting through their
/// work, and how many times *this* task is woken meanwhile says nothing about
/// how long that took. Generous, because under an emulator it genuinely is;
/// finite, because a scheduler that has lost a task should be a sentence in
/// the boot log rather than the boot test's timeout.
const PATIENCE_NANOS: u64 = 20_000_000_000;

/// What the checks found, for the boot log.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Report {
    /// Threads that ran to completion.
    pub(crate) threads: u64,
    /// Context switches made along the way.
    pub(crate) switches: u64,
    /// Tasks moved between processors by work stealing.
    pub(crate) steals: u64,
    /// Shootdowns taken while the thousand ran and were reaped.
    ///
    /// A thread's kernel stack is given back by invalidating its address on
    /// every processor and waiting for each to answer, so this is the number
    /// that says what one program's threads cost every other program on the
    /// machine. The reaper frees a batch of them under one shootdown
    /// (`crate::sched::REAP_BATCH`), which is why this is far below the
    /// thread count; a boot where it approaches that count is a regression.
    pub(crate) shootdowns: u64,
    /// How many processors ran some of the thousand.
    pub(crate) processors: u32,
    /// Which ones, as a bit per processor. Printed beside the count because
    /// the count on its own invites being read as a capability: stealing is
    /// opportunistic, and a processor that lost every race for a task is not
    /// a processor that could not have run one. The fairness check below is
    /// the one that requires *every* processor to run something.
    pub(crate) processor_mask: u64,
    /// Spinners the fairness check ran.
    pub(crate) spinners: usize,
    /// The worst any task's service strayed from its share.
    pub(crate) worst_lag: u64,
    /// What that had to stay inside: a slice, plus the worst overrun served.
    pub(crate) bound: u64,
    /// How long the sleep check actually slept.
    pub(crate) slept: u64,
    /// Guest milliseconds each of the nine checks took, in order.
    pub(crate) spent_ms: [u64; 9],
    /// Spawns the placer sent to a processor other than the caller's.
    pub(crate) placed_elsewhere: u64,
    /// How many distinct processors new tasks were *placed* on, before any
    /// stealing or balancing could move them.
    pub(crate) placed_on: u32,
    /// The busiest and least busy load averages seen while every processor
    /// was running a spinner.
    pub(crate) load_high: u64,
    /// The load average of an idle processor, after it has decayed.
    pub(crate) load_low: u64,
    /// Tasks periodic balancing moved between processors that were all busy.
    pub(crate) balanced: u64,
    /// The slice handed out with one runnable task, and with many.
    pub(crate) slice_one: u64,
    /// As above, with the queue full.
    pub(crate) slice_many: u64,
}

/// Tasks that have finished a phase.
static DONE: AtomicU64 = AtomicU64::new(0);

/// Tasks in the local wake-up check that have run.
static RAN: AtomicU64 = AtomicU64::new(0);

/// What the workers computed, so their work cannot be optimised away.
static SUM: AtomicU64 = AtomicU64::new(0);

/// Spinners that have started spinning.
static SPINNING: AtomicU64 = AtomicU64::new(0);

/// Tells the spinners to stop.
static STOP: AtomicBool = AtomicBool::new(false);

/// Where the checking task waits for a phase to end.
static FINISHED: WaitQueue = WaitQueue::new();

/// Run them.
///
/// # Errors
///
/// The first check that fails, as a sentence.
pub(crate) fn run(topology: &Topology) -> Result<Report, &'static str> {
    let mut report = Report::default();
    // Taken before anything is started, so that what is counted is what these
    // checks' own thousand threads cost: see `Report::shootdowns`.
    let shootdowns_before = crate::smp::shootdowns();
    // Guest milliseconds per phase, collected and printed once at the end
    // rather than as each finishes: these checks cost the armv7a boot test
    // more than every other stage put together, and an attribution nobody can
    // see is one nobody will act on.
    let mut spent = [0u64; 9];
    let mut at = crate::timer::now_nanos();
    // A macro rather than a closure: a closure would borrow `spent` for the
    // whole of the phases below, and releasing that borrow to read it again
    // means either a scope around everything or a `drop` that lints.
    macro_rules! mark {
        ($index:expr) => {{
            let now = crate::timer::now_nanos();
            if let Some(slot) = spent.get_mut($index) {
                *slot = now.saturating_sub(at) / 1_000_000;
            }
            at = now;
        }};
    }

    super::borrow_check::installed(topology)?;
    one_task()?;
    describes_itself()?;
    a_reaper_without_memory_frees_one_at_a_time(topology)?;
    a_dead_task_is_not_filed_as_a_sleeper()?;
    made_runnable_here_runs_without_another_interrupt()?;
    super::preempt_check::run(topology)?;
    a_running_processor_is_not_idle_across_switches(topology)?;
    a_drained_waiter_survives_its_last_look()?;
    mark!(0);
    sleeping(&mut report)?;
    a_running_processor_is_not_idle()?;
    mark!(1);
    many_tasks(topology, &mut report)?;
    mark!(2);
    fairness(topology, &mut report)?;
    mark!(3);
    placement(topology, &mut report)?;
    mark!(4);
    affinity_is_obeyed(topology)?;
    mark!(5);
    load_tracking(topology, &mut report)?;
    mark!(6);
    a_pulled_task_runs(topology)?;
    balancing(topology, &mut report)?;
    mark!(7);
    slice_scaling(&mut report)?;
    super::borrow_check::run(topology)?;
    mark!(8);
    // The last `mark!` advances `at` for a phase that never comes; reading it
    // here is what says so, rather than an allow.
    let _ = at;
    report.spent_ms = spent;

    let summary = super::summary();
    report.switches = summary.switches;
    report.steals = summary.steals;
    report.shootdowns = crate::smp::shootdowns().saturating_sub(shootdowns_before);
    super::check_invariants()?;
    Ok(report)
}

/// A new task is *placed*, not merely created where its parent happened to be.
///
/// The check is deliberately made before anything can run: it reads where each
/// task was put, not where it ended up. Stealing and balancing would spread
/// these out eventually, and that is exactly what this must not be allowed to
/// pass on — the question is whether the decision was made at all.
fn placement(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    let online = topology.online();
    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);

    let placed_before = super::placed_elsewhere();
    let mut tasks = Vec::with_capacity(online);
    let mut landed = 0u64;

    for _ in 0..online {
        let task = super::spawn("placed", spinner, 0, NICE_0_WEIGHT)?;
        let cpu = task.cpu();
        if cpu < 64 {
            landed |= 1u64 << cpu;
        }
        tasks.push(task);
    }
    report.placed_on = landed.count_ones();

    STOP.store(true, Ordering::Release);
    wait_for(
        || DONE.load(Ordering::Acquire) >= online as u64,
        "a placed task never finished",
    )?;

    // **The decision, not the outcome.** Placement can only be observed where
    // it happens: an idle processor steals a new task within microseconds, so
    // a check that reads `task.cpu()` afterwards is measuring stealing. Tested
    // by removing placement entirely — a check on where the tasks ended up
    // passed regardless, on two processors and on four.
    //
    // This check used to require the tasks to spread over two processors or
    // more, on the premise that every processor was idle. On two processors
    // that premise is false: the task doing the spawning is running on one of
    // them. The first spawn goes to the idle processor, the second finds one
    // task on each and a tie the load average settles in favour of the
    // processor idle longer — both land together, the placer having done
    // exactly the right thing, and the check failed. Deterministically, on
    // hardware, where it was found.
    let placed = super::placed_elsewhere().saturating_sub(placed_before);
    report.placed_elsewhere = placed;
    if online > 1 && placed == 0 {
        crate::console::println!(
            "  place    none of {} spawns chose a processor other than the caller's",
            tasks.len(),
        );
        return Err("placement never sent a new task off the processor that created it");
    }

    reap_to(allocations, "placement")?;
    drop(tasks);
    Ok(())
}

/// A task with an affinity runs only inside it.
///
/// The one property that has to hold even when it costs throughput: a task
/// allowed on two processors of four must never be seen on the other two,
/// however idle they are and however loaded its own are.
fn affinity_is_obeyed(topology: &Topology) -> Result<(), &'static str> {
    let online = topology.online();
    if online < 2 {
        return Ok(());
    }
    let allocations = crate::vmap::usage().allocations;
    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);

    // Processors 0 and 1 only, and more tasks than that can comfortably hold,
    // so an unconstrained balancer would have every reason to spill them.
    let mut allowed = CpuSet::empty();
    allowed.insert(0).map_err(|_| "no processor 0")?;
    allowed.insert(1).map_err(|_| "no processor 1")?;

    let count = online * 2;
    let mut tasks = Vec::with_capacity(count);
    for index in 0..count {
        tasks.push(super::spawn_on(
            "confined",
            spinner,
            index,
            NICE_0_WEIGHT,
            index % 2,
            allowed,
        )?);
    }

    wait_for(
        || SPINNING.load(Ordering::Acquire) >= count as u64,
        "a confined task never started",
    )?;
    // Long enough for a balancer to have moved them if it were going to:
    // it looks every 16 milliseconds per processor, so this is several looks.
    super::sleep_for(AFFINITY_WATCH_NANOS);
    STOP.store(true, Ordering::Release);
    wait_for(
        || DONE.load(Ordering::Acquire) >= count as u64,
        "a confined task never stopped",
    )?;

    for task in &tasks {
        let ran_on = task.cpus_run_on();
        if ran_on & !0b11 != 0 {
            crate::console::println!("  affinity a task allowed on 0b11 ran on {ran_on:#b}",);
            return Err("a task ran on a processor its affinity excluded");
        }
    }

    reap_to(allocations, "affinity")?;
    drop(tasks);
    Ok(())
}

/// A processor running something reads as loaded; one running nothing decays.
fn load_tracking(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    let online = topology.online();
    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);

    // One spinner pinned to every processor but the last, so the last is the
    // control: the same machine, the same moment, nothing to run.
    let busy_cpus = online.saturating_sub(1).max(1);
    let mut tasks = Vec::with_capacity(busy_cpus);
    for cpu in 0..busy_cpus {
        tasks.push(super::spawn_on(
            "loaded",
            spinner,
            cpu,
            NICE_0_WEIGHT,
            cpu,
            CpuSet::of(cpu),
        )?);
    }
    wait_for(
        || SPINNING.load(Ordering::Acquire) >= busy_cpus as u64,
        "a load-test task never started",
    )?;

    // Several half-lives, so the average is near its steady state rather than
    // still climbing.
    super::sleep_for(LOAD_SETTLE_NANOS);

    let mut lowest_busy = u64::MAX;
    for cpu in 0..busy_cpus {
        let load = super::cpu_report(cpu)
            .ok_or("a processor has no queue")?
            .load;
        lowest_busy = lowest_busy.min(load);
    }
    report.load_high = lowest_busy;
    if online > 1 {
        let idle = super::cpu_report(online - 1).ok_or("a processor has no queue")?;
        report.load_low = idle.load;
    }

    STOP.store(true, Ordering::Release);
    wait_for(
        || DONE.load(Ordering::Acquire) >= busy_cpus as u64,
        "a load-test task never stopped",
    )?;

    // Half full is a generous floor for a processor that has had a task
    // spinning on it for several half-lives; the point is to catch an average
    // that never moves, not to pin down its exact value.
    if report.load_high < LOAD_SCALE / 2 {
        return Err("a processor running a spinner did not read as loaded");
    }
    if online > 1 && report.load_low >= report.load_high {
        return Err("an idle processor read as loaded as a busy one");
    }

    reap_to(allocations, "load tracking")?;
    drop(tasks);
    Ok(())
}

/// Work moves between processors that are all busy.
///
/// The case work stealing cannot reach, and the reason periodic balancing
/// exists: stealing happens when a processor runs out of work, so a machine
/// where no processor ever does is a machine stealing never touches. Every
/// processor here has a spinner pinned to it, so none of them ever idles, and
/// the movable tasks all start on one.
fn balancing(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    let online = topology.online();
    if online < 2 {
        return Ok(());
    }
    let allocations = crate::vmap::usage().allocations;
    let before = super::balanced_count();
    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);

    let everywhere = CpuSet::first(online).map_err(|_| "too many processors for a set")?;
    let movable = online * 2;
    let total = online + movable;

    let mut tasks = Vec::with_capacity(total);
    // The floor: one per processor, pinned, so nothing ever goes idle.
    for cpu in 0..online {
        tasks.push(super::spawn_on(
            "anchor",
            spinner,
            cpu,
            NICE_0_WEIGHT,
            cpu,
            CpuSet::of(cpu),
        )?);
    }

    // **Every anchor must be *running* before the imbalance is created.**
    // Until then the other processors are still idle, and an idle processor
    // steals — so the movable tasks would be spread by the mechanism this
    // check is meant to exclude, and it would pass without periodic balancing
    // existing at all. That is how this check first failed: not because
    // balancing was broken, but because stealing beat it to the work.
    if let Err(problem) = wait_for(
        || SPINNING.load(Ordering::Acquire) >= online as u64,
        "an anchor task never started",
    ) {
        describe_unstarted(&tasks, SPINNING.load(Ordering::Acquire), |index| index);
        return Err(problem);
    }

    // The imbalance: all of them on processor 0, free to move. No processor
    // will go idle from here until STOP, so nothing but `balance` can move
    // them.
    for index in 0..movable {
        tasks.push(super::spawn_on(
            "movable",
            spinner,
            index,
            NICE_0_WEIGHT,
            0,
            everywhere,
        )?);
    }

    if let Err(problem) = wait_for(
        || SPINNING.load(Ordering::Acquire) >= total as u64,
        "a balancing task never started",
    ) {
        // Anchors wanted their own processor; the movable ones were all put
        // on processor 0 and may since have been moved anywhere.
        describe_unstarted(&tasks, SPINNING.load(Ordering::Acquire), |index| {
            if index < online { index } else { 0 }
        });
        return Err(problem);
    }
    super::sleep_for(BALANCE_SETTLE_NANOS);

    let moved = super::balanced_count().saturating_sub(before);
    report.balanced = moved;

    // Sampled while the tasks are still running: after STOP every queue is
    // empty and every load average is on its way to zero, which says nothing
    // about the state the balancer was looking at.
    let mut spread = Vec::with_capacity(online);
    for cpu in 0..online {
        spread.push(super::cpu_report(cpu).map(|report| (report.load, report.queued)));
    }

    STOP.store(true, Ordering::Release);
    wait_for(
        || DONE.load(Ordering::Acquire) >= total as u64,
        "a balancing task never stopped",
    )?;

    if moved == 0 {
        for (cpu, sample) in spread.iter().enumerate() {
            if let Some((load, queued)) = sample {
                crate::console::println!("  balance  cpu {cpu} load {load} queued {queued}");
            }
        }
        crate::console::println!(
            "  balance  {movable} movable tasks stayed on one processor of {online}",
        );
        return Err("no task was balanced away from an overloaded processor");
    }

    reap_to(allocations, "balancing")?;
    drop(tasks);
    Ok(())
}

/// The slice shrinks as more becomes runnable, and stops at the floor.
fn slice_scaling(report: &mut Report) -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    let here = super::current()
        .ok_or("the checking task is not running")?
        .cpu();

    report.slice_one = super::cpu_report(here).ok_or("no queue here")?.slice_ns;

    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);

    // Enough on this one processor to take the slice to its floor, which the
    // target latency reaches at eight runnable. More would only cost guest
    // milliseconds to demonstrate the same floor.
    let crowd = 8;
    let mut tasks = Vec::with_capacity(crowd);
    for index in 0..crowd {
        tasks.push(super::spawn_on(
            "crowd",
            spinner,
            index,
            NICE_0_WEIGHT,
            here,
            CpuSet::of(here),
        )?);
    }
    wait_for(
        || SPINNING.load(Ordering::Acquire) >= crowd as u64,
        "a crowding task never started",
    )?;
    report.slice_many = super::cpu_report(here).ok_or("no queue here")?.slice_ns;

    STOP.store(true, Ordering::Release);
    wait_for(
        || DONE.load(Ordering::Acquire) >= crowd as u64,
        "a crowding task never stopped",
    )?;

    if report.slice_many >= report.slice_one {
        return Err("the slice did not shrink as the run queue filled");
    }
    if report.slice_many < MIN_SLICE_NS {
        return Err("the slice fell below the floor");
    }

    reap_to(allocations, "slice scaling")?;
    drop(tasks);
    Ok(())
}

/// Tell anything waiting that a counter it might be watching has moved.
///
/// **Every store to a counter a `wait_for` predicate reads must be followed by
/// one of these.** The wait queue is the only thing that can end a wait early;
/// without a notify the waiter sleeps its entire deadline and *then* finds the
/// condition true, so the check passes and costs exactly `PATIENCE_NANOS`.
///
/// That is not hypothetical, and it is why this file needed a comment rather
/// than a convention. `SPINNING` was incremented without one, and all seven
/// waits for "a spinner never started" paid twenty seconds each — around forty
/// seconds of the armv7a boot test, on a run that reported success. It took
/// per-phase timings to see it at all, because a silent tax looks exactly like
/// a slow machine.
fn notify() {
    FINISHED.wake_all();
}

/// Count this task as finished, and wake whoever is waiting for the phase.
fn finish() {
    let _ = DONE.fetch_add(1, Ordering::AcqRel);
    notify();
}

/// Bounded work, and a number at the end that nothing can fold away.
fn worker(argument: usize) {
    let mut value = argument as u64 | 1;
    for _ in 0..WORK_ROUNDS {
        value = value.wrapping_mul(2_654_435_761).rotate_left(7) ^ 0x9E37_79B9;
    }
    let _ = SUM.fetch_add(value & 0xFF, Ordering::Relaxed);
    finish();
}

/// Run until told to stop, never blocking: the only way one of these gets off
/// its processor is by being preempted, which is what the fairness check is
/// about.
fn spinner(_argument: usize) {
    let _ = SPINNING.fetch_add(1, Ordering::AcqRel);
    // The counter every "a spinner never started" wait watches. Without this
    // the waiter has nothing to wake it and sleeps its whole budget.
    notify();
    while !STOP.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    finish();
}

/// Wait for `ready`, giving the processor up meanwhile, and give up rather
/// than hang if it never becomes true.
fn wait_for(ready: impl FnMut() -> bool, what: &'static str) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if FINISHED.wait_until_deadline(ready, deadline) {
        Ok(())
    } else {
        Err(what)
    }
}

/// Free every exited task's stack, and require the arena to come back to
/// where it started.
fn reap_to(allocations: usize, what: &str) -> Result<(), &'static str> {
    // Still a yielding loop, and deliberately: reaping is work *this* task
    // does, so it has to keep being given the processor to do it. Blocking
    // here would wait for something nobody is going to do.
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let _ = super::reap();
        if crate::vmap::usage().allocations <= allocations {
            return Ok(());
        }
        if crate::timer::now_nanos() >= deadline {
            let usage = crate::vmap::usage();
            crate::console::println!(
                "  tasks    after {what}: arena holds {} allocations, expected {allocations}",
                usage.allocations,
            );
            return Err("a task's stack was never given back");
        }
        super::yield_now();
    }
}

/// One task runs, computes what it was given, exits, and is cleaned up after.
fn one_task() -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    DONE.store(0, Ordering::Release);
    SUM.store(0, Ordering::Release);

    let task = super::spawn("check-one", worker, 1, NICE_0_WEIGHT)?;
    wait_for(
        || DONE.load(Ordering::Acquire) >= 1,
        "a single task never finished",
    )?;
    if SUM.load(Ordering::Acquire) == 0 {
        return Err("a task finished without doing its work");
    }
    if task.switches() == 0 {
        return Err("a task finished without ever being switched to");
    }

    reap_to(allocations, "one task")?;
    drop(task);
    Ok(())
}

/// How far out the woken task's abandoned deadline is: far enough that, if it
/// is filed, the timer does not make the dead task runnable before this check
/// has looked, which would be a crash rather than a sentence.
const ABANDONED_SLEEP_NANOS: u64 = 3_600_000_000_000;

/// A task woken between marking itself blocked and blocking, which then
/// exits, is not filed as a sleeper under the deadline it never slept on.
///
/// The window every wait has: the task is blocked with a deadline set and has
/// not reached `block` yet, and a waker on another processor gets there first.
/// The task makes itself its own waker here, with interrupts masked, so the
/// window is hit every time rather than once in a long while.
fn a_dead_task_is_not_filed_as_a_sleeper() -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    let here = super::current()
        .ok_or("the checking task is not running")?
        .cpu();
    DONE.store(0, Ordering::Release);

    // On this processor, so that once the task is seen dead the switch away
    // from it, which is what files a sleeper, has already been made.
    let task = super::spawn_on(
        "check-woken",
        woken_before_blocking,
        0,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    wait_for(
        || DONE.load(Ordering::Acquire) >= 1 && task.is_dead(),
        "a task woken before it blocked never exited",
    )?;

    // Unfiled before anything is reported, so a failure is a sentence and not
    // a dead task the timer wakes an hour from now.
    if super::unfile_sleeper(task.id) {
        return Err("a dead task was filed as a sleeper under a deadline it abandoned");
    }

    reap_to(allocations, "a woken task")?;
    drop(task);
    Ok(())
}

/// Set a deadline and block, and be woken before blocking: what a waiter looks
/// like when its waker wins the race to `block`. Then exit.
fn woken_before_blocking(_argument: usize) {
    if let Some(me) = super::current() {
        let saved = <crate::arch::Irq as IrqControl>::disable();
        me.set_sleep_deadline(crate::timer::now_nanos().saturating_add(ABANDONED_SLEEP_NANOS));
        me.set_state(BLOCKED);
        super::wake(&me);
        <crate::arch::Irq as IrqControl>::restore(saved);
    }
    finish();
}

/// How long the checking task spins, never yielding, for a task it made
/// runnable on its own processor to get a turn.
const LOCAL_WAKE_PATIENCE_NANOS: u64 = 200_000_000;

/// A task spawned or woken onto the processor that did it runs without any
/// other interrupt arriving there.
///
/// The spawner and the waker are this task, which spins while it waits. That
/// is a task in a system call or a kernel thread, which is where a spawn or a
/// wake-up onto its own processor comes from. It leaves through no interrupt
/// exit, the one place a reschedule request was read. A processor running
/// one task has its timer stopped, and a spin lets nothing else in, so only
/// the scheduler's own arrangements can get the new task a turn. Each half
/// begins with a yield, which re-arms this processor's timer for what is on
/// it now. A timer still armed for an earlier sleep would rescue the task for
/// the wrong reason.
///
/// Verifies: L.sched.52
fn made_runnable_here_runs_without_another_interrupt() -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    let here = super::current()
        .ok_or("the checking task is not running")?
        .cpu();
    RAN.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);

    super::yield_now();
    let spawned = super::spawn_on(
        "check-spawned",
        count_a_run,
        0,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    if !spin_until(|| RAN.load(Ordering::Acquire) >= 1) {
        return Err(
            "a task spawned onto its creator's processor waited for an unrelated interrupt",
        );
    }
    // **Its stack given back before the second half, not after.** Freeing a
    // stack invalidates every processor's translations, and on x86-64 that is
    // an interrupt sent to each of them. An idle processor that reaps the
    // first task during the second half's spin sends one here, and its exit
    // makes the decision the second half is asking the scheduler to arrange,
    // passing with the fix reverted. That is what this check first did.
    wait_for(
        || DONE.load(Ordering::Acquire) >= 1,
        "a locally spawned task never finished",
    )?;
    reap_to(allocations, "a locally spawned task")?;

    let parked = super::spawn_on(
        "check-parked",
        park_then_count_a_run,
        0,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    // Off the queue, not only marked: this task runs again only once the
    // parked one has been switched away from, so both are true by then.
    wait_for(
        || parked.state() == BLOCKED && !parked.is_queued(),
        "a task that blocked itself never left its run queue",
    )?;
    super::yield_now();
    super::wake(&parked);
    if !spin_until(|| RAN.load(Ordering::Acquire) >= 2) {
        return Err("a task woken onto its waker's processor waited for an unrelated interrupt");
    }

    wait_for(
        || DONE.load(Ordering::Acquire) >= 2,
        "a locally woken task never finished",
    )?;
    reap_to(allocations, "local wake-ups")?;
    drop((spawned, parked));
    Ok(())
}

/// Spin, with interrupts on and without yielding, until `ready` or
/// [`LOCAL_WAKE_PATIENCE_NANOS`] passes. Answers whether `ready` was the
/// reason.
fn spin_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = crate::timer::now_nanos().saturating_add(LOCAL_WAKE_PATIENCE_NANOS);
    while !ready() {
        if crate::timer::now_nanos() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// The longest a deaf spinner keeps interrupts masked, whether or not it is
/// told to stop. A bound, so that anything that turns out to need that
/// processor's answer, such as a shootdown, is late rather than wedged.
const DEAF_LIMIT_NANOS: u64 = 2_000_000_000;

/// A task pulled onto this processor by balancing gets a turn without any
/// other interrupt arriving here.
///
/// The pull is the same one `balance` makes, made from here. The task pulled
/// waits behind a spinner on another processor that runs with interrupts
/// masked, so that processor can neither run the task first nor be told to.
/// This task then spins without yielding, as a processor's lone task does
/// with its timer stopped, and requires the pulled task to run.
fn a_pulled_task_runs(topology: &Topology) -> Result<(), &'static str> {
    let online = topology.online();
    if online < 2 {
        return Ok(());
    }
    let allocations = crate::vmap::usage().allocations;
    let here = super::current()
        .ok_or("the checking task is not running")?
        .cpu();
    let there = (here + 1) % online;
    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);
    RAN.store(0, Ordering::Release);

    let deaf = super::spawn_on(
        "check-deaf",
        deaf_spinner,
        0,
        NICE_0_WEIGHT,
        there,
        CpuSet::of(there),
    )?;
    wait_for(
        || SPINNING.load(Ordering::Acquire) >= 1,
        "a spinner with interrupts masked never started",
    )?;
    let mut both = CpuSet::of(here);
    both.insert(there)
        .map_err(|_| "a processor outside the set's range")?;
    let pulled = super::spawn_on("check-pulled", count_a_run, 0, NICE_0_WEIGHT, there, both)?;

    super::yield_now();
    let saved = <crate::arch::Irq as IrqControl>::disable();
    let moved = super::pull(here, there);
    <crate::arch::Irq as IrqControl>::restore(saved);
    let ran = moved && spin_until(|| RAN.load(Ordering::Acquire) >= 1);

    STOP.store(true, Ordering::Release);
    wait_for(
        || DONE.load(Ordering::Acquire) >= 2,
        "a pulled task or its deaf neighbour never finished",
    )?;
    if !moved {
        return Err("a task queued behind a busy processor could not be pulled");
    }
    if !ran {
        return Err("a task pulled by balancing waited for an unrelated interrupt");
    }

    reap_to(allocations, "a pulled task")?;
    drop((deaf, pulled));
    Ok(())
}

/// Spin with interrupts masked until told to stop, or until
/// [`DEAF_LIMIT_NANOS`] has passed.
fn deaf_spinner(_argument: usize) {
    let saved = <crate::arch::Irq as IrqControl>::disable();
    let _ = SPINNING.fetch_add(1, Ordering::AcqRel);
    notify();
    let until = crate::timer::now_nanos().saturating_add(DEAF_LIMIT_NANOS);
    while !STOP.load(Ordering::Acquire) && crate::timer::now_nanos() < until {
        core::hint::spin_loop();
    }
    <crate::arch::Irq as IrqControl>::restore(saved);
    finish();
}

/// Count a run, and finish.
fn count_a_run(_argument: usize) {
    let _ = RAN.fetch_add(1, Ordering::AcqRel);
    finish();
}

/// Block with no deadline and on no wait queue, so that only a direct wake-up
/// brings this back, then count a run.
fn park_then_count_a_run(_argument: usize) {
    if let Some(me) = super::current() {
        me.set_state(BLOCKED);
        super::block();
    }
    count_a_run(0);
}

/// A sleep gives the processor up and comes back on time.
/// Verifies: `L.x86_64.91`
fn sleeping(report: &mut Report) -> Result<(), &'static str> {
    let started = crate::timer::now_nanos();
    super::sleep_for(SLEEP_NANOS);
    let elapsed = crate::timer::now_nanos().saturating_sub(started);
    report.slept = elapsed;

    if elapsed < SLEEP_NANOS {
        return Err("a sleep came back before its deadline");
    }
    if elapsed > SLEEP_NANOS * 20 {
        return Err("a sleep came back an order of magnitude late");
    }
    Ok(())
}

/// A processor running a task does not read as idle, however the task got
/// there. The common way is the one that used to miss: the processor halts in
/// its idle loop, a timer interrupt wakes a sleeper, and the interrupt's exit
/// switches straight to it -- so the idle task, preempted on its way out of
/// the halt, never cleared its mark. Other processors then took the machine
/// for quiet and reaped early, which made a reaper check flaky. Each of these
/// sleeps ends that way whenever nothing else is runnable here.
fn a_running_processor_is_not_idle() -> Result<(), &'static str> {
    for _ in 0..IDLE_LOOKS {
        super::sleep_for(IDLE_LOOK_SLEEP_NANOS);
        if super::this_cpu_reads_as_idle() {
            return Err("a processor running a task read as idle to the rest of the machine");
        }
    }
    Ok(())
}

/// A thousand threads, started from one processor, run to completion — and
/// every stack comes back.
/// Verifies: `L.x86_64.17`
fn many_tasks(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    DONE.store(0, Ordering::Release);
    SUM.store(0, Ordering::Release);

    // All of them onto the processor doing the spawning, so that the only way
    // the others get any is by taking them.
    let mut tasks = Vec::with_capacity(THREADS as usize);
    for index in 0..THREADS {
        tasks.push(super::spawn(
            "worker",
            worker,
            index as usize,
            NICE_0_WEIGHT,
        )?);
    }

    wait_for(
        || DONE.load(Ordering::Acquire) >= THREADS,
        "not every thread finished",
    )?;

    let mut processors = 0u64;
    for task in &tasks {
        if task.switches() == 0 {
            return Err("a thread finished without ever being switched to");
        }
        processors |= task.cpus_run_on();
    }
    report.threads = DONE.load(Ordering::Acquire);
    report.processors = processors.count_ones();
    report.processor_mask = processors;

    if topology.online() > 1 && report.processors < 2 {
        return Err("every thread ran on one processor: work stealing moved nothing");
    }

    reap_to(allocations, "a thousand tasks")?;
    drop(tasks);
    Ok(())
}

/// Tasks that never sleep get shares in proportion to their weights, and no
/// task's share strays further from its due than EEVDF allows.
fn fairness(topology: &Topology, report: &mut Report) -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    DONE.store(0, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    STOP.store(false, Ordering::Release);

    let spinners = start_spinners(topology)?;
    let total = spinners.len() as u64;
    if let Err(problem) = wait_for(
        || SPINNING.load(Ordering::Acquire) >= total,
        "a spinner never started",
    ) {
        describe_unstarted(&spinners, SPINNING.load(Ordering::Acquire), |index| {
            index / SPINNERS_PER_CPU
        });
        return Err(problem);
    }

    // The window opens once every spinner is running, and this task sleeps
    // through it: a processor whose queue holds only the spinners is what
    // makes the measurement a measurement of them.
    super::open_window(&spinners);
    super::sleep_for(WINDOW_NANOS);
    super::close_window(&spinners);

    STOP.store(true, Ordering::Release);
    wait_for(
        || DONE.load(Ordering::Acquire) >= total,
        "a spinner never stopped",
    )?;

    report.spinners = spinners.len();
    // Per processor: the lag a queue measured is bounded by the overruns that
    // queue served, not by another's. The boot log carries the worst lag and
    // the bound it was held to.
    let mut bounds = Vec::with_capacity(topology.online());
    for cpu in 0..topology.online() {
        let measured = super::cpu_report(cpu).ok_or("a processor has no queue")?;
        let bound = SLICE_NS.saturating_add(measured.overrun_total);
        if measured.worst_lag > report.worst_lag {
            report.worst_lag = measured.worst_lag;
            report.bound = bound;
        }
        bounds.push(bound);
    }

    if bounds
        .iter()
        .enumerate()
        .any(|(cpu, bound)| super::cpu_report(cpu).is_some_and(|m| m.worst_lag > *bound))
    {
        describe_shares(topology, &spinners, &bounds);
        return Err("a task's service strayed further from its share than EEVDF allows");
    }
    shares_are_proportional(topology, &spinners, &bounds)?;

    reap_to(allocations, "fairness")?;
    drop(spinners);
    Ok(())
}

/// The weights the spinners on each processor are given: two at nice 0 and
/// one at nice -3, so the check covers equal shares and unequal ones.
fn spinner_weight(index: usize) -> u32 {
    if index == SPINNERS_PER_CPU - 1 {
        weight_of_nice(-3).unwrap_or(NICE_0_WEIGHT)
    } else {
        NICE_0_WEIGHT
    }
}

/// Start the spinners, pinned so that each processor's queue is a fixed set.
fn start_spinners(topology: &Topology) -> Result<Vec<Arc<Task>>, &'static str> {
    let mut spinners = Vec::new();
    for cpu in 0..topology.online() {
        for index in 0..SPINNERS_PER_CPU {
            spinners.push(super::spawn_on(
                "spinner",
                spinner,
                cpu,
                spinner_weight(index),
                cpu,
                CpuSet::of(cpu),
            )?);
        }
    }
    Ok(spinners)
}

/// Check, from outside the scheduler, that each processor's spinners got CPU
/// in proportion to their weights.
///
/// The same claim the scheduler measured for itself while the window was
/// open, recomputed here from the runtime counters — so a scheduler whose
/// own measurement was wrong in the same way as its accounting still has to
/// answer to this one.
fn shares_are_proportional(
    topology: &Topology,
    spinners: &[Arc<Task>],
    bounds: &[u64],
) -> Result<(), &'static str> {
    for cpu in 0..topology.online() {
        let bound = bounds.get(cpu).copied().unwrap_or(SLICE_NS);
        let group: Vec<&Arc<Task>> = spinners
            .iter()
            .skip(cpu * SPINNERS_PER_CPU)
            .take(SPINNERS_PER_CPU)
            .collect();

        let total: u128 = group
            .iter()
            .map(|task| u128::from(task.measured_runtime()))
            .sum();
        let weights: u128 = group
            .iter()
            .map(|task| u128::from(task.entity_state().weight))
            .sum();
        if total == 0 || weights == 0 {
            return Err("a processor's spinners ran for no time at all");
        }

        for task in group {
            let had = u128::from(task.measured_runtime());
            let share = total * u128::from(task.entity_state().weight) / weights;
            if share.abs_diff(had) > u128::from(bound) {
                describe_shares(topology, spinners, bounds);
                return Err("a task's share of its processor was not its weight's share");
            }
        }
    }
    Ok(())
}

/// Print what every spinner had against what it was due, and what each
/// processor measured, for a fairness failure that would otherwise be one
/// sentence about "a task".
///
/// Which task it was says whether the weights or the accounting is at fault,
/// and which processor says whether the host stalled one of them.
fn describe_shares(topology: &Topology, spinners: &[Arc<Task>], bounds: &[u64]) {
    for cpu in 0..topology.online() {
        let bound = bounds.get(cpu).copied().unwrap_or(SLICE_NS);
        if let Some(report) = super::cpu_report(cpu) {
            crate::console::println!(
                "  fair     cpu {cpu} measured worst lag {} us against a bound of {} us; overruns {} us in all, {} us at worst; {} picks, {} against the scan",
                report.worst_lag / 1000,
                bound / 1000,
                report.overrun_total / 1000,
                report.worst_overrun / 1000,
                report.picks,
                report.wrong_picks,
            );
        }
        let group: Vec<&Arc<Task>> = spinners
            .iter()
            .skip(cpu * SPINNERS_PER_CPU)
            .take(SPINNERS_PER_CPU)
            .collect();
        let total: u128 = group
            .iter()
            .map(|task| u128::from(task.measured_runtime()))
            .sum();
        let weights: u128 = group
            .iter()
            .map(|task| u128::from(task.entity_state().weight))
            .sum();
        for task in group {
            let had = u128::from(task.measured_runtime());
            let share = (total * u128::from(task.entity_state().weight))
                .checked_div(weights)
                .unwrap_or(0);
            crate::console::println!(
                "  fair     {} #{} weight {} on cpu {} had {} us of {} us due, bound {} us, {} switches",
                task.name,
                task.id,
                task.entity_state().weight,
                cpu,
                had / 1000,
                share / 1000,
                bound / 1000,
                task.switches(),
            );
        }
        super::print_picks(cpu);
    }
}

/// Print where each task that should have started is, and what every queue
/// holds, for a wait that gave up on them.
fn describe_unstarted(tasks: &[Arc<Task>], started: u64, wanted_cpu: impl Fn(usize) -> usize) {
    crate::console::println!("  tasks    {} of {} started", started, tasks.len());
    for (index, task) in tasks.iter().enumerate() {
        crate::console::println!(
            "  tasks    {} {} wanted cpu {}, is on cpu {}, ran on {:#b}, {} switches, state {}",
            task.name,
            index,
            wanted_cpu(index),
            task.cpu(),
            task.cpus_run_on(),
            task.switches(),
            task.state(),
        );
    }
    super::report_queues();
}

/// Before the scheduler runs: a sleep, a wait on one queue and a wait on
/// several spin until their deadlines, since there is no task to switch away
/// from and nothing to be woken by, and the processors' times read as none,
/// since there is no queue to read them from.
///
/// # Errors
///
/// The first of these that did not hold, as a sentence.
pub(crate) fn before_start() -> Result<(), &'static str> {
    if super::started() {
        return Err("the checks for before the scheduler ran after it started");
    }
    match super::cpu_times() {
        Ok(times) if times.is_empty() => {}
        _ => return Err("processor times were read before there were queues to read"),
    }

    let deadline = crate::timer::now_nanos().saturating_add(EARLY_WAIT_NANOS);
    super::sleep_until(deadline);
    if crate::timer::now_nanos() < deadline {
        return Err("a sleep before the scheduler ended before its deadline");
    }

    let queue = WaitQueue::new();
    let other = WaitQueue::new();
    let deadline = crate::timer::now_nanos().saturating_add(EARLY_WAIT_NANOS);
    if queue.wait_until_deadline(|| false, deadline) || crate::timer::now_nanos() < deadline {
        return Err("a wait before the scheduler ended before its deadline, or as satisfied");
    }
    let deadline = crate::timer::now_nanos().saturating_add(EARLY_WAIT_NANOS);
    if WaitQueue::wait_on_any(&[&queue, &other], || false, deadline, EARLY_WAIT_NANOS / 4)
        || crate::timer::now_nanos() < deadline
    {
        return Err("a wait on two queues before the scheduler ended early, or as satisfied");
    }
    if !queue.wait_until_deadline(|| true, 0) {
        return Err("a wait already satisfied before the scheduler was not");
    }
    Ok(())
}

/// How long each of [`before_start`]'s waits lasts.
const EARLY_WAIT_NANOS: u64 = 1_000_000;

/// A task and a wait queue print as what they are: what a failure report
/// that names one shows. The task's print carries its number and name.
fn describes_itself() -> Result<(), &'static str> {
    let me = super::current().ok_or("the checking task is not running")?;
    let printed = alloc::format!("{:?}", *me);
    let named = alloc::format!("name: {:?}", me.name);
    if !printed.starts_with("Task {") || !printed.contains(&named) {
        return Err("a task does not print as itself");
    }
    if !alloc::format!("{:?}", WaitQueue::new()).starts_with("WaitQueue {") {
        return Err("a wait queue does not print as one");
    }
    Ok(())
}

/// With no memory for its lists, the reaper frees dead tasks one at a time,
/// and gives every stack back all the same (finding F-23).
///
/// No idle loop may reap the dead tasks first, and one reaps whenever it
/// believes every processor idle -- which a processor whose idle task was
/// preempted on its way out of a halt used to claim to be, until the switch
/// cleared the mark ([`a_running_processor_is_not_idle`]), and which every
/// processor with nothing to run truly is. So no idle loop runs at all while
/// the tasks die: every other processor holds a spinning
/// task of this check's, and the two tasks that die are pinned here, where
/// this task stays runnable between them. The reap is then made with every
/// allocation of this task failing.
fn a_reaper_without_memory_frees_one_at_a_time(topology: &Topology) -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    let me = super::current().ok_or("the checking task is not running")?;
    let here = me.cpu();
    HOLD.store(true, Ordering::Release);
    HOLDING.store(0, Ordering::Release);
    let mut holders = Vec::new();
    for cpu in (0..topology.online()).filter(|&cpu| cpu != here) {
        holders.push(super::spawn_on(
            "check-hold",
            hold,
            0,
            NICE_0_WEIGHT,
            cpu,
            CpuSet::of(cpu),
        )?);
    }
    let outcome = reap_while_held(&me, here, holders.len() as u64);
    HOLD.store(false, Ordering::Release);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !holders.iter().all(|holder| holder.is_dead()) {
        if crate::timer::now_nanos() >= deadline {
            return Err("a task holding a processor for the reaper check never ended");
        }
        super::yield_now();
    }
    drop(holders);
    let (reaped, failed) = outcome?;
    if reaped < 2 || failed == 0 {
        crate::console::println!("  reap     {reaped} reaped with {failed} allocations failed");
        return Err("the reaper without memory did not free the dead tasks one at a time");
    }
    reap_to(allocations, "a reap without memory")
}

/// Whether the reaper check's holders keep spinning.
static HOLD: AtomicBool = AtomicBool::new(false);
/// How many of them have started.
static HOLDING: AtomicU64 = AtomicU64::new(0);

/// Keep a processor from running its idle loop until [`HOLD`] is cleared.
fn hold(_argument: usize) {
    let _ = HOLDING.fetch_add(1, Ordering::AcqRel);
    while HOLD.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// [`a_reaper_without_memory_frees_one_at_a_time`] once the other processors
/// are held: two tasks die here, and are reaped with no memory. Answers how
/// many were reaped and how many allocations failed.
fn reap_while_held(me: &Task, here: usize, holders: u64) -> Result<(usize, u64), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while HOLDING.load(Ordering::Acquire) < holders {
        if crate::timer::now_nanos() >= deadline {
            return Err("a task holding a processor for the reaper check never started");
        }
        super::yield_now();
    }
    DONE.store(0, Ordering::Release);
    let first = super::spawn_on(
        "check-reap-a",
        worker,
        1,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    let second = super::spawn_on(
        "check-reap-b",
        worker,
        2,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    while DONE.load(Ordering::Acquire) < 2
        || !first.is_dead()
        || !second.is_dead()
        || super::ZOMBIES.lock().len() < 2
    {
        if crate::timer::now_nanos() >= deadline {
            return Err("two tasks for the reaper never reached it");
        }
        super::yield_now();
    }
    drop((first, second));
    crate::fallible::inject(me.id, 1);
    let reaped = super::reap();
    Ok((reaped, crate::fallible::stop_injecting()))
}

/// Resumptions after a switch [`a_running_processor_is_not_idle_across_switches`]'s
/// tasks make between them.
const IDLE_SWITCHES: u64 = 10_000;

/// Every how many turns each of its tasks sleeps instead of yielding.
const IDLE_SLEEP_EVERY: u64 = 16;

/// Its resumptions so far.
static IDLE_RESUMED: AtomicU64 = AtomicU64::new(0);

/// Its resumptions that followed a sleep.
static IDLE_SLEPT: AtomicU64 = AtomicU64::new(0);

/// Set by one of its tasks that found its processor reading as idle.
static READ_AS_IDLE: AtomicBool = AtomicBool::new(false);

/// A processor running a task never reads as idle, after a switch between
/// two tasks or one from the idle task (`docs/OPAQUE-KERNEL.md` §9.8, 2f,
/// where a switch writes `IDLE` only when the bit changes). Two tasks a
/// processor yield to each other until they have resumed after a switch
/// [`IDLE_SWITCHES`] times between them, and every [`IDLE_SLEEP_EVERY`]th
/// turn each sleeps instead, so that a processor whose tasks both sleep
/// halts in its idle loop and a timer interrupt's exit switches from the
/// idle task straight to the sleeper. Each looks at its processor's bit
/// after every resumption.
///
/// Verifies: L.sched.50
fn a_running_processor_is_not_idle_across_switches(
    topology: &Topology,
) -> Result<(), &'static str> {
    IDLE_RESUMED.store(0, Ordering::Release);
    IDLE_SLEPT.store(0, Ordering::Release);
    READ_AS_IDLE.store(false, Ordering::Release);
    DONE.store(0, Ordering::Release);
    let tasks = topology.online().saturating_mul(2);
    let mut running: Vec<Arc<Task>> = Vec::with_capacity(tasks);
    for index in 0..tasks {
        running.push(super::spawn(
            "check-idle",
            switch_and_look,
            index,
            NICE_0_WEIGHT,
        )?);
    }
    wait_for(
        || DONE.load(Ordering::Acquire) >= tasks as u64,
        "a task of the idle-bit check never finished",
    )?;
    for task in &running {
        super::wait_until_gone(task, super::REAPER_PATIENCE_NANOS)?;
    }
    drop(running);
    crate::console::println!(
        "  idle     {} resumptions, {} after a sleep; read as idle: {}",
        IDLE_RESUMED.load(Ordering::Relaxed),
        IDLE_SLEPT.load(Ordering::Relaxed),
        READ_AS_IDLE.load(Ordering::Relaxed),
    );
    if READ_AS_IDLE.load(Ordering::Acquire) {
        return Err("a processor running a task read as idle after a switch");
    }
    Ok(())
}

/// One task of [`a_running_processor_is_not_idle_across_switches`].
fn switch_and_look(_index: usize) {
    let Some(me) = super::current() else {
        finish();
        return;
    };
    let mut turn = 0u64;
    while IDLE_RESUMED.load(Ordering::Relaxed) < IDLE_SWITCHES {
        turn = turn.wrapping_add(1);
        let before = me.switches();
        let sleeps = turn.is_multiple_of(IDLE_SLEEP_EVERY);
        if sleeps {
            super::sleep_for(IDLE_LOOK_SLEEP_NANOS);
        } else {
            super::yield_now();
        }
        if me.switches() == before {
            continue;
        }
        let _ = IDLE_RESUMED.fetch_add(1, Ordering::Relaxed);
        if sleeps {
            let _ = IDLE_SLEPT.fetch_add(1, Ordering::Relaxed);
        }
        if super::this_cpu_reads_as_idle() {
            READ_AS_IDLE.store(true, Ordering::Release);
        }
    }
    drop(me);
    finish();
}

/// How long the checker waits for a waiter in [`a_drained_waiter_survives_its_last_look`]
/// before it calls the waiter lost.
const LAST_LOOK_PATIENCE_NANOS: u64 = 2_000_000_000;

/// How long the interrupt case spins inside the last look, with interrupts
/// on, after arming this processor's timer [`LAST_LOOK_ARM_NANOS`] out.
const LAST_LOOK_SPIN_NANOS: u64 = 20_000_000;

/// How far out the interrupt case arms this processor's timer.
const LAST_LOOK_ARM_NANOS: u64 = 1_000_000;

/// The queue the last look's waiter waits on, and nothing else does.
static LAST_LOOK_QUEUE: WaitQueue = WaitQueue::new();

/// The waiter's condition.
static LAST_LOOK_READY: AtomicBool = AtomicBool::new(false);

/// How many times the waiter's `ready` has been called.
static LAST_LOOK_LOOKS: AtomicU64 = AtomicU64::new(0);

/// Whether the waiter, at the end of its last look, was still on its run
/// queue, not switched out, and still blocked: what the hold promises.
static LAST_LOOK_HELD: AtomicBool = AtomicBool::new(false);

/// Whether a timer interrupt was taken during the interrupt case's spin.
static LAST_LOOK_TICKED: AtomicBool = AtomicBool::new(false);

/// Whether the decision asked for inside the look was made once it was over.
static LAST_LOOK_DECIDED: AtomicBool = AtomicBool::new(false);

/// The case's switch: a lock let go inside the look, which is the decision
/// `preempt_enable` defers to.
const LAST_LOOK_BY_LOCK: usize = 0;

/// The case's switch: a timer interrupt's exit inside the look.
const LAST_LOOK_BY_INTERRUPT: usize = 1;

/// A wait's last look is never cut short by a switch (F-69).
///
/// The race needs two things in turn. First, a wake drains the waiter's entry
/// while the waiter is still runnable, and that drain does nothing. Then a
/// switch comes between the waiter's `BLOCKED` and the end of its last look.
/// `choose_next` takes a task that is not runnable off its queue, and nothing
/// is left to wake it. On a trusting wait there is no deadline either, so the
/// task is lost with its condition met.
///
/// The waiter here plays both steps inside its own last look, so the window
/// is hit every time:
/// 1. it sets itself runnable, wakes its queue (draining itself, to no
///    effect) and sets itself blocked again;
/// 2. it makes the condition true;
/// 3. it asks this processor for a decision, and makes the switch possible:
///    - in the first case, by letting go of a lock;
///    - in the second, by spinning with interrupts on through a timer
///      interrupt it armed.
///
/// With the hold, nothing switches. The task stays queued and is not switched
/// out, and the decision is made once the look is over. Without the hold, the
/// task is lost. The checker then wakes it by hand so the boot goes on, and
/// fails with the case's own sentence.
///
/// Verifies: L.sched.71
fn a_drained_waiter_survives_its_last_look() -> Result<(), &'static str> {
    last_look_case(
        LAST_LOOK_BY_LOCK,
        "a wait's last look was cut short at a lock's release: a waiter whose entry a wake drained \
         while it ran was left blocked on no run queue",
    )?;
    last_look_case(
        LAST_LOOK_BY_INTERRUPT,
        "a wait's last look was cut short at an interrupt's exit: a waiter whose entry a wake \
         drained while it ran was left blocked on no run queue",
    )
}

/// One case of [`a_drained_waiter_survives_its_last_look`], `how` saying which
/// switch the waiter invites, and `lost` the sentence for a waiter lost.
fn last_look_case(how: usize, lost: &'static str) -> Result<(), &'static str> {
    let allocations = crate::vmap::usage().allocations;
    let here = super::current()
        .ok_or("the checking task is not running")?
        .cpu();
    DONE.store(0, Ordering::Release);
    LAST_LOOK_READY.store(false, Ordering::Release);
    LAST_LOOK_LOOKS.store(0, Ordering::Release);
    LAST_LOOK_HELD.store(false, Ordering::Release);
    LAST_LOOK_TICKED.store(false, Ordering::Release);
    LAST_LOOK_DECIDED.store(false, Ordering::Release);

    // On this processor, so that the switch the waiter invites is this
    // processor's, and a lost waiter has no other processor to be found on.
    let waiter = super::spawn_on(
        "check-last-look",
        drained_in_its_last_look,
        how,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    let deadline = crate::timer::now_nanos().saturating_add(LAST_LOOK_PATIENCE_NANOS);
    let finished = FINISHED.wait_until_deadline(|| DONE.load(Ordering::Acquire) >= 1, deadline);
    if !finished {
        // Blocked on no queue: only a direct wake brings it back, and then
        // its wait returns, since its condition was met all along.
        let blocked_off_its_queue = waiter.state() == BLOCKED && !waiter.is_queued();
        super::wake(&waiter);
        wait_for(
            || DONE.load(Ordering::Acquire) >= 1,
            "a waiter lost in its last look did not finish once woken by hand",
        )?;
        reap_to(allocations, "a waiter lost in its last look")?;
        drop(waiter);
        return Err(if blocked_off_its_queue {
            lost
        } else {
            "a waiter never finished its wait, and was not found blocked off its run queue"
        });
    }

    reap_to(allocations, "a waiter's last look")?;
    drop(waiter);
    if LAST_LOOK_LOOKS.load(Ordering::Acquire) < 2 {
        return Err("the last look's waiter never reached its last look");
    }
    if how == LAST_LOOK_BY_INTERRUPT && !LAST_LOOK_TICKED.load(Ordering::Acquire) {
        return Err("no timer interrupt came while the last look's waiter spun in its look");
    }
    if !LAST_LOOK_HELD.load(Ordering::Acquire) {
        return Err("a waiter was taken off its run queue or switched out inside its last look");
    }
    if !LAST_LOOK_DECIDED.load(Ordering::Acquire) {
        return Err("the decision asked for inside a wait's last look was never made");
    }
    Ok(())
}

/// The waiter of [`last_look_case`]: a trusting wait whose last look plays
/// the race, then a look at whether the decision it asked for was made.
fn drained_in_its_last_look(how: usize) {
    let returned = LAST_LOOK_QUEUE.wait_trusting(|| {
        let look = LAST_LOOK_LOOKS.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        if look == 2
            && let Some(me) = super::current()
        {
            last_look_race(&me, how);
        }
        LAST_LOOK_READY.load(Ordering::Acquire)
    });
    // Made by now: the hold's drop, with the request pending and interrupts
    // on, decides it, and nothing else asks this processor for one meanwhile.
    let decided = super::current().is_some_and(|me| returned && !super::resched_asked(me.cpu()));
    LAST_LOOK_DECIDED.store(decided, Ordering::Release);
    finish();
}

/// Inside `me`'s last look, `BLOCKED` and listed: a wake drained while it was
/// runnable, its condition met, and the switch `how` names invited.
fn last_look_race(me: &Arc<Task>, how: usize) {
    let switches = me.switches();
    me.set_state(super::task::RUNNABLE);
    LAST_LOOK_QUEUE.wake_all();
    me.set_state(BLOCKED);
    LAST_LOOK_READY.store(true, Ordering::Release);
    let cpu = me.cpu();
    if how == LAST_LOOK_BY_LOCK {
        super::mark_resched(cpu);
        super::preempt_disable();
        super::preempt_enable();
    } else {
        let ticks = crate::timer::ticks();
        let saved = <crate::arch::Irq as IrqControl>::disable();
        super::mark_resched(cpu);
        crate::timer::after(LAST_LOOK_ARM_NANOS);
        <crate::arch::Irq as IrqControl>::restore(saved);
        let until = crate::timer::now_nanos().saturating_add(LAST_LOOK_SPIN_NANOS);
        while crate::timer::now_nanos() < until {
            core::hint::spin_loop();
        }
        LAST_LOOK_TICKED.store(crate::timer::ticks() != ticks, Ordering::Release);
    }
    let held = me.is_queued() && me.switches() == switches && me.state() == BLOCKED;
    LAST_LOOK_HELD.store(held, Ordering::Release);
}
