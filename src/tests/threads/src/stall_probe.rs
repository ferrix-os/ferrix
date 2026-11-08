//! **stall-probe** (fault-relanding repro, never to land as is): how long the
//! rest of the machine stops while a big threaded process forks, and while it
//! is torn down. A ticker process sleeps 1 ms at a time and records its longest
//! gap; a game-like process (200 MB resident, 40 threads writing it) forks
//! (child `_exit`s at once, as Wine's double fork does) and is then killed.

#![expect(unsafe_code, reason = "fork, shared counters, mappings written by hand")]

use core::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

const PAGE: usize = 4096;
const BIG: usize = 200 * 1024 * 1024;

fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

fn shared_words(n: usize) -> &'static [AtomicU64] {
    // SAFETY: a fresh shared anonymous mapping, zeroed, never unmapped.
    unsafe {
        let at = libc::mmap(ptr::null_mut(), n * 8, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS, -1, 0);
        core::slice::from_raw_parts(at.cast::<AtomicU64>(), n)
    }
}

/// The step: prints the ticker's longest gap in each phase.
pub fn check_stall_probe(_fail: fn(&str) -> !) {
    // [0] phase, [1..4] max gap per phase, [5] ticker stop, [6] game ready
    let words = shared_words(16);
    // SAFETY: the ticker only reads the clock, sleeps and writes the words.
    let ticker = unsafe { libc::fork() };
    if ticker == 0 {
        let mut last = now_us();
        while words[5].load(Ordering::Acquire) == 0 {
            let pause = libc::timespec { tv_sec: 0, tv_nsec: 1_000_000 };
            // SAFETY: a valid timespec.
            let _ = unsafe { libc::nanosleep(&raw const pause, ptr::null_mut()) };
            let now = now_us();
            let phase = words[0].load(Ordering::Acquire) as usize;
            let _ = words[1 + phase.min(3)].fetch_max(now - last, Ordering::AcqRel);
            last = now;
        }
        // SAFETY: ending the ticker.
        unsafe { libc::_exit(0) };
    }
    // SAFETY: the game child runs below alone.
    let game = unsafe { libc::fork() };
    if game == 0 {
        // SAFETY: a fresh private mapping written page by page.
        let at = unsafe { libc::mmap(ptr::null_mut(), BIG, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) }.addr();
        for page in 0..BIG / PAGE {
            // SAFETY: inside the mapping.
            unsafe { ptr::write_volatile(ptr::with_exposed_provenance_mut::<u8>(at + page * PAGE), 1) };
        }
        for t in 0..40_usize {
            let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || loop {
                for page in (t..BIG / PAGE).step_by(40) {
                    // SAFETY: inside the mapping.
                    unsafe { ptr::write_volatile(ptr::with_exposed_provenance_mut::<u8>(at + page * PAGE), 2) };
                }
            });
        }
        words[6].store(1, Ordering::Release);
        // Phase 1: forks, each child gone at once.
        while words[0].load(Ordering::Acquire) != 1 { std::thread::yield_now(); }
        for _ in 0..4 {
            // SAFETY: the child only exits.
            let pid = unsafe { libc::fork() };
            if pid == 0 { unsafe { libc::_exit(0) }; }
            let mut status = 0;
            // SAFETY: our child.
            let _ = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        }
        words[7].store(1, Ordering::Release);
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
    }
    while words[6].load(Ordering::Acquire) == 0 { std::thread::sleep(std::time::Duration::from_millis(10)); }
    std::thread::sleep(std::time::Duration::from_millis(500));
    // Phase 0 was the steady state; 1 the forks; 2 the kill and teardown.
    let t0 = now_us();
    words[0].store(1, Ordering::Release);
    while words[7].load(Ordering::Acquire) == 0 { std::thread::sleep(std::time::Duration::from_millis(5)); }
    let forks = now_us() - t0;
    words[0].store(2, Ordering::Release);
    let t1 = now_us();
    // SAFETY: our child.
    let _ = unsafe { libc::kill(game, libc::SIGKILL) };
    let mut status = 0;
    // SAFETY: our child.
    let _ = unsafe { libc::waitpid(game, &raw mut status, 0) };
    let teardown = now_us() - t1;
    std::thread::sleep(std::time::Duration::from_millis(200));
    words[0].store(3, Ordering::Release);
    std::thread::sleep(std::time::Duration::from_millis(100));
    words[5].store(1, Ordering::Release);
    // SAFETY: our child.
    let _ = unsafe { libc::waitpid(ticker, &raw mut status, 0) };
    println!(
        "threads: stall-probe 4 forks of a 200 MB 40-thread process took {} ms, its teardown {} ms; another process's longest 1 ms sleep: steady {} us, during the forks {} us, during the teardown {} us",
        forks / 1000, teardown / 1000,
        words[1].load(Ordering::Acquire), words[2].load(Ordering::Acquire), words[3].load(Ordering::Acquire)
    );
}
