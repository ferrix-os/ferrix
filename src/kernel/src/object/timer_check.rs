//! Stage 9's timer checks (`docs/microkernel/K10.md` §5): a timer fires no
//! earlier than its deadline and within its window, once a set, never after
//! a set, cancel or close superseded it, coalesces with what is due beside
//! it, is fired at most `FIRE_BATCH` a pass with none lost, and is charged
//! to its job; and a sleeping task woken early leaves its sleeper set by its
//! key alone.
//!
//! Every timer call goes through `native::dispatch` with raw registers, as
//! `object::check` drives the other calls, from a process of its own.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    PACKET_SIGNAL, TIMER_CLOCK_BOOT, TIMER_CLOCK_MONOTONIC, TIMER_RELATIVE, TimerSpec,
};
use ferrix_sched::{CpuSet, NICE_0_WEIGHT, Timeline};

use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::Job;
use crate::object::quota::Resource;
use crate::object::timer::{self, Clock, Timer, TimerCore};
use crate::sched::{self, Due, Fired, WaitQueue};
use crate::sync::SpinLock;

/// The hook that takes a timer's arm off its sleeper set, which exists only
/// in a kernel built with its self-checks (the K10 delta review, ledger
/// 2919, (1)).
#[cfg(ferrix_checks = "on")]
use crate::object::timer::sabotage::take_arm;

/// What stands for it in a kernel built `checks=off`, where no check runs:
/// nothing here is ever called, and the product has no hook to call.
#[cfg(not(ferrix_checks = "on"))]
fn take_arm(_: &TimerCore) -> Option<(timer::Armed, sched::SleepSlot)> {
    None
}

/// A `TimerSpec`, in the check process's scratch region past what
/// `object::check` uses.
const SPEC_AT: u64 = SCRATCH + 0x800;
/// A wait's deadline.
const DEADLINE_AT: u64 = SCRATCH + 0x810;
/// A `PortPacket`.
const PACKET_AT: u64 = SCRATCH + 0x820;
/// A registration's key.
const KEY_AT: u64 = SCRATCH + 0x840;
/// The signals a wait observed.
const OBSERVED_AT: u64 = SCRATCH + 0x848;

/// A millisecond, in nanoseconds.
const MS: u64 = 1_000_000;

/// What the checks counted, for the boot's line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Timers that fired.
    pub(crate) fired: u64,
    /// Interrupt passes the batch case took.
    pub(crate) passes: u64,
    /// Refusals as specified.
    pub(crate) refusals: u64,
}

/// Run every case.
///
/// # Errors
///
/// The first case that failed, as a sentence.
pub(crate) fn run() -> Result<Report, &'static str> {
    let boot = sched::current()
        .ok_or("the checking task is not running")?
        .cpu();
    // Another processor than this task's where there is one: this task's
    // wait below rechecks every few milliseconds, and its entries would sit
    // in the sleeper set between the coalescing pair's.
    let here = match crate::smp::count() {
        0 | 1 => boot,
        count => (boot + 1) % count,
    };
    *OUTCOME.lock() = None;
    // On one processor for good: a timer is filed where it is set, and the
    // pair and early-wake cases are about one processor's sleeper set. The
    // task that runs the boot's checks may move between processors.
    let task = sched::spawn_on(
        "check-timers",
        run_pinned,
        0,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    // One wait with a far deadline, not a poll: a short sleep of this task
    // on the checks' processor would sit in the sleeper set between the
    // coalescing pair's entries.
    let patience = crate::timer::now_nanos().saturating_add(60_000 * MS);
    let _ = FINISHED.wait_until_deadline(|| OUTCOME.lock().is_some(), patience);
    let outcome = OUTCOME
        .lock()
        .take()
        .ok_or("the timer checks never finished")?;
    while !task.is_dead() {
        sched::yield_now();
    }
    outcome
}

/// Woken when the pinned run has ended.
static FINISHED: WaitQueue = WaitQueue::new();

/// What the pinned run of the cases ended with.
static OUTCOME: SpinLock<Option<Result<Report, &'static str>>> = SpinLock::new(None);

/// Run every case on the processor this task is pinned to, and say how it
/// ended.
fn run_pinned(_argument: usize) {
    let outcome = run_here();
    *OUTCOME.lock() = Some(outcome);
    FINISHED.wake_all();
}

/// Every case, in order.
fn run_here() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let side = Side::new()?;
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    check_a_timer_fires_at_its_deadline(&side, port, &mut report)?;
    check_a_deadline_past_four_seconds_arrives_whole(&side)?;
    check_a_cancelled_timer_stays_quiet(&side, port)?;
    check_a_superseded_arm_in_flight_signals_nothing(&side, port, &mut report)?;
    check_timers_in_one_window_fire_together(&side, port, &mut report)?;
    check_a_past_deadline_fires_in_the_call(&side, &mut report)?;
    check_a_full_batch_loses_nothing(&side, port, &mut report)?;
    check_a_closed_timer_never_fires(&side, port)?;
    check_timers_are_charged_to_their_job()?;
    check_what_timer_calls_refuse(&side, port, &mut report)?;
    check_an_early_wake_takes_out_only_its_own_entry()?;
    side.close_everything();
    Ok(report)
}

/// A timer made through `timer_create` on `clock`.
fn create(side: &Side, clock: u64) -> Result<Handle, &'static str> {
    side.handle(nr::TIMER_CREATE, &[clock, 0], "timer_create failed")
}

/// The timer a handle names, as the kernel holds it.
fn core_of(side: &Side, handle: Handle) -> Result<Arc<TimerCore>, &'static str> {
    side.process
        .with_handles(|table| match table.get(handle) {
            Ok((Object::Timer(timer), _)) => Some(Arc::clone(timer.core())),
            _ => None,
        })
        .ok_or("a timer handle named no timer")
}

/// Set `timer` through `timer_set`.
fn set(
    side: &Side,
    timer: Handle,
    deadline: u64,
    slack: u64,
    options: u64,
) -> Result<usize, ferrix_linux_abi::errno::Errno> {
    side.put(SPEC_AT, &TimerSpec { deadline, slack }.to_bytes())
        .map_err(|_| ferrix_linux_abi::errno::Errno::EFAULT)?;
    side.call(nr::TIMER_SET, &[reg(timer), SPEC_AT, options])
}

/// Set `timer` through `timer_set`, which must succeed.
fn arm(
    side: &Side,
    timer: Handle,
    deadline: u64,
    slack: u64,
    options: u64,
) -> Result<(), &'static str> {
    set(side, timer, deadline, slack, options)
        .map(|_| ())
        .map_err(|_| "timer_set failed")
}

/// Register `timer` on `port` for `READABLE`, with `key`.
fn watch(side: &Side, timer: Handle, port: Handle, key: u64) -> Result<(), &'static str> {
    side.put(KEY_AT, &key.to_ne_bytes())?;
    side.call(
        nr::OBJECT_WAIT_ASYNC,
        &[
            reg(timer),
            reg(port),
            u64::from(Signals::READABLE.0),
            KEY_AT,
        ],
    )
    .map(|_| ())
    .map_err(|_| "a timer could not be watched through a port")
}

/// The next packet on `port` within `nanos`, as its key and kind, or `None`
/// if none came.
fn packet_within(
    side: &Side,
    port: Handle,
    nanos: u64,
) -> Result<Option<(u64, u32)>, &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(nanos);
    side.put(DEADLINE_AT, &deadline.to_ne_bytes())?;
    match side.call(nr::PORT_WAIT, &[reg(port), DEADLINE_AT, PACKET_AT]) {
        Ok(_) => {
            let bytes = side.get(PACKET_AT, 16)?;
            let key = bytes
                .get(0..8)
                .and_then(|word| <[u8; 8]>::try_from(word).ok())
                .map(u64::from_ne_bytes)
                .ok_or("a short packet")?;
            let kind = bytes
                .get(8..12)
                .and_then(|word| <[u8; 4]>::try_from(word).ok())
                .map(u32::from_ne_bytes)
                .ok_or("a short packet")?;
            Ok(Some((key, kind)))
        }
        Err(status::TIMED_OUT) => Ok(None),
        Err(_) => Err("port_wait failed"),
    }
}

/// Whether `timer` reads `READABLE` now, through `object_wait_one`.
fn readable(side: &Side, timer: Handle) -> Result<bool, &'static str> {
    side.put(DEADLINE_AT, &0_u64.to_ne_bytes())?;
    let _ = side.call(
        nr::OBJECT_WAIT_ONE,
        &[
            reg(timer),
            u64::from(Signals::READABLE.0),
            DEADLINE_AT,
            OBSERVED_AT,
        ],
    );
    let observed = side.get_u32(OBSERVED_AT)?;
    Ok(Signals(observed).intersects(Signals::READABLE))
}

/// Close a handle the checks made.
fn close(side: &Side, handle: Handle) -> Result<(), &'static str> {
    side.call(nr::HANDLE_CLOSE, &[reg(handle)])
        .map(|_| ())
        .map_err(|_| "closing a timer check's handle failed")
}

/// A timer set 2 ms from now is heard on its port with its key, not before
/// its deadline, and reads `READABLE`.
///
/// Verifies: L.object.214, L.object.216, H.OBJ.21
fn check_a_timer_fires_at_its_deadline(
    side: &Side,
    port: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let timer = create(side, TIMER_CLOCK_MONOTONIC)?;
    let core = core_of(side, timer)?;
    watch(side, timer, port, 0xA1)?;
    let before = crate::timer::now_nanos();
    arm(side, timer, 2 * MS, 0, TIMER_RELATIVE)?;
    let heard = packet_within(side, port, 500 * MS)?;
    if heard != Some((0xA1, PACKET_SIGNAL)) {
        return Err("a timer set 2 ms ahead was not heard on its port");
    }
    let fired = core.report();
    if fired.fires != 1 || fired.fired_at < before.saturating_add(2 * MS) {
        return Err("a timer fired before its deadline");
    }
    if !readable(side, timer)? {
        return Err("a timer that fired did not read READABLE");
    }
    report.fired += 1;
    close(side, timer)
}

/// An absolute deadline past 2^32 ns reaches the timer whole, on every
/// architecture: ARMv7-A's 32-bit registers never carry it.
///
/// Verifies: L.object.214
fn check_a_deadline_past_four_seconds_arrives_whole(side: &Side) -> Result<(), &'static str> {
    let timer = create(side, TIMER_CLOCK_BOOT)?;
    let core = core_of(side, timer)?;
    let deadline = crate::timer::now_nanos().saturating_add((1 << 32) + 12_345);
    arm(side, timer, deadline, (1 << 33) + 7, 0)?;
    if core.report().armed != Some((deadline, deadline + (1 << 33) + 7)) {
        return Err("a deadline past 2^32 ns did not reach the timer whole");
    }
    close(side, timer)
}

/// A timer cancelled before its deadline is never heard and reads quiet.
///
/// Verifies: L.object.215, H.OBJ.21
fn check_a_cancelled_timer_stays_quiet(side: &Side, port: Handle) -> Result<(), &'static str> {
    let timer = create(side, TIMER_CLOCK_MONOTONIC)?;
    let core = core_of(side, timer)?;
    watch(side, timer, port, 0xB1)?;
    // Far enough ahead that the cancel comes first, under an emulator too.
    arm(side, timer, 100 * MS, 0, TIMER_RELATIVE)?;
    let _ = side
        .call(nr::TIMER_CANCEL, &[reg(timer)])
        .map_err(|_| "timer_cancel failed")?;
    if packet_within(side, port, 150 * MS)?.is_some() || core.report().fires != 0 {
        return Err("a cancelled timer fired");
    }
    if readable(side, timer)? || !core.report().home {
        return Err("a cancelled timer kept its arm");
    }
    close(side, timer)
}

/// An arm a pass took off the sleeper set, then superseded by a set before
/// it fired, signals nothing when it fires; the newer arm is filed by that
/// fire and fires once, at its own deadline.
///
/// Verifies: L.object.219, L.object.218, H.OBJ.21
fn check_a_superseded_arm_in_flight_signals_nothing(
    side: &Side,
    port: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let timer = create(side, TIMER_CLOCK_MONOTONIC)?;
    let core = core_of(side, timer)?;
    watch(side, timer, port, 0xC1)?;
    arm(side, timer, 50 * MS, 0, TIMER_RELATIVE)?;
    let (old, slot) = take_arm(&core).ok_or("a set timer was not on its sleeper set")?;
    let before = crate::timer::now_nanos();
    arm(side, timer, 3 * MS, 0, TIMER_RELATIVE)?;
    if core.report().home {
        return Err("a set took a slot an arm in flight held");
    }
    timer::fire(old, slot, 0);
    if readable(side, timer)? || core.report().fires != 0 {
        return Err("a superseded arm signalled when it fired");
    }
    let heard = packet_within(side, port, 500 * MS)?;
    let fired = core.report();
    if heard != Some((0xC1, PACKET_SIGNAL))
        || fired.fires != 1
        || fired.fired_at < before.saturating_add(3 * MS)
    {
        return Err("the arm that superseded one in flight did not fire once at its deadline");
    }
    if packet_within(side, port, 5 * MS)?.is_some() {
        return Err("a timer fired twice for one set");
    }
    report.fired += 1;
    close(side, timer)
}

/// Of two timers, the second keyed 5 ms after the first with its window
/// open at the first's deadline, a pass at that deadline takes both, and
/// they fire in one pass.
///
/// Driven, not waited for: both arms are set far ahead, taken off their
/// sleeper set, and filed in a sleeper set of the check's own that holds
/// nothing else, and the pass the timer interrupt and every decision run
/// (`sched::take_next`) is run over it at the first's deadline exactly. So
/// nothing another task files, no interrupt's timing and no load decides
/// the case: it depends on the rule alone. (Waited for, it failed on 3 of 4
/// AArch64 boots and under load on x86-64: coalescing is a prefix, and
/// another task's entry between the two keys was enough.)
///
/// Verifies: L.object.217
fn check_timers_in_one_window_fire_together(
    side: &Side,
    port: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let first = create(side, TIMER_CLOCK_MONOTONIC)?;
    let second = create(side, TIMER_CLOCK_MONOTONIC)?;
    let (a, b) = (core_of(side, first)?, core_of(side, second)?);
    watch(side, first, port, 0xD1)?;
    watch(side, second, port, 0xD2)?;
    let at = crate::timer::now_nanos().saturating_add(60_000 * MS);
    arm(side, first, at, 0, 0)?;
    arm(side, second, at - MS, 6 * MS, 0)?;
    let mut own: Timeline<Due> = Timeline::new();
    for core in [&a, &b] {
        let (latest, id) = (
            core.report().armed.map_or(0, |(_, latest)| latest),
            core.timeline_id(),
        );
        let (armed, slot) = take_arm(core).ok_or("a set timer was not on its sleeper set")?;
        // NOALLOC: a `Timeline` files the arm in the slot it is given.
        own.insert(latest, id, Due::Fire(armed), slot);
    }
    let mut fired = Fired::new();
    let mut refiled = 0;
    let woke = sched::take_next(&mut own, &mut fired, at, &mut refiled, true);
    let taken = fired.len;
    sched::preempt_disable();
    for (armed, slot) in fired.take().into_iter().flatten() {
        timer::fire(armed, slot, u64::MAX);
    }
    sched::preempt_enable();
    if woke.is_some() || taken != 2 || !own.is_empty() {
        return Err("a timer whose window was open did not fire with the one due beside it");
    }
    for _ in 0..2 {
        if packet_within(side, port, 500 * MS)?.is_none() {
            return Err("a timer in a coalescing pair was not heard on its port");
        }
    }
    let (a, b) = (a.report(), b.report());
    if a.fires != 1 || b.fires != 1 || a.fired_in != u64::MAX || b.fired_in != u64::MAX {
        return Err("a timer whose window was open did not fire with the one due beside it");
    }
    report.fired += 2;
    close(side, first)?;
    close(side, second)
}

/// A deadline already past fires in the call itself.
///
/// Verifies: L.object.214
fn check_a_past_deadline_fires_in_the_call(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let timer = create(side, TIMER_CLOCK_MONOTONIC)?;
    let core = core_of(side, timer)?;
    arm(side, timer, 1, 0, 0)?;
    let fired = core.report();
    if !readable(side, timer)? || fired.fires != 1 || fired.fired_in != 0 {
        return Err("a deadline already past did not fire in timer_set");
    }
    report.fired += 1;
    close(side, timer)
}

/// More timers due at once than one pass fires: no pass fires more than
/// `FIRE_BATCH`, and every one fires.
///
/// Verifies: L.object.216, L.object.218
fn check_a_full_batch_loses_nothing(
    side: &Side,
    port: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    const COUNT: usize = sched::FIRE_BATCH + 4;
    let mut timers = Vec::new();
    let at = crate::timer::now_nanos().saturating_add(200 * MS);
    for key in 0..COUNT as u64 {
        let timer = create(side, TIMER_CLOCK_MONOTONIC)?;
        watch(side, timer, port, 0xE00 + key)?;
        arm(side, timer, at, 0, 0)?;
        timers.push((timer, core_of(side, timer)?));
    }
    for _ in 0..COUNT {
        if packet_within(side, port, 500 * MS)?.is_none() {
            return Err("a timer due in a full batch never fired");
        }
    }
    let mut passes: Vec<(u64, usize)> = Vec::new();
    for (_, core) in &timers {
        let pass = core.report().fired_in;
        match passes.iter_mut().find(|(seen, _)| *seen == pass) {
            Some((_, count)) => *count += 1,
            None => passes.push((pass, 1)),
        }
    }
    if passes
        .iter()
        .any(|&(pass, count)| pass == 0 || count > sched::FIRE_BATCH)
    {
        return Err("one interrupt pass fired more timers than a batch");
    }
    report.passes += passes.len() as u64;
    report.fired += COUNT as u64;
    for (timer, _) in timers {
        close(side, timer)?;
    }
    Ok(())
}

/// The last handle of an armed, watched timer closes: it never fires, and
/// it is freed with its slot.
///
/// Verifies: L.object.220, H.OBJ.21
fn check_a_closed_timer_never_fires(side: &Side, port: Handle) -> Result<(), &'static str> {
    let timer = create(side, TIMER_CLOCK_MONOTONIC)?;
    let gone = Arc::downgrade(&core_of(side, timer)?);
    watch(side, timer, port, 0xF1)?;
    arm(side, timer, 100 * MS, 0, TIMER_RELATIVE)?;
    close(side, timer)?;
    if packet_within(side, port, 150 * MS)?.is_some() {
        return Err("a timer fired after its last handle closed");
    }
    // Its filed arm held the timer: gone means the close took it back.
    if gone.upgrade().is_some() {
        return Err("a closed timer was kept, or kept its arm filed");
    }
    Ok(())
}

/// A timer is charged to its job as one object and its heap as kernel
/// memory, both taken back when it goes; past the object limit none is
/// made.
///
/// Verifies: L.object.213, H.OBJ.21
fn check_timers_are_charged_to_their_job() -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "no memory for a job")?;
    let job = tree.new_child().map_err(|_| "a job refused a child")?;
    let used = |resource| job.usage(resource).map_or(0, |usage| usage.used);
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let made = Timer::new(Clock::Monotonic);
    let objects = used(Resource::Objects);
    let kernel = used(Resource::Kernel);
    let _ = job.set_limit(Resource::Objects, objects);
    let refused = Timer::new(Clock::Monotonic).is_err();
    sched::set_current_group(own);
    let made = made.map_err(|_| "a timer could not be made in a job with room")?;
    if objects != 1 || kernel != timer::heap_bytes() as u64 || !refused {
        return Err("a timer was not charged to its job, or was made past its object limit");
    }
    drop(made);
    if used(Resource::Objects) != 0 || used(Resource::Kernel) != 0 {
        return Err("a timer gone was still charged to its job");
    }
    Ok(())
}

/// What the timer calls refuse.
///
/// Verifies: L.object.213, L.object.214
fn check_what_timer_calls_refuse(
    side: &Side,
    port: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let timer = create(side, TIMER_CLOCK_MONOTONIC)?;
    let blind = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(timer), u64::from((Rights::WAIT | Rights::DUPLICATE).0)],
        "duplicating a timer without WRITE failed",
    )?;
    let cases: [(Result<usize, ferrix_linux_abi::errno::Errno>, _, &str); 6] = [
        (
            side.call(nr::TIMER_CREATE, &[2, 0]),
            status::INVALID_ARGS,
            "a timer on an unknown clock was made",
        ),
        (
            side.call(nr::TIMER_CREATE, &[TIMER_CLOCK_MONOTONIC, 1]),
            status::INVALID_ARGS,
            "timer_create took an option",
        ),
        (
            set(side, timer, MS, 0, 2),
            status::INVALID_ARGS,
            "timer_set took an unknown option",
        ),
        (
            set(side, port, MS, 0, TIMER_RELATIVE),
            status::WRONG_TYPE,
            "a port was set as a timer",
        ),
        (
            set(side, blind, MS, 0, TIMER_RELATIVE),
            status::ACCESS_DENIED,
            "a timer was set without WRITE",
        ),
        (
            side.call(nr::TIMER_CANCEL, &[reg(blind)]),
            status::ACCESS_DENIED,
            "a timer was cancelled without WRITE",
        ),
    ];
    for (answer, wanted, what) in cases {
        if answer != Err(wanted) {
            return Err(what);
        }
        report.refusals += 1;
    }
    close(side, blind)?;
    close(side, timer)
}

/// Set by the sleeper of [`check_an_early_wake_takes_out_only_its_own_entry`]
/// when its sleep is over.
static WOKEN: AtomicBool = AtomicBool::new(false);

/// Sleep a second, and say so when it is over.
fn sleep_a_second(_argument: usize) {
    sched::sleep_for(1_000 * MS);
    WOKEN.store(true, Ordering::Release);
}

/// A task asleep among armed timers on its processor, woken early, takes out
/// exactly its own entry: the sleeper set holds one fewer, and every timer's
/// arm is still filed.
///
/// Verifies: L.sched.84
fn check_an_early_wake_takes_out_only_its_own_entry() -> Result<(), &'static str> {
    let here = sched::current()
        .ok_or("the checking task is not running")?
        .cpu();
    let timers: Vec<Arc<Timer>> = (0..3)
        .map(|_| Timer::new(Clock::Monotonic))
        .collect::<Result<_, _>>()
        .map_err(|_| "no memory for the early-wake check's timers")?;
    for timer in &timers {
        timer.core().set(10_000 * MS, 0, true);
    }
    WOKEN.store(false, Ordering::Release);
    let task = sched::spawn_on(
        "check-early-wake",
        sleep_a_second,
        0,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    let patience = crate::timer::now_nanos().saturating_add(500 * MS);
    while task.filed_at() == 0 {
        if crate::timer::now_nanos() >= patience {
            return Err("a sleeping task was never filed beside the timers");
        }
        sched::yield_now();
    }
    // Read just around the wake, whose removal is synchronous: one entry,
    // the task's, leaves.
    let (before, _) = sched::sleepers_here();
    sched::wake(&task);
    let (after, _) = sched::sleepers_here();
    let patience = crate::timer::now_nanos().saturating_add(500 * MS);
    while !WOKEN.load(Ordering::Acquire) {
        if crate::timer::now_nanos() >= patience {
            return Err("a sleeping task woken early never woke");
        }
        sched::yield_now();
    }
    if before.checked_sub(after) != Some(1)
        || task.filed_at() != 0
        || timers.iter().any(|timer| timer.core().report().home)
    {
        return Err("an early wake took out more or less than its own entry");
    }
    drop(timers);
    Ok(())
}
