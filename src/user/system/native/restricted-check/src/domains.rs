//! Case 193, 194: a real switch between two programs under `partition_speculation=partition`,
//! which stands in for `test-netd`'s second partition for this value's row (ledger 7216
//! T.1, T.3). Two marked jobs: every switch between them decides a barrier, issued where the
//! processor offers one. One marked job: none, the refill counted apart. Nothing goes into
//! restricted mode, so no barrier at an exit stands in. Only an image built `partition` runs
//! them, and only with two processors: this thread and the check's stay off processor 0,
//! and the zero is true because no other process has started yet.
//!
//! No shipped job is marked: under `partition` on a real boot every switch between spaces
//! takes the full barrier, stronger than the flag says (`docs/microkernel/K1.md`, FLIP.md).

use core::sync::atomic::Ordering::{Acquire, Release};
use core::sync::atomic::{AtomicU64, Ordering};

use super::{
    ANY_PROCESSOR, ASK_PIN, ASK_TWO_PROCESSORS, Given, Stack, ask, asked, hold, nr, sys, top,
};

/// Whether this image's rule is `partition`.
const ASK_DOMAIN_BUILT: u8 = 47;
/// The check keeps off processor 0, and reads its counts.
const ASK_DOMAIN_START: u8 = 48;
/// The counts against the word.
const ASK_DOMAIN_END: u8 = 49;
/// The word's top bit: inside one domain.
const WITHIN: u64 = 1 << 63;
/// Its next bit: the trips were to go direct.
const DIRECT: u64 = 1 << 62;
/// Whether the image has direct switches.
const ASK_DOMAIN_DIRECT: u8 = 50;
/// `JOB_SPECULATION_DOMAIN`.
const MARKED: u64 = ferrix_native_abi::types::JOB_SPECULATION_DOMAIN;
/// Round trips a case runs: two switches each.
const ROUNDS: u64 = 64;
/// Tenths of a millisecond a case waits for its programs, which then fails in a sentence.
const PATIENCE: u32 = 200_000;

/// The ends each body uses, and its steps.
static PING_END: AtomicU64 = AtomicU64::new(0);
/// The other end.
static PONG_END: AtomicU64 = AtomicU64::new(0);
/// The channel to the check, for the pins.
static CHECK: AtomicU64 = AtomicU64::new(0);
/// 0 nothing, bit 0 the answerer is waiting on processor 0, bit 1 the caller is ready.
static READY: AtomicU64 = AtomicU64::new(0);
/// 1 the caller may go, 2 it has finished.
static GO: AtomicU64 = AtomicU64::new(0);
/// [`GO`]'s failed value.
const FAILED: u64 = 100;
/// The caller's stack.
static PING_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// The answerer's stack.
static PONG_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));

/// Pin this thread to processor 0, on the check's channel.
fn pinned(case: i32) -> Result<(), i32> {
    let given = Given {
        channel: CHECK.load(Ordering::Relaxed),
        region: 0,
        job: 0,
        space: 0,
        region_space: 0,
        start_answers: ferrix_linux_abi::errno::Errno::ENOSYS,
    };
    let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| case)?;
    ask(&given, ASK_PIN, 0, Some(me)).map_err(|_| case)?;
    // Off the old processor: the way out moves it.
    sys(nr::THREAD_YIELD, [0; 6]).map_err(|_| case)?;
    Ok(())
}

/// `CHANNEL_WRITE_READ` of one word's worth, or none.
fn trip(end: u64, count: u64, word: u64) -> Result<u64, i32> {
    sys(nr::CHANNEL_WRITE_READ, [end, count, word, 0, 0, 0]).map_err(|_| 193)
}

/// The answerer: every message gets one back, until the other end goes.
extern "C" fn pong(_argument: usize) -> ! {
    let end = PONG_END.load(Acquire);
    if pinned(193).is_err() {
        GO.store(FAILED, Release);
    }
    READY.fetch_or(1, Release);
    let mut count = nr::WRITE_READ_NOTHING as u64;
    loop {
        let answer = trip(end, count, 0);
        if answer.is_err() {
            break;
        }
        count = 8;
    }
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// The caller: [`ROUNDS`] round trips, when told.
extern "C" fn ping(_argument: usize) -> ! {
    let end = PING_END.load(Acquire);
    if pinned(193).is_err() {
        GO.store(FAILED, Release);
    }
    READY.fetch_or(2, Release);
    while GO.load(Acquire) == 0 {
        core::hint::spin_loop();
    }
    let mut left = ROUNDS;
    while left > 0 {
        if trip(end, 8, left).is_err() {
            GO.store(FAILED, Release);
        }
        left -= 1;
    }
    if GO.load(Acquire) != FAILED {
        GO.store(2, Release);
    }
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// A process of `job` running `body` on `stack`.
fn started(
    given: &Given,
    job: u64,
    body: extern "C" fn(usize) -> !,
    stack: &Stack,
) -> Result<u64, i32> {
    let mut out = 0_u64;
    let name = b"domains";
    let process = sys(
        nr::PROCESS_CREATE_SHARED,
        [
            given.region,
            job,
            0,
            name.as_ptr().addr() as u64,
            name.len() as u64,
            (&raw mut out).addr() as u64,
        ],
    )
    .map_err(|_| 193)?;
    sys(
        nr::PROCESS_START_SHARED,
        [
            process,
            (body as *const ()).addr() as u64,
            top(stack),
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 193)?;
    Ok(process)
}

/// Wait, spinning off processor 0, while `done` says no.
fn await_that(done: impl Fn() -> bool, case: i32) -> Result<(), i32> {
    let mut left = PATIENCE;
    while !done() {
        hold(GO.load(Acquire) != FAILED, case)?;
        // Not on processor 0: this thread is pinned off it, so a spin takes no switch there.
        sys(nr::THREAD_YIELD, [0; 6]).map_err(|_| case)?;
        left = left.checked_sub(1).ok_or(case)?;
    }
    Ok(())
}

/// One pair, in `first` and `second`; `word` is what [`ASK_DOMAIN_END`] is told.
fn pair(given: &Given, first: u64, second: u64, word: u64, case: i32) -> Result<(), i32> {
    let mut ends = [0_u32; 2];
    sys(
        nr::CHANNEL_CREATE,
        [ends.as_mut_ptr().addr() as u64, 0, 0, 0, 0, 0],
    )
    .map_err(|_| case)?;
    PING_END.store(u64::from(ends[0]), Release);
    PONG_END.store(u64::from(ends[1]), Release);
    READY.store(0, Release);
    GO.store(0, Release);
    let answerer = started(given, second, pong, &PONG_STACK).map_err(|_| case)?;
    await_that(|| READY.load(Acquire) & 1 != 0, case)?;
    let caller = started(given, first, ping, &PING_STACK).map_err(|_| case)?;
    await_that(|| READY.load(Acquire) == 3, case)?;
    // Let the answerer get into its wait; what follows is the two alone on processor 0.
    let mut left = 1_000_u32;
    while left > 0 {
        sys(nr::THREAD_YIELD, [0; 6]).map_err(|_| case)?;
        left -= 1;
    }
    ask(given, ASK_DOMAIN_START, 0, None).map_err(|_| case)?;
    GO.store(1, Release);
    await_that(|| GO.load(Acquire) == 2, case)?;
    // Whatever it says: the two end first, or the check waits on a job with them in it.
    let held = ask(given, ASK_DOMAIN_END, word, None).map_err(|_| case);
    sys(nr::HANDLE_CLOSE, [u64::from(ends[0]), 0, 0, 0, 0, 0]).map_err(|_| case)?;
    // Ended before the next pair runs on their stacks.
    for process in [caller, answerer] {
        let mut observed = 0_u32;
        sys(
            nr::OBJECT_WAIT_ONE,
            [
                process,
                u64::from(ferrix_native_abi::signals::Signals::TERMINATED.0),
                0,
                (&raw mut observed).addr() as u64,
                0,
                0,
            ],
        )
        .map_err(|_| case)?;
        sys(nr::HANDLE_CLOSE, [process, 0, 0, 0, 0, 0]).map_err(|_| case)?;
    }
    held
}

/// Whether the image has reservations and direct switches.
const ASK_EDF_BUILT: u8 = 51;
/// Read the direct switches' count.
const ASK_EDF_START: u8 = 52;
/// It moved by the word.
const ASK_EDF_MOVED: u8 = 53;
/// It did not move, and the decline's did by the word.
const ASK_EDF_DECLINED: u8 = 54;
/// Case 197's answerer's stack: a thread of this process, so its end is not waited for.
static RESERVED_PONG_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// Trips of case 197, each way.
const EDF_TRIPS: u64 = 16;

/// Case 197: under `schedule=edf`, a reserved thread's channel call takes the long way: the
/// direct switch would pass over a reservation and charge neither budget (ledger 7088 E.5,
/// 7134 (d)). The same trips unreserved go direct, so the zero is the decline's.
fn reserved_declines(given: &Given) -> Result<(), i32> {
    if !asked(given, ASK_EDF_BUILT, 0, None)? {
        return Ok(());
    }
    let mut ends = [0_u32; 2];
    sys(
        nr::CHANNEL_CREATE,
        [ends.as_mut_ptr().addr() as u64, 0, 0, 0, 0, 0],
    )
    .map_err(|_| 197)?;
    PONG_END.store(u64::from(ends[1]), Release);
    READY.store(0, Release);
    GO.store(0, Release);
    let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 197)?;
    ask(given, ASK_PIN, 0, Some(me)).map_err(|_| 197)?;
    sys(nr::THREAD_YIELD, [0; 6]).map_err(|_| 197)?;
    sys(
        nr::THREAD_CREATE,
        [
            (pong as *const ()).addr() as u64,
            top(&RESERVED_PONG_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 197)?;
    await_that(|| READY.load(Acquire) & 1 != 0, 197)?;
    let mut left = 1_000_u32;
    while left > 0 {
        sys(nr::THREAD_YIELD, [0; 6]).map_err(|_| 197)?;
        left -= 1;
    }
    let end = u64::from(ends[0]);
    ask(given, ASK_EDF_START, 0, None).map_err(|_| 197)?;
    for trip_number in 0..EDF_TRIPS {
        trip(end, 8, trip_number).map_err(|_| 197)?;
    }
    ask(given, ASK_EDF_MOVED, EDF_TRIPS / 2, None).map_err(|_| 197)?;
    const PERIOD: u64 = 100_000_000;
    sys(
        nr::THREAD_RESERVE,
        [given.job, PERIOD / 5, PERIOD, PERIOD, 0, 0],
    )
    .map_err(|_| 197)?;
    let reserved = (|| -> Result<(), i32> {
        ask(given, ASK_EDF_START, 0, None).map_err(|_| 197)?;
        for trip_number in 0..EDF_TRIPS {
            trip(end, 8, trip_number).map_err(|_| 197)?;
        }
        ask(given, ASK_EDF_DECLINED, EDF_TRIPS / 2, None).map_err(|_| 197)
    })();
    sys(nr::THREAD_RESERVE, [given.job, 0, 0, 0, 0, 0]).map_err(|_| 197)?;
    reserved?;
    sys(nr::HANDLE_CLOSE, [end, 0, 0, 0, 0, 0]).map_err(|_| 197)?;
    let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 197)?;
    ask(given, ASK_PIN, ANY_PROCESSOR, Some(me)).map_err(|_| 197)
}

/// Every case of this file its image has.
pub(super) fn run(given: &Given) -> Result<(), i32> {
    CHECK.store(given.channel, Ordering::Relaxed);
    reserved_declines(given)?;
    switches_across_domains(given)
}

/// Both cases, on an image built `partition` with two processors.
fn switches_across_domains(given: &Given) -> Result<(), i32> {
    if !asked(given, ASK_DOMAIN_BUILT, 0, None)? || !asked(given, ASK_TWO_PROCESSORS, 0, None)? {
        return Ok(());
    }
    CHECK.store(given.channel, Ordering::Relaxed);
    let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 193)?;
    ask(given, ASK_PIN, 1, Some(me)).map_err(|_| 193)?;
    sys(nr::THREAD_YIELD, [0; 6]).map_err(|_| 193)?;
    let one = sys(nr::JOB_CREATE, [given.job, MARKED, 0, 0, 0, 0]).map_err(|_| 193)?;
    let other = sys(nr::JOB_CREATE, [given.job, MARKED, 0, 0, 0, 0]).map_err(|_| 193)?;
    // Direct where the image has direct switches, the long way where it does not: the
    // line says which ran, and the word holds an image with them to it.
    let way = if asked(given, ASK_DOMAIN_DIRECT, 0, None)? {
        DIRECT
    } else {
        0
    };
    pair(given, one, other, way | (ROUNDS * 2 - 2), 193)?;
    let both = sys(nr::JOB_CREATE, [given.job, MARKED, 0, 0, 0, 0]).map_err(|_| 194)?;
    pair(given, both, both, way | WITHIN | (ROUNDS * 2 - 2), 194)?;
    let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 194)?;
    ask(given, ASK_PIN, ANY_PROCESSOR, Some(me)).map_err(|_| 194)
}
