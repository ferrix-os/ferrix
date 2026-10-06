//! Step 4's fast path for `channel_write_read` (`docs/OPAQUE-KERNEL.md` §9.7,
//! part 6): the cases `ipc-equiv` cannot make from ring 3, made here with the
//! threads of check processes driving the `SYSCALL` entry
//! (`arch::drive_native_words`), as `write_read_check` drives it.
//!
//! Every case states the result the general path gives, and holds both boots
//! to it: with `ferrix.fastpath=off` every call takes the general path, and
//! with it on the case is built so that the fast path's test for it is
//! reached, which the decline counters show. The gate boots x86-64 both ways,
//! so a case that holds on both shows the two paths give the same result.
//!
//! - **Case 4 (T7)**: two threads of one process wait on one end, one parked
//!   and one listed, while a third sends from it; each reader is answered
//!   once, the two messages between them, within the bound.
//! - **Case 9 (T11)**: an echo pinned to another processor answers every
//!   trip, and runs on no processor its affinity does not allow.
//! - **Case 10 (T12)**: a spinner of equal weight pinned beside the trips
//!   keeps its share.
//! - **Case 11 (T2)**: the boot check's seccomp probe armed on the caller
//!   refuses the call with its errno, and nothing reaches the echo; a probe
//!   that allows it lets it through.
//! - **Case 14 (T13)**: an end posted in T13's window (the hook of condition
//!   11, which only this check arms) is seen: the call answers `EINTR` and
//!   leaves nobody blocked.
//! - **The general continuation (condition 3)**: a caller the fast path
//!   parked, woken by a general write, by its peer's close and by its
//!   process's kill, answers what the general path answers.
//!
//! x86-64 only, where the fast path is; elsewhere the module says so and
//! checks that no counter moved.

use alloc::sync::Arc;

use ferrix_linux_abi::errno::{self, Errno};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_seccomp::SeccompData;

use super::Object;
use super::channel::Endpoint;
use super::job::KILLED_STATUS;
use super::process::Host as _;
use crate::arch;
use crate::sched::Task;
use crate::sched::direct::{self, Count};
use crate::sync::SpinLock;
use crate::syscall::check::spawn_in;
use crate::syscall::process::{self, Process};
use crate::trap::Verdict;

/// How long a wake may take: the bound a blocked caller is held to.
const WOKEN_WITHIN_NANOS: u64 = 10_000_000_000;
/// How long the check waits for its threads to get going.
const PATIENCE_NANOS: u64 = 60_000_000_000;
/// How often it looks.
const POLL_NANOS: u64 = 1_000_000;
/// Trips a case makes.
const TRIPS: u64 = 200;

/// What a call answered: the return register, then the second to fourth.
type Answer = (isize, [u64; 3]);

/// What the check found, for its boot line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Whether the fast path is on for this boot.
    pub(crate) on: bool,
    /// Cases run.
    pub(crate) cases: u32,
    /// Whether the cases needing two processors ran.
    pub(crate) two_processors: bool,
    /// Trips the fast path took during the check.
    pub(crate) trips: u64,
}

/// Run every case and print the boot line.
///
/// # Errors
///
/// As [`run`].
pub(crate) fn run_and_report() -> Result<(), &'static str> {
    let fast = run()?;
    if arch::FAST_WRITE_READ {
        crate::console::println!(
            "  fastcase {} cases of the fast path's tests answered as the general path answers \
             them{}, every waiter within {} s; the fast path {}: {} trips taken",
            fast.cases,
            if fast.two_processors {
                ", one across two processors"
            } else {
                ""
            },
            WOKEN_WITHIN_NANOS / 1_000_000_000,
            if fast.on { "on" } else { "off" },
            fast.trips
        );
    } else {
        crate::console::println!(
            "  fastcase not checked: the fast path is x86-64's; no fast path counter moved"
        );
    }
    Ok(())
}

/// Run every case.
///
/// # Errors
///
/// The first case whose result the general path does not give, or a fast
/// path test a case was built to reach and did not.
///
/// Verifies: `H.SCHED.13`, `H.OBJ.18`, `L.sched.57`, `L.sched.58`
pub(crate) fn run() -> Result<Report, &'static str> {
    let on = crate::trap::fast_write_read().is_some();
    let before = direct::counts();
    let mut report = Report {
        on,
        ..Report::default()
    };
    if arch::FAST_WRITE_READ {
        check_two_readers_of_one_end(on)?;
        check_a_filtered_call(on)?;
        check_an_end_in_the_last_looks_window(on)?;
        for ending in [Ending::Message, Ending::Close, Ending::Kill] {
            check_a_parked_caller_woken_by(ending, on)?;
        }
        check_a_spinner_keeps_its_share(on)?;
        check_the_barriers_in_a_domain_and_across_two(on)?;
        report.cases = 9;
        if crate::smp::count() >= 2 {
            check_an_echo_on_another_processor(on)?;
            report.cases += 1;
            report.two_processors = true;
        }
    }
    let after = direct::counts();
    let trips = |counts: &[u64]| counts.get(Count::Trip as usize).copied().unwrap_or(0);
    report.trips = trips(&after).wrapping_sub(trips(&before));
    if !on && after != before {
        return Err("a fast path counter moved on a boot without the fast path");
    }
    Ok(report)
}

/// How a case's counter moved, for one that must reach a test with the
/// fast path on.
fn moved(before: &[u64], what: Count) -> u64 {
    let at = |counts: &[u64]| counts.get(what as usize).copied().unwrap_or(0);
    at(&direct::counts()).wrapping_sub(at(before))
}

/// Make the call on `handle` through the entry, sending `count` bytes of
/// `words`, from a thread of the process whose table holds it.
fn call(handle: Handle, count: usize, words: [u64; 3]) -> Answer {
    arch::drive_native_words(
        nr::CHANNEL_WRITE_READ,
        [
            u64::from(handle.0),
            count as u64,
            words[0],
            words[1],
            words[2],
            0,
        ],
    )
}

/// A receive-only call's answer that is a message, as a count and words.
const fn message(answer: Answer) -> Option<(usize, [u64; 3])> {
    if answer.0 < 0 {
        return None;
    }
    Some((answer.0 as usize, answer.1))
}

/// The status a call that failed with `status` answers.
fn refused(status: Errno) -> isize {
    errno::encode(Err(status))
}

/// A check process holding `end` as a handle, and the handle.
fn holding(end: &Arc<Endpoint>) -> Result<(Arc<Process>, Handle), &'static str> {
    holding_in(end, None)
}

/// [`holding`], the process moved into `job` before it runs when one is
/// given, as `process_create` places one.
fn holding_in(
    end: &Arc<Endpoint>,
    job: Option<&Arc<super::job::Job>>,
) -> Result<(Arc<Process>, Handle), &'static str> {
    let process = process::new_for_check().map_err(|_| "no process for the fast path check")?;
    if let Some(job) = job {
        process
            .core()
            .move_new_to(job)
            .map_err(|_| "a job refused the fast path check's process")?;
    }
    let handle = process
        .with_handles(|table| table.insert(Object::Channel(Arc::clone(end)), Rights::CHANNEL))
        .map_err(|_| "no room in the fast path check's table")?;
    Ok((process, handle))
}

/// Wait, sleeping a millisecond at a time, until `done` or `deadline`.
fn wait_until(
    deadline: u64,
    stuck: &'static str,
    done: impl Fn() -> bool,
) -> Result<(), &'static str> {
    while !done() {
        if crate::timer::now_nanos() >= deadline {
            return Err(stuck);
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
    Ok(())
}

/// A processor the trips run on: not this one where there is another, so
/// that the checker's own polling sleeps are not the queue's.
fn trip_processor() -> usize {
    let here = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    let count = crate::smp::count();
    if count >= 2 { (here + 1) % count } else { here }
}

// ---------------------------------------------------------------------------
// The echo
// ---------------------------------------------------------------------------

/// The echo's end, handed to its thread.
static ECHO: SpinLock<Option<Handle>> = SpinLock::new(None);
/// Messages the echoes have read.
static ECHOED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// An echo: a receive-only call, then each message sent back, until the
/// call fails, then the end of its process.
fn echo_in_the_process(_argument: usize) {
    let taken = ECHO.lock().take();
    if let Some(handle) = taken {
        let mut answer = call(handle, nr::WRITE_READ_NOTHING, [0; 3]);
        while let Some((count, words)) = message(answer) {
            let _ = ECHOED.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            answer = call(handle, count, words);
        }
    }
    process::exit_current(0)
}

/// Start an echo on `end`, pinned to `cpu`, and wait until it waits.
fn start_echo(end: &Arc<Endpoint>, cpu: usize) -> Result<(Arc<Process>, Arc<Task>), &'static str> {
    start_echo_in(end, cpu, None)
}

/// [`start_echo`], in `job` when one is given.
fn start_echo_in(
    end: &Arc<Endpoint>,
    cpu: usize,
    job: Option<&Arc<super::job::Job>>,
) -> Result<(Arc<Process>, Arc<Task>), &'static str> {
    let (process, handle) = holding_in(end, job)?;
    *ECHO.lock() = Some(handle);
    let task = spawn_in(&process, "fast path echo", echo_in_the_process, Some(cpu))?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    wait_until(deadline, "the fast path check's echo never waited", || {
        end.reader_waiting()
    })?;
    Ok((process, task))
}

/// Wait until `task` has ended, by `deadline`.
fn wait_dead(task: &Task, deadline: u64, what: &'static str) -> Result<(), &'static str> {
    wait_until(deadline, what, || task.is_dead())
}

// ---------------------------------------------------------------------------
// A caller of trips
// ---------------------------------------------------------------------------

/// The caller's end and how many trips to make.
static CALLER: SpinLock<Option<(Handle, u64)>> = SpinLock::new(None);
/// What the caller found: `Ok` with the trips it made, or what went wrong.
static CALLED: SpinLock<Option<Result<u64, &'static str>>> = SpinLock::new(None);

/// Trips carrying sequence numbers, each answered with its own.
fn trips_in_the_process(_argument: usize) {
    let taken = CALLER.lock().take();
    let found = taken.map_or(Err("no caller's end"), |(handle, trips)| {
        for trip in 1..=trips {
            let sent = [trip, !trip, trip.rotate_left(17)];
            if call(handle, 24, sent) != (24, sent) {
                return Err("a trip was not answered with its own words");
            }
        }
        Ok(trips)
    });
    *CALLED.lock() = Some(found);
    process::exit_current(0)
}

/// Make `trips` trips from a caller pinned to `cpu` against an echo pinned to
/// `echo_cpu`, and answer the echo's task once both have ended.
fn trips_between(cpu: usize, echo_cpu: usize, trips: u64) -> Result<Arc<Task>, &'static str> {
    trips_between_in(cpu, echo_cpu, trips, [None, None])
}

/// [`trips_between`], the echo and the caller in the jobs `jobs` names, in
/// that order, where it names them.
fn trips_between_in(
    cpu: usize,
    echo_cpu: usize,
    trips: u64,
    jobs: [Option<&Arc<super::job::Job>>; 2],
) -> Result<Arc<Task>, &'static str> {
    let (mine, theirs) =
        Endpoint::pair().map_err(|_| "no memory for the fast path check's channel")?;
    let (_echo_process, echo) = start_echo_in(&theirs, echo_cpu, jobs[0])?;
    drop(theirs);
    let (caller_process, handle) = holding_in(&mine, jobs[1])?;
    drop(mine);
    *CALLED.lock() = None;
    *CALLER.lock() = Some((handle, trips));
    let caller = spawn_in(
        &caller_process,
        "fast path caller",
        trips_in_the_process,
        Some(cpu),
    )?;
    drop(caller_process);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    wait_dead(
        &caller,
        deadline,
        "the fast path check's caller never finished its trips",
    )?;
    wait_dead(
        &echo,
        deadline,
        "the fast path check's echo never saw its caller's end",
    )?;
    match CALLED.lock().take() {
        Some(Ok(made)) if made == trips => Ok(echo),
        Some(Err(why)) => Err(why),
        _ => Err("the fast path check's caller made fewer trips than asked"),
    }
}

/// Case 9 (T11): an echo pinned to another processor answers every trip, and
/// runs on no processor but its own: a direct switch to it would have run it
/// on the caller's.
/// Verifies: `L.sched.56`
fn check_an_echo_on_another_processor(on: bool) -> Result<(), &'static str> {
    let before = direct::counts();
    let here = trip_processor();
    let there = (here + 1) % crate::smp::count();
    let echo = trips_between(here, there, TRIPS)?;
    if echo.cpus_run_on() != 1 << there {
        return Err("case 9: an echo pinned to its processor ran on another");
    }
    if on && moved(&before, Count::T11) == 0 {
        return Err("case 9: trips to an echo on another processor never reached T11");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 10: a spinner beside the trips
// ---------------------------------------------------------------------------

/// Set to let the spinner go.
static SPIN_UNTIL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Spin until [`SPIN_UNTIL`].
fn spin(_argument: usize) {
    while crate::timer::now_nanos() < SPIN_UNTIL.load(core::sync::atomic::Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// Case 10 (T12): a spinner of equal weight pinned to the trips' processor
/// keeps at least a quarter of it while the trips run -- a third is its
/// share against the caller and the echo when both are runnable, and they
/// block for every trip -- and the trips still finish.
/// Verifies: `L.sched.56`
fn check_a_spinner_keeps_its_share(on: bool) -> Result<(), &'static str> {
    let before = direct::counts();
    let cpu = trip_processor();
    let start = crate::timer::now_nanos();
    SPIN_UNTIL.store(
        start.saturating_add(400_000_000),
        core::sync::atomic::Ordering::Release,
    );
    let spinner = crate::sched::spawn_on(
        "fast path spinner",
        spin,
        0,
        ferrix_sched::NICE_0_WEIGHT,
        cpu,
        ferrix_sched::CpuSet::of(cpu),
    )?;
    let ran_before = spinner.runtime();
    let _ = trips_between(cpu, cpu, TRIPS * 10)?;
    let window = crate::timer::now_nanos()
        .saturating_sub(start)
        .min(400_000_000);
    let ran = spinner.runtime().saturating_sub(ran_before);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    wait_dead(&spinner, deadline, "case 10: the spinner never stopped")?;
    if window > 50_000_000 && ran < window / 4 {
        crate::console::println!("  fastpath case 10: the spinner ran {ran} ns of {window}");
        return Err("case 10: a spinner beside the trips fell below its share");
    }
    if on && moved(&before, Count::T12) == 0 {
        return Err("case 10: trips beside a spinner never reached T12");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 4: two readers of one end
// ---------------------------------------------------------------------------

/// The readers' end.
static READERS: SpinLock<Option<Handle>> = SpinLock::new(None);
/// What each reader was answered, by the order they finished.
static READ: SpinLock<[Option<Answer>; 2]> = SpinLock::new([None, None]);
/// The sender's end and what it sends.
static SENDER: SpinLock<Option<Handle>> = SpinLock::new(None);
/// What the sender was answered.
static SENT: SpinLock<Option<Answer>> = SpinLock::new(None);

/// A reader: one receive-only call, recorded.
fn read_in_the_process(_argument: usize) {
    let handle = *READERS.lock();
    if let Some(handle) = handle {
        let answer = call(handle, nr::WRITE_READ_NOTHING, [0; 3]);
        let mut read = READ.lock();
        if let Some(slot) = read.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(answer);
        }
    }
    process::exit_thread_current(0)
}

/// The third thread: one call that sends `"x"` from the readers' end.
fn send_in_the_process(_argument: usize) {
    let taken = SENDER.lock().take();
    if let Some(handle) = taken {
        *SENT.lock() = Some(call(handle, 1, [u64::from(b'x'), 0, 0]));
    }
    process::exit_thread_current(0)
}

/// Case 4 (T7): two threads of one process wait on one end, the first parked
/// by the receive half and the second, finding it parked, listed on the
/// queue; a third thread of the process sends `"x"` from that end to an echo
/// and waits there too. The echo's `"x"` and a later `"y"` answer two of the
/// three, each once, and the third is answered by the close. Every waiter is
/// answered within the bound.
/// Verifies: `L.object.164`, `L.object.165`, `L.object.166`
fn check_two_readers_of_one_end(on: bool) -> Result<(), &'static str> {
    let before = direct::counts();
    let cpu = trip_processor();
    let (mine, theirs) =
        Endpoint::pair().map_err(|_| "no memory for the fast path check's channel")?;
    let (process, handle) = holding(&mine)?;
    *READ.lock() = [None, None];
    *READERS.lock() = Some(handle);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let first = spawn_in(&process, "fast path reader", read_in_the_process, Some(cpu))?;
    wait_until(deadline, "case 4: the first reader never waited", || {
        mine.reader_waiting()
    })?;
    let second = spawn_in(&process, "fast path reader", read_in_the_process, Some(cpu))?;
    wait_until(deadline, "case 4: the second reader never waited", || {
        mine.waiters().listed() != 0
    })?;
    // The echo, on the other end, and the sender beside the two readers.
    let (echo_process, echo) = start_echo(&theirs, cpu)?;
    *SENT.lock() = None;
    *SENDER.lock() = Some(handle);
    let sender = spawn_in(&process, "fast path sender", send_in_the_process, Some(cpu))?;
    drop(process);
    let bound = crate::timer::now_nanos().saturating_add(WOKEN_WITHIN_NANOS);
    // Two of the three are answered by the echo's "x" and this "y".
    wait_until(
        bound,
        "case 4: a reader was never woken by the echo's answer",
        || READ.lock().iter().flatten().next().is_some() || SENT.lock().is_some(),
    )?;
    theirs
        .write_small(b"y")
        .map_err(|_| "case 4: the check could not write its message")?;
    let second_answered = wait_until(bound, "case 4: a second waiter was never answered", || {
        let answered = READ.lock().iter().flatten().count() + usize::from(SENT.lock().is_some());
        answered >= 2
    });
    if second_answered.is_err() {
        crate::console::println!(
            "  fastpath case 4: read {:?}, sent {:?}, waiting {} listed {}, echoed {}, tasks {} {} {}",
            *READ.lock(),
            *SENT.lock(),
            mine.reader_waiting(),
            mine.waiters().listed(),
            ECHOED.load(core::sync::atomic::Ordering::Acquire),
            first.state(),
            second.state(),
            sender.state()
        );
    }
    second_answered?;
    // The third by its peer's close: the echo ended, and its end let go.
    process::kill(&echo_process, KILLED_STATUS);
    drop(echo_process);
    drop(theirs);
    drop(mine);
    wait_dead(&echo, bound, "case 4: the echo never ended")?;
    for task in [&first, &second, &sender] {
        wait_dead(
            task,
            bound,
            "case 4: a waiter of one end was never woken within the bound",
        )?;
    }
    let mut answers: alloc::vec::Vec<Answer> = READ.lock().iter().flatten().copied().collect();
    answers.extend(SENT.lock().iter().copied());
    let x = (1, [u64::from(b'x'), 0, 0]);
    let y = (1, [u64::from(b'y'), 0, 0]);
    let closed = (
        refused(status::PEER_CLOSED),
        [nr::WRITE_READ_NOTHING as u64, 0, 0],
    );
    let count = |answer: Answer| answers.iter().filter(|seen| **seen == answer).count();
    let sender_closed = SENT
        .lock()
        .is_some_and(|answer| answer.0 == refused(status::PEER_CLOSED));
    let readers_closed = count(closed);
    if answers.len() != 3
        || count(x) != 1
        || count(y) != 1
        || readers_closed + usize::from(sender_closed) != 1
    {
        return Err(
            "case 4: the three waiters of one end were not answered x, y and the close once each",
        );
    }
    if on && moved(&before, Count::Park) == 0 {
        return Err("case 4: the first reader never parked");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 11: a filtered call
// ---------------------------------------------------------------------------

/// The filtered caller's end, and which rule it arms.
static FILTERED: SpinLock<Option<(Handle, bool)>> = SpinLock::new(None);
/// What its call answered.
static FILTERED_ANSWER: SpinLock<Option<Answer>> = SpinLock::new(None);

/// The probe's rule that refuses `channel_write_read` with `EPERM`.
fn refuse_write_read(data: &SeccompData) -> Verdict {
    if data.nr as usize == nr::CHANNEL_WRITE_READ {
        Verdict::Errno(Errno::EPERM.0 as u32)
    } else {
        Verdict::Continue
    }
}

/// The probe's rule that lets it through.
fn allow_write_read(_data: &SeccompData) -> Verdict {
    Verdict::Continue
}

/// One call under the probe, armed on this thread for it alone.
fn filtered_in_the_process(_argument: usize) {
    let taken = FILTERED.lock().take();
    if let Some((handle, refuse)) = taken {
        crate::syscall::seccomp::arm_probe(if refuse {
            refuse_write_read
        } else {
            allow_write_read
        });
        let answer = call(handle, 1, [u64::from(b'f'), 0, 0]);
        crate::syscall::seccomp::disarm_probe();
        *FILTERED_ANSWER.lock() = Some(answer);
    }
    process::exit_current(0)
}

/// Case 11 (T2): with the probe armed to refuse it, the call answers the
/// probe's `EPERM` and the echo parked on the other end is not answered; with
/// it armed to allow it, the call is answered by the echo.
/// Verifies: `L.object.169`
fn check_a_filtered_call(on: bool) -> Result<(), &'static str> {
    let before = direct::counts();
    let cpu = trip_processor();
    for refuse in [true, false] {
        let (mine, theirs) =
            Endpoint::pair().map_err(|_| "no memory for the fast path check's channel")?;
        let (_echo_process, echo) = start_echo(&theirs, cpu)?;
        let echoed = ECHOED.load(core::sync::atomic::Ordering::Acquire);
        let (process, handle) = holding(&mine)?;
        drop(mine);
        *FILTERED_ANSWER.lock() = None;
        *FILTERED.lock() = Some((handle, refuse));
        let caller = spawn_in(
            &process,
            "fast path filtered",
            filtered_in_the_process,
            Some(cpu),
        )?;
        drop(process);
        let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
        wait_dead(
            &caller,
            deadline,
            "case 11: the filtered caller never finished",
        )?;
        let answer = FILTERED_ANSWER.lock().take();
        if refuse {
            if answer.map(|answer| answer.0) != Some(Errno::EPERM.as_return_value()) {
                return Err("case 11: a call the filter refuses was not refused with its errno");
            }
            if ECHOED.load(core::sync::atomic::Ordering::Acquire) != echoed {
                return Err("case 11: a call the filter refuses reached the echo");
            }
        } else if answer != Some((1, [u64::from(b'f'), 0, 0])) {
            return Err("case 11: a call the filter allows was not answered by the echo");
        }
        drop(theirs);
        wait_dead(&echo, deadline, "case 11: the echo never ended")?;
    }
    if on && moved(&before, Count::T2) < 2 {
        return Err("case 11: a filtered call never reached T2");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 14: an end in T13's window
// ---------------------------------------------------------------------------

/// The windowed caller's end.
static WINDOWED: SpinLock<Option<Handle>> = SpinLock::new(None);
/// Set when the checker has armed the hook on the caller.
static GO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// What the windowed call answered.
static WINDOWED_ANSWER: SpinLock<Option<Answer>> = SpinLock::new(None);

/// One call, once the hook is armed on it.
fn windowed_in_the_process(_argument: usize) {
    while !GO.load(core::sync::atomic::Ordering::Acquire) {
        crate::sched::sleep_for(POLL_NANOS);
    }
    let taken = WINDOWED.lock().take();
    if let Some(handle) = taken {
        *WINDOWED_ANSWER.lock() = Some(call(handle, 1, [u64::from(b'w'), 0, 0]));
    }
    process::exit_current(0)
}

/// Case 14 (T13, condition 11): an end posted on the caller between its
/// entry and its last look -- by the fast path's hook, which this case alone
/// arms -- is seen there: the call answers `EINTR` within the bound, as a
/// call whose process ends while it waits does, and nothing is left blocked.
/// With the fast path off the hook is never reached, and the call is
/// answered.
/// Verifies: `L.sched.56`, `L.sched.62`
fn check_an_end_in_the_last_looks_window(on: bool) -> Result<(), &'static str> {
    let before = direct::counts();
    let cpu = trip_processor();
    let (mine, theirs) =
        Endpoint::pair().map_err(|_| "no memory for the fast path check's channel")?;
    let (_echo_process, echo) = start_echo(&theirs, cpu)?;
    let (process, handle) = holding(&mine)?;
    drop(mine);
    GO.store(false, core::sync::atomic::Ordering::Release);
    *WINDOWED_ANSWER.lock() = None;
    *WINDOWED.lock() = Some(handle);
    let caller = spawn_in(
        &process,
        "fast path windowed",
        windowed_in_the_process,
        Some(cpu),
    )?;
    crate::sched::work::arm(crate::sched::work::HOOK_LAST_LOOK, &caller);
    GO.store(true, core::sync::atomic::Ordering::Release);
    let bound = crate::timer::now_nanos().saturating_add(WOKEN_WITHIN_NANOS);
    let ended = wait_dead(
        &caller,
        bound,
        "case 14: a caller whose end was posted in T13's window stayed blocked past the bound",
    );
    crate::sched::work::disarm();
    ended?;
    drop(process);
    drop(theirs);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    wait_dead(&echo, deadline, "case 14: the echo never ended")?;
    let answer = WINDOWED_ANSWER.lock().take();
    if on {
        if answer.is_some_and(|answer| answer.0 != Errno::EINTR.as_return_value()) {
            return Err("case 14: a call with an end posted in T13's window did not answer EINTR");
        }
        if moved(&before, Count::T13) == 0 {
            return Err("case 14: an end posted in T13's window was not declined there");
        }
    } else if answer != Some((1, [u64::from(b'w'), 0, 0])) {
        return Err("case 14: with the fast path off, the call was not answered");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The general continuation
// ---------------------------------------------------------------------------

/// What wakes a parked caller in [`check_a_parked_caller_woken_by`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// A general write on the caller's end.
    Message,
    /// Its peer's close.
    Close,
    /// Its process's kill.
    Kill,
}

/// The sink's end: a reader that takes one message and does not answer.
static SINK: SpinLock<Option<Handle>> = SpinLock::new(None);
/// What the sink read.
static SUNK: SpinLock<Option<Answer>> = SpinLock::new(None);
/// The parked caller's end.
static PARKED: SpinLock<Option<Handle>> = SpinLock::new(None);
/// What the parked caller was answered.
static PARKED_ANSWER: SpinLock<Option<Answer>> = SpinLock::new(None);

/// A sink: one receive-only call, recorded, then a second that waits until
/// its process is killed, holding its end open until then without a sleep
/// that would put it on its processor's sleeper set.
fn sink_in_the_process(_argument: usize) {
    let taken = SINK.lock().take();
    if let Some(handle) = taken {
        *SUNK.lock() = Some(call(handle, nr::WRITE_READ_NOTHING, [0; 3]));
        let _ = call(handle, nr::WRITE_READ_NOTHING, [0; 3]);
    }
    process::exit_current(0)
}

/// A caller that sends `"s"` and waits for whatever answers, once the
/// checker has gone to sleep: a checker still runnable on the caller's
/// processor is a task waiting there, and T12 declines.
fn parked_in_the_process(_argument: usize) {
    loop {
        crate::sched::sleep_for(POLL_NANOS);
        if GO.load(core::sync::atomic::Ordering::Acquire) {
            break;
        }
    }
    let taken = PARKED.lock().take();
    if let Some(handle) = taken {
        *PARKED_ANSWER.lock() = Some(call(handle, 1, [u64::from(b's'), 0, 0]));
    }
    process::exit_current(0)
}

/// The general continuation (condition 3): a caller whose send was handed
/// to a parked sink, and who is parked in turn, is woken by `ending` -- a
/// message written on its end by a third party, its peer's close, its
/// process's kill -- and answers what the general path answers: the message,
/// `PEER_CLOSED`, or nothing at all, its thread ended on the way out.
/// Verifies: `L.object.165`, `L.object.168`, `L.sched.60`
fn check_a_parked_caller_woken_by(ending: Ending, on: bool) -> Result<(), &'static str> {
    // A send the fast path declined -- its processor's queue held for a
    // moment by a sleeper whose time had come -- is answered by the general
    // path as well, but leaves no caller parked by the fast path to
    // continue; so the case is made again, a few times at most, until one
    // was.
    for _ in 0..8 {
        let before = direct::counts();
        attempt_a_parked_caller(ending)?;
        if !on || moved(&before, Count::Trip) != 0 {
            return Ok(());
        }
    }
    Err("continuation: the caller's send was never handed over directly")
}

/// One try of [`check_a_parked_caller_woken_by`].
fn attempt_a_parked_caller(ending: Ending) -> Result<(), &'static str> {
    let cpu = trip_processor();
    let (mine, theirs) =
        Endpoint::pair().map_err(|_| "no memory for the fast path check's channel")?;
    let (sink_process, sink_handle) = holding(&theirs)?;
    *SUNK.lock() = None;
    *SINK.lock() = Some(sink_handle);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let sink = spawn_in(
        &sink_process,
        "fast path sink",
        sink_in_the_process,
        Some(cpu),
    )?;
    wait_until(deadline, "the continuation's sink never waited", || {
        theirs.reader_waiting()
    })?;
    let (caller_process, handle) = holding(&mine)?;
    *PARKED_ANSWER.lock() = None;
    *PARKED.lock() = Some(handle);
    GO.store(false, core::sync::atomic::Ordering::Release);
    let caller = spawn_in(
        &caller_process,
        "fast path parked",
        parked_in_the_process,
        Some(cpu),
    )?;
    GO.store(true, core::sync::atomic::Ordering::Release);
    // Polled slowly, so that this task's own sleep is seldom what is due on
    // a processor it shares with the two.
    while !(SUNK.lock().is_some() && mine.reader_waiting()) {
        if crate::timer::now_nanos() >= deadline {
            return Err("the continuation's caller never waited");
        }
        crate::sched::sleep_for(20 * POLL_NANOS);
    }
    if SUNK.lock().take() != Some((1, [u64::from(b's'), 0, 0])) {
        return Err("the continuation's sink did not read the caller's message");
    }
    let mut theirs = Some(theirs);
    match ending {
        Ending::Message => theirs
            .as_ref()
            .ok_or("the continuation's end is gone")?
            .write_small(b"m")
            .map_err(|_| "the continuation's check could not write its message")?,
        // The sink leaves, and with it the last holder of its end but this.
        Ending::Close => {
            process::kill(&sink_process, KILLED_STATUS);
            theirs = None;
        }
        Ending::Kill => process::kill(&caller_process, KILLED_STATUS),
    }
    let bound = crate::timer::now_nanos().saturating_add(WOKEN_WITHIN_NANOS);
    wait_dead(
        &caller,
        bound,
        match ending {
            Ending::Message => {
                "continuation: a parked caller was not woken by a message within the bound"
            }
            Ending::Close => {
                "continuation: a parked caller was not woken by its peer's close within the bound"
            }
            Ending::Kill => {
                "continuation: a parked caller was not woken by its kill within the bound"
            }
        },
    )?;
    process::kill(&sink_process, KILLED_STATUS);
    drop(caller_process);
    drop(theirs);
    drop(mine);
    wait_dead(
        &sink,
        crate::timer::now_nanos().saturating_add(PATIENCE_NANOS),
        "the continuation's sink never ended",
    )?;
    drop(sink_process);
    let answer = PARKED_ANSWER.lock().take();
    let fine = match ending {
        Ending::Message => answer == Some((1, [u64::from(b'm'), 0, 0])),
        Ending::Close => {
            answer
                == Some((
                    refused(status::PEER_CLOSED),
                    [nr::WRITE_READ_NOTHING as u64, 0, 0],
                ))
                || answer.is_some_and(|answer| answer.0 == refused(status::PEER_CLOSED))
        }
        Ending::Kill => answer.is_none_or(|answer| answer.0 == Errno::EINTR.as_return_value()),
    };
    if !fine {
        return Err(match ending {
            Ending::Message => "continuation: a parked caller woken by a message did not answer it",
            Ending::Close => {
                "continuation: a parked caller woken by its peer's close did not answer PEER_CLOSED"
            }
            Ending::Kill => "continuation: a parked caller woken by its kill did not answer EINTR",
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Case 13: the barrier decisions
// ---------------------------------------------------------------------------

/// Case 13 (part 4): the same trips, between two programs of one speculation
/// domain and between two of two domains, make the barrier decisions the
/// switch makes on either path, because both switch through
/// `AddressSpace::install`: inside the domain each switch refills the
/// return stack and issues no `IBPB`, across two each switch issues one.
/// The few switches of the programs' own start and end, to and from the
/// idle task, are allowed for.
fn check_the_barriers_in_a_domain_and_across_two(on: bool) -> Result<(), &'static str> {
    if !arch::HARDENED {
        return Ok(());
    }
    let before = direct::counts();
    let cpu = trip_processor();
    let tree = super::job::Job::new_root().map_err(|_| "no memory for case 13's jobs")?;
    let one = tree
        .new_child_domain()
        .map_err(|_| "case 13: a job refused a marked child")?;
    let other = tree
        .new_child_domain()
        .map_err(|_| "case 13: a job refused a second marked child")?;
    // Allowed for the start and the end of the two programs.
    const SLACK: u64 = 8;
    let decided = || arch::barrier_decisions_on(cpu);
    let refilled = || arch::refills_in_domain_on(cpu);
    let (decided_before, refilled_before) = (decided(), refilled());
    let _ = trips_between_in(cpu, cpu, TRIPS, [Some(&one), Some(&one)])?;
    let (decided_in, refilled_in) = (
        decided().wrapping_sub(decided_before),
        refilled().wrapping_sub(refilled_before),
    );
    if decided_in > SLACK || refilled_in < 2 * TRIPS {
        crate::console::println!(
            "  fastpath case 13: in a domain {decided_in} barriers decided, {refilled_in} refills"
        );
        return Err(
            "case 13: trips inside one speculation domain made barrier decisions a switch inside it does not",
        );
    }
    let decided_before = decided();
    let _ = trips_between_in(cpu, cpu, TRIPS, [Some(&one), Some(&other)])?;
    let decided_across = decided().wrapping_sub(decided_before);
    if decided_across < 2 * TRIPS {
        crate::console::println!(
            "  fastpath case 13: across two domains {decided_across} barriers decided"
        );
        return Err(
            "case 13: trips across two speculation domains skipped a barrier a switch between them makes",
        );
    }
    if on && moved(&before, Count::Trip) == 0 && crate::smp::count() >= 2 {
        return Err("case 13: no trip of the barrier case was handed over directly");
    }
    Ok(())
}
