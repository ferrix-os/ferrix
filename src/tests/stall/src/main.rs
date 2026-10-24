//! Woken sleepers against a ring-3 load that saturates every processor,
//! run as init by `cargo xtask test-stall` (finding F-95).
//!
//! On the N150, a game starting kept every other program off every core for
//! one to three seconds: a 10 ms sleeper pinned to each core woke up to 3 s
//! late, all in the same second. This program is that measurement without
//! the game. Canaries, one pinned to each processor and one free to run
//! anywhere, each in a process of its own, sleep 10 ms again and again and
//! record how late each sleep ended. Beside them, one phase after another,
//! a load process is started and killed:
//!
//! * **quiet**: no load, the floor;
//! * **hog**: 16 threads that only compute, the scheduler's own share;
//! * **futex**: 8 pairs of threads handing a futex back and forth;
//! * **fault**: 8 threads that map 32 MiB, touch every page and unmap it;
//! * **bigunmap**: 6 such threads at 8 MiB, and one that maps, fills and
//!   unmaps a large region, whose unmap is one long operation on the
//!   process's address space;
//! * **wine**: 8 threads each opening 64 MiB of one `PROT_NONE` reservation,
//!   touching it, giving it back (`MADV_DONTNEED`) and closing it, as Wine
//!   manages its heaps;
//! * **convoy**: 12 threads that map, touch and unmap 96 MiB each, so that
//!   one unmap holds the address space while eleven threads wait for it;
//! * **forkbig**: a large region filled once, 6 such threads, and the main
//!   thread forking a child that exits at once, in a loop: each fork copies
//!   the large region's page map.
//! * **quiet2**: no load again, after them: a run whose quiet phases are
//!   late measured the host, not the kernel.
//!
//! Each phase prints, per canary, `stall: <phase> <canary> wakes <n> p50 <ms>
//! p99 <ms> max <ms>`, then `stall: <phase> worst <ms>`; the program ends
//! with `stall: done` and status 0. Whether a phase's worst is within its
//! bound is the row's to say (`tools/common/xtask/src/stall.rs`), so that the
//! bounds live in one place.

use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long each phase runs.
const PHASE: Duration = Duration::from_secs(4);
/// How long each canary sleeps.
const NAP: Duration = Duration::from_millis(10);
/// The most processors given a canary of their own.
const MAX_CPUS: usize = 8;
/// Lateness samples kept per canary and phase, the latest first overwritten.
const SAMPLES: usize = 1024;
/// The phases, in order: index 0 is "between phases" and is not reported.
const PHASES: [&str; 9] = [
    "quiet", "hog", "futex", "fault", "wine", "convoy", "bigunmap", "forkbig", "quiet2",
];
/// The large region of bigunmap and forkbig, in MiB.
const BIG_MIB: usize = 1024;
/// A page.
const PAGE: usize = 4096;

/// One canary's record of one phase.
#[repr(C)]
#[derive(Debug)]
struct Stats {
    /// Wakes counted.
    wakes: AtomicU64,
    /// The latest wake, in microseconds.
    max_us: AtomicU64,
    /// Lateness of each wake in microseconds, the ring of the last
    /// [`SAMPLES`].
    samples: [AtomicU32; SAMPLES],
}

/// Everything the canaries and the phase runner share, in memory mapped
/// shared before the canaries are forked.
#[repr(C)]
#[derive(Debug)]
struct Shared {
    /// The phase running now, one past its index in [`PHASES`]; 0 between.
    phase: AtomicU32,
    /// Set when the canaries are to exit.
    stop: AtomicU32,
    /// `[canary][phase]`: canaries `0..cpus` are pinned, `cpus` is free.
    stats: [[Stats; PHASES.len() + 1]; MAX_CPUS + 1],
}

/// The shared record, mapped once by `main` before anything is forked.
fn shared() -> Result<&'static Shared, String> {
    // SAFETY: an anonymous shared mapping of the struct's size, which the
    // kernel hands back zeroed; every field of `Shared` is an atomic, for
    // which zero is a valid value, and the mapping is never unmapped, so the
    // reference is good for the program's life in every process that
    // inherits it.
    unsafe {
        let at = libc::mmap(
            core::ptr::null_mut(),
            size_of::<Shared>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if at == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        Ok(&*at.cast::<Shared>())
    }
}

/// Keep the calling thread on processor `cpu`.
fn pin(cpu: usize) -> Result<(), String> {
    // SAFETY: a zeroed set with one processor added, passed with its size.
    let answer = unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set)
    };
    if answer != 0 {
        return Err(format!("pin to {cpu}: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// A canary's life: sleep [`NAP`], record how late the sleep ended, until
/// told to stop.
fn canary(shared: &Shared, index: usize, cpu: Option<usize>) -> ! {
    if let Some(cpu) = cpu
        && let Err(why) = pin(cpu)
    {
        println!("stall: FAILED canary {index}: {why}");
        exit(1);
    }
    let Some(mine) = shared.stats.get(index) else {
        exit(1);
    };
    while shared.stop.load(Ordering::Relaxed) == 0 {
        let asleep = Instant::now();
        std::thread::sleep(NAP);
        let late = asleep.elapsed().saturating_sub(NAP);
        let phase = shared.phase.load(Ordering::Relaxed) as usize;
        let Some(stats) = mine.get(phase) else {
            continue;
        };
        let us = u64::try_from(late.as_micros()).unwrap_or(u64::MAX);
        let n = stats.wakes.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = usize::try_from(n)
            .ok()
            .and_then(|n| stats.samples.get(n % SAMPLES))
        {
            slot.store(u32::try_from(us).unwrap_or(u32::MAX), Ordering::Relaxed);
        }
        let _ = stats.max_us.fetch_max(us, Ordering::Relaxed);
    }
    exit(0)
}

/// Leave the process now, with `status`, after the output is out.
fn exit(status: i32) -> ! {
    let _ = std::io::stdout().flush();
    // SAFETY: ends this process; nothing of it is needed after.
    unsafe { libc::_exit(status) }
}

/// Compute forever.
fn hog() {
    let mut a = 1.0_f64;
    loop {
        a = core::hint::black_box(a * 1.000_001 + 1e-9);
    }
}

/// One side of a futex hand-off: wait for `turn` to be `me`, give it to the
/// other side, wake it.
fn futex_side(turn: &'static AtomicU32, me: u32) {
    loop {
        while turn.load(Ordering::Acquire) != me {
            // SAFETY: a private futex wait on a live atomic, for as long as
            // it holds the other side's value.
            let _ = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    turn.as_ptr(),
                    libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                    1 - me,
                    core::ptr::null::<libc::timespec>(),
                )
            };
        }
        turn.store(1 - me, Ordering::Release);
        // SAFETY: a private futex wake of one waiter on a live atomic.
        let _ = unsafe {
            libc::syscall(
                libc::SYS_futex,
                turn.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                1,
            )
        };
    }
}

/// Map `bytes` of private anonymous memory, or `None`.
fn map(bytes: usize) -> Option<*mut u8> {
    // SAFETY: a fresh private anonymous mapping; nothing else names it.
    let at = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    (at != libc::MAP_FAILED).then_some(at.cast())
}

/// Write one byte of every page of the `bytes` at `at`, which [`map`] made.
fn touch(at: *mut u8, bytes: usize) {
    for offset in (0..bytes).step_by(PAGE) {
        // SAFETY: inside the mapping `map` made of `bytes`, writable.
        unsafe { at.add(offset).write_volatile(1) };
    }
}

/// Map `mib`, touch every page, unmap, forever.
fn faulter(mib: usize) {
    let bytes = mib << 20;
    loop {
        if let Some(at) = map(bytes) {
            touch(at, bytes);
            // SAFETY: the mapping `map` made, unmapped once.
            let _ = unsafe { libc::munmap(at.cast(), bytes) };
        }
    }
}

/// Wine's pattern: one large `PROT_NONE` reservation, and `count` threads
/// each making 64 MiB of it readable and writable, touching it, giving the
/// pages back and closing it again, forever.
fn wine(count: usize) {
    const CHUNK: usize = 64 << 20;
    let bytes = count * CHUNK;
    // SAFETY: a fresh private anonymous reservation nothing else names.
    let at = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            bytes,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return;
    }
    let base = at as usize;
    for index in 0..count {
        let chunk = base + index * CHUNK;
        let _ = std::thread::spawn(move || {
            loop {
                let at = chunk as *mut u8;
                // SAFETY: the thread's own 64 MiB of the reservation above,
                // opened before it is touched and closed after.
                unsafe {
                    let _ = libc::mprotect(at.cast(), CHUNK, libc::PROT_READ | libc::PROT_WRITE);
                    touch(at, CHUNK);
                    let _ = libc::madvise(at.cast(), CHUNK, libc::MADV_DONTNEED);
                    let _ = libc::mprotect(at.cast(), CHUNK, libc::PROT_NONE);
                }
            }
        });
    }
}

/// The slowest of reading the load's `stat`, `status` and `maps`, in
/// microseconds: what a monitor reading another program's `/proc` sees.
fn proc_read(pid: libc::pid_t) -> u64 {
    let mut worst = 0;
    for file in ["stat", "status", "maps"] {
        let begun = Instant::now();
        let _ = std::fs::read(format!("/proc/{pid}/{file}"));
        worst = worst.max(u64::try_from(begun.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    worst
}

/// Start `count` threads running `body`.
fn threads(count: usize, body: fn()) {
    for _ in 0..count {
        let _ = std::thread::spawn(body);
    }
}

/// The load of phase `name`, run in the calling process forever.
fn load(name: &str) -> ! {
    match name {
        "hog" => threads(16, hog),
        "futex" => {
            for _ in 0..8 {
                let turn: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
                let _ = std::thread::spawn(move || futex_side(turn, 0));
                let _ = std::thread::spawn(move || futex_side(turn, 1));
            }
        }
        "fault" => threads(8, || faulter(32)),
        "wine" => wine(8),
        "convoy" => threads(12, || faulter(96)),
        "bigunmap" => {
            threads(6, || faulter(8));
            threads(1, || faulter(BIG_MIB));
        }
        "forkbig" => {
            let bytes = BIG_MIB << 20;
            if let Some(at) = map(bytes) {
                touch(at, bytes);
            }
            threads(6, || faulter(8));
            loop {
                // SAFETY: the child only exits; the parent waits for it.
                unsafe {
                    let child = libc::fork();
                    if child == 0 {
                        libc::_exit(0);
                    }
                    if child > 0 {
                        let _ = libc::waitpid(child, core::ptr::null_mut(), 0);
                    }
                }
            }
        }
        _ => {}
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Fork a process of its own group running phase `name`'s load.
fn start_load(name: &str) -> Result<libc::pid_t, String> {
    // SAFETY: the child makes itself a group and runs the load until killed.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: its own process group, so that a kill reaches every
        // process the load makes.
        let _ = unsafe { libc::setpgid(0, 0) };
        load(name);
    }
    Ok(pid)
}

/// End a load started by [`start_load`], and every process it made.
fn end_load(pid: libc::pid_t) {
    // SAFETY: the load's own group, then its leader reaped, then any
    // children it made that are this process's now.
    unsafe {
        let _ = libc::kill(-pid, libc::SIGKILL);
        let _ = libc::waitpid(pid, core::ptr::null_mut(), 0);
        while libc::waitpid(-1, core::ptr::null_mut(), libc::WNOHANG) > 0 {}
    }
}

/// Milliseconds, from microseconds, for printing.
fn ms(us: u64) -> String {
    format!("{}.{:02}", us / 1000, us % 1000 / 10)
}

/// Print phase `phase`'s lines, the worst canary last.
fn report(shared: &Shared, phase: usize, cpus: usize) {
    let name = PHASES.get(phase.wrapping_sub(1)).copied().unwrap_or("?");
    let mut worst = 0;
    for canary in 0..=cpus {
        let Some(stats) = shared
            .stats
            .get(canary)
            .and_then(|phases| phases.get(phase))
        else {
            continue;
        };
        let wakes = stats.wakes.load(Ordering::Relaxed);
        let kept = usize::try_from(wakes).unwrap_or(SAMPLES).min(SAMPLES);
        let mut samples: Vec<u32> = stats
            .samples
            .iter()
            .take(kept)
            .map(|slot| slot.load(Ordering::Relaxed))
            .collect();
        samples.sort_unstable();
        let at = |fraction: usize| {
            samples
                .get(kept.saturating_mul(fraction) / 100)
                .or_else(|| samples.last())
                .copied()
                .map_or(0, u64::from)
        };
        let max = stats.max_us.load(Ordering::Relaxed);
        worst = worst.max(max);
        let who = if canary < cpus {
            format!("cpu{canary}")
        } else {
            "free".to_owned()
        };
        println!(
            "stall: {name} {who} wakes {wakes} p50 {} p99 {} max {} ms",
            ms(at(50)),
            ms(at(99)),
            ms(max)
        );
    }
    println!("stall: {name} worst {} ms", ms(worst));
    let _ = std::io::stdout().flush();
}

fn main() {
    // As init, `/proc` is this program's to mount.
    // SAFETY: two literal C strings; a failure (already mounted) is ignored.
    let _ = unsafe {
        libc::mount(
            c"proc".as_ptr(),
            c"/proc".as_ptr(),
            c"proc".as_ptr(),
            0,
            core::ptr::null(),
        )
    };
    let shared = match shared() {
        Ok(shared) => shared,
        Err(why) => {
            println!("stall: FAILED {why}");
            exit(1);
        }
    };
    let cpus = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(MAX_CPUS);
    println!("stall: {cpus} processors, {} s a phase", PHASE.as_secs());
    let mut canaries = Vec::new();
    for index in 0..=cpus {
        // SAFETY: the child runs its canary loop and exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            canary(shared, index, (index < cpus).then_some(index));
        }
        if pid < 0 {
            println!("stall: FAILED fork: {}", std::io::Error::last_os_error());
            exit(1);
        }
        canaries.push(pid);
    }
    for (index, name) in PHASES.iter().enumerate() {
        let load = if name.starts_with("quiet") {
            None
        } else {
            match start_load(name) {
                Ok(pid) => Some(pid),
                Err(why) => {
                    println!("stall: FAILED {name}: {why}");
                    exit(1);
                }
            }
        };
        let phase = u32::try_from(index + 1).unwrap_or(0);
        shared.phase.store(phase, Ordering::Relaxed);
        let begun = Instant::now();
        let mut proc_us = 0;
        while begun.elapsed() < PHASE {
            if let Some(pid) = load {
                proc_us = proc_us.max(proc_read(pid));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let ran = begun.elapsed();
        shared.phase.store(0, Ordering::Relaxed);
        if let Some(pid) = load {
            end_load(pid);
        }
        // The canaries' last wakes of the phase land before it is read.
        std::thread::sleep(Duration::from_millis(200));
        if ran > PHASE.saturating_mul(2) {
            println!(
                "stall: {name} ran {} ms where {} were asked: the phase runner was kept off too",
                ran.as_millis(),
                PHASE.as_millis()
            );
        }
        report(shared, index + 1, cpus);
        if load.is_some() {
            println!("stall: {name} proc {} ms", ms(proc_us));
        }
    }
    shared.stop.store(1, Ordering::Relaxed);
    for pid in canaries {
        // SAFETY: reaping a canary this process forked.
        let _ = unsafe { libc::waitpid(pid, core::ptr::null_mut(), 0) };
    }
    println!("stall: done");
    exit(0);
}
