//! Linux-ABI round trips between two processes, timed: run as init by
//! `cargo xtask bench-pipe`, and the same program as `/init` of a Linux guest
//! for the like-for-like comparison.
//!
//! It is the in-tree form of the `lbench` that measured Linux's own floors
//! under the same QEMU and KVM line (`~/.local/share/ferrix/linux-bench`,
//! 2026-10-06) and of `rbench`'s ping-pongs on the host: `lfence; rdtsc;
//! lfence` around each sample, one sample one round trip, 2,000 untimed
//! round trips and then 20,000 timed, five times, the samples sorted and the
//! percentiles read off the sorted array. Both processes share the one
//! processor of a `--smp 1` guest, so each round trip is two switches of
//! address space, as `bench-ipc`'s `domain-call` is.
//!
//! The lines, one per repetition: `LB name=<what> rep=<r> n=<count>
//! min=<ticks> p50= p90= p99= mean= p50_ns= p90_ns= p99_ns=`, then
//! `LB summary name=<what> p50_ns=<median of the five p50s>`, and `LB done`
//! with status 0 at the end; `LB error <what>: <why>` and status 1 on a
//! failure.
//!
//! * `null-getppid`: `getppid`, the general system call's floor;
//! * `pipe-pingpong-8B`: the parent writes 8 bytes into one pipe and reads 8
//!   back from another; a forked child reads and echoes them;
//! * `unix-stream-pingpong-8B`: the same over a `socketpair(AF_UNIX,
//!   SOCK_STREAM)`, one socket a side;
//! * `futex-pingpong`: a shared word handed over with `FUTEX_WAKE` and
//!   `FUTEX_WAIT` on a shared anonymous page (not private: two processes).

use std::io::Write;
use std::time::Instant;

/// Untimed round trips before each repetition's samples.
const WARMUP: usize = 2_000;
/// Timed round trips a repetition.
const SAMPLES: usize = 20_000;
/// Repetitions a test.
const REPEATS: usize = if cfg!(feature = "profile") { 60 } else { 5 };

/// With `profile`, the one test to run.
const ONLY: Option<&str> = if cfg!(feature = "profile") {
    match option_env!("PIPE_BENCH_ONLY") {
        Some(name) => Some(name),
        None => Some("pipe"),
    }
} else {
    None
};

/// Whether `test` runs.
fn runs(test: &str) -> bool {
    ONLY.is_none_or(|only| only == test)
}
/// Bytes a ping-pong message carries.
const LEN: usize = 8;

/// The counter: the TSC on x86-64, fenced as `bench-ipc`'s is; elsewhere
/// the monotonic clock in nanoseconds.
#[inline(always)]
fn now() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `lfence` and `rdtsc` are unprivileged and touch no memory.
        unsafe {
            core::arch::x86_64::_mm_lfence();
            let tsc = core::arch::x86_64::_rdtsc();
            core::arch::x86_64::_mm_lfence();
            tsc
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid `timespec` the call writes.
        let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
        u64::try_from(ts.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(ts.tv_nsec).unwrap_or(0))
    }
}

/// Counter ticks a microsecond, measured against the monotonic clock over
/// 200 ms (1000 where the counter is already in nanoseconds).
fn ticks_per_us() -> u64 {
    if !cfg!(target_arch = "x86_64") {
        return 1000;
    }
    let wall = Instant::now();
    let start = now();
    while wall.elapsed().as_millis() < 200 {}
    let ticks = now().saturating_sub(start);
    let us = u64::try_from(wall.elapsed().as_micros()).unwrap_or(1).max(1);
    (ticks / us).max(1)
}

/// A failure: its line, and the status 1.
fn fail(what: &str) -> ! {
    let why = std::io::Error::last_os_error();
    println!("LB error {what}: {why}");
    let _ = std::io::stdout().flush();
    // SAFETY: `_exit` ends the process; nothing is left to unwind.
    unsafe { libc::_exit(1) }
}

/// Read exactly `buf.len()` bytes from `fd`.
fn read_all(fd: i32, buf: &mut [u8]) {
    let mut got = 0;
    while let Some(rest) = buf.get_mut(got..).filter(|rest| !rest.is_empty()) {
        // SAFETY: `rest` is writable for its length.
        let n = unsafe { libc::read(fd, rest.as_mut_ptr().cast(), rest.len()) };
        if n <= 0 {
            fail("read");
        }
        got += usize::try_from(n).unwrap_or(0);
    }
}

/// Write all of `buf` to `fd` in one call.
fn write_all(fd: i32, buf: &[u8]) {
    // SAFETY: `buf` is readable for its length.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if usize::try_from(n).ok() != Some(buf.len()) {
        fail("write");
    }
}

/// The samples of one repetition, and the scale to nanoseconds.
struct Samples {
    /// Ticks a round trip.
    ticks: Vec<u64>,
    /// Counter ticks a microsecond.
    per_us: u64,
}

impl Samples {
    /// Sort, print the repetition's line, and answer its p50 in ns.
    fn report(&mut self, name: &str, rep: usize) -> u64 {
        self.ticks.sort_unstable();
        let n = self.ticks.len();
        let at = |q: usize| self.ticks.get(n * q / 100).copied().unwrap_or(0);
        let ns = |ticks: u64| ticks.saturating_mul(1000) / self.per_us;
        let mean = self.ticks.iter().sum::<u64>() / u64::try_from(n.max(1)).unwrap_or(1);
        let (min, p50, p90, p99) = (at(0), at(50), at(90), at(99));
        println!(
            "LB name={name} rep={rep} n={n} min={min} p50={p50} p90={p90} p99={p99} mean={mean} \
             p50_ns={} p90_ns={} p99_ns={}",
            ns(p50),
            ns(p90),
            ns(p99)
        );
        self.ticks.clear();
        ns(p50)
    }
}

/// Time `op` REPEATS times, WARMUP untimed and SAMPLES timed calls each,
/// and print the summary line.
fn bench(name: &str, samples: &mut Samples, mut op: impl FnMut()) {
    let mut p50s = Vec::with_capacity(REPEATS);
    for rep in 1..=REPEATS {
        for _ in 0..WARMUP {
            op();
        }
        for _ in 0..SAMPLES {
            let start = now();
            op();
            let end = now();
            samples.ticks.push(end.saturating_sub(start));
        }
        p50s.push(samples.report(name, rep));
    }
    p50s.sort_unstable();
    let median = p50s.get(REPEATS / 2).copied().unwrap_or(0);
    println!("LB summary name={name} p50_ns={median}");
}

/// Fork an echo child that reads LEN bytes from `child_in` and writes them
/// to `child_out` for every round trip, closing `close_in_child` first; time
/// the parent's write to `parent_out` and read from `parent_in`.
fn pingpong(
    name: &str,
    samples: &mut Samples,
    (parent_in, parent_out): (i32, i32),
    (child_in, child_out): (i32, i32),
    close_in_child: &[i32],
) {
    let total = REPEATS * (WARMUP + SAMPLES);
    // SAFETY: one thread; the child only reads, writes and exits.
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail("fork");
    }
    if child == 0 {
        for &fd in close_in_child {
            // SAFETY: closing a descriptor the child does not use.
            let _ = unsafe { libc::close(fd) };
        }
        let mut buf = [0u8; LEN];
        for _ in 0..total {
            read_all(child_in, &mut buf);
            write_all(child_out, &buf);
        }
        // SAFETY: the child ends here without running the parent's exit.
        unsafe { libc::_exit(0) };
    }
    let mut buf = [7u8; LEN];
    let mut sent: u8 = 0;
    bench(name, samples, || {
        sent = sent.wrapping_add(1);
        buf[0] = sent;
        write_all(parent_out, &buf);
        read_all(parent_in, &mut buf);
        if buf[0] != sent {
            println!("LB error {name}: the echo did not carry the bytes sent");
            // SAFETY: as in `fail`.
            unsafe { libc::_exit(1) };
        }
    });
    let mut status = 0;
    // SAFETY: `status` is writable; `child` is this process's child.
    let _ = unsafe { libc::waitpid(child, &raw mut status, 0) };
}

/// The pipe ping-pong.
fn bench_pipe(samples: &mut Samples) {
    let mut to = [0i32; 2];
    let mut from = [0i32; 2];
    // SAFETY: each array holds the two descriptors `pipe` writes.
    if unsafe { libc::pipe(to.as_mut_ptr()) } != 0 || unsafe { libc::pipe(from.as_mut_ptr()) } != 0
    {
        fail("pipe");
    }
    let [to_read, to_write] = to;
    let [from_read, from_write] = from;
    // The parent writes `to` and reads `from`; the child the other way.
    pingpong(
        "pipe-pingpong-8B",
        samples,
        (from_read, to_write),
        (to_read, from_write),
        &[to_write, from_read],
    );
    for fd in [to_read, to_write, from_read, from_write] {
        // SAFETY: closing this function's own descriptors.
        let _ = unsafe { libc::close(fd) };
    }
}

/// The Unix stream socket ping-pong.
fn bench_unix(samples: &mut Samples) {
    let mut pair = [0i32; 2];
    // SAFETY: `pair` holds the two descriptors `socketpair` writes.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0 {
        fail("socketpair");
    }
    let [parent, child] = pair;
    pingpong(
        "unix-stream-pingpong-8B",
        samples,
        (parent, parent),
        (child, child),
        &[parent],
    );
    for fd in pair {
        // SAFETY: closing this function's own descriptors.
        let _ = unsafe { libc::close(fd) };
    }
}

/// `futex(2)` on a shared word.
fn futex(word: *mut u32, op: i32, value: u32) {
    // SAFETY: `word` points into a live shared mapping.
    let _ = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word,
            op,
            value,
            core::ptr::null::<libc::timespec>(),
            core::ptr::null::<u32>(),
            0,
        )
    };
}

/// The futex ping-pong: the word is 1 when the parent has sent, 0 when the
/// child has answered; each side always makes its wake and its wait.
fn bench_futex(samples: &mut Samples) {
    // SAFETY: a fresh shared anonymous page.
    let page = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        fail("mmap");
    }
    let word = page.cast::<u32>();
    // SAFETY: `word` is the start of the page, aligned, shared with the child.
    let atomic = unsafe { core::sync::atomic::AtomicU32::from_ptr(word) };
    use core::sync::atomic::Ordering::{Acquire, Release};
    atomic.store(0, Release);
    let total = REPEATS * (WARMUP + SAMPLES);
    // SAFETY: one thread; the child only touches the shared word.
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail("fork");
    }
    if child == 0 {
        for _ in 0..total {
            while atomic.load(Acquire) == 0 {
                futex(word, libc::FUTEX_WAIT, 0);
            }
            atomic.store(0, Release);
            futex(word, libc::FUTEX_WAKE, 1);
        }
        // SAFETY: the child ends here.
        unsafe { libc::_exit(0) };
    }
    bench("futex-pingpong", samples, || {
        atomic.store(1, Release);
        futex(word, libc::FUTEX_WAKE, 1);
        while atomic.load(Acquire) == 1 {
            futex(word, libc::FUTEX_WAIT, 1);
        }
    });
    let mut status = 0;
    // SAFETY: as in `pingpong`.
    let _ = unsafe { libc::waitpid(child, &raw mut status, 0) };
}

fn main() {
    let per_us = ticks_per_us();
    println!("LB start warmup={WARMUP} samples={SAMPLES} repeats={REPEATS} ticks_per_us={per_us}");
    let mut samples = Samples {
        ticks: Vec::with_capacity(SAMPLES),
        per_us,
    };
    if runs("null") {
        bench("null-getppid", &mut samples, || {
            // SAFETY: `getppid` has no arguments and always succeeds.
            let _ = unsafe { libc::getppid() };
        });
    }
    if runs("pipe") {
        bench_pipe(&mut samples);
    }
    if runs("unix") {
        bench_unix(&mut samples);
    }
    if runs("futex") {
        bench_futex(&mut samples);
    }
    println!("LB done");
    let _ = std::io::stdout().flush();
}
