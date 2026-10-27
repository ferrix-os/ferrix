//! The real-time tier's self-checks (`docs/SCHED-CLASSES.md` §9e, R12):
//! `SCHED_FIFO` and `SCHED_RR` as the kernel runs them, with kernel tasks.
//!
//! Every case runs its real-time tasks on one processor, `c`, pinned there,
//! and is driven from a task pinned to another, `d`, so that the driver is
//! never behind the tasks it judges. A machine of one processor runs none of
//! them. Run times are each task's own measure of the time it held the
//! processor (`spin_for`): a gap of more than [`GAP_NANOS`] between two
//! looks at the clock is time it did not run.
//!
//! * **Order** (H.SCHED.19, L.sched.109): behind a priority-99 holder, tasks
//!   of priorities 10, 20 and 20 run 20, 20, 10 once it leaves, the two 20s
//!   in arrival order, and a fair task spinning there runs only after all
//!   three. A task woken from another processor while a fair task spins
//!   alone there, tickless, runs within [`WAKE_NANOS`]; so does one woken by
//!   a fair task on its own processor, which then spins on: the wake asks
//!   that processor for a decision, taken at its timer armed a moment away
//!   (`resched_here`), or at once on the way out of a system call.
//! * **Placement** (L.sched.110): a real-time task woken from another
//!   processor while its own is busy and a third idle runs on its own; a
//!   fair one, the control, is placed on the idle one.
//! * **FIFO keeps its place**: a `SCHED_FIFO` task preempted by a higher
//!   priority runs again before the task of its own priority that arrived
//!   after it.
//! * **Round-robin**: two `SCHED_RR` tasks of one priority share the
//!   processor at the quantum; two `SCHED_FIFO` ones, the control, do not;
//!   a `SCHED_FIFO` task that yields lets its peer run.
//! * **The floor** (H.SCHED.20): a fair task beside a spinning `SCHED_FIFO`
//!   task is given between [`FLOOR_LEAST_NANOS`] and [`FLOOR_MOST_NANOS`] of
//!   [`FLOOR_WINDOW_NANOS`], and the `SCHED_FIFO` task the rest, unthrottled
//!   at each period's end.
//! * **The move** (L.sched.112, L.sched.107, L.sched.108): a fair task made
//!   `SCHED_FIFO` while it runs beside another fair task takes the processor
//!   from it, and given back to fair shares it again; it keeps its node
//!   across the return, and a weight set while it was real-time is its fair
//!   record's.
//! * **Flips**: an unpinned running task flipped between `SCHED_FIFO` and
//!   fair 200 times beside a waiting one, with processors idle to steal:
//!   the running task is moved only at its processor's next decision
//!   (line 30, B1; its control is the move made at the call, FX-0537).
//! * **The modes** (L.sched.115, L.sched.116): a `SoftRt` domain is refused
//!   by name where the tier is built, `ModeUnavailable` where it is not, and
//!   a real-time policy refused there too.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sched_class::{CpuSet, DomainError, Mode, RtParams, RtPolicy};
use ferrix_sched_fair::NICE_0_WEIGHT;

use super::task::Task;
use super::{PolicyRefused, WaitQueue};

/// A gap between two looks at the clock longer than this is time the task
/// did not hold the processor.
const GAP_NANOS: u64 = 200_000;

/// How soon a woken real-time task must run while a fair task spins on its
/// processor: without the wake's preemption it would wait for ever, the fair
/// task running tickless, so a bound well above an emulated processor's
/// stalls on a loaded host still tells the two apart (a loaded gate host
/// missed 20 ms, 2026-10-27).
const WAKE_NANOS: u64 = 100_000_000;

/// The round-robin case's window.
const RR_WINDOW_NANOS: u64 = 400_000_000;

/// The least each round-robin task must have run of it: a quarter, where an
/// even share is a half.
const RR_LEAST_NANOS: u64 = 100_000_000;

/// The most the second `SCHED_FIFO` task of the control may have run.
const FIFO_CONTROL_MOST_NANOS: u64 = 5_000_000;

/// The floor's window: two periods and a little.
const FLOOR_WINDOW_NANOS: u64 = 2_100_000_000;

/// The least a fair task must be given of it: 50 ms a period over two
/// periods, less a margin for where the window falls.
const FLOOR_LEAST_NANOS: u64 = 60_000_000;

/// The most: well short of the half a fair share would give. An emulated
/// processor's timer comes late enough to give it several times the floor
/// (ARMv7-A under `tcg`: 334 ms, 2026-10-27).
const FLOOR_MOST_NANOS: u64 = 700_000_000;

/// The least the spinning `SCHED_FIFO` task must be given of it: more than
/// one period's 950 ms, so it ran again after a throttle.
const FLOOR_FIFO_LEAST_NANOS: u64 = 1_200_000_000;

/// How long a case waits for something before it fails.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// The order the tasks of a case ran in: ids, packed a byte each.
static LOG: AtomicU64 = AtomicU64::new(0);
/// How many entries [`LOG`] holds.
static LOGGED: AtomicUsize = AtomicUsize::new(0);
/// What the fair spinner's progress counter read when each logged task ran,
/// or'ed: zero when it never moved while they ran.
static FAIR_MOVED: AtomicBool = AtomicBool::new(false);
/// The fair spinner's progress, and the value it had when the holder left.
static FAIR_PROGRESS: AtomicU64 = AtomicU64::new(0);
static FAIR_AT_RELEASE: AtomicU64 = AtomicU64::new(0);
/// Tells the holder to leave, and the spinners to stop.
static RELEASE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// Tasks of a case that have started.
static STARTED: AtomicUsize = AtomicUsize::new(0);
/// What each spinner measured it ran, by its argument's slot.
static RAN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// When a woken task was woken, and when it ran.
static WOKEN_AT: AtomicU64 = AtomicU64::new(0);
static RAN_AT: AtomicU64 = AtomicU64::new(0);
/// What the woken task waits for.
static WOKEN: AtomicBool = AtomicBool::new(false);
static QUEUE: WaitQueue = WaitQueue::new();
/// The driver's answer: 0 running, 1 passed, 2 failed, and why.
static VERDICT: AtomicUsize = AtomicUsize::new(0);
static WHY: spin_cell::Why = spin_cell::Why::new();

/// One `&'static str` handed from the driver task to the checker.
mod spin_cell {
    use ferrix_sync::SpinLock;

    /// The failing case's sentence.
    #[derive(Debug)]
    pub(super) struct Why(SpinLock<&'static str>);

    impl Why {
        /// Empty.
        pub(super) const fn new() -> Why {
            Why(SpinLock::new(""))
        }
        /// Keep `why`.
        pub(super) fn set(&self, why: &'static str) {
            *self.0.lock() = why;
        }
        /// What was kept.
        pub(super) fn get(&self) -> &'static str {
            *self.0.lock()
        }
    }
}

/// Run them, where the build has a tier above fair; otherwise the refusals
/// alone. What ran, for the boot log.
///
/// # Errors
///
/// The first case that failed, as a sentence.
pub(super) fn run() -> Result<&'static str, &'static str> {
    modes()?;
    if !super::realtime_built() {
        return Ok("no tier above fair: SoftRt unavailable and SCHED_FIFO refused, as built");
    }
    let online = super::QUEUES.get().map_or(0, Vec::len);
    if online < 2 {
        return Ok("one processor: the real-time cases need two");
    }
    let here = super::this_cpu().unwrap_or(0);
    let c = (here + 1) % online;
    let d = (c + 1) % online;
    VERDICT.store(0, Ordering::Release);
    let driver = super::spawn_on("rt-driver", drive, c, NICE_0_WEIGHT, d, CpuSet::of(d))?;
    let deadline = crate::timer::now_nanos().saturating_add(6 * PATIENCE_NANOS);
    while VERDICT.load(Ordering::Acquire) == 0 {
        if crate::timer::now_nanos() >= deadline {
            return Err("the real-time checks never finished");
        }
        super::sleep_for(10_000_000);
    }
    drop(driver);
    match VERDICT.load(Ordering::Acquire) {
        1 => Ok(
            "priority order, preemption on a remote and a local wake, FIFO's place, \
                 RR's quantum (and FIFO's control), a yield, the 50 ms floor, flips of a \
                 running task, the move between tiers",
        ),
        _ => Err(WHY.get()),
    }
}

/// The modes, and the policy refused where the tier is not built
/// (L.sched.115, L.sched.116).
///
/// Verifies: L.sched.115, L.sched.116
fn modes() -> Result<(), &'static str> {
    let made = super::make_domain(CpuSet::of(0), Mode::SoftRt);
    if super::realtime_built() {
        if made != Err(DomainError::NotCreatable(Mode::SoftRt)) {
            return Err("a SoftRt domain was not refused as compiled, not yet creatable");
        }
    } else {
        if made != Err(DomainError::ModeUnavailable(Mode::SoftRt)) {
            return Err("a SoftRt domain was not refused as unavailable without the tier");
        }
        let me = super::current().ok_or("the real-time check runs in no task")?;
        let fifo = RtParams {
            policy: RtPolicy::Fifo,
            priority: 10,
        };
        if super::set_policy(&me, Some(fifo), false) != Err(PolicyRefused::NoTier) {
            return Err("SCHED_FIFO was not refused in a build without the tier");
        }
        if !me.is_fair() {
            return Err("a refused SCHED_FIFO left its task real-time");
        }
    }
    Ok(())
}

/// The driver, pinned to `d`, running every case on `c`, its argument.
fn drive(c: usize) {
    let verdict = order(c)
        .and_then(|()| placement(c))
        .and_then(|()| fifo_keeps_its_place(c))
        .and_then(|()| round_robin(c))
        .and_then(|()| floor(c))
        .and_then(|()| flips(c))
        .and_then(|()| the_move(c));
    STOP.store(true, Ordering::Release);
    RELEASE.store(true, Ordering::Release);
    match verdict {
        Ok(()) => VERDICT.store(1, Ordering::Release),
        Err(why) => {
            WHY.set(why);
            VERDICT.store(2, Ordering::Release);
        }
    }
}

/// `policy` at `priority`.
const fn params(policy: RtPolicy, priority: u8) -> RtParams {
    RtParams { policy, priority }
}

/// Wait until `ready`, sleeping between looks, or fail with `what`.
fn wait_until(mut ready: impl FnMut() -> bool, what: &'static str) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !ready() {
        if crate::timer::now_nanos() >= deadline {
            return Err(what);
        }
        super::sleep_for(1_000_000);
    }
    Ok(())
}

/// Clear the log and the flags for a case.
fn reset() {
    LOG.store(0, Ordering::Release);
    LOGGED.store(0, Ordering::Release);
    FAIR_MOVED.store(false, Ordering::Release);
    RELEASE.store(false, Ordering::Release);
    STOP.store(false, Ordering::Release);
    STARTED.store(0, Ordering::Release);
    for ran in &RAN {
        ran.store(0, Ordering::Release);
    }
}

/// Append `id` to the log, and note whether the fair spinner has moved
/// since the holder left.
fn log(id: u64) {
    let at = LOGGED.fetch_add(1, Ordering::AcqRel);
    if at < 8 {
        let _ = LOG.fetch_or(id << (8 * at), Ordering::AcqRel);
    }
    if FAIR_PROGRESS.load(Ordering::Acquire) != FAIR_AT_RELEASE.load(Ordering::Acquire) {
        FAIR_MOVED.store(true, Ordering::Release);
    }
}

/// The log, packed, once `n` entries are in it.
fn logged(n: usize) -> Result<u64, &'static str> {
    wait_until(
        || LOGGED.load(Ordering::Acquire) >= n,
        "a real-time task of a case never ran",
    )?;
    Ok(LOG.load(Ordering::Acquire))
}

/// A fair spinner: counts until told to stop.
fn fair_spinner(_: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    while !STOP.load(Ordering::Acquire) {
        let _ = FAIR_PROGRESS.fetch_add(1, Ordering::AcqRel);
        core::hint::spin_loop();
    }
}

/// The holder: spins until released, then notes the fair spinner's count.
fn holder(_: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    while !RELEASE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    FAIR_AT_RELEASE.store(FAIR_PROGRESS.load(Ordering::Acquire), Ordering::Release);
}

/// A task that logs its argument and ends.
fn logger(id: usize) {
    log(id as u64);
}

/// Spin, measuring the time held, into `RAN[slot]` until told to stop; with
/// `yielding`, giving the processor up at every look.
fn spin_for(slot: usize, yielding: bool) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    let mut last = crate::timer::now_nanos();
    let mut held = 0_u64;
    while !STOP.load(Ordering::Acquire) {
        let now = crate::timer::now_nanos();
        let gap = now.saturating_sub(last);
        if gap < GAP_NANOS {
            held = held.saturating_add(gap);
        }
        last = now;
        if let Some(ran) = RAN.get(slot) {
            ran.store(held, Ordering::Release);
        }
        if yielding {
            super::yield_now();
        }
    }
}

/// [`spin_for`] without yielding.
fn spinner(slot: usize) {
    spin_for(slot, false);
}

/// [`spin_for`], yielding.
fn yielder(slot: usize) {
    spin_for(slot, true);
}

/// Wait for `count` tasks of the case to have started.
fn started(count: usize) -> Result<(), &'static str> {
    wait_until(
        || STARTED.load(Ordering::Acquire) >= count,
        "a task of a real-time case never started",
    )
}

/// Stop the case's spinners and give them time to end.
fn stop() {
    STOP.store(true, Ordering::Release);
    RELEASE.store(true, Ordering::Release);
    super::sleep_for(20_000_000);
}

/// Priority order, and preemption on a remote and a local wake.
///
/// Verifies: H.SCHED.19, L.sched.109
fn order(c: usize) -> Result<(), &'static str> {
    reset();
    let _fair = super::spawn_on("rt-fair", fair_spinner, 0, NICE_0_WEIGHT, c, CpuSet::of(c))?;
    started(1)?;
    let _holder = super::spawn_rt_on(
        "rt-holder",
        holder,
        0,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 99),
    )?;
    started(2)?;
    let _ten = super::spawn_rt_on(
        "rt-10",
        logger,
        1,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 10),
    )?;
    let _twenty = super::spawn_rt_on(
        "rt-20",
        logger,
        2,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 20),
    )?;
    let _again = super::spawn_rt_on(
        "rt-20b",
        logger,
        3,
        c,
        CpuSet::of(c),
        params(RtPolicy::Rr, 20),
    )?;
    super::sleep_for(5_000_000);
    if LOGGED.load(Ordering::Acquire) != 0 {
        stop();
        return Err("a real-time task ran past a priority-99 task holding its processor");
    }
    RELEASE.store(true, Ordering::Release);
    let order = logged(3)?;
    if order != 2 | (3 << 8) | (1 << 16) {
        stop();
        crate::console::println!("  sched    real-time order {order:#x}, expected 0x10302");
        return Err(
            "real-time tasks did not run highest priority first, in arrival order within one",
        );
    }
    if FAIR_MOVED.load(Ordering::Acquire) {
        stop();
        return Err("a fair task ran while real-time tasks waited on its processor");
    }
    wakes(c)
}

/// [`order`]'s wakes, with its fair task still spinning on `c`: a remote
/// one and a local one.
///
/// Verifies: L.sched.109
fn wakes(c: usize) -> Result<(), &'static str> {
    // The remote wake: the fair task spins alone on `c`, tickless.
    WOKEN.store(false, Ordering::Release);
    RAN_AT.store(0, Ordering::Release);
    let _woken = super::spawn_rt_on(
        "rt-woken",
        woken,
        0,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 5),
    )?;
    started(3)?;
    super::sleep_for(20_000_000);
    WOKEN_AT.store(crate::timer::now_nanos(), Ordering::Release);
    WOKEN.store(true, Ordering::Release);
    QUEUE.wake_all();
    wait_until(
        || RAN_AT.load(Ordering::Acquire) != 0,
        "a real-time task woken from another processor never ran",
    )?;
    let late = RAN_AT
        .load(Ordering::Acquire)
        .saturating_sub(WOKEN_AT.load(Ordering::Acquire));
    if late > WAKE_NANOS {
        stop();
        return Err("a real-time task woken from another processor waited behind a fair one");
    }
    // The local wake: a fair task on `c` wakes it, and spins on.
    WOKEN.store(false, Ordering::Release);
    RAN_AT.store(0, Ordering::Release);
    let _again = super::spawn_rt_on(
        "rt-woken-here",
        woken,
        0,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 5),
    )?;
    started(4)?;
    super::sleep_for(20_000_000);
    let _waker = super::spawn_on("rt-waker", local_waker, 0, NICE_0_WEIGHT, c, CpuSet::of(c))?;
    wait_until(
        || RAN_AT.load(Ordering::Acquire) != 0,
        "a real-time task woken on its own processor never ran",
    )?;
    let late = RAN_AT
        .load(Ordering::Acquire)
        .saturating_sub(WOKEN_AT.load(Ordering::Acquire));
    stop();
    if late > WAKE_NANOS {
        return Err("a real-time task woken on its own processor waited behind its fair waker");
    }
    Ok(())
}

/// Wait to be woken, then note when it ran.
fn woken(_: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let _ = QUEUE.wait_until_deadline(|| WOKEN.load(Ordering::Acquire), deadline);
    RAN_AT.store(crate::timer::now_nanos(), Ordering::Release);
}

/// Wake the real-time task from its own processor, then spin on until it
/// has run.
fn local_waker(_: usize) {
    WOKEN_AT.store(crate::timer::now_nanos(), Ordering::Release);
    WOKEN.store(true, Ordering::Release);
    QUEUE.wake_all();
    while RAN_AT.load(Ordering::Acquire) == 0 && !STOP.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// The processor each woken task last ran on.
static RAN_ON: AtomicUsize = AtomicUsize::new(usize::MAX);

/// A real-time task woken from another processor while its own is busy
/// stays there, though another is idle; a fair one, the control, does not.
/// Only with three processors or more, so that one is idle.
///
/// Verifies: L.sched.110
fn placement(c: usize) -> Result<(), &'static str> {
    let online = super::QUEUES.get().map_or(0, Vec::len);
    if online < 3 {
        return Ok(());
    }
    let all = CpuSet::first(online).map_err(|_| "no set of every processor")?;
    reset();
    let _busy = super::spawn_on("rt-busy", fair_spinner, 0, NICE_0_WEIGHT, c, CpuSet::of(c))?;
    started(1)?;
    let fifo = params(RtPolicy::Fifo, 5);
    let rt_ran_on = woken_ran_on(2, || {
        super::spawn_rt_on("rt-placed", woken_where, 0, c, all, fifo)
    })?;
    let fair_ran_on = woken_ran_on(3, || {
        super::spawn_on("rt-placed-fair", woken_where, 0, NICE_0_WEIGHT, c, all)
    })?;
    stop();
    if rt_ran_on != c {
        return Err("a woken real-time task was moved off its busy processor");
    }
    if fair_ran_on == c {
        return Err("the control failed: a woken fair task stayed behind a busy processor");
    }
    Ok(())
}

/// Start the `n`th task of the case with `spawn`, the condition it waits
/// for cleared first, wait for it to start and wait, wake it from here, and
/// answer where it ran.
fn woken_ran_on(
    n: usize,
    spawn: impl FnOnce() -> Result<Arc<Task>, &'static str>,
) -> Result<usize, &'static str> {
    WOKEN.store(false, Ordering::Release);
    RAN_ON.store(usize::MAX, Ordering::Release);
    let _task = spawn()?;
    started(n)?;
    super::sleep_for(20_000_000);
    WOKEN.store(true, Ordering::Release);
    QUEUE.wake_all();
    wait_until(
        || RAN_ON.load(Ordering::Acquire) != usize::MAX,
        "a woken task of the placement case never ran",
    )?;
    Ok(RAN_ON.load(Ordering::Acquire))
}

/// Wait to be woken, then note where it ran.
fn woken_where(_: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let _ = QUEUE.wait_until_deadline(|| WOKEN.load(Ordering::Acquire), deadline);
    RAN_ON.store(
        super::this_cpu().unwrap_or(usize::MAX - 1),
        Ordering::Release,
    );
}

/// A preempted `SCHED_FIFO` task keeps the head of its priority.
fn fifo_keeps_its_place(c: usize) -> Result<(), &'static str> {
    reset();
    FAIR_AT_RELEASE.store(FAIR_PROGRESS.load(Ordering::Acquire), Ordering::Release);
    let _first = super::spawn_rt_on(
        "rt-first",
        first_in,
        1,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 10),
    )?;
    started(1)?;
    let _second = super::spawn_rt_on(
        "rt-second",
        logger,
        2,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 10),
    )?;
    super::sleep_for(5_000_000);
    let _higher = super::spawn_rt_on(
        "rt-higher",
        higher,
        3,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 20),
    )?;
    let order = logged(3)?;
    stop();
    if order != 3 | (1 << 8) | (2 << 16) {
        crate::console::println!("  sched    FIFO place order {order:#x}, expected 0x20103");
        return Err("a preempted SCHED_FIFO task lost the head of its priority");
    }
    Ok(())
}

/// The first of a priority: spins until the higher task has been, then logs.
fn first_in(id: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    while !RELEASE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    log(id as u64);
}

/// The higher task: logs, and lets the first go on.
fn higher(id: usize) {
    log(id as u64);
    RELEASE.store(true, Ordering::Release);
}

/// Two `SCHED_RR` tasks share their processor at the quantum; two
/// `SCHED_FIFO` ones, the control, do not; a yield lets the peer run.
fn round_robin(c: usize) -> Result<(), &'static str> {
    let both = |policy: RtPolicy, what: &'static str, task: fn(usize)| {
        reset();
        let _a = super::spawn_rt_on(what, task, 0, c, CpuSet::of(c), params(policy, 10))?;
        started(1)?;
        let _b = super::spawn_rt_on(what, task, 1, c, CpuSet::of(c), params(policy, 10))?;
        super::sleep_for(RR_WINDOW_NANOS);
        let ran = [
            RAN[0].load(Ordering::Acquire),
            RAN[1].load(Ordering::Acquire),
        ];
        stop();
        Ok::<_, &'static str>(ran)
    };
    let [a, b] = both(RtPolicy::Rr, "rt-rr", spinner)?;
    if a < RR_LEAST_NANOS || b < RR_LEAST_NANOS {
        crate::console::println!("  sched    RR ran {a} and {b} ns of {RR_WINDOW_NANOS}");
        return Err("two SCHED_RR tasks of one priority did not share their processor");
    }
    let [_, b] = both(RtPolicy::Fifo, "rt-fifo", spinner)?;
    if b > FIFO_CONTROL_MOST_NANOS {
        return Err("the control failed: the second SCHED_FIFO task ran beside the first");
    }
    let [a, b] = both(RtPolicy::Fifo, "rt-yield", yielder)?;
    if a == 0 || b == 0 {
        return Err("a SCHED_FIFO task that yields did not let its peer run");
    }
    Ok(())
}

/// The floor: a fair task beside a spinning `SCHED_FIFO` one.
///
/// Verifies: H.SCHED.20
fn floor(c: usize) -> Result<(), &'static str> {
    // A period of its own: whatever the cases before took of this one is
    // over.
    super::sleep_for(1_000_000_000);
    reset();
    let _fair = super::spawn_on("rt-floor-fair", spinner, 1, NICE_0_WEIGHT, c, CpuSet::of(c))?;
    started(1)?;
    let _fifo = super::spawn_rt_on(
        "rt-floor",
        spinner,
        0,
        c,
        CpuSet::of(c),
        params(RtPolicy::Fifo, 10),
    )?;
    super::sleep_for(FLOOR_WINDOW_NANOS);
    let (fifo, fair) = (
        RAN[0].load(Ordering::Acquire),
        RAN[1].load(Ordering::Acquire),
    );
    stop();
    crate::console::println!(
        "  sched    floor: fair {} ms, SCHED_FIFO {} ms of {} ms",
        fair / 1_000_000,
        fifo / 1_000_000,
        FLOOR_WINDOW_NANOS / 1_000_000
    );
    if !(FLOOR_LEAST_NANOS..=FLOOR_MOST_NANOS).contains(&fair) {
        return Err("a fair task beside a spinning SCHED_FIFO task was not given the floor");
    }
    if fifo < FLOOR_FIFO_LEAST_NANOS {
        return Err("a throttled SCHED_FIFO task did not run again at its period's end");
    }
    Ok(())
}

/// The move between tiers, the node kept, and a weight kept.
///
/// Verifies: L.sched.107, L.sched.108, L.sched.112
fn the_move(c: usize) -> Result<(), &'static str> {
    reset();
    let mover = super::spawn_on("rt-mover", spinner, 0, NICE_0_WEIGHT, c, CpuSet::of(c))?;
    let _other = super::spawn_on("rt-other", spinner, 1, NICE_0_WEIGHT, c, CpuSet::of(c))?;
    started(2)?;
    super::sleep_for(50_000_000);
    let fifo = params(RtPolicy::Fifo, 10);
    let outcome = moved(&mover, fifo);
    stop();
    outcome
}

/// The running task's move between tiers is made at its processor's next
/// decision, never while it runs (line 30, B1): an unpinned task spinning
/// on `c` beside a pinned one, so that `c` has a task waiting for an idle
/// processor to steal, is flipped between `SCHED_FIFO` and fair 200 times,
/// 10 ms after each flip to fair,
/// from here. Moved at the call instead, it sits in fair's tree as a waiting
/// task while it runs, an idle processor steals it, and `move_onto` stops
/// the machine (FX-0537): that change is this case's control. Only with
/// three processors or more, so that one is idle to steal.
///
/// Verifies: L.sched.112
fn flips(c: usize) -> Result<(), &'static str> {
    let online = super::QUEUES.get().map_or(0, Vec::len);
    if online < 3 {
        return Ok(());
    }
    let all = CpuSet::first(online).map_err(|_| "no set of every processor")?;
    reset();
    let _pinned = super::spawn_on(
        "rt-flip-pinned",
        spinner,
        1,
        NICE_0_WEIGHT,
        c,
        CpuSet::of(c),
    )?;
    let flipper = super::spawn_on("rt-flipper", spinner, 0, NICE_0_WEIGHT, c, all)?;
    started(2)?;
    let fifo = params(RtPolicy::Fifo, 10);
    let mut outcome = Ok(());
    for flip in 0..200 {
        let policy = (flip % 2 == 0).then_some(fifo);
        if super::set_policy(&flipper, policy, false).is_err() {
            outcome = Err("a flip between SCHED_FIFO and fair was refused");
            break;
        }
        // Long enough after a flip to fair for an idle processor to look
        // for work: the window the control opens.
        super::sleep_for(if policy.is_none() {
            10_000_000
        } else {
            1_000_000
        });
    }
    stop();
    outcome?;
    if !flipper.is_fair() {
        return Err("a task flipped back to fair kept a real-time policy");
    }
    Ok(())
}

/// [`the_move`]'s steps on `mover`, spinning beside another fair task.
fn moved(mover: &Arc<Task>, fifo: RtParams) -> Result<(), &'static str> {
    super::set_policy(mover, Some(fifo), false).map_err(|_| "SCHED_FIFO was refused")?;
    if !mover.owns_upper_node() || mover.rt_params() != Some(fifo) {
        return Err("a task made SCHED_FIFO did not keep its node and its policy");
    }
    // A weight set while it is real-time is its fair record's.
    let weight = 3121;
    super::set_weight(mover, weight);
    super::sleep_for(20_000_000);
    // Moved at its processor's decision: its run slot is parked while the
    // tier above fair holds it.
    if !mover.run_slot_parked() {
        return Err("a queued SCHED_FIFO task's run slot was not parked");
    }
    let other = RAN[1].load(Ordering::Acquire);
    super::sleep_for(100_000_000);
    if RAN[1].load(Ordering::Acquire).saturating_sub(other) > FIFO_CONTROL_MOST_NANOS {
        return Err("a fair task ran beside a task moved to SCHED_FIFO on its processor");
    }
    super::set_policy(mover, None, false).map_err(|_| "SCHED_OTHER was refused")?;
    if !mover.is_fair() || !mover.owns_upper_node() {
        return Err("a task given back to fair lost its node or kept its policy");
    }
    super::sleep_for(20_000_000);
    if mover.run_slot_parked() {
        return Err("a task given back to fair left its run slot parked");
    }
    let other = RAN[1].load(Ordering::Acquire);
    super::sleep_for(100_000_000);
    if RAN[1].load(Ordering::Acquire).saturating_sub(other) < 10_000_000 {
        return Err("a task given back to fair still held its processor from a fair task");
    }
    if mover.base_weight() != weight || mover.entity_state().weight != mover.effective_weight() {
        return Err("a weight set on a real-time task was not its fair record's on its return");
    }
    Ok(())
}
