//! Microkernel M0's Linux rows, timed (`docs/MICROKERNEL.md` §7,
//! `docs/roadmap/microkernel-m0-baselines.md`): run as init by
//! `cargo xtask bench-rows`, and the same program as `/init` of a Linux guest
//! for the like-for-like comparison.
//!
//! Each sample is timed as `pipe-bench`'s are: `lfence; rdtsc; lfence` on
//! both sides of one operation, the samples sorted, the percentiles read off
//! the sorted array, five repetitions a test and the median of their p50s on
//! the summary line. The lines are `pipe-bench`'s: `LB name=<what> rep=<r>
//! n=<count> min=<ticks> p50= p90= p99= mean= p50_ns= p90_ns= p99_ns=`, then
//! `LB summary name=<what> p50_ns=<median of the p50s>`, and `LB done` at the
//! end; `LB error <what>: <why>` and status 1 on a failure.
//!
//! * `null-getppid`: `getppid`, the bare call. The frozen baseline's row.
//! * `getppid-mean`: the same call by `clockbench`'s method, which measured
//!   the 357 ns of 2026-10-10: 20,000 calls between two `clock_gettime`
//!   readings, their mean (a repetition's figure is that mean, in every
//!   column).
//! * `fork-exec-wait`: `fork`, the child `execve`s `/bin/rows-true` (a
//!   static program that returns at once), the parent `waitpid`s it.
//! * `fork-exit-wait`: the same without the `execve`, the child `_exit`s:
//!   what of the row is the fork and the wait.
//! * `mmap-munmap-4K`: one private anonymous 4 KiB mapping made and unmade.
//! * `mmap-touch-munmap-4K`: the same with one byte written between, so the
//!   page is faulted in and freed: an allocator's churn. The row's figure.
//! * `signal-self`: `tgkill` of the calling thread with `SIGUSR1`, caught by
//!   a handler that counts: the signal delivered to the running thread on its
//!   way out of the call, the handler run and `rt_sigreturn` (lmbench's
//!   `lat_sig catch`).
//! * `futex-thread-pingpong`: two threads of one process hand a word over
//!   with `FUTEX_WAKE_PRIVATE` and `FUTEX_WAIT_PRIVATE`; one sample is a wake
//!   of the waiting thread and the wait for its wake back.
//! * `pagecache-miss-4K`: where `/data` holds the rustc volume, 4 KiB `pread`s
//!   of its two largest libraries at every MiB, each the first read of its
//!   page in this boot: the page cache's miss through the filesystem and the
//!   ring-3 block driver. One pass (a page misses once a boot), so its
//!   summary is the one repetition's p50; the seam's counters say how many
//!   pages each read filled from the disk.
//! * `LB cpuid ...`: the speculation bits the processor model passes on
//!   (`MICROKERNEL.md` §7, HR6).

use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// Untimed operations before each repetition's samples, for the fast tests.
const WARMUP: usize = 2_000;
/// Timed operations a repetition, for the fast tests.
const SAMPLES: usize = 20_000;
/// Untimed and timed operations a repetition for the process tests.
const PROCESS_WARMUP: usize = 20;
/// See [`PROCESS_WARMUP`].
const PROCESS_SAMPLES: usize = 1_000;
/// Repetitions a test.
const REPEATS: usize = 5;
/// `clockbench`'s count of `getppid` calls between its two clock readings.
const MEAN_CALLS: usize = 20_000;
/// The page-cache test's stride and read.
const STRIDE: u64 = 1 << 20;
/// See [`STRIDE`].
const PAGE: usize = 4096;
/// Where the fork+exec+wait test's program is carried.
const TRUE_PROGRAM: &core::ffi::CStr = c"/bin/rows-true";
/// The negative controls' injected delay: each row named here gets that many
/// nanoseconds of busy wait inside every one of its timed samples. Empty in
/// every measured run; a control replaces this one line (`gate.sh control`,
/// `docs/roadmap/microkernel-m0-baselines.md`) and the `LB inject` line says
/// that it did.
const INJECT: &[(&str, u64)] = &[];

/// Spin for the delay [`INJECT`] gives `name`, if any: `per_us` counter
/// ticks a microsecond.
#[inline(always)]
fn inject(name: &str, per_us: u64) {
    let Some(&(_, ns)) = INJECT.iter().find(|(row, _)| *row == name) else {
        return;
    };
    let ticks = ns.saturating_mul(per_us) / 1000;
    let start = now();
    while now().saturating_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

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
        monotonic_ns()
    }
}

/// The monotonic clock in nanoseconds, through the C library.
fn monotonic_ns() -> u64 {
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
    let us = u64::try_from(wall.elapsed().as_micros())
        .unwrap_or(1)
        .max(1);
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

/// The samples of one repetition, and the scale to nanoseconds.
#[derive(Debug)]
struct Samples {
    /// Ticks an operation.
    ticks: Vec<u64>,
    /// Counter ticks a microsecond.
    per_us: u64,
}

impl Samples {
    /// Ticks to nanoseconds.
    fn ns(&self, ticks: u64) -> u64 {
        ticks.saturating_mul(1000) / self.per_us
    }

    /// Sort, print the repetition's line, and answer its p50 in ns.
    fn report(&mut self, name: &str, rep: usize) -> u64 {
        self.ticks.sort_unstable();
        let n = self.ticks.len();
        let at = |q: usize| self.ticks.get(n * q / 100).copied().unwrap_or(0);
        let mean = self.ticks.iter().sum::<u64>() / u64::try_from(n.max(1)).unwrap_or(1);
        let (min, p50, p90, p99) = (at(0), at(50), at(90), at(99));
        println!(
            "LB name={name} rep={rep} n={n} min={min} p50={p50} p90={p90} p99={p99} mean={mean} \
             p50_ns={} p90_ns={} p99_ns={}",
            self.ns(p50),
            self.ns(p90),
            self.ns(p99)
        );
        self.ticks.clear();
        self.ns(p50)
    }
}

/// Print the summary line: the median of the repetitions' p50s.
fn summarize(name: &str, mut p50s: Vec<u64>) {
    p50s.sort_unstable();
    let median = p50s.get(p50s.len() / 2).copied().unwrap_or(0);
    println!("LB summary name={name} p50_ns={median}");
    let _ = std::io::stdout().flush();
}

/// Time `op` REPEATS times, `warmup` untimed and `samples` timed calls each,
/// and print the summary line.
fn bench(name: &str, samples: &mut Samples, warmup: usize, count: usize, mut op: impl FnMut()) {
    let mut p50s = Vec::with_capacity(REPEATS);
    for rep in 1..=REPEATS {
        for _ in 0..warmup {
            op();
        }
        for _ in 0..count {
            let start = now();
            op();
            inject(name, samples.per_us);
            let end = now();
            samples.ticks.push(end.saturating_sub(start));
        }
        p50s.push(samples.report(name, rep));
    }
    summarize(name, p50s);
}

/// `getppid`, which always succeeds.
fn getppid() {
    // SAFETY: `getppid` has no arguments and always succeeds.
    let _ = unsafe { libc::syscall(libc::SYS_getppid) };
}

/// `clockbench`'s method: [`MEAN_CALLS`] calls between two monotonic clock
/// readings, their mean; five repetitions after one untimed.
fn bench_getppid_mean() {
    let name = "getppid-mean";
    let mut p50s = Vec::with_capacity(REPEATS);
    for rep in 0..=REPEATS {
        let start = monotonic_ns();
        for _ in 0..MEAN_CALLS {
            getppid();
        }
        let mean = monotonic_ns().saturating_sub(start) / u64::try_from(MEAN_CALLS).unwrap_or(1);
        if rep == 0 {
            continue;
        }
        println!(
            "LB name={name} rep={rep} n={MEAN_CALLS} min=0 p50=0 p90=0 p99=0 mean=0 \
             p50_ns={mean} p90_ns={mean} p99_ns={mean}"
        );
        p50s.push(mean);
    }
    summarize(name, p50s);
}

/// Children reaped with status 0, for a failure's line.
static REAPED: AtomicU64 = AtomicU64::new(0);

/// Wait for `child` and require it to have exited with 0.
fn reap(child: libc::pid_t, what: &str) {
    let mut status = 0;
    // SAFETY: `status` is writable; `child` is this process's child.
    if unsafe { libc::waitpid(child, &raw mut status, 0) } != child {
        fail(what);
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let free = meminfo
            .lines()
            .find(|line| line.starts_with("MemFree"))
            .unwrap_or("MemFree unknown");
        println!(
            "LB error {what}: the child ended with status {status:#x}, after {} children ended \
             with 0 ({free})",
            REAPED.load(Ordering::Relaxed)
        );
        let _ = std::io::stdout().flush();
        // SAFETY: as in `fail`.
        unsafe { libc::_exit(1) }
    }
    let _ = REAPED.fetch_add(1, Ordering::Relaxed);
}

/// The guest's free memory in KiB, from `/proc/meminfo`, when it says.
fn free_kib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemFree"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Time the process test `name` and say how much free memory it took.
fn bench_process(name: &str, samples: &mut Samples, exec: bool) {
    let before = free_kib();
    bench(name, samples, PROCESS_WARMUP, PROCESS_SAMPLES, || {
        fork_wait(exec)
    });
    if let (Some(before), Some(after)) = (before, free_kib()) {
        let children = u64::try_from(REPEATS * (PROCESS_WARMUP + PROCESS_SAMPLES)).unwrap_or(1);
        println!(
            "LB meminfo {name} free-before-kib {before} free-after-kib {after} children {children} \
             kib-per-child {}",
            before.saturating_sub(after) / children.max(1)
        );
    }
}

/// `fork`, then in the child `execve` of [`TRUE_PROGRAM`] (or `_exit(0)`
/// when `exec` is false), and the parent's `waitpid`.
fn fork_wait(exec: bool) {
    // SAFETY: one thread runs here; the child only execs or exits.
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail("fork");
    }
    if child == 0 {
        if exec {
            let argv = [TRUE_PROGRAM.as_ptr(), core::ptr::null()];
            let envp = [core::ptr::null::<libc::c_char>()];
            // SAFETY: a NUL-terminated path, and argv and envp ending in null.
            let _ = unsafe { libc::execve(TRUE_PROGRAM.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
            // SAFETY: as in `fail`; the child says nothing, its status does.
            unsafe { libc::_exit(127) }
        }
        // SAFETY: the child ends here without running the parent's exit.
        unsafe { libc::_exit(0) }
    }
    reap(
        child,
        if exec {
            "fork-exec-wait"
        } else {
            "fork-exit-wait"
        },
    );
}

/// One private anonymous 4 KiB mapping, written once when `touch`, unmapped.
fn mmap_churn(touch: bool) {
    // SAFETY: a fresh anonymous mapping, no file.
    let page = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        fail("mmap");
    }
    if touch {
        // SAFETY: the page was just mapped writable.
        unsafe { core::ptr::write_volatile(page.cast::<u8>(), 1) };
    }
    // SAFETY: unmapping the mapping made above, which nothing else uses.
    if unsafe { libc::munmap(page, PAGE) } != 0 {
        fail("munmap");
    }
}

/// Signals the handler has caught.
static CAUGHT: AtomicU64 = AtomicU64::new(0);

/// `SIGUSR1`'s handler: count.
extern "C" fn caught(_signal: libc::c_int) {
    let _ = CAUGHT.fetch_add(1, Ordering::Relaxed);
}

/// The self-signal test: the handler installed, then `tgkill` of this thread.
fn bench_signal(samples: &mut Samples) {
    // SAFETY: a zeroed `sigaction` is a valid start; the handler is a plain
    // `extern "C"` function that only touches an atomic.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = caught as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let _ = libc::sigemptyset(&raw mut action.sa_mask);
        if libc::sigaction(libc::SIGUSR1, &raw const action, core::ptr::null_mut()) != 0 {
            fail("sigaction");
        }
    }
    // SAFETY: no arguments.
    let pid = unsafe { libc::getpid() };
    // SAFETY: no arguments.
    let tid = unsafe { libc::gettid() };
    let before = CAUGHT.load(Ordering::Relaxed);
    bench("signal-self", samples, WARMUP, SAMPLES, || {
        // SAFETY: a signal to this thread, whose handler is installed.
        let _ = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGUSR1) };
    });
    let expected = u64::try_from(REPEATS * (WARMUP + SAMPLES)).unwrap_or(0);
    let got = CAUGHT.load(Ordering::Relaxed).saturating_sub(before);
    if got != expected {
        println!("LB error signal-self: {got} signals caught of {expected} sent");
        let _ = std::io::stdout().flush();
        // SAFETY: as in `fail`.
        unsafe { libc::_exit(1) }
    }
}

/// The word the futex test's two threads hand over: 1 when the main thread
/// has sent, 0 when the other has answered.
static WORD: AtomicU32 = AtomicU32::new(0);

/// `futex(2)` on [`WORD`], private to this process.
fn futex(op: i32, value: u32) {
    // SAFETY: `WORD` is a static, aligned, alive for the program.
    let _ = unsafe {
        libc::syscall(
            libc::SYS_futex,
            WORD.as_ptr(),
            op | libc::FUTEX_PRIVATE_FLAG,
            value,
            core::ptr::null::<libc::timespec>(),
            core::ptr::null::<u32>(),
            0,
        )
    };
}

/// The futex ping-pong between two threads of this process.
fn bench_futex_threads(samples: &mut Samples) {
    WORD.store(0, Ordering::Release);
    let total = REPEATS * (WARMUP + SAMPLES);
    let other = std::thread::spawn(move || {
        for _ in 0..total {
            while WORD.load(Ordering::Acquire) == 0 {
                futex(libc::FUTEX_WAIT, 0);
            }
            WORD.store(0, Ordering::Release);
            futex(libc::FUTEX_WAKE, 1);
        }
    });
    bench("futex-thread-pingpong", samples, WARMUP, SAMPLES, || {
        WORD.store(1, Ordering::Release);
        futex(libc::FUTEX_WAKE, 1);
        while WORD.load(Ordering::Acquire) == 1 {
            futex(libc::FUTEX_WAIT, 1);
        }
    });
    if other.join().is_err() {
        println!("LB error futex-thread-pingpong: the other thread did not finish");
        let _ = std::io::stdout().flush();
        // SAFETY: as in `fail`.
        unsafe { libc::_exit(1) }
    }
}

/// The seam's counters (`/proc/ferrix-seam`): `from-disk` and `submitted`,
/// where the kernel has them.
fn seam_counters() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/ferrix-seam").ok()?;
    let words: Vec<&str> = text.split_whitespace().collect();
    let field = |name: &str| -> Option<u64> {
        let at = words.iter().position(|word| *word == name)?;
        words.get(at + 1)?.parse().ok()
    };
    Some((field("from-disk")?, field("submitted")?))
}

/// The page-cache test: the two largest files of `/data/rust/lib`, read 4 KiB
/// at every [`STRIDE`] after they are opened, each read timed.
fn bench_pagecache(samples: &mut Samples) {
    let name = "pagecache-miss-4K";
    let Ok(entries) = std::fs::read_dir("/data/rust/lib") else {
        println!("LB skip {name}: no /data/rust/lib (the rustc volume is not attached)");
        return;
    };
    let mut files: Vec<(u64, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| (meta.len(), entry.path()))
        })
        .collect();
    files.sort_unstable();
    let opened: Vec<(u64, std::fs::File, String)> = files
        .iter()
        .rev()
        .take(2)
        .filter_map(|(len, path)| {
            Some((
                *len,
                std::fs::File::open(path).ok()?,
                path.display().to_string(),
            ))
        })
        .collect();
    if opened.is_empty() {
        fail("pagecache-miss-4K: no file to read");
    }
    let mut buf = vec![0u8; PAGE];
    let before = seam_counters();
    for (len, file, path) in &opened {
        use std::os::fd::AsRawFd as _;
        println!("LB pagecache file {path} bytes {len}");
        let mut offset = 0u64;
        while offset.saturating_add(PAGE as u64) <= *len {
            let at = libc::off_t::try_from(offset).unwrap_or(0);
            let start = now();
            // SAFETY: `buf` is writable for PAGE bytes.
            let n = unsafe { libc::pread(file.as_raw_fd(), buf.as_mut_ptr().cast(), PAGE, at) };
            inject(name, samples.per_us);
            let end = now();
            if usize::try_from(n).ok() != Some(PAGE) {
                fail("pagecache-miss-4K: pread");
            }
            samples.ticks.push(end.saturating_sub(start));
            offset = offset.saturating_add(STRIDE);
        }
    }
    let reads = samples.ticks.len();
    let p50 = samples.report(name, 1);
    if let (Some((disk0, sub0)), Some((disk1, sub1))) = (before, seam_counters()) {
        println!(
            "LB pagecache reads {reads} from-disk-pages {} ring-commands {}",
            disk1.saturating_sub(disk0),
            sub1.saturating_sub(sub0)
        );
    }
    summarize(name, vec![p50]);
}

/// The processor's speculation bits, as this guest sees them: CPUID
/// 0x8000_0021 EAX and 0x8000_0008 EBX (AMD), and 7.0 EDX (both vendors).
fn cpuid_line() {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::__cpuid_count;
        // `cpuid` is unprivileged and safe; a leaf past the maximum reads as
        // the highest leaf, which the maximum check below avoids.
        let (max_ext, e21, e08, l7) = {
            let max_ext = __cpuid_count(0x8000_0000, 0).eax;
            let e21 = if max_ext >= 0x8000_0021 {
                __cpuid_count(0x8000_0021, 0).eax
            } else {
                0
            };
            let e08 = if max_ext >= 0x8000_0008 {
                __cpuid_count(0x8000_0008, 0).ebx
            } else {
                0
            };
            (max_ext, e21, e08, __cpuid_count(7, 0).edx)
        };
        let bit = |word: u32, n: u32| (word >> n) & 1;
        println!(
            "LB cpuid max_ext={max_ext:#x} 80000021.eax={e21:#010x} 80000008.ebx={e08:#010x} \
             7.edx={l7:#010x} auto_ibrs={} eraps={} sbpb={} ibpb_brtype={} srso_no={} \
             srso_user_kernel_no={} ibpb={} btc_no={} ibrs_7={} stibp_7={} ssbd_7={} \
             arch_capabilities_7={}",
            bit(e21, 8),
            bit(e21, 24),
            bit(e21, 27),
            bit(e21, 28),
            bit(e21, 29),
            bit(e21, 30),
            bit(e08, 12),
            bit(e08, 29),
            bit(l7, 26),
            bit(l7, 27),
            bit(l7, 31),
            bit(l7, 29),
        );
    }
}

fn main() {
    let per_us = ticks_per_us();
    println!(
        "LB start warmup={WARMUP} samples={SAMPLES} process_samples={PROCESS_SAMPLES} \
         repeats={REPEATS} ticks_per_us={per_us}"
    );
    cpuid_line();
    for (row, ns) in INJECT {
        println!("LB inject {row} {ns} ns a sample: a negative control's delay");
    }
    let mut samples = Samples {
        ticks: Vec::with_capacity(SAMPLES),
        per_us,
    };
    // The page cache first, before any other test's memory use could evict
    // what the boot left cached (it cached nothing of these files).
    bench_pagecache(&mut samples);
    bench("null-getppid", &mut samples, WARMUP, SAMPLES, getppid);
    bench_getppid_mean();
    bench("mmap-munmap-4K", &mut samples, WARMUP, SAMPLES, || {
        mmap_churn(false)
    });
    bench(
        "mmap-touch-munmap-4K",
        &mut samples,
        WARMUP,
        SAMPLES,
        || mmap_churn(true),
    );
    bench_signal(&mut samples);
    bench_futex_threads(&mut samples);
    bench_process("fork-exec-wait", &mut samples, true);
    bench_process("fork-exit-wait", &mut samples, false);
    println!("LB done");
    let _ = std::io::stdout().flush();
}
