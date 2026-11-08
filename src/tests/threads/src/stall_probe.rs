//! **stall-probe** (`docs/ROBUSTNESS.md` R5): how long the rest of the
//! machine stops while a big threaded process forks, and while it is torn
//! down.
//!
//! A *ticker* process sleeps 1 ms at a time and records its longest gap
//! between two wake-ups in each phase. A *game* process maps 320 MiB of
//! private memory, writes every page of it, and starts 40 threads that keep
//! writing it, as a game's workers do. Then the game forks [`FORKS`] times,
//! each child `_exit`ing at once, as Wine's `CreateProcess` does (a double
//! fork, then `execve`). Then the game is killed and reaped.
//!
//! The ticker's longest gap less its 1 ms sleep is the worst scheduling
//! latency another process saw. Phase 0 is the steady state with the game's
//! threads running, phase 1 the forks, phase 2 the kill and the teardown. The
//! step passes when phase 1 stays within [`FORK_BUDGET_US`] (100 ms, interim
//! until R5's step 2) and phase 2 within [`TEARDOWN_BUDGET_US`] (50 ms, interim
//! too; both go to 10 ms with step 2), and
//! prints the numbers either way, so a run is also a measurement. It is a
//! timing row, not yet evidence for a requirement.
//!
//! Endfield (5 GB resident) stopped the whole of the N150's user space for
//! about 10 s at such a fork, and its own hang watchdog then killed its main
//! thread. The cause was a fork that held the address space's spin lock,
//! preemption off, across work in proportion to the resident pages, with the
//! game's faulting threads spinning on that lock on every other processor.

#![expect(
    unsafe_code,
    reason = "fork, a shared anonymous mapping for the counters, and memory written by hand"
)]

use core::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const PAGE: usize = 4096;

/// The game's resident private memory: at least 300 MB, as R5's row asks.
const BIG: usize = 320 * 1024 * 1024;

/// The game's writing threads.
const WRITERS: usize = 40;

/// The forks the game makes, each child gone at once.
pub(crate) const FORKS: usize = 4;

/// The most another process's 1 ms sleep may overrun by while the game
/// forks. **Interim** (ledger 8299 C): step 1 of R5 leaves `Vmo::fork` under
/// the space's lock, about 33 ms per 320 MiB; step 2, the fork in bounded
/// steps, lowers this to 10 ms.
pub(crate) const FORK_BUDGET_US: u64 = 100_000;

/// The most it may overrun by while the game is killed and torn down.
/// **Interim** too (ledger 8324 CT.1): five runs of step 1 measured 0.06 to
/// 29.9 ms, of a cause not yet found; R5's step 2 names it and lowers this to
/// 10 ms.
pub(crate) const TEARDOWN_BUDGET_US: u64 = 50_000;

/// The ticker's sleep.
const TICK_US: u64 = 1_000;

// The shared words, by index.
/// The phase the ticker files its gaps under: 0 steady, 1 forks, 2 teardown,
/// 3 after.
const PHASE: usize = 0;
/// The longest gap per phase, at `GAP + phase`.
const GAP: usize = 1;
/// Set to stop the ticker.
const STOP: usize = 5;
/// Set by the game once its memory is written and its threads run.
const READY: usize = 6;
/// Set by the game once its forks are done.
const FORKED: usize = 7;
/// The game's longest single fork, in microseconds.
const LONGEST_FORK: usize = 8;
/// How many shared words there are.
const WORDS: usize = 16;

fn now_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    u64::try_from(ts.tv_sec).unwrap_or(0) * 1_000_000
        + u64::try_from(ts.tv_nsec).unwrap_or(0) / 1000
}

/// Words shared by the three processes: a shared anonymous mapping, zeroed,
/// never unmapped.
fn shared_words(fail: fn(&str) -> !) -> &'static [AtomicU64] {
    // SAFETY: a fresh mapping, asked for by nobody else.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            WORDS * 8,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        fail("stall-probe: no shared page for the counters");
    }
    // SAFETY: `WORDS` zeroed words, aligned to a page, alive for good.
    unsafe { core::slice::from_raw_parts(at.cast::<AtomicU64>(), WORDS) }
}

fn word(words: &[AtomicU64], at: usize) -> &AtomicU64 {
    // Every index here is a constant below `WORDS`.
    words
        .get(at)
        .unwrap_or_else(|| words.first().unwrap_or_else(|| std::process::abort()))
}

/// The ticker: sleep 1 ms, file the gap under the current phase, until told
/// to stop.
fn ticker(words: &'static [AtomicU64]) -> ! {
    let mut last = now_us();
    while word(words, STOP).load(Ordering::Acquire) == 0 {
        let pause = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        // SAFETY: a valid timespec.
        let _ = unsafe { libc::nanosleep(&raw const pause, ptr::null_mut()) };
        let now = now_us();
        let phase = usize::try_from(word(words, PHASE).load(Ordering::Acquire)).unwrap_or(3);
        let _ = word(words, GAP + phase.min(3)).fetch_max(now - last, Ordering::AcqRel);
        last = now;
    }
    // SAFETY: ending the ticker.
    unsafe { libc::_exit(0) }
}

/// The game: memory written, threads writing it, then the forks, then
/// waiting to be killed.
fn game(words: &'static [AtomicU64]) -> ! {
    // SAFETY: a fresh private mapping, written below page by page.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            BIG,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        // SAFETY: ending the game; the parent reads the status.
        unsafe { libc::_exit(3) };
    }
    let at = at.addr();
    for page in 0..BIG / PAGE {
        // SAFETY: inside the mapping.
        unsafe { ptr::write_volatile(ptr::with_exposed_provenance_mut::<u8>(at + page * PAGE), 1) };
    }
    for first in 0..WRITERS {
        let spawned = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                loop {
                    for page in (first..BIG / PAGE).step_by(WRITERS) {
                        // SAFETY: inside the mapping, which lives as long as
                        // the process.
                        unsafe {
                            ptr::write_volatile(
                                ptr::with_exposed_provenance_mut::<u8>(at + page * PAGE),
                                2,
                            );
                        };
                    }
                }
            });
        if spawned.is_err() {
            // SAFETY: ending the game.
            unsafe { libc::_exit(4) };
        }
    }
    word(words, READY).store(1, Ordering::Release);
    while word(words, PHASE).load(Ordering::Acquire) != 1 {
        std::thread::yield_now();
    }
    for _ in 0..FORKS {
        let started = now_us();
        // SAFETY: the child only exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: ending the child, as Wine's first child does.
            unsafe { libc::_exit(0) };
        }
        let took = now_us() - started;
        let _ = word(words, LONGEST_FORK).fetch_max(took, Ordering::AcqRel);
        if pid > 0 {
            let mut status = 0;
            // SAFETY: our own child.
            let _ = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        }
    }
    word(words, FORKED).store(1, Ordering::Release);
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The step. Prints its measurement, and fails past the budget.
pub(crate) fn check_stall_probe(fail: fn(&str) -> !) {
    let words = shared_words(fail);
    // SAFETY: the child runs `ticker` alone, which only reads the clock,
    // sleeps and writes the shared words.
    let ticker_pid = unsafe { libc::fork() };
    if ticker_pid == 0 {
        ticker(words);
    }
    if ticker_pid < 0 {
        fail("stall-probe: the ticker could not be forked");
    }
    // SAFETY: the child runs `game` alone.
    let game_pid = unsafe { libc::fork() };
    if game_pid == 0 {
        game(words);
    }
    if game_pid < 0 {
        fail("stall-probe: the game could not be forked");
    }
    let readied = now_us();
    while word(words, READY).load(Ordering::Acquire) == 0 {
        if now_us() - readied > 120_000_000 {
            fail("stall-probe: the game did not get ready in 120 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Phase 0, the steady state, for half a second.
    std::thread::sleep(Duration::from_millis(500));

    let t0 = now_us();
    word(words, PHASE).store(1, Ordering::Release);
    while word(words, FORKED).load(Ordering::Acquire) == 0 {
        if now_us() - t0 > 300_000_000 {
            fail("stall-probe: the game's forks did not end in 300 s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let forks = now_us() - t0;

    word(words, PHASE).store(2, Ordering::Release);
    let t1 = now_us();
    // SAFETY: our own child.
    let _ = unsafe { libc::kill(game_pid, libc::SIGKILL) };
    let mut status = 0;
    // SAFETY: our own child.
    let _ = unsafe { libc::waitpid(game_pid, &raw mut status, 0) };
    let teardown = now_us() - t1;
    // The teardown may finish after the reap: the space can go last.
    std::thread::sleep(Duration::from_millis(300));
    word(words, PHASE).store(3, Ordering::Release);
    std::thread::sleep(Duration::from_millis(50));
    word(words, STOP).store(1, Ordering::Release);
    // SAFETY: our own child.
    let _ = unsafe { libc::waitpid(ticker_pid, &raw mut status, 0) };

    let late = |phase: usize| {
        word(words, GAP + phase)
            .load(Ordering::Acquire)
            .saturating_sub(TICK_US)
    };
    let (steady, forking, tearing) = (late(0), late(1), late(2));
    println!(
        "threads: stall-probe {FORKS} forks of a {} MiB {WRITERS}-thread process took {} ms \
         (the longest {} ms), its kill and reap {} ms; another process's worst lateness \
         past a 1 ms sleep: steady {steady} us, forks {forking} us, teardown {tearing} us \
         (interim budgets {FORK_BUDGET_US} us forking and {TEARDOWN_BUDGET_US} us tearing down)",
        BIG >> 20,
        forks / 1000,
        word(words, LONGEST_FORK).load(Ordering::Acquire) / 1000,
        teardown / 1000,
    );
    if forking > FORK_BUDGET_US {
        fail("stall-probe: another process waited past the budget while a big process forked");
    }
    if tearing > TEARDOWN_BUDGET_US {
        fail(
            "stall-probe: another process waited past the budget while a big process was torn down",
        );
    }
}
